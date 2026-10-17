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
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph},
    Terminal,
};

use crate::herdr_client::HerdrApi;
use crate::poller::{PollRequest, PollState, Poller};
use crate::view::{RowKind, TreeView};

/// How long to wait for a key event before checking for preview updates.
const EVENT_POLL: Duration = Duration::from_millis(100);

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

/// Run the TUI event loop. A background `Poller` refreshes the selected pane's
/// preview once per second; the loop polls for key events with a short timeout
/// and drains preview updates each iteration, applying only current-generation
/// results. The `Poller` is stopped and joined when it drops at scope exit.
pub fn run(view: &mut TreeView, api: &dyn HerdrApi) -> Result<LoopOutcome> {
    let mut guard = TerminalGuard::new()?;

    let mut poll_state = PollState::new();
    let mut current_pane = selected_pane_id(view);
    let generation = poll_state.generation();
    let mut preview = String::new();
    let mut status: Option<String> = None;

    let poller = Poller::spawn(PollRequest {
        generation,
        pane_id: current_pane.clone(),
    });

    loop {
        guard
            .terminal()
            .draw(|frame| draw(frame, view, &preview, status.as_deref()))?;

        if event::poll(EVENT_POLL)? {
            if let Event::Key(key) = event::read()? {
                if key.kind == KeyEventKind::Press {
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
                        _ => {}
                    }
                }
            }
        }

        // On selection change, bump the generation, clear the stale preview,
        // and retarget the poller.
        let new_pane = selected_pane_id(view);
        if new_pane != current_pane {
            let gen = poll_state.bump();
            preview.clear();
            poller.set_request(PollRequest {
                generation: gen,
                pane_id: new_pane.clone(),
            });
            current_pane = new_pane;
        }

        // Drain any pending updates; apply only the current generation.
        while let Ok(update) = poller.updates.try_recv() {
            if poll_state.accepts(update.generation) && Some(&update.pane_id) == current_pane.as_ref()
            {
                preview = update.text;
            }
        }
    }
}

/// Draw the two-pane layout: tree on the left, preview on the right, plus a
/// help/status bar at the bottom.
pub fn draw(frame: &mut ratatui::Frame, view: &TreeView, preview: &str, status: Option<&str>) {
    let outer = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1), Constraint::Length(1)])
        .split(frame.area());

    let chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(40), Constraint::Percentage(60)])
        .split(outer[0]);

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

    // Bottom help/status bar.
    let bar = match status {
        Some(s) => Line::from(Span::styled(
            format!(" {s} "),
            Style::default().add_modifier(Modifier::BOLD),
        )),
        None => Line::from(
            " ↑/↓ move  ←/→ collapse/expand  space toggle  Enter jump  q/Esc quit ".to_string(),
        ),
    };
    frame.render_widget(Paragraph::new(bar), outer[1]);
}
