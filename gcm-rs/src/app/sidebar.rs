//! The left sidebar (session tree, SFTP pane, tunnels) and the start screen.

use super::*;

impl App {
    pub(super) fn sidebar(&mut self, ui: &mut Ui) {
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

    pub(super) fn start_tunnel(&mut self, t: &Tunnel) {
        if let Err(e) = self
            .tunnels
            .start(t, &self.store.sessions, &self.store.settings)
        {
            self.error(format!("Tunnel {}: {e:#}", t.display_name()));
        }
    }

    /// Saved port forwards, each started and stopped on its own (MobaSSHTunnel).
    pub(super) fn tunnels_ui(&mut self, ui: &mut Ui) {
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
    pub(super) fn sftp_ui(&mut self, ui: &mut Ui) {
        let active = self
            .dock
            .find_active_focused()
            .map(|(_, tab)| (tab.id, tab.session.clone()));
        // Mosh hosts are reached over SSH too, so they can be browsed the same way.
        let Some((id, session)) =
            active.filter(|(_, s)| matches!(s.protocol, Protocol::Ssh | Protocol::Mosh))
        else {
            ui.label(RichText::new("Select an SSH tab to browse its files.").weak());
            return;
        };
        ui.label(
            RichText::new(format!(
                "{} {}",
                session.protocol.icon(),
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

    pub(super) fn sessions_ui(&mut self, ui: &mut Ui) {
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
    pub(super) fn folder_ui(
        &mut self,
        ui: &mut Ui,
        folder: &str,
        sessions: &[Session],
        expand_all: bool,
    ) {
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

    pub(super) fn welcome(&mut self, ui: &mut Ui) {
        ui.vertical_centered(|ui| {
            ui.add_space(ui.available_height() * 0.2);
            ui.heading(RichText::new("GNOME Connection Manager").size(28.0));
            ui.label(
                RichText::new("SSH · Mosh · Telnet · Serial · RDP · VNC · Local shell").weak(),
            );
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
}
