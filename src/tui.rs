//! The ratatui/crossterm terminal UI. Owns the alternate-screen/raw-mode
//! lifecycle via a RAII guard so the terminal is always restored, even on a
//! panic. Rendering and key handling delegate to `TreeView` (pure logic).

use std::io::{self, Stdout};
use std::time::Duration;

use anyhow::{Context, Result};
use crossterm::{
    event::{self, Event, KeyCode, KeyEventKind},
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

/// Resolve the pane id to preview for the currently selected row.
fn selected_pane_id(view: &TreeView) -> Option<String> {
    let row = view.selected_row()?;
    row.representative_pane(view.tree()).map(|p| p.pane_id.clone())
}

/// Rebuild the tree after closing `closed_pane`, waiting briefly for herdr to
/// actually drop the pane (and any now-empty tab/workspace) so the UI doesn't
/// momentarily show a dead row. Gives up after a short budget and returns the
/// latest tree regardless.
fn rebuild_without(api: &dyn HerdrApi, closed_pane: &str) -> Result<Tree> {
    use std::time::{Duration, Instant};
    let deadline = Instant::now() + Duration::from_millis(500);
    loop {
        let panes = api.panes()?;
        let still_present = panes.iter().any(|p| p.pane_id == closed_pane);
        if !still_present || Instant::now() >= deadline {
            let tabs = api.tabs()?;
            let workspaces = api.workspaces()?;
            return Ok(Tree::build(
                workspaces,
                tabs,
                panes,
                crate::herdr_client::own_pane_id().as_deref(),
            ));
        }
        std::thread::sleep(Duration::from_millis(25));
    }
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

    // Kind icon so sessions, tabs, and panes are distinguishable at a glance.
    let (kind_icon, kind_color) = match row.kind {
        RowKind::Workspace { .. } => ("◆ ", Color::Cyan),
        RowKind::Tab { .. } => ("▪ ", Color::Gray),
        RowKind::Pane { .. } => ("› ", Color::DarkGray),
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

    // The name. Workspaces bold; focused workspace cyan; selected white+bold.
    let mut name_style = Style::default();
    match row.kind {
        RowKind::Workspace { .. } => {
            name_style = name_style.add_modifier(Modifier::BOLD);
            if row.focused {
                name_style = name_style.fg(Color::Cyan);
            }
        }
        RowKind::Tab { .. } => {
            name_style = name_style.fg(Color::Gray);
        }
        RowKind::Pane { .. } => {}
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
            let mut scroll_ref = scroll;
            guard.terminal().draw(|frame| {
                draw(
                    frame,
                    &rows,
                    selected,
                    &preview_text,
                    &title,
                    status.as_deref(),
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

                    // When a close is pending, intercept keys as a yes/no
                    // confirmation (tmux style): y or Enter confirms, anything
                    // else cancels.
                    if let Some(pane_id) = confirm_close.clone() {
                        match key.code {
                            KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter => {
                                match api.close_pane(&pane_id) {
                                    Ok(()) => {
                                        status = Some(format!("closed {pane_id}"));
                                        // herdr removes the pane (and any now-empty
                                        // tab/workspace) asynchronously. Rebuild
                                        // once it's actually gone so the tree
                                        // doesn't briefly show a dead row.
                                        if let Ok(tree) = rebuild_without(api, &pane_id) {
                                            view.set_tree(tree);
                                        }
                                        // The tree shrank; reset the scroll so the
                                        // view can't be stuck scrolled past the
                                        // new, shorter content.
                                        scroll = 0;
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
                            KeyCode::Char('q') | KeyCode::Esc => return Ok(LoopOutcome::Quit),
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
                                // Arm a tmux-style yes/no confirmation.
                                match view.selected_row().map(|r| r.kind) {
                                    Some(RowKind::Pane { pane_id, tab_id, .. }) => {
                                        if own_tab.as_deref() == Some(tab_id.as_str()) {
                                            // Closing a pane in the overlay's own
                                            // tab would kill the navigator itself.
                                            status = Some(
                                                "can't close a pane in the navigator's own tab"
                                                    .to_string(),
                                            );
                                        } else {
                                            let name = view
                                                .selected_row()
                                                .map(|r| r.name)
                                                .unwrap_or_else(|| pane_id.clone());
                                            confirm_close = Some(pane_id);
                                            status = Some(format!("kill pane \"{name}\"? (y/n)"));
                                        }
                                    }
                                    _ => {
                                        status = Some("select a pane to close".to_string());
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
            " ↑/↓ move   ←/→ fold   ⏎ jump   x close   q quit ",
            Style::default().fg(Color::DarkGray),
        )),
    };
    frame.render_widget(Paragraph::new(bar), outer[1]);
}


#[cfg(test)]
mod tests {
    use super::truncate_display;

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
