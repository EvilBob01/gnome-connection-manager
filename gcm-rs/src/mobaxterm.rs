//! Import sessions from a MobaXterm `.mxtsessions` export.
//!
//! The format is undocumented; this follows the community write-up at
//! <https://gist.github.com/Ruzgfpegk/ab597838e4abbe8de30d7224afd062ea>
//! (MobaXterm 26.3). It is an INI file in Windows-1252: `[Bookmarks]` and
//! `[Bookmarks_N]` sections, each with `SubRep=` (folder, `\`-separated) and
//! one `Name=value` line per session. A value is
//! `<logout flag>#<icon>#<settings>#<terminal>#<start mode>#<comment>#<tab colour>`,
//! where `<settings>` is `%`-separated with the session type first.
//!
//! Only types whose field layout is documented are imported (SSH, SFTP,
//! RDP, VNC); others are reported as skipped rather than guessed.

use std::fs;
use std::path::Path;

use anyhow::{Context, Result};

use crate::config::{Imported, Protocol, Session, color_hex};

/// Windows-1252 differs from Latin-1 only in 0x80..=0x9F.
fn decode_cp1252(bytes: &[u8]) -> String {
    const HIGH: [char; 32] = [
        '€', '\u{81}', '‚', 'ƒ', '„', '…', '†', '‡', 'ˆ', '‰', 'Š', '‹', 'Œ', '\u{8d}', 'Ž',
        '\u{8f}', '\u{90}', '‘', '’', '“', '”', '•', '–', '—', '˜', '™', 'š', '›', 'œ', '\u{9d}',
        'ž', 'Ÿ',
    ];
    // A file saved by another tool may already be UTF-8.
    if let Ok(s) = std::str::from_utf8(bytes) {
        return s.to_string();
    }
    bytes
        .iter()
        .map(|&b| {
            if (0x80..0xa0).contains(&b) {
                HIGH[(b - 0x80) as usize]
            } else {
                b as char
            }
        })
        .collect()
}

/// MobaXterm's escapes inside fields.
fn unescape(s: &str) -> String {
    s.replace("__PTVIRG__", ";")
        .replace("__DBLQUO__", "\"")
        .replace("__PIPE__", "|")
        .replace("__DIEZE__", "#")
        .replace("__PERCENT__", "%")
        .replace("_CurrentDrive_", "C")
}

fn is_on(v: &str) -> bool {
    v == "-1"
}

/// "R,G,B" to `#rrggbb`, unless it is MobaXterm's default.
fn color(field: &str, default: &str) -> String {
    if field == default {
        return String::new();
    }
    let parts: Vec<u8> = field
        .split(',')
        .filter_map(|p| p.trim().parse().ok())
        .collect();
    match parts[..] {
        [r, g, b] => color_hex([r, g, b]),
        _ => String::new(),
    }
}

pub fn import(path: &Path) -> Result<Imported> {
    let bytes = fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    Ok(parse(&decode_cp1252(&bytes)))
}

pub fn parse(text: &str) -> Imported {
    let mut out = Imported {
        sessions: Vec::new(),
        skipped: Vec::new(),
    };
    let mut folder = String::new();
    let mut in_bookmarks = false;
    for line in text.lines() {
        let line = line.trim_end_matches('\r');
        if let Some(section) = line
            .trim()
            .strip_prefix('[')
            .and_then(|l| l.strip_suffix(']'))
        {
            in_bookmarks = section == "Bookmarks" || section.starts_with("Bookmarks_");
            folder.clear();
            continue;
        }
        if !in_bookmarks {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        match key {
            "SubRep" => {
                folder = value.trim().replace('\\', "/");
                // A full export's root is shown as "User sessions" but is not a real folder.
                if folder == "User sessions" {
                    folder.clear();
                }
            }
            "ImgNum" => {}
            name => match session(name.trim(), value, &folder) {
                Ok(s) => out.sessions.push(s),
                Err(why) => out.skipped.push(format!("{}: {why}", name.trim())),
            },
        }
    }
    out
}

fn session(name: &str, value: &str, folder: &str) -> std::result::Result<Session, String> {
    let groups: Vec<&str> = value.split('#').collect();
    let settings: Vec<&str> = groups
        .get(2)
        .ok_or("not a session line")?
        .split('%')
        .collect();
    let term: Vec<&str> = groups
        .get(3)
        .map(|g| g.split('%').collect())
        .unwrap_or_default();
    let get = |i: usize| settings.get(i).copied().unwrap_or("").trim();
    let tget = |i: usize| term.get(i).copied().unwrap_or("").trim();

    let protocol = match get(0) {
        "0" | "7" => Protocol::Ssh,
        "4" => Protocol::Rdp,
        "5" => Protocol::Vnc,
        other => return Err(format!("session type {other} is not supported yet")),
    };
    let mut s = Session::new(protocol);
    s.name = name.to_string();
    s.folder = folder.to_string();
    s.host = get(1).to_string();
    if s.host.is_empty() {
        return Err("no host".into());
    }
    s.port = get(2).parse().unwrap_or(0);
    if s.port == protocol.default_port() {
        s.port = 0;
    }
    s.description = groups
        .get(5)
        .map(|c| unescape(c.trim()))
        .unwrap_or_default();

    match get(0) {
        "0" => {
            let user = get(3);
            if user != "<default>" {
                s.user = user.to_string();
            }
            s.x11 = is_on(get(5));
            s.compression = is_on(get(6));
            s.commands = unescape(get(7));
            s.private_key = unescape(get(14));
            s.agent_forward = is_on(get(34));
            // Jump hosts: parallel `__PIPE__`-separated lists of host, port and user.
            let hosts = get(8);
            if !hosts.is_empty() {
                let split = |v: &str| v.split("__PIPE__").map(str::to_string).collect::<Vec<_>>();
                let (hosts, ports, users) = (split(hosts), split(get(9)), split(get(10)));
                let jumps: Vec<String> = hosts
                    .iter()
                    .enumerate()
                    .filter(|(_, h)| !h.is_empty())
                    .map(|(i, h)| {
                        let user = users
                            .get(i)
                            .filter(|u| !u.is_empty())
                            .map(|u| format!("{u}@"))
                            .unwrap_or_default();
                        let port = ports
                            .get(i)
                            .filter(|p| !p.is_empty() && *p != "22")
                            .map(|p| format!(":{p}"))
                            .unwrap_or_default();
                        format!("{user}{h}{port}")
                    })
                    .collect();
                // The field is split again like a command line, so anything
                // beyond plain host-name characters is quoted.
                let joined = jumps.join(",");
                let plain = joined
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || "@,:._-[]%".contains(c));
                if !jumps.is_empty()
                    && !jumps.iter().any(|j| j.starts_with('-'))
                    && let Ok(quoted) = shlex::try_quote(&joined)
                {
                    s.extra_args = if plain {
                        format!("-J {joined}")
                    } else {
                        format!("-J {quoted}")
                    };
                }
            }
            s.fg_color = color(tget(6), "236,236,236");
            s.bg_color = color(tget(7), "30,30,30");
            s.log = is_on(tget(11));
        }
        // SFTP sessions become SSH sessions; the SFTP pane browses them.
        "7" => {
            s.user = get(3).to_string();
            s.compression = is_on(get(5));
            s.private_key = unescape(get(9));
        }
        "4" => s.user = get(3).to_string(),
        _ => {}
    }
    Ok(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    // The two SSH examples are verbatim from the format write-up.
    const SAMPLE: &str = "[Bookmarks]\r\nSubRep=\r\nImgNum=42\r\n\
Reference Session= #109#0%localhost%22%<default>%%-1%-1%%%%%0%0%0%%%-1%0%0%0%%1080%%0%0%1%%0%%%%0%-1%-1%0#MobaFont%10%0%0%-1%15%236,236,236%30,30,30%180,180,192%0%-1%0%%xterm%-1%0%_Std_Colors_0_%80%24%0%0%-1%<none>%%0%0%-1%-1#0# #-1\r\n\
\r\n[Bookmarks_1]\r\nSubRep=Lab\\Linux\r\nImgNum=41\r\n\
Deck=#109#0%192.168.137.40%2222%deck%%0%0%uptime__PTVIRG__ w%bastion__PIPE__inner%22__PIPE__2200%jump__PIPE__%0%0%0%_CurrentDrive_:\\keys\\deck.pem%%-1%0%0%0%%1080%%0%0%1%#MobaFont%10%0%0%-1%15%255,204,102%43,15,15%180,180,192%0%-1%-1%%xterm%-1%0%_Std_Colors_0_%80%24%0%1%-1%<none>%%0%0%-1%-1#0#prod __DIEZE__1#-1\r\n\
rdp-desktop=#91#4%192.168.1.50%3389%admin%0%0%0%0%-1%0%0%-1#MobaFont#0# #-1\r\n\
vnc-box=#128#5%10.0.0.9%5901%-1%0#MobaFont#0# #-1\r\n\
files=#140#7%sftp.example.org%22%bob%-1%-1%%0%0%#MobaFont#0# #-1\r\n\
router=#98#1%192.168.1.1%23%admin#MobaFont#0# #-1\r\n\
\r\n[Misc]\r\nNotASession=#109#0%x%22\r\n";

    #[test]
    fn parses_sessions() {
        let r = parse(SAMPLE);
        assert_eq!(r.sessions.len(), 5, "{:?}", r.skipped);
        assert_eq!(r.skipped.len(), 1);
        assert!(r.skipped[0].starts_with("router: session type 1"));

        let reference = &r.sessions[0];
        assert_eq!(
            (
                reference.name.as_str(),
                reference.host.as_str(),
                reference.port
            ),
            ("Reference Session", "localhost", 0)
        );
        assert_eq!(reference.user, "", "<default> means no user");
        assert!(reference.x11 && reference.compression);
        assert_eq!(
            (reference.fg_color.as_str(), reference.bg_color.as_str()),
            ("", ""),
            "defaults are not imported"
        );
        assert_eq!(reference.folder, "");

        let deck = &r.sessions[1];
        assert_eq!(deck.folder, "Lab/Linux");
        assert_eq!(
            (deck.host.as_str(), deck.port, deck.user.as_str()),
            ("192.168.137.40", 2222, "deck")
        );
        assert!(!deck.x11 && !deck.compression);
        assert_eq!(deck.commands, "uptime; w");
        assert_eq!(deck.extra_args, "-J jump@bastion,inner:2200");
        assert_eq!(deck.private_key, "C:\\keys\\deck.pem");
        assert_eq!(
            (deck.fg_color.as_str(), deck.bg_color.as_str()),
            ("#ffcc66", "#2b0f0f")
        );
        assert!(deck.log);
        assert_eq!(deck.description, "prod #1");

        let rdp = &r.sessions[2];
        assert_eq!(
            (rdp.protocol, rdp.host.as_str(), rdp.port, rdp.user.as_str()),
            (Protocol::Rdp, "192.168.1.50", 0, "admin")
        );
        let vnc = &r.sessions[3];
        assert_eq!((vnc.protocol, vnc.port), (Protocol::Vnc, 5901));
        let sftp = &r.sessions[4];
        assert_eq!(
            (sftp.protocol, sftp.user.as_str(), sftp.compression),
            (Protocol::Ssh, "bob", true)
        );
    }

    #[test]
    fn decodes_cp1252() {
        assert_eq!(decode_cp1252(b"caf\xe9 \x80"), "café €");
        assert_eq!(decode_cp1252("café".as_bytes()), "café");
    }
}
