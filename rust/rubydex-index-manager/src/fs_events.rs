//! `notify`-backed file-system events for the disk-index manager.
//!
//! The manager batches changes through [`NotifySource`]: a burst of events inside the
//! debounce window collapses into one batch. See `docs/disk-persisted-index/fs-event-crate.md`
//! for the backend choice and its known limitations; swapping the event crate means
//! replacing `NotifySource`.

use notify::{Config, Event, EventHandler, EventKindMask, RecommendedWatcher, Result, Watcher};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Shared state that every watcher of one source reports into.
struct EventCollector {
    pending: Arc<Mutex<Vec<PathBuf>>>,
    last_event: Arc<Mutex<Instant>>,
}

impl EventHandler for EventCollector {
    fn handle_event(&mut self, event: Result<Event>) {
        if let Ok(change) = event {
            self.pending.lock().unwrap().extend(change.paths);
            *self.last_event.lock().unwrap() = Instant::now();
        }
    }
}

/// `notify`-backed source: Linux inotify, macOS `FSEvents`, Windows
/// `ReadDirectoryChangesW`, polling fallback elsewhere.
pub struct NotifySource {
    _watchers: Vec<RecommendedWatcher>,
    pending: Arc<Mutex<Vec<PathBuf>>>,
    last_event: Arc<Mutex<Instant>>,
    debounce: Duration,
}

impl NotifySource {
    /// Subscribe to `paths` (watched recursively). Only creation, modification and
    /// removal events are collected; access events are ignored. A path that cannot be
    /// watched (a workspace that went away) is reported and skipped, never fatal: the
    /// manager keeps serving the other sessions.
    #[must_use]
    pub fn new(paths: &[PathBuf], debounce: Duration) -> NotifySource {
        let pending = Arc::new(Mutex::new(Vec::<PathBuf>::new()));
        let last_event = Arc::new(Mutex::new(Instant::now()));
        let config = Config::default()
            .with_event_kinds(EventKindMask::CREATE | EventKindMask::ALL_MODIFY | EventKindMask::REMOVE);

        let watchers = paths
            .iter()
            .filter_map(|path| {
                let collector = EventCollector {
                    pending: Arc::clone(&pending),
                    last_event: Arc::clone(&last_event),
                };
                let shown = path.display();
                let Ok(mut watcher) = RecommendedWatcher::new(collector, config) else {
                    eprintln!("rubydex-index-manager: cannot watch {shown}");
                    return None;
                };
                if let Err(error) = watcher.watch(path, notify::RecursiveMode::Recursive) {
                    eprintln!("rubydex-index-manager: cannot watch {shown}: {error:?}");
                    return None;
                }
                Some(watcher)
            })
            .collect();

        NotifySource {
            _watchers: watchers,
            pending,
            last_event,
            debounce,
        }
    }
}

impl NotifySource {
    /// Returns the next batch of changed paths, debounced: a burst of events inside the
    /// debounce window collapses into one batch. Gives up after `timeout` and returns an
    /// empty batch so a broken backend fails instead of hanging.
    ///
    /// # Panics
    /// Panics if the shared batch lock is poisoned (a watcher thread panicked while holding it).
    pub fn try_next_batch(&mut self, timeout: Duration) -> Vec<PathBuf> {
        let started = Instant::now();
        loop {
            std::thread::sleep(Duration::from_millis(20));
            let mut guard = self.pending.lock().unwrap();
            let last = self.last_event.lock().unwrap();
            let quiet = last.elapsed() >= self.debounce;
            if guard.is_empty() || !quiet {
                if started.elapsed() >= timeout {
                    return Vec::<PathBuf>::new();
                }
                continue;
            }
            let mut batch = guard.clone();
            guard.clear();
            batch.sort();
            batch.dedup();
            return batch;
        }
    }
}
