//! Import PuTTY saved sessions (KiTTY uses the same keys).
//!
//! PuTTY keeps sessions in the Windows registry under
//! `HKCU\Software\SimonTatham\PuTTY\Sessions`, or on Unix as one
//! `Key=Value` file per session in `~/.putty/sessions`. Session names are
//! URL-encoded (`My%20Server`). Registry contents are read through a `.reg`
//! export, either one the user made or one taken with `reg export`.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow};

use crate::config::{FlowControl, Imported, Parity, Protocol, Session, color_hex};

const REG_KEY: &str = r"HKEY_CURRENT_USER\Software\SimonTatham\PuTTY\Sessions";

/// Folder imported sessions are put in, since PuTTY has none.
const FOLDER: &str = "PuTTY";

type Values = BTreeMap<String, String>;

/// Decode PuTTY's session-name escaping (`%20` and friends).
fn url_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let Ok(hex) = std::str::from_utf8(&bytes[i + 1..i + 3])
            && let Ok(b) = u8::from_str_radix(hex, 16)
        {
            out.push(b);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// `.reg` files from regedit/reg.exe are UTF-16LE with a BOM; others may be UTF-8.
fn decode_reg(bytes: &[u8]) -> String {
    if let Some(rest) = bytes.strip_prefix(&[0xff, 0xfe]) {
        let units: Vec<u16> = rest
            .chunks_exact(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .collect();
        return String::from_utf16_lossy(&units);
    }
    String::from_utf8_lossy(bytes.strip_prefix(&[0xef, 0xbb, 0xbf]).unwrap_or(bytes)).into_owned()
}

/// Sessions in a `.reg` export: `[...\Sessions\<name>]` sections with
/// `"Key"="string"` and `"Key"=dword:0000002a` values.
fn parse_reg(text: &str) -> Vec<(String, Values)> {
    let prefix = format!("{REG_KEY}\\").to_lowercase();
    let mut out: Vec<(String, Values)> = Vec::new();
    let mut current: Option<usize> = None;
    for line in text.lines() {
        let line = line.trim();
        if let Some(section) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
            current = section
                .to_lowercase()
                .strip_prefix(&prefix)
                .filter(|rest| !rest.is_empty() && !rest.contains('\\'))
                .map(|_| {
                    out.push((section[prefix.len()..].to_string(), Values::new()));
                    out.len() - 1
                });
            continue;
        }
        let Some(idx) = current else { continue };
        let Some(rest) = line.strip_prefix('"') else {
            continue;
        };
        let Some((key, value)) = rest.split_once("\"=") else {
            continue;
        };
        let value = if let Some(v) = value.strip_prefix('"').and_then(|v| v.strip_suffix('"')) {
            v.replace("\\\\", "\u{0}")
                .replace("\\\"", "\"")
                .replace('\u{0}', "\\")
        } else if let Some(hex) = value.strip_prefix("dword:") {
            u32::from_str_radix(hex, 16)
                .map(|n| n.to_string())
                .unwrap_or_default()
        } else {
            continue; // hex: values (binary/multi-string) are not used by PuTTY sessions
        };
        out[idx].1.insert(key.to_string(), value);
    }
    out
}

/// One file of `~/.putty/sessions`.
fn parse_unix_session(text: &str) -> Values {
    text.lines()
        .filter_map(|l| l.split_once('='))
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

/// PuTTY's "R,G,B" colour, unless it is PuTTY's default.
fn color(values: &Values, key: &str, default: &str) -> String {
    match values.get(key) {
        Some(v) if v != default => {
            let parts: Vec<u8> = v.split(',').filter_map(|p| p.trim().parse().ok()).collect();
            match parts[..] {
                [r, g, b] => color_hex([r, g, b]),
                _ => String::new(),
            }
        }
        _ => String::new(),
    }
}

/// `L8080=localhost:80,R9000=localhost:9000,D1080`, optionally with a 4/6
/// address-family prefix, to gcm-rs tunnels (`L 8080:localhost:80`).
fn forwards(spec: &str) -> Vec<String> {
    spec.split(',')
        .filter_map(|f| {
            let f = f.trim().trim_start_matches(['4', '6']);
            let kind = f.chars().next()?;
            let rest = &f[1..];
            match kind {
                'L' | 'R' => {
                    let (listen, dest) = rest.split_once('=')?;
                    Some(format!("{kind} {listen}:{dest}"))
                }
                'D' => Some(format!("D {}", rest.trim_end_matches('='))),
                _ => None,
            }
        })
        .collect()
}

fn session(raw_name: &str, v: &Values) -> std::result::Result<Session, String> {
    let get = |k: &str| v.get(k).map(String::as_str).unwrap_or("").trim();
    let num = |k: &str| get(k).parse::<u32>().unwrap_or(0);
    let on = |k: &str| num(k) == 1;
    let protocol = match get("Protocol") {
        "" | "ssh" => Protocol::Ssh,
        "telnet" => Protocol::Telnet,
        "serial" => Protocol::Serial,
        other => return Err(format!("protocol {other} is not supported")),
    };
    let mut s = Session::new(protocol);
    s.name = url_decode(raw_name);
    s.folder = FOLDER.into();
    if protocol == Protocol::Serial {
        s.host = get("SerialLine").to_string();
        if num("SerialSpeed") > 0 {
            s.serial.baud = num("SerialSpeed");
        }
        if (5..=8).contains(&num("SerialDataBits")) {
            s.serial.data_bits = num("SerialDataBits") as u8;
        }
        // Stop bits are stored in halves.
        s.serial.stop_bits = if num("SerialStopHalfbits") >= 4 { 2 } else { 1 };
        s.serial.parity = match num("SerialParity") {
            1 => Parity::Odd,
            2 => Parity::Even,
            _ => Parity::None,
        };
        s.serial.flow_control = match num("SerialFlowControl") {
            1 => FlowControl::Software,
            2 | 3 => FlowControl::Hardware,
            _ => FlowControl::None,
        };
    } else {
        // HostName may carry the user, as in "root@host".
        let host = get("HostName");
        match host.rsplit_once('@') {
            Some((user, h)) => {
                s.user = user.to_string();
                s.host = h.to_string();
            }
            None => s.host = host.to_string(),
        }
        if !get("UserName").is_empty() {
            s.user = get("UserName").to_string();
        }
        s.port = num("PortNumber").try_into().unwrap_or(0);
        if s.port == protocol.default_port() {
            s.port = 0;
        }
    }
    if s.host.is_empty() {
        return Err("no host".into());
    }
    if protocol == Protocol::Ssh {
        s.private_key = get("PublicKeyFile").to_string();
        s.x11 = on("X11Forward");
        s.agent_forward = on("AgentFwd");
        s.compression = on("Compression");
        s.keepalive = num("PingIntervalSecs");
        s.tunnels = forwards(get("PortForwardings"));
    }
    s.fg_color = color(v, "Colour0", "187,187,187");
    s.bg_color = color(v, "Colour2", "0,0,0");
    s.log = num("LogType") != 0;
    Ok(s)
}

fn collect(raw: Vec<(String, Values)>) -> Imported {
    let mut out = Imported {
        sessions: Vec::new(),
        skipped: Vec::new(),
    };
    for (name, values) in raw {
        // PuTTY's template for new sessions, not a real one.
        if name == "Default%20Settings" {
            continue;
        }
        match session(&name, &values) {
            Ok(s) => out.sessions.push(s),
            Err(why) => out.skipped.push(format!("{}: {why}", url_decode(&name))),
        }
    }
    out
}

/// Import a `.reg` file exported from the PuTTY registry key.
pub fn import_reg(path: &Path) -> Result<Imported> {
    let bytes = fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    let raw = parse_reg(&decode_reg(&bytes));
    if raw.is_empty() {
        return Err(anyhow!(
            "no PuTTY sessions in {} (expected keys under {REG_KEY})",
            path.display()
        ));
    }
    Ok(collect(raw))
}

/// Where this platform's PuTTY keeps its sessions, for the menu entry.
pub fn local_source() -> String {
    if cfg!(windows) {
        REG_KEY.into()
    } else {
        sessions_dir().display().to_string()
    }
}

fn sessions_dir() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_default()
        .join(".putty")
        .join("sessions")
}

/// Import the sessions of the PuTTY installed on this computer.
pub fn import_local() -> Result<Imported> {
    if cfg!(windows) {
        // Let reg.exe read the registry; its export is what import_reg parses.
        let tmp =
            std::env::temp_dir().join(format!("gcm-rs-putty-{}.reg", crate::config::new_id()));
        let mut cmd = std::process::Command::new("reg");
        cmd.args(["export", r"HKCU\Software\SimonTatham\PuTTY\Sessions"])
            .arg(&tmp)
            .arg("/y");
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
        }
        let status = cmd.status().context("running reg export")?;
        let result = if status.success() {
            import_reg(&tmp)
        } else {
            Err(anyhow!("no PuTTY sessions found in the registry"))
        };
        let _ = fs::remove_file(&tmp);
        return result;
    }
    let dir = sessions_dir();
    let mut raw = Vec::new();
    for entry in fs::read_dir(&dir).with_context(|| format!("reading {}", dir.display()))? {
        let entry = entry?;
        if entry.file_type()?.is_file() {
            let text = fs::read_to_string(entry.path()).unwrap_or_default();
            raw.push((
                entry.file_name().to_string_lossy().into_owned(),
                parse_unix_session(&text),
            ));
        }
    }
    raw.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(collect(raw))
}

#[cfg(test)]
mod tests {
    use super::*;

    const REG: &str = r#"Windows Registry Editor Version 5.00

[HKEY_CURRENT_USER\Software\SimonTatham\PuTTY\Sessions]

[HKEY_CURRENT_USER\Software\SimonTatham\PuTTY\Sessions\Default%20Settings]
"HostName"=""

[HKEY_CURRENT_USER\Software\SimonTatham\PuTTY\Sessions\Web%20Server%201]
"HostName"="root@web1.example.org"
"PortNumber"=dword:00000836
"Protocol"="ssh"
"PublicKeyFile"="C:\\Users\\bob\\keys\\web.ppk"
"X11Forward"=dword:00000001
"AgentFwd"=dword:00000000
"Compression"=dword:00000001
"PingIntervalSecs"=dword:0000001e
"PortForwardings"="L8080=localhost:80,4R9000=localhost:9000,D1080"
"Colour0"="255,255,255"
"Colour2"="0,0,64"
"LogType"=dword:00000001

[HKEY_CURRENT_USER\Software\SimonTatham\PuTTY\Sessions\router]
"HostName"="192.168.1.1"
"UserName"="admin"
"PortNumber"=dword:00000017
"Protocol"="telnet"
"Colour0"="187,187,187"

[HKEY_CURRENT_USER\Software\SimonTatham\PuTTY\Sessions\console]
"Protocol"="serial"
"SerialLine"="COM3"
"SerialSpeed"=dword:00002580
"SerialDataBits"=dword:00000007
"SerialStopHalfbits"=dword:00000004
"SerialParity"=dword:00000002
"SerialFlowControl"=dword:00000001

[HKEY_CURRENT_USER\Software\SimonTatham\PuTTY\Sessions\old-rlogin]
"HostName"="legacy"
"Protocol"="rlogin"

[HKEY_CURRENT_USER\Software\SimonTatham\PuTTY\Jumplist]
"Recent sessions"="x"
"#;

    fn utf16(s: &str) -> Vec<u8> {
        let mut out = vec![0xff, 0xfe];
        for u in s.replace('\n', "\r\n").encode_utf16() {
            out.extend(u.to_le_bytes());
        }
        out
    }

    #[test]
    fn imports_reg_export() {
        let dir = std::env::temp_dir().join(format!("gcm-putty-{}", crate::config::new_id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("putty.reg");
        fs::write(&path, utf16(REG)).unwrap();
        let r = import_reg(&path).unwrap();
        fs::remove_dir_all(&dir).unwrap();

        assert_eq!(r.sessions.len(), 3);
        assert_eq!(r.skipped, ["old-rlogin: protocol rlogin is not supported"]);

        let web = &r.sessions[0];
        assert_eq!(
            (web.name.as_str(), web.folder.as_str()),
            ("Web Server 1", "PuTTY")
        );
        assert_eq!(
            (web.user.as_str(), web.host.as_str(), web.port),
            ("root", "web1.example.org", 2102)
        );
        assert_eq!(web.private_key, r"C:\Users\bob\keys\web.ppk");
        assert!(web.x11 && web.compression && !web.agent_forward);
        assert_eq!(web.keepalive, 30);
        assert_eq!(
            web.tunnels,
            ["L 8080:localhost:80", "R 9000:localhost:9000", "D 1080"]
        );
        assert_eq!(
            (web.fg_color.as_str(), web.bg_color.as_str(), web.log),
            ("#ffffff", "#000040", true)
        );

        let router = &r.sessions[1];
        assert_eq!(
            (router.protocol, router.user.as_str(), router.port),
            (Protocol::Telnet, "admin", 0)
        );
        assert_eq!(
            router.fg_color, "",
            "PuTTY's default colour is not imported"
        );

        let console = &r.sessions[2];
        assert_eq!(
            (console.protocol, console.host.as_str()),
            (Protocol::Serial, "COM3")
        );
        let sc = &console.serial;
        assert_eq!((sc.baud, sc.data_bits, sc.stop_bits), (9600, 7, 2));
        assert_eq!(
            (sc.parity, sc.flow_control),
            (Parity::Even, FlowControl::Software)
        );
    }

    #[test]
    fn url_decoding() {
        assert_eq!(url_decode("My%20Server%2Fprod"), "My Server/prod");
        assert_eq!(url_decode("caf%C3%A9 100%"), "café 100%");
        assert_eq!(url_decode("é%zz"), "é%zz");
    }

    #[test]
    fn unix_session_files() {
        let v = parse_unix_session(
            "HostName=db.internal\nUserName=pg\nPortNumber=2200\nProtocol=ssh\n",
        );
        let s = session("db%2Dprimary", &v).unwrap();
        assert_eq!(
            (s.name.as_str(), s.host.as_str(), s.user.as_str(), s.port),
            ("db-primary", "db.internal", "pg", 2200)
        );
    }

    #[test]
    fn rejects_unrelated_reg() {
        let dir = std::env::temp_dir().join(format!("gcm-putty-{}", crate::config::new_id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("other.reg");
        fs::write(
            &path,
            "Windows Registry Editor Version 5.00\n\n[HKEY_CURRENT_USER\\Software\\Other]\n",
        )
        .unwrap();
        assert!(import_reg(&path).is_err());
        fs::remove_dir_all(&dir).unwrap();
    }
}
