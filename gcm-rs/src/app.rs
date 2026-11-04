//! Main window: session tree on the left, tabbed and splittable terminals
//! in the middle, MobaXterm-style toolbar with quick connect on top.

use eframe::egui::{self, Color32, Key, RichText, Ui, WidgetText};
use egui_dock::tab_viewer::OnCloseResponse;
use egui_dock::{DockArea, DockState, Node, NodePath, Split, Style, TabViewer};

use crate::config::{FlowControl, Parity, Protocol, Session, Settings, Store, new_id};
use crate::gcm_import;
use crate::launch::{self, Launch};
use crate::terminal::Terminal;

pub struct Tab {
    id: u64,
    session: Session,
    term: Terminal,
}

enum Action {
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
    ConfirmImport(Vec<Session>),
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
    status: Option<(String, bool)>,
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
            status: None,
        };
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
                if ui.button("Import from GNOME Connection Manager…").clicked() {
                    self.import_gcm();
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

    fn import_gcm(&mut self) {
        let Some(path) = gcm_import::default_path() else {
            self.error("Cannot find the home directory");
            return;
        };
        if !path.exists() {
            self.error(format!("{} not found", path.display()));
            return;
        }
        match gcm_import::import(&path) {
            Ok(list) if list.is_empty() => self.info("No hosts found in the GCM config"),
            Ok(list) => self.modal = Some(Modal::ConfirmImport(list)),
            Err(e) => self.error(format!("Import failed: {e:#}")),
        }
    }

    fn sidebar(&mut self, ui: &mut Ui) {
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
                    if ui.link("Import from GNOME Connection Manager").clicked() {
                        self.import_gcm();
                    }
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
                if ui
                    .add_sized(size, egui::Button::new("📥\nImport GCM"))
                    .clicked()
                {
                    self.import_gcm();
                }
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
            Modal::ConfirmImport(list) => {
                let n = list.len();
                let mut import = false;
                confirm(
                    ctx,
                    "Import",
                    &format!(
                        "Import {n} session(s) from {}?",
                        gcm_import::default_path().unwrap().display()
                    ),
                    &mut keep,
                    || import = true,
                );
                if import {
                    let list = std::mem::take(list);
                    for s in list {
                        self.store.upsert(s);
                    }
                    self.save();
                    self.info(format!("Imported {n} session(s)"));
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
            ui.label("Description");
            ui.text_edit_singleline(&mut s.description);
            ui.end_row();
        });
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
        let name = format!("{} {}", tab.session.protocol.icon(), label);
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
        tab.term.ui(
            ui,
            s.font_size,
            s.dark_mode,
            s.copy_on_select,
            s.paste_on_right_click,
        );
    }

    fn context_menu(&mut self, ui: &mut Ui, tab: &mut Tab, path: NodePath) {
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
        ui.separator();
        if ui.button("Close").clicked() {
            self.actions.push(Action::Close(tab.id));
            ui.close();
        }
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

        egui::Panel::top("toolbar").show(ui, |ui| self.toolbar(ui));

        egui::Panel::bottom("status").show(ui, |ui| {
            ui.horizontal(|ui| {
                let tabs = self.dock.iter_all_tabs().count();
                ui.label(format!("{tabs} tab(s)"));
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
                };
                DockArea::new(&mut self.dock)
                    .style(style)
                    .show_leaf_collapse_buttons(false)
                    .show_leaf_close_all_buttons(false)
                    .show_inside(ui, &mut viewer);
            });

        self.modal_ui(ctx);
        if !self.actions.is_empty() {
            ctx.request_repaint();
        }
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        self.save();
    }
}
