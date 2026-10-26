//! Rules of the shell's tab, workspace and pane operations, with no UI. The shell turns the
//! answers into backend calls. Same behavior as GTK (`app.rs`) unless a line says otherwise.

use uuid::Uuid;

/// Tabs "Close Tabs to the Right" closes: every tab after `index`. Empty for the last tab.
pub fn tabs_right_of(tabs: &[Uuid], index: usize) -> Vec<Uuid> {
    tabs.iter().skip(index + 1).copied().collect()
}

/// Tabs "Close Other Tabs" closes: all but `index`.
pub fn tabs_other_than(tabs: &[Uuid], index: usize) -> Vec<Uuid> {
    tabs.iter().enumerate().filter(|(i, _)| *i != index).map(|(_, t)| *t).collect()
}

/// The two workspace positions "Move up" and "Move down" swap. `None` at either end.
pub fn workspace_swap(index: usize, count: usize, up: bool) -> Option<(usize, usize)> {
    if index >= count {
        return None;
    }
    if up {
        index.checked_sub(1).map(|to| (index, to))
    } else if index + 1 < count {
        Some((index, index + 1))
    } else {
        None
    }
}

/// Deleting a workspace with more than one tab asks first (GTK, AC-31). The last workspace
/// cannot be deleted at all.
pub fn delete_workspace_asks(tab_count: usize) -> bool {
    tab_count > 1
}

pub fn can_delete_workspace(workspace_count: usize) -> bool {
    workspace_count > 1
}

/// The words of the delete confirmation.
pub fn delete_workspace_text(name: &str, tab_count: usize) -> String {
    format!(
        "Delete \"{}\"? This workspace has {tab_count} tabs. Closing it will kill all running shells.",
        crate::notify::sanitize(name, 60)
    )
}

/// The window title. One workspace: the tab's title and "Forgetty" (no workspace name, GTK AC-09).
/// Several: the workspace name first. Windows keeps its own "- Forgetty" ending, GTK shows the
/// bare pane title.
pub fn window_title(workspace_count: usize, workspace: &str, tab: Option<&str>) -> String {
    let ws = workspace.trim();
    match (workspace_count > 1 && !ws.is_empty(), tab.map(str::trim).filter(|t| !t.is_empty())) {
        (false, Some(t)) => format!("{t} - Forgetty"),
        (false, None) => "Forgetty".to_string(),
        (true, Some(t)) => format!("{ws} - {t} - Forgetty"),
        (true, None) => format!("{ws} - Forgetty"),
    }
}

/// A bell or a notification from a pane raises the pane's ring and its tab's badge unless the
/// user is looking at that pane (GTK: the pane has no keyboard focus). With the window in the
/// background no pane is being looked at.
pub fn raises_attention(window_active: bool, pane_focused: bool) -> bool {
    !(window_active && pane_focused)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ids(n: usize) -> Vec<Uuid> {
        (0..n).map(|_| Uuid::new_v4()).collect()
    }

    #[test]
    fn close_right_and_others() {
        let t = ids(4);
        assert_eq!(tabs_right_of(&t, 1), vec![t[2], t[3]]);
        assert!(tabs_right_of(&t, 3).is_empty());
        assert!(tabs_right_of(&t, 9).is_empty());
        assert_eq!(tabs_other_than(&t, 2), vec![t[0], t[1], t[3]]);
        assert!(tabs_other_than(&t[..1], 0).is_empty());
    }

    #[test]
    fn workspace_move_ends() {
        assert_eq!(workspace_swap(1, 3, true), Some((1, 0)));
        assert_eq!(workspace_swap(1, 3, false), Some((1, 2)));
        assert_eq!(workspace_swap(0, 3, true), None);
        assert_eq!(workspace_swap(2, 3, false), None);
        assert_eq!(workspace_swap(0, 1, false), None);
        assert_eq!(workspace_swap(5, 3, true), None);
    }

    #[test]
    fn delete_rules() {
        assert!(!can_delete_workspace(1));
        assert!(can_delete_workspace(2));
        assert!(!delete_workspace_asks(1));
        assert!(delete_workspace_asks(2));
        let t = delete_workspace_text("work\u{202e}", 3);
        assert!(t.contains("\"work\"") && t.contains("3 tabs") && !t.contains('\u{202e}'), "{t}");
    }

    #[test]
    fn title_names_the_workspace_only_when_there_are_several() {
        assert_eq!(window_title(1, "Default", Some("cmd")), "cmd - Forgetty");
        assert_eq!(window_title(1, "Default", None), "Forgetty");
        assert_eq!(window_title(2, "work", Some("cmd")), "work - cmd - Forgetty");
        assert_eq!(window_title(2, "work", None), "work - Forgetty");
        assert_eq!(window_title(2, "  ", Some("cmd")), "cmd - Forgetty");
        assert_eq!(window_title(3, "work", Some("  ")), "work - Forgetty");
    }

    #[test]
    fn attention_only_when_nobody_looks() {
        assert!(!raises_attention(true, true));
        assert!(raises_attention(true, false));
        assert!(raises_attention(false, true));
        assert!(raises_attention(false, false));
    }
}
