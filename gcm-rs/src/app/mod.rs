//! Main window: session tree on the left, tabbed and splittable terminals
//! in the middle, MobaXterm-style toolbar with quick connect on top.

use eframe::egui::{self, Color32, Key, RichText, Ui, WidgetText};
use egui_dock::tab_viewer::OnCloseResponse;
use egui_dock::{DockArea, DockState, Node, NodePath, Split, Style, TabViewer};

use serde::{Deserialize, Serialize};

use crate::config::{FlowControl, Parity, Protocol, Session, Settings, Snippet, Store, new_id};
use crate::gcm_import;
use crate::launch::{self, Launch};
use crate::sftp::Browser;
use crate::shortcuts::Command;
use crate::terminal::Terminal;
use crate::tunnel::{Status as TunnelStatus, Tunnel, TunnelKind, TunnelManager};

pub struct Tab {
    id: u64,
    session: Session,
    term: Terminal,
    /// Receives input typed in other tabs while multi-exec is on.
    in_group: bool,
    /// Title set with "Rename tab", replacing the session name.
    custom_title: Option<String>,
}

/// A tab as remembered between runs (Settings > Reopen tabs).
#[derive(Serialize, Deserialize)]
struct SavedTab {
    session: Session,
    title: Option<String>,
    in_group: bool,
}

/// Give a text field keyboard focus on the first frame it is shown (for
/// example when its dialog opens), and leave focus alone afterwards:
/// requesting it every frame restarts the input method and loses keys.
pub(crate) fn focus_on_open(ui: &Ui, field: &egui::Response) {
    let key = field.id.with("focus_on_open");
    let pass = ui.ctx().cumulative_pass_nr();
    let shown_last_pass = ui.ctx().data_mut(|d| {
        let prev: Option<u64> = d.get_temp(key);
        d.insert_temp(key, pass);
        prev.is_some_and(|p| p + 1 >= pass)
    });
    if !shown_last_pass {
        field.request_focus();
    }
}

fn layout_path() -> std::path::PathBuf {
    crate::config::config_dir().join("layout.json")
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
    /// Type text into a tab (None: the focused one), e.g. a snippet.
    SendText(Option<u64>, String),
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
    RenameTab {
        id: u64,
        title: String,
    },
    Snippets {
        items: Vec<Snippet>,
        selected: Option<usize>,
    },
    /// Enter the master password to use saved passwords.
    Unlock {
        password: String,
        error: Option<String>,
    },
    MasterPassword {
        new: String,
        repeat: String,
        error: Option<String>,
    },
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
    /// The tab whose terminal last had keyboard focus; egui_dock's own
    /// notion of the focused pane can be unset (e.g. tabs added to an empty layout).
    last_focused_tab: Option<u64>,
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
            last_focused_tab: None,
            status: None,
        };
        // With a master password, tunnels wait for the unlock dialog so
        // they start with their saved passwords.
        if !app.store.is_locked() {
            app.start_autostart_tunnels();
        }
        if app.store.is_locked() {
            app.modal = Some(Modal::Unlock {
                password: String::new(),
                error: None,
            });
        } else if app.store.settings.restore_tabs {
            app.restore_layout(&cc.egui_ctx);
        }
        if app.store.settings.open_local_on_startup {
            app.actions
                .push(Action::Open(Session::new(Protocol::Local)));
        }
        app
    }

    pub(crate) fn start_autostart_tunnels(&mut self) {
        for t in self
            .store
            .tunnels
            .clone()
            .into_iter()
            .filter(|t| t.autostart)
        {
            if let Err(e) = self
                .tunnels
                .start(&t, &self.store.sessions, &self.store.settings)
            {
                self.error(format!("Tunnel {}: {e:#}", t.display_name()));
            }
        }
    }

    /// Reopen the tabs saved at the last exit, in the same split layout.
    fn restore_layout(&mut self, ctx: &egui::Context) {
        let Ok(text) = std::fs::read_to_string(layout_path()) else {
            return;
        };
        let saved: DockState<SavedTab> = match serde_json::from_str(&text) {
            Ok(s) => s,
            Err(e) => {
                log::warn!("ignoring {}: {e}", layout_path().display());
                return;
            }
        };
        let restored = saved.filter_map_tabs(|st| {
            // Saved sessions come from the store, so they get their password back.
            let session = self
                .store
                .find(st.session.id)
                .cloned()
                .unwrap_or_else(|| st.session.clone());
            let mut tab = self.connect(ctx, session)?;
            tab.custom_title = st.title.clone();
            tab.in_group = st.in_group;
            Some(tab)
        });
        if restored.iter_all_tabs().next().is_some() {
            self.dock = restored;
        }
    }

    /// Remember open tabs for next time, or forget them if the setting is off.
    fn save_layout(&mut self) {
        let path = layout_path();
        if !self.store.settings.restore_tabs {
            let _ = std::fs::remove_file(&path);
            return;
        }
        let saved = self.dock.filter_map_tabs(|t| {
            (!t.term.has_exited()).then(|| SavedTab {
                session: t.session.without_secrets(),
                title: t.custom_title.clone(),
                in_group: t.in_group,
            })
        });
        match serde_json::to_string_pretty(&saved) {
            Ok(json) => {
                if let Err(e) = std::fs::write(&path, json) {
                    log::warn!("cannot save {}: {e}", path.display());
                }
            }
            Err(e) => log::warn!("cannot save tab layout: {e}"),
        }
    }

    fn focused_tab_id(&mut self) -> Option<u64> {
        let live = |id: u64| self.dock.iter_all_tabs().any(|(_, t)| t.id == id);
        match self.last_focused_tab {
            Some(id) if live(id) => Some(id),
            _ => self.dock.find_active_focused().map(|(_, t)| t.id),
        }
    }

    /// Activate the next (or previous) tab, across all panes.
    fn cycle_tab(&mut self, forward: bool) {
        let tabs: Vec<(egui_dock::TabPath, u64)> =
            self.dock.iter_all_tabs().map(|(p, t)| (p, t.id)).collect();
        if tabs.is_empty() {
            return;
        }
        let current = self.focused_tab_id();
        let pos = tabs
            .iter()
            .position(|(_, id)| Some(*id) == current)
            .unwrap_or(0);
        let next = if forward {
            (pos + 1) % tabs.len()
        } else {
            (pos + tabs.len() - 1) % tabs.len()
        };
        let (path, id) = tabs[next];
        self.dock.set_focused_node_and_surface(NodePath {
            surface: path.surface,
            node: path.node,
        });
        let _ = self.dock.set_active_tab(path);
        if let Some((_, tab)) = self.dock.iter_all_tabs_mut().find(|(_, t)| t.id == id) {
            tab.term.request_focus();
        }
    }

    /// Run a command from a keyboard shortcut.
    fn command(&mut self, ctx: &egui::Context, cmd: Command) {
        let focused = self.focused_tab_id();
        let focused_tab = |dock: &DockState<Tab>| {
            focused.and_then(|id| {
                dock.iter_all_tabs()
                    .find(|(_, t)| t.id == id)
                    .map(|(_, t)| t.session.clone())
            })
        };
        match cmd {
            Command::NewSession => {
                self.modal = Some(Modal::Edit {
                    session: Session::new(Protocol::Ssh),
                    is_new: true,
                    save: true,
                })
            }
            Command::LocalShell => self
                .actions
                .push(Action::Open(Session::new(Protocol::Local))),
            Command::QuickConnect => {
                ctx.memory_mut(|m| m.request_focus(egui::Id::new("quick_connect")))
            }
            Command::CloseTab => {
                if let Some(id) = focused {
                    let running = self
                        .dock
                        .iter_all_tabs()
                        .any(|(_, t)| t.id == id && !t.term.has_exited());
                    if running && self.store.settings.confirm_close_tab {
                        self.modal = Some(Modal::ConfirmClose(id));
                    } else {
                        self.actions.push(Action::Close(id));
                    }
                }
            }
            Command::NextTab => self.cycle_tab(true),
            Command::PrevTab => self.cycle_tab(false),
            Command::DuplicateTab => {
                if let Some(s) = focused_tab(&self.dock) {
                    self.actions.push(Action::Open(s));
                }
            }
            Command::RenameTab => {
                if let Some(id) = focused {
                    let title = self
                        .dock
                        .iter_all_tabs()
                        .find(|(_, t)| t.id == id)
                        .map(|(_, t)| {
                            t.custom_title
                                .clone()
                                .unwrap_or_else(|| t.session.display_name())
                        })
                        .unwrap_or_default();
                    self.modal = Some(Modal::RenameTab { id, title });
                }
            }
            Command::ToggleSidebar => self.show_sidebar = !self.show_sidebar,
            Command::ToggleMultiExec => self.multiexec = !self.multiexec,
            Command::Snippets => {
                self.modal = Some(Modal::Snippets {
                    selected: (!self.store.snippets.is_empty()).then_some(0),
                    items: self.store.snippets.clone(),
                })
            }
        }
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
        // Windows has no X server or DISPLAY of its own; provide both for -X.
        let mut env = Vec::new();
        if cfg!(windows) && session.x11 && session.protocol == Protocol::Ssh {
            match crate::x11::ensure(&self.store.settings) {
                Ok(display) => env.push(("DISPLAY".to_string(), display)),
                Err(e) => self.error(format!("X11 forwarding: {e:#}")),
            }
        }
        let settings = &self.store.settings;
        let result = launch::build(&session, settings).and_then(|l| match l {
            Launch::Pty {
                program,
                args,
                autofill,
            } => Terminal::spawn_pty(ctx, &program, &args, &env, autofill, settings.scrollback)
                .map(Some),
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
                    custom_title: None,
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
                Action::SendText(target, text) => {
                    let id = target.or_else(|| self.focused_tab_id());
                    let tab = self
                        .dock
                        .iter_all_tabs_mut()
                        .find(|(_, t)| Some(t.id) == id);
                    let sent = tab.map(|(_, tab)| {
                        tab.term.send_text(&text);
                        tab.term.request_focus();
                    });
                    if sent.is_none() {
                        self.error("Open a terminal tab first");
                    }
                }
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
        // Shortcuts go first so the terminal never sees their keys.
        if self.modal.is_none() {
            for (cmd, sc) in crate::shortcuts::bindings(&self.store.settings.shortcuts) {
                if ctx.input_mut(|i| i.consume_shortcut(&sc)) {
                    self.command(ctx, cmd);
                }
            }
        }
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
                if self.store.is_locked()
                    && ui
                        .link("🔒 Saved passwords locked")
                        .on_hover_text("Enter the master password to use saved passwords")
                        .clicked()
                {
                    self.modal = Some(Modal::Unlock {
                        password: String::new(),
                        error: None,
                    });
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
                    snippets: &self.store.snippets,
                    focused_tab: &mut self.last_focused_tab,
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
        self.save_layout();
        self.save();
    }
}
