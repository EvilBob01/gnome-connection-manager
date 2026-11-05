//! Modal dialogs and the forms inside them.

use super::*;

impl App {
    pub(super) fn modal_ui(&mut self, ctx: &egui::Context) {
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

            if s.protocol == Protocol::Mosh {
                ui.label("Private key");
                ui.add(
                    egui::TextEdit::singleline(&mut s.private_key).hint_text("~/.ssh/id_ed25519"),
                );
                ui.end_row();
                ui.label("Mosh options");
                ui.add(
                    egui::TextEdit::singleline(&mut s.extra_args)
                        .hint_text("e.g. --predict=always --server=/usr/local/bin/mosh-server"),
                );
                ui.end_row();
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
                let r = ui.text_edit_singleline(&mut s.extra_args);
                if s.protocol == Protocol::Telnet {
                    r.on_hover_text("Only used with the system telnet program (Settings)");
                }
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
                ui.horizontal(|ui| {
                    use crate::config::ImeMode;
                    ui.label("Input method (IME) in terminals:");
                    for (mode, label) in [(ImeMode::Auto, "Auto"), (ImeMode::On, "On"), (ImeMode::Off, "Off")] {
                        ui.radio_value(&mut s.ime, mode, label);
                    }
                })
                .response
                .on_hover_text(
                    "For typing Chinese, Japanese, Korean and other composed text. Auto turns it on \
                     on Windows, macOS and Wayland, and on X11 when an input method such as IBus or \
                     Fcitx is configured.",
                );
            });
            ui.end_row();
            ui.label("SSH command");
            ui.text_edit_singleline(&mut s.ssh_command);
            ui.end_row();
            ui.label("Telnet");
            ui.vertical(|ui| {
                ui.checkbox(
                    &mut s.use_system_telnet,
                    "Use the system telnet program instead of the built-in client",
                );
                ui.add_enabled(
                    s.use_system_telnet,
                    egui::TextEdit::singleline(&mut s.telnet_command),
                );
            });
            ui.end_row();
            ui.label("Mosh command");
            ui.text_edit_singleline(&mut s.mosh_command);
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
