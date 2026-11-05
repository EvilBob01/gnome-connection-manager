//! Menu bar, toolbar, quick connect, the MultiExec bar and the import menu.

use super::*;

impl App {
    pub(super) fn toolbar(&mut self, ui: &mut Ui) {
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
                let label = if self.store.has_master_password() {
                    "Master password…"
                } else {
                    "Set master password…"
                };
                if ui.button(label).clicked() {
                    self.modal = Some(if self.store.is_locked() {
                        Modal::Unlock {
                            password: String::new(),
                            error: None,
                        }
                    } else {
                        Modal::MasterPassword {
                            new: String::new(),
                            repeat: String::new(),
                            error: None,
                        }
                    });
                    ui.close();
                }
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
            ui.menu_button(RichText::new("📋  Snippets").size(14.0), |ui| {
                if self.store.snippets.is_empty() {
                    ui.label(RichText::new("No snippets yet").weak());
                }
                for sn in &self.store.snippets {
                    let target = if self.multiexec {
                        "the MultiExec group"
                    } else {
                        "the current tab"
                    };
                    if ui
                        .button(&sn.name)
                        .on_hover_text(format!("Send to {target}:\n{}", sn.text))
                        .clicked()
                    {
                        self.actions.push(Action::SendText(None, sn.keystrokes()));
                        ui.close();
                    }
                }
                ui.separator();
                if ui.button("Manage snippets…").clicked() {
                    self.modal = Some(Modal::Snippets {
                        selected: (!self.store.snippets.is_empty()).then_some(0),
                        items: self.store.snippets.clone(),
                    });
                    ui.close();
                }
            });
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
                    .id(egui::Id::new("quick_connect"))
                    .hint_text("user@host:port   mosh://host   telnet://host   rdp://host")
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
    pub(super) fn multiexec_bar(&mut self, ui: &mut Ui) {
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
    pub(super) fn mirror_input(&mut self) {
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
    pub(super) fn import_menu(&mut self, ui: &mut Ui) {
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

    pub(super) fn import_from(&mut self, source: ImportSource) {
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
}
