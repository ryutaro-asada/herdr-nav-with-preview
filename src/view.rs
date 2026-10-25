//! View-model for the tree: flattens the `workspace > tab > pane` tree into a
//! list of visible rows honoring per-node collapse state, and tracks the
//! selected row. All logic here is pure and unit-tested; the ratatui layer in
//! `tui.rs` only renders what this produces and forwards key events.

use std::collections::HashSet;

use crate::tree::{PaneNode, TabNode, Tree, WorkspaceNode};

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

impl RowKind {
    /// The workspace this node belongs to.
    pub fn workspace_id(&self) -> &str {
        match self {
            RowKind::Workspace { workspace_id }
            | RowKind::Tab { workspace_id, .. }
            | RowKind::Pane { workspace_id, .. } => workspace_id,
        }
    }

    /// The node's own id: the workspace, tab, or pane id.
    pub fn id(&self) -> &str {
        match self {
            RowKind::Workspace { workspace_id } => workspace_id,
            RowKind::Tab { tab_id, .. } => tab_id,
            RowKind::Pane { pane_id, .. } => pane_id,
        }
    }

    /// Every id on the path to this node, workspace first.
    fn ids(&self) -> Vec<&str> {
        match self {
            RowKind::Workspace { workspace_id } => vec![workspace_id],
            RowKind::Tab {
                workspace_id,
                tab_id,
            } => vec![workspace_id, tab_id],
            RowKind::Pane {
                workspace_id,
                tab_id,
                pane_id,
            } => vec![workspace_id, tab_id, pane_id],
        }
    }
}

/// A single rendered row in the left tree pane.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    pub depth: usize,
    /// Clean display name (no internal ids): workspace/tab label or pane title.
    pub name: String,
    pub kind: RowKind,
    /// True when the node has children and can be expanded/collapsed.
    pub expandable: bool,
    /// True when the node is currently expanded.
    pub expanded: bool,
    /// Agent status for the node (`working`/`idle`/`blocked`/`done`/`unknown`).
    pub agent_status: Option<String>,
    /// Agent kind badge for pane rows (e.g. `claude`).
    pub agent: Option<String>,
    /// True when this node is the focused workspace.
    pub focused: bool,
    /// True when this row is the last child under its parent (for tree glyphs).
    pub is_last: bool,
    /// For each ancestor level (excluding this node), whether that ancestor was
    /// the last child. Used to draw continuous vertical tree lines: a level
    /// draws `│ ` when the ancestor still has siblings below, or `  ` when it
    /// was the last child.
    pub ancestors_last: Vec<bool>,
}

impl Row {
    /// The pane id this row resolves to for preview/focus, if any. Workspaces
    /// and tabs resolve to their first pane (the representative pane).
    pub fn representative_pane<'a>(&self, tree: &'a Tree) -> Option<&'a PaneNode> {
        match &self.kind {
            RowKind::Pane { pane_id, .. } => tree.panes().find(|p| &p.pane_id == pane_id),
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
    /// also target their representative pane. The pane-level step uses
    /// `agent focus`, which only works when an agent is present, so we only set
    /// `pane_id` for panes that actually host an agent; plain shells rely on
    /// workspace + tab focus (which already brings the pane forward).
    pub fn focus_plan(&self, tree: &Tree) -> crate::herdr_client::FocusPlan {
        use crate::herdr_client::FocusPlan;

        let rep = self.representative_pane(tree);
        let agent_pane = rep
            .filter(|p| p.agent.as_deref().is_some_and(|a| !a.is_empty()))
            .map(|p| p.pane_id.clone());
        let mut plan = match &self.kind {
            RowKind::Pane {
                workspace_id,
                tab_id,
                ..
            }
            | RowKind::Tab {
                workspace_id,
                tab_id,
            } => FocusPlan::tab(workspace_id, tab_id),
            RowKind::Workspace { workspace_id } => {
                let mut plan = FocusPlan::workspace(workspace_id);
                plan.tab_id = rep.map(|p| p.tab_id.clone());
                plan
            }
        };
        plan.pane_id = agent_pane;
        plan
    }
}

/// A unique key for a collapsible node (workspace or tab).
fn ws_key(id: &str) -> String {
    format!("w:{id}")
}
fn tab_key(id: &str) -> String {
    format!("t:{id}")
}

/// Compute the first visible row index for a window of `height` rows out of
/// `total`, keeping `selected` visible while scrolling the *minimum* amount
/// from `prev` (the previous offset). This makes scrolling sticky: the window
/// only moves when the selection would leave it, so moving up one row doesn't
/// drag the whole view and clip the bottom. Pure and unit-tested.
pub fn scroll_offset(selected: usize, total: usize, height: usize, prev: usize) -> usize {
    if height == 0 || total <= height {
        return 0;
    }
    let max_offset = total - height;
    let mut offset = prev.min(max_offset);
    if selected < offset {
        // Selection moved above the window: reveal it at the top.
        offset = selected;
    } else if selected >= offset + height {
        // Selection moved below the window: reveal it at the bottom.
        offset = selected + 1 - height;
    }
    offset.min(max_offset)
}

/// Does a row match the search query? Case-insensitive substring match against
/// the display name, the agent kind, and the kind's ids (so you can search by
/// workspace/tab/pane id too). Empty query matches everything.
pub fn row_matches_query(row: &Row, query: &str) -> bool {
    let q = query.trim().to_lowercase();
    if q.is_empty() {
        return true;
    }
    let mut hay = row.name.to_lowercase();
    for part in row.agent.as_deref().into_iter().chain(row.kind.ids()) {
        hay.push(' ');
        hay.push_str(&part.to_lowercase());
    }
    hay.contains(&q)
}

/// Filter a flattened (pre-order, depth 0/1/2) row list to those matching
/// `query`, keeping the ancestors needed to reach each match. A workspace or
/// tab is kept if it matches directly or has a kept descendant.
pub fn filter_rows(all: &[Row], query: &str) -> Vec<Row> {
    if query.trim().is_empty() {
        return all.to_vec();
    }
    let n = all.len();
    let mut keep = vec![false; n];
    for (i, row) in all.iter().enumerate() {
        if row_matches_query(row, query) {
            keep[i] = true;
            // Keep ancestors: the nearest preceding rows with smaller depth.
            let mut need = row.depth;
            let mut j = i;
            while need > 0 && j > 0 {
                j -= 1;
                if all[j].depth < need {
                    keep[j] = true;
                    need = all[j].depth;
                }
            }
        }
    }
    all.iter()
        .zip(keep)
        .filter(|&(_, k)| k)
        .map(|(r, _)| r.clone())
        .collect()
}

/// Does `label` already lead with a number plus a separator (e.g.
/// herdr-auto-title's `2 · api`, or `2. api`)? Then we don't prefix the tab
/// number: such labels carry their own (position) number, which can differ
/// from herdr's internal tab number. Pure.
fn has_number_prefix(label: &str) -> bool {
    let rest = label.trim_start_matches(|c: char| c.is_ascii_digit());
    rest.len() < label.len() && (rest.starts_with(" ·") || rest.starts_with(". "))
}

/// Workspace row name: `number  label` (the id when the label is blank),
/// plus the git branch in parentheses when the workspace cwd is a repo.
fn workspace_row_name(w: &WorkspaceNode) -> String {
    let label = if w.label.trim().is_empty() {
        w.workspace_id.clone()
    } else {
        w.label.clone()
    };
    // Labels may already end with the branch (e.g. "name (branch)").
    let label = match &w.branch {
        Some(b) if !b.is_empty() && !label.ends_with(&format!("({b})")) => {
            format!("{label} ({b})")
        }
        _ => label,
    };
    if w.number > 0 {
        format!("{}  {}", w.number, label)
    } else {
        label
    }
}

/// Tab row name. Tab labels are often just the tab number, so show `#N` when
/// the label is empty or equals the number (instead of "2  2"), otherwise
/// `N  label`.
fn tab_row_name(t: &TabNode) -> String {
    let label = t.label.trim();
    if label.is_empty() || label == t.number.to_string() {
        if t.number > 0 {
            format!("#{}", t.number)
        } else {
            t.tab_id.clone()
        }
    } else if t.number > 0 && !has_number_prefix(label) {
        format!("{}  {}", t.number, label)
    } else {
        label.to_string()
    }
}

pub struct TreeView {
    tree: Tree,
    /// Keys of nodes that are collapsed. Default (absent) == expanded.
    collapsed: HashSet<String>,
    selected: usize,
    /// Incremental search query. When non-empty, `rows()` shows only rows that
    /// match (plus their ancestors), with all nodes expanded.
    query: String,
}

impl TreeView {
    pub fn new(tree: Tree) -> Self {
        Self {
            tree,
            collapsed: HashSet::new(),
            selected: 0,
            query: String::new(),
        }
    }

    /// The current search query.
    pub fn query(&self) -> &str {
        &self.query
    }

    /// Whether a search filter is currently active.
    pub fn is_searching(&self) -> bool {
        !self.query.is_empty()
    }

    /// Append a character to the search query and clamp the selection.
    pub fn push_query_char(&mut self, c: char) {
        self.query.push(c);
        self.clamp_selection();
    }

    /// Remove the last character from the search query and clamp the selection.
    pub fn pop_query_char(&mut self) {
        self.query.pop();
        self.clamp_selection();
    }

    /// Clear the search query entirely.
    pub fn clear_query(&mut self) {
        self.query.clear();
        self.clamp_selection();
    }

    fn clamp_selection(&mut self) {
        let len = self.rows().len();
        if len == 0 {
            self.selected = 0;
        } else if self.selected >= len {
            self.selected = len - 1;
        }
    }

    pub fn tree(&self) -> &Tree {
        &self.tree
    }

    /// Move the initial selection to the pane the user was on when they opened
    /// the navigator. Always lands on a *pane* row: the exact focused pane, or
    /// failing that the first pane under the given tab, or the first pane under
    /// the given workspace. Only if no pane can be resolved does it fall back
    /// to the matching tab or workspace row.
    pub fn select_for_launch(
        &mut self,
        workspace_id: Option<&str>,
        tab_id: Option<&str>,
        pane_id: Option<&str>,
    ) {
        let rows = self.rows();
        let is = |want: Option<&str>, id: &str| want == Some(id);
        // Lower rank = better match, in the order described above.
        let rank = |kind: &RowKind| match kind {
            RowKind::Pane {
                workspace_id: w,
                tab_id: t,
                pane_id: p,
            } => {
                if is(pane_id, p) {
                    Some(0)
                } else if is(tab_id, t) {
                    Some(1)
                } else if is(workspace_id, w) {
                    Some(2)
                } else {
                    None
                }
            }
            RowKind::Tab { tab_id: t, .. } => is(tab_id, t).then_some(3),
            RowKind::Workspace { workspace_id: w } => is(workspace_id, w).then_some(4),
        };
        // `min_by_key` keeps the first of equal ranks, i.e. the topmost row.
        let found = rows
            .iter()
            .enumerate()
            .filter_map(|(i, r)| rank(&r.kind).map(|k| (i, k)))
            .min_by_key(|&(_, k)| k)
            .map(|(i, _)| i);
        if let Some(i) = found {
            self.selected = i;
        }
    }

    pub fn selected_index(&self) -> usize {
        self.selected
    }

    /// Swap in a freshly polled tree, keeping the selection on the same node
    /// (matched by its ids) even if rows above it appeared or disappeared.
    /// Falls back to clamping when the node is gone. Returns whether anything
    /// changed (so the caller can skip a redraw).
    pub fn refresh_tree(&mut self, tree: Tree) -> bool {
        if tree == self.tree {
            return false;
        }
        let prev = self.selected_row().map(|r| r.kind);
        self.tree = tree;
        let rows = self.rows();
        match prev.and_then(|k| rows.iter().position(|r| r.kind == k)) {
            Some(i) => self.selected = i,
            None => self.clamp_selection(),
        }
        true
    }

    /// Replace the underlying tree (e.g. after a refresh), clamping selection.
    pub fn set_tree(&mut self, tree: Tree) {
        self.tree = tree;
        self.clamp_selection();
    }

    fn is_collapsed(&self, key: &str) -> bool {
        self.collapsed.contains(key)
    }

    /// Compute the currently visible rows. With no search query, honors
    /// collapse state. With a query, shows every row that matches (plus the
    /// ancestors needed to reach it), fully expanded.
    pub fn rows(&self) -> Vec<Row> {
        if self.query.is_empty() {
            return self.all_rows(true);
        }
        let all = self.all_rows(false);
        filter_rows(&all, &self.query)
    }

    /// Build the full flattened row list. When `honor_collapse` is true,
    /// collapsed nodes hide their descendants; when false, everything is
    /// expanded (used while searching).
    fn all_rows(&self, honor_collapse: bool) -> Vec<Row> {
        let mut rows = Vec::new();
        let ws_count = self.tree.workspaces.len();
        for (wi, w) in self.tree.workspaces.iter().enumerate() {
            let wkey = ws_key(&w.workspace_id);
            let w_collapsed = honor_collapse && self.is_collapsed(&wkey);
            let w_is_last = wi + 1 == ws_count;
            rows.push(Row {
                depth: 0,
                name: workspace_row_name(w),
                kind: RowKind::Workspace {
                    workspace_id: w.workspace_id.clone(),
                },
                expandable: !w.tabs.is_empty(),
                expanded: !w_collapsed,
                agent_status: w.agent_status.clone(),
                agent: None,
                focused: w.focused,
                is_last: w_is_last,
                ancestors_last: vec![],
            });
            if w_collapsed {
                continue;
            }
            let tab_count = w.tabs.len();
            for (ti, t) in w.tabs.iter().enumerate() {
                let tkey = tab_key(&t.tab_id);
                let t_collapsed = honor_collapse && self.is_collapsed(&tkey);
                let t_is_last = ti + 1 == tab_count;
                rows.push(Row {
                    depth: 1,
                    name: tab_row_name(t),
                    kind: RowKind::Tab {
                        workspace_id: t.workspace_id.clone(),
                        tab_id: t.tab_id.clone(),
                    },
                    expandable: !t.panes.is_empty(),
                    expanded: !t_collapsed,
                    agent_status: t.agent_status.clone(),
                    agent: None,
                    focused: false,
                    is_last: t_is_last,
                    ancestors_last: vec![w_is_last],
                });
                if t_collapsed {
                    continue;
                }
                let pane_count = t.panes.len();
                for (pi, p) in t.panes.iter().enumerate() {
                    rows.push(Row {
                        depth: 2,
                        name: p.title.clone(),
                        kind: RowKind::Pane {
                            workspace_id: p.workspace_id.clone(),
                            tab_id: p.tab_id.clone(),
                            pane_id: p.pane_id.clone(),
                        },
                        expandable: false,
                        expanded: false,
                        agent_status: p.agent_status.clone(),
                        agent: p.agent.clone(),
                        focused: false,
                        is_last: pi + 1 == pane_count,
                        ancestors_last: vec![w_is_last, t_is_last],
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
        if self.selected + 1 < self.rows().len() {
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
            if !self.collapsed.remove(&key) {
                self.collapsed.insert(key);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::herdr_client::{Pane, Tab, Workspace};

    fn sample_tree_panes() -> Vec<Pane> {
        vec![Pane {
            pane_id: "w1:p1".into(),
            tab_id: "w1:t1".into(),
            workspace_id: "w1".into(),
            label: None,
            terminal_title_stripped: Some("shell-a".into()),
            terminal_title: None,
            agent: None,
            agent_status: None,
            focused: false,
            cwd: None,
        }]
    }

    fn sample_tree() -> Tree {
        let ws = vec![Workspace {
            workspace_id: "w1".into(),
            label: "proj".into(),
            number: 1,
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
                number: 1,
                pane_count: 1,
                focused: false,
                agent_status: None,
            },
            Tab {
                tab_id: "w1:t2".into(),
                workspace_id: "w1".into(),
                label: "b".into(),
                number: 2,
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
                label: None,
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
                label: None,
                terminal_title_stripped: Some("shell-b".into()),
                terminal_title: None,
                agent: Some("claude".into()),
                agent_status: None,
                focused: false,
                cwd: None,
            },
        ];
        Tree::build(ws, tabs, panes, None)
    }

    #[test]
    fn flattens_fully_expanded() {
        let view = TreeView::new(sample_tree());
        let rows = view.rows();
        // 1 workspace + 2 tabs + 2 panes = 5 rows.
        assert_eq!(rows.len(), 5);
        assert_eq!(rows[0].depth, 0);
        // Workspace shows "number  label" and is marked focused.
        assert!(rows[0].name.contains("proj"));
        assert!(rows[0].focused);
        assert_eq!(rows[1].depth, 1);
        assert_eq!(rows[2].depth, 2);
        assert_eq!(rows[2].name, "shell-a");
        assert_eq!(rows[4].agent.as_deref(), Some("claude"));
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
        assert_eq!(
            rows[1].kind,
            RowKind::Tab {
                workspace_id: "w1".into(),
                tab_id: "w1:t1".into()
            }
        );
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
    fn tab_label_with_leading_number_is_not_prefixed_again() {
        assert!(has_number_prefix("1 · api"));
        assert!(has_number_prefix("12. api"));
        assert!(!has_number_prefix("2024 plan"));
        assert!(!has_number_prefix("api"));
        let mut t = sample_tree();
        t.workspaces[0].tabs[0].label = "1 · proj › nvim".into();
        let view = TreeView::new(t);
        let rows = view.rows();
        assert_eq!(rows[1].name, "1 · proj › nvim");
        // Plain labels still get the number.
        assert_eq!(rows[3].name, "2  b");
    }

    #[test]
    fn pane_label_wins_over_terminal_title() {
        let mut p = sample_tree_panes().remove(0);
        p.label = Some("api › fix-login".into());
        assert_eq!(p.display_title(), "api › fix-login");
        p.label = Some("  ".into());
        assert_eq!(p.display_title(), "shell-a");
    }

    #[test]
    fn refresh_tree_keeps_selection_on_same_node() {
        let mut view = TreeView::new(sample_tree());
        for _ in 0..4 {
            view.move_down();
        }
        // rows: ws, t1, p1, t2, p2 -> p2 selected.
        let before = view.selected_row().unwrap().kind;
        assert!(matches!(&before, RowKind::Pane { pane_id, .. } if pane_id == "w1:p2"));
        // Same tree: no change reported.
        assert!(!view.refresh_tree(sample_tree()));
        // Tab t1 (and p1) disappear: p2 moves from index 4 to 2 and stays
        // selected; status changes are picked up.
        let mut t = sample_tree();
        t.workspaces[0].tabs.remove(0);
        t.workspaces[0].tabs[0].panes[0].agent_status = Some("working".into());
        assert!(view.refresh_tree(t));
        assert_eq!(view.selected_index(), 2);
        let row = view.selected_row().unwrap();
        assert_eq!(row.kind, before);
        assert_eq!(row.agent_status.as_deref(), Some("working"));
    }

    #[test]
    fn refresh_tree_clamps_when_selected_node_is_gone() {
        let mut view = TreeView::new(sample_tree());
        for _ in 0..4 {
            view.move_down();
        }
        let mut t = sample_tree();
        t.workspaces[0].tabs.truncate(1); // drop t2 + p2 (selected)
        assert!(view.refresh_tree(t));
        assert_eq!(view.selected_index(), 2); // clamped to last row (p1)
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
    fn empty_tree_is_safe() {
        // Reproduces "closed all panes" ending in an empty tree: no rows, no
        // selected row, no panic from the read paths the TUI uses.
        let view = TreeView::new(Tree::default());
        assert!(view.rows().is_empty());
        assert!(view.selected_row().is_none());
        // scroll_offset must be a safe no-op on an empty list.
        assert_eq!(scroll_offset(view.selected_index(), 0, 10, 0), 0);
    }

    #[test]
    fn set_tree_shrink_reclamps_into_range() {
        // Select the last row, then shrink the tree to a single workspace with
        // one tab/pane. Selection must stay within the new, smaller row set.
        let mut view = TreeView::new(sample_tree());
        for _ in 0..4 {
            view.move_down();
        }
        assert_eq!(view.selected_index(), 4);

        let ws = vec![Workspace {
            workspace_id: "w1".into(),
            label: "proj".into(),
            number: 1,
            active_tab_id: None,
            tab_count: 1,
            pane_count: 1,
            focused: true,
            agent_status: None,
        }];
        let tabs = vec![Tab {
            tab_id: "w1:t1".into(),
            workspace_id: "w1".into(),
            label: "a".into(),
            number: 1,
            pane_count: 1,
            focused: false,
            agent_status: None,
        }];
        let panes = vec![Pane {
            pane_id: "w1:p1".into(),
            tab_id: "w1:t1".into(),
            workspace_id: "w1".into(),
            label: None,
            terminal_title_stripped: Some("only".into()),
            terminal_title: None,
            agent: None,
            agent_status: None,
            focused: false,
            cwd: None,
        }];
        view.set_tree(Tree::build(ws, tabs, panes, None));
        let len = view.rows().len();
        assert_eq!(len, 3); // ws + tab + pane
        assert!(view.selected_index() < len);
        assert!(view.selected_row().is_some());
    }

    #[test]
    fn focus_plan_for_each_row_kind() {
        let view = TreeView::new(sample_tree());
        let rows = view.rows();
        let tree = view.tree();
        // In sample_tree, w1:p1 has NO agent and w1:p2 has agent "claude".
        // The pane-level (agent focus) step is only set for panes with an agent.

        // Workspace row -> workspace + first tab; pane step omitted (w1:p1 has
        // no agent).
        let wp = rows[0].focus_plan(tree);
        assert_eq!(wp.workspace_id.as_deref(), Some("w1"));
        assert_eq!(wp.tab_id.as_deref(), Some("w1:t1"));
        assert_eq!(wp.pane_id, None);

        // Tab row (index 3 is w1:t2) -> its pane w1:p2 has an agent, so the
        // agent-focus step is included.
        let tp = rows[3].focus_plan(tree);
        assert_eq!(tp.workspace_id.as_deref(), Some("w1"));
        assert_eq!(tp.tab_id.as_deref(), Some("w1:t2"));
        assert_eq!(tp.pane_id.as_deref(), Some("w1:p2"));

        // Pane row (index 2 is w1:p1, no agent) -> workspace + tab only.
        let pp = rows[2].focus_plan(tree);
        assert_eq!(pp.workspace_id.as_deref(), Some("w1"));
        assert_eq!(pp.tab_id.as_deref(), Some("w1:t1"));
        assert_eq!(pp.pane_id, None);

        // Pane row (index 4 is w1:p2, has agent) -> includes agent-focus step.
        let pp2 = rows[4].focus_plan(tree);
        assert_eq!(pp2.tab_id.as_deref(), Some("w1:t2"));
        assert_eq!(pp2.pane_id.as_deref(), Some("w1:p2"));
    }

    #[test]
    fn scroll_offset_behaviour() {
        // Everything fits: no scroll regardless of prev.
        assert_eq!(scroll_offset(0, 5, 10, 0), 0);
        assert_eq!(scroll_offset(4, 5, 5, 0), 0);
        // Zero height is a safe no-op.
        assert_eq!(scroll_offset(3, 10, 0, 0), 0);
        // Selection already visible within [prev, prev+height): keep prev.
        assert_eq!(scroll_offset(2, 20, 5, 0), 0);
        assert_eq!(scroll_offset(4, 20, 5, 0), 0);
        assert_eq!(scroll_offset(7, 20, 5, 5), 5); // 7 in [5,10): stay
                                                   // Selection below the window: reveal at bottom.
        assert_eq!(scroll_offset(5, 20, 5, 0), 1); // 5 >= 0+5 -> 1
        assert_eq!(scroll_offset(12, 20, 5, 5), 8); // 12 >= 10 -> 12+1-5
                                                    // Selection above the window: reveal at top (the bug we fixed — moving
                                                    // up should NOT drag the bottom; it stops as soon as selection shows).
        assert_eq!(scroll_offset(3, 20, 5, 8), 3); // jumped up above window
        assert_eq!(scroll_offset(9, 20, 5, 10), 9); // one above window top
                                                    // Never scroll past the end.
        assert_eq!(scroll_offset(19, 20, 5, 0), 15);
        assert_eq!(scroll_offset(10, 20, 5, 99), 10); // prev clamped
    }

    #[test]
    fn scroll_offset_sticky_up_does_not_clip_bottom() {
        // 20 rows, window 5. Start at the very bottom (offset 15, selected 19).
        let mut offset = scroll_offset(19, 20, 5, 15);
        assert_eq!(offset, 15);
        // Move up one at a time; the window must stay put until the selection
        // reaches its top edge, instead of shifting every step.
        for sel in (15..=19).rev() {
            offset = scroll_offset(sel, 20, 5, offset);
            assert_eq!(
                offset, 15,
                "window should not move while sel {sel} is visible"
            );
        }
        // Now move above the top (14): window scrolls up by exactly one.
        offset = scroll_offset(14, 20, 5, offset);
        assert_eq!(offset, 14);
    }

    #[test]
    fn select_for_launch_prefers_pane_then_tab_then_workspace() {
        // Rows: 0=w1, 1=w1:t1, 2=w1:p1, 3=w1:t2, 4=w1:p2
        let mut v = TreeView::new(sample_tree());
        v.select_for_launch(Some("w1"), Some("w1:t2"), Some("w1:p2"));
        assert_eq!(v.selected_index(), 4); // exact pane

        // No exact pane: land on the first pane under the given tab.
        let mut v = TreeView::new(sample_tree());
        v.select_for_launch(Some("w1"), Some("w1:t2"), None);
        assert_eq!(v.selected_index(), 4); // w1:t2's pane (w1:p2)

        // No tab: land on the first pane under the workspace.
        let mut v = TreeView::new(sample_tree());
        v.select_for_launch(Some("w1"), None, None);
        assert_eq!(v.selected_index(), 2); // first pane w1:p1

        // Unknown ids leave selection unchanged.
        let mut v = TreeView::new(sample_tree());
        v.move_down();
        v.select_for_launch(Some("zzz"), Some("zzz"), Some("zzz"));
        assert_eq!(v.selected_index(), 1);
    }

    #[test]
    fn empty_query_shows_all_rows() {
        let view = TreeView::new(sample_tree());
        assert!(!view.is_searching());
        assert_eq!(view.rows().len(), 5); // ws + 2 tabs + 2 panes
    }

    #[test]
    fn query_filters_to_matches_plus_ancestors() {
        // Search "claude": only w1:p2 (agent claude) matches. Its ancestors
        // (w1 workspace, w1:t2 tab) are kept so it's reachable.
        let mut view = TreeView::new(sample_tree());
        for c in "claude".chars() {
            view.push_query_char(c);
        }
        assert!(view.is_searching());
        let rows = view.rows();
        // workspace w1, tab w1:t2, pane w1:p2 = 3 rows (w1:t1 and w1:p1 dropped).
        assert_eq!(rows.len(), 3);
        assert!(matches!(rows[0].kind, RowKind::Workspace { .. }));
        assert!(matches!(&rows[1].kind, RowKind::Tab { tab_id, .. } if tab_id == "w1:t2"));
        assert!(matches!(&rows[2].kind, RowKind::Pane { pane_id, .. } if pane_id == "w1:p2"));
    }

    #[test]
    fn query_matches_pane_title() {
        let mut view = TreeView::new(sample_tree());
        for c in "shell-a".chars() {
            view.push_query_char(c);
        }
        let rows = view.rows();
        // Only w1:p1 ("shell-a") + ancestors w1, w1:t1 = 3.
        assert_eq!(rows.len(), 3);
        assert!(matches!(&rows[2].kind, RowKind::Pane { pane_id, .. } if pane_id == "w1:p1"));
    }

    #[test]
    fn query_matches_workspace_label() {
        let mut view = TreeView::new(sample_tree());
        for c in "proj".chars() {
            view.push_query_char(c);
        }
        // Workspace matches -> workspace kept; descendants are not auto-kept
        // (only ancestors of matches are), so just the workspace row shows.
        let rows = view.rows();
        assert_eq!(rows.len(), 1);
        assert!(matches!(rows[0].kind, RowKind::Workspace { .. }));
    }

    #[test]
    fn query_is_case_insensitive_and_clears() {
        let mut view = TreeView::new(sample_tree());
        for c in "CLAUDE".chars() {
            view.push_query_char(c);
        }
        assert_eq!(view.rows().len(), 3);
        view.pop_query_char(); // "CLAUD"
        assert_eq!(view.rows().len(), 3);
        view.clear_query();
        assert!(!view.is_searching());
        assert_eq!(view.rows().len(), 5);
    }

    #[test]
    fn no_match_yields_empty_and_selection_clamps() {
        let mut view = TreeView::new(sample_tree());
        view.move_down();
        view.move_down(); // selected index 2
        for c in "zzzzz".chars() {
            view.push_query_char(c);
        }
        assert_eq!(view.rows().len(), 0);
        assert_eq!(view.selected_index(), 0); // clamped
        assert!(view.selected_row().is_none());
    }

    #[test]
    fn row_matches_query_direct() {
        let view = TreeView::new(sample_tree());
        let rows = view.rows();
        let pane_claude = rows
            .iter()
            .find(|r| matches!(&r.kind, RowKind::Pane { pane_id, .. } if pane_id == "w1:p2"))
            .unwrap();
        assert!(row_matches_query(pane_claude, "claude"));
        assert!(row_matches_query(pane_claude, "w1:p2")); // id match
        assert!(!row_matches_query(pane_claude, "nope"));
        assert!(row_matches_query(pane_claude, "")); // empty matches
    }

    #[test]
    fn workspace_name_includes_branch_when_present() {
        let mut tree = sample_tree();
        tree.workspaces[0].branch = Some("feature/foo".to_string());
        let view = TreeView::new(tree);
        let rows = view.rows();
        // Workspace row name: "1  proj (feature/foo)".
        assert!(
            rows[0].name.contains("proj (feature/foo)"),
            "got {:?}",
            rows[0].name
        );
    }

    #[test]
    fn workspace_name_without_branch_is_plain() {
        // sample_tree has branch=None, so no parentheses are added.
        let view = TreeView::new(sample_tree());
        let rows = view.rows();
        assert!(rows[0].name.contains("proj"));
        assert!(!rows[0].name.contains('('), "got {:?}", rows[0].name);
    }

    #[test]
    fn workspace_name_does_not_repeat_branch_already_in_label() {
        let mut tree = sample_tree();
        tree.workspaces[0].label = "proj (main)".to_string();
        tree.workspaces[0].branch = Some("main".to_string());
        let view = TreeView::new(tree);
        let rows = view.rows();
        assert!(
            rows[0].name.ends_with("proj (main)"),
            "got {:?}",
            rows[0].name
        );
        assert!(
            !rows[0].name.contains("(main) (main)"),
            "got {:?}",
            rows[0].name
        );
    }
}
