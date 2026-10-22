//! Programs that set the clipboard (OSC 52, write only), per machine.
//!
//! A pane view finds the request (`osc52::Scanner`) and tells the shell. The shell applies the
//! machine's policy: `off` drops it, `allow` copies, `ask` shows a prompt that does not block
//! the window. The first answer is kept as the policy for that machine. A request to read the
//! clipboard is never answered, and a write over 1 MiB is dropped.

use forgetty_shell_core::osc52::{Policy, Request};
use gpui::{ClipboardItem, Context};
use uuid::Uuid;

use crate::shell::AppShell;
use crate::view::ViewEvent;

/// A write that waits for the user's answer.
#[derive(Clone, Debug)]
pub struct ClipPrompt {
    pub machine: Uuid,
    pub text: String,
}

/// Counters for the smoke.
#[derive(Clone, Copy, Default, Debug)]
pub struct ClipStats {
    pub copied: u32,
    pub dropped_off: u32,
    pub prompted: u32,
    pub too_large: u32,
    pub reads_denied: u32,
}

impl AppShell {
    pub fn machine_name(&self, id: Uuid) -> String {
        self.machines.iter().find(|m| m.id == id).map(|m| m.name.clone()).unwrap_or_else(|| "this machine".into())
    }

    /// A pane of `machine` reported a request.
    pub fn on_view_event(&mut self, machine: Uuid, ev: &ViewEvent, cx: &mut Context<Self>) {
        let ViewEvent::Clipboard(req) = ev;
        match req {
            Request::ReadDenied => self.clip_stats.reads_denied += 1,
            Request::TooLarge => self.clip_stats.too_large += 1,
            Request::Write { text } => {
                if text.is_empty() {
                    return;
                }
                match self.osc52.get(&machine.to_string()) {
                    Policy::Off => self.clip_stats.dropped_off += 1,
                    Policy::Allow => {
                        cx.write_to_clipboard(ClipboardItem::new_string(text.clone()));
                        self.clip_stats.copied += 1;
                    }
                    Policy::Ask => {
                        // One prompt at a time. A newer request replaces the waiting text.
                        if self.clip_prompt.is_none() {
                            self.clip_stats.prompted += 1;
                        }
                        self.clip_prompt = Some(ClipPrompt { machine, text: text.clone() });
                        cx.notify();
                    }
                }
            }
        }
    }

    /// The answer to the prompt: remembered as the machine's policy.
    pub fn clip_answer(&mut self, allow: bool, cx: &mut Context<Self>) {
        let Some(p) = self.clip_prompt.take() else { return };
        self.set_clip_policy(p.machine, if allow { Policy::Allow } else { Policy::Off }, Some(p.text), cx);
    }

    /// Set the policy of `machine`. `pending` is the text that waited for the answer.
    pub fn set_clip_policy(&mut self, machine: Uuid, policy: Policy, pending: Option<String>, cx: &mut Context<Self>) {
        self.osc52.set(&machine.to_string(), policy);
        let waiting = match &self.clip_prompt {
            Some(p) if p.machine == machine => self.clip_prompt.take().map(|p| p.text),
            _ => None,
        };
        if let Some(text) = pending.or(waiting) {
            match policy {
                Policy::Allow => {
                    cx.write_to_clipboard(ClipboardItem::new_string(text));
                    self.clip_stats.copied += 1;
                }
                Policy::Off => self.clip_stats.dropped_off += 1,
                Policy::Ask => {}
            }
        }
        cx.notify();
    }

    /// Palette actions: the policy of the machine on screen.
    pub fn set_active_clip_policy(&mut self, policy: Policy, cx: &mut Context<Self>) {
        let id = self.machine.id;
        self.set_clip_policy(id, policy, None, cx);
    }
}
