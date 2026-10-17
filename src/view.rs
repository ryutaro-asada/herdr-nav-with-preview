//! View-model for the tree: flattens the `workspace > tab > pane` tree into a
//! list of visible rows honoring per-node collapse state, and tracks the
//! selected row. All logic here is pure and unit-tested; the ratatui layer in
//! `tui.rs` only renders what this produces and forwards key events.

use std::collections::HashSet;

use crate::tree::Tree;

/// What kind of node a visible row refers to, plus the ids needed to act on it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RowKind {
    Workspace {
        workspace_id: String,
    },
    Tab {
        workspace_id: String,
        tab_id: String,
    },
    Pane {
        workspace_id: String,
        tab_id: String,
        pane_id: String,
    },
}

/// A single rendered row in the left tree pane.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    pub depth: usize,
    pub label: String,
    pub kind: RowKind,
    /// True when the node has children and can be expanded/collapsed.
    pub expandable: bool,
    /// True when the node is currently expanded.
    pub expanded: bool,
}

impl Row {
    /// The pane id this row resolves to for preview/focus, if any. Workspaces
    /// and tabs resolve to their first pane (the representative pane).
    pub fn representative_pane<'a>(&self, tree: &'a Tree) -> Option<&'a crate::tree::PaneNode> {
        match &self.kind {
            RowKind::Pane { pane_id, .. } => tree
                .workspaces
                .iter()
                .flat_map(|w| &w.tabs)
                .flat_map(|t| &t.panes)
                .find(|p| &p.pane_id == pane_id),
            RowKind::Tab { tab_id, .. } => tree
                .workspaces
                .iter()
                .flat_map(|w| &w.tabs)
                .find(|t| &t.tab_id == tab_id)
                .and_then(|t| t.panes.first()),
            RowKind::Workspace { workspace_id } => tree
                .workspaces
                .iter()
                .find(|w| &w.workspace_id == workspace_id)
                .and_then(|w| w.tabs.first())
                .and_then(|t| t.panes.first()),
        }
    }

    /// Build the focus plan for jumping to this row. For a workspace or tab we
    /// also target their representative pane so the jump lands as precisely as
    /// the backend allows.
    pub fn focus_plan(&self, tree: &Tree) -> crate::herdr_client::FocusPlan {
        use crate::herdr_client::FocusPlan;
        match &self.kind {
            RowKind::Pane {
                workspace_id,
                tab_id,
                pane_id,
            } => FocusPlan::pane(workspace_id, tab_id, pane_id),
            RowKind::Tab {
                workspace_id,
                tab_id,
            } => {
                let mut plan = FocusPlan::tab(workspace_id, tab_id);
                plan.pane_id = self.representative_pane(tree).map(|p| p.pane_id.clone());
                plan
            }
            RowKind::Workspace { workspace_id } => {
                let mut plan = FocusPlan::workspace(workspace_id);
                if let Some(p) = self.representative_pane(tree) {
                    plan.tab_id = Some(p.tab_id.clone());
                    plan.pane_id = Some(p.pane_id.clone());
                }
                plan
            }
        }
    }
}

/// A unique key for a collapsible node (workspace or tab).
fn ws_key(id: &str) -> String {
    format!("w:{id}")
}
fn tab_key(id: &str) -> String {
    format!("t:{id}")
}

/// Compute the first visible row index so that `selected` stays within a
/// window of `height` rows out of `total`. Pure and unit-tested.
pub fn scroll_offset(selected: usize, total: usize, height: usize) -> usize {
    if height == 0 || total <= height {
        return 0;
    }
    let max_offset = total - height;
    // Keep the selection on screen: if it would fall below the window, scroll
    // down just enough; the window never scrolls past the last row.
    if selected < height {
        0
    } else {
        (selected + 1 - height).min(max_offset)
    }
}

pub struct TreeView {
    tree: Tree,
    /// Keys of nodes that are collapsed. Default (absent) == expanded.
    collapsed: HashSet<String>,
    selected: usize,
}

impl TreeView {
    pub fn new(tree: Tree) -> Self {
        Self {
            tree,
            collapsed: HashSet::new(),
            selected: 0,
        }
    }

    pub fn tree(&self) -> &Tree {
        &self.tree
    }

    pub fn selected_index(&self) -> usize {
        self.selected
    }

    /// Replace the underlying tree (e.g. after a refresh), clamping selection.
    #[allow(dead_code)]
    pub fn set_tree(&mut self, tree: Tree) {
        self.tree = tree;
        let max = self.rows().len().saturating_sub(1);
        if self.selected > max {
            self.selected = max;
        }
    }

    fn is_collapsed(&self, key: &str) -> bool {
        self.collapsed.contains(key)
    }

    /// Compute the currently visible rows given collapse state.
    pub fn rows(&self) -> Vec<Row> {
        let mut rows = Vec::new();
        for w in &self.tree.workspaces {
            let wkey = ws_key(&w.workspace_id);
            let w_collapsed = self.is_collapsed(&wkey);
            let marker = if w.focused { " *" } else { "" };
            rows.push(Row {
                depth: 0,
                label: format!("{} [{}]{}", w.workspace_id, w.label, marker),
                kind: RowKind::Workspace {
                    workspace_id: w.workspace_id.clone(),
                },
                expandable: !w.tabs.is_empty(),
                expanded: !w_collapsed,
            });
            if w_collapsed {
                continue;
            }
            for t in &w.tabs {
                let tkey = tab_key(&t.tab_id);
                let t_collapsed = self.is_collapsed(&tkey);
                rows.push(Row {
                    depth: 1,
                    label: format!("{} ({})", t.tab_id, t.label),
                    kind: RowKind::Tab {
                        workspace_id: t.workspace_id.clone(),
                        tab_id: t.tab_id.clone(),
                    },
                    expandable: !t.panes.is_empty(),
                    expanded: !t_collapsed,
                });
                if t_collapsed {
                    continue;
                }
                for p in &t.panes {
                    let agent = p
                        .agent
                        .as_deref()
                        .map(|a| format!(" <{a}>"))
                        .unwrap_or_default();
                    rows.push(Row {
                        depth: 2,
                        label: format!("{}{}", p.title, agent),
                        kind: RowKind::Pane {
                            workspace_id: p.workspace_id.clone(),
                            tab_id: p.tab_id.clone(),
                            pane_id: p.pane_id.clone(),
                        },
                        expandable: false,
                        expanded: false,
                    });
                }
            }
        }
        rows
    }

    pub fn selected_row(&self) -> Option<Row> {
        self.rows().into_iter().nth(self.selected)
    }

    pub fn move_down(&mut self) {
        let len = self.rows().len();
        if len == 0 {
            return;
        }
        if self.selected + 1 < len {
            self.selected += 1;
        }
    }

    pub fn move_up(&mut self) {
        if self.selected > 0 {
            self.selected -= 1;
        }
    }

    fn selected_key(&self) -> Option<String> {
        self.selected_row().and_then(|r| match r.kind {
            RowKind::Workspace { workspace_id } => Some(ws_key(&workspace_id)),
            RowKind::Tab { tab_id, .. } => Some(tab_key(&tab_id)),
            RowKind::Pane { .. } => None,
        })
    }

    /// Collapse the selected node (left arrow). On a pane, no-op.
    pub fn collapse(&mut self) {
        if let Some(key) = self.selected_key() {
            self.collapsed.insert(key);
        }
    }

    /// Expand the selected node (right arrow). On a pane, no-op.
    pub fn expand(&mut self) {
        if let Some(key) = self.selected_key() {
            self.collapsed.remove(&key);
        }
    }

    /// Toggle expand/collapse of the selected node.
    pub fn toggle(&mut self) {
        if let Some(key) = self.selected_key() {
            if self.collapsed.contains(&key) {
                self.collapsed.remove(&key);
            } else {
                self.collapsed.insert(key);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::herdr_client::{Pane, Tab, Workspace};

    fn sample_tree() -> Tree {
        let ws = vec![Workspace {
            workspace_id: "w1".into(),
            label: "proj".into(),
            active_tab_id: None,
            tab_count: 2,
            pane_count: 2,
            focused: true,
            agent_status: None,
        }];
        let tabs = vec![
            Tab {
                tab_id: "w1:t1".into(),
                workspace_id: "w1".into(),
                label: "a".into(),
                pane_count: 1,
                focused: false,
                agent_status: None,
            },
            Tab {
                tab_id: "w1:t2".into(),
                workspace_id: "w1".into(),
                label: "b".into(),
                pane_count: 1,
                focused: false,
                agent_status: None,
            },
        ];
        let panes = vec![
            Pane {
                pane_id: "w1:p1".into(),
                tab_id: "w1:t1".into(),
                workspace_id: "w1".into(),
                terminal_title_stripped: Some("shell-a".into()),
                terminal_title: None,
                agent: None,
                agent_status: None,
                focused: false,
                cwd: None,
            },
            Pane {
                pane_id: "w1:p2".into(),
                tab_id: "w1:t2".into(),
                workspace_id: "w1".into(),
                terminal_title_stripped: Some("shell-b".into()),
                terminal_title: None,
                agent: Some("claude".into()),
                agent_status: None,
                focused: false,
                cwd: None,
            },
        ];
        Tree::build(ws, tabs, panes)
    }

    #[test]
    fn flattens_fully_expanded() {
        let view = TreeView::new(sample_tree());
        let rows = view.rows();
        // 1 workspace + 2 tabs + 2 panes = 5 rows.
        assert_eq!(rows.len(), 5);
        assert_eq!(rows[0].depth, 0);
        assert!(rows[0].label.contains("w1 [proj] *"));
        assert_eq!(rows[1].depth, 1);
        assert_eq!(rows[2].depth, 2);
        assert!(rows[2].label.contains("shell-a"));
        assert!(rows[4].label.contains("<claude>"));
    }

    #[test]
    fn collapse_workspace_hides_descendants() {
        let mut view = TreeView::new(sample_tree());
        // Select workspace row (index 0) and collapse.
        view.collapse();
        let rows = view.rows();
        assert_eq!(rows.len(), 1);
        assert!(!rows[0].expanded);
    }

    #[test]
    fn collapse_tab_hides_only_its_panes() {
        let mut view = TreeView::new(sample_tree());
        // Move to first tab (index 1) and collapse it.
        view.move_down();
        view.collapse();
        let rows = view.rows();
        // workspace, tab1(collapsed), tab2, pane2 = 4 rows.
        assert_eq!(rows.len(), 4);
        assert_eq!(rows[1].kind, RowKind::Tab { workspace_id: "w1".into(), tab_id: "w1:t1".into() });
        assert!(!rows[1].expanded);
    }

    #[test]
    fn navigation_clamps() {
        let mut view = TreeView::new(sample_tree());
        view.move_up(); // already at top
        assert_eq!(view.selected_index(), 0);
        for _ in 0..10 {
            view.move_down();
        }
        assert_eq!(view.selected_index(), 4); // last of 5 rows
    }

    #[test]
    fn toggle_round_trips() {
        let mut view = TreeView::new(sample_tree());
        assert_eq!(view.rows().len(), 5);
        view.toggle(); // collapse workspace
        assert_eq!(view.rows().len(), 1);
        view.toggle(); // expand again
        assert_eq!(view.rows().len(), 5);
    }

    #[test]
    fn representative_pane_resolution() {
        let view = TreeView::new(sample_tree());
        let rows = view.rows();
        // Workspace row -> first pane.
        assert_eq!(
            rows[0].representative_pane(view.tree()).unwrap().pane_id,
            "w1:p1"
        );
        // Tab2 row -> its pane.
        assert_eq!(
            rows[3].representative_pane(view.tree()).unwrap().pane_id,
            "w1:p2"
        );
        // Pane row -> itself.
        assert_eq!(
            rows[2].representative_pane(view.tree()).unwrap().pane_id,
            "w1:p1"
        );
    }

    #[test]
    fn set_tree_clamps_selection() {
        let mut view = TreeView::new(sample_tree());
        for _ in 0..4 {
            view.move_down();
        }
        assert_eq!(view.selected_index(), 4);
        // Replace with an empty tree; selection must clamp to 0.
        view.set_tree(Tree::default());
        assert_eq!(view.selected_index(), 0);
    }

    #[test]
    fn focus_plan_for_each_row_kind() {
        let view = TreeView::new(sample_tree());
        let rows = view.rows();
        let tree = view.tree();

        // Workspace row -> workspace + its first tab/pane.
        let wp = rows[0].focus_plan(tree);
        assert_eq!(wp.workspace_id.as_deref(), Some("w1"));
        assert_eq!(wp.tab_id.as_deref(), Some("w1:t1"));
        assert_eq!(wp.pane_id.as_deref(), Some("w1:p1"));

        // Tab row (index 3 is w1:t2) -> workspace + tab + its pane.
        let tp = rows[3].focus_plan(tree);
        assert_eq!(tp.workspace_id.as_deref(), Some("w1"));
        assert_eq!(tp.tab_id.as_deref(), Some("w1:t2"));
        assert_eq!(tp.pane_id.as_deref(), Some("w1:p2"));

        // Pane row (index 2 is w1:p1) -> full triple.
        let pp = rows[2].focus_plan(tree);
        assert_eq!(pp.workspace_id.as_deref(), Some("w1"));
        assert_eq!(pp.tab_id.as_deref(), Some("w1:t1"));
        assert_eq!(pp.pane_id.as_deref(), Some("w1:p1"));
    }

    #[test]
    fn scroll_offset_behaviour() {
        // Everything fits: no scroll.
        assert_eq!(scroll_offset(0, 5, 10), 0);
        assert_eq!(scroll_offset(4, 5, 5), 0);
        // Zero height is a safe no-op.
        assert_eq!(scroll_offset(3, 10, 0), 0);
        // Selection within the first window: offset 0.
        assert_eq!(scroll_offset(2, 20, 5), 0);
        assert_eq!(scroll_offset(4, 20, 5), 0);
        // Selection past the window: scroll so selection is the last visible row.
        assert_eq!(scroll_offset(5, 20, 5), 1);
        assert_eq!(scroll_offset(9, 20, 5), 5);
        // Never scroll past the end.
        assert_eq!(scroll_offset(19, 20, 5), 15);
    }
}
