//! SSH tunnel manager, in the spirit of MobaXterm's MobaSSHTunnel: saved
//! port forwards that run on their own, independent of terminal tabs.
//!
//! Each running tunnel is one `ssh -N` process using a saved SSH session's
//! host, user, key and options; passwords go through the askpass helper as
//! for SFTP. A local or dynamic tunnel counts as up once its port is
//! listening, a remote one once ssh has stayed connected for a few seconds.

use std::collections::{HashMap, VecDeque};
use std::io::{BufRead, BufReader};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow};
use serde::{Deserialize, Serialize};

use crate::config::{Protocol, Session, Settings};
use crate::launch;

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum TunnelKind {
    /// `-L`: a local port reaches a host as seen from the server.
    #[default]
    Local,
    /// `-R`: a port on the server reaches a host as seen from here.
    Remote,
    /// `-D`: a local SOCKS proxy through the server.
    Dynamic,
}

impl TunnelKind {
    pub const ALL: [TunnelKind; 3] = [TunnelKind::Local, TunnelKind::Remote, TunnelKind::Dynamic];

    pub fn label(self) -> &'static str {
        match self {
            TunnelKind::Local => "Local (-L)",
            TunnelKind::Remote => "Remote (-R)",
            TunnelKind::Dynamic => "Dynamic SOCKS (-D)",
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Default)]
#[serde(default)]
pub struct Tunnel {
    pub id: u64,
    pub name: String,
    /// The saved SSH session to tunnel through.
    pub session_id: u64,
    pub kind: TunnelKind,
    /// Listening address; empty means localhost (local/dynamic) or the
    /// server's default (remote).
    pub bind_address: String,
    pub listen_port: u16,
    pub dest_host: String,
    pub dest_port: u16,
    /// Start when gcm starts.
    pub autostart: bool,
}

/// IPv6 literals need brackets inside a forward spec.
fn host_part(h: &str) -> String {
    if h.contains(':') {
        format!("[{h}]")
    } else {
        h.to_string()
    }
}

impl Tunnel {
    pub fn new() -> Self {
        Tunnel {
            id: crate::config::new_id(),
            ..Default::default()
        }
    }

    pub fn display_name(&self) -> String {
        if self.name.is_empty() {
            self.summary()
        } else {
            self.name.clone()
        }
    }

    pub fn validate(&self) -> Result<()> {
        if self.listen_port == 0 {
            return Err(anyhow!("set a listening port"));
        }
        if self.kind != TunnelKind::Dynamic
            && (self.dest_host.trim().is_empty() || self.dest_port == 0)
        {
            return Err(anyhow!("set a destination host and port"));
        }
        Ok(())
    }

    /// The ssh flag and forward spec, e.g. `-L 127.0.0.1:8080:db:5432`.
    pub fn forward(&self) -> Result<[String; 2]> {
        self.validate()?;
        let bind = if self.bind_address.is_empty() {
            String::new()
        } else {
            format!("{}:", host_part(&self.bind_address))
        };
        let dest = format!("{}:{}", host_part(self.dest_host.trim()), self.dest_port);
        Ok(match self.kind {
            TunnelKind::Local => ["-L".into(), format!("{bind}{}:{dest}", self.listen_port)],
            TunnelKind::Remote => ["-R".into(), format!("{bind}{}:{dest}", self.listen_port)],
            TunnelKind::Dynamic => ["-D".into(), format!("{bind}{}", self.listen_port)],
        })
    }

    /// One-line description, e.g. `localhost:8080 → db:5432`.
    pub fn summary(&self) -> String {
        let bind = if self.bind_address.is_empty() {
            "localhost"
        } else {
            &self.bind_address
        };
        match self.kind {
            TunnelKind::Local => format!(
                "{bind}:{} → {}:{}",
                self.listen_port, self.dest_host, self.dest_port
            ),
            TunnelKind::Remote => format!(
                "server {}:{} → {}:{}",
                bind, self.listen_port, self.dest_host, self.dest_port
            ),
            TunnelKind::Dynamic => format!("SOCKS {bind}:{}", self.listen_port),
        }
    }

    /// Where to check that a local or dynamic tunnel is listening. None for
    /// remote tunnels and for bind addresses that are host names.
    fn local_probe(&self) -> Option<SocketAddr> {
        if self.kind == TunnelKind::Remote {
            return None;
        }
        let ip = match self.bind_address.as_str() {
            "" | "localhost" => IpAddr::V4(Ipv4Addr::LOCALHOST),
            "*" => IpAddr::V4(Ipv4Addr::UNSPECIFIED),
            a => a.trim_matches(['[', ']']).parse().ok()?,
        };
        Some(SocketAddr::new(ip, self.listen_port))
    }
}

/// `ssh` arguments for running `t` through `s`.
pub fn tunnel_args(t: &Tunnel, s: &Session) -> Result<Vec<String>> {
    if s.protocol != Protocol::Ssh {
        return Err(anyhow!("\"{}\" is not an SSH session", s.display_name()));
    }
    if s.host.is_empty() {
        return Err(anyhow!("session \"{}\" has no host", s.display_name()));
    }
    let mut args = vec!["-N".into(), "-o".into(), "ExitOnForwardFailure=yes".into()];
    if s.keepalive == 0 {
        // Notice a dead link instead of looking up forever.
        args.extend(["-o".into(), "ServerAliveInterval=30".into()]);
    }
    if s.port != 0 && s.port != 22 {
        args.extend(["-p".into(), s.port.to_string()]);
    }
    args.extend(launch::ssh_common_args(s)?);
    args.extend(t.forward()?);
    if !s.user.is_empty() {
        args.extend(["-l".into(), s.user.clone()]);
    }
    args.push(s.host.clone());
    Ok(args)
}

/// A tunnel nobody can see must not outlive gcm, even when gcm is killed
/// rather than closed. On Linux the kernel signals the child when its parent
/// dies (`PR_SET_PDEATHSIG`).
#[cfg(target_os = "linux")]
fn die_with_parent(cmd: &mut Command) {
    use std::os::unix::process::CommandExt;
    // SAFETY: prctl is async-signal-safe and touches no memory of the parent.
    unsafe {
        cmd.pre_exec(|| {
            libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM);
            Ok(())
        });
    }
}

#[cfg(not(target_os = "linux"))]
fn die_with_parent(_cmd: &mut Command) {}

/// Windows counterpart of [`die_with_parent`]: put the child in a job object
/// that kills its processes when gcm's last handle to it closes, which
/// happens however gcm exits.
#[cfg(windows)]
fn kill_on_exit_job(child: &Child) {
    use std::os::windows::io::AsRawHandle;
    use std::sync::OnceLock;
    use windows_sys::Win32::Foundation::HANDLE;
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
        JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
        SetInformationJobObject,
    };
    // The handle is deliberately never closed: closing it is what kills the tunnels.
    static JOB: OnceLock<usize> = OnceLock::new();
    // SAFETY: plain Win32 calls on a handle owned by this function.
    let job = *JOB.get_or_init(|| unsafe {
        let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
        let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
        info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        SetInformationJobObject(
            job,
            JobObjectExtendedLimitInformation,
            (&info as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
            std::mem::size_of_val(&info) as u32,
        );
        job as usize
    });
    if job != 0 {
        // SAFETY: both handles are valid for the duration of the call.
        unsafe {
            AssignProcessToJobObject(job as HANDLE, child.as_raw_handle() as HANDLE);
        }
    }
}

/// True if something is already listening on `addr`.
fn port_in_use(addr: SocketAddr) -> bool {
    matches!(TcpListener::bind(addr), Err(e) if e.kind() == std::io::ErrorKind::AddrInUse)
}

#[derive(Clone, Debug, PartialEq)]
pub enum Status {
    Stopped,
    Starting,
    Up,
    Failed(String),
}

struct Running {
    child: Child,
    started: Instant,
    probe: Option<SocketAddr>,
    up: bool,
    /// Last lines ssh wrote to stderr.
    log: Arc<Mutex<VecDeque<String>>>,
}

/// How long a remote tunnel's ssh must stay up before it counts as up.
const SETTLE: Duration = Duration::from_secs(3);

#[derive(Default)]
pub struct TunnelManager {
    running: HashMap<u64, Running>,
    failed: HashMap<u64, String>,
}

impl TunnelManager {
    /// Start `t`; a failure is also kept as the tunnel's status.
    pub fn start(&mut self, t: &Tunnel, sessions: &[Session], settings: &Settings) -> Result<()> {
        let r = self.try_start(t, sessions, settings);
        if let Err(e) = &r {
            self.failed.insert(t.id, e.to_string());
        }
        r
    }

    fn try_start(&mut self, t: &Tunnel, sessions: &[Session], settings: &Settings) -> Result<()> {
        let s = sessions
            .iter()
            .find(|s| s.id == t.session_id)
            .ok_or_else(|| anyhow!("the SSH session for this tunnel no longer exists"))?;
        let args = tunnel_args(t, s)?;
        let probe = t.local_probe();
        if let Some(addr) = probe
            && port_in_use(addr)
        {
            return Err(anyhow!(
                "port {} is already in use on this computer",
                t.listen_port
            ));
        }
        let mut cmd = launch::background_ssh(&settings.ssh_command, &s.password);
        cmd.args(&args);
        self.spawn(t.id, cmd, probe)
    }

    fn spawn(&mut self, id: u64, mut cmd: Command, probe: Option<SocketAddr>) -> Result<()> {
        self.stop(id);
        cmd.stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        die_with_parent(&mut cmd);
        let mut child = cmd.spawn().map_err(|e| anyhow!("cannot start ssh: {e}"))?;
        #[cfg(windows)]
        kill_on_exit_job(&child);
        let log = Arc::new(Mutex::new(VecDeque::new()));
        if let Some(stderr) = child.stderr.take() {
            let log = log.clone();
            thread::spawn(move || {
                for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                    let mut l = log.lock().unwrap();
                    if l.len() == 20 {
                        l.pop_front();
                    }
                    l.push_back(line);
                }
            });
        }
        self.failed.remove(&id);
        self.running.insert(
            id,
            Running {
                child,
                started: Instant::now(),
                probe,
                up: false,
                log,
            },
        );
        Ok(())
    }

    pub fn stop(&mut self, id: u64) {
        if let Some(mut r) = self.running.remove(&id) {
            let _ = r.child.kill();
            let _ = r.child.wait();
        }
        self.failed.remove(&id);
    }

    /// Check on every running tunnel: notice exits and mark tunnels up.
    pub fn poll(&mut self) {
        let mut exited = Vec::new();
        for (id, r) in &mut self.running {
            if let Ok(Some(status)) = r.child.try_wait() {
                // Give the stderr reader a moment to collect ssh's last words.
                thread::sleep(Duration::from_millis(30));
                let log = r.log.lock().unwrap();
                let text = log
                    .iter()
                    .map(String::as_str)
                    .collect::<Vec<_>>()
                    .join("\n");
                let msg = if text.trim().is_empty() {
                    format!("ssh exited ({status})")
                } else {
                    launch::explain_ssh_failure(&text)
                };
                exited.push((*id, msg));
                continue;
            }
            if !r.up {
                r.up = match r.probe {
                    Some(addr) => port_in_use(addr),
                    None => r.started.elapsed() >= SETTLE,
                };
            }
        }
        for (id, msg) in exited {
            self.running.remove(&id);
            self.failed.insert(id, msg);
        }
    }

    pub fn status(&self, id: u64) -> Status {
        match (self.running.get(&id), self.failed.get(&id)) {
            (Some(r), _) if r.up => Status::Up,
            (Some(_), _) => Status::Starting,
            (None, Some(msg)) => Status::Failed(msg.clone()),
            (None, None) => Status::Stopped,
        }
    }

    pub fn is_running(&self, id: u64) -> bool {
        self.running.contains_key(&id)
    }

    pub fn running_count(&self) -> usize {
        self.running.len()
    }

    pub fn up_count(&self) -> usize {
        self.running.values().filter(|r| r.up).count()
    }
}

impl Drop for TunnelManager {
    fn drop(&mut self) {
        let ids: Vec<u64> = self.running.keys().copied().collect();
        ids.into_iter().for_each(|id| self.stop(id));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session() -> Session {
        let mut s = Session::new(Protocol::Ssh);
        s.host = "gw".into();
        s.user = "bob".into();
        s.port = 2200;
        s.extra_args = "-o 'ProxyJump bastion' -v".into();
        s
    }

    #[test]
    fn forward_specs() {
        let mut t = Tunnel::new();
        t.listen_port = 8080;
        t.dest_host = "db".into();
        t.dest_port = 5432;
        assert_eq!(t.forward().unwrap(), ["-L", "8080:db:5432"]);
        t.bind_address = "0.0.0.0".into();
        t.kind = TunnelKind::Remote;
        assert_eq!(t.forward().unwrap(), ["-R", "0.0.0.0:8080:db:5432"]);
        t.kind = TunnelKind::Local;
        t.bind_address = "::1".into();
        t.dest_host = "fe80::2".into();
        assert_eq!(t.forward().unwrap(), ["-L", "[::1]:8080:[fe80::2]:5432"]);
        t.kind = TunnelKind::Dynamic;
        t.bind_address.clear();
        t.dest_host.clear();
        assert_eq!(t.forward().unwrap(), ["-D", "8080"]);
        t.listen_port = 0;
        assert!(t.forward().is_err());
        t.listen_port = 1;
        t.kind = TunnelKind::Local;
        assert!(t.forward().is_err(), "local tunnel needs a destination");
    }

    #[test]
    fn args() {
        let mut t = Tunnel::new();
        t.listen_port = 8080;
        t.dest_host = "db".into();
        t.dest_port = 5432;
        assert_eq!(
            tunnel_args(&t, &session()).unwrap(),
            [
                "-N",
                "-o",
                "ExitOnForwardFailure=yes",
                "-o",
                "ServerAliveInterval=30",
                "-p",
                "2200",
                "-o",
                "ProxyJump bastion",
                "-L",
                "8080:db:5432",
                "-l",
                "bob",
                "gw"
            ]
        );
        let mut telnet = session();
        telnet.protocol = Protocol::Telnet;
        assert!(tunnel_args(&t, &telnet).is_err());
    }

    #[test]
    fn probes() {
        let mut t = Tunnel::new();
        t.listen_port = 1080;
        assert_eq!(t.local_probe(), Some("127.0.0.1:1080".parse().unwrap()));
        t.bind_address = "::1".into();
        assert_eq!(t.local_probe(), Some("[::1]:1080".parse().unwrap()));
        t.bind_address = "myhost".into();
        assert_eq!(t.local_probe(), None);
        t.kind = TunnelKind::Remote;
        t.bind_address.clear();
        assert_eq!(t.local_probe(), None);
    }

    /// Lifecycle with stand-in processes: one that listens on the port like
    /// ssh would, one that fails with a message.
    #[cfg(unix)]
    #[test]
    fn lifecycle() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        let probe: SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();

        let mut m = TunnelManager::default();
        let mut cmd = Command::new("python3");
        cmd.args(["-c", &format!("import socket,time;s=socket.socket();s.bind(('127.0.0.1',{port}));s.listen();time.sleep(30)")]);
        if Command::new("python3").arg("-V").output().is_err() {
            eprintln!("python3 not available; skipping");
            return;
        }
        m.spawn(1, cmd, Some(probe)).unwrap();
        let t0 = Instant::now();
        while m.status(1) != Status::Up {
            assert!(
                t0.elapsed() < Duration::from_secs(10),
                "never came up: {:?}",
                m.status(1)
            );
            thread::sleep(Duration::from_millis(50));
            m.poll();
        }
        assert_eq!(m.up_count(), 1);
        m.stop(1);
        assert_eq!(m.status(1), Status::Stopped);
        thread::sleep(Duration::from_millis(200));
        assert!(!port_in_use(probe), "stopping must free the port");

        let mut bad = Command::new("sh");
        bad.args([
            "-c",
            "echo 'bind [127.0.0.1]:8080: Address already in use' >&2; exit 255",
        ]);
        m.spawn(2, bad, None).unwrap();
        let t0 = Instant::now();
        while !matches!(m.status(2), Status::Failed(_)) {
            assert!(t0.elapsed() < Duration::from_secs(10));
            thread::sleep(Duration::from_millis(50));
            m.poll();
        }
        assert_eq!(
            m.status(2),
            Status::Failed("bind [127.0.0.1]:8080: Address already in use".into())
        );
    }
}
