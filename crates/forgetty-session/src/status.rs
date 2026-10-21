//! Per-pane program status: the records of the Program Status Protocol
//! (OSC 7501), reduced to one pane status.
//!
//! Wire states map to the names the server API uses:
//!
//! | OSC 7501 `state` | API `status` |
//! |---|---|
//! | `idle` | `idle` |
//! | `working` | `working` |
//! | `blocked` | `waiting` |
//! | `done` | `done` |
//! | `error` | `failed` |
//!
//! A pane can hold several records (one per `id`). The pane status shown to
//! clients is the most urgent one: waiting, then failed, then working, then
//! done, then idle. Ties go to the record updated last.

use crate::osc::{ReportState, StatusReport};

/// Records kept per pane. The protocol requires at least 64.
pub const MAX_RECORDS: usize = 64;

/// Status of a pane, as the API names it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PaneStatus {
    #[default]
    Idle,
    Working,
    Waiting,
    Done,
    Failed,
}

impl PaneStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            PaneStatus::Idle => "idle",
            PaneStatus::Working => "working",
            PaneStatus::Waiting => "waiting",
            PaneStatus::Done => "done",
            PaneStatus::Failed => "failed",
        }
    }

    /// Parse an API status name. `blocked` and `error` (the wire names) are
    /// accepted as aliases.
    pub fn parse(s: &str) -> Option<PaneStatus> {
        match s {
            "idle" => Some(PaneStatus::Idle),
            "working" => Some(PaneStatus::Working),
            "waiting" | "blocked" => Some(PaneStatus::Waiting),
            "done" => Some(PaneStatus::Done),
            "failed" | "error" => Some(PaneStatus::Failed),
            _ => None,
        }
    }

    fn urgency(self) -> u8 {
        match self {
            PaneStatus::Waiting => 4,
            PaneStatus::Failed => 3,
            PaneStatus::Working => 2,
            PaneStatus::Done => 1,
            PaneStatus::Idle => 0,
        }
    }

    fn from_report(s: ReportState) -> Option<PaneStatus> {
        match s {
            ReportState::Idle => Some(PaneStatus::Idle),
            ReportState::Working => Some(PaneStatus::Working),
            ReportState::Done => Some(PaneStatus::Done),
            ReportState::Blocked => Some(PaneStatus::Waiting),
            ReportState::Error => Some(PaneStatus::Failed),
            ReportState::Clear => None,
        }
    }
}

/// The pane status plus what goes with it.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct StatusSnapshot {
    pub status: PaneStatus,
    pub message: Option<String>,
    /// `permission`, `question` or `auth` (waiting only).
    pub kind: Option<String>,
    pub progress: Option<u8>,
    pub app: Option<String>,
    /// Id of the record this snapshot comes from. `None` is the root record.
    pub record_id: Option<String>,
}

#[derive(Debug, Clone)]
struct Record {
    /// Empty string is the root record.
    id: String,
    status: PaneStatus,
    kind: Option<String>,
    progress: Option<u8>,
    app: Option<String>,
    msg: Option<String>,
    stamp: u64,
}

/// The status records of one pane.
#[derive(Debug, Default)]
pub struct StatusTracker {
    records: Vec<Record>,
    tick: u64,
}

impl StatusTracker {
    pub fn new() -> Self {
        Self::default()
    }

    /// Apply one validated report. Each report replaces its record whole.
    pub fn apply(&mut self, report: StatusReport) {
        self.tick += 1;
        let id = report.id.clone().unwrap_or_default();
        let Some(status) = PaneStatus::from_report(report.state) else {
            // clear: remove the record and everything beneath it.
            if id.is_empty() {
                self.records.clear();
            } else {
                let prefix = format!("{id}/");
                self.records.retain(|r| r.id != id && !r.id.starts_with(&prefix));
            }
            return;
        };
        let rec = Record {
            id: id.clone(),
            status,
            kind: report.kind,
            progress: report.progress,
            app: report.app,
            msg: report.msg,
            stamp: self.tick,
        };
        if let Some(slot) = self.records.iter_mut().find(|r| r.id == id) {
            *slot = rec;
            return;
        }
        if self.records.len() >= MAX_RECORDS {
            if let Some(oldest) =
                self.records.iter().enumerate().min_by_key(|(_, r)| r.stamp).map(|(i, _)| i)
            {
                self.records.swap_remove(oldest);
            }
        }
        self.records.push(rec);
    }

    /// Drop `working` and `waiting` records. Called when the process exits and
    /// when a new shell prompt begins. `done` and `failed` survive both.
    pub fn drop_transient(&mut self) {
        self.records.retain(|r| !matches!(r.status, PaneStatus::Working | PaneStatus::Waiting));
    }

    /// Number of records held.
    pub fn len(&self) -> usize {
        self.records.len()
    }

    pub fn is_empty(&self) -> bool {
        self.records.is_empty()
    }

    /// The pane status: the most urgent record, `idle` when there is none.
    pub fn effective(&self) -> StatusSnapshot {
        let best = self.records.iter().max_by_key(|r| (r.status.urgency(), r.stamp));
        let Some(r) = best else { return StatusSnapshot::default() };
        StatusSnapshot {
            status: r.status,
            message: r.msg.clone(),
            kind: r.kind.clone(),
            progress: r.progress,
            app: r.app.clone().or_else(|| self.inherited_app(&r.id)),
            record_id: (!r.id.is_empty()).then(|| r.id.clone()),
        }
    }

    /// `app` of the nearest ancestor record that has one.
    fn inherited_app(&self, id: &str) -> Option<String> {
        let mut cur = id;
        while let Some(pos) = cur.rfind('/') {
            cur = &cur[..pos];
            if let Some(a) = self.records.iter().find(|r| r.id == cur).and_then(|r| r.app.clone()) {
                return Some(a);
            }
        }
        self.records.iter().find(|r| r.id.is_empty()).and_then(|r| r.app.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn report(state: ReportState, id: Option<&str>, msg: Option<&str>) -> StatusReport {
        StatusReport {
            id: id.map(str::to_string),
            state,
            kind: None,
            progress: None,
            app: None,
            title: None,
            msg: msg.map(str::to_string),
        }
    }

    #[test]
    fn default_is_idle() {
        let t = StatusTracker::new();
        let s = t.effective();
        assert_eq!(s.status, PaneStatus::Idle);
        assert_eq!(s.message, None);
    }

    #[test]
    fn wire_states_map_to_api_names() {
        let mut t = StatusTracker::new();
        for (st, want) in [
            (ReportState::Working, "working"),
            (ReportState::Blocked, "waiting"),
            (ReportState::Done, "done"),
            (ReportState::Error, "failed"),
            (ReportState::Idle, "idle"),
        ] {
            t.apply(report(st, None, Some("m")));
            assert_eq!(t.effective().status.as_str(), want);
        }
    }

    #[test]
    fn a_report_replaces_its_record_whole() {
        let mut t = StatusTracker::new();
        let mut r = report(ReportState::Working, None, Some("first"));
        r.app = Some("cargo".into());
        t.apply(r);
        t.apply(report(ReportState::Working, None, None));
        let s = t.effective();
        assert_eq!(s.message, None);
        assert_eq!(s.app, None, "a key missing from the report is missing afterwards");
        assert_eq!(t.len(), 1);
    }

    #[test]
    fn most_urgent_record_wins() {
        let mut t = StatusTracker::new();
        t.apply(report(ReportState::Working, None, Some("root")));
        t.apply(report(ReportState::Blocked, Some("eu"), Some("approve?")));
        t.apply(report(ReportState::Done, Some("us"), Some("ok")));
        let s = t.effective();
        assert_eq!(s.status, PaneStatus::Waiting);
        assert_eq!(s.message.as_deref(), Some("approve?"));
        assert_eq!(s.record_id.as_deref(), Some("eu"));
        t.apply(report(ReportState::Clear, Some("eu"), None));
        assert_eq!(t.effective().status, PaneStatus::Working);
    }

    #[test]
    fn clear_removes_children_and_clear_all_empties() {
        let mut t = StatusTracker::new();
        t.apply(report(ReportState::Working, Some("build"), None));
        t.apply(report(ReportState::Working, Some("build/test"), None));
        t.apply(report(ReportState::Working, Some("buildx"), None));
        t.apply(report(ReportState::Clear, Some("build"), None));
        assert_eq!(t.len(), 1, "build and build/test go, buildx stays");
        t.apply(report(ReportState::Clear, None, None));
        assert!(t.is_empty());
        assert_eq!(t.effective().status, PaneStatus::Idle);
    }

    #[test]
    fn transient_records_drop_but_done_and_failed_survive() {
        let mut t = StatusTracker::new();
        t.apply(report(ReportState::Working, Some("a"), None));
        t.apply(report(ReportState::Blocked, Some("b"), None));
        t.apply(report(ReportState::Done, Some("c"), Some("fin")));
        t.apply(report(ReportState::Error, Some("d"), None));
        t.drop_transient();
        assert_eq!(t.len(), 2);
        assert_eq!(t.effective().status, PaneStatus::Failed);
    }

    #[test]
    fn app_is_inherited_from_the_nearest_ancestor() {
        let mut t = StatusTracker::new();
        let mut root = report(ReportState::Working, None, None);
        root.app = Some("deploy".into());
        t.apply(root);
        t.apply(report(ReportState::Blocked, Some("eu/web"), None));
        assert_eq!(t.effective().app.as_deref(), Some("deploy"));
    }

    #[test]
    fn record_count_is_capped_and_least_recent_is_evicted() {
        let mut t = StatusTracker::new();
        for i in 0..(MAX_RECORDS + 20) {
            t.apply(report(ReportState::Working, Some(&format!("r{i}")), None));
        }
        assert_eq!(t.len(), MAX_RECORDS);
        // the first ones are gone, the last one is there
        assert!(!t.records.iter().any(|r| r.id == "r0"));
        assert!(t.records.iter().any(|r| r.id == format!("r{}", MAX_RECORDS + 19)));
    }

    #[test]
    fn parse_accepts_api_and_wire_names() {
        assert_eq!(PaneStatus::parse("waiting"), Some(PaneStatus::Waiting));
        assert_eq!(PaneStatus::parse("blocked"), Some(PaneStatus::Waiting));
        assert_eq!(PaneStatus::parse("failed"), Some(PaneStatus::Failed));
        assert_eq!(PaneStatus::parse("error"), Some(PaneStatus::Failed));
        assert_eq!(PaneStatus::parse("nope"), None);
    }
}
