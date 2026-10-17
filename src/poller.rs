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
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use ansi_to_tui::IntoText;
use ratatui::text::Text;

use crate::herdr_client::{CliHerdr, HerdrApi};

/// Poll interval for the live preview when the selection is unchanged.
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

/// Shared state between the UI thread and the worker, guarded by a mutex and
/// paired with a condvar so the worker wakes immediately when the request
/// changes (e.g. the user moves to a different pane) instead of finishing a
/// full sleep interval first.
struct Shared {
    request: PollRequest,
    stop: bool,
}

/// Handle to the running poll thread.
pub struct Poller {
    pub updates: Receiver<PreviewUpdate>,
    shared: Arc<(Mutex<Shared>, Condvar)>,
    handle: Option<JoinHandle<()>>,
}

impl Poller {
    /// Spawn the worker. The worker creates its own `CliHerdr` (stateless; it
    /// only reads `HERDR_BIN_PATH`), so nothing non-`Send` crosses the thread
    /// boundary.
    pub fn spawn(initial: PollRequest) -> Self {
        let (tx, rx): (Sender<PreviewUpdate>, Receiver<PreviewUpdate>) =
            std::sync::mpsc::channel();
        let shared = Arc::new((
            Mutex::new(Shared {
                request: initial,
                stop: false,
            }),
            Condvar::new(),
        ));

        let thread_shared = Arc::clone(&shared);
        let handle = thread::spawn(move || {
            let api = CliHerdr::new();
            // Remember the last raw text per generation so we can skip sending
            // (and thus skip a UI redraw) when nothing changed.
            let mut last_raw: Option<(u64, String)> = None;
            let (lock, cvar) = &*thread_shared;

            loop {
                // Snapshot the current request under the lock.
                let req = {
                    let guard = lock.lock().unwrap();
                    if guard.stop {
                        break;
                    }
                    guard.request.clone()
                };

                if let Some(pane_id) = req.pane_id.clone() {
                    let raw = match api.read_pane_ansi(&pane_id, PREVIEW_LINES) {
                        Ok(t) => t,
                        Err(e) => format!("(failed to read pane {pane_id}: {e})"),
                    };
                    let unchanged = matches!(
                        &last_raw,
                        Some((g, prev)) if *g == req.generation && *prev == raw
                    );
                    if !unchanged {
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

                // Wait up to POLL_INTERVAL, but wake early if the request
                // changed (new pane) or stop was requested. Re-fetch
                // immediately when the generation advanced.
                let guard = lock.lock().unwrap();
                let (guard, _timeout) = cvar
                    .wait_timeout_while(guard, POLL_INTERVAL, |s| {
                        !s.stop && s.request.generation == req.generation
                    })
                    .unwrap();
                if guard.stop {
                    break;
                }
            }
        });

        Self {
            updates: rx,
            shared,
            handle: Some(handle),
        }
    }

    /// Point the worker at a new pane/generation and wake it immediately.
    pub fn set_request(&self, req: PollRequest) {
        let (lock, cvar) = &*self.shared;
        {
            let mut guard = lock.lock().unwrap();
            guard.request = req;
        }
        cvar.notify_all();
    }
}

impl Drop for Poller {
    fn drop(&mut self) {
        let (lock, cvar) = &*self.shared;
        {
            let mut guard = lock.lock().unwrap();
            guard.stop = true;
        }
        cvar.notify_all();
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

    // Live integration check: proves the worker wakes immediately on a request
    // change instead of waiting out the poll interval. Requires a running herdr
    // with at least one pane. Run explicitly:
    //   cargo test -- --ignored switch_latency
    #[test]
    #[ignore]
    fn switch_latency_is_fast() {
        use std::time::Instant;

        let api = CliHerdr::new();
        let panes = api.panes().expect("herdr must be running");
        assert!(panes.len() >= 2, "need at least two panes");

        // Start on pane 0 and drain its first update.
        let poller = Poller::spawn(PollRequest {
            generation: 1,
            pane_id: Some(panes[0].pane_id.clone()),
        });
        let _ = poller.updates.recv_timeout(Duration::from_secs(2)).unwrap();

        // Switch to pane 1 and time how long the first update for gen 2 takes.
        let start = Instant::now();
        poller.set_request(PollRequest {
            generation: 2,
            pane_id: Some(panes[1].pane_id.clone()),
        });
        loop {
            let upd = poller
                .updates
                .recv_timeout(Duration::from_secs(2))
                .expect("update should arrive");
            if upd.generation == 2 {
                break;
            }
        }
        let elapsed = start.elapsed();
        // Must be far below the 1s poll interval thanks to the condvar wake.
        assert!(
            elapsed < Duration::from_millis(400),
            "switch took too long: {elapsed:?}"
        );
    }
}
