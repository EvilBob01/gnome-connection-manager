//! Configurable keyboard shortcuts for app commands.
//!
//! Defaults use Ctrl+Shift (or Ctrl+Tab) so that plain Ctrl+letter keeps
//! reaching the terminal. Settings store only the bindings the user changed,
//! as text such as "Ctrl+Shift+T"; "none" disables a command's shortcut.

use std::collections::BTreeMap;

use eframe::egui::{Key, KeyboardShortcut, Modifiers};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Command {
    NewSession,
    LocalShell,
    QuickConnect,
    CloseTab,
    NextTab,
    PrevTab,
    DuplicateTab,
    RenameTab,
    ToggleSidebar,
    ToggleMultiExec,
    Snippets,
}

impl Command {
    pub const ALL: [Command; 11] = [
        Command::NewSession,
        Command::LocalShell,
        Command::QuickConnect,
        Command::CloseTab,
        Command::NextTab,
        Command::PrevTab,
        Command::DuplicateTab,
        Command::RenameTab,
        Command::ToggleSidebar,
        Command::ToggleMultiExec,
        Command::Snippets,
    ];

    /// Key in the settings file.
    pub fn id(self) -> &'static str {
        match self {
            Command::NewSession => "new_session",
            Command::LocalShell => "local_shell",
            Command::QuickConnect => "quick_connect",
            Command::CloseTab => "close_tab",
            Command::NextTab => "next_tab",
            Command::PrevTab => "prev_tab",
            Command::DuplicateTab => "duplicate_tab",
            Command::RenameTab => "rename_tab",
            Command::ToggleSidebar => "toggle_sidebar",
            Command::ToggleMultiExec => "toggle_multiexec",
            Command::Snippets => "snippets",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Command::NewSession => "New session",
            Command::LocalShell => "Local shell",
            Command::QuickConnect => "Go to quick connect",
            Command::CloseTab => "Close tab",
            Command::NextTab => "Next tab",
            Command::PrevTab => "Previous tab",
            Command::DuplicateTab => "Duplicate tab",
            Command::RenameTab => "Rename tab",
            Command::ToggleSidebar => "Show/hide sidebar",
            Command::ToggleMultiExec => "MultiExec on/off",
            Command::Snippets => "Manage snippets",
        }
    }

    pub fn default_shortcut(self) -> &'static str {
        match self {
            Command::NewSession => "Ctrl+Shift+N",
            Command::LocalShell => "Ctrl+Shift+T",
            Command::QuickConnect => "Ctrl+Shift+K",
            Command::CloseTab => "Ctrl+Shift+W",
            Command::NextTab => "Ctrl+Tab",
            Command::PrevTab => "Ctrl+Shift+Tab",
            Command::DuplicateTab => "Ctrl+Shift+D",
            Command::RenameTab => "Ctrl+Shift+R",
            Command::ToggleSidebar => "Ctrl+Shift+B",
            Command::ToggleMultiExec => "Ctrl+Shift+M",
            Command::Snippets => "Ctrl+Shift+S",
        }
    }
}

/// Parse "Ctrl+Shift+T", "Alt+F2", "Ctrl+PageDown" (case-insensitive).
pub fn parse(text: &str) -> Option<KeyboardShortcut> {
    let mut mods = Modifiers::NONE;
    let mut key = None;
    for part in text.split('+').map(str::trim) {
        match part.to_ascii_lowercase().as_str() {
            "ctrl" | "control" => mods.ctrl = true,
            "shift" => mods.shift = true,
            "alt" | "option" => mods.alt = true,
            "cmd" | "command" | "super" | "win" => mods.mac_cmd = true,
            "" => return None,
            name => {
                if key.is_some() {
                    return None;
                }
                key = Some(
                    Key::ALL
                        .iter()
                        .copied()
                        .find(|k| k.name().eq_ignore_ascii_case(name))?,
                );
            }
        }
    }
    // egui treats "command" as Ctrl outside macOS; match on it so Ctrl works everywhere.
    if mods.ctrl {
        mods.command = true;
    }
    Some(KeyboardShortcut::new(mods, key?))
}

/// The active bindings: defaults overridden by `overrides`, most specific
/// first, since egui also matches a shortcut when extra Shift is held.
pub fn bindings(overrides: &BTreeMap<String, String>) -> Vec<(Command, KeyboardShortcut)> {
    let mut out: Vec<(Command, KeyboardShortcut)> = Command::ALL
        .iter()
        .filter_map(|&c| {
            let text = overrides
                .get(c.id())
                .map(String::as_str)
                .unwrap_or(c.default_shortcut());
            parse(text).map(|sc| (c, sc))
        })
        .collect();
    out.sort_by_key(|(_, sc)| {
        let m = sc.modifiers;
        std::cmp::Reverse(
            u8::from(m.ctrl) + u8::from(m.shift) + u8::from(m.alt) + u8::from(m.mac_cmd),
        )
    });
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses() {
        let sc = parse("Ctrl+Shift+T").unwrap();
        assert_eq!(sc.logical_key, Key::T);
        assert!(
            sc.modifiers.ctrl && sc.modifiers.shift && sc.modifiers.command && !sc.modifiers.alt
        );
        assert_eq!(parse("ctrl+pagedown").unwrap().logical_key, Key::PageDown);
        assert_eq!(parse("Alt+F2").unwrap().logical_key, Key::F2);
        assert!(parse("Ctrl+").is_none());
        assert!(parse("Ctrl+Nope").is_none());
        assert!(parse("none").is_none());
        assert!(parse("A+B").is_none());
    }

    #[test]
    fn defaults_parse_and_overrides_apply() {
        for c in Command::ALL {
            assert!(parse(c.default_shortcut()).is_some(), "{c:?}");
        }
        let mut o = BTreeMap::new();
        o.insert("local_shell".to_string(), "Alt+F1".to_string());
        o.insert("close_tab".to_string(), "none".to_string());
        let b = bindings(&o);
        assert_eq!(b.len(), Command::ALL.len() - 1);
        let shell = b.iter().find(|(c, _)| *c == Command::LocalShell).unwrap().1;
        assert_eq!(shell.logical_key, Key::F1);
        // Ctrl+Shift+Tab must be tried before Ctrl+Tab.
        let pos = |c| b.iter().position(|(x, _)| *x == c).unwrap();
        assert!(pos(Command::PrevTab) < pos(Command::NextTab));
    }
}
