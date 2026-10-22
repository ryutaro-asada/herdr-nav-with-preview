//! The ratatui/crossterm terminal UI. Owns the alternate-screen/raw-mode
//! lifecycle via a RAII guard so the terminal is always restored, even on a
//! panic. Rendering and key handling delegate to `TreeView` (pure logic).

use std::io::{self, Stdout};
use std::time::Duration;

use anyhow::{Context, Result};
use crossterm::{
    event::{self, Event, KeyCode, KeyEventKind, KeyModifiers},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout},
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Block, Borders, Paragraph},
    Terminal,
};

use crate::herdr_client::HerdrApi;
use crate::poller::{PollRequest, PollState, Poller};
use crate::tree::Tree;
use crate::view::{RowKind, TreeView};

/// How long to wait for a key event before checking for preview updates.
const EVENT_POLL: Duration = Duration::from_millis(30);

/// RAII guard: enters raw mode + alternate screen on construction and restores
/// the terminal on drop. Installs a panic hook so a panic also restores.
pub struct TerminalGuard {
    terminal: Terminal<CrosstermBackend<Stdout>>,
}

impl TerminalGuard {
    pub fn new() -> Result<Self> {
        let default_hook = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            let _ = disable_raw_mode();
            let _ = execute!(io::stdout(), LeaveAlternateScreen);
            default_hook(info);
        }));

        enable_raw_mode().context("failed to enable raw mode")?;
        let mut stdout = io::stdout();
        execute!(stdout, EnterAlternateScreen).context("failed to enter alternate screen")?;
        let backend = CrosstermBackend::new(stdout);
        let terminal = Terminal::new(backend).context("failed to create terminal")?;
        Ok(Self { terminal })
    }

    pub fn terminal(&mut self) -> &mut Terminal<CrosstermBackend<Stdout>> {
        &mut self.terminal
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(self.terminal.backend_mut(), LeaveAlternateScreen);
        let _ = self.terminal.show_cursor();
    }
}

/// Outcome of the event loop so the caller can act after the terminal has been
/// restored.
pub enum LoopOutcome {
    /// User quit without jumping.
    Quit,
    /// User pressed Enter and focus was moved to this row's target.
    Jumped,
}

/// What the active name-entry prompt will create when confirmed.
enum InputTarget {
    /// Create a new workspace with the entered label in this cwd.
    Workspace { cwd: Option<String> },
    /// Create a new tab in this workspace.
    Tab { workspace_id: String },
}

/// Resolve the pane id to preview for the currently selected row.
fn selected_pane_id(view: &TreeView) -> Option<String> {
    let row = view.selected_row()?;
    row.representative_pane(view.tree()).map(|p| p.pane_id.clone())
}

/// Rebuild the workspace > tab > pane tree from the live herdr state.
fn build_tree(api: &dyn HerdrApi) -> Result<Tree> {
    let mut tree = Tree::build(
        api.workspaces()?,
        api.tabs()?,
        api.panes()?,
        crate::herdr_client::own_pane_id().as_deref(),
    );
    tree.resolve_branches();
    Ok(tree)
}

/// Rebuild the tree right after closing `closed_pane`, filtering out both the
/// navigator's own overlay pane and the just-closed pane. Non-blocking: if
/// herdr's `pane list` hasn't dropped the closed pane yet, we remove it here so
/// no dead row is shown; the next poll converges on the real state.
fn build_tree_excluding(api: &dyn HerdrApi, closed_pane: &str) -> Result<Tree> {
    let mut panes = api.panes()?;
    panes.retain(|p| p.pane_id != closed_pane);
    let mut tree = Tree::build(
        api.workspaces()?,
        api.tabs()?,
        panes,
        crate::herdr_client::own_pane_id().as_deref(),
    );
    tree.resolve_branches();
    Ok(tree)
}

/// Color for an agent status, matching herdr's semantics.
fn status_color(status: Option<&str>) -> Color {
    match status {
        Some("working") => Color::Yellow,
        Some("blocked") => Color::Red,
        Some("idle") => Color::Green,
        Some("done") => Color::Cyan,
        _ => Color::DarkGray,
    }
}

/// Status glyph, chosen to read like herdr's native agent indicators:
/// working = half-filled (in progress), blocked = needs attention,
/// done = finished check, idle = ready dot.
fn status_glyph(status: Option<&str>) -> &'static str {
    match status {
        Some("working") => "◐",
        Some("blocked") => "▲",
        Some("done") => "✓",
        Some("idle") => "●",
        _ => "·",
    }
}

/// Per-kind color: workspace blue, tab purple, pane peach. Chosen to stay
/// distinct from the agent-status dot colors (yellow/red/green/cyan).
fn kind_color(kind: &RowKind) -> Color {
    match kind {
        RowKind::Workspace { .. } => Color::Rgb(130, 170, 255),
        RowKind::Tab { .. } => Color::Rgb(198, 160, 246),
        RowKind::Pane { .. } => Color::Rgb(245, 169, 127),
    }
}

/// Build a styled line for one tree row, herdr-navigator style:
/// `▎ <tree-glyph> <status-dot> <name> [agent]`. When selected, the whole row
/// is filled with a solid highlight background across the full width.
fn row_line(row: &crate::view::Row, selected: bool, width: usize) -> Line<'static> {
    use crate::view::RowKind;

    // Solid fill behind the selected row, like herdr's native cursor.
    let sel_bg = Color::Rgb(68, 80, 120);
    let base = |s: Style| if selected { s.bg(sel_bg) } else { s };

    let mut spans: Vec<Span<'static>> = Vec::new();

    // Selection accent bar in the first column.
    if selected {
        spans.push(Span::styled(
            "▏",
            Style::default().fg(Color::Cyan).bg(sel_bg),
        ));
    } else {
        spans.push(Span::raw(" "));
    }

    // Indent + continuous tree lines for tabs/panes.
    //
    // Each workspace is its own small tree, so we don't draw a vertical line at
    // the workspace level (the first `ancestors_last` entry). For the remaining
    // ancestor levels draw "│ " (ancestor still has siblings below) or "  "
    // (ancestor was the last child), then the node's own branch "├─"/"└─".
    if row.depth > 0 {
        let mut tree_prefix = String::new();
        // Skip the workspace-level ancestor (index 0); start the vertical lines
        // from the tab level downward.
        for &anc_last in row.ancestors_last.iter().skip(1) {
            tree_prefix.push_str(if anc_last { "  " } else { "│ " });
        }
        tree_prefix.push_str(if row.is_last { "└─" } else { "├─" });
        tree_prefix.push(' ');
        spans.push(Span::styled(
            tree_prefix,
            base(Style::default().fg(Color::DarkGray)),
        ));
    }

    // Expand/collapse caret for expandable nodes.
    if row.expandable {
        let caret = if row.expanded { "▾ " } else { "▸ " };
        spans.push(Span::styled(caret, base(Style::default().fg(Color::DarkGray))));
    }

    // Kind icon so workspaces, tabs, and panes are distinguishable at a glance;
    // icon and name share the kind's color.
    let kind_color = kind_color(&row.kind);
    let kind_icon = match row.kind {
        RowKind::Workspace { .. } => "◆ ",
        RowKind::Tab { .. } => "▪ ",
        RowKind::Pane { .. } => "› ",
    };
    spans.push(Span::styled(kind_icon, base(Style::default().fg(kind_color))));

    // Agent status dot: only on pane rows, so the session/tab rows stay clean
    // and the running state reads at the pane level.
    if matches!(row.kind, RowKind::Pane { .. }) {
        let status = row.agent_status.as_deref();
        spans.push(Span::styled(
            format!("{} ", status_glyph(status)),
            base(Style::default().fg(status_color(status))),
        ));
    }

    // The name, in the kind's color. Workspaces bold; the focused workspace is
    // also underlined; the selected row is white+bold for contrast on the fill.
    let mut name_style = Style::default().fg(kind_color);
    if let RowKind::Workspace { .. } = row.kind {
        name_style = name_style.add_modifier(Modifier::BOLD);
        if row.focused {
            name_style = name_style.add_modifier(Modifier::UNDERLINED);
        }
    }
    if selected {
        name_style = name_style.fg(Color::White).add_modifier(Modifier::BOLD);
    }

    // Width already consumed by the prefix spans.
    let used: usize = spans
        .iter()
        .map(|s| unicode_width::UnicodeWidthStr::width(s.content.as_ref()))
        .sum();
    let agent_badge = row.agent.as_deref().map(|a| format!("  {a}"));
    let badge_w = agent_badge
        .as_deref()
        .map(unicode_width::UnicodeWidthStr::width)
        .unwrap_or(0);
    let avail = width.saturating_sub(used);
    let name = truncate_display(&row.name, avail.saturating_sub(badge_w).max(1));
    let name_w = unicode_width::UnicodeWidthStr::width(name.as_str());
    spans.push(Span::styled(name, base(name_style)));
    if let Some(badge) = agent_badge {
        let badge_style = base(Style::default().fg(Color::Gray));
        spans.push(Span::styled(badge, badge_style));
    }

    // Pad to full width so the highlight fill spans the whole row.
    if selected {
        let drawn = used + name_w + badge_w;
        let pad = width.saturating_sub(drawn);
        if pad > 0 {
            spans.push(Span::styled(" ".repeat(pad), Style::default().bg(sel_bg)));
        }
    }

    Line::from(spans)
}

/// `base` if no existing label equals it, otherwise `base-2`, `base-3`, ...
/// (the first free number). Pure.
fn unique_label(base: &str, existing: &[&str]) -> String {
    if !existing.contains(&base) {
        return base.to_string();
    }
    (2..)
        .map(|n| format!("{base}-{n}"))
        .find(|c| !existing.contains(&c.as_str()))
        .expect("unbounded range always yields a free label")
}

/// The project name for a cwd: its last path component (basename), matching
/// herdr's native workspace auto-naming. Empty if the path has no component.
fn project_name(cwd: &str) -> String {
    let trimmed = cwd.trim_end_matches('/');
    trimmed
        .rsplit('/')
        .next()
        .unwrap_or("")
        .to_string()
}

/// Selection/filter state for the directory picker. Keeps absolute paths (used
/// to create the workspace) and their `~`-abbreviated display form (used for
/// matching and drawing), the fuzzy query, the ranked matches for the current
/// query, and the selected index into those matches. Unit-tested.
struct DirPicker {
    paths: Vec<String>,
    displays: Vec<String>,
    query: String,
    ranked: Vec<crate::dirs::Ranked>,
    selected: usize,
}

impl DirPicker {
    fn new(paths: Vec<String>, home: Option<&str>) -> Self {
        let displays = paths
            .iter()
            .map(|p| crate::dirs::abbreviate_home(p, home))
            .collect();
        let mut picker = Self {
            paths,
            displays,
            query: String::new(),
            ranked: Vec::new(),
            selected: 0,
        };
        picker.refilter();
        picker
    }

    fn refilter(&mut self) {
        self.ranked = crate::dirs::rank(&self.displays, &self.query);
        self.selected = 0;
    }

    fn len(&self) -> usize {
        self.ranked.len()
    }

    fn move_down(&mut self) {
        if self.selected + 1 < self.len() {
            self.selected += 1;
        }
    }

    fn move_up(&mut self) {
        if self.selected > 0 {
            self.selected -= 1;
        }
    }

    fn push(&mut self, c: char) {
        self.query.push(c);
        self.refilter();
    }

    fn pop(&mut self) {
        self.query.pop();
        self.refilter();
    }

    fn clear(&mut self) {
        self.query.clear();
        self.refilter();
    }

    /// The absolute path of the currently selected match, if any.
    fn selection(&self) -> Option<String> {
        self.ranked
            .get(self.selected)
            .map(|r| self.paths[r.index].clone())
    }

    /// Display rows for drawing: (display string, matched char positions).
    fn rows(&self) -> Vec<(&str, &[usize])> {
        self.ranked
            .iter()
            .map(|r| (self.displays[r.index].as_str(), r.positions.as_slice()))
            .collect()
    }
}

/// Interactive directory picker: a fuzzy list over `candidates` (absolute
/// paths). Type to filter (fzf-style, ranked), Up/Down or Ctrl+n/Ctrl+p to
/// move, Ctrl+u to clear, Enter to choose, Esc to cancel. Reuses the active
/// `TerminalGuard`. Returns the chosen absolute path, or `None` if cancelled.
fn pick_directory(guard: &mut TerminalGuard, candidates: Vec<String>) -> Result<Option<String>> {
    if candidates.is_empty() {
        return Ok(None);
    }
    let home = std::env::var("HOME").ok();
    let mut picker = DirPicker::new(candidates, home.as_deref());
    let mut scroll: usize = 0;
    let mut dirty = true;
    loop {
        if dirty {
            let rows = picker.rows();
            let selected = picker.selected;
            let query = picker.query.clone();
            let total = picker.paths.len();
            guard.terminal().draw(|frame| {
                draw_dir_picker(frame, &rows, selected, &query, total, &mut scroll)
            })?;
            dirty = false;
        }
        if event::poll(EVENT_POLL)? {
            if let Event::Key(key) = event::read()? {
                if key.kind != KeyEventKind::Press {
                    continue;
                }
                dirty = true;
                let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
                match key.code {
                    KeyCode::Esc => return Ok(None),
                    KeyCode::Enter => return Ok(picker.selection()),
                    KeyCode::Down => picker.move_down(),
                    KeyCode::Up => picker.move_up(),
                    KeyCode::Char('n') | KeyCode::Char('j') if ctrl => picker.move_down(),
                    KeyCode::Char('p') | KeyCode::Char('k') if ctrl => picker.move_up(),
                    KeyCode::Char('u') if ctrl => picker.clear(),
                    KeyCode::Char('c') if ctrl => return Ok(None),
                    KeyCode::Backspace => picker.pop(),
                    // Ignore other control chords instead of typing them.
                    KeyCode::Char(_) if ctrl => {}
                    KeyCode::Char(c) => picker.push(c),
                    _ => {}
                }
            }
        }
    }
}

/// Fit `s` into `width` columns, keeping the end (the project name) and
/// replacing a dropped prefix with `…`. Returns the visible chars as
/// (char, original index) pairs so match highlights stay aligned. Pure.
fn fit_tail(s: &str, width: usize) -> (bool, Vec<(char, usize)>) {
    use unicode_width::UnicodeWidthChar;
    let chars: Vec<char> = s.chars().collect();
    let total: usize = chars.iter().map(|c| c.width().unwrap_or(0)).sum();
    if total <= width {
        return (false, chars.into_iter().enumerate().map(|(i, c)| (c, i)).collect());
    }
    let budget = width.saturating_sub(1);
    let mut used = 0;
    let mut start = chars.len();
    while start > 0 {
        let w = chars[start - 1].width().unwrap_or(0);
        if used + w > budget {
            break;
        }
        used += w;
        start -= 1;
    }
    (
        true,
        chars[start..]
            .iter()
            .enumerate()
            .map(|(k, &c)| (c, start + k))
            .collect(),
    )
}

/// Render the directory picker: the ranked matching directories (matched
/// characters highlighted, selected row filled) and a filter prompt below.
fn draw_dir_picker(
    frame: &mut ratatui::Frame,
    rows: &[(&str, &[usize])],
    selected: usize,
    query: &str,
    total: usize,
    scroll: &mut usize,
) {
    let outer = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1), Constraint::Length(1)])
        .split(frame.area());

    let inner_w = outer[0].width.saturating_sub(2) as usize;
    let inner_h = outer[0].height.saturating_sub(2) as usize;
    let offset = crate::view::scroll_offset(selected, rows.len(), inner_h, *scroll);
    *scroll = offset;

    let sel_bg = Color::Rgb(68, 80, 120);
    let mut lines: Vec<Line> = Vec::new();
    if rows.is_empty() {
        lines.push(Line::from(Span::styled(
            " (no matching directories)",
            Style::default().fg(Color::DarkGray),
        )));
    }
    for (i, (display, positions)) in rows.iter().enumerate().skip(offset).take(inner_h.max(1)) {
        let is_sel = i == selected;
        let base = if is_sel {
            Style::default().bg(sel_bg).fg(Color::White)
        } else {
            Style::default()
        };
        let hl = base.fg(Color::Yellow).add_modifier(Modifier::BOLD);
        let mut spans: Vec<Span> = Vec::new();
        spans.push(Span::styled(
            if is_sel { "▏ " } else { "  " },
            if is_sel { Style::default().fg(Color::Cyan).bg(sel_bg) } else { base },
        ));
        let (cut, visible) = fit_tail(display, inner_w.saturating_sub(2).max(1));
        if cut {
            spans.push(Span::styled("…", base.fg(Color::DarkGray)));
        }
        let mut used = 2 + usize::from(cut);
        for (c, idx) in visible {
            let style = if positions.binary_search(&idx).is_ok() { hl } else { base };
            spans.push(Span::styled(c.to_string(), style));
            used += unicode_width::UnicodeWidthChar::width(c).unwrap_or(0);
        }
        if is_sel && used < inner_w {
            spans.push(Span::styled(" ".repeat(inner_w - used), base));
        }
        lines.push(Line::from(spans));
    }

    let list = Paragraph::new(lines).block(
        Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::DarkGray))
            .title(Span::styled(
                " new workspace — pick a directory ",
                Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
            )),
    );
    frame.render_widget(list, outer[0]);

    let bar = Line::from(vec![
        Span::styled(" > ", Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)),
        Span::styled(format!("{query}\u{2588}"), Style::default().fg(Color::White)),
        Span::styled(
            format!(
                "   {}/{}   ↑↓/^n^p move  ^u clear  ⏎ choose  Esc cancel",
                rows.len(),
                total
            ),
            Style::default().fg(Color::DarkGray),
        ),
    ]);
    frame.render_widget(Paragraph::new(bar), outer[1]);
}

/// Truncate `s` so its display width fits in `max_width` columns, appending an
/// ellipsis when truncated. Honors wide (CJK) characters so Japanese titles
/// don't overrun the pane. Pure and unit-tested.
fn truncate_display(s: &str, max_width: usize) -> String {
    use unicode_width::UnicodeWidthChar;
    if max_width == 0 {
        return String::new();
    }
    let total: usize = s.chars().map(|c| c.width().unwrap_or(0)).sum();
    if total <= max_width {
        return s.to_string();
    }
    // Reserve one column for the ellipsis.
    let budget = max_width.saturating_sub(1);
    let mut out = String::new();
    let mut used = 0usize;
    for c in s.chars() {
        let w = c.width().unwrap_or(0);
        if used + w > budget {
            break;
        }
        out.push(c);
        used += w;
    }
    out.push('…');
    out
}

/// Run the TUI event loop. A background `Poller` refreshes the selected pane's
/// preview once per second; the loop polls for key events with a short timeout
/// and drains preview updates each iteration, applying only current-generation
/// results. The `Poller` is stopped and joined when it drops at scope exit.
pub fn run(view: &mut TreeView, api: &dyn HerdrApi) -> Result<LoopOutcome> {
    let mut guard = TerminalGuard::new()?;

    let mut poll_state = PollState::new();
    let mut current_pane = selected_pane_id(view);
    let generation = poll_state.generation();
    // The preview is parsed to colored Text on the poller thread; the UI only
    // stores and renders it.
    let mut preview_text: Text<'static> = Text::default();
    let mut status: Option<String> = None;
    // Pane id armed for close; set when `x` is first pressed on a pane.
    let mut confirm_close: Option<String> = None;
    // The tab the overlay itself lives in. Closing panes in this tab would
    // take the navigator down with it, so we refuse to close them.
    let own_tab = crate::herdr_client::own_tab_id();
    // When `Some`, we're in the name-entry mode and this holds the input buffer.
    let mut input_buf: Option<String> = None;
    // What the current prompt will create (workspace or tab).
    let mut input_target: Option<InputTarget> = None;
    // True while the incremental search prompt is active (`/`).
    let mut searching = false;

    // The pane/tab/workspace the user was on when they opened the navigator.
    // After a destructive action (close) herdr may re-assign focus within our
    // home workspace, so we re-assert this focus to keep the user's place.
    let home = crate::herdr_client::LaunchContext::from_env();

    let poller = Poller::spawn(PollRequest {
        generation,
        pane_id: current_pane.clone(),
    });

    // Only redraw when something actually changed. This keeps key handling
    // snappy and avoids re-flattening the tree / re-parsing ANSI every tick.
    let mut dirty = true;
    // Persisted vertical scroll offset for the tree (sticky scrolling).
    let mut scroll: usize = 0;

    loop {
        if dirty {
            let rows = view.rows();
            let selected = view.selected_index();
            let title = preview_title(&rows, selected);
            // In text-entry mode, show the prompt (with a cursor) in the bar;
            // otherwise show the current status/help.
            let bar_text = if searching {
                Some(format!("/{}\u{2588}  ({} matches)", view.query(), rows.len()))
            } else {
                input_buf.as_ref().map(|b| {
                    let what = match input_target {
                        Some(InputTarget::Tab { .. }) => "new tab name",
                        _ => "new workspace name",
                    };
                    format!("{what}: {b}\u{2588}")
                })
            };
            let bar = bar_text.as_deref().or(status.as_deref());
            let mut scroll_ref = scroll;
            guard.terminal().draw(|frame| {
                draw(
                    frame,
                    &rows,
                    selected,
                    &preview_text,
                    &title,
                    bar,
                    &mut scroll_ref,
                )
            })?;
            scroll = scroll_ref;
            dirty = false;
        }

        if event::poll(EVENT_POLL)? {
            if let Event::Key(key) = event::read()? {
                if key.kind == KeyEventKind::Press {
                    dirty = true;

                    // Incremental search mode (entered with `/`) takes top
                    // priority: printable keys edit the query and filter live,
                    // but Up/Down still move within the results, and Enter jumps.
                    if searching {
                        match key.code {
                            KeyCode::Esc => {
                                searching = false;
                                view.clear_query();
                            }
                            KeyCode::Enter => {
                                // Confirm the search: keep the filtered view but
                                // leave edit mode so normal keys work. If a row
                                // is selected, jump to it (same as normal Enter).
                                searching = false;
                                if let Some(row) = view.selected_row() {
                                    let plan = row.focus_plan(view.tree());
                                    match api.apply_focus(&plan) {
                                        Ok(()) => return Ok(LoopOutcome::Jumped),
                                        Err(e) => status = Some(format!("focus failed: {e}")),
                                    }
                                }
                            }
                            KeyCode::Backspace => view.pop_query_char(),
                            KeyCode::Down => view.move_down(),
                            KeyCode::Up => view.move_up(),
                            // Ctrl+n / Ctrl+p move within results (fzf-style).
                            // Must be handled before the plain Char arm so the
                            // letters don't get typed into the query.
                            KeyCode::Char('n') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                                view.move_down()
                            }
                            KeyCode::Char('p') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                                view.move_up()
                            }
                            KeyCode::Char(c) => view.push_query_char(c),
                            _ => {}
                        }
                    } else if let Some(buf) = input_buf.as_mut() {
                        match key.code {
                            KeyCode::Enter => {
                                let label = buf.trim().to_string();
                                match input_target.take() {
                                    Some(InputTarget::Workspace { cwd }) => {
                                        let label = if label.is_empty() {
                                            view.tree().next_workspace_number().to_string()
                                        } else {
                                            label
                                        };
                                        match api.create_workspace(&label, cwd.as_deref()) {
                                            Ok(new_id) => {
                                                status =
                                                    Some(format!("created workspace {label}"));
                                                if let Ok(tree) = build_tree(api) {
                                                    view.set_tree(tree);
                                                }
                                                view.select_for_launch(Some(&new_id), None, None);
                                                scroll = 0;
                                                current_pane = None;
                                            }
                                            Err(e) => {
                                                status = Some(format!("create failed: {e}"));
                                            }
                                        }
                                    }
                                    Some(InputTarget::Tab { workspace_id }) => {
                                        // herdr names tabs by number; the entered
                                        // label is only applied when non-empty.
                                        match api.create_tab(&workspace_id) {
                                            Ok(new_tab) => {
                                                if !label.is_empty() {
                                                    let _ = api.rename_tab(&new_tab, &label);
                                                }
                                                status = Some("added tab".to_string());
                                                if let Ok(tree) = build_tree(api) {
                                                    view.set_tree(tree);
                                                }
                                                view.select_for_launch(
                                                    Some(&workspace_id),
                                                    Some(&new_tab),
                                                    None,
                                                );
                                                current_pane = None;
                                            }
                                            Err(e) => {
                                                status = Some(format!("add tab failed: {e}"));
                                            }
                                        }
                                    }
                                    None => {}
                                }
                                input_buf = None;
                            }
                            KeyCode::Esc => {
                                status = None;
                                input_buf = None;
                                input_target = None;
                            }
                            KeyCode::Backspace => {
                                buf.pop();
                            }
                            KeyCode::Char(c) => {
                                buf.push(c);
                            }
                            _ => {}
                        }
                    } else if let Some(pane_id) = confirm_close.clone() {
                        // When a close is pending, intercept keys as a yes/no
                        // confirmation (tmux style): y or Enter confirms,
                        // anything else cancels.
                        match key.code {
                            KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter => {
                                match api.close_pane(&pane_id) {
                                    Ok(()) => {
                                        status = Some(format!("closed {pane_id}"));
                                        // Rebuild immediately (non-blocking) and
                                        // defensively drop the just-closed pane in
                                        // case herdr's list hasn't updated yet.
                                        let fresh = build_tree_excluding(api, &pane_id);
                                        if let Ok(tree) = &fresh {
                                            view.set_tree(tree.clone());
                                        }
                                        scroll = 0;
                                        current_pane = None;
                                        // When running as an overlay *pane*, herdr
                                        // may move focus within our home workspace
                                        // on close, so we re-assert the user's
                                        // original pane. As a popup we must NOT
                                        // change focus here: a focus change while
                                        // the popup is open dismisses it (flinging
                                        // the user out of nav). Popups restore the
                                        // prior focus automatically when closed.
                                        let is_popup =
                                            crate::herdr_client::own_pane_id().is_none();
                                        if !is_popup {
                                            if let (Some(hp), Ok(tree)) =
                                                (home.focused_pane_id.as_deref(), &fresh)
                                            {
                                                let still_there = tree
                                                    .workspaces
                                                    .iter()
                                                    .flat_map(|w| &w.tabs)
                                                    .flat_map(|t| &t.panes)
                                                    .any(|p| p.pane_id == hp);
                                                if hp != pane_id && still_there {
                                                    if let Some(ws) =
                                                        home.workspace_id.as_deref()
                                                    {
                                                        let _ = api.focus_workspace(ws);
                                                    }
                                                    if let Some(tab) = home.tab_id.as_deref() {
                                                        let _ = api.focus_tab(tab);
                                                    }
                                                }
                                            }
                                        }
                                        // Repaint: closing can leave a dirty
                                        // backbuffer behind the popup.
                                        guard.terminal().clear().ok();
                                    }
                                    Err(e) => {
                                        status = Some(format!("close failed: {e}"));
                                    }
                                }
                            }
                            _ => {
                                status = None;
                            }
                        }
                        confirm_close = None;
                    } else {
                        match key.code {
                            KeyCode::Char('q') => return Ok(LoopOutcome::Quit),
                            KeyCode::Esc => {
                                // Esc clears an active search filter first;
                                // otherwise it quits.
                                if view.is_searching() {
                                    view.clear_query();
                                } else {
                                    return Ok(LoopOutcome::Quit);
                                }
                            }
                            KeyCode::Char('/') => {
                                // Enter incremental search mode.
                                searching = true;
                                view.clear_query();
                                status = None;
                            }
                            KeyCode::Char('n') => {
                                // Open a prompt to add a tab to the selected
                                // workspace. Default name is the next tab number.
                                let ws = view.selected_row().map(|r| match r.kind {
                                    RowKind::Workspace { workspace_id } => workspace_id,
                                    RowKind::Tab { workspace_id, .. } => workspace_id,
                                    RowKind::Pane { workspace_id, .. } => workspace_id,
                                });
                                match ws {
                                    Some(wid) => {
                                        let default =
                                            view.tree().next_tab_number(&wid).to_string();
                                        status = None;
                                        input_target =
                                            Some(InputTarget::Tab { workspace_id: wid });
                                        input_buf = Some(default);
                                    }
                                    None => {
                                        status =
                                            Some("select a workspace to add a tab".to_string());
                                    }
                                }
                            }
                            KeyCode::Char('N') => {
                                // Add a workspace: first pick the directory via a
                                // fuzzy picker (current cwd, zoxide, and scanned
                                // roots), then open the name prompt for it.
                                let current_cwd = view.selected_row().and_then(|r| {
                                    r.representative_pane(view.tree())
                                        .and_then(|p| p.cwd.clone())
                                });
                                let open_cwds: Vec<String> = view
                                    .tree()
                                    .workspaces
                                    .iter()
                                    .flat_map(|w| w.tabs.iter())
                                    .flat_map(|t| t.panes.iter())
                                    .filter_map(|p| p.cwd.clone())
                                    .collect();
                                let candidates = crate::dirs::collect_candidates(
                                    current_cwd.as_deref(),
                                    &open_cwds,
                                );
                                let chosen = pick_directory(&mut guard, candidates)?;
                                // The picker painted its own screen; repaint the
                                // tree regardless of the outcome.
                                guard.terminal().clear().ok();
                                if let Some(dir) = chosen {
                                    // Default name = project dir name, made unique
                                    // among existing workspace labels (`name-2`, ...).
                                    // No branch: it would go stale on checkout (the
                                    // tree shows the live branch anyway).
                                    let name = project_name(&dir);
                                    let default = if name.is_empty() {
                                        view.tree().next_workspace_number().to_string()
                                    } else {
                                        let labels: Vec<&str> = view
                                            .tree()
                                            .workspaces
                                            .iter()
                                            .map(|w| w.label.as_str())
                                            .collect();
                                        unique_label(&name, &labels)
                                    };
                                    status = None;
                                    input_target = Some(InputTarget::Workspace {
                                        cwd: Some(dir),
                                    });
                                    input_buf = Some(default);
                                } else {
                                    status = None; // cancelled
                                }
                            }
                            KeyCode::Enter => {
                                if let Some(row) = view.selected_row() {
                                    let plan = row.focus_plan(view.tree());
                                    match api.apply_focus(&plan) {
                                        Ok(()) => return Ok(LoopOutcome::Jumped),
                                        Err(e) => {
                                            status = Some(format!("focus failed: {e}"));
                                        }
                                    }
                                }
                            }
                            KeyCode::Down | KeyCode::Char('j') => view.move_down(),
                            KeyCode::Up | KeyCode::Char('k') => view.move_up(),
                            KeyCode::Right | KeyCode::Char('l') => view.expand(),
                            KeyCode::Left | KeyCode::Char('h') => view.collapse(),
                            KeyCode::Char(' ') => view.toggle(),
                            KeyCode::Char('x') => {
                                // Arm a close confirmation for the selected row's
                                // representative pane. Works on pane, tab, and
                                // workspace rows (each resolves to a pane), so a
                                // following Enter confirms the close instead of
                                // falling through to the jump action.
                                let target = view.selected_row().and_then(|r| {
                                    r.representative_pane(view.tree()).map(|p| {
                                        (p.pane_id.clone(), p.tab_id.clone(), r.name.clone())
                                    })
                                });
                                match target {
                                    Some((pid, tid, name)) => {
                                        if own_tab.as_deref() == Some(tid.as_str()) {
                                            // This pane lives in the navigator's own
                                            // tab; closing it could disturb nav.
                                            status = Some(
                                                "can't close a pane in the navigator's own tab"
                                                    .to_string(),
                                            );
                                        } else {
                                            confirm_close = Some(pid);
                                            status = Some(format!(
                                                "kill pane \"{name}\"?  Enter = yes   Esc = no"
                                            ));
                                        }
                                    }
                                    None => {
                                        status = Some("nothing to close".to_string());
                                    }
                                }
                            }
                            _ => {
                                dirty = false;
                            }
                        }
                    }
                }
            }
        }

        // On selection change, bump the generation, clear the stale preview,
        // and retarget the poller.
        let new_pane = selected_pane_id(view);
        if new_pane != current_pane {
            let gen = poll_state.bump();
            preview_text = Text::default();
            dirty = true;
            poller.set_request(PollRequest {
                generation: gen,
                pane_id: new_pane.clone(),
            });
            current_pane = new_pane;
        }

        // Drain any pending updates; apply only the current generation. The
        // text is already ANSI-parsed by the poller, so this is just a move.
        while let Ok(update) = poller.updates.try_recv() {
            if poll_state.accepts(update.generation)
                && Some(&update.pane_id) == current_pane.as_ref()
            {
                preview_text = update.text;
                dirty = true;
            }
        }
    }
}

/// Title for the preview block based on the selected row.
fn preview_title(rows: &[crate::view::Row], selected: usize) -> String {
    match rows.get(selected).map(|r| &r.kind) {
        Some(RowKind::Pane { pane_id, .. }) => format!(" preview: {pane_id} "),
        Some(RowKind::Tab { tab_id, .. }) => format!(" preview: {tab_id} "),
        Some(RowKind::Workspace { workspace_id }) => format!(" preview: {workspace_id} "),
        None => " preview ".to_string(),
    }
}

/// Draw the two-pane layout: tree on the left, preview on the right, plus a
/// help/status bar at the bottom. `rows` is precomputed by the caller so the
/// tree is only flattened once per frame, and `preview_text` is the already
/// ANSI-parsed preview so parsing is not repeated every frame.
pub fn draw(
    frame: &mut ratatui::Frame,
    rows: &[crate::view::Row],
    selected: usize,
    preview_text: &Text<'static>,
    preview_title: &str,
    status: Option<&str>,
    prev_offset: &mut usize,
) {
    let outer = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1), Constraint::Length(1)])
        .split(frame.area());

    let chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(45), Constraint::Percentage(55)])
        .split(outer[0]);

    // Vertical scroll: keep the selected row visible, scrolling the minimum
    // from the previous offset so moving up doesn't drag the view and clip the
    // bottom. Inner height excludes the top/bottom border rows.
    let tree_inner_height = chunks[0].height.saturating_sub(2) as usize;
    let offset = crate::view::scroll_offset(selected, rows.len(), tree_inner_height, *prev_offset);
    *prev_offset = offset;
    // Inner width excludes the left/right borders; used to trim long labels.
    let tree_inner_width = chunks[0].width.saturating_sub(2) as usize;

    let mut lines: Vec<Line> = Vec::with_capacity(tree_inner_height.max(1));
    for (i, row) in rows
        .iter()
        .enumerate()
        .skip(offset)
        .take(tree_inner_height.max(1))
    {
        lines.push(row_line(row, i == selected, tree_inner_width));
    }
    if lines.is_empty() {
        lines.push(Line::from(Span::styled(
            " No workspaces open — start one in herdr first.",
            Style::default().fg(Color::DarkGray),
        )));
    }

    let tree_widget = Paragraph::new(lines).block(
        Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::DarkGray))
            .title(Span::styled(
                " spaces ",
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            )),
    );
    frame.render_widget(tree_widget, chunks[0]);

    let preview_widget = Paragraph::new(preview_text.clone()).block(
        Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::DarkGray))
            .title(Span::styled(
                preview_title.to_string(),
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            )),
    );
    frame.render_widget(preview_widget, chunks[1]);

    // Bottom help/status bar.
    let bar = match status {
        Some(s) => Line::from(Span::styled(
            format!(" {s} "),
            Style::default()
                .fg(Color::Red)
                .add_modifier(Modifier::BOLD),
        )),
        None => Line::from(Span::styled(
            " ↑/↓ move   ←/→ fold   / search   ⏎ jump   n tab   N space   x close   q quit ",
            Style::default().fg(Color::DarkGray),
        )),
    };
    frame.render_widget(Paragraph::new(bar), outer[1]);
}


#[cfg(test)]
mod tests {
    use super::truncate_display;
    use super::project_name;

    #[test]
    fn project_name_is_basename() {
        assert_eq!(project_name("/Users/asaryu/job/tmp1/sptv-video"), "sptv-video");
        assert_eq!(project_name("/Users/asaryu/job/tmp1/sptv-video/"), "sptv-video");
        assert_eq!(project_name("/"), "");
        assert_eq!(project_name("plain"), "plain");
        assert_eq!(project_name("~/repos/herder-nav-with-preview"), "herder-nav-with-preview");
    }

    #[test]
    fn no_truncation_when_fits() {
        assert_eq!(truncate_display("hello", 10), "hello");
        assert_eq!(truncate_display("hello", 5), "hello");
    }

    #[test]
    fn ascii_truncates_with_ellipsis() {
        // 10 chars into width 5 -> 4 chars + ellipsis.
        assert_eq!(truncate_display("abcdefghij", 5), "abcd…");
    }

    #[test]
    fn wide_chars_counted_as_two() {
        // Each CJK char is width 2. Width budget 5 -> ellipsis reserves 1,
        // leaving 4 cols = two wide chars.
        assert_eq!(truncate_display("あいうえお", 5), "あい…");
    }

    #[test]
    fn zero_width_is_empty() {
        assert_eq!(truncate_display("anything", 0), "");
    }
}
