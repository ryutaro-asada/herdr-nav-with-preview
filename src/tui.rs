//! The ratatui/crossterm terminal UI. Owns the alternate-screen/raw-mode
//! lifecycle via a RAII guard so the terminal is always restored, even on a
//! panic. Rendering and key handling delegate to `TreeView` (pure logic).

use std::io::{self, Stdout};

use anyhow::{Context, Result};
use crossterm::{
    event::{self, Event, KeyCode, KeyEventKind},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph},
    Terminal,
};

use crate::herdr_client::HerdrApi;
use crate::view::{RowKind, TreeView};

/// Number of visible lines to request for the preview.
const PREVIEW_LINES: u32 = 200;

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
    Quit,
}

/// Resolve the pane id to preview for the currently selected row.
fn selected_pane_id(view: &TreeView) -> Option<String> {
    let row = view.selected_row()?;
    row.representative_pane(view.tree()).map(|p| p.pane_id.clone())
}

/// Fetch the preview text for a pane, returning an error string on failure so
/// the UI can display it inline.
fn fetch_preview(api: &dyn HerdrApi, pane_id: &str) -> String {
    match api.read_pane(pane_id, PREVIEW_LINES) {
        Ok(text) => text,
        Err(e) => format!("(failed to read pane {pane_id}: {e})"),
    }
}

/// Run the TUI event loop. The preview for the selected pane is fetched once on
/// start and re-fetched whenever the selection moves to a different pane.
pub fn run(view: &mut TreeView, api: &dyn HerdrApi) -> Result<LoopOutcome> {
    let mut guard = TerminalGuard::new()?;

    let mut current_pane = selected_pane_id(view);
    let mut preview = match &current_pane {
        Some(id) => fetch_preview(api, id),
        None => String::new(),
    };

    loop {
        guard.terminal().draw(|frame| draw(frame, view, &preview))?;

        if let Event::Key(key) = event::read()? {
            if key.kind != KeyEventKind::Press {
                continue;
            }
            match key.code {
                KeyCode::Char('q') | KeyCode::Esc => return Ok(LoopOutcome::Quit),
                KeyCode::Down | KeyCode::Char('j') => view.move_down(),
                KeyCode::Up | KeyCode::Char('k') => view.move_up(),
                KeyCode::Right | KeyCode::Char('l') => view.expand(),
                KeyCode::Left | KeyCode::Char('h') => view.collapse(),
                KeyCode::Char(' ') => view.toggle(),
                _ => {}
            }
        }

        // Re-fetch only when the selected pane actually changed.
        let new_pane = selected_pane_id(view);
        if new_pane != current_pane {
            preview = match &new_pane {
                Some(id) => fetch_preview(api, id),
                None => String::new(),
            };
            current_pane = new_pane;
        }
    }
}

/// Draw the two-pane layout: tree on the left, preview on the right.
pub fn draw(frame: &mut ratatui::Frame, view: &TreeView, preview: &str) {
    let chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(40), Constraint::Percentage(60)])
        .split(frame.area());

    let rows = view.rows();
    let selected = view.selected_index();

    let mut lines: Vec<Line> = Vec::with_capacity(rows.len());
    for (i, row) in rows.iter().enumerate() {
        let indent = "  ".repeat(row.depth);
        let marker = if row.expandable {
            if row.expanded {
                "▾ "
            } else {
                "▸ "
            }
        } else {
            "  "
        };
        let content = format!("{indent}{marker}{}", row.label);
        let style = if i == selected {
            Style::default().add_modifier(Modifier::REVERSED | Modifier::BOLD)
        } else {
            Style::default()
        };
        lines.push(Line::from(Span::styled(content, style)));
    }
    if lines.is_empty() {
        lines.push(Line::from(
            "No workspaces found. Open a workspace in herdr first.",
        ));
    }

    let tree_widget = Paragraph::new(lines).block(
        Block::default()
            .borders(Borders::ALL)
            .title(" workspaces "),
    );
    frame.render_widget(tree_widget, chunks[0]);

    let title = view
        .selected_row()
        .map(|r| match r.kind {
            RowKind::Pane { pane_id, .. } => format!(" preview: {pane_id} "),
            RowKind::Tab { tab_id, .. } => format!(" preview: {tab_id} "),
            RowKind::Workspace { workspace_id } => format!(" preview: {workspace_id} "),
        })
        .unwrap_or_else(|| " preview ".to_string());

    // Clip to the inner height so long output doesn't overflow the block.
    let inner_height = chunks[1].height.saturating_sub(2) as usize;
    let clipped: String = preview
        .lines()
        .take(inner_height.max(1))
        .collect::<Vec<_>>()
        .join("\n");

    let preview_widget =
        Paragraph::new(clipped).block(Block::default().borders(Borders::ALL).title(title));
    frame.render_widget(preview_widget, chunks[1]);
}
