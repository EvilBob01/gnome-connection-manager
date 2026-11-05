//! Main window: session tree on the left, tabbed and splittable terminals
//! in the middle, MobaXterm-style toolbar with quick connect on top.

use eframe::egui::{self, Color32, Key, RichText, Ui, WidgetText};
use egui_dock::tab_viewer::OnCloseResponse;
use egui_dock::{DockArea, DockState, Node, NodePath, Split, Style, TabViewer};

use crate::config::{FlowControl, Parity, Protocol, Session, Settings, Store, new_id};
use crate::gcm_import;
use crate::launch::{self, Launch};
use crate::sftp::Browser;
use crate::terminal::Terminal;
use crate::tunnel::{Status as TunnelStatus, Tunnel, TunnelKind, TunnelManager};

pub struct Tab {
    id: u64,
    session: Session,
    term: Terminal,
    /// Receives input typed in other tabs while multi-exec is on.
    in_group: bool,
}

enum Action {
    /// Show a message in the status bar (true = error).
    Notify(String, bool),
    Open(Session),
    Split(Session, NodePath, Split),
    Reconnect(u64),
    Close(u64),
    Edit(Session),
    Delete(u64),
}

enum Modal {
    Edit {
        session: Session,
        is_new: bool,
        save: bool,
    },
    Settings(Settings),
    ConfirmClose(u64),
    ConfirmDelete(u64),
    ConfirmImport {
        imported: crate::config::Imported,
        source: String,
    },
    EditTunnel {
        tunnel: Tunnel,
        is_new: bool,
    },
    ConfirmDeleteTunnel(u64),
    About,
}

pub struct App {
    store: Store,
    dock: DockState<Tab>,
    actions: Vec<Action>,
    modal: Option<Modal>,
    quick: String,
    filter: String,
    show_sidebar: bool,
    /// MobaXterm's MultiExec / GCM's cluster mode: typing goes to every tab in the group.
    multiexec: bool,
    /// Command line in the multi-exec bar.
    multiexec_line: String,
    sidebar_tab: SidebarTab,
    /// SFTP browsers by terminal tab id, created when first shown.
    browsers: std::collections::HashMap<u64, Browser>,
    tunnels: TunnelManager,
    status: Option<(String, bool)>,
}

#[derive(Clone, Copy)]
enum ImportSource {
    Gcm,
    MobaXterm,
    PuttyLocal,
    PuttyReg,
}

#[derive(PartialEq, Clone, Copy)]
enum SidebarTab {
    Sessions,
    Sftp,
    Tunnels,
}

impl App {
    pub fn new(cc: &eframe::CreationContext<'_>, store: Store) -> Self {
        crate::fonts::install(&cc.egui_ctx, &store.settings.font_path);
        cc.egui_ctx.set_visuals(if store.settings.dark_mode {
            egui::Visuals::dark()
        } else {
            egui::Visuals::light()
        });
        let mut app = Self {
            store,
            dock: DockState::new(Vec::new()),
            actions: Vec::new(),
            modal: None,
            quick: String::new(),
            filter: String::new(),
            show_sidebar: true,
            multiexec: false,
            multiexec_line: String::new(),
            sidebar_tab: SidebarTab::Sessions,
            browsers: Default::default(),
            tunnels: TunnelManager::default(),
            status: None,
        };
        for t in app
            .store
            .tunnels
            .clone()
            .into_iter()
            .filter(|t| t.autostart)
        {
            if let Err(e) = app
                .tunnels
                .start(&t, &app.store.sessions, &app.store.settings)
            {
                app.error(format!("Tunnel {}: {e:#}", t.display_name()));
            }
        }
        if app.store.settings.open_local_on_startup {
            app.actions
                .push(Action::Open(Session::new(Protocol::Local)));
        }
        app
    }

    /// Open tabs for command-line arguments: a saved session name, or quick-connect syntax.
    pub fn open_args(&mut self, args: &[String]) {
        for arg in args {
            let saved = self
                .store
                .sessions
                .iter()
                .find(|s| s.display_name().eq_ignore_ascii_case(arg))
                .cloned();
            match saved.or_else(|| Session::from_quick(arg)) {
                Some(s) => self.actions.push(Action::Open(s)),
                None => self.error(format!("Unknown session or address: {arg}")),
            }
        }
    }

    fn info(&mut self, msg: impl Into<String>) {
        self.status = Some((msg.into(), false));
    }

    fn error(&mut self, msg: impl Into<String>) {
        let msg = msg.into();
        log::error!("{msg}");
        self.status = Some((msg, true));
    }

    fn save(&mut self) {
        if let Err(e) = self.store.save() {
            self.error(format!("Saving settings failed: {e:#}"));
        }
    }

    /// Start a session. Terminal protocols return a tab; RDP/VNC launch an
    /// external client and return None.
    fn connect(&mut self, ctx: &egui::Context, session: Session) -> Option<Tab> {
        let settings = &self.store.settings;
        let result = launch::build(&session, settings).and_then(|l| match l {
            Launch::Pty {
                program,
                args,
                autofill,
            } => Terminal::spawn_pty(ctx, &program, &args, autofill, settings.scrollback).map(Some),
            Launch::Serial {
                device,
                settings: s,
            } => Terminal::open_serial(ctx, &device, &s, settings.scrollback).map(Some),
            Launch::External { program, args } => {
                launch::spawn_external(&program, &args).map(|_| None)
            }
        });
        match result {
            Ok(Some(mut term)) => {
                term.set_colors(
                    crate::config::parse_color(&session.fg_color),
                    crate::config::parse_color(&session.bg_color),
                );
                if session.log
                    && let Err(e) = term.start_log(&settings.log_dir(), &session.display_name())
                {
                    self.error(format!("Cannot start log: {e:#}"));
                }
                for (delay, text) in
                    launch::command_schedule(&session.commands, session.protocol == Protocol::Local)
                {
                    term.schedule(delay, text);
                }
                self.info(format!("Connected: {}", session.display_name()));
                Some(Tab {
                    id: new_id(),
                    session,
                    term,
                    in_group: true,
                })
            }
            Ok(None) => {
                self.info(format!(
                    "Launched {} client for {}",
                    session.protocol.label(),
                    session.display_name()
                ));
                None
            }
            Err(e) => {
                self.error(format!("{}: {e:#}", session.display_name()));
                None
            }
        }
    }

    fn run_actions(&mut self, ctx: &egui::Context) {
        for action in std::mem::take(&mut self.actions) {
            match action {
                Action::Notify(msg, true) => self.error(msg),
                Action::Notify(msg, false) => self.info(msg),
                Action::Open(session) => {
                    if let Some(tab) = self.connect(ctx, session) {
                        self.dock.push_to_focused_leaf(tab);
                    }
                }
                Action::Split(session, path, dir) => {
                    if let Some(tab) = self.connect(ctx, session) {
                        self.dock.split(path, dir, 0.5, Node::leaf(tab));
                    }
                }
                Action::Reconnect(id) => {
                    let Some(path) = self.dock.find_tab_from(|t| t.id == id) else {
                        continue;
                    };
                    let session = self
                        .dock
                        .iter_all_tabs()
                        .find(|(_, t)| t.id == id)
                        .unwrap()
                        .1
                        .session
                        .clone();
                    if let Some(new_tab) = self.connect(ctx, session) {
                        if let Some((_, tab)) =
                            self.dock.iter_all_tabs_mut().find(|(_, t)| t.id == id)
                        {
                            *tab = new_tab;
                        }
                        let _ = self.dock.set_active_tab(path);
                    }
                }
                Action::Close(id) => {
                    if let Some(path) = self.dock.find_tab_from(|t| t.id == id) {
                        self.dock.remove_tab(path);
                    }
                }
                Action::Edit(session) => {
                    self.modal = Some(Modal::Edit {
                        session,
                        is_new: false,
                        save: true,
                    });
                }
                Action::Delete(id) => self.modal = Some(Modal::ConfirmDelete(id)),
            }
        }
    }

    fn toolbar(&mut self, ui: &mut Ui) {
        egui::MenuBar::new().ui(ui, |ui| {
            ui.menu_button("Session", |ui| {
                if ui.button("New session…").clicked() {
                    self.modal = Some(Modal::Edit {
                        session: Session::new(Protocol::Ssh),
                        is_new: true,
                        save: true,
                    });
                }
                if ui.button("Local shell").clicked() {
                    self.actions
                        .push(Action::Open(Session::new(Protocol::Local)));
                }
                ui.separator();
                ui.menu_button("Import sessions", |ui| self.import_menu(ui));
                if ui.button("Open log folder").clicked() {
                    let dir = self.store.settings.log_dir();
                    if let Err(e) = std::fs::create_dir_all(&dir)
                        .map_err(anyhow::Error::from)
                        .and_then(|_| launch::open_path(&dir))
                    {
                        self.error(format!("{}: {e:#}", dir.display()));
                    }
                    ui.close();
                }
                ui.separator();
                if ui.button("Quit").clicked() {
                    ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
                }
            });
            ui.menu_button("View", |ui| {
                ui.checkbox(&mut self.show_sidebar, "Session sidebar");
                if ui
                    .checkbox(&mut self.store.settings.dark_mode, "Dark theme")
                    .changed()
                {
                    let dark = self.store.settings.dark_mode;
                    ui.ctx().set_visuals(if dark {
                        egui::Visuals::dark()
                    } else {
                        egui::Visuals::light()
                    });
                    self.save();
                }
                ui.separator();
                if ui.button("Settings…").clicked() {
                    self.modal = Some(Modal::Settings(self.store.settings.clone()));
                }
            });
            ui.menu_button("Help", |ui| {
                if ui.button("About").clicked() {
                    self.modal = Some(Modal::About);
                }
            });
        });

        ui.horizontal(|ui| {
            let big = |icon: &str, label: &str| {
                egui::Button::new(RichText::new(format!("{icon}  {label}")).size(14.0))
            };
            if ui
                .add(big("➕", "Session"))
                .on_hover_text("Create a saved session")
                .clicked()
            {
                self.modal = Some(Modal::Edit {
                    session: Session::new(Protocol::Ssh),
                    is_new: true,
                    save: true,
                });
            }
            if ui
                .add(big("💻", "Shell"))
                .on_hover_text("Open a local terminal")
                .clicked()
            {
                self.actions
                    .push(Action::Open(Session::new(Protocol::Local)));
            }
            if ui
                .add(big("🔌", "Serial"))
                .on_hover_text("Open a serial port")
                .clicked()
            {
                self.modal = Some(Modal::Edit {
                    session: Session::new(Protocol::Serial),
                    is_new: true,
                    save: false,
                });
            }
            if ui.add(big("⚙", "Settings")).clicked() {
                self.modal = Some(Modal::Settings(self.store.settings.clone()));
            }
            let tunnels_btn = egui::Button::new(RichText::new("⇄  Tunnels").size(14.0))
                .selected(self.show_sidebar && self.sidebar_tab == SidebarTab::Tunnels);
            if ui
                .add(tunnels_btn)
                .on_hover_text("SSH tunnel manager")
                .clicked()
            {
                self.show_sidebar = true;
                self.sidebar_tab = SidebarTab::Tunnels;
            }
            let me = egui::Button::new(RichText::new("⚡  MultiExec").size(14.0))
                .selected(self.multiexec);
            if ui
                .add(me)
                .on_hover_text("Type into all terminals in the group at once")
                .clicked()
            {
                self.multiexec = !self.multiexec;
            }
            ui.separator();
            ui.label("Quick connect:");
            let resp = ui.add(
                egui::TextEdit::singleline(&mut self.quick)
                    .hint_text("user@host:port   telnet://host   rdp://host")
                    .desired_width(280.0),
            );
            let go = ui.button("Connect").clicked()
                || (resp.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter)));
            if go {
                match Session::from_quick(&self.quick) {
                    Some(s) => {
                        self.actions.push(Action::Open(s));
                        self.quick.clear();
                    }
                    None => self.error("Quick connect: expected [proto://][user@]host[:port]"),
                }
            }
        });
    }

    /// Bar shown while multi-exec is on: who is in the group, plus a command
    /// line that sends one line to all of them (GCM's cluster window).
    fn multiexec_bar(&mut self, ui: &mut Ui) {
        let accent = Color32::from_rgb(230, 140, 20);
        ui.horizontal(|ui| {
            ui.label(RichText::new("⚡ MultiExec").color(accent).strong());
            let (live, total) = self.dock.iter_all_tabs().fold((0, 0), |(l, t), (_, tab)| {
                (l + usize::from(tab.in_group), t + 1)
            });
            ui.label(format!("typing goes to {live} of {total} tab(s)"));
            if ui.small_button("All").clicked() {
                self.dock
                    .iter_all_tabs_mut()
                    .for_each(|(_, t)| t.in_group = true);
            }
            if ui.small_button("None").clicked() {
                self.dock
                    .iter_all_tabs_mut()
                    .for_each(|(_, t)| t.in_group = false);
            }
            ui.separator();
            let resp = ui.add(
                egui::TextEdit::singleline(&mut self.multiexec_line)
                    .hint_text("command for all tabs in the group")
                    .desired_width(320.0),
            );
            let enter = resp.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter));
            if (ui.button("Send").clicked() || enter) && !self.multiexec_line.is_empty() {
                let line = format!("{}\r", std::mem::take(&mut self.multiexec_line));
                let input = [crate::terminal::Input::Bytes(line.into_bytes())];
                for (_, tab) in self.dock.iter_all_tabs_mut() {
                    if tab.in_group {
                        tab.term.replay(&input);
                    }
                }
                resp.request_focus();
            }
            if ui.button("Stop").clicked() {
                self.multiexec = false;
            }
        });
    }

    /// Copy what was typed into a grouped tab to the rest of the group.
    fn mirror_input(&mut self) {
        let mut typed = Vec::new();
        for (_, tab) in self.dock.iter_all_tabs_mut() {
            let inputs = tab.term.take_input();
            if self.multiexec && tab.in_group && !inputs.is_empty() {
                typed.push((tab.id, inputs));
            }
        }
        for (src, inputs) in typed {
            for (_, tab) in self.dock.iter_all_tabs_mut() {
                if tab.in_group && tab.id != src {
                    tab.term.replay(&inputs);
                }
            }
        }
    }

    /// Entries for the Import menus.
    fn import_menu(&mut self, ui: &mut Ui) {
        if ui.button("GNOME Connection Manager (~/.gcm)").clicked() {
            ui.close();
            self.import_from(ImportSource::Gcm);
        }
        if ui
            .button("MobaXterm sessions file (.mxtsessions)…")
            .clicked()
        {
            ui.close();
            self.import_from(ImportSource::MobaXterm);
        }
        if ui
            .button("PuTTY sessions on this computer")
            .on_hover_text(crate::putty::local_source())
            .clicked()
        {
            ui.close();
            self.import_from(ImportSource::PuttyLocal);
        }
        if ui.button("PuTTY registry export (.reg)…").clicked() {
            ui.close();
            self.import_from(ImportSource::PuttyReg);
        }
    }

    fn import_from(&mut self, source: ImportSource) {
        let pick = |title: &str, name: &str, ext: &[&str]| {
            rfd::FileDialog::new()
                .set_title(title)
                .add_filter(name, ext)
                .add_filter("All files", &["*"])
                .pick_file()
        };
        let result = match source {
            ImportSource::Gcm => match gcm_import::default_path().filter(|p| p.exists()) {
                Some(path) => gcm_import::import(&path).map(|sessions| {
                    let imported = crate::config::Imported {
                        sessions,
                        skipped: Vec::new(),
                    };
                    (imported, path.display().to_string())
                }),
                None => Err(anyhow::anyhow!("~/.gcm/gcm.conf not found")),
            },
            ImportSource::MobaXterm => {
                let Some(path) = pick(
                    "Import MobaXterm sessions",
                    "MobaXterm sessions",
                    &["mxtsessions", "moba"],
                ) else {
                    return;
                };
                crate::mobaxterm::import(&path).map(|i| (i, path.display().to_string()))
            }
            ImportSource::PuttyLocal => {
                crate::putty::import_local().map(|i| (i, crate::putty::local_source()))
            }
            ImportSource::PuttyReg => {
                let Some(path) = pick("Import PuTTY sessions", "Registry export", &["reg"]) else {
                    return;
                };
                crate::putty::import_reg(&path).map(|i| (i, path.display().to_string()))
            }
        };
        match result {
            Ok((imported, _)) if imported.sessions.is_empty() && imported.skipped.is_empty() => {
                self.info("No sessions found to import")
            }
            Ok((imported, source)) => self.modal = Some(Modal::ConfirmImport { imported, source }),
            Err(e) => self.error(format!("Import failed: {e:#}")),
        }
    }

    fn sidebar(&mut self, ui: &mut Ui) {
        ui.horizontal(|ui| {
            ui.selectable_value(&mut self.sidebar_tab, SidebarTab::Sessions, "☰ Sessions");
            ui.selectable_value(&mut self.sidebar_tab, SidebarTab::Sftp, "📁 SFTP");
            ui.selectable_value(&mut self.sidebar_tab, SidebarTab::Tunnels, "⇄ Tunnels");
        });
        ui.separator();
        match self.sidebar_tab {
            SidebarTab::Sessions => self.sessions_ui(ui),
            SidebarTab::Sftp => self.sftp_ui(ui),
            SidebarTab::Tunnels => self.tunnels_ui(ui),
        }
    }

    fn start_tunnel(&mut self, t: &Tunnel) {
        if let Err(e) = self
            .tunnels
            .start(t, &self.store.sessions, &self.store.settings)
        {
            self.error(format!("Tunnel {}: {e:#}", t.display_name()));
        }
    }

    /// Saved port forwards, each started and stopped on its own (MobaSSHTunnel).
    fn tunnels_ui(&mut self, ui: &mut Ui) {
        let has_ssh = self
            .store
            .sessions
            .iter()
            .any(|s| s.protocol == Protocol::Ssh);
        ui.horizontal(|ui| {
            ui.heading("Tunnels");
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let add = ui
                    .add_enabled(has_ssh, egui::Button::new("➕").small())
                    .on_hover_text("New tunnel")
                    .on_disabled_hover_text("Save an SSH session first; tunnels run through one");
                if add.clicked() {
                    let mut tunnel = Tunnel::new();
                    if let Some(s) = self
                        .store
                        .sessions
                        .iter()
                        .find(|s| s.protocol == Protocol::Ssh)
                    {
                        tunnel.session_id = s.id;
                    }
                    self.modal = Some(Modal::EditTunnel {
                        tunnel,
                        is_new: true,
                    });
                }
            });
        });
        if self.store.tunnels.is_empty() {
            ui.label(
                RichText::new(
                    "Port forwards that run on their own, without a terminal tab. \
                     Add one with ➕.",
                )
                .weak(),
            );
            return;
        }
        ui.horizontal(|ui| {
            if ui.small_button("▶ Start all").clicked() {
                for t in self.store.tunnels.clone() {
                    if !self.tunnels.is_running(t.id) {
                        self.start_tunnel(&t);
                    }
                }
            }
            if ui.small_button("■ Stop all").clicked() {
                self.store
                    .tunnels
                    .iter()
                    .for_each(|t| self.tunnels.stop(t.id));
            }
        });
        ui.separator();

        egui::ScrollArea::vertical()
            .auto_shrink([false; 2])
            .show(ui, |ui| {
                for t in self.store.tunnels.clone() {
                    let status = self.tunnels.status(t.id);
                    let (dot, tip) = match &status {
                        TunnelStatus::Stopped => (Color32::GRAY, "Stopped"),
                        TunnelStatus::Starting => (Color32::from_rgb(230, 180, 0), "Connecting…"),
                        TunnelStatus::Up => (Color32::from_rgb(40, 190, 90), "Up"),
                        TunnelStatus::Failed(_) => (Color32::from_rgb(230, 80, 80), "Failed"),
                    };
                    let running = self.tunnels.is_running(t.id);
                    let via = self
                        .store
                        .find(t.session_id)
                        .map(|s| s.display_name())
                        .unwrap_or_else(|| "missing session".into());
                    ui.horizontal(|ui| {
                        ui.label(RichText::new("●").color(dot)).on_hover_text(tip);
                        let toggle = ui
                            .small_button(if running { "■" } else { "▶" })
                            .on_hover_text(if running { "Stop" } else { "Start" });
                        let name = ui
                            .add(
                                egui::Label::new(RichText::new(t.display_name()).strong())
                                    .selectable(false)
                                    .sense(egui::Sense::click()),
                            )
                            .on_hover_text("Double-click to start or stop; right-click for more");
                        if toggle.clicked() || name.double_clicked() {
                            if running {
                                self.tunnels.stop(t.id);
                            } else {
                                self.start_tunnel(&t);
                            }
                        }
                        name.context_menu(|ui| {
                            if ui.button(if running { "Stop" } else { "Start" }).clicked() {
                                if running {
                                    self.tunnels.stop(t.id);
                                } else {
                                    self.start_tunnel(&t);
                                }
                                ui.close();
                            }
                            if ui.button("Edit…").clicked() {
                                self.modal = Some(Modal::EditTunnel {
                                    tunnel: t.clone(),
                                    is_new: false,
                                });
                                ui.close();
                            }
                            if ui.button("Duplicate").clicked() {
                                let mut copy = t.clone();
                                copy.id = new_id();
                                copy.name = format!("{} (copy)", t.display_name());
                                self.store.tunnels.push(copy);
                                self.save();
                                ui.close();
                            }
                            if t.kind != TunnelKind::Remote
                                && ui.button("Copy local address").clicked()
                            {
                                let host = if t.bind_address.is_empty() {
                                    "localhost"
                                } else {
                                    &t.bind_address
                                };
                                ui.ctx().copy_text(format!("{host}:{}", t.listen_port));
                                ui.close();
                            }
                            ui.separator();
                            if ui.button("Delete").clicked() {
                                self.modal = Some(Modal::ConfirmDeleteTunnel(t.id));
                                ui.close();
                            }
                        });
                    });
                    ui.label(
                        RichText::new(format!("{}  via {via}", t.summary()))
                            .small()
                            .weak(),
                    );
                    if let TunnelStatus::Failed(msg) = &status {
                        ui.label(
                            RichText::new(msg)
                                .small()
                                .color(Color32::from_rgb(230, 80, 80)),
                        );
                    }
                    ui.add_space(4.0);
                }
            });
    }

    /// File browser for the SSH session in the focused tab, like MobaXterm's
    /// SFTP pane. It opens its own connection with the system sftp client.
    fn sftp_ui(&mut self, ui: &mut Ui) {
        let active = self
            .dock
            .find_active_focused()
            .map(|(_, tab)| (tab.id, tab.session.clone()));
        let Some((id, session)) = active.filter(|(_, s)| s.protocol == Protocol::Ssh) else {
            ui.label(RichText::new("Select an SSH tab to browse its files.").weak());
            return;
        };
        ui.label(
            RichText::new(format!(
                "{} {}",
                Protocol::Ssh.icon(),
                session.display_name()
            ))
            .strong(),
        );
        let settings = &self.store.settings;
        let browser = self
            .browsers
            .entry(id)
            .or_insert_with(|| Browser::connect(ui.ctx(), &session, settings));

        // Files dropped on the window go to the current remote folder.
        let dropped: Vec<std::path::PathBuf> = ui.input(|i| {
            i.raw
                .dropped_files
                .iter()
                .map(|f| f.path().to_path_buf())
                .collect()
        });
        if !dropped.is_empty() && !browser.is_closed() {
            browser.upload(dropped);
        }
        if ui.input(|i| !i.raw.hovered_files.is_empty()) {
            ui.label(RichText::new("Drop to upload here").color(ui.visuals().hyperlink_color));
        }

        let download_dir = settings.download_dir();
        if browser.ui(ui, &download_dir) {
            self.browsers.insert(
                id,
                Browser::connect(ui.ctx(), &session, &self.store.settings),
            );
        }
    }

    fn sessions_ui(&mut self, ui: &mut Ui) {
        ui.horizontal(|ui| {
            ui.heading("Sessions");
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.small_button("➕").on_hover_text("New session").clicked() {
                    self.modal = Some(Modal::Edit {
                        session: Session::new(Protocol::Ssh),
                        is_new: true,
                        save: true,
                    });
                }
            });
        });
        ui.add(
            egui::TextEdit::singleline(&mut self.filter)
                .hint_text("🔍 Filter")
                .desired_width(f32::INFINITY),
        );
        ui.separator();

        let filter = self.filter.to_lowercase();
        let mut sessions: Vec<Session> = self
            .store
            .sessions
            .iter()
            .filter(|s| {
                filter.is_empty()
                    || [&s.name, &s.host, &s.user, &s.folder, &s.description]
                        .iter()
                        .any(|f| f.to_lowercase().contains(&filter))
            })
            .cloned()
            .collect();
        sessions.sort_by_key(|s| s.display_name().to_lowercase());

        egui::ScrollArea::vertical()
            .auto_shrink([false; 2])
            .show(ui, |ui| {
                if self.store.sessions.is_empty() {
                    ui.label(RichText::new("No saved sessions yet.").weak());
                    ui.menu_button("Import sessions…", |ui| self.import_menu(ui));
                }
                self.folder_ui(ui, "", &sessions, !filter.is_empty());
            });
    }

    /// One level of the tree: sub-folders first, then the sessions in `folder`.
    fn folder_ui(&mut self, ui: &mut Ui, folder: &str, sessions: &[Session], expand_all: bool) {
        let prefix = if folder.is_empty() {
            String::new()
        } else {
            format!("{folder}/")
        };
        let mut children: Vec<String> = sessions
            .iter()
            .filter_map(|s| {
                s.folder
                    .strip_prefix(&prefix)
                    .filter(|r| !r.is_empty() || !folder.is_empty())
            })
            .filter_map(|rest| rest.split('/').find(|p| !p.is_empty()).map(str::to_string))
            .filter(|c| !c.is_empty())
            .collect();
        children.sort_by_key(|c| c.to_lowercase());
        children.dedup();

        for child in children {
            let path = format!("{prefix}{child}");
            let open = !self.store.settings.collapsed.contains(&path);
            let id = ui.make_persistent_id(("folder", &path));
            let mut state = egui::collapsing_header::CollapsingState::load_with_default_open(
                ui.ctx(),
                id,
                open || expand_all,
            );
            if expand_all {
                state.set_open(true);
            }
            let header = state.show_header(ui, |ui| {
                ui.label(format!("📁 {child}"));
            });
            let now_open = header.is_open();
            header.body(|ui| self.folder_ui(ui, &path, sessions, expand_all));
            if !expand_all && now_open != open {
                if now_open {
                    self.store.settings.collapsed.retain(|c| c != &path);
                } else {
                    self.store.settings.collapsed.push(path.clone());
                }
                self.save();
            }
        }

        for s in sessions
            .iter()
            .filter(|s| s.folder.trim_matches('/') == folder)
        {
            let label = format!("{} {}", s.protocol.icon(), s.display_name());
            let resp = ui.selectable_label(false, label);
            let resp = if s.description.is_empty() && s.host.is_empty() {
                resp
            } else {
                let mut tip = format!("{}  {}", s.protocol.label(), s.host);
                if !s.user.is_empty() {
                    tip = format!("{}  {}@{}", s.protocol.label(), s.user, s.host);
                }
                if !s.description.is_empty() {
                    tip.push_str(&format!("\n{}", s.description));
                }
                resp.on_hover_text(tip)
            };
            if resp.double_clicked() {
                self.actions.push(Action::Open(s.clone()));
            }
            resp.context_menu(|ui| {
                if ui.button("Connect").clicked() {
                    self.actions.push(Action::Open(s.clone()));
                    ui.close();
                }
                if ui.button("Edit…").clicked() {
                    self.actions.push(Action::Edit(s.clone()));
                    ui.close();
                }
                if ui.button("Duplicate").clicked() {
                    let mut copy = s.clone();
                    copy.id = new_id();
                    copy.name = format!("{} (copy)", s.display_name());
                    self.store.upsert(copy);
                    self.save();
                    ui.close();
                }
                ui.separator();
                if ui.button("Delete").clicked() {
                    self.actions.push(Action::Delete(s.id));
                    ui.close();
                }
            });
        }
    }

    fn welcome(&mut self, ui: &mut Ui) {
        ui.vertical_centered(|ui| {
            ui.add_space(ui.available_height() * 0.2);
            ui.heading(RichText::new("GNOME Connection Manager").size(28.0));
            ui.label(RichText::new("SSH · Telnet · Serial · RDP · VNC · Local shell").weak());
            ui.add_space(24.0);
            ui.horizontal(|ui| {
                let w = 520.0_f32.min(ui.available_width());
                ui.add_space((ui.available_width() - w).max(0.0) / 2.0);
                let size = egui::vec2(120.0, 60.0);
                if ui
                    .add_sized(size, egui::Button::new("➕\nNew session"))
                    .clicked()
                {
                    self.modal = Some(Modal::Edit {
                        session: Session::new(Protocol::Ssh),
                        is_new: true,
                        save: true,
                    });
                }
                if ui
                    .add_sized(size, egui::Button::new("💻\nLocal shell"))
                    .clicked()
                {
                    self.actions
                        .push(Action::Open(Session::new(Protocol::Local)));
                }
                if ui
                    .add_sized(size, egui::Button::new("🔌\nSerial port"))
                    .clicked()
                {
                    self.modal = Some(Modal::Edit {
                        session: Session::new(Protocol::Serial),
                        is_new: true,
                        save: false,
                    });
                }
                let import = ui.add_sized(size, egui::Button::new("📥\nImport…"));
                egui::Popup::menu(&import).show(|ui| self.import_menu(ui));
            });
            ui.add_space(16.0);
            ui.label(
                RichText::new(
                    "Double-click a saved session to connect. Drag tabs to split the view.",
                )
                .weak(),
            );
        });
    }

    fn modal_ui(&mut self, ctx: &egui::Context) {
        let Some(mut modal) = self.modal.take() else {
            return;
        };
        let mut keep = true;
        match &mut modal {
            Modal::Edit {
                session,
                is_new,
                save,
            } => {
                let title = if *is_new {
                    "New session"
                } else {
                    "Edit session"
                };
                egui::Window::new(title)
                    .collapsible(false)
                    .resizable(false)
                    .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
                    .show(ctx, |ui| {
                        session_form(ui, session, &self.store.folders());
                        ui.separator();
                        if *is_new {
                            ui.checkbox(save, "Save to session list");
                        }
                        ui.horizontal(|ui| {
                            let valid = session.protocol == Protocol::Local
                                || !session.host.trim().is_empty();
                            let connect_label = if *save { "Save and connect" } else { "Connect" };
                            if ui
                                .add_enabled(valid, egui::Button::new(connect_label))
                                .clicked()
                            {
                                session.host = session.host.trim().to_string();
                                if *save {
                                    self.store.upsert(session.clone());
                                    self.save();
                                }
                                self.actions.push(Action::Open(session.clone()));
                                keep = false;
                            }
                            if *save && ui.add_enabled(valid, egui::Button::new("Save")).clicked() {
                                session.host = session.host.trim().to_string();
                                self.store.upsert(session.clone());
                                self.save();
                                keep = false;
                            }
                            if ui.button("Cancel").clicked()
                                || ui.input(|i| i.key_pressed(Key::Escape))
                            {
                                keep = false;
                            }
                        });
                    });
            }
            Modal::Settings(s) => {
                egui::Window::new("Settings")
                    .collapsible(false)
                    .resizable(false)
                    .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
                    .show(ctx, |ui| {
                        settings_form(ui, s);
                        ui.separator();
                        ui.horizontal(|ui| {
                            if ui.button("Save").clicked() {
                                ctx.set_visuals(if s.dark_mode {
                                    egui::Visuals::dark()
                                } else {
                                    egui::Visuals::light()
                                });
                                if s.font_path != self.store.settings.font_path {
                                    crate::fonts::install(ctx, &s.font_path);
                                }
                                self.store.settings = s.clone();
                                self.save();
                                keep = false;
                            }
                            if ui.button("Cancel").clicked() {
                                keep = false;
                            }
                            ui.label(
                                RichText::new(self.store.path().display().to_string())
                                    .weak()
                                    .small(),
                            );
                        });
                    });
            }
            Modal::ConfirmClose(id) => {
                let id = *id;
                confirm(
                    ctx,
                    "Close tab",
                    "The session is still running. Close it?",
                    &mut keep,
                    || self.actions.push(Action::Close(id)),
                );
            }
            Modal::ConfirmDelete(id) => {
                let id = *id;
                let name = self
                    .store
                    .find(id)
                    .map(|s| s.display_name())
                    .unwrap_or_default();
                let mut delete = false;
                confirm(
                    ctx,
                    "Delete session",
                    &format!("Delete \"{name}\"?"),
                    &mut keep,
                    || delete = true,
                );
                if delete {
                    self.store.remove(id);
                    self.save();
                }
            }
            Modal::ConfirmImport { imported, source } => {
                let new = imported
                    .sessions
                    .iter()
                    .filter(|s| !self.store.has_equivalent(s))
                    .count();
                let dup = imported.sessions.len() - new;
                egui::Window::new("Import sessions")
                    .collapsible(false)
                    .resizable(false)
                    .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
                    .show(ctx, |ui| {
                        ui.label(RichText::new(source.as_str()).weak().small());
                        ui.label(format!("{new} new session(s) to import."));
                        if dup > 0 {
                            ui.label(format!(
                                "{dup} already in your list will be left as they are."
                            ));
                        }
                        if !imported.skipped.is_empty() {
                            egui::CollapsingHeader::new(format!(
                                "{} entr{} cannot be imported",
                                imported.skipped.len(),
                                if imported.skipped.len() == 1 {
                                    "y"
                                } else {
                                    "ies"
                                }
                            ))
                            .show(ui, |ui| {
                                egui::ScrollArea::vertical()
                                    .max_height(160.0)
                                    .show(ui, |ui| {
                                        for why in &imported.skipped {
                                            ui.label(RichText::new(why).small());
                                        }
                                    });
                            });
                        }
                        ui.separator();
                        ui.horizontal(|ui| {
                            if ui
                                .add_enabled(new > 0, egui::Button::new("Import"))
                                .clicked()
                            {
                                let sessions = std::mem::take(&mut imported.sessions);
                                let (added, _) = self.store.import(sessions);
                                self.save();
                                self.info(format!("Imported {added} session(s)"));
                                keep = false;
                            }
                            if ui.button("Cancel").clicked()
                                || ui.input(|i| i.key_pressed(Key::Escape))
                            {
                                keep = false;
                            }
                        });
                    });
            }
            Modal::EditTunnel { tunnel, is_new } => {
                let title = if *is_new { "New tunnel" } else { "Edit tunnel" };
                let ssh: Vec<(u64, String)> = self
                    .store
                    .sessions
                    .iter()
                    .filter(|s| s.protocol == Protocol::Ssh)
                    .map(|s| (s.id, s.display_name()))
                    .collect();
                egui::Window::new(title)
                    .collapsible(false)
                    .resizable(false)
                    .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
                    .min_width(500.0)
                    .show(ctx, |ui| {
                        tunnel_form(ui, tunnel, &ssh);
                        ui.separator();
                        let problem = tunnel.validate().err().map(|e| e.to_string());
                        if let Some(p) = &problem {
                            ui.label(RichText::new(p).small().weak());
                        }
                        ui.horizontal(|ui| {
                            let ok = problem.is_none();
                            let save = ui.add_enabled(ok, egui::Button::new("Save")).clicked();
                            let start = ui
                                .add_enabled(ok, egui::Button::new("Save and start"))
                                .clicked();
                            if save || start {
                                tunnel.dest_host = tunnel.dest_host.trim().to_string();
                                let was_running = self.tunnels.is_running(tunnel.id);
                                match self.store.tunnels.iter_mut().find(|t| t.id == tunnel.id) {
                                    Some(existing) => *existing = tunnel.clone(),
                                    None => self.store.tunnels.push(tunnel.clone()),
                                }
                                self.save();
                                if start || was_running {
                                    let t = tunnel.clone();
                                    self.start_tunnel(&t);
                                }
                                keep = false;
                            }
                            if ui.button("Cancel").clicked()
                                || ui.input(|i| i.key_pressed(Key::Escape))
                            {
                                keep = false;
                            }
                        });
                    });
            }
            Modal::ConfirmDeleteTunnel(id) => {
                let id = *id;
                let name = self
                    .store
                    .tunnels
                    .iter()
                    .find(|t| t.id == id)
                    .map(|t| t.display_name())
                    .unwrap_or_default();
                let mut delete = false;
                confirm(
                    ctx,
                    "Delete tunnel",
                    &format!("Delete tunnel \"{name}\"?"),
                    &mut keep,
                    || delete = true,
                );
                if delete {
                    self.tunnels.stop(id);
                    self.store.tunnels.retain(|t| t.id != id);
                    self.save();
                }
            }
            Modal::About => {
                egui::Window::new("About")
                    .collapsible(false)
                    .resizable(false)
                    .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
                    .show(ctx, |ui| {
                        ui.heading(format!(
                            "GNOME Connection Manager {}",
                            env!("CARGO_PKG_VERSION")
                        ));
                        ui.label("Rust/egui rewrite of the tabbed connection manager.");
                        ui.label("Terminal emulation by alacritty_terminal.");
                        if ui.button("Close").clicked() {
                            keep = false;
                        }
                    });
            }
        }
        if keep && self.modal.is_none() {
            self.modal = Some(modal);
        }
    }
}

fn confirm(
    ctx: &egui::Context,
    title: &str,
    text: &str,
    keep: &mut bool,
    mut on_yes: impl FnMut(),
) {
    egui::Window::new(title)
        .collapsible(false)
        .resizable(false)
        .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
        .show(ctx, |ui| {
            ui.label(text);
            ui.horizontal(|ui| {
                if ui.button("Yes").clicked() || ui.input(|i| i.key_pressed(Key::Enter)) {
                    on_yes();
                    *keep = false;
                }
                if ui.button("No").clicked() || ui.input(|i| i.key_pressed(Key::Escape)) {
                    *keep = false;
                }
            });
        });
}

fn session_form(ui: &mut Ui, s: &mut Session, folders: &[String]) {
    egui::Grid::new("session_form")
        .num_columns(2)
        .spacing([12.0, 6.0])
        .show(ui, |ui| {
            ui.label("Type");
            ui.horizontal_wrapped(|ui| {
                for p in Protocol::ALL {
                    ui.selectable_value(&mut s.protocol, p, format!("{} {}", p.icon(), p.label()));
                }
            });
            ui.end_row();

            ui.label("Name");
            ui.text_edit_singleline(&mut s.name);
            ui.end_row();

            ui.label("Folder");
            ui.horizontal(|ui| {
                ui.add(egui::TextEdit::singleline(&mut s.folder).hint_text("e.g. prod/web"));
                egui::ComboBox::from_id_salt("folder_pick")
                    .selected_text("")
                    .width(24.0)
                    .show_ui(ui, |ui| {
                        for f in folders {
                            ui.selectable_value(&mut s.folder, f.clone(), f);
                        }
                    });
            });
            ui.end_row();

            match s.protocol {
                Protocol::Serial => {
                    ui.label("Device");
                    ui.horizontal(|ui| {
                        let hint = if cfg!(windows) {
                            "COM3"
                        } else {
                            "/dev/ttyUSB0"
                        };
                        ui.add(egui::TextEdit::singleline(&mut s.host).hint_text(hint));
                        egui::ComboBox::from_id_salt("ports")
                            .selected_text("")
                            .width(24.0)
                            .show_ui(ui, |ui| match serialport::available_ports() {
                                Ok(ports) if !ports.is_empty() => {
                                    for p in ports {
                                        ui.selectable_value(
                                            &mut s.host,
                                            p.port_name.clone(),
                                            &p.port_name,
                                        );
                                    }
                                }
                                _ => {
                                    ui.label("No ports found");
                                }
                            });
                    });
                    ui.end_row();
                    ui.label("Speed");
                    egui::ComboBox::from_id_salt("baud")
                        .selected_text(s.serial.baud.to_string())
                        .show_ui(ui, |ui| {
                            for b in [
                                300, 1200, 2400, 4800, 9600, 19200, 38400, 57600, 115200, 230400,
                                460800, 921600,
                            ] {
                                ui.selectable_value(&mut s.serial.baud, b, b.to_string());
                            }
                        });
                    ui.end_row();
                    ui.label("Framing");
                    ui.horizontal(|ui| {
                        egui::ComboBox::from_id_salt("bits")
                            .selected_text(format!("{} bits", s.serial.data_bits))
                            .show_ui(ui, |ui| {
                                for b in [5u8, 6, 7, 8] {
                                    ui.selectable_value(
                                        &mut s.serial.data_bits,
                                        b,
                                        format!("{b} bits"),
                                    );
                                }
                            });
                        egui::ComboBox::from_id_salt("parity")
                            .selected_text(format!("{:?} parity", s.serial.parity))
                            .show_ui(ui, |ui| {
                                for p in [Parity::None, Parity::Odd, Parity::Even] {
                                    ui.selectable_value(&mut s.serial.parity, p, format!("{p:?}"));
                                }
                            });
                        egui::ComboBox::from_id_salt("stop")
                            .selected_text(format!("{} stop", s.serial.stop_bits))
                            .show_ui(ui, |ui| {
                                for b in [1u8, 2] {
                                    ui.selectable_value(
                                        &mut s.serial.stop_bits,
                                        b,
                                        format!("{b} stop"),
                                    );
                                }
                            });
                    });
                    ui.end_row();
                    ui.label("Flow control");
                    egui::ComboBox::from_id_salt("flow")
                        .selected_text(format!("{:?}", s.serial.flow_control))
                        .show_ui(ui, |ui| {
                            for f in [
                                FlowControl::None,
                                FlowControl::Software,
                                FlowControl::Hardware,
                            ] {
                                ui.selectable_value(
                                    &mut s.serial.flow_control,
                                    f,
                                    format!("{f:?}"),
                                );
                            }
                        });
                    ui.end_row();
                }
                Protocol::Local => {
                    ui.label("Shell");
                    ui.add(
                        egui::TextEdit::singleline(&mut s.host).hint_text(launch::default_shell()),
                    );
                    ui.end_row();
                    ui.label("Arguments");
                    ui.text_edit_singleline(&mut s.extra_args);
                    ui.end_row();
                }
                _ => {
                    ui.label("Host");
                    ui.horizontal(|ui| {
                        ui.text_edit_singleline(&mut s.host);
                        ui.label("Port");
                        let mut port = if s.port == 0 {
                            String::new()
                        } else {
                            s.port.to_string()
                        };
                        let resp = ui.add(
                            egui::TextEdit::singleline(&mut port)
                                .hint_text(s.protocol.default_port().to_string())
                                .desired_width(60.0),
                        );
                        if resp.changed() {
                            s.port = port.trim().parse().unwrap_or(0);
                        }
                    });
                    ui.end_row();
                    ui.label("User");
                    ui.text_edit_singleline(&mut s.user);
                    ui.end_row();
                    ui.label("Password");
                    ui.add(egui::TextEdit::singleline(&mut s.password).password(true))
                        .on_hover_text("Typed in when the server asks. SSH keys are safer.");
                    ui.end_row();
                }
            }

            if s.protocol == Protocol::Ssh {
                ui.label("Private key");
                ui.add(
                    egui::TextEdit::singleline(&mut s.private_key).hint_text("~/.ssh/id_ed25519"),
                );
                ui.end_row();
                ui.label("Options");
                ui.horizontal(|ui| {
                    ui.checkbox(&mut s.x11, "X11 forwarding");
                    ui.checkbox(&mut s.agent_forward, "Agent forwarding");
                    ui.checkbox(&mut s.compression, "Compression");
                });
                ui.end_row();
                ui.label("Keep-alive (s)");
                ui.add(egui::DragValue::new(&mut s.keepalive).range(0..=3600));
                ui.end_row();
                ui.label("Tunnels");
                let mut text = s.tunnels.join("\n");
                if ui
                    .add(
                        egui::TextEdit::multiline(&mut text)
                            .desired_rows(2)
                            .hint_text("L 8080:localhost:80\nR 9000:localhost:9000\nD 1080"),
                    )
                    .changed()
                {
                    s.tunnels = text
                        .lines()
                        .map(str::trim)
                        .filter(|l| !l.is_empty())
                        .map(str::to_string)
                        .collect();
                }
                ui.end_row();
            }
            if matches!(s.protocol, Protocol::Ssh | Protocol::Telnet) {
                ui.label("Extra arguments");
                ui.text_edit_singleline(&mut s.extra_args);
                ui.end_row();
            }
            if !s.protocol.is_external() {
                ui.label("Startup commands");
                ui.add(
                    egui::TextEdit::multiline(&mut s.commands)
                        .desired_rows(2)
                        .hint_text("One per line; ##D=1000 waits 1s"),
                );
                ui.end_row();
            }
            if !s.protocol.is_external() {
                ui.label("Colours");
                ui.horizontal(|ui| {
                    color_choice(ui, &mut s.fg_color, "Text", [0xd4, 0xd4, 0xd4]);
                    color_choice(ui, &mut s.bg_color, "Background", [0x1e, 0x1e, 0x1e]);
                });
                ui.end_row();
                ui.label("Logging");
                ui.checkbox(&mut s.log, "Save terminal output to a log file");
                ui.end_row();
            }
            ui.label("Description");
            ui.text_edit_singleline(&mut s.description);
            ui.end_row();
        });
}

fn tunnel_form(ui: &mut Ui, t: &mut Tunnel, ssh_sessions: &[(u64, String)]) {
    let port_edit = |ui: &mut Ui, port: &mut u16| {
        let mut text = if *port == 0 {
            String::new()
        } else {
            port.to_string()
        };
        if ui
            .add(egui::TextEdit::singleline(&mut text).desired_width(60.0))
            .changed()
        {
            *port = text.trim().parse().unwrap_or(0);
        }
    };
    egui::Grid::new("tunnel_form")
        .num_columns(2)
        .spacing([12.0, 6.0])
        .show(ui, |ui| {
            ui.label("Name");
            ui.add(egui::TextEdit::singleline(&mut t.name).hint_text("e.g. Postgres on prod"));
            ui.end_row();

            ui.label("Through");
            let current = ssh_sessions
                .iter()
                .find(|(id, _)| *id == t.session_id)
                .map(|(_, n)| n.clone())
                .unwrap_or_else(|| "choose an SSH session".into());
            egui::ComboBox::from_id_salt("tunnel_session")
                .selected_text(current)
                .show_ui(ui, |ui| {
                    for (id, name) in ssh_sessions {
                        ui.selectable_value(&mut t.session_id, *id, name);
                    }
                });
            ui.end_row();

            ui.label("Type");
            ui.vertical(|ui| {
                for k in TunnelKind::ALL {
                    ui.radio_value(&mut t.kind, k, k.label());
                }
            });
            ui.end_row();

            let (listen_label, bind_hint) = match t.kind {
                TunnelKind::Remote => ("Port on server", "server default"),
                _ => ("Local port", "localhost"),
            };
            ui.label(listen_label);
            ui.horizontal(|ui| {
                port_edit(ui, &mut t.listen_port);
                ui.label("bind address");
                ui.add(
                    egui::TextEdit::singleline(&mut t.bind_address)
                        .hint_text(bind_hint)
                        .desired_width(120.0),
                );
            });
            ui.end_row();

            if t.kind != TunnelKind::Dynamic {
                ui.label(if t.kind == TunnelKind::Local {
                    "Destination (from server)"
                } else {
                    "Destination (from here)"
                });
                ui.horizontal(|ui| {
                    ui.add(
                        egui::TextEdit::singleline(&mut t.dest_host)
                            .hint_text("host")
                            .desired_width(160.0),
                    );
                    ui.label("port");
                    port_edit(ui, &mut t.dest_port);
                });
                ui.end_row();
            }

            ui.label("");
            ui.checkbox(&mut t.autostart, "Start when gcm starts");
            ui.end_row();
        });
    let help = match t.kind {
        TunnelKind::Local => {
            "Connections to the local port are forwarded through the server to the destination."
        }
        TunnelKind::Remote => {
            "Connections to the port on the server are forwarded back here, to the destination."
        }
        TunnelKind::Dynamic => "A SOCKS5 proxy on the local port; point a browser or tool at it.",
    };
    ui.label(RichText::new(help).small().weak());
}

/// A checkbox to override a colour plus its picker. `value` is `#rrggbb`,
/// or empty for the theme's colour.
fn color_choice(ui: &mut Ui, value: &mut String, label: &str, default: [u8; 3]) {
    let mut custom = !value.is_empty();
    if ui.checkbox(&mut custom, label).changed() {
        *value = if custom {
            crate::config::color_hex(default)
        } else {
            String::new()
        };
    }
    if custom {
        let mut rgb = crate::config::parse_color(value).unwrap_or(default);
        if ui.color_edit_button_srgb(&mut rgb).changed() {
            *value = crate::config::color_hex(rgb);
        }
    }
}

fn settings_form(ui: &mut Ui, s: &mut Settings) {
    egui::Grid::new("settings_form")
        .num_columns(2)
        .spacing([12.0, 6.0])
        .show(ui, |ui| {
            ui.label("Font size");
            ui.add(egui::Slider::new(&mut s.font_size, 8.0..=32.0));
            ui.end_row();
            ui.label("Font file");
            ui.add(
                egui::TextEdit::singleline(&mut s.font_path)
                    .hint_text("built-in (Hack); path to a .ttf/.otf")
                    .desired_width(360.0),
            );
            ui.end_row();
            ui.label("Scrollback lines");
            ui.add(
                egui::DragValue::new(&mut s.scrollback)
                    .range(100..=200_000)
                    .speed(100),
            );
            ui.end_row();
            ui.label("Behaviour");
            ui.vertical(|ui| {
                ui.checkbox(&mut s.dark_mode, "Dark theme");
                ui.checkbox(&mut s.copy_on_select, "Copy on select");
                ui.checkbox(
                    &mut s.paste_on_right_click,
                    "Paste on right click (otherwise show a menu)",
                );
                ui.checkbox(
                    &mut s.confirm_close_tab,
                    "Confirm before closing a running tab",
                );
                ui.checkbox(
                    &mut s.open_local_on_startup,
                    "Open a local shell on startup",
                );
            });
            ui.end_row();
            ui.label("SSH command");
            ui.text_edit_singleline(&mut s.ssh_command);
            ui.end_row();
            ui.label("Telnet command");
            ui.text_edit_singleline(&mut s.telnet_command);
            ui.end_row();
            ui.label("SFTP command");
            ui.text_edit_singleline(&mut s.sftp_command);
            ui.end_row();
            ui.label("Download folder");
            let default_dl = dirs::download_dir()
                .map(|d| d.display().to_string())
                .unwrap_or_default();
            ui.add(egui::TextEdit::singleline(&mut s.download_dir).hint_text(default_dl));
            ui.end_row();
            ui.label("Log folder");
            let default_logs = crate::config::config_dir()
                .join("logs")
                .display()
                .to_string();
            ui.add(egui::TextEdit::singleline(&mut s.log_dir).hint_text(default_logs));
            ui.end_row();
            ui.label("Local shell");
            ui.add(
                egui::TextEdit::singleline(&mut s.local_shell).hint_text(launch::default_shell()),
            );
            ui.end_row();
            ui.label("RDP client");
            ui.add(egui::TextEdit::singleline(&mut s.rdp_command).desired_width(360.0));
            ui.end_row();
            ui.label("VNC client");
            ui.add(egui::TextEdit::singleline(&mut s.vnc_command).desired_width(360.0));
            ui.end_row();
            ui.label("");
            ui.label(
                RichText::new("Placeholders: {host} {port} {user} {password}")
                    .weak()
                    .small(),
            );
            ui.end_row();
        });
}

struct Viewer<'a> {
    settings: &'a Settings,
    actions: &'a mut Vec<Action>,
    modal: &'a mut Option<Modal>,
    multiexec: bool,
}

impl TabViewer for Viewer<'_> {
    type Tab = Tab;

    fn id(&mut self, tab: &mut Tab) -> egui::Id {
        egui::Id::new(("tab", tab.id))
    }

    fn title(&mut self, tab: &mut Tab) -> WidgetText {
        // Local shells have no saved name, so show what the shell reports (usually the cwd).
        let label = match tab.term.title() {
            Some(t) if tab.session.protocol == Protocol::Local && tab.session.name.is_empty() => t,
            _ => tab.session.display_name(),
        };
        let mut name = format!("{} {}", tab.session.protocol.icon(), label);
        if tab.term.log_path().is_some() {
            name = format!("● {name}");
        }
        if self.multiexec && tab.in_group {
            name = format!("⚡ {name}");
        }
        if tab.term.is_exited() {
            RichText::new(name).color(Color32::GRAY).italics().into()
        } else {
            name.into()
        }
    }

    fn ui(&mut self, ui: &mut Ui, tab: &mut Tab) {
        let exited = tab.term.is_exited();
        if exited {
            let resp = ui.horizontal(|ui| {
                ui.label(RichText::new("Session stopped.").color(Color32::from_rgb(220, 120, 0)));
                let r = ui.button("⟳ Reconnect (R)").clicked();
                let c = ui.button("Close tab (Enter)").clicked();
                (r, c)
            });
            let (reconnect, close) = resp.inner;
            let (key_r, key_enter) =
                ui.input(|i| (i.key_pressed(Key::R), i.key_pressed(Key::Enter)));
            // Only react to keys when no text field has focus.
            let free = ui.memory(|m| m.focused().is_none());
            if reconnect || (free && key_r) {
                self.actions.push(Action::Reconnect(tab.id));
            } else if close || (free && key_enter) {
                self.actions.push(Action::Close(tab.id));
            }
        }
        let s = self.settings;
        let rect = ui.available_rect_before_wrap();
        tab.term.ui(
            ui,
            s.font_size,
            s.dark_mode,
            s.copy_on_select,
            s.paste_on_right_click,
        );
        if self.multiexec && tab.in_group {
            // Make it obvious which terminals will receive keystrokes.
            ui.painter().rect_stroke(
                rect,
                0.0,
                egui::Stroke::new(2.0, Color32::from_rgb(230, 140, 20)),
                egui::StrokeKind::Inside,
            );
        }
    }

    fn context_menu(&mut self, ui: &mut Ui, tab: &mut Tab, path: NodePath) {
        ui.checkbox(&mut tab.in_group, "Include in MultiExec");
        match tab.term.log_path() {
            Some(path) => {
                if ui
                    .button("Stop logging")
                    .on_hover_text(path.display().to_string())
                    .clicked()
                {
                    tab.term.stop_log();
                    self.actions.push(Action::Notify(
                        format!("Log saved: {}", path.display()),
                        false,
                    ));
                    ui.close();
                }
            }
            None => {
                if ui.button("Start logging").clicked() {
                    let dir = self.settings.log_dir();
                    let note = match tab.term.start_log(&dir, &tab.session.display_name()) {
                        Ok(path) => Action::Notify(format!("Logging to {}", path.display()), false),
                        Err(e) => Action::Notify(format!("Cannot start log: {e:#}"), true),
                    };
                    self.actions.push(note);
                    ui.close();
                }
            }
        }
        if ui.button("Open log folder").clicked() {
            let dir = self.settings.log_dir();
            if let Err(e) = std::fs::create_dir_all(&dir)
                .map_err(anyhow::Error::from)
                .and_then(|_| launch::open_path(&dir))
            {
                self.actions
                    .push(Action::Notify(format!("{}: {e:#}", dir.display()), true));
            }
            ui.close();
        }
        ui.separator();
        if ui.button("Duplicate").clicked() {
            self.actions.push(Action::Open(tab.session.clone()));
            ui.close();
        }
        if ui.button("Split right").clicked() {
            self.actions
                .push(Action::Split(tab.session.clone(), path, Split::Right));
            ui.close();
        }
        if ui.button("Split down").clicked() {
            self.actions
                .push(Action::Split(tab.session.clone(), path, Split::Below));
            ui.close();
        }
        if ui.button("Reconnect").clicked() {
            self.actions.push(Action::Reconnect(tab.id));
            ui.close();
        }
        if ui.button("Clear scrollback").clicked() {
            tab.term.clear_scrollback();
            ui.close();
        }
        // egui_dock appends its own Eject and Close entries.
    }

    fn on_close(&mut self, tab: &mut Tab) -> OnCloseResponse {
        if self.settings.confirm_close_tab && !tab.term.is_exited() {
            *self.modal = Some(Modal::ConfirmClose(tab.id));
            OnCloseResponse::Ignore
        } else {
            OnCloseResponse::Close
        }
    }

    fn on_tab_button(&mut self, tab: &mut Tab, response: &egui::Response) {
        if response.clicked() {
            tab.term.request_focus();
        }
    }

    fn scroll_bars(&self, _tab: &Tab) -> [bool; 2] {
        [false, false]
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        let ctx = &ctx;
        self.run_actions(ctx);
        self.tunnels.poll();
        if self.tunnels.running_count() > 0 {
            ctx.request_repaint_after(std::time::Duration::from_millis(500));
        }
        // Drop browsers whose terminal tab has been closed.
        if !self.browsers.is_empty() {
            let live: std::collections::HashSet<u64> =
                self.dock.iter_all_tabs().map(|(_, t)| t.id).collect();
            self.browsers.retain(|id, _| live.contains(id));
        }

        egui::Panel::top("toolbar").show(ui, |ui| self.toolbar(ui));
        if self.multiexec {
            egui::Panel::top("multiexec").show(ui, |ui| self.multiexec_bar(ui));
        }

        egui::Panel::bottom("status").show(ui, |ui| {
            ui.horizontal(|ui| {
                let tabs = self.dock.iter_all_tabs().count();
                ui.label(format!("{tabs} tab(s)"));
                let (up, running) = (self.tunnels.up_count(), self.tunnels.running_count());
                if running > 0 {
                    ui.separator();
                    let text = format!("⇄ {up}/{running} tunnel(s) up");
                    if ui.link(text).clicked() {
                        self.show_sidebar = true;
                        self.sidebar_tab = SidebarTab::Tunnels;
                    }
                }
                if let Some((msg, is_err)) = &self.status {
                    ui.separator();
                    let color = if *is_err {
                        Color32::from_rgb(230, 80, 80)
                    } else {
                        ui.visuals().weak_text_color()
                    };
                    ui.label(RichText::new(msg).color(color));
                }
            });
        });

        if self.show_sidebar {
            let resp = egui::Panel::left("sessions")
                .resizable(true)
                .default_size(self.store.settings.sidebar_width)
                .size_range(160.0..=600.0)
                .show(ui, |ui| self.sidebar(ui));
            self.store.settings.sidebar_width = resp.response.rect.width();
        }

        egui::CentralPanel::default()
            .frame(egui::Frame::NONE)
            .show(ui, |ui| {
                if self.dock.iter_all_tabs().next().is_none() {
                    self.welcome(ui);
                    return;
                }
                let mut style = Style::from_egui(ui.style().as_ref());
                style.tab_bar.fill_tab_bar = true;
                let mut viewer = Viewer {
                    settings: &self.store.settings,
                    actions: &mut self.actions,
                    modal: &mut self.modal,
                    multiexec: self.multiexec,
                };
                DockArea::new(&mut self.dock)
                    .style(style)
                    .show_leaf_collapse_buttons(false)
                    .show_leaf_close_all_buttons(false)
                    .show_inside(ui, &mut viewer);
            });
        self.mirror_input();

        self.modal_ui(ctx);
        if !self.actions.is_empty() {
            ctx.request_repaint();
        }
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        // Stop tunnels now rather than whenever the app is dropped.
        self.tunnels = TunnelManager::default();
        self.save();
    }
}
