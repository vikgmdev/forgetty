//! Keybinding resolution and configuration.
//!
//! Maps key combinations to terminal or UI actions, with support for
//! user-configurable overrides from the configuration file.
//! Default bindings follow Windows Terminal conventions.

use winit::event::KeyEvent;
use winit::keyboard::{Key, ModifiersState, NamedKey};

/// Actions that can be triggered by keybindings.
#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    NewTab,
    CloseTab,
    NextTab,
    PrevTab,
    SplitDown,
    SplitRight,
    ClosePane,
    FocusUp,
    FocusDown,
    FocusLeft,
    FocusRight,
    Copy,
    Paste,
    ClearScreen,
    ScrollPageUp,
    ScrollPageDown,
    ResetScroll,
    None,
}

/// A key combination (key + modifiers).
#[derive(Debug, Clone, PartialEq)]
pub struct KeyCombo {
    pub key: Key,
    pub modifiers: ModifiersState,
}

/// Manages the set of keybindings.
pub struct KeyBindings {
    bindings: Vec<(KeyCombo, Action)>,
}

impl KeyBindings {
    /// Create the default set of keybindings (Windows Terminal style).
    pub fn default_bindings() -> Self {
        let ctrl = ModifiersState::CONTROL;
        let ctrl_shift = ModifiersState::CONTROL | ModifiersState::SHIFT;
        let alt = ModifiersState::ALT;
        let alt_shift = ModifiersState::ALT | ModifiersState::SHIFT;
        let shift = ModifiersState::SHIFT;

        let bindings = vec![
            // Tab management
            (KeyCombo { key: Key::Character("t".into()), modifiers: ctrl_shift }, Action::NewTab),
            (KeyCombo { key: Key::Character("T".into()), modifiers: ctrl_shift }, Action::NewTab),
            (KeyCombo { key: Key::Named(NamedKey::Tab), modifiers: ctrl }, Action::NextTab),
            (KeyCombo { key: Key::Named(NamedKey::Tab), modifiers: ctrl_shift }, Action::PrevTab),
            // Pane splitting — Alt+Shift+Minus = split down, Alt+Shift+Equal = split right
            (KeyCombo { key: Key::Character("-".into()), modifiers: alt_shift }, Action::SplitDown),
            (KeyCombo { key: Key::Character("_".into()), modifiers: alt_shift }, Action::SplitDown),
            (
                KeyCombo { key: Key::Character("=".into()), modifiers: alt_shift },
                Action::SplitRight,
            ),
            (
                KeyCombo { key: Key::Character("+".into()), modifiers: alt_shift },
                Action::SplitRight,
            ),
            // Close pane — Ctrl+Shift+W
            (
                KeyCombo { key: Key::Character("w".into()), modifiers: ctrl_shift },
                Action::ClosePane,
            ),
            (
                KeyCombo { key: Key::Character("W".into()), modifiers: ctrl_shift },
                Action::ClosePane,
            ),
            // Focus navigation — Alt+Arrow
            (KeyCombo { key: Key::Named(NamedKey::ArrowUp), modifiers: alt }, Action::FocusUp),
            (KeyCombo { key: Key::Named(NamedKey::ArrowDown), modifiers: alt }, Action::FocusDown),
            (KeyCombo { key: Key::Named(NamedKey::ArrowLeft), modifiers: alt }, Action::FocusLeft),
            (
                KeyCombo { key: Key::Named(NamedKey::ArrowRight), modifiers: alt },
                Action::FocusRight,
            ),
            // Clipboard — Ctrl+C (copy when selection exists, otherwise pass through)
            // Ctrl+V paste
            (KeyCombo { key: Key::Character("c".into()), modifiers: ctrl_shift }, Action::Copy),
            (KeyCombo { key: Key::Character("C".into()), modifiers: ctrl_shift }, Action::Copy),
            (KeyCombo { key: Key::Character("v".into()), modifiers: ctrl_shift }, Action::Paste),
            (KeyCombo { key: Key::Character("V".into()), modifiers: ctrl_shift }, Action::Paste),
            // Scrolling
            (
                KeyCombo { key: Key::Named(NamedKey::PageUp), modifiers: shift },
                Action::ScrollPageUp,
            ),
            (
                KeyCombo { key: Key::Named(NamedKey::PageDown), modifiers: shift },
                Action::ScrollPageDown,
            ),
        ];

        Self { bindings }
    }

    /// Check if a key event matches a binding, return the action.
    pub fn match_key(&self, event: &KeyEvent, modifiers: ModifiersState) -> Action {
        for (combo, action) in &self.bindings {
            if combo.modifiers == modifiers && combo.key == event.logical_key {
                return action.clone();
            }
        }
        Action::None
    }

    /// Get a list of shortcut descriptions for display in the menu bar.
    pub fn shortcut_descriptions() -> Vec<(&'static str, &'static str)> {
        vec![
            ("Alt+Shift+-", "Split \u{2193}"),
            ("Alt+Shift+=", "Split \u{2192}"),
            ("Alt+\u{2190}\u{2191}\u{2192}\u{2193}", "Navigate"),
            ("Ctrl+Shift+W", "Close"),
            ("Ctrl+Shift+T", "New Tab"),
            ("Ctrl+Tab", "Next Tab"),
            ("Ctrl+Shift+C", "Copy"),
            ("Ctrl+Shift+V", "Paste"),
        ]
    }
}
