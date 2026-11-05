//! SFTP side browser.
//!
//! Runs the system `sftp` client with piped stdin/stdout instead of linking
//! an SSH library, so `~/.ssh/config`, keys, the agent and `known_hosts` apply
//! exactly as in the SSH tab. A saved password reaches ssh through
//! `SSH_ASKPASS`, which points back at this binary (see [`crate::launch::askpass`]).
//!
//! With stdin not a terminal, sftp echoes each command as `sftp> cmd` and
//! line-buffers its output; every batch ends with `pwd`, whose reply marks
//! the end of the batch and reports the working directory.

use std::collections::BTreeSet;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};
use std::thread;

use anyhow::{Result, anyhow};
use eframe::egui::{self, Color32, Key, RichText, Sense, Ui};
use egui_extras::{Column, TableBuilder};

use crate::config::{Session, Settings};

#[derive(Clone, Debug, PartialEq)]
pub struct Entry {
    pub name: String,
    pub is_dir: bool,
    pub is_link: bool,
    pub size: u64,
    pub perms: String,
    pub modified: String,
}

/// Parse one line of `ls -la` output, e.g.
/// `-rw-r--r--    1 bob   staff   1234 Nov  4 17:23 my file.txt`.
pub fn parse_ls_line(line: &str) -> Option<Entry> {
    let kind = line.chars().next()?;
    if !"-dlcbps".contains(kind) {
        return None;
    }
    let mut rest = line;
    let mut fields = Vec::with_capacity(8);
    for _ in 0..8 {
        rest = rest.trim_start();
        let end = rest.find(char::is_whitespace)?;
        fields.push(&rest[..end]);
        rest = &rest[end..];
    }
    // Exactly one space separates the date from the name; keep any others.
    let mut name = rest.strip_prefix(' ')?.to_string();
    let is_link = kind == 'l';
    if is_link && let Some(pos) = name.find(" -> ") {
        name.truncate(pos);
    }
    if name.is_empty() {
        return None;
    }
    Some(Entry {
        name,
        is_dir: kind == 'd',
        is_link,
        size: fields[4].parse().ok()?,
        perms: fields[0].to_string(),
        modified: fields[5..8].join(" "),
    })
}

/// Quote an argument for sftp's command parser. Inside double quotes it
/// unescapes `\\` and `\"` and treats glob characters literally.
pub fn quote(arg: &str) -> String {
    let mut out = String::with_capacity(arg.len() + 2);
    out.push('"');
    for c in arg.chars() {
        if c == '"' || c == '\\' {
            out.push('\\');
        }
        out.push(c);
    }
    out.push('"');
    out
}

/// A quoted remote name that sftp cannot take for an option.
fn remote(name: &str) -> String {
    if name.starts_with('-') {
        quote(&format!("./{name}"))
    } else {
        quote(name)
    }
}

/// Whether a name from the server is safe to use as a local file name.
fn safe_local_name(name: &str) -> bool {
    !name.is_empty()
        && name != "."
        && name != ".."
        && !name.contains(['/', '\0'])
        && !(cfg!(windows) && name.contains(['\\', ':']))
        && std::path::Path::new(name).components().count() == 1
}

/// `sftp` arguments for an SSH session. Only the ssh options sftp also
/// understands (`-o`, `-F`, `-J`, `-i`) are taken from the extra arguments;
/// tunnels and forwarding stay with the terminal tab.
pub fn sftp_args(s: &Session) -> Result<Vec<String>> {
    if s.host.is_empty() {
        return Err(anyhow!("no host set"));
    }
    crate::launch::check_target(s)?;
    let mut args = Vec::new();
    if s.port != 0 && s.port != 22 {
        args.extend(["-P".into(), s.port.to_string()]);
    }
    args.extend(crate::launch::ssh_common_args(s)?);
    let host = if s.host.contains(':') {
        format!("[{}]", s.host)
    } else {
        s.host.clone()
    };
    args.push(if s.user.is_empty() {
        host
    } else {
        format!("{}@{host}", s.user)
    });
    Ok(args)
}

#[derive(Clone, Debug)]
enum Op {
    List(Option<String>),
    Get {
        items: Vec<Entry>,
        local_dir: PathBuf,
    },
    Put(Vec<PathBuf>),
    Mkdir(String),
    Remove(Vec<Entry>),
    Rename(String, String),
}

enum Reply {
    Listing {
        cwd: String,
        entries: Vec<Entry>,
    },
    Done {
        message: String,
        opened: Option<PathBuf>,
    },
    Error(String),
    /// Sent after the replies of each operation.
    Finished,
    Closed(String),
}

struct Section {
    lines: Vec<String>,
}

struct Conn {
    stdin: ChildStdin,
    out: BufReader<std::io::PipeReader>,
}

impl Conn {
    /// Send `cmds` followed by `pwd` and collect each command's output.
    /// `Err` carries everything read if sftp exits first.
    fn batch(&mut self, cmds: &[String]) -> std::result::Result<(Vec<Section>, String), String> {
        let mut script = String::new();
        for c in cmds {
            script.push_str(c);
            script.push('\n');
        }
        script.push_str("pwd\n");
        let mut seen: Vec<String> = Vec::new();
        if self
            .stdin
            .write_all(script.as_bytes())
            .and_then(|_| self.stdin.flush())
            .is_err()
        {
            return Err(String::new());
        }
        let mut sections: Vec<Section> = Vec::new();
        let mut buf = Vec::new();
        loop {
            buf.clear();
            match self.out.read_until(b'\n', &mut buf) {
                Ok(0) | Err(_) => return Err(seen.join("\n")),
                Ok(_) => {}
            }
            let line = String::from_utf8_lossy(&buf)
                .trim_end_matches(['\r', '\n'])
                .to_string();
            if line.starts_with("sftp> ") {
                sections.push(Section { lines: Vec::new() });
                continue;
            }
            if sections.len() == cmds.len() + 1
                && let Some(cwd) = line.strip_prefix("Remote working directory: ")
            {
                sections.pop();
                return Ok((sections, cwd.to_string()));
            }
            match sections.last_mut() {
                Some(s) => s.lines.push(line),
                None if !line.is_empty() => seen.push(line),
                None => {}
            }
        }
    }
}

fn is_progress(line: &str) -> bool {
    ["Fetching ", "Uploading ", "Entering ", "Retrieving "]
        .iter()
        .any(|p| line.starts_with(p))
}

/// Error text from sections that should be silent apart from progress lines.
fn errors(sections: &[Section]) -> Vec<String> {
    sections
        .iter()
        .flat_map(|s| s.lines.iter())
        .filter(|l| !l.trim().is_empty() && !is_progress(l))
        .cloned()
        .collect()
}

fn listing(section: &Section) -> (Vec<Entry>, Vec<String>) {
    let mut entries = Vec::new();
    let mut errs = Vec::new();
    for line in &section.lines {
        match parse_ls_line(line) {
            Some(e) if e.name == "." || e.name == ".." => {}
            Some(e) => entries.push(e),
            None if !line.trim().is_empty() => errs.push(line.clone()),
            None => {}
        }
    }
    entries.sort_by(|a, b| {
        (!a.is_dir, a.name.to_lowercase()).cmp(&(!b.is_dir, b.name.to_lowercase()))
    });
    (entries, errs)
}

fn run(conn: &mut Conn, op: Op) -> std::result::Result<Vec<Reply>, String> {
    // Every op but a download ends with a fresh listing.
    let (mut cmds, relist) = match &op {
        Op::List(Some(dir)) => (vec![format!("cd {}", quote(dir))], true),
        Op::List(None) => (vec![], true),
        Op::Get { items, local_dir } => (
            items
                .iter()
                .map(|e| {
                    // The name comes from the server: one plain component only,
                    // so a download cannot land outside the chosen folder.
                    if !safe_local_name(&e.name) {
                        return Err(format!("refusing to download {:?}", e.name));
                    }
                    let local = local_dir.join(&e.name);
                    let r = if e.is_dir { "-r " } else { "" };
                    Ok(format!(
                        "get -p {r}{} {}",
                        remote(&e.name),
                        quote(&local.to_string_lossy())
                    ))
                })
                .collect::<std::result::Result<_, _>>()?,
            false,
        ),
        Op::Put(paths) => (
            paths
                .iter()
                .map(|p| {
                    let r = if p.is_dir() { "-r " } else { "" };
                    format!("put -p {r}{}", quote(&p.to_string_lossy()))
                })
                .collect(),
            true,
        ),
        Op::Mkdir(name) => (vec![format!("mkdir {}", remote(name))], true),
        Op::Remove(items) => (
            items
                .iter()
                .map(|e| {
                    format!(
                        "{} {}",
                        if e.is_dir { "rmdir" } else { "rm" },
                        remote(&e.name)
                    )
                })
                .collect(),
            true,
        ),
        Op::Rename(from, to) => (
            vec![format!("rename {} {}", remote(from), remote(to))],
            true,
        ),
    };
    // sftp reads one command per line.
    if cmds.iter().any(|c| c.contains(['\n', '\r'])) {
        return Err("names with line breaks are not supported".into());
    }
    let n = cmds.len();
    if relist {
        cmds.push("ls -la".into());
    }
    let (sections, cwd) = conn.batch(&cmds)?;
    let mut replies = Vec::new();
    let mut errs = errors(&sections[..n.min(sections.len())]);
    if relist && let Some(ls) = sections.get(n) {
        let (entries, ls_errs) = listing(ls);
        errs.extend(ls_errs);
        replies.push(Reply::Listing { cwd, entries });
    }
    if !errs.is_empty() {
        replies.push(Reply::Error(errs.join("\n")));
    } else {
        match op {
            Op::Get { items, local_dir } => replies.push(Reply::Done {
                message: format!(
                    "Downloaded {} item(s) to {}",
                    items.len(),
                    local_dir.display()
                ),
                opened: Some(local_dir),
            }),
            Op::Put(paths) => replies.push(Reply::Done {
                message: format!("Uploaded {} item(s)", paths.len()),
                opened: None,
            }),
            _ => {}
        }
    }
    Ok(replies)
}

fn worker(
    mut cmd: Command,
    child_slot: Arc<Mutex<Option<Child>>>,
    ops: Receiver<Op>,
    replies: Sender<Reply>,
    ctx: egui::Context,
) {
    let send = |r: Reply| {
        let _ = replies.send(r);
        ctx.request_repaint();
    };
    // One pipe for stdout and stderr keeps error lines next to the command that caused them.
    let spawned = std::io::pipe().and_then(|(reader, writer)| {
        let writer2 = writer.try_clone()?;
        cmd.stdin(Stdio::piped()).stdout(writer).stderr(writer2);
        let child = cmd.spawn()?;
        Ok((reader, child))
    });
    // The Command still holds our copy of the pipe's write end; close it so EOF arrives.
    drop(cmd);
    let (reader, mut child) = match spawned {
        Ok(x) => x,
        Err(e) => return send(Reply::Closed(format!("cannot start sftp: {e}"))),
    };
    let stdin = child.stdin.take().expect("piped stdin");
    *child_slot.lock().unwrap() = Some(child);
    let mut conn = Conn {
        stdin,
        out: BufReader::new(reader),
    };

    let mut first = match conn.batch(&[]) {
        Ok(_) => Some(Op::List(None)),
        Err(out) => {
            send(Reply::Closed(crate::launch::explain_ssh_failure(&out)));
            None
        }
    };
    while first.is_some() {
        let op = match first.take() {
            Some(op) => op,
            None => break,
        };
        match run(&mut conn, op) {
            Ok(rs) => {
                rs.into_iter().for_each(&send);
                send(Reply::Finished);
            }
            Err(out) => {
                send(Reply::Closed(crate::launch::explain_ssh_failure(&out)));
                break;
            }
        }
        first = ops.recv().ok();
    }
    if let Some(mut c) = child_slot.lock().unwrap().take() {
        let _ = c.kill();
        let _ = c.wait();
    }
}

enum Input {
    Mkdir(String),
    Rename { from: String, to: String },
    ConfirmDelete(Vec<Entry>),
}

pub struct Browser {
    ops: Sender<Op>,
    replies: Receiver<Reply>,
    child: Arc<Mutex<Option<Child>>>,
    cwd: String,
    /// The login directory, from the first listing.
    home: String,
    path_edit: String,
    entries: Vec<Entry>,
    selected: BTreeSet<String>,
    pending: usize,
    status: Option<(String, bool)>,
    opened: Option<PathBuf>,
    closed: bool,
    input: Option<Input>,
}

impl Browser {
    pub fn connect(ctx: &egui::Context, session: &Session, settings: &Settings) -> Self {
        let label = format!("Connecting to {}…", session.host);
        match sftp_args(session) {
            Ok(args) => {
                let mut cmd = crate::launch::background_ssh(
                    &settings.sftp_command,
                    &session.password,
                    &session.host,
                );
                cmd.args(&args);
                Self::spawn(ctx, cmd, label)
            }
            Err(e) => {
                let mut b = Self::new(channel().0, channel().1, label);
                b.closed = true;
                b.pending = 0;
                b.status = Some((e.to_string(), true));
                b
            }
        }
    }

    fn new(ops: Sender<Op>, replies: Receiver<Reply>, status: String) -> Self {
        Browser {
            ops,
            replies,
            child: Arc::new(Mutex::new(None)),
            cwd: String::new(),
            home: String::new(),
            path_edit: String::new(),
            entries: Vec::new(),
            selected: BTreeSet::new(),
            pending: 1,
            status: Some((status, false)),
            opened: None,
            closed: false,
            input: None,
        }
    }

    /// Run `cmd` (an sftp invocation) on a worker thread.
    fn spawn(ctx: &egui::Context, cmd: Command, status: String) -> Self {
        let (ops_tx, ops_rx) = channel();
        let (rep_tx, rep_rx) = channel();
        let b = Self::new(ops_tx, rep_rx, status);
        let (child, ctx) = (b.child.clone(), ctx.clone());
        thread::spawn(move || worker(cmd, child, ops_rx, rep_tx, ctx));
        b
    }

    pub fn is_closed(&self) -> bool {
        self.closed
    }

    fn send(&mut self, op: Op) {
        if self.closed {
            return;
        }
        if self.ops.send(op).is_ok() {
            self.pending += 1;
        }
    }

    fn poll(&mut self) {
        while let Ok(r) = self.replies.try_recv() {
            match r {
                Reply::Listing { cwd, entries } => {
                    if self.home.is_empty() {
                        self.home = cwd.clone();
                    }
                    if cwd != self.cwd {
                        self.selected.clear();
                    }
                    self.selected
                        .retain(|n| entries.iter().any(|e| &e.name == n));
                    self.path_edit = cwd.clone();
                    self.cwd = cwd;
                    self.entries = entries;
                    if self.status.as_ref().is_some_and(|(_, err)| !err) {
                        self.status = None;
                    }
                }
                Reply::Done { message, opened } => {
                    self.status = Some((message, false));
                    self.opened = opened;
                }
                Reply::Error(e) => self.status = Some((e, true)),
                Reply::Finished => self.pending = self.pending.saturating_sub(1),
                Reply::Closed(e) => {
                    self.closed = true;
                    self.pending = 0;
                    self.status = Some((e, true));
                }
            }
        }
    }

    pub fn upload(&mut self, paths: Vec<PathBuf>) {
        if !paths.is_empty() {
            self.status = Some((format!("Uploading {} item(s)…", paths.len()), false));
            self.send(Op::Put(paths));
        }
    }

    fn download(&mut self, items: Vec<Entry>, dir: &Path) {
        if items.is_empty() {
            return;
        }
        if let Err(e) = std::fs::create_dir_all(dir) {
            self.status = Some((format!("{}: {e}", dir.display()), true));
            return;
        }
        self.status = Some((format!("Downloading {} item(s)…", items.len()), false));
        self.send(Op::Get {
            items,
            local_dir: dir.to_path_buf(),
        });
    }

    fn selected_entries(&self) -> Vec<Entry> {
        self.entries
            .iter()
            .filter(|e| self.selected.contains(&e.name))
            .cloned()
            .collect()
    }

    fn cd(&mut self, dir: String) {
        self.send(Op::List(Some(dir)));
    }

    /// Returns true when the user asks to reconnect.
    pub fn ui(&mut self, ui: &mut Ui, download_dir: &Path) -> bool {
        self.poll();
        let busy = self.pending > 0;
        let mut reconnect = false;

        ui.horizontal(|ui| {
            ui.add_enabled_ui(!self.closed, |ui| {
                if ui
                    .small_button("⬆")
                    .on_hover_text("Parent folder")
                    .clicked()
                {
                    self.cd("..".into());
                }
                if ui.small_button("🏠").on_hover_text("Home folder").clicked()
                    && !self.home.is_empty()
                {
                    self.cd(self.home.clone());
                }
                if ui.small_button("⟳").on_hover_text("Refresh").clicked() {
                    self.send(Op::List(None));
                }
            });
            if busy {
                ui.spinner();
            }
        });
        let resp = ui.add_enabled(
            !self.closed,
            egui::TextEdit::singleline(&mut self.path_edit).desired_width(f32::INFINITY),
        );
        if resp.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter)) {
            let target = self.path_edit.trim().to_string();
            if !target.is_empty() {
                self.cd(target);
            }
        }

        ui.horizontal_wrapped(|ui| {
            ui.add_enabled_ui(!self.closed, |ui| {
                if ui
                    .button("⬆ Upload")
                    .on_hover_text("Upload files (or drop them here)")
                    .clicked()
                    && let Some(files) = rfd::FileDialog::new()
                        .set_title("Upload files")
                        .pick_files()
                {
                    self.upload(files);
                }
                let sel = self.selected_entries();
                if ui
                    .add_enabled(!sel.is_empty(), egui::Button::new("⬇ Download"))
                    .clicked()
                {
                    self.download(sel.clone(), download_dir);
                }
                if ui.button("📁+").on_hover_text("New folder").clicked() {
                    self.input = Some(Input::Mkdir(String::new()));
                }
                if ui
                    .add_enabled(sel.len() == 1, egui::Button::new("✏"))
                    .on_hover_text("Rename")
                    .clicked()
                {
                    let name = sel[0].name.clone();
                    self.input = Some(Input::Rename {
                        from: name.clone(),
                        to: name,
                    });
                }
                if ui
                    .add_enabled(!sel.is_empty(), egui::Button::new("🗑"))
                    .on_hover_text("Delete")
                    .clicked()
                {
                    self.input = Some(Input::ConfirmDelete(sel));
                }
            });
        });

        self.input_ui(ui);

        if let Some((msg, is_err)) = &self.status {
            let color = if *is_err {
                Color32::from_rgb(230, 80, 80)
            } else {
                ui.visuals().weak_text_color()
            };
            ui.label(RichText::new(msg).color(color).small());
        }
        if let Some(dir) = self.opened.clone()
            && ui.small_button("Open download folder").clicked()
        {
            if let Err(e) = crate::launch::open_path(&dir) {
                self.status = Some((e.to_string(), true));
            }
            self.opened = None;
        }
        if self.closed {
            if ui.button("⟳ Reconnect").clicked() {
                reconnect = true;
            }
            return reconnect;
        }
        ui.separator();
        self.table(ui, download_dir);
        reconnect
    }

    fn input_ui(&mut self, ui: &mut Ui) {
        let Some(input) = self.input.take() else {
            return;
        };
        let mut keep = true;
        match input {
            Input::Mkdir(mut name) => {
                ui.horizontal(|ui| {
                    let r = ui.add(
                        egui::TextEdit::singleline(&mut name)
                            .hint_text("folder name")
                            .desired_width(120.0),
                    );
                    crate::app::focus_on_open(ui, &r);
                    let enter = r.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter));
                    if (ui.button("Create").clicked() || enter) && !name.trim().is_empty() {
                        self.send(Op::Mkdir(name.trim().to_string()));
                        keep = false;
                    }
                    if ui.button("✖").clicked() || ui.input(|i| i.key_pressed(Key::Escape)) {
                        keep = false;
                    }
                });
                if keep {
                    self.input = Some(Input::Mkdir(name));
                }
            }
            Input::Rename { from, mut to } => {
                ui.horizontal(|ui| {
                    let r = ui.add(egui::TextEdit::singleline(&mut to).desired_width(120.0));
                    crate::app::focus_on_open(ui, &r);
                    let enter = r.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter));
                    if (ui.button("Rename").clicked() || enter)
                        && !to.trim().is_empty()
                        && to != from
                    {
                        self.send(Op::Rename(from.clone(), to.trim().to_string()));
                        keep = false;
                    }
                    if ui.button("✖").clicked() || ui.input(|i| i.key_pressed(Key::Escape)) {
                        keep = false;
                    }
                });
                if keep {
                    self.input = Some(Input::Rename { from, to });
                }
            }
            Input::ConfirmDelete(items) => {
                let what = if items.len() == 1 {
                    format!("\"{}\"", items[0].name)
                } else {
                    format!("{} items", items.len())
                };
                ui.label(format!("Delete {what}? Folders must be empty."));
                ui.horizontal(|ui| {
                    if ui.button("Delete").clicked() {
                        self.send(Op::Remove(items.clone()));
                        keep = false;
                    }
                    if ui.button("Cancel").clicked() || ui.input(|i| i.key_pressed(Key::Escape)) {
                        keep = false;
                    }
                });
                if keep {
                    self.input = Some(Input::ConfirmDelete(items));
                }
            }
        }
    }

    fn table(&mut self, ui: &mut Ui, download_dir: &Path) {
        let row_h = ui.text_style_height(&egui::TextStyle::Body) + 4.0;
        let mut clicked: Option<(usize, bool)> = None;
        let mut activated: Option<usize> = None;
        let mut menu_action: Option<(usize, &'static str)> = None;
        TableBuilder::new(ui)
            .striped(true)
            .sense(Sense::click())
            .column(Column::remainder().clip(true))
            .column(Column::auto().at_least(56.0))
            .header(row_h, |mut h| {
                h.col(|ui| {
                    ui.strong("Name");
                });
                h.col(|ui| {
                    ui.strong("Size");
                });
            })
            .body(|body| {
                body.rows(row_h, self.entries.len(), |mut row| {
                    let i = row.index();
                    let e = &self.entries[i];
                    row.set_selected(self.selected.contains(&e.name));
                    row.col(|ui| {
                        let icon = if e.is_dir {
                            "📁"
                        } else if e.is_link {
                            "🔗"
                        } else {
                            "📄"
                        };
                        ui.add(
                            egui::Label::new(format!("{icon} {}", e.name))
                                .selectable(false)
                                .truncate(),
                        );
                    });
                    row.col(|ui| {
                        if !e.is_dir {
                            ui.add(egui::Label::new(human_size(e.size)).selectable(false));
                        }
                    });
                    let resp = row
                        .response()
                        .on_hover_text(format!("{}\n{}  {}", e.name, e.perms, e.modified));
                    if resp.double_clicked() {
                        activated = Some(i);
                    } else if resp.clicked() {
                        clicked = Some((i, ui_modifiers_toggle(&resp)));
                    }
                    resp.context_menu(|ui| {
                        for (label, act) in [
                            ("Open", "open"),
                            ("Download", "download"),
                            ("Rename", "rename"),
                            ("Delete", "delete"),
                        ] {
                            if ui.button(label).clicked() {
                                menu_action = Some((i, act));
                                ui.close();
                            }
                        }
                        if ui.button("Copy path").clicked() {
                            menu_action = Some((i, "copy"));
                            ui.close();
                        }
                    });
                });
            });

        if let Some((i, toggle)) = clicked {
            let name = self.entries[i].name.clone();
            if toggle {
                if !self.selected.remove(&name) {
                    self.selected.insert(name);
                }
            } else {
                self.selected = BTreeSet::from([name]);
            }
        }
        if let Some(i) = activated {
            let e = self.entries[i].clone();
            if e.is_dir || e.is_link {
                self.cd(e.name);
            } else {
                self.download(vec![e], download_dir);
            }
        }
        if let Some((i, act)) = menu_action {
            let e = self.entries[i].clone();
            match act {
                "open" if e.is_dir || e.is_link => self.cd(e.name),
                "open" | "download" => self.download(vec![e], download_dir),
                "rename" => {
                    self.input = Some(Input::Rename {
                        from: e.name.clone(),
                        to: e.name,
                    })
                }
                "delete" => self.input = Some(Input::ConfirmDelete(vec![e])),
                "copy" => {
                    let path = if self.cwd.ends_with('/') {
                        format!("{}{}", self.cwd, e.name)
                    } else {
                        format!("{}/{}", self.cwd, e.name)
                    };
                    ui.ctx().copy_text(path);
                }
                _ => {}
            }
        }
    }
}

fn ui_modifiers_toggle(resp: &egui::Response) -> bool {
    resp.ctx.input(|i| i.modifiers.command || i.modifiers.ctrl)
}

impl Drop for Browser {
    fn drop(&mut self) {
        // Ends a transfer in progress too; the worker then sees EOF and exits.
        if let Some(c) = self.child.lock().unwrap().as_mut() {
            let _ = c.kill();
        }
    }
}

pub fn human_size(n: u64) -> String {
    const UNITS: [&str; 5] = ["B", "K", "M", "G", "T"];
    let mut v = n as f64;
    let mut u = 0;
    while v >= 1024.0 && u < UNITS.len() - 1 {
        v /= 1024.0;
        u += 1;
    }
    if u == 0 {
        format!("{n} B")
    } else {
        format!("{v:.1} {}", UNITS[u])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Protocol;

    #[test]
    fn server_names_stay_in_the_download_folder() {
        assert!(safe_local_name("report.txt"));
        assert!(safe_local_name("-rf"));
        assert!(!safe_local_name(".."));
        assert!(!safe_local_name("."));
        assert!(!safe_local_name("../../.bashrc"));
        assert!(!safe_local_name("/etc/passwd"));
        assert!(!safe_local_name(""));
        assert_eq!(remote("-rf"), "\"./-rf\"");
        assert_eq!(remote("a b"), "\"a b\"");
    }

    #[test]
    fn parses_ls() {
        let e =
            parse_ls_line("-rw-r--r--    1 bob      staff        1234 Nov  4 17:23 my  file.txt")
                .unwrap();
        assert_eq!(
            (e.name.as_str(), e.size, e.is_dir),
            ("my  file.txt", 1234, false)
        );
        assert_eq!(e.modified, "Nov 4 17:23");
        let e = parse_ls_line("drwxr-xr-x    ? root     root         4096 Jan  1  2020 dir one")
            .unwrap();
        assert!(e.is_dir);
        assert_eq!(e.name, "dir one");
        let e = parse_ls_line(
            "lrwxrwxrwx    1 root     root            7 Nov  4 20:40 link -> dir one",
        )
        .unwrap();
        assert!(e.is_link);
        assert_eq!(e.name, "link");
        assert!(parse_ls_line("Can't ls: \"/nope\" not found").is_none());
        assert!(parse_ls_line("").is_none());
    }

    #[test]
    fn quoting() {
        assert_eq!(quote(r#"a "b" \c*"#), r#""a \"b\" \\c*""#);
    }

    #[test]
    fn args() {
        let mut s = Session::new(Protocol::Ssh);
        s.host = "fe80::1".into();
        s.user = "bob".into();
        s.port = 2200;
        s.tunnels = vec!["L 1:x:1".into()];
        s.extra_args = "-o 'ProxyJump bastion' -v -J jump -oBatchMode=no".into();
        assert_eq!(
            sftp_args(&s).unwrap(),
            [
                "-P",
                "2200",
                "-o",
                "ProxyJump bastion",
                "-J",
                "jump",
                "-oBatchMode=no",
                "bob@[fe80::1]"
            ]
        );
    }

    /// Drives the real sftp client against a local sftp-server (no network);
    /// skipped where OpenSSH's sftp-server is not installed.
    #[test]
    fn live_sftp_session() {
        use std::time::{Duration, Instant};
        let Some(server) = [
            "/usr/lib/openssh/sftp-server",
            "/usr/libexec/openssh/sftp-server",
            "/usr/libexec/sftp-server",
        ]
        .into_iter()
        .find(|p| Path::new(p).exists()) else {
            eprintln!("sftp-server not found; skipping");
            return;
        };
        let root = std::env::temp_dir().join(format!("gcm-sftp-{}", crate::config::new_id()));
        let remote = root.join("remote");
        let local = root.join("local");
        std::fs::create_dir_all(remote.join("sub dir")).unwrap();
        std::fs::create_dir_all(&local).unwrap();
        std::fs::write(remote.join("odd *name?.txt"), "glob chars").unwrap();
        std::fs::write(local.join("up load.txt"), "uploaded").unwrap();

        let mut cmd = Command::new("sftp");
        cmd.args(["-D", server]).current_dir(&remote);
        let mut b = Browser::spawn(&egui::Context::default(), cmd, String::new());
        let wait = |b: &mut Browser| {
            let t = Instant::now();
            while b.pending > 0 && !b.closed {
                assert!(t.elapsed() < Duration::from_secs(20), "sftp timed out");
                thread::sleep(Duration::from_millis(20));
                b.poll();
            }
            assert!(!b.closed, "sftp closed: {:?}", b.status);
        };
        let names = |b: &Browser| b.entries.iter().map(|e| e.name.clone()).collect::<Vec<_>>();

        wait(&mut b);
        assert_eq!(names(&b), ["sub dir", "odd *name?.txt"]);
        assert!(b.entries[0].is_dir);

        b.upload(vec![local.join("up load.txt")]);
        wait(&mut b);
        assert_eq!(
            std::fs::read_to_string(remote.join("up load.txt")).unwrap(),
            "uploaded"
        );
        assert!(names(&b).contains(&"up load.txt".to_string()));

        let item = b
            .entries
            .iter()
            .find(|e| e.name == "odd *name?.txt")
            .unwrap()
            .clone();
        b.download(vec![item], &local);
        wait(&mut b);
        assert_eq!(
            std::fs::read_to_string(local.join("odd *name?.txt")).unwrap(),
            "glob chars"
        );

        b.send(Op::Mkdir("new \"dir\"".into()));
        wait(&mut b);
        b.send(Op::Rename("new \"dir\"".into(), "renamed".into()));
        wait(&mut b);
        assert!(remote.join("renamed").is_dir());

        b.cd("sub dir".into());
        wait(&mut b);
        assert!(b.cwd.ends_with("/sub dir"), "{}", b.cwd);
        assert!(b.entries.is_empty());
        b.cd("..".into());
        wait(&mut b);

        // Errors are reported and the session stays usable.
        b.cd("does-not-exist".into());
        wait(&mut b);
        assert!(
            b.status
                .as_ref()
                .is_some_and(|(m, err)| *err && !m.is_empty()),
            "{:?}",
            b.status
        );
        assert!(names(&b).contains(&"renamed".to_string()));

        let dir = b
            .entries
            .iter()
            .find(|e| e.name == "renamed")
            .unwrap()
            .clone();
        let file = b
            .entries
            .iter()
            .find(|e| e.name == "up load.txt")
            .unwrap()
            .clone();
        b.send(Op::Remove(vec![dir, file]));
        wait(&mut b);
        assert!(!remote.join("renamed").exists() && !remote.join("up load.txt").exists());

        drop(b);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn sizes() {
        assert_eq!(human_size(512), "512 B");
        assert_eq!(human_size(1536), "1.5 K");
        assert_eq!(human_size(5 * 1024 * 1024 * 1024), "5.0 G");
    }
}
