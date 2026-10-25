//! Thin client around the `herdr` CLI.
//!
//! herdr injects `HERDR_BIN_PATH` into plugin processes so they can call back
//! into the running server portably. We shell out to that binary and parse the
//! JSON it prints on stdout.
//!
//! Verified against herdr 0.7.5:
//! - `herdr workspace list`  -> `{ "result": { "workspaces": [...] } }`
//! - `herdr tab list`        -> `{ "result": { "tabs": [...] } }`
//! - `herdr pane list`       -> `{ "result": { "panes": [...] } }`
//! - `herdr pane read <id> --source visible [--lines N]` -> plain text
//!
//! The list commands emit JSON by default; passing `--json` is rejected on
//! 0.7.x, so we must not add it.

use std::process::Command;

use anyhow::{anyhow, Context, Result};
use serde::Deserialize;

/// Resolve the herdr binary to call. Prefer the injected `HERDR_BIN_PATH`,
/// falling back to `herdr` on `PATH` for local development.
pub fn herdr_bin() -> String {
    std::env::var("HERDR_BIN_PATH").unwrap_or_else(|_| "herdr".to_string())
}

/// Current git branch for a directory, or `None` when it is not a git repo,
/// git is unavailable, or HEAD is detached. Uses `git -C <cwd> rev-parse
/// --abbrev-ref HEAD`; a detached HEAD reports `HEAD`, which we treat as none.
pub fn git_branch(cwd: &str) -> Option<String> {
    let output = std::process::Command::new("git")
        .args(["-C", cwd, "rev-parse", "--abbrev-ref", "HEAD"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let branch = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if branch.is_empty() || branch == "HEAD" {
        None
    } else {
        Some(branch)
    }
}

/// Directories known to `zoxide`, most-frecent first, or an empty list when
/// zoxide is not installed/usable. Shells out to `zoxide query -l`.
pub fn zoxide_dirs() -> Vec<String> {
    let output = match std::process::Command::new("zoxide")
        .args(["query", "-l"])
        .output()
    {
        Ok(o) if o.status.success() => o,
        _ => return Vec::new(),
    };
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .collect()
}

/// The navigator's own overlay pane id, if launched as a plugin pane. herdr
/// sets `HERDR_PANE_ID` to the running pane; we exclude it from the tree so the
/// navigator doesn't list itself. Popups are not panes and have no
/// `HERDR_PANE_ID`, in which case this is `None` (and nothing needs excluding).
pub fn own_pane_id() -> Option<String> {
    std::env::var("HERDR_PANE_ID")
        .ok()
        .filter(|s| !s.is_empty())
}

/// The tab the navigator was launched from. Prefer `HERDR_TAB_ID` (set for
/// pane entrypoints); fall back to the launch context's `tab_id` (popups don't
/// get `HERDR_TAB_ID` but still receive `HERDR_PLUGIN_CONTEXT_JSON`).
pub fn own_tab_id() -> Option<String> {
    std::env::var("HERDR_TAB_ID")
        .ok()
        .filter(|s| !s.is_empty())
        .or_else(|| LaunchContext::from_env().tab_id)
}

/// The pane/tab/workspace the overlay was launched from, so the navigator can
/// preselect the row for the pane the user was on. herdr injects
/// `HERDR_PLUGIN_CONTEXT_JSON` (richest, includes `focused_pane_id`) plus
/// `HERDR_WORKSPACE_ID` / `HERDR_TAB_ID`. Note `HERDR_PANE_ID` is the overlay's
/// *own* pane, not the origin, so we must not use it here.
#[derive(Debug, Clone, Default, Deserialize, PartialEq, Eq)]
pub struct LaunchContext {
    #[serde(default)]
    pub workspace_id: Option<String>,
    #[serde(default)]
    pub tab_id: Option<String>,
    #[serde(default)]
    pub focused_pane_id: Option<String>,
}

impl LaunchContext {
    /// Read the launch context from the environment herdr injects.
    pub fn from_env() -> Self {
        if let Ok(json) = std::env::var("HERDR_PLUGIN_CONTEXT_JSON") {
            if let Ok(ctx) = Self::from_json(&json) {
                // Backfill workspace/tab from the dedicated vars if missing.
                return ctx.with_env_fallback();
            }
        }
        LaunchContext::default().with_env_fallback()
    }

    fn with_env_fallback(mut self) -> Self {
        if self.workspace_id.is_none() {
            self.workspace_id = std::env::var("HERDR_WORKSPACE_ID").ok();
        }
        if self.tab_id.is_none() {
            self.tab_id = std::env::var("HERDR_TAB_ID").ok();
        }
        self
    }

    /// Parse from the `HERDR_PLUGIN_CONTEXT_JSON` payload.
    pub fn from_json(json: &str) -> Result<Self> {
        serde_json::from_str(json).context("failed to parse HERDR_PLUGIN_CONTEXT_JSON")
    }
}

/// A herdr workspace (top-level project container).
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct Workspace {
    pub workspace_id: String,
    #[serde(default)]
    pub label: String,
    #[serde(default)]
    pub number: u32,
    #[serde(default)]
    pub active_tab_id: Option<String>,
    #[serde(default)]
    pub tab_count: u32,
    #[serde(default)]
    pub pane_count: u32,
    #[serde(default)]
    pub focused: bool,
    #[serde(default)]
    pub agent_status: Option<String>,
}

/// A tab (layout) inside a workspace.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct Tab {
    pub tab_id: String,
    pub workspace_id: String,
    #[serde(default)]
    pub label: String,
    #[serde(default)]
    pub number: u32,
    #[serde(default)]
    pub pane_count: u32,
    #[serde(default)]
    pub focused: bool,
    #[serde(default)]
    pub agent_status: Option<String>,
}

/// A pane (a real terminal) inside a tab.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct Pane {
    pub pane_id: String,
    pub tab_id: String,
    pub workspace_id: String,
    /// Pane label (herdr >= 0.8; set by `herdr pane rename` or naming
    /// plugins such as herdr-auto-title).
    #[serde(default)]
    pub label: Option<String>,
    #[serde(default)]
    pub terminal_title_stripped: Option<String>,
    #[serde(default)]
    pub terminal_title: Option<String>,
    #[serde(default)]
    pub agent: Option<String>,
    #[serde(default)]
    pub agent_status: Option<String>,
    #[serde(default)]
    pub focused: bool,
    #[serde(default)]
    pub cwd: Option<String>,
}

impl Pane {
    /// Human-friendly one-line label for the pane, preferring its label, then
    /// a stripped terminal title, then the raw title, then the agent name,
    /// then a generic "shell". Never falls back to the raw pane id, which
    /// looks like noise.
    pub fn display_title(&self) -> String {
        self.label
            .as_deref()
            .filter(|s| !s.trim().is_empty())
            .or(self.terminal_title_stripped.as_deref())
            .filter(|s| !s.trim().is_empty())
            .or(self.terminal_title.as_deref())
            .filter(|s| !s.trim().is_empty())
            .map(|s| s.to_string())
            .or_else(|| self.agent.clone().filter(|s| !s.trim().is_empty()))
            .unwrap_or_else(|| "shell".to_string())
    }
}

#[derive(Debug, Deserialize)]
struct WorkspaceListResult {
    #[serde(default)]
    workspaces: Vec<Workspace>,
}

#[derive(Debug, Deserialize)]
struct WorkspaceListEnvelope {
    result: WorkspaceListResult,
}

#[derive(Debug, Deserialize)]
struct TabListResult {
    #[serde(default)]
    tabs: Vec<Tab>,
}

#[derive(Debug, Deserialize)]
struct TabListEnvelope {
    result: TabListResult,
}

#[derive(Debug, Deserialize)]
struct PaneListResult {
    #[serde(default)]
    panes: Vec<Pane>,
}

#[derive(Debug, Deserialize)]
struct PaneListEnvelope {
    result: PaneListResult,
}

// --- JSON parsing (pure functions, unit-tested against real fixtures) ---

pub fn parse_workspaces(json: &str) -> Result<Vec<Workspace>> {
    let env: WorkspaceListEnvelope =
        serde_json::from_str(json).context("failed to parse `workspace list` JSON")?;
    Ok(env.result.workspaces)
}

pub fn parse_tabs(json: &str) -> Result<Vec<Tab>> {
    let env: TabListEnvelope =
        serde_json::from_str(json).context("failed to parse `tab list` JSON")?;
    Ok(env.result.tabs)
}

pub fn parse_panes(json: &str) -> Result<Vec<Pane>> {
    let env: PaneListEnvelope =
        serde_json::from_str(json).context("failed to parse `pane list` JSON")?;
    Ok(env.result.panes)
}

#[derive(Debug, Deserialize)]
struct CreatedWorkspace {
    workspace_id: String,
}

#[derive(Debug, Deserialize)]
struct CreateWorkspaceResult {
    workspace: CreatedWorkspace,
}

#[derive(Debug, Deserialize)]
struct CreateWorkspaceEnvelope {
    result: CreateWorkspaceResult,
}

/// Parse the workspace id from `workspace create` output.
pub fn parse_created_workspace_id(json: &str) -> Result<String> {
    let env: CreateWorkspaceEnvelope =
        serde_json::from_str(json).context("failed to parse `workspace create` JSON")?;
    Ok(env.result.workspace.workspace_id)
}

#[derive(Debug, Deserialize)]
struct CreatedTab {
    tab_id: String,
}

#[derive(Debug, Deserialize)]
struct CreateTabResult {
    tab: CreatedTab,
}

#[derive(Debug, Deserialize)]
struct CreateTabEnvelope {
    result: CreateTabResult,
}

/// Parse the new tab id from `tab create` output (`.result.tab.tab_id`).
pub fn parse_created_tab_id(json: &str) -> Result<String> {
    let env: CreateTabEnvelope =
        serde_json::from_str(json).context("failed to parse `tab create` JSON")?;
    Ok(env.result.tab.tab_id)
}

// --- CLI invocation ---

/// A plan describing how to jump focus to a target, as an ordered list of
/// herdr focus steps. On herdr 0.7.x there is no "focus pane by id", so we
/// focus the workspace, then the tab (which brings its pane forward), and
/// finally attempt `agent focus <pane_id>` as a best-effort for pane-level
/// precision. Steps are applied in order; a failing best-effort step does not
/// fail the jump.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FocusPlan {
    pub workspace_id: Option<String>,
    pub tab_id: Option<String>,
    /// Best-effort pane focus via `agent focus <pane_id>`; failures ignored.
    pub pane_id: Option<String>,
}

impl FocusPlan {
    pub fn workspace(workspace_id: impl Into<String>) -> Self {
        Self {
            workspace_id: Some(workspace_id.into()),
            tab_id: None,
            pane_id: None,
        }
    }

    pub fn tab(workspace_id: impl Into<String>, tab_id: impl Into<String>) -> Self {
        Self {
            workspace_id: Some(workspace_id.into()),
            tab_id: Some(tab_id.into()),
            pane_id: None,
        }
    }

    #[allow(dead_code)]
    pub fn pane(
        workspace_id: impl Into<String>,
        tab_id: impl Into<String>,
        pane_id: impl Into<String>,
    ) -> Self {
        Self {
            workspace_id: Some(workspace_id.into()),
            tab_id: Some(tab_id.into()),
            pane_id: Some(pane_id.into()),
        }
    }
}

/// Trait so the TUI can depend on an abstraction and tests can inject fakes.
pub trait HerdrApi {
    fn workspaces(&self) -> Result<Vec<Workspace>>;
    fn tabs(&self) -> Result<Vec<Tab>>;
    fn panes(&self) -> Result<Vec<Pane>>;
    #[allow(dead_code)]
    fn read_pane(&self, pane_id: &str, lines: u32) -> Result<String>;
    /// Like `read_pane` but preserves ANSI SGR escapes for colored rendering.
    fn read_pane_ansi(&self, pane_id: &str, lines: u32) -> Result<String>;
    fn focus_workspace(&self, workspace_id: &str) -> Result<()>;
    fn focus_tab(&self, tab_id: &str) -> Result<()>;
    fn focus_agent(&self, target: &str) -> Result<()>;
    /// Close (kill) a pane by id.
    fn close_pane(&self, pane_id: &str) -> Result<()>;
    /// Create a new workspace with the given label and optional cwd; returns
    /// its workspace id.
    fn create_workspace(&self, label: &str, cwd: Option<&str>) -> Result<String>;
    /// Create a new tab in the given workspace, returning the new tab id.
    fn create_tab(&self, workspace_id: &str) -> Result<String>;
    /// Rename a tab.
    fn rename_tab(&self, tab_id: &str, label: &str) -> Result<()>;

    /// Execute a focus plan. The workspace and tab steps are required (an error
    /// propagates); the pane step is best-effort and its error is swallowed.
    fn apply_focus(&self, plan: &FocusPlan) -> Result<()> {
        if let Some(ws) = &plan.workspace_id {
            self.focus_workspace(ws)?;
        }
        if let Some(tab) = &plan.tab_id {
            self.focus_tab(tab)?;
        }
        if let Some(pane) = &plan.pane_id {
            // Best effort: pane-level focus is not guaranteed on all versions.
            let _ = self.focus_agent(pane);
        }
        Ok(())
    }
}

/// Real implementation that shells out to the herdr binary.
pub struct CliHerdr {
    bin: String,
}

impl CliHerdr {
    pub fn new() -> Self {
        Self { bin: herdr_bin() }
    }

    fn run(&self, args: &[&str]) -> Result<String> {
        let output = Command::new(&self.bin)
            .args(args)
            .output()
            .with_context(|| format!("failed to run `{} {}`", self.bin, args.join(" ")))?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(anyhow!(
                "`{} {}` exited with {}: {}",
                self.bin,
                args.join(" "),
                output.status,
                stderr.trim()
            ));
        }
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    }
}

impl Default for CliHerdr {
    fn default() -> Self {
        Self::new()
    }
}

impl HerdrApi for CliHerdr {
    fn workspaces(&self) -> Result<Vec<Workspace>> {
        parse_workspaces(&self.run(&["workspace", "list"])?)
    }

    fn tabs(&self) -> Result<Vec<Tab>> {
        parse_tabs(&self.run(&["tab", "list"])?)
    }

    fn panes(&self) -> Result<Vec<Pane>> {
        parse_panes(&self.run(&["pane", "list"])?)
    }

    fn read_pane(&self, pane_id: &str, lines: u32) -> Result<String> {
        let lines = lines.to_string();
        self.run(&[
            "pane", "read", pane_id, "--source", "visible", "--lines", &lines,
        ])
    }

    fn read_pane_ansi(&self, pane_id: &str, lines: u32) -> Result<String> {
        let lines = lines.to_string();
        self.run(&[
            "pane", "read", pane_id, "--source", "visible", "--lines", &lines, "--ansi",
        ])
    }

    fn focus_workspace(&self, workspace_id: &str) -> Result<()> {
        self.run(&["workspace", "focus", workspace_id]).map(|_| ())
    }

    fn focus_tab(&self, tab_id: &str) -> Result<()> {
        self.run(&["tab", "focus", tab_id]).map(|_| ())
    }

    fn focus_agent(&self, target: &str) -> Result<()> {
        self.run(&["agent", "focus", target]).map(|_| ())
    }

    fn close_pane(&self, pane_id: &str) -> Result<()> {
        self.run(&["pane", "close", pane_id]).map(|_| ())
    }

    fn create_workspace(&self, label: &str, cwd: Option<&str>) -> Result<String> {
        let mut args: Vec<&str> = vec!["workspace", "create", "--label", label, "--no-focus"];
        if let Some(c) = cwd {
            args.push("--cwd");
            args.push(c);
        }
        let out = self.run(&args)?;
        parse_created_workspace_id(&out)
    }

    fn create_tab(&self, workspace_id: &str) -> Result<String> {
        let out = self.run(&["tab", "create", "--workspace", workspace_id, "--no-focus"])?;
        parse_created_tab_id(&out)
    }

    fn rename_tab(&self, tab_id: &str, label: &str) -> Result<()> {
        self.run(&["tab", "rename", tab_id, label]).map(|_| ())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Real output captured from herdr 0.7.5 (trimmed to two entries).
    const WORKSPACE_JSON: &str = r#"
    {"id":"cli:workspace:list","result":{"type":"workspace_list","workspaces":[
      {"active_tab_id":"w4:t1","agent_status":"unknown","focused":false,"label":"aaaaa","number":1,"pane_count":1,"tab_count":1,"workspace_id":"w4"},
      {"active_tab_id":"wC:t1","agent_status":"working","focused":true,"label":"herder-nav-with-preview","number":9,"pane_count":1,"tab_count":1,"workspace_id":"wC"}
    ]}}"#;

    const TAB_JSON: &str = r#"
    {"id":"cli:tab:list","result":{"tabs":[
      {"agent_status":"unknown","focused":false,"label":"1","number":1,"pane_count":1,"tab_id":"w4:t1","workspace_id":"w4"},
      {"agent_status":"working","focused":true,"label":"1","number":1,"pane_count":1,"tab_id":"wC:t1","workspace_id":"wC"}
    ],"type":"tab_list"}}"#;

    const PANE_JSON: &str = r#"
    {"id":"cli:pane:list","result":{"panes":[
      {"agent_status":"unknown","cwd":"/Users/asaryu/memo","focused":false,"foreground_cwd":"/Users/asaryu/memo","pane_id":"w4:p1","revision":12,"scroll":{"max_offset_from_bottom":0,"offset_from_bottom":0,"viewport_rows":57},"tab_id":"w4:t1","terminal_id":"term_x","terminal_title":"nvim weekly-report.md","terminal_title_stripped":"nvim weekly-report.md","workspace_id":"w4"},
      {"agent":"claude","agent_session":{"agent":"claude","kind":"id","source":"herdr:claude","value":"abc"},"agent_status":"idle","cwd":"/x","focused":false,"foreground_cwd":"/x","pane_id":"w5:p3","revision":5816,"scroll":{"max_offset_from_bottom":0,"offset_from_bottom":0,"viewport_rows":57},"tab_id":"w5:t3","terminal_id":"term_y","terminal_title":"title","terminal_title_stripped":"","workspace_id":"w5"}
    ],"type":"pane_list"}}"#;

    #[test]
    fn parses_workspaces() {
        let ws = parse_workspaces(WORKSPACE_JSON).unwrap();
        assert_eq!(ws.len(), 2);
        assert_eq!(ws[0].workspace_id, "w4");
        assert_eq!(ws[0].label, "aaaaa");
        assert_eq!(ws[0].active_tab_id.as_deref(), Some("w4:t1"));
        assert!(!ws[0].focused);
        assert!(ws[1].focused);
        assert_eq!(ws[1].label, "herder-nav-with-preview");
    }

    #[test]
    fn parses_tabs() {
        let tabs = parse_tabs(TAB_JSON).unwrap();
        assert_eq!(tabs.len(), 2);
        assert_eq!(tabs[0].tab_id, "w4:t1");
        assert_eq!(tabs[0].workspace_id, "w4");
        assert_eq!(tabs[1].tab_id, "wC:t1");
        assert!(tabs[1].focused);
    }

    #[test]
    fn parses_panes() {
        let panes = parse_panes(PANE_JSON).unwrap();
        assert_eq!(panes.len(), 2);
        assert_eq!(panes[0].pane_id, "w4:p1");
        assert_eq!(panes[0].tab_id, "w4:t1");
        assert_eq!(panes[0].agent, None);
        assert_eq!(panes[1].agent.as_deref(), Some("claude"));
        // Stripped title is empty -> display_title falls back to raw title.
        assert_eq!(panes[1].display_title(), "title");
        assert_eq!(panes[0].display_title(), "nvim weekly-report.md");
    }

    #[test]
    fn empty_lists_parse() {
        assert!(parse_workspaces(r#"{"result":{"workspaces":[]}}"#)
            .unwrap()
            .is_empty());
        assert!(parse_tabs(r#"{"result":{"tabs":[]}}"#).unwrap().is_empty());
        assert!(parse_panes(r#"{"result":{"panes":[]}}"#)
            .unwrap()
            .is_empty());
    }

    #[test]
    fn missing_optional_fields_ok() {
        // Only required ids present; optional fields default.
        let panes = parse_panes(
            r#"{"result":{"panes":[{"pane_id":"w1:p1","tab_id":"w1:t1","workspace_id":"w1"}]}}"#,
        )
        .unwrap();
        assert_eq!(panes.len(), 1);
        assert_eq!(panes[0].display_title(), "shell");
        assert!(!panes[0].focused);
    }

    #[test]
    fn invalid_json_errors() {
        assert!(parse_workspaces("not json").is_err());
        assert!(parse_panes(r#"{"result":{}}"#).is_ok()); // panes defaults to empty
    }

    use std::cell::RefCell;

    /// Fake API that records the focus calls made against it, in order.
    #[derive(Default)]
    struct FakeApi {
        calls: RefCell<Vec<String>>,
        fail_agent: bool,
    }

    impl HerdrApi for FakeApi {
        fn workspaces(&self) -> Result<Vec<Workspace>> {
            Ok(vec![])
        }
        fn tabs(&self) -> Result<Vec<Tab>> {
            Ok(vec![])
        }
        fn panes(&self) -> Result<Vec<Pane>> {
            Ok(vec![])
        }
        fn read_pane(&self, _pane_id: &str, _lines: u32) -> Result<String> {
            Ok(String::new())
        }
        fn read_pane_ansi(&self, _pane_id: &str, _lines: u32) -> Result<String> {
            Ok(String::new())
        }
        fn focus_workspace(&self, workspace_id: &str) -> Result<()> {
            self.calls.borrow_mut().push(format!("ws:{workspace_id}"));
            Ok(())
        }
        fn focus_tab(&self, tab_id: &str) -> Result<()> {
            self.calls.borrow_mut().push(format!("tab:{tab_id}"));
            Ok(())
        }
        fn focus_agent(&self, target: &str) -> Result<()> {
            self.calls.borrow_mut().push(format!("agent:{target}"));
            if self.fail_agent {
                Err(anyhow!("agent focus failed"))
            } else {
                Ok(())
            }
        }
        fn close_pane(&self, pane_id: &str) -> Result<()> {
            self.calls.borrow_mut().push(format!("close:{pane_id}"));
            Ok(())
        }
        fn create_workspace(&self, label: &str, cwd: Option<&str>) -> Result<String> {
            self.calls
                .borrow_mut()
                .push(format!("create:{label}:{}", cwd.unwrap_or("-")));
            Ok("wNEW".to_string())
        }
        fn create_tab(&self, workspace_id: &str) -> Result<String> {
            self.calls.borrow_mut().push(format!("tab:{workspace_id}"));
            Ok(format!("{workspace_id}:tN"))
        }
        fn rename_tab(&self, tab_id: &str, label: &str) -> Result<()> {
            self.calls
                .borrow_mut()
                .push(format!("renametab:{tab_id}:{label}"));
            Ok(())
        }
    }

    #[test]
    fn apply_focus_runs_steps_in_order() {
        let api = FakeApi::default();
        let plan = FocusPlan::pane("w1", "w1:t1", "w1:p1");
        api.apply_focus(&plan).unwrap();
        assert_eq!(
            *api.calls.borrow(),
            vec!["ws:w1", "tab:w1:t1", "agent:w1:p1"]
        );
    }

    #[test]
    fn apply_focus_ignores_agent_failure() {
        let api = FakeApi {
            fail_agent: true,
            ..Default::default()
        };
        let plan = FocusPlan::pane("w1", "w1:t1", "w1:p1");
        // Best-effort pane focus failure must not fail the whole jump.
        assert!(api.apply_focus(&plan).is_ok());
    }

    #[test]
    fn apply_focus_workspace_only() {
        let api = FakeApi::default();
        let plan = FocusPlan::workspace("w2");
        api.apply_focus(&plan).unwrap();
        assert_eq!(*api.calls.borrow(), vec!["ws:w2"]);
    }

    #[test]
    fn close_pane_is_recorded() {
        let api = FakeApi::default();
        api.close_pane("w1:p1").unwrap();
        assert_eq!(*api.calls.borrow(), vec!["close:w1:p1"]);
    }

    #[test]
    fn parses_created_workspace_id() {
        let json = r#"{"id":"cli:workspace:create","result":{"type":"workspace_info","workspace":{"workspace_id":"wJ","label":"x","number":10},"root_pane":{"pane_id":"wJ:p1"}}}"#;
        assert_eq!(parse_created_workspace_id(json).unwrap(), "wJ");
    }

    #[test]
    fn parses_created_tab_id() {
        let json = r#"{"id":"cli:tab:create","result":{"type":"tab_created","tab":{"tab_id":"w1:t2","workspace_id":"w1","number":2},"root_pane":{"pane_id":"w1:pA"}}}"#;
        assert_eq!(parse_created_tab_id(json).unwrap(), "w1:t2");
    }

    #[test]
    fn launch_context_parses_focused_pane() {
        let json = r#"{"workspace_id":"wC","workspace_label":"proj","tab_id":"wC:t1","focused_pane_id":"wC:p1","focused_pane_agent":"kiro","invocation_source":"api"}"#;
        let ctx = LaunchContext::from_json(json).unwrap();
        assert_eq!(ctx.workspace_id.as_deref(), Some("wC"));
        assert_eq!(ctx.tab_id.as_deref(), Some("wC:t1"));
        assert_eq!(ctx.focused_pane_id.as_deref(), Some("wC:p1"));
    }

    #[test]
    fn launch_context_tolerates_missing_fields() {
        let ctx = LaunchContext::from_json(r#"{"workspace_id":"wA"}"#).unwrap();
        assert_eq!(ctx.workspace_id.as_deref(), Some("wA"));
        assert_eq!(ctx.tab_id, None);
        assert_eq!(ctx.focused_pane_id, None);
    }
}
