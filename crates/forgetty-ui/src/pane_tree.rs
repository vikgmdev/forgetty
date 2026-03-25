//! Pane tree layout for split panes.
//!
//! Manages the binary tree structure used for horizontal and vertical
//! pane splits within a tab.

use crate::pane::PaneId;

/// Direction of a split.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum SplitDirection {
    /// Side by side (left | right).
    Horizontal,
    /// Top and bottom (top / bottom).
    Vertical,
}

/// A rectangle in pixel coordinates.
#[derive(Debug, Clone, Copy)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

/// A node in the binary pane tree.
#[derive(Debug, Clone)]
pub enum PaneNode {
    /// A leaf node containing a single pane.
    Leaf(PaneId),
    /// A split node containing two children.
    Split { direction: SplitDirection, ratio: f32, first: Box<PaneNode>, second: Box<PaneNode> },
}

impl PaneNode {
    /// Calculate layout rectangles for all panes given a bounding rect.
    pub fn layout(&self, rect: Rect) -> Vec<(PaneId, Rect)> {
        let mut result = Vec::new();
        self.layout_inner(rect, &mut result);
        result
    }

    fn layout_inner(&self, rect: Rect, out: &mut Vec<(PaneId, Rect)>) {
        match self {
            PaneNode::Leaf(id) => {
                out.push((*id, rect));
            }
            PaneNode::Split { direction, ratio, first, second } => match direction {
                SplitDirection::Horizontal => {
                    let first_width = rect.width * ratio;
                    let second_width = rect.width - first_width;
                    first.layout_inner(
                        Rect { x: rect.x, y: rect.y, width: first_width, height: rect.height },
                        out,
                    );
                    second.layout_inner(
                        Rect {
                            x: rect.x + first_width,
                            y: rect.y,
                            width: second_width,
                            height: rect.height,
                        },
                        out,
                    );
                }
                SplitDirection::Vertical => {
                    let first_height = rect.height * ratio;
                    let second_height = rect.height - first_height;
                    first.layout_inner(
                        Rect { x: rect.x, y: rect.y, width: rect.width, height: first_height },
                        out,
                    );
                    second.layout_inner(
                        Rect {
                            x: rect.x,
                            y: rect.y + first_height,
                            width: rect.width,
                            height: second_height,
                        },
                        out,
                    );
                }
            },
        }
    }

    /// Split a leaf pane into two. The target pane becomes the first child,
    /// and the new pane becomes the second child.
    /// Returns true if the target was found and split.
    pub fn split(&mut self, target: PaneId, direction: SplitDirection, new_pane: PaneId) -> bool {
        match self {
            PaneNode::Leaf(id) if *id == target => {
                let old = PaneNode::Leaf(*id);
                let new = PaneNode::Leaf(new_pane);
                *self = PaneNode::Split {
                    direction,
                    ratio: 0.5,
                    first: Box::new(old),
                    second: Box::new(new),
                };
                true
            }
            PaneNode::Leaf(_) => false,
            PaneNode::Split { first, second, .. } => {
                first.split(target, direction, new_pane)
                    || second.split(target, direction, new_pane)
            }
        }
    }

    /// Remove a pane from the tree. The sibling of the removed pane replaces
    /// its parent split node.
    /// Returns true if the pane was found and removed.
    /// Returns false if the pane is the only leaf (root leaf).
    pub fn remove(&mut self, target: PaneId) -> bool {
        match self {
            PaneNode::Leaf(_) => false,
            PaneNode::Split { first, second, .. } => {
                // Check if one of the direct children is the target leaf.
                if matches!(first.as_ref(), PaneNode::Leaf(id) if *id == target) {
                    *self = *second.clone();
                    return true;
                }
                if matches!(second.as_ref(), PaneNode::Leaf(id) if *id == target) {
                    *self = *first.clone();
                    return true;
                }
                // Recurse.
                first.remove(target) || second.remove(target)
            }
        }
    }

    /// Find an adjacent pane in a given direction from a source pane.
    ///
    /// For Horizontal direction: forward=true means right, forward=false means left.
    /// For Vertical direction: forward=true means down, forward=false means up.
    pub fn find_adjacent(
        &self,
        from: PaneId,
        direction: SplitDirection,
        forward: bool,
    ) -> Option<PaneId> {
        // Strategy: find the path to `from`, then walk up looking for a split
        // in the matching direction where `from` is in the side we'd move away from.
        let path = self.path_to(from)?;
        // Walk up the path from deepest to shallowest.
        for i in (0..path.len()).rev() {
            let node = self.node_at_path(&path[..i])?;
            if let PaneNode::Split { direction: d, first, second, .. } = node {
                if *d == direction {
                    let in_first = first.contains(from);
                    if forward && in_first {
                        // Move from first to second; pick the nearest leaf.
                        return Some(second.first_leaf());
                    } else if !forward && !in_first {
                        // Move from second to first; pick the last leaf.
                        return Some(first.last_leaf());
                    }
                }
            }
        }
        None
    }

    /// Get all leaf pane IDs in order.
    pub fn pane_ids(&self) -> Vec<PaneId> {
        let mut result = Vec::new();
        self.collect_ids(&mut result);
        result
    }

    fn collect_ids(&self, out: &mut Vec<PaneId>) {
        match self {
            PaneNode::Leaf(id) => out.push(*id),
            PaneNode::Split { first, second, .. } => {
                first.collect_ids(out);
                second.collect_ids(out);
            }
        }
    }

    /// Check if this subtree contains a pane.
    pub fn contains(&self, id: PaneId) -> bool {
        match self {
            PaneNode::Leaf(leaf_id) => *leaf_id == id,
            PaneNode::Split { first, second, .. } => first.contains(id) || second.contains(id),
        }
    }

    /// Get the first (leftmost/topmost) leaf.
    fn first_leaf(&self) -> PaneId {
        match self {
            PaneNode::Leaf(id) => *id,
            PaneNode::Split { first, .. } => first.first_leaf(),
        }
    }

    /// Get the last (rightmost/bottommost) leaf.
    fn last_leaf(&self) -> PaneId {
        match self {
            PaneNode::Leaf(id) => *id,
            PaneNode::Split { second, .. } => second.last_leaf(),
        }
    }

    /// Build a path to a pane. Each element is 0 (first) or 1 (second).
    fn path_to(&self, target: PaneId) -> Option<Vec<u8>> {
        match self {
            PaneNode::Leaf(id) => {
                if *id == target {
                    Some(Vec::new())
                } else {
                    None
                }
            }
            PaneNode::Split { first, second, .. } => {
                if let Some(mut path) = first.path_to(target) {
                    path.insert(0, 0);
                    Some(path)
                } else if let Some(mut path) = second.path_to(target) {
                    path.insert(0, 1);
                    Some(path)
                } else {
                    None
                }
            }
        }
    }

    /// Get the node at a given path.
    fn node_at_path(&self, path: &[u8]) -> Option<&PaneNode> {
        if path.is_empty() {
            return Some(self);
        }
        match self {
            PaneNode::Leaf(_) => None,
            PaneNode::Split { first, second, .. } => match path[0] {
                0 => first.node_at_path(&path[1..]),
                1 => second.node_at_path(&path[1..]),
                _ => None,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pid(n: u64) -> PaneId {
        PaneId(n)
    }

    fn full_rect() -> Rect {
        Rect { x: 0.0, y: 0.0, width: 1000.0, height: 800.0 }
    }

    #[test]
    fn test_single_leaf_layout() {
        let tree = PaneNode::Leaf(pid(1));
        let layout = tree.layout(full_rect());
        assert_eq!(layout.len(), 1);
        assert_eq!(layout[0].0, pid(1));
        assert!((layout[0].1.width - 1000.0).abs() < f32::EPSILON);
        assert!((layout[0].1.height - 800.0).abs() < f32::EPSILON);
    }

    #[test]
    fn test_horizontal_split_layout() {
        let tree = PaneNode::Split {
            direction: SplitDirection::Horizontal,
            ratio: 0.5,
            first: Box::new(PaneNode::Leaf(pid(1))),
            second: Box::new(PaneNode::Leaf(pid(2))),
        };
        let layout = tree.layout(full_rect());
        assert_eq!(layout.len(), 2);

        // First pane: left half
        assert_eq!(layout[0].0, pid(1));
        assert!((layout[0].1.x - 0.0).abs() < f32::EPSILON);
        assert!((layout[0].1.width - 500.0).abs() < f32::EPSILON);

        // Second pane: right half
        assert_eq!(layout[1].0, pid(2));
        assert!((layout[1].1.x - 500.0).abs() < f32::EPSILON);
        assert!((layout[1].1.width - 500.0).abs() < f32::EPSILON);
    }

    #[test]
    fn test_vertical_split_layout() {
        let tree = PaneNode::Split {
            direction: SplitDirection::Vertical,
            ratio: 0.5,
            first: Box::new(PaneNode::Leaf(pid(1))),
            second: Box::new(PaneNode::Leaf(pid(2))),
        };
        let layout = tree.layout(full_rect());
        assert_eq!(layout.len(), 2);

        assert_eq!(layout[0].0, pid(1));
        assert!((layout[0].1.y - 0.0).abs() < f32::EPSILON);
        assert!((layout[0].1.height - 400.0).abs() < f32::EPSILON);

        assert_eq!(layout[1].0, pid(2));
        assert!((layout[1].1.y - 400.0).abs() < f32::EPSILON);
        assert!((layout[1].1.height - 400.0).abs() < f32::EPSILON);
    }

    #[test]
    fn test_nested_split_layout() {
        // Horizontal split, with the right side split vertically.
        //  [1] | [2]
        //      | [3]
        let mut tree = PaneNode::Leaf(pid(1));
        assert!(tree.split(pid(1), SplitDirection::Horizontal, pid(2)));
        assert!(tree.split(pid(2), SplitDirection::Vertical, pid(3)));

        let layout = tree.layout(full_rect());
        assert_eq!(layout.len(), 3);

        // Pane 1: left half, full height
        assert_eq!(layout[0].0, pid(1));
        assert!((layout[0].1.width - 500.0).abs() < f32::EPSILON);
        assert!((layout[0].1.height - 800.0).abs() < f32::EPSILON);

        // Pane 2: right half, top half
        assert_eq!(layout[1].0, pid(2));
        assert!((layout[1].1.x - 500.0).abs() < f32::EPSILON);
        assert!((layout[1].1.width - 500.0).abs() < f32::EPSILON);
        assert!((layout[1].1.height - 400.0).abs() < f32::EPSILON);

        // Pane 3: right half, bottom half
        assert_eq!(layout[2].0, pid(3));
        assert!((layout[2].1.x - 500.0).abs() < f32::EPSILON);
        assert!((layout[2].1.y - 400.0).abs() < f32::EPSILON);
    }

    #[test]
    fn test_split_leaf() {
        let mut tree = PaneNode::Leaf(pid(1));
        assert!(tree.split(pid(1), SplitDirection::Horizontal, pid(2)));

        let ids = tree.pane_ids();
        assert_eq!(ids.len(), 2);
        assert!(ids.contains(&pid(1)));
        assert!(ids.contains(&pid(2)));
    }

    #[test]
    fn test_split_nonexistent() {
        let mut tree = PaneNode::Leaf(pid(1));
        assert!(!tree.split(pid(99), SplitDirection::Horizontal, pid(2)));
    }

    #[test]
    fn test_remove_from_split() {
        let mut tree = PaneNode::Leaf(pid(1));
        tree.split(pid(1), SplitDirection::Horizontal, pid(2));

        // Remove pane 2; tree should collapse back to just pane 1.
        assert!(tree.remove(pid(2)));
        assert_eq!(tree.pane_ids(), vec![pid(1)]);
    }

    #[test]
    fn test_remove_from_root_leaf() {
        let mut tree = PaneNode::Leaf(pid(1));
        // Cannot remove the only leaf.
        assert!(!tree.remove(pid(1)));
    }

    #[test]
    fn test_remove_first_child() {
        let mut tree = PaneNode::Leaf(pid(1));
        tree.split(pid(1), SplitDirection::Horizontal, pid(2));

        assert!(tree.remove(pid(1)));
        assert_eq!(tree.pane_ids(), vec![pid(2)]);
    }

    #[test]
    fn test_find_adjacent_horizontal() {
        // [1] | [2]
        let mut tree = PaneNode::Leaf(pid(1));
        tree.split(pid(1), SplitDirection::Horizontal, pid(2));

        // From pane 1, go right -> pane 2
        assert_eq!(tree.find_adjacent(pid(1), SplitDirection::Horizontal, true), Some(pid(2)));
        // From pane 2, go left -> pane 1
        assert_eq!(tree.find_adjacent(pid(2), SplitDirection::Horizontal, false), Some(pid(1)));
        // From pane 1, go left -> None (no pane to the left)
        assert_eq!(tree.find_adjacent(pid(1), SplitDirection::Horizontal, false), None);
        // From pane 2, go right -> None
        assert_eq!(tree.find_adjacent(pid(2), SplitDirection::Horizontal, true), None);
    }

    #[test]
    fn test_find_adjacent_vertical() {
        // [1]
        // ---
        // [2]
        let mut tree = PaneNode::Leaf(pid(1));
        tree.split(pid(1), SplitDirection::Vertical, pid(2));

        assert_eq!(tree.find_adjacent(pid(1), SplitDirection::Vertical, true), Some(pid(2)));
        assert_eq!(tree.find_adjacent(pid(2), SplitDirection::Vertical, false), Some(pid(1)));
    }

    #[test]
    fn test_find_adjacent_cross_direction() {
        // [1] | [2]  — horizontal split
        // Looking for vertical adjacency should find nothing.
        let mut tree = PaneNode::Leaf(pid(1));
        tree.split(pid(1), SplitDirection::Horizontal, pid(2));

        assert_eq!(tree.find_adjacent(pid(1), SplitDirection::Vertical, true), None);
    }

    #[test]
    fn test_pane_ids() {
        let mut tree = PaneNode::Leaf(pid(1));
        tree.split(pid(1), SplitDirection::Horizontal, pid(2));
        tree.split(pid(2), SplitDirection::Vertical, pid(3));

        let ids = tree.pane_ids();
        assert_eq!(ids, vec![pid(1), pid(2), pid(3)]);
    }
}
