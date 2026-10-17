//! Background preview polling.
//!
//! A worker thread polls the currently-selected pane about once per second and
//! sends `(generation, pane_id, text)` back to the UI thread over an mpsc
//! channel. The UI bumps a generation counter every time the selection changes
//! and ignores any result whose generation is stale, so output from a
//! previously-selected pane never clobbers the current one.
//!
//! The pure generation/stale-decision logic lives in `PollState` and is
//! unit-tested; the thread wiring is a thin shell around it.

use std::sync::mpsc::{Receiver, Sender};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex,
};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use ansi_to_tui::IntoText;
use ratatui::text::Text;

use crate::herdr_client::{CliHerdr, HerdrApi};

/// Poll interval for the live preview.
pub const POLL_INTERVAL: Duration = Duration::from_millis(1000);
const PREVIEW_LINES: u32 = 100;

/// A preview result tagged with the generation it was requested under. The
/// ANSI-colored text is parsed on the worker thread so the UI thread never
/// pays the parsing cost.
#[derive(Debug, Clone)]
pub struct PreviewUpdate {
    pub generation: u64,
    pub pane_id: String,
    pub text: Text<'static>,
}

/// Request the worker thread should act on: which pane and which generation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PollRequest {
    pub generation: u64,
    pub pane_id: Option<String>,
}

/// Pure state machine for deciding whether an incoming update is current.
#[derive(Debug, Default)]
pub struct PollState {
    current_generation: u64,
}

impl PollState {
    pub fn new() -> Self {
        Self {
            current_generation: 0,
        }
    }

    pub fn generation(&self) -> u64 {
        self.current_generation
    }

    /// Advance to a new generation (call when the selected pane changes).
    pub fn bump(&mut self) -> u64 {
        self.current_generation += 1;
        self.current_generation
    }

    /// Should an update tagged with `generation` be applied?
    pub fn accepts(&self, generation: u64) -> bool {
        generation == self.current_generation
    }
}

/// Handle to the running poll thread.
pub struct Poller {
    pub updates: Receiver<PreviewUpdate>,
    request: Arc<Mutex<PollRequest>>,
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl Poller {
    /// Spawn the worker. The worker creates its own `CliHerdr` (stateless; it
    /// only reads `HERDR_BIN_PATH`), so nothing non-`Send` crosses the thread
    /// boundary.
    pub fn spawn(initial: PollRequest) -> Self {
        let (tx, rx): (Sender<PreviewUpdate>, Receiver<PreviewUpdate>) =
            std::sync::mpsc::channel();
        let request = Arc::new(Mutex::new(initial));
        let stop = Arc::new(AtomicBool::new(false));

        let thread_request = Arc::clone(&request);
        let thread_stop = Arc::clone(&stop);
        let handle = thread::spawn(move || {
            let api = CliHerdr::new();
            // Remember the last raw text per generation so we can skip sending
            // (and thus skip a UI redraw) when nothing changed.
            let mut last_raw: Option<(u64, String)> = None;
            loop {
                if thread_stop.load(Ordering::Relaxed) {
                    break;
                }
                let req = thread_request.lock().unwrap().clone();
                if let Some(pane_id) = req.pane_id.clone() {
                    let raw = match api.read_pane_ansi(&pane_id, PREVIEW_LINES) {
                        Ok(t) => t,
                        Err(e) => format!("(failed to read pane {pane_id}: {e})"),
                    };

                    // Skip if the content is identical to what we last sent for
                    // this generation; avoids a needless UI redraw every tick.
                    let unchanged = matches!(
                        &last_raw,
                        Some((g, prev)) if *g == req.generation && *prev == raw
                    );
                    if !unchanged {
                        // Parse ANSI here, off the UI thread.
                        let text = raw
                            .as_bytes()
                            .into_text()
                            .unwrap_or_else(|_| Text::from(raw.clone()));
                        last_raw = Some((req.generation, raw));
                        if tx
                            .send(PreviewUpdate {
                                generation: req.generation,
                                pane_id,
                                text,
                            })
                            .is_err()
                        {
                            break;
                        }
                    }
                }
                // Sleep in small slices so stop is responsive.
                let mut slept = Duration::ZERO;
                while slept < POLL_INTERVAL {
                    if thread_stop.load(Ordering::Relaxed) {
                        break;
                    }
                    thread::sleep(Duration::from_millis(50));
                    slept += Duration::from_millis(50);
                }
            }
        });

        Self {
            updates: rx,
            request,
            stop,
            handle: Some(handle),
        }
    }

    /// Point the worker at a new pane/generation.
    pub fn set_request(&self, req: PollRequest) {
        *self.request.lock().unwrap() = req;
    }
}

impl Drop for Poller {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bump_increments_generation() {
        let mut s = PollState::new();
        assert_eq!(s.generation(), 0);
        assert_eq!(s.bump(), 1);
        assert_eq!(s.bump(), 2);
        assert_eq!(s.generation(), 2);
    }

    #[test]
    fn accepts_only_current_generation() {
        let mut s = PollState::new();
        s.bump(); // 1
        s.bump(); // 2
        assert!(s.accepts(2));
        assert!(!s.accepts(1)); // stale
        assert!(!s.accepts(3)); // not yet
    }

    #[test]
    fn stale_update_rejected_after_selection_change() {
        // Simulate: select pane A (gen 1), a late update for gen 1 arrives
        // after we've moved to pane B (gen 2). It must be rejected.
        let mut s = PollState::new();
        let gen_a = s.bump(); // 1
        let late = PreviewUpdate {
            generation: gen_a,
            pane_id: "A".into(),
            text: Text::from("old"),
        };
        let _gen_b = s.bump(); // 2 (selection moved to B)
        assert!(!s.accepts(late.generation));
    }

    #[test]
    fn current_update_accepted() {
        let mut s = PollState::new();
        let g = s.bump();
        let upd = PreviewUpdate {
            generation: g,
            pane_id: "A".into(),
            text: Text::from("fresh"),
        };
        assert!(s.accepts(upd.generation));
    }
}
