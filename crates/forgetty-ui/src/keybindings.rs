//! Keybinding resolution and configuration.
//!
//! Maps key combinations to terminal or UI actions, with support for
//! user-configurable overrides from the configuration file.

use winit::event::KeyEvent;
use winit::keyboard::{Key, ModifiersState, NamedKey};

/// Actions that can be triggered by keybindings.
#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    NewTab,
    CloseTab,
    NextTab,
    PrevTab,
    SplitHorizontal,
    SplitVertical,
    ClosePane,
    FocusNext,
    FocusUp,
    FocusDown,
    FocusLeft,
    FocusRight,
    Copy,
    Paste,
    ScrollUp,
    ScrollDown,
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
    /// Create the default set of keybindings.
    pub fn default_bindings() -> Self {
        let ctrl_shift = ModifiersState::CONTROL | ModifiersState::SHIFT;
        let shift = ModifiersState::SHIFT;

        let bindings = vec![
            // Tab management
            (KeyCombo { key: Key::Character("T".into()), modifiers: ctrl_shift }, Action::NewTab),
            (KeyCombo { key: Key::Character("W".into()), modifiers: ctrl_shift }, Action::CloseTab),
            (
                KeyCombo { key: Key::Named(NamedKey::Tab), modifiers: ModifiersState::CONTROL },
                Action::NextTab,
            ),
            (KeyCombo { key: Key::Named(NamedKey::Tab), modifiers: ctrl_shift }, Action::PrevTab),
            // Pane management
            (
                KeyCombo { key: Key::Character("D".into()), modifiers: ctrl_shift },
                Action::SplitHorizontal,
            ),
            (
                KeyCombo { key: Key::Character("E".into()), modifiers: ctrl_shift },
                Action::SplitVertical,
            ),
            (
                KeyCombo { key: Key::Character("X".into()), modifiers: ctrl_shift },
                Action::ClosePane,
            ),
            // Focus navigation
            (
                KeyCombo { key: Key::Named(NamedKey::ArrowUp), modifiers: ctrl_shift },
                Action::FocusUp,
            ),
            (
                KeyCombo { key: Key::Named(NamedKey::ArrowDown), modifiers: ctrl_shift },
                Action::FocusDown,
            ),
            (
                KeyCombo { key: Key::Named(NamedKey::ArrowLeft), modifiers: ctrl_shift },
                Action::FocusLeft,
            ),
            (
                KeyCombo { key: Key::Named(NamedKey::ArrowRight), modifiers: ctrl_shift },
                Action::FocusRight,
            ),
            // Clipboard
            (KeyCombo { key: Key::Character("C".into()), modifiers: ctrl_shift }, Action::Copy),
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
    ///
    /// The modifiers must match exactly (not a subset).
    pub fn match_key(&self, event: &KeyEvent, modifiers: ModifiersState) -> Action {
        for (combo, action) in &self.bindings {
            if combo.modifiers == modifiers && combo.key == event.logical_key {
                return action.clone();
            }
        }
        Action::None
    }
}
