use std::collections::HashMap;

use anyhow::{Context, Result};
use crossterm::event::{KeyCode, KeyModifiers};
use serde::{Deserialize, Serialize};

/// Actions that keyboard events can trigger.
#[derive(Debug, Clone, Copy, Hash, PartialEq, Eq)]
pub enum Action {
    SendMessage,
    InsertNewline,
    Quit,
    CursorLeft,
    CursorRight,
    CursorHome,
    CursorEnd,
    KillLine,
    DeleteBefore,
    DeleteAfter,
    HistoryUp,
    HistoryDown,
    ScrollUp,
    ScrollDown,
    ScrollToBottom,
    AgentCommand,
    /// Accept suggestion / complete (Tab).
    Complete,
    /// Switch between primary agent and subagent view.
    SwitchView,
    /// Cancel the running agent (Escape while streaming).
    CancelAgent,
}

/// Resolved keybinding map.
#[derive(Debug, Clone)]
pub struct Keybindings {
    map: HashMap<(KeyCode, KeyModifiers), Action>,
}

/// A single action→keys mapping from TOML.
/// Accepts either `"key"` or `["key1", "key2"]`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum KeyList {
    Single(String),
    Multiple(Vec<String>),
}

impl KeyList {
    fn into_vec(self) -> Vec<String> {
        match self {
            KeyList::Single(s) => vec![s],
            KeyList::Multiple(v) => v,
        }
    }
}

pub type RawBindings = HashMap<String, KeyList>;

impl Keybindings {
    /// Build from optional user overrides, filling missing actions with defaults.
    pub fn from_config(raw: Option<&RawBindings>) -> Self {
        let mut map = HashMap::new();
        let mut insert = |action: Action, keys: &[String]| {
            for key_str in keys {
                if let Ok((code, mods)) = parse_key_def(key_str) {
                    map.insert((code, mods), action);
                }
            }
        };

        // Start with defaults
        let defaults = default_bindings();
        for (action, keys) in &defaults {
            insert(*action, keys);
        }

        // Apply user overrides
        if let Some(raw) = raw {
            for (name, keylist) in raw {
                if let Some(action) = action_from_name(name) {
                    // Remove all existing default bindings for this action
                    map.retain(|_, v| *v != action);
                    // Add the user's bindings
                    for key_str in keylist.clone().into_vec() {
                        if let Ok((code, mods)) = parse_key_def(&key_str) {
                            map.insert((code, mods), action);
                        }
                    }
                }
            }
        }

        Self { map }
    }

    /// Look up the action for a key event.
    pub fn lookup(
        &self,
        code: KeyCode,
        modifiers: KeyModifiers,
    ) -> Option<Action> {
        if let Some(&action) = self.map.get(&(code, modifiers)) {
            return Some(action);
        }
        // Fall back to code-only match only for SHIFT (e.g. Shift+A → 'a').
        // Do NOT fall back for Ctrl/Alt/Super — those are distinct key combos.
        if modifiers == KeyModifiers::SHIFT {
            return self.map.get(&(code, KeyModifiers::NONE)).copied();
        }
        None
    }
}

fn action_from_name(name: &str) -> Option<Action> {
    match name {
        "send_message" => Some(Action::SendMessage),
        "insert_newline" => Some(Action::InsertNewline),
        "quit" => Some(Action::Quit),
        "cursor_left" => Some(Action::CursorLeft),
        "cursor_right" => Some(Action::CursorRight),
        "cursor_home" => Some(Action::CursorHome),
        "cursor_end" => Some(Action::CursorEnd),
        "kill_line" => Some(Action::KillLine),
        "delete_before" => Some(Action::DeleteBefore),
        "delete_after" => Some(Action::DeleteAfter),
        "history_up" => Some(Action::HistoryUp),
        "history_down" => Some(Action::HistoryDown),
        "scroll_up" => Some(Action::ScrollUp),
        "scroll_down" => Some(Action::ScrollDown),
        "scroll_to_bottom" => Some(Action::ScrollToBottom),
        "agent_command" => Some(Action::AgentCommand),
        "complete" => Some(Action::Complete),
        "switch_view" => Some(Action::SwitchView),
        "cancel_agent" => Some(Action::CancelAgent),
        _ => None,
    }
}

fn default_bindings() -> Vec<(Action, Vec<String>)> {
    vec![
        (Action::SendMessage, vec!["enter".into()]),
        (Action::InsertNewline, vec!["alt+enter".into()]),
        (Action::Quit, vec!["ctrl+c".into()]),
        (Action::CursorLeft, vec!["left".into()]),
        (Action::CursorRight, vec!["right".into()]),
        (Action::CursorHome, vec!["home".into(), "ctrl+a".into()]),
        (Action::CursorEnd, vec!["end".into(), "ctrl+e".into()]),
        (Action::KillLine, vec!["ctrl+k".into()]),
        (Action::DeleteBefore, vec!["backspace".into()]),
        (Action::DeleteAfter, vec!["delete".into(), "ctrl+d".into()]),
        (Action::HistoryUp, vec!["up".into()]),
        (Action::HistoryDown, vec!["down".into()]),
        (Action::ScrollUp, vec!["pageup".into(), "ctrl+up".into()]),
        (
            Action::ScrollDown,
            vec!["pagedown".into(), "ctrl+down".into()],
        ),
        (Action::ScrollToBottom, vec!["ctrl+end".into()]),
        (Action::AgentCommand, vec!["ctrl+p".into()]),
        (Action::Complete, vec!["tab".into()]),
        (Action::SwitchView, vec!["f5".into()]),
        (Action::CancelAgent, vec!["esc".into()]),
    ]
}

// ── Parser ────────────────────────────────────────────────

fn parse_key_def(s: &str) -> Result<(KeyCode, KeyModifiers)> {
    let parts: Vec<&str> = s.split('+').collect();
    let mut mods = KeyModifiers::NONE;
    let mut key_part = None;

    for part in &parts {
        match part.to_lowercase().as_str() {
            "ctrl" | "control" => mods |= KeyModifiers::CONTROL,
            "alt" | "option" => mods |= KeyModifiers::ALT,
            "shift" => mods |= KeyModifiers::SHIFT,
            "super" | "cmd" | "command" => mods |= KeyModifiers::SUPER,
            _ => key_part = Some(*part),
        }
    }

    let key = key_part.context("Missing key in binding")?;
    let code = match key.to_lowercase().as_str() {
        "enter" => KeyCode::Enter,
        "esc" | "escape" => KeyCode::Esc,
        "backspace" | "bs" => KeyCode::Backspace,
        "delete" | "del" => KeyCode::Delete,
        "left" => KeyCode::Left,
        "right" => KeyCode::Right,
        "up" => KeyCode::Up,
        "down" => KeyCode::Down,
        "home" => KeyCode::Home,
        "end" => KeyCode::End,
        "pageup" | "pgup" => KeyCode::PageUp,
        "pagedown" | "pgdn" => KeyCode::PageDown,
        "space" => KeyCode::Char(' '),
        "tab" => KeyCode::Tab,
        _ if key.len() > 1 && key.starts_with('f') => {
            if let Ok(n) = key[1..].parse::<u8>() {
                if (1..=12).contains(&n) {
                    KeyCode::F(n)
                } else {
                    anyhow::bail!("Unknown function key: '{}'", key);
                }
            } else {
                let chars: Vec<char> = key.chars().collect();
                if chars.len() == 1 {
                    KeyCode::Char(chars[0].to_ascii_lowercase())
                } else {
                    anyhow::bail!("Unknown key: '{}'", key);
                }
            }
        }
        _ => {
            let chars: Vec<char> = key.chars().collect();
            if chars.len() == 1 {
                KeyCode::Char(chars[0].to_ascii_lowercase())
            } else {
                anyhow::bail!("Unknown key: '{}'", key);
            }
        }
    };

    Ok((code, mods))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_simple() {
        let (code, mods) = parse_key_def("enter").unwrap();
        assert_eq!(code, KeyCode::Enter);
        assert_eq!(mods, KeyModifiers::NONE);
    }

    #[test]
    fn test_parse_with_modifier() {
        let (code, mods) = parse_key_def("ctrl+c").unwrap();
        assert_eq!(code, KeyCode::Char('c'));
        assert_eq!(mods, KeyModifiers::CONTROL);
    }

    #[test]
    fn test_parse_alt_enter() {
        let (code, mods) = parse_key_def("alt+enter").unwrap();
        assert_eq!(code, KeyCode::Enter);
        assert_eq!(mods, KeyModifiers::ALT);
    }

    #[test]
    fn test_parse_case_insensitive() {
        let (code, mods) = parse_key_def("Ctrl+C").unwrap();
        assert_eq!(code, KeyCode::Char('c'));
        assert_eq!(mods, KeyModifiers::CONTROL);
    }

    #[test]
    fn test_parse_named_keys() {
        assert_eq!(parse_key_def("esc").unwrap().0, KeyCode::Esc);
        assert_eq!(parse_key_def("pageup").unwrap().0, KeyCode::PageUp);
        assert_eq!(parse_key_def("backspace").unwrap().0, KeyCode::Backspace);
        assert_eq!(parse_key_def("space").unwrap().0, KeyCode::Char(' '));
    }

    #[test]
    fn test_parse_unknown_key_fails() {
        assert!(parse_key_def("foobar").is_err());
    }

    #[test]
    fn test_default_keybindings() {
        let kb = Keybindings::from_config(None);
        assert_eq!(
            kb.lookup(KeyCode::Enter, KeyModifiers::NONE),
            Some(Action::SendMessage)
        );
        assert_eq!(
            kb.lookup(KeyCode::Enter, KeyModifiers::ALT),
            Some(Action::InsertNewline)
        );
        assert_eq!(
            kb.lookup(KeyCode::Esc, KeyModifiers::NONE),
            Some(Action::CancelAgent)
        );
        assert_eq!(
            kb.lookup(KeyCode::Char('c'), KeyModifiers::CONTROL),
            Some(Action::Quit)
        );
        assert_eq!(
            kb.lookup(KeyCode::Char('a'), KeyModifiers::CONTROL),
            Some(Action::CursorHome)
        );
        assert_eq!(
            kb.lookup(KeyCode::Char('e'), KeyModifiers::CONTROL),
            Some(Action::CursorEnd)
        );
        assert_eq!(
            kb.lookup(KeyCode::Char('d'), KeyModifiers::CONTROL),
            Some(Action::DeleteAfter)
        );
        assert_eq!(
            kb.lookup(KeyCode::Char('k'), KeyModifiers::CONTROL),
            Some(Action::KillLine)
        );
    }

    #[test]
    fn test_lookup_fallback_shift_only() {
        let kb = Keybindings::from_config(None);
        // Ctrl+End is explicitly bound to ScrollToBottom
        assert_eq!(
            kb.lookup(KeyCode::End, KeyModifiers::CONTROL),
            Some(Action::ScrollToBottom)
        );
        // Shift+End should fall back to End (CursorEnd)
        assert_eq!(
            kb.lookup(KeyCode::End, KeyModifiers::SHIFT),
            Some(Action::CursorEnd)
        );
        // Alt+End should NOT fall back (no explicit binding)
        assert_eq!(kb.lookup(KeyCode::End, KeyModifiers::ALT), None);
    }

    #[test]
    fn test_empty_config_falls_back_to_defaults() {
        let raw: Option<RawBindings> = None;
        let kb = Keybindings::from_config(raw.as_ref());
        assert_eq!(
            kb.lookup(KeyCode::Enter, KeyModifiers::NONE),
            Some(Action::SendMessage)
        );
    }

    #[test]
    fn test_user_override() {
        let mut raw = RawBindings::new();
        raw.insert("send_message".into(), KeyList::Single("ctrl+m".into()));
        let kb = Keybindings::from_config(Some(&raw));
        // Custom binding works
        assert_eq!(
            kb.lookup(KeyCode::Char('m'), KeyModifiers::CONTROL),
            Some(Action::SendMessage)
        );
        // Default binding no longer present (replaced by override)
        assert_eq!(kb.lookup(KeyCode::Enter, KeyModifiers::NONE), None);
    }

    #[test]
    fn test_unknown_action_name_is_ignored() {
        let mut raw = RawBindings::new();
        raw.insert("nonexistent".into(), KeyList::Single("x".into()));
        // Should not panic — just ignore
        let kb = Keybindings::from_config(Some(&raw));
        assert_eq!(kb.lookup(KeyCode::Char('x'), KeyModifiers::NONE), None);
    }
}
