//! Layout model. Mirrors `forgetty-session::layout` and PROTOCOL_V2 section 4
//! (`get_layout`, `LayoutChanged`): workspaces, then tabs, then a pane tree
//! with split ratios. The serde shape is the wire shape of the server.

use forgetty_core::PaneId;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// `horizontal` = children side by side (split right), `vertical` = stacked
/// (split down). Same strings as the socket API.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SplitDir {
    Horizontal,
    Vertical,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum PaneTree {
    Leaf { pane_id: PaneId },
    Split {
        direction: SplitDir,
        /// Share of the first child, 0..1.
        ratio: f32,
        first: Box<PaneTree>,
        second: Box<PaneTree>,
    },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Tab {
    pub id: Uuid,
    /// Reported by the client from title sequences; empty until known.
    #[serde(default)]
    pub title: String,
    pub pane_tree: PaneTree,
    #[serde(default)]
    pub active_pane_id: Option<PaneId>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Workspace {
    pub id: Uuid,
    pub name: String,
    pub tabs: Vec<Tab>,
    #[serde(default)]
    pub active_tab: usize,
    #[serde(default)]
    pub color: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Layout {
    pub workspaces: Vec<Workspace>,
    #[serde(default)]
    pub active_workspace: usize,
}

impl Layout {
    pub fn active_workspace(&self) -> Option<&Workspace> {
        self.workspaces.get(self.active_workspace)
    }

    pub fn active_tab(&self) -> Option<&Tab> {
        let ws = self.active_workspace()?;
        ws.tabs.get(ws.active_tab)
    }

    /// (workspace index, tab index) of a tab id.
    pub fn find_tab(&self, tab_id: Uuid) -> Option<(usize, usize)> {
        self.workspaces.iter().enumerate().find_map(|(w, ws)| {
            ws.tabs.iter().position(|t| t.id == tab_id).map(|t| (w, t))
        })
    }

    /// (workspace index, tab index) of the tab that holds a pane.
    pub fn find_pane(&self, pane: PaneId) -> Option<(usize, usize)> {
        self.workspaces.iter().enumerate().find_map(|(w, ws)| {
            ws.tabs.iter().position(|t| t.pane_tree.contains(pane)).map(|t| (w, t))
        })
    }

    pub fn all_panes(&self) -> Vec<PaneId> {
        let mut out = Vec::new();
        for ws in &self.workspaces {
            for t in &ws.tabs {
                t.pane_tree.collect_leaves(&mut out);
            }
        }
        out
    }

    pub fn tab_count(&self) -> usize {
        self.workspaces.iter().map(|w| w.tabs.len()).sum()
    }
}

impl Tab {
    /// Focused pane: the stored one if it is still a leaf, else the first leaf.
    pub fn focused_pane(&self) -> PaneId {
        match self.active_pane_id {
            Some(p) if self.pane_tree.contains(p) => p,
            _ => self.pane_tree.first_leaf(),
        }
    }
}

// ---- geometry --------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Default)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

impl Rect {
    pub fn new(x: f32, y: f32, w: f32, h: f32) -> Self {
        Self { x, y, w, h }
    }
    pub fn right(&self) -> f32 {
        self.x + self.w
    }
    pub fn bottom(&self) -> f32 {
        self.y + self.h
    }
    pub fn contains(&self, x: f32, y: f32) -> bool {
        x >= self.x && x < self.right() && y >= self.y && y < self.bottom()
    }
}

/// A draggable split divider.
#[derive(Clone, Debug, PartialEq)]
pub struct Divider {
    /// Path from the root (false = first child, true = second child).
    pub path: Vec<bool>,
    pub dir: SplitDir,
    /// The whole split node's area (panes plus divider).
    pub area: Rect,
    /// The divider bar itself.
    pub bar: Rect,
    pub ratio: f32,
    /// Leftmost leaf of the first child: the id `update_split_ratios` uses.
    pub first_leaf: PaneId,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Side {
    Left,
    Right,
    Up,
    Down,
}

#[derive(Clone, Debug, Default)]
pub struct PaneGeometry {
    pub panes: Vec<(PaneId, Rect)>,
    pub dividers: Vec<Divider>,
}

impl PaneTree {
    pub fn leaf(pane_id: PaneId) -> Self {
        PaneTree::Leaf { pane_id }
    }

    pub fn collect_leaves(&self, out: &mut Vec<PaneId>) {
        match self {
            PaneTree::Leaf { pane_id } => out.push(*pane_id),
            PaneTree::Split { first, second, .. } => {
                first.collect_leaves(out);
                second.collect_leaves(out);
            }
        }
    }

    pub fn leaves(&self) -> Vec<PaneId> {
        let mut v = Vec::new();
        self.collect_leaves(&mut v);
        v
    }

    pub fn first_leaf(&self) -> PaneId {
        match self {
            PaneTree::Leaf { pane_id } => *pane_id,
            PaneTree::Split { first, .. } => first.first_leaf(),
        }
    }

    pub fn contains(&self, pane: PaneId) -> bool {
        match self {
            PaneTree::Leaf { pane_id } => *pane_id == pane,
            PaneTree::Split { first, second, .. } => first.contains(pane) || second.contains(pane),
        }
    }

    pub fn pane_count(&self) -> usize {
        match self {
            PaneTree::Leaf { .. } => 1,
            PaneTree::Split { first, second, .. } => first.pane_count() + second.pane_count(),
        }
    }

    /// Replace leaf `target` by a split holding `target` and `new_pane`.
    pub fn split_leaf(&mut self, target: PaneId, dir: SplitDir, new_pane: PaneId) -> bool {
        match self {
            PaneTree::Leaf { pane_id } if *pane_id == target => {
                *self = PaneTree::Split {
                    direction: dir,
                    ratio: 0.5,
                    first: Box::new(PaneTree::leaf(target)),
                    second: Box::new(PaneTree::leaf(new_pane)),
                };
                true
            }
            PaneTree::Leaf { .. } => false,
            PaneTree::Split { first, second, .. } => {
                first.split_leaf(target, dir, new_pane) || second.split_leaf(target, dir, new_pane)
            }
        }
    }

    /// Remove a leaf; its sibling takes the parent's place. Returns the
    /// remaining tree and the leaf that inherits focus, or `None` when the
    /// tree was only that leaf.
    pub fn remove_leaf(self, target: PaneId) -> Option<(PaneTree, PaneId)> {
        match self {
            PaneTree::Leaf { pane_id } if pane_id == target => None,
            leaf @ PaneTree::Leaf { .. } => {
                let f = leaf.first_leaf();
                Some((leaf, f))
            }
            PaneTree::Split { direction, ratio, first, second } => {
                if let PaneTree::Leaf { pane_id } = *first {
                    if pane_id == target {
                        let f = second.first_leaf();
                        return Some((*second, f));
                    }
                }
                if let PaneTree::Leaf { pane_id } = *second {
                    if pane_id == target {
                        let f = first.last_leaf();
                        return Some((*first, f));
                    }
                }
                if first.contains(target) {
                    let (t, f) = first.remove_leaf(target)?;
                    Some((PaneTree::Split { direction, ratio, first: Box::new(t), second }, f))
                } else {
                    let (t, f) = second.remove_leaf(target)?;
                    Some((PaneTree::Split { direction, ratio, first, second: Box::new(t) }, f))
                }
            }
        }
    }

    pub fn last_leaf(&self) -> PaneId {
        match self {
            PaneTree::Leaf { pane_id } => *pane_id,
            PaneTree::Split { second, .. } => second.last_leaf(),
        }
    }

    /// Set the ratio of the split whose first child's leftmost leaf is `first_leaf`.
    /// Outermost match wins, as on the server.
    pub fn set_ratio(&mut self, first_leaf: PaneId, ratio: f32) -> bool {
        match self {
            PaneTree::Leaf { .. } => false,
            PaneTree::Split { ratio: r, first, second, .. } => {
                if first.first_leaf() == first_leaf {
                    *r = ratio;
                    return true;
                }
                first.set_ratio(first_leaf, ratio) || second.set_ratio(first_leaf, ratio)
            }
        }
    }

    pub fn ratio_at(&self, path: &[bool]) -> Option<f32> {
        match (self, path.split_first()) {
            (PaneTree::Split { ratio, .. }, None) => Some(*ratio),
            (PaneTree::Split { first, second, .. }, Some((go_second, rest))) => {
                if *go_second { second } else { first }.ratio_at(rest)
            }
            _ => None,
        }
    }

    pub fn set_ratio_at(&mut self, path: &[bool], value: f32) -> bool {
        match (self, path.split_first()) {
            (PaneTree::Split { ratio, .. }, None) => {
                *ratio = value;
                true
            }
            (PaneTree::Split { first, second, .. }, Some((go_second, rest))) => {
                if *go_second { second } else { first }.set_ratio_at(rest, value)
            }
            _ => false,
        }
    }

    /// Lay the tree out in `rect`. `bar` is the divider thickness; `min` the
    /// smallest pane extent a drag may leave.
    pub fn geometry(&self, rect: Rect, bar: f32) -> PaneGeometry {
        let mut g = PaneGeometry::default();
        self.geo(rect, bar, &mut Vec::new(), &mut g);
        g
    }

    fn geo(&self, rect: Rect, bar: f32, path: &mut Vec<bool>, g: &mut PaneGeometry) {
        match self {
            PaneTree::Leaf { pane_id } => g.panes.push((*pane_id, rect)),
            PaneTree::Split { direction, ratio, first, second } => {
                let r = ratio.clamp(0.02, 0.98);
                let (r1, b, r2) = match direction {
                    SplitDir::Horizontal => {
                        let avail = (rect.w - bar).max(0.0);
                        let w1 = (avail * r).round();
                        (
                            Rect::new(rect.x, rect.y, w1, rect.h),
                            Rect::new(rect.x + w1, rect.y, bar, rect.h),
                            Rect::new(rect.x + w1 + bar, rect.y, (avail - w1).max(0.0), rect.h),
                        )
                    }
                    SplitDir::Vertical => {
                        let avail = (rect.h - bar).max(0.0);
                        let h1 = (avail * r).round();
                        (
                            Rect::new(rect.x, rect.y, rect.w, h1),
                            Rect::new(rect.x, rect.y + h1, rect.w, bar),
                            Rect::new(rect.x, rect.y + h1 + bar, rect.w, (avail - h1).max(0.0)),
                        )
                    }
                };
                g.dividers.push(Divider {
                    path: path.clone(),
                    dir: *direction,
                    area: rect,
                    bar: b,
                    ratio: *ratio,
                    first_leaf: first.first_leaf(),
                });
                path.push(false);
                first.geo(r1, bar, path, g);
                path.pop();
                path.push(true);
                second.geo(r2, bar, path, g);
                path.pop();
            }
        }
    }
}

impl Divider {
    /// Ratio for a pointer position (window px), keeping `min` px on both sides.
    pub fn ratio_for(&self, x: f32, y: f32, bar: f32, min: f32) -> f32 {
        let (pos, origin, total) = match self.dir {
            SplitDir::Horizontal => (x, self.area.x, self.area.w),
            SplitDir::Vertical => (y, self.area.y, self.area.h),
        };
        let avail = (total - bar).max(1.0);
        let lo = (min / avail).min(0.5);
        (((pos - origin - bar / 2.0) / avail).clamp(lo, 1.0 - lo) * 1000.0).round() / 1000.0
    }
}

/// Pane to focus when moving in a direction. Picks the nearest pane whose
/// extent overlaps along the other axis.
pub fn neighbor(panes: &[(PaneId, Rect)], from: PaneId, side: Side) -> Option<PaneId> {
    let (_, r) = panes.iter().find(|(p, _)| *p == from)?;
    let (cx, cy) = (r.x + r.w / 2.0, r.y + r.h / 2.0);
    let mut best: Option<(f32, f32, PaneId)> = None;
    for (p, o) in panes {
        if *p == from {
            continue;
        }
        let (gap, overlap) = match side {
            Side::Left => (r.x - o.right(), overlap_1d(r.y, r.bottom(), o.y, o.bottom())),
            Side::Right => (o.x - r.right(), overlap_1d(r.y, r.bottom(), o.y, o.bottom())),
            Side::Up => (r.y - o.bottom(), overlap_1d(r.x, r.right(), o.x, o.right())),
            Side::Down => (o.y - r.bottom(), overlap_1d(r.x, r.right(), o.x, o.right())),
        };
        if gap < -1.0 || overlap <= 0.0 {
            continue;
        }
        let ocx = o.x + o.w / 2.0;
        let ocy = o.y + o.h / 2.0;
        let off = match side {
            Side::Left | Side::Right => (ocy - cy).abs(),
            Side::Up | Side::Down => (ocx - cx).abs(),
        };
        let cand = (gap.max(0.0), off, *p);
        if best.map_or(true, |b| (cand.0, cand.1) < (b.0, b.1)) {
            best = Some(cand);
        }
    }
    best.map(|b| b.2)
}

fn overlap_1d(a0: f32, a1: f32, b0: f32, b1: f32) -> f32 {
    a1.min(b1) - a0.max(b0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p() -> PaneId {
        PaneId::new()
    }

    #[test]
    fn wire_shape_roundtrip() {
        let (a, b) = (p(), p());
        let t = PaneTree::Split {
            direction: SplitDir::Horizontal,
            ratio: 0.4,
            first: Box::new(PaneTree::leaf(a)),
            second: Box::new(PaneTree::leaf(b)),
        };
        let j = serde_json::to_value(&t).unwrap();
        assert_eq!(j["Split"]["direction"], "horizontal");
        assert!(j["Split"]["first"]["Leaf"]["pane_id"].is_string());
        let back: PaneTree = serde_json::from_value(j).unwrap();
        assert_eq!(back, t);
    }

    #[test]
    fn split_remove_focus() {
        let (a, b, c) = (p(), p(), p());
        let mut t = PaneTree::leaf(a);
        assert!(t.split_leaf(a, SplitDir::Horizontal, b));
        assert!(t.split_leaf(b, SplitDir::Vertical, c));
        assert_eq!(t.leaves(), vec![a, b, c]);
        let (t2, f) = t.remove_leaf(b).unwrap();
        assert_eq!(t2.leaves(), vec![a, c]);
        assert_eq!(f, c);
        let (t3, f) = t2.remove_leaf(c).unwrap();
        assert_eq!(t3, PaneTree::leaf(a));
        assert_eq!(f, a);
        assert!(t3.remove_leaf(a).is_none());
    }

    #[test]
    fn geometry_and_neighbors_2x2() {
        let (a, b, c, d) = (p(), p(), p(), p());
        let mut t = PaneTree::leaf(a);
        t.split_leaf(a, SplitDir::Horizontal, b);
        t.split_leaf(a, SplitDir::Vertical, c);
        t.split_leaf(b, SplitDir::Vertical, d);
        let g = t.geometry(Rect::new(0.0, 0.0, 1001.0, 801.0), 1.0);
        assert_eq!(g.panes.len(), 4);
        assert_eq!(g.dividers.len(), 3);
        let by = |id| g.panes.iter().find(|(p, _)| *p == id).unwrap().1;
        assert!(by(a).right() <= by(b).x);
        assert!(by(a).bottom() <= by(c).y);
        assert_eq!(neighbor(&g.panes, a, Side::Right), Some(b));
        assert_eq!(neighbor(&g.panes, a, Side::Down), Some(c));
        assert_eq!(neighbor(&g.panes, d, Side::Left), Some(c));
        assert_eq!(neighbor(&g.panes, d, Side::Up), Some(b));
        assert_eq!(neighbor(&g.panes, a, Side::Left), None);
        assert_eq!(neighbor(&g.panes, a, Side::Up), None);
    }

    #[test]
    fn divider_ratio_clamps() {
        let (a, b) = (p(), p());
        let mut t = PaneTree::leaf(a);
        t.split_leaf(a, SplitDir::Horizontal, b);
        let g = t.geometry(Rect::new(0.0, 0.0, 1001.0, 500.0), 1.0);
        let d = &g.dividers[0];
        let r = d.ratio_for(250.5, 10.0, 1.0, 100.0);
        assert!((r - 0.25).abs() < 0.01, "{r}");
        assert!(d.ratio_for(-50.0, 0.0, 1.0, 100.0) >= 0.09);
        assert!(d.ratio_for(5000.0, 0.0, 1.0, 100.0) <= 0.91);
        assert!(t.set_ratio_at(&d.path, 0.3));
        assert_eq!(t.ratio_at(&[]), Some(0.3));
        assert!(t.set_ratio(a, 0.7));
        assert_eq!(t.ratio_at(&[]), Some(0.7));
    }
}
