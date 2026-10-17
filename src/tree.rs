//! Builds a `workspace > tab > pane` tree from the flat lists returned by the
//! herdr CLI, grouping tabs under workspaces and panes under tabs by their id
//! fields. Ordering is stabilized by id so the UI is deterministic.

use crate::herdr_client::{Pane, Tab, Workspace};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaneNode {
    pub pane_id: String,
    pub tab_id: String,
    pub workspace_id: String,
    pub title: String,
    pub agent: Option<String>,
    pub agent_status: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TabNode {
    pub tab_id: String,
    pub workspace_id: String,
    pub label: String,
    pub panes: Vec<PaneNode>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceNode {
    pub workspace_id: String,
    pub label: String,
    pub focused: bool,
    pub tabs: Vec<TabNode>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Tree {
    pub workspaces: Vec<WorkspaceNode>,
}

impl Tree {
    /// Assemble a tree from the three flat lists. Panes are matched to tabs by
    /// `tab_id`; tabs are matched to workspaces by `workspace_id`. Entries are
    /// sorted by their id for stable display order.
    pub fn build(workspaces: Vec<Workspace>, tabs: Vec<Tab>, panes: Vec<Pane>) -> Self {
        let mut panes = panes;
        panes.sort_by(|a, b| a.pane_id.cmp(&b.pane_id));

        let mut tabs = tabs;
        tabs.sort_by(|a, b| a.tab_id.cmp(&b.tab_id));

        let mut workspaces = workspaces;
        workspaces.sort_by(|a, b| a.workspace_id.cmp(&b.workspace_id));

        let ws_nodes = workspaces
            .into_iter()
            .map(|w| {
                let tab_nodes = tabs
                    .iter()
                    .filter(|t| t.workspace_id == w.workspace_id)
                    .map(|t| {
                        let pane_nodes = panes
                            .iter()
                            .filter(|p| p.tab_id == t.tab_id)
                            .map(|p| PaneNode {
                                pane_id: p.pane_id.clone(),
                                tab_id: p.tab_id.clone(),
                                workspace_id: p.workspace_id.clone(),
                                title: p.display_title(),
                                agent: p.agent.clone(),
                                agent_status: p.agent_status.clone(),
                            })
                            .collect();
                        TabNode {
                            tab_id: t.tab_id.clone(),
                            workspace_id: t.workspace_id.clone(),
                            label: t.label.clone(),
                            panes: pane_nodes,
                        }
                    })
                    .collect();
                WorkspaceNode {
                    workspace_id: w.workspace_id.clone(),
                    label: w.label.clone(),
                    focused: w.focused,
                    tabs: tab_nodes,
                }
            })
            .collect();

        Tree {
            workspaces: ws_nodes,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.workspaces.is_empty()
    }

    /// Render the tree as indented text. Used by the `--dump-tree` debug command
    /// and handy for assertions.
    pub fn to_indented_text(&self) -> String {
        let mut out = String::new();
        for w in &self.workspaces {
            out.push_str(&format!(
                "{} [{}]{}\n",
                w.workspace_id,
                w.label,
                if w.focused { " *" } else { "" }
            ));
            for t in &w.tabs {
                out.push_str(&format!("  {} ({})\n", t.tab_id, t.label));
                for p in &t.panes {
                    let agent = p
                        .agent
                        .as_deref()
                        .map(|a| format!(" <{}>", a))
                        .unwrap_or_default();
                    out.push_str(&format!("    {} {}{}\n", p.pane_id, p.title, agent));
                }
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::herdr_client::{Pane, Tab, Workspace};

    fn ws(id: &str, label: &str, focused: bool) -> Workspace {
        Workspace {
            workspace_id: id.into(),
            label: label.into(),
            active_tab_id: None,
            tab_count: 0,
            pane_count: 0,
            focused,
            agent_status: None,
        }
    }

    fn tab(id: &str, ws: &str, label: &str) -> Tab {
        Tab {
            tab_id: id.into(),
            workspace_id: ws.into(),
            label: label.into(),
            pane_count: 0,
            focused: false,
            agent_status: None,
        }
    }

    fn pane(id: &str, tab: &str, ws: &str, title: &str) -> Pane {
        Pane {
            pane_id: id.into(),
            tab_id: tab.into(),
            workspace_id: ws.into(),
            terminal_title_stripped: Some(title.into()),
            terminal_title: None,
            agent: None,
            agent_status: None,
            focused: false,
            cwd: None,
        }
    }

    #[test]
    fn builds_nested_structure() {
        let tree = Tree::build(
            vec![ws("w2", "second", false), ws("w1", "first", true)],
            vec![
                tab("w1:t1", "w1", "a"),
                tab("w1:t2", "w1", "b"),
                tab("w2:t1", "w2", "c"),
            ],
            vec![
                pane("w1:p1", "w1:t1", "w1", "p1"),
                pane("w1:p2", "w1:t2", "w1", "p2"),
                pane("w2:p1", "w2:t1", "w2", "p3"),
            ],
        );

        // Sorted by workspace_id: w1 then w2.
        assert_eq!(tree.workspaces.len(), 2);
        assert_eq!(tree.workspaces[0].workspace_id, "w1");
        assert!(tree.workspaces[0].focused);
        assert_eq!(tree.workspaces[1].workspace_id, "w2");

        // w1 has two tabs each with one pane.
        assert_eq!(tree.workspaces[0].tabs.len(), 2);
        assert_eq!(tree.workspaces[0].tabs[0].tab_id, "w1:t1");
        assert_eq!(tree.workspaces[0].tabs[0].panes.len(), 1);
        assert_eq!(tree.workspaces[0].tabs[0].panes[0].pane_id, "w1:p1");

        // w2 has one tab with one pane.
        assert_eq!(tree.workspaces[1].tabs.len(), 1);
        assert_eq!(tree.workspaces[1].tabs[0].panes[0].pane_id, "w2:p1");
    }

    #[test]
    fn tab_with_multiple_panes() {
        let tree = Tree::build(
            vec![ws("w1", "x", false)],
            vec![tab("w1:t1", "w1", "a")],
            vec![
                pane("w1:p2", "w1:t1", "w1", "second"),
                pane("w1:p1", "w1:t1", "w1", "first"),
            ],
        );
        let panes = &tree.workspaces[0].tabs[0].panes;
        assert_eq!(panes.len(), 2);
        // Sorted by pane_id.
        assert_eq!(panes[0].pane_id, "w1:p1");
        assert_eq!(panes[1].pane_id, "w1:p2");
    }

    #[test]
    fn empty_inputs() {
        let tree = Tree::build(vec![], vec![], vec![]);
        assert!(tree.is_empty());
        assert_eq!(tree.to_indented_text(), "");
    }

    #[test]
    fn workspace_without_tabs_and_tab_without_panes() {
        let tree = Tree::build(
            vec![ws("w1", "x", false)],
            vec![tab("w1:t1", "w1", "a")],
            vec![],
        );
        assert_eq!(tree.workspaces[0].tabs.len(), 1);
        assert!(tree.workspaces[0].tabs[0].panes.is_empty());
    }

    #[test]
    fn indented_text_shows_hierarchy() {
        let tree = Tree::build(
            vec![ws("w1", "proj", true)],
            vec![tab("w1:t1", "w1", "main")],
            vec![pane("w1:p1", "w1:t1", "w1", "shell")],
        );
        let text = tree.to_indented_text();
        assert!(text.contains("w1 [proj] *"));
        assert!(text.contains("  w1:t1 (main)"));
        assert!(text.contains("    w1:p1 shell"));
    }
}
