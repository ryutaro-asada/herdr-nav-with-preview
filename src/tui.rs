//! The ratatui/crossterm terminal UI. Owns the alternate-screen/raw-mode
//! lifecycle via a RAII guard so the terminal is always restored, even on a
//! panic. Rendering and key handling delegate to `TreeView` (pure logic).

use std::io::{self, Stdout};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use crossterm::{
    event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers},
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

/// Input mode of the event loop; key handling depends on it.
enum Mode {
    Normal,
    /// Incremental search (`/`).
    Search,
    /// Name entry for a new workspace or tab.
    Prompt {
        target: InputTarget,
        text: String,
    },
    /// `x` armed a close of this pane; the next key confirms or cancels.
    ConfirmClose {
        pane_id: String,
    },
}

/// What the event loop does after a key press.
enum Flow {
    Redraw,
    /// The key has no binding; nothing changed, so skip the redraw.
    Unhandled,
    Exit(LoopOutcome),
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
    row.representative_pane(view.tree())
        .map(|p| p.pane_id.clone())
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

/// Braille spinner frames for the `working` state.
const SPINNER: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// How often the spinner advances (also the redraw tick while anything works).
const SPINNER_INTERVAL: Duration = Duration::from_millis(100);

/// Status glyph: working = animated spinner (`tick` picks the frame),
/// blocked = needs attention, done = finished check, idle = ready dot.
fn status_glyph(status: Option<&str>, tick: usize) -> &'static str {
    match status {
        Some("working") => SPINNER[tick % SPINNER.len()],
        Some("blocked") => "▲",
        Some("done") => "✓",
        Some("idle") => "●",
        _ => "·",
    }
}

/// Per-kind color: workspace blue, tab purple, pane peach. Chosen to stay
/// distinct from the agent-status dot colors (yellow/red/green/cyan).
/// Is this an "active" status worth flagging loudly (working/blocked/done)?
fn is_notable_status(status: Option<&str>) -> bool {
    matches!(status, Some("working" | "blocked" | "done"))
}

/// Human-facing word for a herdr agent status: `blocked` (needs your input
/// or permission) reads as "waiting", `done` (finished, not yet looked at) as
/// "complete". `None` for unknown/no agent.
fn status_word(status: Option<&str>) -> Option<&'static str> {
    match status {
        Some("working") => Some("working"),
        Some("blocked") => Some("waiting"),
        Some("done") => Some("complete"),
        Some("idle") => Some("idle"),
        _ => None,
    }
}

/// Filled status chip for agent pane rows: working (yellow), waiting (red,
/// needs you), complete (cyan), idle (muted gray). `None` without an agent.
fn status_chip(status: Option<&str>) -> Option<(String, Style)> {
    let word = status_word(status)?;
    let style = match status {
        Some("working") => Style::default().fg(Color::Black).bg(Color::Yellow),
        Some("blocked") => Style::default().fg(Color::White).bg(Color::Red),
        Some("done") => Style::default().fg(Color::Black).bg(Color::Cyan),
        _ => Style::default().fg(Color::Gray).bg(Color::Rgb(60, 60, 70)),
    };
    let style = if status == Some("idle") {
        style
    } else {
        style.add_modifier(Modifier::BOLD)
    };
    Some((format!(" {word} "), style))
}

/// Counts of panes per notable status, for the tree title summary:
/// `(working, blocked, done)`.
fn status_counts(tree: &Tree) -> (usize, usize, usize) {
    let mut c = (0, 0, 0);
    for p in tree.panes() {
        match p.agent_status.as_deref() {
            Some("working") => c.0 += 1,
            Some("blocked") => c.1 += 1,
            Some("done") => c.2 += 1,
            _ => {}
        }
    }
    c
}

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
fn row_line(row: &crate::view::Row, selected: bool, width: usize, tick: usize) -> Line<'static> {
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
        spans.push(Span::styled(
            caret,
            base(Style::default().fg(Color::DarkGray)),
        ));
    }

    // Kind icon so workspaces, tabs, and panes are distinguishable at a glance;
    // icon and name share the kind's color.
    let kind_color = kind_color(&row.kind);
    let kind_icon = match row.kind {
        RowKind::Workspace { .. } => "◆ ",
        RowKind::Tab { .. } => "▪ ",
        RowKind::Pane { .. } => "› ",
    };
    spans.push(Span::styled(
        kind_icon,
        base(Style::default().fg(kind_color)),
    ));

    // Agent status glyph. Pane rows always show it; workspace/tab rows show
    // herdr's aggregated status only when notable (working/blocked/done), so
    // activity is visible even when the node is collapsed.
    let status = row.agent_status.as_deref();
    let is_pane = matches!(row.kind, RowKind::Pane { .. });
    if is_pane || is_notable_status(status) {
        let mut st = Style::default().fg(status_color(status));
        if is_notable_status(status) {
            st = st.add_modifier(Modifier::BOLD);
        }
        spans.push(Span::styled(
            format!("{} ", status_glyph(status, tick)),
            base(st),
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
    // Right-aligned status chip on pane rows (working/blocked/done).
    let chip = if is_pane { status_chip(status) } else { None };
    let chip_w = chip
        .as_ref()
        .map(|(t, _)| unicode_width::UnicodeWidthStr::width(t.as_str()) + 1)
        .unwrap_or(0);
    let avail = width.saturating_sub(used);
    let name = truncate_display(&row.name, avail.saturating_sub(badge_w + chip_w).max(1));
    let name_w = unicode_width::UnicodeWidthStr::width(name.as_str());
    spans.push(Span::styled(name, base(name_style)));
    if let Some(badge) = agent_badge {
        let badge_style = base(Style::default().fg(Color::Gray));
        spans.push(Span::styled(badge, badge_style));
    }

    // Pad so the chip sits at the right edge and (when selected) the highlight
    // fill spans the whole row.
    let drawn = used + name_w + badge_w;
    let pad = width.saturating_sub(drawn + chip_w);
    if pad > 0 && (selected || chip.is_some()) {
        spans.push(Span::styled(" ".repeat(pad), base(Style::default())));
    }
    if let Some((text, style)) = chip {
        spans.push(Span::styled(text, style));
        spans.push(Span::styled(" ", base(Style::default())));
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
    trimmed.rsplit('/').next().unwrap_or("").to_string()
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
        return (
            false,
            chars.into_iter().enumerate().map(|(i, c)| (c, i)).collect(),
        );
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
            if is_sel {
                Style::default().fg(Color::Cyan).bg(sel_bg)
            } else {
                base
            },
        ));
        let (cut, visible) = fit_tail(display, inner_w.saturating_sub(2).max(1));
        if cut {
            spans.push(Span::styled("…", base.fg(Color::DarkGray)));
        }
        let mut used = 2 + usize::from(cut);
        for (c, idx) in visible {
            let style = if positions.binary_search(&idx).is_ok() {
                hl
            } else {
                base
            };
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
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            )),
    );
    frame.render_widget(list, outer[0]);

    let bar = Line::from(vec![
        Span::styled(
            " > ",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!("{query}\u{2588}"),
            Style::default().fg(Color::White),
        ),
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
    let mut app = App::new(view, api);

    let mut poll_state = PollState::new();
    let poller = Poller::spawn(PollRequest {
        generation: poll_state.generation(),
        pane_id: app.current_pane.clone(),
    });
    // Periodic tree refresh so agent statuses (and added/closed panes) stay
    // live while the navigator is open.
    let tree_poller = crate::poller::TreePoller::spawn(|| {
        Tree::fetch(&crate::herdr_client::CliHerdr::new(), None).ok()
    });
    // The preview is parsed to colored Text on the poller thread; the UI only
    // stores and renders it.
    let mut preview_text: Text<'static> = Text::default();

    // Only redraw when something actually changed. This keeps key handling
    // snappy and avoids re-flattening the tree / re-parsing ANSI every tick.
    let mut dirty = true;
    // Spinner animation: the frame index derives from elapsed time, and we
    // redraw on each new frame only while some agent is working.
    let started = Instant::now();
    let mut tick: usize = 0;

    loop {
        let counts = status_counts(app.view.tree());
        let now_tick = (started.elapsed().as_millis() / SPINNER_INTERVAL.as_millis()) as usize;
        if now_tick != tick {
            tick = now_tick;
            if counts.0 > 0 {
                dirty = true;
            }
        }
        if dirty {
            app.render(&mut guard, &preview_text, tick, counts)?;
            dirty = false;
        }

        if event::poll(EVENT_POLL)? {
            if let Event::Key(key) = event::read()? {
                if key.kind == KeyEventKind::Press {
                    match app.handle_key(key, &mut guard)? {
                        Flow::Redraw => dirty = true,
                        Flow::Unhandled => {}
                        Flow::Exit(outcome) => return Ok(outcome),
                    }
                }
            }
        }

        // Apply the latest polled tree (statuses etc.), keeping the selection
        // on the same node. The preview retargeting below handles the case
        // where the selected pane vanished.
        let mut latest = None;
        while let Ok(tree) = tree_poller.updates.try_recv() {
            latest = Some(tree);
        }
        if let Some(tree) = latest {
            if Instant::now() >= app.tree_hold_until && app.view.refresh_tree(tree) {
                dirty = true;
            }
        }

        // On selection change, bump the generation, clear the stale preview,
        // and retarget the poller.
        let new_pane = selected_pane_id(app.view);
        if new_pane != app.current_pane {
            let gen = poll_state.bump();
            preview_text = Text::default();
            dirty = true;
            poller.set_request(PollRequest {
                generation: gen,
                pane_id: new_pane.clone(),
            });
            app.current_pane = new_pane;
        }

        // Drain any pending updates; apply only the current generation. The
        // text is already ANSI-parsed by the poller, so this is just a move.
        while let Ok(update) = poller.updates.try_recv() {
            if poll_state.accepts(update.generation)
                && Some(&update.pane_id) == app.current_pane.as_ref()
            {
                preview_text = update.text;
                dirty = true;
            }
        }
    }
}

/// Key-handling state of the event loop.
struct App<'a> {
    view: &'a mut TreeView,
    api: &'a dyn HerdrApi,
    mode: Mode,
    status: Option<String>,
    /// Persisted vertical scroll offset for the tree (sticky scrolling).
    scroll: usize,
    /// Pane the preview poller targets. Set to `None` to force a retarget.
    current_pane: Option<String>,
    /// After we mutate the tree ourselves (create/close), ignore polled trees
    /// until this deadline: one built just before the change could resurrect
    /// a closed pane or drop a just-created one.
    tree_hold_until: Instant,
    /// The tab the overlay itself lives in. Closing panes in this tab would
    /// take the navigator down with it, so we refuse to close them.
    own_tab: Option<String>,
    /// The pane/tab/workspace the user was on when they opened the navigator.
    /// After a close, herdr may re-assign focus within our home workspace, so
    /// we re-assert this focus to keep the user's place.
    home: crate::herdr_client::LaunchContext,
}

impl<'a> App<'a> {
    fn new(view: &'a mut TreeView, api: &'a dyn HerdrApi) -> Self {
        let current_pane = selected_pane_id(view);
        Self {
            view,
            api,
            mode: Mode::Normal,
            status: None,
            scroll: 0,
            current_pane,
            tree_hold_until: Instant::now(),
            own_tab: crate::herdr_client::own_tab_id(),
            home: crate::herdr_client::LaunchContext::from_env(),
        }
    }

    fn render(
        &mut self,
        guard: &mut TerminalGuard,
        preview: &Text<'static>,
        tick: usize,
        counts: (usize, usize, usize),
    ) -> Result<()> {
        let rows = self.view.rows();
        // In text-entry modes, show the prompt (with a cursor) in the bar
        // instead of the status/help.
        let prompt = match &self.mode {
            Mode::Search => Some(format!(
                "/{}\u{2588}  ({} matches)",
                self.view.query(),
                rows.len()
            )),
            Mode::Prompt { target, text } => {
                let what = match target {
                    InputTarget::Tab { .. } => "new tab name (empty = auto)",
                    InputTarget::Workspace { .. } => "new workspace name",
                };
                Some(format!("{what}: {text}\u{2588}"))
            }
            Mode::Normal | Mode::ConfirmClose { .. } => None,
        };
        let screen = Screen {
            rows: &rows,
            selected: self.view.selected_index(),
            preview,
            status: prompt.as_deref().or(self.status.as_deref()),
            tick,
            counts,
        };
        let scroll = &mut self.scroll;
        guard
            .terminal()
            .draw(|frame| draw(frame, &screen, scroll))?;
        Ok(())
    }

    fn handle_key(&mut self, key: KeyEvent, guard: &mut TerminalGuard) -> Result<Flow> {
        // Each handler sets `self.mode` again if the mode continues.
        match std::mem::replace(&mut self.mode, Mode::Normal) {
            Mode::Search => Ok(self.on_search_key(key)),
            Mode::Prompt { target, text } => {
                self.on_prompt_key(key, target, text);
                Ok(Flow::Redraw)
            }
            Mode::ConfirmClose { pane_id } => {
                self.on_confirm_close_key(key, &pane_id, guard);
                Ok(Flow::Redraw)
            }
            Mode::Normal => self.on_normal_key(key, guard),
        }
    }

    /// Incremental search: printable keys edit the query and filter live,
    /// Up/Down (and Ctrl+n/Ctrl+p, fzf-style) move within the results.
    fn on_search_key(&mut self, key: KeyEvent) -> Flow {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Esc => {
                self.view.clear_query();
                return Flow::Redraw;
            }
            // Leave edit mode but keep the filtered view, and jump to the
            // selected row like a normal Enter.
            KeyCode::Enter => return self.jump(),
            KeyCode::Backspace => self.view.pop_query_char(),
            KeyCode::Down => self.view.move_down(),
            KeyCode::Up => self.view.move_up(),
            // Before the plain Char arm so the letters aren't typed.
            KeyCode::Char('n') if ctrl => self.view.move_down(),
            KeyCode::Char('p') if ctrl => self.view.move_up(),
            KeyCode::Char(c) => self.view.push_query_char(c),
            _ => {}
        }
        self.mode = Mode::Search;
        Flow::Redraw
    }

    fn on_prompt_key(&mut self, key: KeyEvent, target: InputTarget, mut text: String) {
        match key.code {
            KeyCode::Enter => self.submit_prompt(target, text.trim()),
            KeyCode::Esc => self.status = None,
            code => {
                match code {
                    KeyCode::Backspace => {
                        text.pop();
                    }
                    KeyCode::Char(c) => text.push(c),
                    _ => {}
                }
                self.mode = Mode::Prompt { target, text };
            }
        }
    }

    fn submit_prompt(&mut self, target: InputTarget, label: &str) {
        match target {
            InputTarget::Workspace { cwd } => {
                let label = if label.is_empty() {
                    self.view.tree().next_workspace_number().to_string()
                } else {
                    label.to_string()
                };
                match self.api.create_workspace(&label, cwd.as_deref()) {
                    Ok(new_id) => {
                        self.status = Some(format!("created workspace {label}"));
                        self.reload_tree();
                        self.view.select_for_launch(Some(&new_id), None, None);
                        self.scroll = 0;
                        self.after_mutation();
                    }
                    Err(e) => self.status = Some(format!("create failed: {e}")),
                }
            }
            InputTarget::Tab { workspace_id } => match self.api.create_tab(&workspace_id) {
                Ok(new_tab) => {
                    // An empty name leaves the tab unnamed, so herdr (or a
                    // naming plugin such as herdr-auto-title) names it.
                    if !label.is_empty() {
                        let _ = self.api.rename_tab(&new_tab, label);
                    }
                    self.status = Some("added tab".to_string());
                    self.reload_tree();
                    self.view
                        .select_for_launch(Some(&workspace_id), Some(&new_tab), None);
                    self.after_mutation();
                }
                Err(e) => self.status = Some(format!("add tab failed: {e}")),
            },
        }
    }

    /// tmux-style confirmation: `y` or Enter closes the armed pane, anything
    /// else cancels.
    fn on_confirm_close_key(&mut self, key: KeyEvent, pane_id: &str, guard: &mut TerminalGuard) {
        if !matches!(key.code, KeyCode::Char('y' | 'Y') | KeyCode::Enter) {
            self.status = None;
            return;
        }
        if let Err(e) = self.api.close_pane(pane_id) {
            self.status = Some(format!("close failed: {e}"));
            return;
        }
        self.status = Some(format!("closed {pane_id}"));
        // Drop the just-closed pane explicitly in case herdr's `pane list`
        // hasn't caught up yet; the next poll converges on the real state.
        let refreshed = match Tree::fetch(self.api, Some(pane_id)) {
            Ok(tree) => {
                self.view.set_tree(tree);
                true
            }
            Err(_) => false,
        };
        self.scroll = 0;
        self.after_mutation();
        if refreshed {
            self.restore_home_focus(pane_id);
        }
        // Repaint: closing can leave a dirty backbuffer behind the popup.
        guard.terminal().clear().ok();
    }

    /// Re-focus the pane the user launched the navigator from, if it still
    /// exists and wasn't the one just closed.
    fn restore_home_focus(&self, closed_pane: &str) {
        // Only when running as an overlay *pane*: herdr may move focus within
        // our home workspace on close. As a popup we must NOT change focus: a
        // focus change while the popup is open dismisses it (flinging the
        // user out of nav). Popups restore the prior focus when closed.
        let is_popup = crate::herdr_client::own_pane_id().is_none();
        if is_popup {
            return;
        }
        let Some(home_pane) = self.home.focused_pane_id.as_deref() else {
            return;
        };
        let still_there = self.view.tree().panes().any(|p| p.pane_id == home_pane);
        if home_pane == closed_pane || !still_there {
            return;
        }
        if let Some(ws) = self.home.workspace_id.as_deref() {
            let _ = self.api.focus_workspace(ws);
        }
        if let Some(tab) = self.home.tab_id.as_deref() {
            let _ = self.api.focus_tab(tab);
        }
    }

    fn on_normal_key(&mut self, key: KeyEvent, guard: &mut TerminalGuard) -> Result<Flow> {
        match key.code {
            KeyCode::Char('q') => return Ok(Flow::Exit(LoopOutcome::Quit)),
            // Esc clears an active search filter first; otherwise it quits.
            KeyCode::Esc => {
                if self.view.is_searching() {
                    self.view.clear_query();
                } else {
                    return Ok(Flow::Exit(LoopOutcome::Quit));
                }
            }
            KeyCode::Char('/') => {
                self.mode = Mode::Search;
                self.view.clear_query();
                self.status = None;
            }
            KeyCode::Char('n') => self.start_tab_prompt(),
            KeyCode::Char('N') => self.start_workspace_prompt(guard)?,
            KeyCode::Enter => return Ok(self.jump()),
            KeyCode::Down | KeyCode::Char('j') => self.view.move_down(),
            KeyCode::Up | KeyCode::Char('k') => self.view.move_up(),
            KeyCode::Right | KeyCode::Char('l') => self.view.expand(),
            KeyCode::Left | KeyCode::Char('h') => self.view.collapse(),
            KeyCode::Char(' ') => self.view.toggle(),
            KeyCode::Char('x') => self.arm_close(),
            _ => return Ok(Flow::Unhandled),
        }
        Ok(Flow::Redraw)
    }

    /// Focus the selected row's target; the navigator exits on success.
    fn jump(&mut self) -> Flow {
        if let Some(row) = self.view.selected_row() {
            match self.api.apply_focus(&row.focus_plan(self.view.tree())) {
                Ok(()) => return Flow::Exit(LoopOutcome::Jumped),
                Err(e) => self.status = Some(format!("focus failed: {e}")),
            }
        }
        Flow::Redraw
    }

    /// Open an (initially empty) name prompt for a new tab in the selected
    /// row's workspace.
    fn start_tab_prompt(&mut self) {
        match self.view.selected_row() {
            Some(row) => {
                self.status = None;
                self.mode = Mode::Prompt {
                    target: InputTarget::Tab {
                        workspace_id: row.kind.workspace_id().to_string(),
                    },
                    text: String::new(),
                };
            }
            None => self.status = Some("select a workspace to add a tab".to_string()),
        }
    }

    /// Pick a directory with the fuzzy picker (current cwd, zoxide, and
    /// scanned roots), then open the name prompt for a workspace there.
    fn start_workspace_prompt(&mut self, guard: &mut TerminalGuard) -> Result<()> {
        let tree = self.view.tree();
        let current_cwd = self
            .view
            .selected_row()
            .and_then(|r| r.representative_pane(tree).and_then(|p| p.cwd.clone()));
        let open_cwds: Vec<String> = tree.panes().filter_map(|p| p.cwd.clone()).collect();
        let candidates = crate::dirs::collect_candidates(current_cwd.as_deref(), &open_cwds);
        let chosen = pick_directory(guard, candidates)?;
        // The picker painted its own screen; repaint the tree regardless of
        // the outcome.
        guard.terminal().clear().ok();
        self.status = None;
        if let Some(dir) = chosen {
            // Default name = project dir name, made unique among existing
            // workspace labels (`name-2`, ...). No branch: it would go stale
            // on checkout (the tree shows the live branch anyway).
            let name = project_name(&dir);
            let tree = self.view.tree();
            let default = if name.is_empty() {
                tree.next_workspace_number().to_string()
            } else {
                let labels: Vec<&str> = tree.workspaces.iter().map(|w| w.label.as_str()).collect();
                unique_label(&name, &labels)
            };
            self.mode = Mode::Prompt {
                target: InputTarget::Workspace { cwd: Some(dir) },
                text: default,
            };
        }
        Ok(())
    }

    /// Arm a close confirmation for the selected row's representative pane.
    /// Works on pane, tab, and workspace rows (each resolves to a pane), so a
    /// following Enter confirms the close instead of jumping.
    fn arm_close(&mut self) {
        let target = self.view.selected_row().and_then(|r| {
            r.representative_pane(self.view.tree())
                .map(|p| (p.pane_id.clone(), p.tab_id.clone(), r.name.clone()))
        });
        self.status = Some(match target {
            None => "nothing to close".to_string(),
            Some((_, tab_id, _)) if self.own_tab.as_deref() == Some(tab_id.as_str()) => {
                "can't close a pane in the navigator's own tab".to_string()
            }
            Some((pane_id, _, name)) => {
                self.mode = Mode::ConfirmClose { pane_id };
                format!("kill pane \"{name}\"?  Enter = yes   Esc = no")
            }
        });
    }

    fn reload_tree(&mut self) {
        if let Ok(tree) = Tree::fetch(self.api, None) {
            self.view.set_tree(tree);
        }
    }

    /// Retarget the preview and hold off polled trees after a local
    /// create/close.
    fn after_mutation(&mut self) {
        self.current_pane = None;
        self.tree_hold_until =
            Instant::now() + crate::poller::TREE_POLL_INTERVAL + Duration::from_millis(1000);
    }
}

/// Tree block title: ` spaces ` plus a summary of agent activity across all
/// panes, e.g. ` ⠋ 2 working  ▲ 1 waiting  ✓ 1 complete `. Zero counts are
/// hidden.
fn tree_title(counts: (usize, usize, usize), tick: usize) -> Line<'static> {
    let mut spans = vec![Span::styled(
        " spaces ",
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD),
    )];
    let (working, blocked, done) = counts;
    for (n, status) in [(working, "working"), (blocked, "blocked"), (done, "done")] {
        if n > 0 {
            let word = status_word(Some(status)).unwrap_or(status);
            spans.push(Span::styled(
                format!(" {} {n} {word} ", status_glyph(Some(status), tick)),
                Style::default()
                    .fg(status_color(Some(status)))
                    .add_modifier(Modifier::BOLD),
            ));
        }
    }
    Line::from(spans)
}

/// Title for the preview block based on the selected row.
fn preview_title(rows: &[crate::view::Row], selected: usize) -> String {
    match rows.get(selected) {
        Some(row) => format!(" preview: {} ", row.kind.id()),
        None => " preview ".to_string(),
    }
}

/// One frame's worth of data for `draw`. `rows` is precomputed by the caller
/// so the tree is only flattened once per frame, and `preview` is the already
/// ANSI-parsed preview so parsing is not repeated every frame.
struct Screen<'a> {
    rows: &'a [crate::view::Row],
    selected: usize,
    preview: &'a Text<'static>,
    /// Status or prompt text for the bottom bar; `None` shows the key help.
    status: Option<&'a str>,
    tick: usize,
    counts: (usize, usize, usize),
}

/// Draw the two-pane layout: tree on the left, preview on the right, plus a
/// help/status bar at the bottom. `prev_offset` carries the tree's scroll
/// offset between frames.
fn draw(frame: &mut ratatui::Frame, screen: &Screen, prev_offset: &mut usize) {
    let Screen {
        rows,
        selected,
        preview,
        status,
        tick,
        counts,
    } = *screen;
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
        lines.push(row_line(row, i == selected, tree_inner_width, tick));
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
            .title(tree_title(counts, tick)),
    );
    frame.render_widget(tree_widget, chunks[0]);

    let preview_widget = Paragraph::new(preview.clone()).block(
        Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::DarkGray))
            .title(Span::styled(
                preview_title(rows, selected),
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
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
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
    use super::project_name;
    use super::truncate_display;
    use super::unique_label;

    #[test]
    fn unique_label_increments_on_collision() {
        assert_eq!(unique_label("api", &[]), "api");
        assert_eq!(unique_label("api", &["web", "api-2"]), "api");
        assert_eq!(unique_label("api", &["api"]), "api-2");
        assert_eq!(unique_label("api", &["api", "api-2"]), "api-3");
        // Fills the first gap.
        assert_eq!(unique_label("api", &["api", "api-3"]), "api-2");
    }
    use super::{status_chip, status_word};

    #[test]
    fn status_words_read_naturally() {
        assert_eq!(status_word(Some("working")), Some("working"));
        assert_eq!(status_word(Some("blocked")), Some("waiting"));
        assert_eq!(status_word(Some("done")), Some("complete"));
        assert_eq!(status_word(Some("idle")), Some("idle"));
        assert_eq!(status_word(Some("unknown")), None);
        assert!(status_chip(None).is_none());
        assert_eq!(status_chip(Some("done")).unwrap().0, " complete ");
    }

    #[test]
    fn project_name_is_basename() {
        assert_eq!(
            project_name("/Users/asaryu/job/tmp1/sptv-video"),
            "sptv-video"
        );
        assert_eq!(
            project_name("/Users/asaryu/job/tmp1/sptv-video/"),
            "sptv-video"
        );
        assert_eq!(project_name("/"), "");
        assert_eq!(project_name("plain"), "plain");
        assert_eq!(
            project_name("~/repos/herder-nav-with-preview"),
            "herder-nav-with-preview"
        );
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
