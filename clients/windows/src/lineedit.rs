//! Minimal single-line text editor for the palette query and tab rename.

use gpui::Keystroke;

#[derive(Clone, Debug, Default)]
pub struct LineEdit {
    pub text: String,
    /// Caret position in chars.
    pub cursor: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EditResult {
    /// Text or caret changed.
    Edited,
    /// Key handled, nothing changed.
    Handled,
    /// Not an editing key.
    Ignored,
}

impl LineEdit {
    pub fn new(text: &str) -> Self {
        Self { text: text.to_string(), cursor: text.chars().count() }
    }

    fn byte_at(&self, chars: usize) -> usize {
        self.text.char_indices().nth(chars).map(|(i, _)| i).unwrap_or(self.text.len())
    }

    pub fn len(&self) -> usize {
        self.text.chars().count()
    }

    pub fn insert(&mut self, s: &str) {
        let at = self.byte_at(self.cursor);
        self.text.insert_str(at, s);
        self.cursor += s.chars().count();
    }

    pub fn backspace(&mut self) -> bool {
        if self.cursor == 0 {
            return false;
        }
        let a = self.byte_at(self.cursor - 1);
        let b = self.byte_at(self.cursor);
        self.text.replace_range(a..b, "");
        self.cursor -= 1;
        true
    }

    pub fn delete(&mut self) -> bool {
        if self.cursor >= self.len() {
            return false;
        }
        let a = self.byte_at(self.cursor);
        let b = self.byte_at(self.cursor + 1);
        self.text.replace_range(a..b, "");
        true
    }

    /// Apply an editing keystroke.
    pub fn key(&mut self, ks: &Keystroke) -> EditResult {
        let m = &ks.modifiers;
        if m.control || m.alt || m.platform {
            return EditResult::Ignored;
        }
        let moved = |s: &mut Self, to: usize| {
            if s.cursor == to {
                EditResult::Handled
            } else {
                s.cursor = to;
                EditResult::Edited
            }
        };
        match ks.key.as_str() {
            "backspace" => {
                if self.backspace() {
                    EditResult::Edited
                } else {
                    EditResult::Handled
                }
            }
            "delete" => {
                if self.delete() {
                    EditResult::Edited
                } else {
                    EditResult::Handled
                }
            }
            "left" => moved(self, self.cursor.saturating_sub(1)),
            "right" => moved(self, (self.cursor + 1).min(self.len())),
            "home" => moved(self, 0),
            "end" => moved(self, self.len()),
            _ => match ks.key_char.as_deref() {
                Some(t) if !t.is_empty() && t.chars().all(|c| !c.is_control()) => {
                    self.insert(t);
                    EditResult::Edited
                }
                _ => EditResult::Ignored,
            },
        }
    }
}
