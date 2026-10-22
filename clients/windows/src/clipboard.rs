//! Programs that set the clipboard (OSC 52, write only), per machine.
//!
//! A pane view finds the request (`osc52::Scanner`) and tells the shell. The shell applies the
//! machine's policy: `off` drops it, `allow` copies, `ask` shows a prompt that does not block
//! the window. The first answer is kept as the policy for that machine. A request to read the
//! clipboard is never answered, and a write over 1 MiB is dropped.
//!
//! The prompt is hard to hijack:
//! - While a prompt shows, every new request (from any machine) is dropped and counted. A later
//!   request cannot swap the text under the user's click.
//! - Each prompt has a nonce that its buttons carry. An answer with another nonce is ignored.
//! - Clicks in the first 500 ms after the prompt appears are ignored (a prompt that pops up
//!   under a double click or a keystroke cannot be answered by accident).

use std::time::{Duration, Instant};

use forgetty_shell_core::osc52::{preview, Policy, Preview, Request};
use gpui::{ClipboardItem, Context};
use uuid::Uuid;

use crate::shell::AppShell;
use crate::view::ViewEvent;

/// Clicks before this age are ignored.
pub const CLICK_GUARD: Duration = Duration::from_millis(500);

/// A write that waits for the user's answer.
#[derive(Clone, Debug)]
pub struct ClipPrompt {
    pub machine: Uuid,
    pub text: String,
    pub nonce: u64,
    pub shown_at: Instant,
    pub preview: Preview,
}

/// Counters for the smoke.
#[derive(Clone, Copy, Default, Debug)]
pub struct ClipStats {
    pub copied: u32,
    pub dropped_off: u32,
    pub prompted: u32,
    pub too_large: u32,
    pub reads_denied: u32,
    /// Requests dropped because a prompt was already showing.
    pub dropped_while_prompt: u32,
    /// Answers ignored: wrong nonce, or too early.
    pub answers_ignored: u32,
}

impl AppShell {
    pub fn machine_name(&self, id: Uuid) -> String {
        self.machines.iter().find(|m| m.id == id).map(|m| m.name.clone()).unwrap_or_else(|| "this machine".into())
    }

    /// Put text on the clipboard on behalf of a program, and remember the clipboard counter, so
    /// other code (the pair dialog) can tell that this text did not come from the user.
    fn write_clip_from_program(&mut self, text: String, cx: &mut Context<Self>) {
        cx.write_to_clipboard(ClipboardItem::new_string(text.clone()));
        self.osc52_clip_seq = Some(crate::winsys::clipboard_sequence());
        self.osc52_last_text = Some(text);
        self.clip_stats.copied += 1;
    }

    /// Did a program put this clipboard content there (our OSC 52 path)?
    pub fn clipboard_is_from_osc52(&self, text: &str) -> bool {
        let seq = crate::winsys::clipboard_sequence();
        (seq != 0 && self.osc52_clip_seq == Some(seq)) || self.osc52_last_text.as_deref() == Some(text)
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
                if self.clip_prompt.is_some() {
                    self.clip_stats.dropped_while_prompt += 1;
                    return;
                }
                match self.osc52.get(&machine.to_string()) {
                    Policy::Off => self.clip_stats.dropped_off += 1,
                    Policy::Allow => self.write_clip_from_program(text.clone(), cx),
                    Policy::Ask => {
                        self.clip_nonce += 1;
                        let nonce = self.clip_nonce;
                        self.clip_stats.prompted += 1;
                        self.clip_prompt =
                            Some(ClipPrompt { machine, text: text.clone(), nonce, shown_at: Instant::now(), preview: preview(text) });
                        // Draw the buttons as live once the guard time has passed.
                        cx.spawn(async move |this, cx| {
                            cx.background_executor().timer(CLICK_GUARD + Duration::from_millis(30)).await;
                            let _ = this.update(cx, |_, cx| cx.notify());
                        })
                        .detach();
                        cx.notify();
                    }
                }
            }
        }
    }

    /// The answer to the prompt `nonce`: remembered as the machine's policy.
    pub fn clip_answer(&mut self, allow: bool, nonce: u64, cx: &mut Context<Self>) {
        self.clip_answer_at(allow, nonce, Instant::now(), cx);
    }

    /// `clip_answer` with the time of the click (the smoke passes its own).
    pub fn clip_answer_at(&mut self, allow: bool, nonce: u64, now: Instant, cx: &mut Context<Self>) {
        let ok = self.clip_prompt.as_ref().is_some_and(|p| p.nonce == nonce && now.saturating_duration_since(p.shown_at) >= CLICK_GUARD);
        if !ok {
            if self.clip_prompt.is_some() {
                self.clip_stats.answers_ignored += 1;
            }
            return;
        }
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
                Policy::Allow => self.write_clip_from_program(text, cx),
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
