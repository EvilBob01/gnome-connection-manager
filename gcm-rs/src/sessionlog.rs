//! Session logging: terminal output saved as plain text, one file per
//! session start, like GCM's per-host log option.
//!
//! Escape sequences (colours, cursor movement, titles) are removed so the
//! file reads like the screen did. A carriage return not followed by a line
//! feed starts the line over, so progress bars leave only their final state,
//! and backspace removes the previous character.

use std::fs::{self, File};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

#[derive(Clone, Copy, PartialEq, Debug)]
enum State {
    Ground,
    Esc,
    Csi,
    /// OSC, DCS, SOS, PM, APC: skipped up to BEL or ESC \.
    Str,
    StrEsc,
    /// ESC ( B and friends: skip one more byte.
    Charset,
}

/// Turns raw terminal output into lines of text.
pub struct Stripper {
    state: State,
    line: Vec<u8>,
    cr: bool,
}

impl Default for Stripper {
    fn default() -> Self {
        Stripper {
            state: State::Ground,
            line: Vec::new(),
            cr: false,
        }
    }
}

impl Stripper {
    /// Feed output; completed lines (with `\n`) are appended to `out`.
    pub fn feed(&mut self, bytes: &[u8], out: &mut Vec<u8>) {
        for &b in bytes {
            match self.state {
                State::Ground => self.ground(b, out),
                State::Esc => {
                    self.state = match b {
                        b'[' => State::Csi,
                        b']' | b'P' | b'X' | b'^' | b'_' => State::Str,
                        b'(' | b')' | b'*' | b'+' => State::Charset,
                        _ => State::Ground,
                    }
                }
                State::Csi => {
                    if (0x40..=0x7e).contains(&b) {
                        self.state = State::Ground;
                    }
                }
                State::Str => match b {
                    0x07 => self.state = State::Ground,
                    0x1b => self.state = State::StrEsc,
                    _ => {}
                },
                State::StrEsc => {
                    self.state = if b == b'\\' {
                        State::Ground
                    } else {
                        State::Str
                    }
                }
                State::Charset => self.state = State::Ground,
            }
        }
    }

    fn ground(&mut self, b: u8, out: &mut Vec<u8>) {
        if self.cr && b != b'\n' {
            // A bare CR returns to the start of the line: what follows replaces it.
            self.line.clear();
        }
        self.cr = false;
        match b {
            0x1b => self.state = State::Esc,
            b'\n' => {
                out.extend_from_slice(&self.line);
                out.push(b'\n');
                self.line.clear();
            }
            b'\r' => self.cr = true,
            0x08 => {
                // Drop one UTF-8 character.
                while let Some(c) = self.line.pop() {
                    if c & 0xc0 != 0x80 {
                        break;
                    }
                }
            }
            b'\t' => self.line.push(b),
            0x00..=0x1f | 0x7f => {}
            _ => self.line.push(b),
        }
    }

    /// The unfinished last line, e.g. a shell prompt.
    pub fn finish(&mut self, out: &mut Vec<u8>) {
        if !self.line.is_empty() {
            out.extend_from_slice(&self.line);
            out.push(b'\n');
            self.line.clear();
        }
    }
}

pub struct Logger {
    file: BufWriter<File>,
    strip: Stripper,
    path: PathBuf,
    buf: Vec<u8>,
}

/// Keep letters, digits, `-`, `_` and `.` so the name is a safe file name.
fn file_stem(name: &str) -> String {
    let s: String = name
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || "-_.".contains(c) {
                c
            } else {
                '_'
            }
        })
        .collect();
    if s.trim_matches('_').is_empty() {
        "session".into()
    } else {
        s
    }
}

impl Logger {
    /// Create `<dir>/<name>_<YYYYmmdd_HHMMSS>.log`.
    pub fn create(dir: &Path, name: &str) -> Result<Self> {
        fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        let now = chrono::Local::now();
        let mut path = dir.join(format!(
            "{}_{}.log",
            file_stem(name),
            now.format("%Y%m%d_%H%M%S")
        ));
        // Two tabs of one session opened in the same second get separate files.
        let mut n = 2;
        while path.exists() {
            path = dir.join(format!(
                "{}_{}_{n}.log",
                file_stem(name),
                now.format("%Y%m%d_%H%M%S")
            ));
            n += 1;
        }
        // Logs can hold whatever crossed the screen, so only the owner may read them.
        let mut opts = fs::OpenOptions::new();
        opts.write(true).create_new(true);
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut opts, 0o600);
        let file = opts
            .open(&path)
            .with_context(|| format!("creating {}", path.display()))?;
        let mut file = BufWriter::new(file);
        writeln!(
            file,
            "# gcm session log: {name}, started {}",
            now.format("%Y-%m-%d %H:%M:%S %z")
        )?;
        Ok(Logger {
            file,
            strip: Stripper::default(),
            path,
            buf: Vec::new(),
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn write(&mut self, bytes: &[u8]) {
        self.buf.clear();
        self.strip.feed(bytes, &mut self.buf);
        if !self.buf.is_empty() {
            // A full disk should not take the session down with it.
            let _ = self
                .file
                .write_all(&self.buf)
                .and_then(|_| self.file.flush());
        }
    }
}

impl Drop for Logger {
    fn drop(&mut self) {
        self.buf.clear();
        self.strip.finish(&mut self.buf);
        let _ = self.file.write_all(&self.buf);
        let _ = writeln!(
            self.file,
            "# log closed {}",
            chrono::Local::now().format("%Y-%m-%d %H:%M:%S %z")
        );
        let _ = self.file.flush();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strip(chunks: &[&[u8]]) -> String {
        let mut s = Stripper::default();
        let mut out = Vec::new();
        for c in chunks {
            s.feed(c, &mut out);
        }
        s.finish(&mut out);
        String::from_utf8(out).unwrap()
    }

    #[test]
    fn removes_escapes() {
        assert_eq!(strip(&[b"\x1b[1;31mred\x1b[0m plain\r\n"]), "red plain\n");
        assert_eq!(strip(&[b"\x1b]0;user@host: ~\x07$ ls\r\n"]), "$ ls\n");
        assert_eq!(strip(&[b"\x1b]2;title\x1b\\ok\n"]), "ok\n");
        assert_eq!(strip(&[b"\x1b(Bline\n"]), "line\n");
        assert_eq!(strip(&[b"\x1b[?2004h\x1b[K\x1b=x\n"]), "x\n");
    }

    #[test]
    fn sequences_split_across_reads() {
        assert_eq!(
            strip(&[b"a\x1b[3", b"1m", b"b\x1b]0;t", b"itle\x07c\r", b"\n"]),
            "abc\n"
        );
    }

    #[test]
    fn carriage_return_and_backspace() {
        assert_eq!(strip(&[b" 10%\r 50%\r100%\r\n"]), "100%\n");
        assert_eq!(strip(&[b"lsx\x08 \x08\r\n"]), "ls\n");
        assert_eq!(strip(&[b"caf\xc3\xa9\x08e\n"]), "cafe\n");
        assert_eq!(strip(&[b"prompt$ "]), "prompt$ \n");
    }

    #[test]
    fn writes_file() {
        let dir = std::env::temp_dir().join(format!("gcm-log-{}", crate::config::new_id()));
        let path = {
            let mut l = Logger::create(&dir, "web 1/prod").unwrap();
            l.write(b"\x1b[32mhello\x1b[0m\r\nprompt$ ");
            l.path().to_path_buf()
        };
        assert!(
            path.file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("web_1_prod_")
        );
        let text = fs::read_to_string(&path).unwrap();
        let body: Vec<&str> = text.lines().filter(|l| !l.starts_with("# ")).collect();
        assert_eq!(body, ["hello", "prompt$ "]);
        assert!(text.lines().last().unwrap().starts_with("# log closed"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        fs::remove_dir_all(dir).unwrap();
    }
}
