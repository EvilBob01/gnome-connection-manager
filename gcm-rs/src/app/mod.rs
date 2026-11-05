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
            Launch::Telnet {
                host,
                port,
                autofill,
            } => Ok(Some(Terminal::open_telnet(
                ctx,
                &host,
                port,
                autofill,
                settings.scrollback,
            ))),
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
                let mut msg = format!("{}: {e:#}", session.display_name());
                let missing = msg.contains("No such file")
                    || msg.contains("cannot find")
                    || msg.contains("not found");
                if session.protocol == Protocol::Mosh && missing {
                    msg.push_str(if cfg!(windows) {
                        " (Windows has no native mosh; install it in WSL or Cygwin and set Settings > Mosh command)"
                    } else {
                        " (install the mosh package, e.g. apt/dnf install mosh)"
                    });
                }
                self.error(msg);
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
}

mod dialogs;
mod sidebar;
mod tabs;
mod toolbar;

use tabs::Viewer;

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
