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
}

#[derive(Default, PartialEq, Clone, Copy)]
enum Stage {
    #[default]
    Init,
    Tabs,
    Splits,
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
                self.wp3d.max_gap = self.wp3d.max_gap.max(now.duration_since(prev).as_secs_f64() * 1000.0);
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
                    self.wp3d.stage = Stage::Close;
                }
            }
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
    }
}
