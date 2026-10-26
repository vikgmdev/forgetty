//! WP3d: how long a new tab and a split take to appear and to print, and how long the action
//! itself holds the UI thread. One state machine, stepped per painted frame.
//!
//! Per sample, from the moment the action starts (`t0`):
//! - `call`: how long the action call took. This is the time the UI thread was held by it.
//! - `frame`: until the pane has a view that was painted once.
//! - `output`: until the pane shows any text (the shell printed its first line).
//! Also the longest gap between two frames while the phase ran.

use std::time::{Duration, Instant};

use forgetty_shell_core::layout::SplitDir;
use serde_json::json;

use super::{check, fail, record, snap, ShellSmoke};
use crate::shell::AppShell;
use forgetty_shell_core::{PaneId, Uuid};
use gpui::{Context, Window};

/// Samples of each kind.
const TABS: usize = 6;
const SPLITS: usize = 4;
/// The slow-start check makes every start wait this long, and starts in a folder that is not there.
const SLOW_MS: u64 = 1500;
const MISSING_DIR: &str = r"C:\forgetty-no-such-folder-wp3d";

#[derive(Default)]
pub struct Wp3d {
    stage: Stage,
    i: usize,
    t0: Option<Instant>,
    call_ms: f64,
    tabs_before: usize,
    leaves_before: usize,
    pane: Option<PaneId>,
    frame_ms: Option<f64>,
    new_tab: Vec<[f64; 3]>,
    split: Vec<[f64; 3]>,
    tabs: Vec<Uuid>,
    tabs0: usize,
    last_step: Option<Instant>,
    max_gap: f64,
    slow_gap: f64,
    typed: bool,
    notice: Option<String>,
}

#[derive(Default, PartialEq, Clone, Copy)]
enum Stage {
    #[default]
    Init,
    Tabs,
    Splits,
    Slow,
    Close,
}

fn median(v: &[f64]) -> f64 {
    let mut s = v.to_vec();
    s.sort_by(|a, b| a.partial_cmp(b).unwrap());
    s.get(s.len() / 2).copied().unwrap_or(0.0)
}

fn r1(x: f64) -> f64 {
    (x * 10.0).round() / 10.0
}

impl ShellSmoke {
    pub(super) fn wp3d(&mut self, shell: &mut AppShell, _window: &mut Window, cx: &mut Context<AppShell>) {
        if self.sub_t0.elapsed() > Duration::from_secs(15) {
            record("wp3d_stuck", json!(format!("stage {} i {}", self.wp3d.stage as u8, self.wp3d.i)));
            fail("wp3d: a new tab or split did not show a pane with text");
        }
        let now = Instant::now();
        if let Some(prev) = self.wp3d.last_step.replace(now) {
            // Only the gaps while a sample is running count.
            if self.wp3d.t0.is_some() {
                let gap = now.duration_since(prev).as_secs_f64() * 1000.0;
                if self.wp3d.stage == Stage::Slow {
                    self.wp3d.slow_gap = self.wp3d.slow_gap.max(gap);
                } else {
                    self.wp3d.max_gap = self.wp3d.max_gap.max(gap);
                }
            }
        }
        match self.wp3d.stage {
            Stage::Init => {
                self.wp3d.tabs0 = shell.layout.tab_count();
                self.wp3d.stage = Stage::Tabs;
                self.sub_t0 = Instant::now();
            }
            Stage::Tabs | Stage::Splits => {
                let splitting = self.wp3d.stage == Stage::Splits;
                let Some(t0) = self.wp3d.t0 else {
                    // Start a sample: the action, timed.
                    self.wp3d.tabs_before = shell.layout.tab_count();
                    self.wp3d.leaves_before = shell.active_tab().map(|t| t.pane_tree.leaves().len()).unwrap_or(0);
                    self.wp3d.pane = None;
                    self.wp3d.frame_ms = None;
                    let t0 = Instant::now();
                    if splitting {
                        let dir = if self.wp3d.i % 2 == 0 { SplitDir::Horizontal } else { SplitDir::Vertical };
                        shell.split(dir, cx);
                    } else {
                        shell.new_tab(cx);
                    }
                    self.wp3d.call_ms = t0.elapsed().as_secs_f64() * 1000.0;
                    self.wp3d.t0 = Some(t0);
                    return;
                };
                let appeared = if splitting {
                    shell.active_tab().is_some_and(|t| t.pane_tree.leaves().len() == self.wp3d.leaves_before + 1)
                } else {
                    shell.layout.tab_count() == self.wp3d.tabs_before + 1
                };
                if !appeared {
                    return;
                }
                let Some(pane) = shell.active_pane() else { return };
                self.wp3d.pane = Some(pane);
                let Some(view) = shell.views.get(&pane) else { return };
                if self.wp3d.frame_ms.is_none() && view.read(cx).frames >= 1 {
                    self.wp3d.frame_ms = Some(t0.elapsed().as_secs_f64() * 1000.0);
                }
                let has_text = snap(shell, cx, pane).is_some_and(|s| s.rows.iter().any(|r| !r.trim().is_empty()));
                let (Some(frame), true) = (self.wp3d.frame_ms, has_text) else { return };
                let out = t0.elapsed().as_secs_f64() * 1000.0;
                let sample = [self.wp3d.call_ms, frame, out];
                if splitting {
                    self.wp3d.split.push(sample);
                } else {
                    self.wp3d.new_tab.push(sample);
                    if let Some(t) = shell.active_tab().map(|t| t.id) {
                        self.wp3d.tabs.push(t);
                    }
                }
                self.wp3d.t0 = None;
                self.wp3d.i += 1;
                self.sub_t0 = Instant::now();
                if !splitting && self.wp3d.i == TABS {
                    self.wp3d.stage = Stage::Splits;
                    self.wp3d.i = 0;
                } else if splitting && self.wp3d.i == SPLITS {
                    self.wp3d.stage = Stage::Slow;
                }
            }
            Stage::Slow => self.wp3d_slow(shell, cx),
            Stage::Close => {
                if !self.wp3d.tabs.is_empty() {
                    for t in std::mem::take(&mut self.wp3d.tabs) {
                        shell.close_tab(t, cx);
                    }
                    self.sub_t0 = Instant::now();
                    return;
                }
                if shell.layout.tab_count() != self.wp3d.tabs0 {
                    return;
                }
                self.wp3d_report();
                self.goto(super::Phase::RSetup);
            }
        }
    }

    fn wp3d_report(&mut self) {
        let d = &self.wp3d;
        let col = |v: &[[f64; 3]], k: usize| v.iter().map(|s| s[k]).collect::<Vec<f64>>();
        for (name, v) in [("new_tab", &d.new_tab), ("split", &d.split)] {
            let (call, frame, out) = (col(v, 0), col(v, 1), col(v, 2));
            let max = |x: &[f64]| x.iter().copied().fold(0.0, f64::max);
            record(
                &format!("wp3d_{name}_ms"),
                json!({
                    "samples": v.len(),
                    "call_median": r1(median(&call)), "call_max": r1(max(&call)),
                    "frame_median": r1(median(&frame)), "frame_max": r1(max(&frame)),
                    "output_median": r1(median(&out)), "output_max": r1(max(&out)),
                    "all": v.iter().map(|s| [r1(s[0]), r1(s[1]), r1(s[2])]).collect::<Vec<_>>(),
                }),
            );
        }
        record("wp3d_max_frame_gap_ms", json!(r1(d.max_gap)));
        check("wp3d_measured", d.new_tab.len() == TABS && d.split.len() == SPLITS);
        // The action must not hold the UI thread longer than one frame (16.7 ms at 60 Hz). Before
        // the start moved to a worker thread the call took about 4.6 ms on the CI runner (more on a
        // PC with a virus scanner); now it only updates the layout.
        let call_max = d.new_tab.iter().chain(d.split.iter()).map(|s| s[0]).fold(0.0, f64::max);
        check("wp3d_call_within_one_frame", call_max < 16.7);
        // No gap between painted frames near the length of the slow start.
        check("wp3d_no_long_frame_gap", d.max_gap < 250.0 && d.slow_gap < 250.0);
    }

    /// A start that takes 1.5 s, in a folder that does not exist. The tab must be there at once
    /// and say it is starting, what is typed before the shell is up must arrive in order, the
    /// notice must name the folder, and the UI must keep painting meanwhile.
    fn wp3d_slow(&mut self, shell: &mut AppShell, cx: &mut Context<AppShell>) {
        let Some(t0) = self.wp3d.t0 else {
            self.local.set_spawn_delay(Duration::from_millis(SLOW_MS));
            self.wp3d.tabs_before = shell.layout.tab_count();
            self.sub_t0 = Instant::now();
            let t0 = Instant::now();
            shell.new_tab_with(Vec::new(), Some(MISSING_DIR.into()), cx);
            self.wp3d.call_ms = t0.elapsed().as_secs_f64() * 1000.0;
            self.wp3d.t0 = Some(t0);
            return;
        };
        if shell.layout.tab_count() != self.wp3d.tabs_before + 1 {
            return;
        }
        let Some(pane) = shell.active_pane() else { return };
        let Some(view) = shell.views.get(&pane).cloned() else { return };
        if !self.wp3d.typed {
            let up = t0.elapsed().as_secs_f64() * 1000.0;
            let title = shell.meta.get(&pane).map(|m| m.title.clone()).unwrap_or_default();
            record("wp3d_slow_call_ms", json!(r1(self.wp3d.call_ms)));
            record("wp3d_slow_view_ms", json!(r1(up)));
            check("wp3d_slow_call_returns", self.wp3d.call_ms < SLOW_MS as f64 / 4.0);
            check("wp3d_slow_pane_at_once", up < SLOW_MS as f64 / 2.0);
            check("wp3d_slow_starting_title", title == forgetty_shell_core::local::STARTING_TITLE);
            // Typed before the shell exists: kept, and sent in this order.
            view.update(cx, |v, _| {
                v.send_bytes(b"echo ORDER-A\r");
                v.send_bytes(b"echo ORDER-B\r");
            });
            self.wp3d.typed = true;
            return;
        }
        if self.wp3d.notice.is_none() {
            self.wp3d.notice = view.read(cx).notice_text();
        }
        let rows = snap(shell, cx, pane).map(|s| s.rows).unwrap_or_default();
        let b_lines = rows.iter().filter(|r| r.contains("ORDER-B")).count();
        // The command line and its output: two lines each.
        if b_lines < 2 || self.wp3d.notice.is_none() {
            return;
        }
        let a_last = rows.iter().rposition(|r| r.contains("ORDER-A"));
        let b_first = rows.iter().position(|r| r.contains("ORDER-B"));
        record("wp3d_slow_notice", json!(self.wp3d.notice.clone().unwrap_or_default()));
        check("wp3d_slow_input_in_order", matches!((a_last, b_first), (Some(a), Some(b)) if a < b));
        check("wp3d_slow_notice_names_folder", self.wp3d.notice.as_deref().is_some_and(|n| n.contains("forgetty-no-such-folder-wp3d")));
        let title = shell.meta.get(&pane).map(|m| m.title.clone()).unwrap_or_default();
        check("wp3d_slow_title_cleared", title != forgetty_shell_core::local::STARTING_TITLE);
        record("wp3d_slow_max_frame_gap_ms", json!(r1(self.wp3d.slow_gap)));
        self.local.set_spawn_delay(Duration::ZERO);
        if let Some(t) = shell.active_tab().map(|t| t.id) {
            self.wp3d.tabs.push(t);
        }
        self.wp3d.t0 = None;
        self.sub_t0 = Instant::now();
        self.wp3d.stage = Stage::Close;
    }
}
