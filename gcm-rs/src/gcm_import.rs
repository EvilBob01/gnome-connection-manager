//! Import hosts from the original Python GCM (`~/.gcm/gcm.conf`).

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow};
use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use sha2::{Digest, Sha256};

use crate::config::{Protocol, Session};

pub fn default_path() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join(".gcm").join("gcm.conf"))
}

/// Read every `[host ...]` section of a GCM config. Passwords are decrypted
/// with the key GCM keeps in `.gcm.key` beside the config file.
pub fn import(path: &Path) -> Result<Vec<Session>> {
    let text = fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let ini = parse_ini(&text);
    let key_file = fs::read_to_string(path.with_file_name(".gcm.key")).unwrap_or_default();
    let password = format!("{}{}", username(), key_file);
    let aes = ini
        .get("options")
        .and_then(|o| o.get("version"))
        .is_some_and(|v| v != "0");

    let mut out = Vec::new();
    for (section, kv) in &ini {
        if !section.starts_with("host ") {
            continue;
        }
        let get = |k: &str| kv.get(k).cloned().unwrap_or_default();
        let flag = |k: &str| {
            matches!(
                get(k).to_ascii_lowercase().as_str(),
                "1" | "true" | "yes" | "on"
            )
        };

        let host = get("host");
        let mut s = Session::new(match get("type").as_str() {
            "telnet" => Protocol::Telnet,
            _ if host.is_empty() => Protocol::Local,
            _ => Protocol::Ssh,
        });
        s.name = get("name");
        s.folder = get("group").trim_matches('/').to_string();
        s.description = get("description");
        s.host = host;
        s.user = get("user");
        s.port = get("port").trim().parse().unwrap_or(0);
        if s.port == s.protocol.default_port() {
            s.port = 0;
        }
        let enc = get("pass");
        if !enc.is_empty() {
            let r = if aes {
                decrypt_aes(&password, &enc)
            } else {
                decrypt_xor(&password, &enc)
            };
            s.password = r.unwrap_or_else(|e| {
                log::warn!("cannot decrypt GCM password for {}: {e}", s.name);
                String::new()
            });
        }
        s.private_key = get("private_key");
        s.extra_args = get("extra_params");
        s.commands = get("commands").replace('\0', "\n").replace("\\n", "\n");
        s.keepalive = get("keepalive").trim().parse().unwrap_or(0);
        s.x11 = flag("x11");
        s.agent_forward = flag("agent");
        s.compression = flag("compression");
        s.log = flag("log");
        let color = |k: &str| crate::config::parse_color(&get(k)).map(crate::config::color_hex);
        // GCM only applied colours when both were set.
        if let (Some(fg), Some(bg)) = (color("font-color"), color("back-color")) {
            s.fg_color = fg;
            s.bg_color = bg;
        }
        s.tunnels = get("tunnel")
            .split(',')
            .filter(|t| !t.is_empty())
            .map(|t| match t.strip_suffix(":*:*") {
                Some(port) => format!("D {port}"),
                None => format!("L {t}"),
            })
            .collect();
        out.push(s);
    }
    out.sort_by(|a, b| (&a.folder, &a.name).cmp(&(&b.folder, &b.name)));
    Ok(out)
}

fn username() -> String {
    ["USER", "LOGNAME", "USERNAME"]
        .iter()
        .find_map(|v| std::env::var(v).ok())
        .unwrap_or_default()
}

/// Minimal configparser-compatible reader: `[section]`, `key = value` or `key: value`.
fn parse_ini(text: &str) -> HashMap<String, HashMap<String, String>> {
    let mut out: HashMap<String, HashMap<String, String>> = HashMap::new();
    let mut current = String::new();
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') || trimmed.starts_with(';') {
            continue;
        }
        if let Some(name) = trimmed.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
            current = name.to_string();
            out.entry(current.clone()).or_default();
            continue;
        }
        let Some(pos) = trimmed.find(['=', ':']) else {
            continue;
        };
        let (k, v) = trimmed.split_at(pos);
        out.entry(current.clone())
            .or_default()
            .insert(k.trim().to_ascii_lowercase(), v[1..].trim().to_string());
    }
    out
}

/// pyAES.decrypt: OFB mode, key = SHA-256(password), the IV prepended, and
/// PKCS#7-style padding.
fn decrypt_aes(password: &str, enc: &str) -> Result<String> {
    let raw = B64.decode(enc.trim())?;
    if raw.len() < 32 || raw.len() % 16 != 0 {
        return Err(anyhow!("bad ciphertext length {}", raw.len()));
    }
    let key: [u8; 32] = Sha256::digest(password.as_bytes()).into();
    let cipher = PyAes::new(&key);
    let mut stream: [u8; 16] = raw[..16].try_into().unwrap();
    let mut plain = Vec::with_capacity(raw.len() - 16);
    for chunk in raw[16..].chunks(16) {
        cipher.encrypt_block(&mut stream);
        plain.extend(chunk.iter().zip(stream.iter()).map(|(c, k)| c ^ k));
    }
    let pad = *plain.last().unwrap() as usize;
    if pad == 0 || pad > 16 {
        return Err(anyhow!("bad padding (wrong key?)"));
    }
    plain.truncate(plain.len() - pad);
    Ok(String::from_utf8(plain)?)
}

/// The block cipher in GCM's pyAES.py. It keeps the state row-major but adds
/// round keys column-major, so it is not interoperable with standard AES and
/// has to be reproduced exactly.
struct PyAes {
    sbox: [u8; 256],
    round_keys: [u8; 240],
}

impl PyAes {
    fn new(key: &[u8; 32]) -> Self {
        let sbox = sbox();
        let mut ek = [0u8; 240];
        ek[..32].copy_from_slice(key);
        let mut rcon = 1u8;
        let mut size = 32;
        while size < 240 {
            let mut t = [ek[size - 4], ek[size - 3], ek[size - 2], ek[size - 1]];
            if size % 32 == 0 {
                t.rotate_left(1);
                t = t.map(|b| sbox[b as usize]);
                t[0] ^= rcon;
                rcon = gmul(rcon, 2);
            } else if size % 32 == 16 {
                t = t.map(|b| sbox[b as usize]);
            }
            for b in t {
                ek[size] = ek[size - 32] ^ b;
                size += 1;
            }
        }
        Self {
            sbox,
            round_keys: ek,
        }
    }

    fn encrypt_block(&self, state: &mut [u8; 16]) {
        let add = |state: &mut [u8; 16], round: usize| {
            for (s, k) in state
                .iter_mut()
                .zip(&self.round_keys[round * 16..round * 16 + 16])
            {
                *s ^= k;
            }
        };
        let sub_shift = |state: &mut [u8; 16]| {
            for b in state.iter_mut() {
                *b = self.sbox[*b as usize];
            }
            for row in 0..4 {
                state[row * 4..row * 4 + 4].rotate_left(row);
            }
        };
        add(state, 0);
        for round in 1..14 {
            sub_shift(state);
            for col in 0..4 {
                let c = [state[col], state[4 + col], state[8 + col], state[12 + col]];
                for row in 0..4 {
                    state[row * 4 + col] = gmul(c[row], 2)
                        ^ gmul(c[(row + 1) % 4], 3)
                        ^ c[(row + 2) % 4]
                        ^ c[(row + 3) % 4];
                }
            }
            add(state, round);
        }
        sub_shift(state);
        add(state, 14);
    }
}

fn gmul(mut a: u8, mut b: u8) -> u8 {
    let mut p = 0;
    while b != 0 {
        if b & 1 != 0 {
            p ^= a;
        }
        let hi = a & 0x80;
        a <<= 1;
        if hi != 0 {
            a ^= 0x1b;
        }
        b >>= 1;
    }
    p
}

/// The AES S-box: multiplicative inverse in GF(2^8) followed by the affine map.
fn sbox() -> [u8; 256] {
    let mut out = [0u8; 256];
    for (i, slot) in out.iter_mut().enumerate() {
        let inv = if i == 0 {
            0
        } else {
            (1..=255u8).find(|&x| gmul(i as u8, x) == 1).unwrap()
        };
        let mut s = inv;
        for shift in 1..5 {
            s ^= inv.rotate_left(shift);
        }
        *slot = s ^ 0x63;
    }
    out
}

/// GCM before 1.0: base64 of the password XORed with the key.
fn decrypt_xor(password: &str, enc: &str) -> Result<String> {
    let raw = B64.decode(enc.trim())?;
    let key = password.as_bytes();
    if key.is_empty() {
        return Err(anyhow!("empty key"));
    }
    let plain: Vec<u8> = raw
        .iter()
        .enumerate()
        .map(|(i, b)| b ^ key[i % key.len()])
        .collect();
    Ok(String::from_utf8_lossy(&plain).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    // Produced by the original pyAES.encrypt("hunter2 ü", "bobdeadbeef").
    const PYAES_SAMPLE: &str = "AAECAwQFBgcICQoLDA0OD3X165rbVQ3vjbbUuE4AY84=";

    #[test]
    fn pyaes_block_matches_python() {
        // pyAES.aesEncrypt(list(range(16)), passwordToKey(b"bobdeadbeef"))
        let key: [u8; 32] = Sha256::digest(b"bobdeadbeef").into();
        let mut block: [u8; 16] = std::array::from_fn(|i| i as u8);
        PyAes::new(&key).encrypt_block(&mut block);
        let hex: String = block.iter().map(|b| format!("{b:02x}")).collect();
        assert_eq!(hex, "1d8085eebe273fcf4e0ad2be480665c8");
    }

    #[test]
    fn decrypts_pyaes_output() {
        assert_eq!(
            decrypt_aes("bobdeadbeef", PYAES_SAMPLE).unwrap(),
            "hunter2 ü"
        );
    }

    #[test]
    fn imports_hosts() {
        let dir = std::env::temp_dir().join(format!("gcm-import-{}", crate::config::new_id()));
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join("gcm.conf"),
            "[options]\nversion = 1.2.0\n\n[host 1]\ngroup = prod/web\nname = web1\nhost = 10.0.0.5\nuser = admin\n\
             pass = \nport = 2222\ntunnel = 8080:localhost:80,1080:*:*\ntype = ssh\ncommands = uptime\\nw\n\
             keepalive = 30\nx11 = True\nagent = False\nfont-color = #fff\nback-color = rgb(0,0,64)\nlog = True\n\n[host 2]\ngroup = \nname = local\nhost = \nuser = \npass = \n",
        )
        .unwrap();
        let hosts = import(&dir.join("gcm.conf")).unwrap();
        fs::remove_dir_all(&dir).unwrap();
        assert_eq!(hosts.len(), 2);
        let web = hosts.iter().find(|h| h.name == "web1").unwrap();
        assert_eq!(web.folder, "prod/web");
        assert_eq!(web.port, 2222);
        assert_eq!(web.tunnels, vec!["L 8080:localhost:80", "D 1080"]);
        assert_eq!(web.commands, "uptime\nw");
        assert!(web.x11 && !web.agent_forward);
        assert_eq!(
            (web.fg_color.as_str(), web.bg_color.as_str(), web.log),
            ("#ffffff", "#000040", true)
        );
        assert_eq!(
            hosts.iter().find(|h| h.name == "local").unwrap().protocol,
            Protocol::Local
        );
    }
}
