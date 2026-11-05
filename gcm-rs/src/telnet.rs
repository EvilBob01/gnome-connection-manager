//! Built-in Telnet client (RFC 854), so no external `telnet` program is
//! needed; Windows ships none by default.
//!
//! [`Protocol`] is a pure state machine: it strips commands out of what the
//! server sends, answers option negotiation, and encodes what the user types.
//! It agrees to the options a terminal needs (server echo, suppress go-ahead,
//! binary, window size, terminal type) and refuses everything else.
//! [`connect`] wraps it around a TCP connection for the terminal tab.

use std::io::{self, Read, Write};
use std::net::{Shutdown, TcpStream, ToSocketAddrs};
use std::sync::{Arc, Mutex};
use std::time::Duration;

const IAC: u8 = 255;
const DONT: u8 = 254;
const DO: u8 = 253;
const WONT: u8 = 252;
const WILL: u8 = 251;
const SB: u8 = 250;
const SE: u8 = 240;

const OPT_BINARY: u8 = 0;
const OPT_ECHO: u8 = 1;
const OPT_SGA: u8 = 3;
const OPT_TTYPE: u8 = 24;
const OPT_NAWS: u8 = 31;

const TTYPE_IS: u8 = 0;
const TTYPE_SEND: u8 = 1;

/// Terminal type reported to the server.
const TERMINAL_TYPE: &[u8] = b"XTERM-256COLOR";

#[derive(Clone, Copy, PartialEq, Debug)]
enum State {
    Data,
    Iac,
    /// After WILL/WONT/DO/DONT: the option byte follows.
    Negotiate(u8),
    Sub,
    SubIac,
}

pub struct Protocol {
    state: State,
    sub: Vec<u8>,
    /// The server asked for our window size (DO NAWS, answered WILL).
    naws: bool,
    /// The server sends binary data and accepts it from us.
    binary_out: bool,
    size: (u16, u16),
    /// Options already answered, so a server repeating itself gets no reply
    /// and negotiation cannot loop.
    answered: Vec<(u8, u8)>,
}

impl Protocol {
    pub fn new(cols: u16, rows: u16) -> Self {
        Protocol {
            state: State::Data,
            sub: Vec::new(),
            naws: false,
            binary_out: false,
            size: (cols, rows),
            // What greeting() already said; the server agreeing needs no answer.
            answered: vec![
                (WILL, OPT_TTYPE),
                (WILL, OPT_NAWS),
                (DO, OPT_SGA),
                (DO, OPT_ECHO),
            ],
        }
    }

    /// What a client says first: it will report its type and size, and
    /// wants the server to echo and not send go-aheads.
    pub fn greeting() -> Vec<u8> {
        vec![
            IAC, WILL, OPT_TTYPE, IAC, WILL, OPT_NAWS, IAC, DO, OPT_SGA, IAC, DO, OPT_ECHO,
        ]
    }

    /// Split server output into terminal data and protocol replies.
    pub fn receive(&mut self, input: &[u8], data: &mut Vec<u8>, reply: &mut Vec<u8>) {
        for &b in input {
            self.state = match self.state {
                State::Data if b == IAC => State::Iac,
                State::Data => {
                    data.push(b);
                    State::Data
                }
                State::Iac => match b {
                    IAC => {
                        data.push(IAC);
                        State::Data
                    }
                    WILL | WONT | DO | DONT => State::Negotiate(b),
                    SB => {
                        self.sub.clear();
                        State::Sub
                    }
                    // NOP, data mark, break, go-ahead and the rest carry nothing for us.
                    _ => State::Data,
                },
                State::Negotiate(verb) => {
                    self.negotiate(verb, b, reply);
                    State::Data
                }
                State::Sub if b == IAC => State::SubIac,
                State::Sub => {
                    self.push_sub(b);
                    State::Sub
                }
                State::SubIac if b == SE => {
                    self.subnegotiation(reply);
                    State::Data
                }
                State::SubIac => {
                    // IAC IAC inside a subnegotiation is a literal 255.
                    self.push_sub(b);
                    State::Sub
                }
            };
        }
    }

    /// Subnegotiations we understand are a few bytes; a server that never
    /// ends one must not fill memory.
    fn push_sub(&mut self, b: u8) {
        const MAX_SUB: usize = 4096;
        if self.sub.len() < MAX_SUB {
            self.sub.push(b);
        }
    }

    fn negotiate(&mut self, verb: u8, opt: u8, reply: &mut Vec<u8>) {
        let mut send_size = false;
        let response = match (verb, opt) {
            (DO, OPT_NAWS) => {
                self.naws = true;
                send_size = true;
                WILL
            }
            (DO, OPT_TTYPE) | (DO, OPT_SGA) => WILL,
            (DO, OPT_BINARY) => {
                self.binary_out = true;
                WILL
            }
            (DO, _) => WONT,
            (DONT, OPT_NAWS) => {
                self.naws = false;
                WONT
            }
            (DONT, OPT_BINARY) => {
                self.binary_out = false;
                WONT
            }
            (DONT, _) => WONT,
            (WILL, OPT_ECHO | OPT_SGA | OPT_BINARY) => DO,
            (WILL, _) => DONT,
            (WONT, _) => DONT,
            _ => return,
        };
        // Reply once per (answer, option); RFC 854 forbids acknowledging an
        // option that is already in the requested state.
        if !self.answered.contains(&(response, opt)) {
            self.answered.retain(|&(_, o)| o != opt);
            self.answered.push((response, opt));
            reply.extend([IAC, response, opt]);
        }
        // The size follows the agreement, also when the agreement was ours.
        if send_size {
            reply.extend(self.naws_message());
        }
    }

    fn subnegotiation(&mut self, reply: &mut Vec<u8>) {
        if self.sub.first() == Some(&OPT_TTYPE) && self.sub.get(1) == Some(&TTYPE_SEND) {
            reply.extend([IAC, SB, OPT_TTYPE, TTYPE_IS]);
            reply.extend(TERMINAL_TYPE);
            reply.extend([IAC, SE]);
        }
    }

    fn naws_message(&self) -> Vec<u8> {
        let mut out = vec![IAC, SB, OPT_NAWS];
        for v in [self.size.0, self.size.1] {
            for b in v.to_be_bytes() {
                out.push(b);
                if b == IAC {
                    out.push(IAC);
                }
            }
        }
        out.extend([IAC, SE]);
        out
    }

    /// Record a new window size; returns the message to send if the server
    /// asked for sizes.
    pub fn resize(&mut self, cols: u16, rows: u16) -> Option<Vec<u8>> {
        if self.size == (cols, rows) {
            return None;
        }
        self.size = (cols, rows);
        self.naws.then(|| self.naws_message())
    }

    /// Encode keyboard input: double IAC bytes, and outside binary mode
    /// send CR as CR NUL as the protocol requires.
    pub fn encode(&self, input: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(input.len() + 4);
        for &b in input {
            out.push(b);
            if b == IAC {
                out.push(IAC);
            } else if b == b'\r' && !self.binary_out {
                out.push(0);
            }
        }
        out
    }
}

struct Shared {
    proto: Protocol,
    stream: TcpStream,
}

/// Reads server output with protocol traffic removed and answered.
pub struct Reader {
    shared: Arc<Mutex<Shared>>,
    stream: TcpStream,
    buf: Vec<u8>,
    pending: Vec<u8>,
}

impl Read for Reader {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        while self.pending.is_empty() {
            let n = self.stream.read(&mut self.buf)?;
            if n == 0 {
                return Ok(0);
            }
            let mut reply = Vec::new();
            let mut shared = self.shared.lock().unwrap();
            shared
                .proto
                .receive(&self.buf[..n], &mut self.pending, &mut reply);
            if !reply.is_empty() {
                shared.stream.write_all(&reply)?;
            }
        }
        let n = out.len().min(self.pending.len());
        out[..n].copy_from_slice(&self.pending[..n]);
        self.pending.drain(..n);
        Ok(n)
    }
}

/// Sends keyboard input, encoded.
pub struct Writer {
    shared: Arc<Mutex<Shared>>,
}

impl Write for Writer {
    fn write(&mut self, input: &[u8]) -> io::Result<usize> {
        let mut shared = self.shared.lock().unwrap();
        let bytes = shared.proto.encode(input);
        shared.stream.write_all(&bytes)?;
        Ok(input.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.shared.lock().unwrap().stream.flush()
    }
}

/// Window size changes and hang-up.
pub struct Control {
    shared: Arc<Mutex<Shared>>,
}

impl Control {
    pub fn resize(&self, cols: u16, rows: u16) {
        let mut shared = self.shared.lock().unwrap();
        if let Some(msg) = shared.proto.resize(cols, rows) {
            let _ = shared.stream.write_all(&msg);
        }
    }

    pub fn close(&self) {
        let _ = self.shared.lock().unwrap().stream.shutdown(Shutdown::Both);
    }
}

/// Connect to `host:port`, trying each address with a timeout.
pub fn connect(
    host: &str,
    port: u16,
    cols: u16,
    rows: u16,
) -> io::Result<(Reader, Writer, Control)> {
    let mut last = io::Error::new(io::ErrorKind::NotFound, format!("{host}: no address found"));
    let mut stream = None;
    for addr in (host, port).to_socket_addrs()? {
        match TcpStream::connect_timeout(&addr, Duration::from_secs(10)) {
            Ok(s) => {
                stream = Some(s);
                break;
            }
            Err(e) => last = e,
        }
    }
    let stream = stream.ok_or(last)?;
    stream.set_nodelay(true)?;
    let mut write_half = stream.try_clone()?;
    write_half.write_all(&Protocol::greeting())?;
    let shared = Arc::new(Mutex::new(Shared {
        proto: Protocol::new(cols, rows),
        stream: write_half,
    }));
    Ok((
        Reader {
            shared: shared.clone(),
            stream,
            buf: vec![0; 16 * 1024],
            pending: Vec::new(),
        },
        Writer {
            shared: shared.clone(),
        },
        Control { shared },
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;
    use std::thread;

    #[test]
    fn endless_subnegotiation_is_bounded() {
        let mut p = Protocol::new(80, 24);
        let (mut data, mut reply) = (Vec::new(), Vec::new());
        p.receive(&[IAC, SB], &mut data, &mut reply);
        p.receive(&vec![b'x'; 1 << 20], &mut data, &mut reply);
        assert!(p.sub.len() <= 4096);
    }

    fn run(p: &mut Protocol, input: &[u8]) -> (Vec<u8>, Vec<u8>) {
        let (mut data, mut reply) = (Vec::new(), Vec::new());
        p.receive(input, &mut data, &mut reply);
        (data, reply)
    }

    #[test]
    fn strips_and_answers_negotiation() {
        let mut p = Protocol::new(80, 24);
        let (data, reply) = run(
            &mut p,
            &[
                b'h', IAC, WILL, OPT_BINARY, b'i', IAC, DO, 39, IAC, IAC, b'!',
            ],
        );
        assert_eq!(data, [b'h', b'i', IAC, b'!']);
        assert_eq!(reply, [IAC, DO, OPT_BINARY, IAC, WONT, 39]);
        // Repeats are not answered again, so negotiation cannot loop.
        let (_, reply) = run(&mut p, &[IAC, WILL, OPT_BINARY]);
        assert!(reply.is_empty());
        // The greeting already asked for echo; the server agreeing needs no reply.
        let (_, reply) = run(&mut p, &[IAC, WILL, OPT_ECHO]);
        assert!(reply.is_empty());
    }

    #[test]
    fn window_size_and_terminal_type() {
        let mut p = Protocol::new(80, 24);
        let (_, reply) = run(&mut p, &[IAC, DO, OPT_NAWS]);
        assert_eq!(reply, [IAC, SB, OPT_NAWS, 0, 80, 0, 24, IAC, SE]);
        // 255 columns must be doubled inside the subnegotiation.
        assert_eq!(
            p.resize(255, 50).unwrap(),
            [IAC, SB, OPT_NAWS, 0, 255, 255, 0, 50, IAC, SE]
        );
        assert!(p.resize(255, 50).is_none());

        let (data, reply) = run(&mut p, &[IAC, SB, OPT_TTYPE, TTYPE_SEND, IAC, SE, b'x']);
        assert_eq!(data, b"x");
        let mut want = vec![IAC, SB, OPT_TTYPE, TTYPE_IS];
        want.extend(TERMINAL_TYPE);
        want.extend([IAC, SE]);
        assert_eq!(reply, want);
    }

    #[test]
    fn split_across_reads() {
        let mut p = Protocol::new(80, 24);
        let (d1, r1) = run(&mut p, &[b'a', IAC]);
        let (d2, r2) = run(&mut p, &[WILL]);
        let (d3, r3) = run(&mut p, &[OPT_BINARY, b'b']);
        assert_eq!([d1, d2, d3].concat(), b"ab");
        assert_eq!([r1, r2, r3].concat(), [IAC, DO, OPT_BINARY]);
    }

    #[test]
    fn encodes_input() {
        let mut p = Protocol::new(80, 24);
        assert_eq!(
            p.encode(&[b'l', b's', b'\r', IAC]),
            [b'l', b's', b'\r', 0, IAC, IAC]
        );
        run(&mut p, &[IAC, DO, OPT_BINARY]);
        assert_eq!(p.encode(b"\r"), b"\r");
    }

    /// A tiny server: negotiates, asks for login, and checks what comes back.
    #[test]
    fn talks_to_a_server() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = thread::spawn(move || {
            let (mut s, _) = listener.accept().unwrap();
            s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
            s.write_all(&[IAC, DO, OPT_NAWS, IAC, WILL, OPT_ECHO])
                .unwrap();
            s.write_all(b"login: ").unwrap();
            let mut got = Vec::new();
            let mut buf = [0u8; 256];
            while !got.ends_with(&[b'\r', 0]) {
                let n = s.read(&mut buf).unwrap();
                assert!(n > 0, "client closed early: {got:?}");
                got.extend_from_slice(&buf[..n]);
            }
            s.write_all(b"\r\nwelcome\r\n").unwrap();
            got
        });

        let (mut reader, mut writer, control) = connect("127.0.0.1", port, 100, 30).unwrap();
        let mut seen = Vec::new();
        let mut buf = [0u8; 64];
        while !seen.ends_with(b"login: ") {
            let n = reader.read(&mut buf).unwrap();
            seen.extend_from_slice(&buf[..n]);
        }
        assert_eq!(seen, b"login: ", "commands must not reach the terminal");
        writer.write_all(b"bob\r").unwrap();
        while !String::from_utf8_lossy(&seen).contains("welcome") {
            let n = reader.read(&mut buf).unwrap();
            seen.extend_from_slice(&buf[..n]);
        }
        control.close();

        let got = server.join().unwrap();
        let greeting = Protocol::greeting();
        assert!(got.starts_with(&greeting));
        let rest = &got[greeting.len()..];
        // The window size (asked for with DO NAWS), nothing for the echo the
        // greeting already requested, then the typed login with CR NUL.
        assert_eq!(
            rest,
            [&[IAC, SB, OPT_NAWS, 0, 100, 0, 30, IAC, SE][..], b"bob\r\0"].concat()
        );
    }
}
