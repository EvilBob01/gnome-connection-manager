//! Turn a saved session into something to run. Terminal protocols run the
//! system's own `ssh`/`telnet`/shell inside a pseudo-terminal; RDP and VNC are
//! handed to an external client.

use anyhow::{Result, anyhow};

use crate::config::{Protocol, SerialSettings, Session, Settings};

/// Credentials typed into the terminal when the remote end prompts for them.
#[derive(Clone, Default, Debug)]
pub struct Autofill {
    pub user: Option<String>,
    pub password: Option<String>,
}

#[derive(Debug)]
pub enum Launch {
    Pty {
        program: String,
        args: Vec<String>,
        autofill: Autofill,
    },
    Serial {
        device: String,
        settings: SerialSettings,
    },
    /// The built-in Telnet client.
    Telnet {
        host: String,
        port: u16,
        autofill: Autofill,
    },
    External {
        program: String,
        args: Vec<String>,
    },
}

fn non_empty(s: &str) -> Option<String> {
    (!s.is_empty()).then(|| s.to_string())
}

fn split_args(extra: &str) -> Result<Vec<String>> {
    shlex::split(extra).ok_or_else(|| anyhow!("cannot parse extra arguments: {extra}"))
}

pub fn build(s: &Session, settings: &Settings) -> Result<Launch> {
    match s.protocol {
        Protocol::Ssh => {
            if s.host.is_empty() {
                return Err(anyhow!("no host set"));
            }
            let mut args = Vec::new();
            if s.port != 0 && s.port != 22 {
                args.extend(["-p".into(), s.port.to_string()]);
            }
            if !s.user.is_empty() {
                args.extend(["-l".into(), s.user.clone()]);
            }
            if s.keepalive > 0 {
                args.extend(["-o".into(), format!("ServerAliveInterval={}", s.keepalive)]);
            }
            for t in &s.tunnels {
                args.extend(tunnel_args(t)?);
            }
            if s.x11 {
                args.push("-X".into());
            }
            if s.agent_forward {
                args.push("-A".into());
            }
            if s.compression {
                args.push("-C".into());
            }
            if !s.private_key.is_empty() {
                args.extend(["-i".into(), s.private_key.clone()]);
            }
            args.extend(split_args(&s.extra_args)?);
            args.push(s.host.clone());
            Ok(Launch::Pty {
                program: settings.ssh_command.clone(),
                args,
                autofill: Autofill {
                    user: None,
                    password: non_empty(&s.password),
                },
            })
        }
        Protocol::Telnet if !settings.use_system_telnet => {
            if s.host.is_empty() {
                return Err(anyhow!("no host set"));
            }
            Ok(Launch::Telnet {
                host: s.host.clone(),
                port: s.effective_port(),
                autofill: Autofill {
                    user: non_empty(&s.user),
                    password: non_empty(&s.password),
                },
            })
        }
        Protocol::Mosh => {
            if s.host.is_empty() {
                return Err(anyhow!("no host set"));
            }
            // mosh starts the server over ssh; port and key go to that ssh.
            let mut ssh = vec![settings.ssh_command.clone()];
            if s.port != 0 && s.port != 22 {
                ssh.extend(["-p".into(), s.port.to_string()]);
            }
            let mut for_ssh = s.clone();
            for_ssh.extra_args.clear();
            ssh.extend(ssh_common_args(&for_ssh)?);
            let mut args = Vec::new();
            if ssh.len() > 1 || settings.ssh_command != "ssh" {
                let joined = shlex::try_join(ssh.iter().map(String::as_str))
                    .map_err(|e| anyhow!("cannot quote ssh options: {e}"))?;
                args.push(format!("--ssh={joined}"));
            }
            // Extra arguments are mosh's own, e.g. --predict=always.
            args.extend(split_args(&s.extra_args)?);
            args.push(if s.user.is_empty() {
                s.host.clone()
            } else {
                format!("{}@{}", s.user, s.host)
            });
            Ok(Launch::Pty {
                program: settings.mosh_command.clone(),
                args,
                autofill: Autofill {
                    user: None,
                    password: non_empty(&s.password),
                },
            })
        }
        Protocol::Telnet => {
            if s.host.is_empty() {
                return Err(anyhow!("no host set"));
            }
            let mut args = split_args(&s.extra_args)?;
            args.push(s.host.clone());
            args.push(s.effective_port().to_string());
            Ok(Launch::Pty {
                program: settings.telnet_command.clone(),
                args,
                autofill: Autofill {
                    user: non_empty(&s.user),
                    password: non_empty(&s.password),
                },
            })
        }
        Protocol::Local => {
            let shell = if !s.host.is_empty() {
                s.host.clone()
            } else if !settings.local_shell.is_empty() {
                settings.local_shell.clone()
            } else {
                default_shell()
            };
            Ok(Launch::Pty {
                program: shell,
                args: split_args(&s.extra_args)?,
                autofill: Autofill::default(),
            })
        }
        Protocol::Serial => {
            if s.host.is_empty() {
                return Err(anyhow!("no serial device set"));
            }
            Ok(Launch::Serial {
                device: s.host.clone(),
                settings: s.serial.clone(),
            })
        }
        Protocol::Rdp | Protocol::Vnc => {
            let template = if s.protocol == Protocol::Rdp {
                &settings.rdp_command
            } else {
                &settings.vnc_command
            };
            let (program, args) = expand_template(template, s)?;
            Ok(Launch::External { program, args })
        }
    }
}

/// ssh options shared by the terminal-less clients (sftp, tunnels): key,
/// keep-alive, compression, and the `-o`/`-F`/`-J`/`-i` options from the
/// session's extra arguments. Port and target are left to the caller, as
/// sftp spells the port flag differently.
pub fn ssh_common_args(s: &Session) -> Result<Vec<String>> {
    let mut args = Vec::new();
    if s.keepalive > 0 {
        args.extend(["-o".into(), format!("ServerAliveInterval={}", s.keepalive)]);
    }
    if s.compression {
        args.push("-C".into());
    }
    if !s.private_key.is_empty() {
        args.extend(["-i".into(), s.private_key.clone()]);
    }
    let mut it = split_args(&s.extra_args)?.into_iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "-o" | "-F" | "-J" | "-i" => {
                if let Some(v) = it.next() {
                    args.extend([a, v]);
                }
            }
            _ if a.starts_with("-o") && a.len() > 2 => args.push(a),
            _ => {}
        }
    }
    Ok(args)
}

const ASKPASS_FLAG: &str = "GCM_ASKPASS";
const ASKPASS_SECRET: &str = "GCM_ASKPASS_SECRET";

/// Entry point when ssh runs this binary as its askpass helper. Answers
/// password and passphrase prompts with the session's saved password and
/// refuses anything else (host-key confirmations, one-time codes), which
/// ssh treats as "no". Returns the process exit code.
pub fn askpass() -> Option<i32> {
    std::env::var_os(ASKPASS_FLAG)?;
    let prompt = std::env::args().nth(1).unwrap_or_default().to_lowercase();
    let secret = std::env::var(ASKPASS_SECRET).unwrap_or_default();
    if !secret.is_empty() && (prompt.contains("password") || prompt.contains("passphrase")) {
        println!("{secret}");
        Some(0)
    } else {
        Some(1)
    }
}

/// A command for an ssh-based program running without a terminal (sftp,
/// tunnels). Prompts go to [`askpass`]; on Windows no console window opens.
pub fn background_ssh(program: &str, password: &str) -> std::process::Command {
    let mut cmd = std::process::Command::new(program);
    if let Ok(exe) = std::env::current_exe() {
        cmd.env("SSH_ASKPASS", exe)
            .env("SSH_ASKPASS_REQUIRE", "force")
            .env(ASKPASS_FLAG, "1");
        // OpenSSH before 8.4 only uses askpass when DISPLAY is set.
        if cfg!(unix) && std::env::var_os("DISPLAY").is_none() {
            cmd.env("DISPLAY", ":0");
        }
    }
    if !password.is_empty() {
        cmd.env(ASKPASS_SECRET, password);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    cmd
}

/// Turn ssh's last words into something actionable.
pub fn explain_ssh_failure(out: &str) -> String {
    let out = out.trim();
    if out.contains("Host key verification failed") {
        format!("{out}\nConnect once in an SSH tab to accept the host key, then try again.")
    } else if out.is_empty() {
        "Connection closed".into()
    } else {
        out.to_string()
    }
}

pub fn default_shell() -> String {
    if cfg!(windows) {
        "powershell.exe".into()
    } else {
        std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into())
    }
}

/// `L 8080:host:80`, `R 9000:localhost:9000`, `D 1080`; a bare spec means `-L`.
fn tunnel_args(t: &str) -> Result<Vec<String>> {
    let t = t.trim();
    let (flag, spec) = match t.split_once(char::is_whitespace) {
        Some((f, spec)) => (f.to_ascii_uppercase(), spec.trim()),
        None => ("L".to_string(), t),
    };
    if !matches!(flag.as_str(), "L" | "R" | "D") || spec.is_empty() {
        return Err(anyhow!(
            "bad tunnel \"{t}\": use L/R/D followed by the forward spec"
        ));
    }
    Ok(vec![format!("-{flag}"), spec.to_string()])
}

/// Split the template shell-style, then fill placeholders in each word, so a
/// password with spaces stays one argument. Words whose placeholder value is
/// empty are dropped (e.g. `/p:{password}` with no saved password).
fn expand_template(template: &str, s: &Session) -> Result<(String, Vec<String>)> {
    let words = split_args(template)?;
    let vars = [
        ("{host}", s.host.clone()),
        ("{port}", s.effective_port().to_string()),
        ("{user}", s.user.clone()),
        ("{password}", s.password.clone()),
    ];
    let mut out = Vec::new();
    'word: for w in words {
        let mut w = w;
        for (k, v) in &vars {
            if w.contains(k) {
                if v.is_empty() {
                    continue 'word;
                }
                w = w.replace(k, v);
            }
        }
        out.push(w);
    }
    if out.is_empty() {
        return Err(anyhow!("empty command template"));
    }
    let program = out.remove(0);
    Ok((program, out))
}

/// Show a folder in the desktop's file manager.
pub fn open_path(path: &std::path::Path) -> Result<()> {
    let program = if cfg!(windows) {
        "explorer"
    } else if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    };
    spawn_external(program, &[path.to_string_lossy().into_owned()])
}

pub fn spawn_external(program: &str, args: &[String]) -> Result<()> {
    std::process::Command::new(program)
        .args(args)
        .stdin(std::process::Stdio::null())
        .spawn()
        .map_err(|e| anyhow!("cannot start {program}: {e}"))?;
    Ok(())
}

/// GCM-compatible startup commands: lines are sent together, `##D=<ms>`
/// adds a delay before the next batch. Returns (delay from connect in ms, text).
pub fn command_schedule(commands: &str, local: bool) -> Vec<(u64, String)> {
    let mut out = Vec::new();
    let mut at: u64 = if local { 700 } else { 3000 };
    let mut batch: Vec<&str> = Vec::new();
    for line in commands.lines() {
        if let Some(ms) = line
            .strip_prefix("##D=")
            .and_then(|d| d.parse::<u64>().ok())
        {
            if !batch.is_empty() {
                out.push((at, batch.join("\r") + "\r"));
                batch.clear();
            }
            at += ms;
        } else {
            batch.push(line);
        }
    }
    if !batch.is_empty() {
        out.push((at, batch.join("\r") + "\r"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ssh_args() {
        let mut s = Session::new(Protocol::Ssh);
        s.host = "h".into();
        s.user = "u".into();
        s.port = 2222;
        s.tunnels = vec![
            "L 8080:localhost:80".into(),
            "D 1080".into(),
            "9000:x:9000".into(),
        ];
        s.agent_forward = true;
        s.extra_args = "-o 'StrictHostKeyChecking accept-new'".into();
        let Launch::Pty { program, args, .. } = build(&s, &Settings::default()).unwrap() else {
            panic!()
        };
        assert_eq!(program, "ssh");
        assert_eq!(
            args,
            [
                "-p",
                "2222",
                "-l",
                "u",
                "-L",
                "8080:localhost:80",
                "-D",
                "1080",
                "-L",
                "9000:x:9000",
                "-A",
                "-o",
                "StrictHostKeyChecking accept-new",
                "h"
            ]
        );
        s.tunnels = vec!["X 1".into()];
        assert!(build(&s, &Settings::default()).is_err());
    }

    #[test]
    fn telnet_builtin_or_system() {
        let mut s = Session::new(Protocol::Telnet);
        s.host = "router".into();
        s.user = "admin".into();
        let mut settings = Settings::default();
        let Launch::Telnet {
            host,
            port,
            autofill,
        } = build(&s, &settings).unwrap()
        else {
            panic!()
        };
        assert_eq!(
            (host.as_str(), port, autofill.user.as_deref()),
            ("router", 23, Some("admin"))
        );
        settings.use_system_telnet = true;
        let Launch::Pty { program, args, .. } = build(&s, &settings).unwrap() else {
            panic!()
        };
        assert_eq!(
            (program.as_str(), args),
            ("telnet", vec!["router".to_string(), "23".to_string()])
        );
    }

    #[test]
    fn mosh_args() {
        let mut s = Session::new(Protocol::Mosh);
        s.host = "box".into();
        s.user = "bob".into();
        let Launch::Pty { program, args, .. } = build(&s, &Settings::default()).unwrap() else {
            panic!()
        };
        assert_eq!(
            (program.as_str(), args),
            ("mosh", vec!["bob@box".to_string()])
        );

        s.port = 2222;
        s.private_key = "/home/bob/.ssh/id ed25519".into();
        s.extra_args = "--predict=always".into();
        let Launch::Pty { args, .. } = build(&s, &Settings::default()).unwrap() else {
            panic!()
        };
        assert_eq!(
            args,
            [
                "--ssh=ssh -p 2222 -i '/home/bob/.ssh/id ed25519'",
                "--predict=always",
                "bob@box"
            ]
        );
    }

    #[test]
    fn rdp_template_drops_empty_placeholders() {
        let mut s = Session::new(Protocol::Rdp);
        s.host = "win".into();
        s.password = "a b".into();
        let (p, a) =
            expand_template("xfreerdp /v:{host}:{port} /u:{user} /p:{password}", &s).unwrap();
        assert_eq!(p, "xfreerdp");
        assert_eq!(a, ["/v:win:3389", "/p:a b"]);
    }

    #[test]
    fn schedule() {
        assert_eq!(
            command_schedule("ls\npwd\n##D=500\nuptime", false),
            vec![
                (3000, "ls\rpwd\r".to_string()),
                (3500, "uptime\r".to_string())
            ]
        );
        assert!(command_schedule("", true).is_empty());
    }
}
