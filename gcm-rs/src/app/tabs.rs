//! Terminal tabs: titles, the tab menu, close handling (egui_dock TabViewer).

use super::*;

pub(super) struct Viewer<'a> {
    pub(super) settings: &'a Settings,
    pub(super) actions: &'a mut Vec<Action>,
    pub(super) modal: &'a mut Option<Modal>,
    pub(super) multiexec: bool,
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
            s.ime_enabled(),
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
