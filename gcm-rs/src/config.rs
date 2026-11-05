//! Saved sessions and application settings, stored as TOML.
//!
//! Passwords are encrypted at rest with AES-256-GCM using a random key kept
//! next to the config file (mode 0600 on Unix). Like the original GCM this
//! only keeps passwords out of plain sight; anyone who can read both files
//! can recover them. Prefer SSH keys.

use std::fs;
use std::path::{Path, PathBuf};

use aes_gcm::aead::{Aead, KeyInit};
use aes_gcm::{Aes256Gcm, Nonce};
use anyhow::{Context, Result, anyhow};
use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use rand::RngCore;
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum Protocol {
    #[default]
    Ssh,
    Telnet,
    Serial,
    Rdp,
    Vnc,
    Local,
    Mosh,
}

impl Protocol {
    pub const ALL: [Protocol; 7] = [
        Protocol::Ssh,
        Protocol::Mosh,
        Protocol::Telnet,
        Protocol::Serial,
        Protocol::Rdp,
        Protocol::Vnc,
        Protocol::Local,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Protocol::Ssh => "SSH",
            Protocol::Telnet => "Telnet",
            Protocol::Serial => "Serial",
            Protocol::Rdp => "RDP",
            Protocol::Vnc => "VNC",
            Protocol::Local => "Local shell",
            Protocol::Mosh => "Mosh",
        }
    }

    pub fn icon(self) -> &'static str {
        match self {
            Protocol::Ssh => "🔐",
            Protocol::Telnet => "📟",
            Protocol::Serial => "🔌",
            Protocol::Rdp => "🖥",
            Protocol::Vnc => "🖵",
            Protocol::Local => "💻",
            Protocol::Mosh => "📡",
        }
    }

    pub fn default_port(self) -> u16 {
        match self {
            // Mosh logs in over SSH first.
            Protocol::Ssh | Protocol::Mosh => 22,
            Protocol::Telnet => 23,
            Protocol::Rdp => 3389,
            Protocol::Vnc => 5900,
            Protocol::Serial | Protocol::Local => 0,
        }
    }

    /// Protocols handed to an external GUI client instead of a terminal tab.
    pub fn is_external(self) -> bool {
        matches!(self, Protocol::Rdp | Protocol::Vnc)
    }
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum Parity {
    #[default]
    None,
    Odd,
    Even,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum FlowControl {
    #[default]
    None,
    Software,
    Hardware,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct SerialSettings {
    pub baud: u32,
    pub data_bits: u8,
    pub parity: Parity,
    pub stop_bits: u8,
    pub flow_control: FlowControl,
}

impl Default for SerialSettings {
    fn default() -> Self {
        Self {
            baud: 115200,
            data_bits: 8,
            parity: Parity::None,
            stop_bits: 1,
            flow_control: FlowControl::None,
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Default)]
#[serde(default)]
pub struct Session {
    pub id: u64,
    pub name: String,
    /// Folder path, `/`-separated. Empty means top level.
    pub folder: String,
    pub description: String,
    pub protocol: Protocol,
    /// Host name, or the serial device (`/dev/ttyUSB0`, `COM3`).
    pub host: String,
    /// 0 means the protocol default.
    pub port: u16,
    pub user: String,
    #[serde(skip)]
    pub password: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    password_enc: String,
    pub private_key: String,
    /// Extra command-line arguments, split shell-style.
    pub extra_args: String,
    /// Lines sent after connecting. `##D=<ms>` inserts a delay (GCM syntax).
    pub commands: String,
    /// SSH: ServerAliveInterval in seconds, 0 to disable.
    pub keepalive: u32,
    pub x11: bool,
    pub agent_forward: bool,
    pub compression: bool,
    /// SSH forwards, one per entry: `L 8080:localhost:80`, `R 9000:localhost:9000`, `D 1080`.
    pub tunnels: Vec<String>,
    pub serial: SerialSettings,
    /// Terminal text and background colours as `#rrggbb`; empty means the theme's.
    pub fg_color: String,
    pub bg_color: String,
    /// Save the terminal output of this session to a log file.
    pub log: bool,
}

impl Session {
    pub fn new(protocol: Protocol) -> Self {
        Self {
            id: new_id(),
            protocol,
            ..Default::default()
        }
    }

    pub fn effective_port(&self) -> u16 {
        if self.port == 0 {
            self.protocol.default_port()
        } else {
            self.port
        }
    }

    pub fn display_name(&self) -> String {
        if !self.name.is_empty() {
            self.name.clone()
        } else if !self.host.is_empty() {
            self.host.clone()
        } else {
            self.protocol.label().to_string()
        }
    }

    /// Parse a MobaXterm-style quick connect string: `[proto://][user@]host[:port]`.
    pub fn from_quick(input: &str) -> Option<Self> {
        let input = input.trim();
        if input.is_empty() {
            return None;
        }
        let (protocol, rest) = match input.split_once("://") {
            Some((p, rest)) => {
                let p = match p.to_ascii_lowercase().as_str() {
                    "ssh" => Protocol::Ssh,
                    "telnet" => Protocol::Telnet,
                    "mosh" => Protocol::Mosh,
                    "rdp" => Protocol::Rdp,
                    "vnc" => Protocol::Vnc,
                    "serial" => Protocol::Serial,
                    _ => return None,
                };
                (p, rest)
            }
            None => (Protocol::Ssh, input),
        };
        let mut s = Session::new(protocol);
        let rest = match rest.rsplit_once('@') {
            Some((user, host)) => {
                s.user = user.to_string();
                host
            }
            None => rest,
        };
        if protocol == Protocol::Serial {
            // serial://COM3:9600 or serial:///dev/ttyUSB0:115200
            match rest.rsplit_once(':') {
                Some((dev, baud)) if baud.parse::<u32>().is_ok() => {
                    s.host = dev.to_string();
                    s.serial.baud = baud.parse().unwrap();
                }
                _ => s.host = rest.to_string(),
            }
        } else {
            // Bracketed IPv6 ([::1]:22) or host:port; a bare IPv6 address has several colons.
            if let Some(stripped) = rest.strip_prefix('[') {
                let (h, tail) = stripped.split_once(']')?;
                s.host = h.to_string();
                if let Some(p) = tail.strip_prefix(':') {
                    s.port = p.parse().ok()?;
                }
            } else if rest.matches(':').count() == 1 {
                let (h, p) = rest.split_once(':').unwrap();
                s.host = h.to_string();
                s.port = p.parse().ok()?;
            } else {
                s.host = rest.to_string();
            }
        }
        if s.host.is_empty() {
            return None;
        }
        Some(s)
    }
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum ImeMode {
    #[default]
    Auto,
    On,
    Off,
}

/// For `ImeMode::Auto`: is there an input method to talk to? Windows,
/// macOS and Wayland always have one. On X11, enabling input-method mode
/// with no IM server running can swallow key presses, so only do it when
/// one is configured.
fn ime_available() -> bool {
    use std::sync::OnceLock;
    static AVAILABLE: OnceLock<bool> = OnceLock::new();
    *AVAILABLE.get_or_init(|| {
        let var = |k: &str| std::env::var(k).unwrap_or_default();
        ime_available_from(cfg!(unix) && !cfg!(target_os = "macos"), &var)
    })
}

fn ime_available_from(x11_or_wayland: bool, var: &dyn Fn(&str) -> String) -> bool {
    if !x11_or_wayland || !var("WAYLAND_DISPLAY").is_empty() {
        return true;
    }
    let xmod = var("XMODIFIERS");
    (xmod.contains("@im=") && !xmod.contains("@im=none"))
        || ["GTK_IM_MODULE", "QT_IM_MODULE"]
            .iter()
            .any(|k| !matches!(var(k).as_str(), "" | "none" | "xim"))
}

/// Parse a colour as `#rgb`, `#rrggbb`, `#rrrgggbbb`, `#rrrrggggbbbb` (what
/// GTK accepts, and so what GCM saved) or `rgb(r,g,b)` / `rgba(r,g,b,a)`.
pub fn parse_color(s: &str) -> Option<[u8; 3]> {
    let s = s.trim();
    if let Some(hex) = s.strip_prefix('#') {
        let n = hex.len();
        if !matches!(n, 3 | 6 | 9 | 12) || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
            return None;
        }
        let k = n / 3;
        let max = (1u64 << (4 * k)) - 1;
        let mut out = [0u8; 3];
        for (i, c) in out.iter_mut().enumerate() {
            let v = u64::from_str_radix(&hex[i * k..(i + 1) * k], 16).ok()?;
            *c = ((v * 255 + max / 2) / max) as u8;
        }
        return Some(out);
    }
    let inner = s
        .strip_prefix("rgba(")
        .or_else(|| s.strip_prefix("rgb("))?
        .strip_suffix(')')?;
    let parts: Vec<&str> = inner.split(',').map(str::trim).collect();
    if parts.len() < 3 {
        return None;
    }
    let mut out = [0u8; 3];
    for (c, p) in out.iter_mut().zip(&parts) {
        *c = p.parse().ok()?;
    }
    Some(out)
}

pub fn color_hex([r, g, b]: [u8; 3]) -> String {
    format!("#{r:02x}{g:02x}{b:02x}")
}

/// Sessions read from another program's configuration.
pub struct Imported {
    pub sessions: Vec<Session>,
    /// One line per entry that could not be imported, with the reason.
    pub skipped: Vec<String>,
}

pub fn new_id() -> u64 {
    rand::rng().next_u64()
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct Settings {
    pub font_size: f32,
    pub scrollback: usize,
    pub dark_mode: bool,
    pub copy_on_select: bool,
    pub paste_on_right_click: bool,
    pub confirm_close_tab: bool,
    pub open_local_on_startup: bool,
    /// ssh binary; Windows 10+ ships OpenSSH as `ssh`.
    pub ssh_command: String,
    pub telnet_command: String,
    /// Run `telnet_command` instead of the built-in Telnet client.
    pub use_system_telnet: bool,
    pub mosh_command: String,
    pub sftp_command: String,
    /// Where the SFTP browser saves downloads; empty means the system Downloads folder.
    pub download_dir: String,
    /// Empty means `$SHELL` (Unix) or PowerShell (Windows).
    pub local_shell: String,
    /// External client templates. Placeholders: {host} {port} {user} {password}.
    pub rdp_command: String,
    pub vnc_command: String,
    /// Optional monospace font file for terminals (e.g. Consolas, JetBrains Mono).
    pub font_path: String,
    /// Where session logs go; empty means `logs` in the config folder.
    pub log_dir: String,
    /// Input method (IME) for typing e.g. Chinese or Japanese into terminals.
    pub ime: ImeMode,
    pub sidebar_width: f32,
    /// Folders the user collapsed in the session tree.
    pub collapsed: Vec<String>,
}

impl Settings {
    /// Whether terminals should enable the platform input method.
    pub fn ime_enabled(&self) -> bool {
        match self.ime {
            ImeMode::On => true,
            ImeMode::Off => false,
            ImeMode::Auto => ime_available(),
        }
    }

    pub fn log_dir(&self) -> PathBuf {
        if self.log_dir.is_empty() {
            config_dir().join("logs")
        } else {
            PathBuf::from(&self.log_dir)
        }
    }

    pub fn download_dir(&self) -> PathBuf {
        if !self.download_dir.is_empty() {
            return PathBuf::from(&self.download_dir);
        }
        dirs::download_dir()
            .or_else(dirs::home_dir)
            .unwrap_or_else(|| PathBuf::from("."))
    }
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            font_size: 14.0,
            scrollback: 10_000,
            dark_mode: true,
            copy_on_select: false,
            paste_on_right_click: true,
            confirm_close_tab: true,
            open_local_on_startup: false,
            ssh_command: "ssh".into(),
            telnet_command: "telnet".into(),
            use_system_telnet: false,
            mosh_command: "mosh".into(),
            sftp_command: "sftp".into(),
            download_dir: String::new(),
            local_shell: String::new(),
            rdp_command: if cfg!(windows) {
                "mstsc /v:{host}:{port}".into()
            } else {
                "xfreerdp /v:{host}:{port} /u:{user} /p:{password} /cert:tofu /dynamic-resolution"
                    .into()
            },
            vnc_command: if cfg!(windows) {
                "vncviewer.exe {host}::{port}".into()
            } else {
                "vncviewer {host}::{port}".into()
            },
            font_path: String::new(),
            log_dir: String::new(),
            ime: ImeMode::Auto,
            sidebar_width: 240.0,
            collapsed: Vec::new(),
        }
    }
}

#[derive(Serialize, Deserialize, Default)]
struct FileFormat {
    #[serde(default)]
    settings: Settings,
    #[serde(default, rename = "session")]
    sessions: Vec<Session>,
    #[serde(default, rename = "tunnel")]
    tunnels: Vec<crate::tunnel::Tunnel>,
}

pub struct Store {
    pub settings: Settings,
    pub sessions: Vec<Session>,
    pub tunnels: Vec<crate::tunnel::Tunnel>,
    path: PathBuf,
    key: [u8; 32],
}

pub fn config_dir() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("gcm-rs")
}

impl Store {
    pub fn load_default() -> Result<Self> {
        Self::load(&config_dir())
    }

    pub fn load(dir: &Path) -> Result<Self> {
        fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        let key = load_or_create_key(&dir.join("key"))?;
        let path = dir.join("gcm.toml");
        let file: FileFormat = if path.exists() {
            let text =
                fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
            toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?
        } else {
            FileFormat::default()
        };
        let mut store = Store {
            settings: file.settings,
            sessions: file.sessions,
            tunnels: file.tunnels,
            path,
            key,
        };
        for s in &mut store.sessions {
            if s.id == 0 {
                s.id = new_id();
            }
            if !s.password_enc.is_empty() {
                // A bad entry should not stop the app from starting.
                s.password = decrypt(&store.key, &s.password_enc).unwrap_or_else(|e| {
                    log::warn!("cannot decrypt password for {}: {e}", s.display_name());
                    String::new()
                });
            }
        }
        Ok(store)
    }

    pub fn save(&mut self) -> Result<()> {
        for s in &mut self.sessions {
            s.password_enc = if s.password.is_empty() {
                String::new()
            } else {
                encrypt(&self.key, &s.password)?
            };
        }
        let file = FileFormat {
            settings: self.settings.clone(),
            sessions: self.sessions.clone(),
            tunnels: self.tunnels.clone(),
        };
        let text = toml::to_string_pretty(&file)?;
        // Write then rename so a crash never leaves a truncated config.
        let tmp = self.path.with_extension("toml.tmp");
        fs::write(&tmp, text)?;
        restrict_permissions(&tmp);
        fs::rename(&tmp, &self.path)?;
        Ok(())
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn find(&self, id: u64) -> Option<&Session> {
        self.sessions.iter().find(|s| s.id == id)
    }

    /// True if a session with the same name, place and target already exists.
    pub fn has_equivalent(&self, s: &Session) -> bool {
        self.sessions.iter().any(|e| {
            e.name == s.name
                && e.folder == s.folder
                && e.protocol == s.protocol
                && e.host == s.host
                && e.effective_port() == s.effective_port()
                && e.user == s.user
        })
    }

    /// Add imported sessions, skipping ones already present. Returns how
    /// many were added and how many were duplicates.
    pub fn import(&mut self, sessions: Vec<Session>) -> (usize, usize) {
        let (mut added, mut dup) = (0, 0);
        for s in sessions {
            if self.has_equivalent(&s) {
                dup += 1;
            } else {
                self.sessions.push(s);
                added += 1;
            }
        }
        (added, dup)
    }

    pub fn upsert(&mut self, session: Session) {
        match self.sessions.iter_mut().find(|s| s.id == session.id) {
            Some(existing) => *existing = session,
            None => self.sessions.push(session),
        }
    }

    pub fn remove(&mut self, id: u64) {
        self.sessions.retain(|s| s.id != id);
    }

    /// All folder paths, including parents of nested folders, sorted.
    pub fn folders(&self) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        for s in &self.sessions {
            let mut acc = String::new();
            for part in s.folder.split('/').filter(|p| !p.is_empty()) {
                if !acc.is_empty() {
                    acc.push('/');
                }
                acc.push_str(part);
                if !out.contains(&acc) {
                    out.push(acc.clone());
                }
            }
        }
        out.sort();
        out
    }
}

fn load_or_create_key(path: &Path) -> Result<[u8; 32]> {
    if let Ok(bytes) = fs::read(path) {
        return bytes
            .try_into()
            .map_err(|_| anyhow!("{} is not a 32-byte key", path.display()));
    }
    let mut key = [0u8; 32];
    rand::rng().fill_bytes(&mut key);
    fs::write(path, key)?;
    restrict_permissions(path);
    Ok(key)
}

#[cfg(unix)]
fn restrict_permissions(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o600));
}

#[cfg(not(unix))]
fn restrict_permissions(_path: &Path) {}

fn encrypt(key: &[u8; 32], plain: &str) -> Result<String> {
    let cipher = Aes256Gcm::new(key.into());
    let mut nonce = [0u8; 12];
    rand::rng().fill_bytes(&mut nonce);
    let ct = cipher
        .encrypt(Nonce::from_slice(&nonce), plain.as_bytes())
        .map_err(|_| anyhow!("encrypt failed"))?;
    let mut out = nonce.to_vec();
    out.extend(ct);
    Ok(B64.encode(out))
}

fn decrypt(key: &[u8; 32], enc: &str) -> Result<String> {
    let raw = B64.decode(enc)?;
    if raw.len() < 12 {
        return Err(anyhow!("ciphertext too short"));
    }
    let (nonce, ct) = raw.split_at(12);
    let cipher = Aes256Gcm::new(key.into());
    let plain = cipher
        .decrypt(Nonce::from_slice(nonce), ct)
        .map_err(|_| anyhow!("wrong key or corrupted data"))?;
    Ok(String::from_utf8(plain)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn password_roundtrip() {
        let dir = std::env::temp_dir().join(format!("gcm-rs-test-{}", new_id()));
        let mut store = Store::load(&dir).unwrap();
        let mut s = Session::new(Protocol::Ssh);
        s.name = "box".into();
        s.host = "example.org".into();
        s.password = "s3cret ü".into();
        store.upsert(s);
        store.save().unwrap();
        let text = fs::read_to_string(dir.join("gcm.toml")).unwrap();
        assert!(!text.contains("s3cret"));
        let again = Store::load(&dir).unwrap();
        assert_eq!(again.sessions[0].password, "s3cret ü");
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn ime_auto_detection() {
        let env = |pairs: &'static [(&'static str, &'static str)]| {
            move |k: &str| {
                pairs
                    .iter()
                    .find(|(n, _)| *n == k)
                    .map(|(_, v)| v.to_string())
                    .unwrap_or_default()
            }
        };
        assert!(
            ime_available_from(false, &env(&[])),
            "Windows/macOS always have an IME"
        );
        assert!(ime_available_from(
            true,
            &env(&[("WAYLAND_DISPLAY", "wayland-0")])
        ));
        assert!(ime_available_from(
            true,
            &env(&[("XMODIFIERS", "@im=ibus")])
        ));
        assert!(ime_available_from(
            true,
            &env(&[("GTK_IM_MODULE", "fcitx")])
        ));
        assert!(!ime_available_from(true, &env(&[])), "bare X11");
        assert!(!ime_available_from(
            true,
            &env(&[("XMODIFIERS", "@im=none")])
        ));
        assert!(!ime_available_from(true, &env(&[("GTK_IM_MODULE", "xim")])));
    }

    #[test]
    fn colors() {
        assert_eq!(parse_color("#ff8000"), Some([255, 128, 0]));
        assert_eq!(parse_color("#f80"), Some([255, 136, 0]));
        assert_eq!(parse_color("#ffff00000000"), Some([255, 0, 0]));
        assert_eq!(parse_color("rgb(1, 2, 3)"), Some([1, 2, 3]));
        assert_eq!(parse_color("rgba(1,2,3,0.5)"), Some([1, 2, 3]));
        // GCM's unpadded "%x%x%x" output for (255, 0, 128) is ambiguous; GTK rejected it too.
        assert_eq!(parse_color("#ff080"), None);
        assert_eq!(parse_color(""), None);
        assert_eq!(color_hex([255, 128, 0]), "#ff8000");
    }

    #[test]
    fn quick_connect() {
        let s = Session::from_quick("root@10.0.0.1:2222").unwrap();
        assert_eq!(
            (s.protocol, s.user.as_str(), s.host.as_str(), s.port),
            (Protocol::Ssh, "root", "10.0.0.1", 2222)
        );
        let s = Session::from_quick("telnet://router").unwrap();
        assert_eq!(
            (s.protocol, s.host.as_str(), s.effective_port()),
            (Protocol::Telnet, "router", 23)
        );
        let s = Session::from_quick("mosh://bob@box").unwrap();
        assert_eq!(
            (s.protocol, s.user.as_str(), s.effective_port()),
            (Protocol::Mosh, "bob", 22)
        );
        let s = Session::from_quick("[fe80::1]:22").unwrap();
        assert_eq!(s.host, "fe80::1");
        let s = Session::from_quick("fe80::1").unwrap();
        assert_eq!(s.host, "fe80::1");
        let s = Session::from_quick("serial://COM3:9600").unwrap();
        assert_eq!((s.host.as_str(), s.serial.baud), ("COM3", 9600));
        assert!(Session::from_quick("host:notaport").is_none());
        assert!(Session::from_quick("").is_none());
    }

    #[test]
    fn import_skips_duplicates() {
        let dir = std::env::temp_dir().join(format!("gcm-rs-test-{}", new_id()));
        let mut store = Store::load(&dir).unwrap();
        let mut s = Session::new(Protocol::Ssh);
        s.name = "web".into();
        s.host = "web1".into();
        s.port = 22;
        let mut again = s.clone();
        again.id = new_id();
        again.port = 0; // same effective port
        let mut other = s.clone();
        other.id = new_id();
        other.host = "web2".into();
        assert_eq!(store.import(vec![s]), (1, 0));
        assert_eq!(store.import(vec![again, other]), (1, 1));
        assert_eq!(store.sessions.len(), 2);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn folders_include_parents() {
        let dir = std::env::temp_dir().join(format!("gcm-rs-test-{}", new_id()));
        let mut store = Store::load(&dir).unwrap();
        let mut s = Session::new(Protocol::Ssh);
        s.folder = "prod/db".into();
        store.upsert(s);
        assert_eq!(
            store.folders(),
            vec!["prod".to_string(), "prod/db".to_string()]
        );
        fs::remove_dir_all(dir).unwrap();
    }
}
