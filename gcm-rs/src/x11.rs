//! X11 forwarding on Windows, where no X server runs by default: before an
//! SSH session with X11 forwarding starts, make sure one is listening (start
//! VcXsrv or Xming if needed) and give ssh a DISPLAY to forward to.
//! Linux and macOS desktops already provide both.

use std::net::{TcpStream, ToSocketAddrs};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow};

use crate::config::Settings;

/// Host and TCP port of an X display such as `localhost:0.0` or `:1`
/// (display N listens on 6000 + N).
pub fn display_addr(display: &str) -> Option<(String, u16)> {
    let (host, rest) = display.rsplit_once(':')?;
    let number: u16 = rest.split('.').next()?.parse().ok()?;
    let host = if host.is_empty() { "localhost" } else { host };
    Some((host.to_string(), 6000u16.checked_add(number)?))
}

fn listening(display: &str) -> bool {
    let Some((host, port)) = display_addr(display) else {
        return false;
    };
    let Ok(addrs) = (host.as_str(), port).to_socket_addrs() else {
        return false;
    };
    addrs
        .into_iter()
        .any(|a| TcpStream::connect_timeout(&a, Duration::from_millis(300)).is_ok())
}

/// Well-known X servers and the arguments that make them accept local
/// TCP clients in multi-window mode.
fn known_servers() -> Vec<(PathBuf, Vec<String>)> {
    let pf = |var: &str, default: &str| {
        PathBuf::from(std::env::var(var).unwrap_or_else(|_| default.into()))
    };
    let args = |a: &[&str]| a.iter().map(|s| s.to_string()).collect::<Vec<_>>();
    let mut out = Vec::new();
    for base in [
        pf("ProgramFiles", r"C:\Program Files"),
        pf("ProgramFiles(x86)", r"C:\Program Files (x86)"),
    ] {
        out.push((
            base.join("VcXsrv").join("vcxsrv.exe"),
            args(&[":0", "-multiwindow", "-clipboard", "-wgl", "-ac"]),
        ));
        out.push((
            base.join("Xming").join("Xming.exe"),
            args(&[":0", "-multiwindow", "-clipboard", "-ac"]),
        ));
    }
    out
}

/// The DISPLAY to give ssh, starting an X server first if none answers.
/// `Settings::x_server_command` overrides the built-in list of servers.
pub fn ensure(settings: &Settings) -> Result<String> {
    let display = std::env::var("DISPLAY")
        .ok()
        .filter(|d| !d.is_empty())
        .unwrap_or_else(|| settings.x_display.clone());
    if listening(&display) {
        return Ok(display);
    }
    let (program, args) = if settings.x_server_command.trim().is_empty() {
        known_servers()
            .into_iter()
            .find(|(p, _)| p.exists())
            .map(|(p, a)| (p.to_string_lossy().into_owned(), a))
            .ok_or_else(|| {
                anyhow!("no X server is running for {display}. Install VcXsrv, Xming or X410, or set Settings > X server command")
            })?
    } else {
        let mut words = shlex::split(&settings.x_server_command)
            .ok_or_else(|| anyhow!("cannot parse the X server command"))?;
        if words.is_empty() {
            return Err(anyhow!("empty X server command"));
        }
        let program = words.remove(0);
        (program, words)
    };
    crate::launch::spawn_external(&program, &args)?;
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(8) {
        if listening(&display) {
            return Ok(display);
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    Err(anyhow!(
        "started {program}, but nothing answers on {display}"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_addresses() {
        assert_eq!(
            display_addr("localhost:0.0"),
            Some(("localhost".into(), 6000))
        );
        assert_eq!(display_addr(":1"), Some(("localhost".into(), 6001)));
        assert_eq!(
            display_addr("10.0.0.5:10.0"),
            Some(("10.0.0.5".into(), 6010))
        );
        assert_eq!(display_addr("nonsense"), None);
    }

    #[test]
    fn detects_a_listening_server() {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        if port < 6000 {
            return; // cannot express this port as a display
        }
        assert!(listening(&format!("127.0.0.1:{}", port - 6000)));
    }
}
