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
    watchers: Vec<RecommendedWatcher>,
    pending: Arc<Mutex<Vec<PathBuf>>>,
    last_event: Arc<Mutex<Instant>>,
    debounce: Duration,
    /// Roots that could not be subscribed when they were tried. A per-user inotify budget is spent
    /// for minutes at a time (editors and other tools share it), so the manager keeps these to retry
    /// rather than leaving the workspace unwatched for its whole lifetime.
    unwatched: Vec<PathBuf>,
}

impl NotifySource {
    /// Subscribe to `paths` (watched recursively). Only creation, modification and
    /// removal events are collected; access events are ignored. A path that cannot be
    /// watched (a workspace that went away, or a spent inotify budget) is reported and
    /// kept in [`Self::unwatched`], never fatal: the manager keeps serving the other
    /// sessions.
    #[must_use]
    pub fn new(paths: &[PathBuf], debounce: Duration) -> NotifySource {
        let mut source = NotifySource {
            watchers: Vec::new(),
            pending: Arc::new(Mutex::new(Vec::<PathBuf>::new())),
            last_event: Arc::new(Mutex::new(Instant::now())),
            debounce,
            unwatched: Vec::new(),
        };
        for path in paths {
            source.subscribe(path.clone());
        }
        source
    }

    /// Attempts to watch one root recursively, recording it as unwatched when that fails.
    fn subscribe(&mut self, path: PathBuf) {
        let collector = EventCollector {
            pending: Arc::clone(&self.pending),
            last_event: Arc::clone(&self.last_event),
        };
        let config = Config::default()
            .with_event_kinds(EventKindMask::CREATE | EventKindMask::ALL_MODIFY | EventKindMask::REMOVE);
        let shown = path.display();
        let Ok(mut watcher) = RecommendedWatcher::new(collector, config) else {
            eprintln!("rubydex-index-manager: cannot watch {shown}");
            self.unwatched.push(path);
            return;
        };
        if let Err(error) = watcher.watch(&path, notify::RecursiveMode::Recursive) {
            eprintln!("rubydex-index-manager: cannot watch {shown}: {error:?}");
            self.unwatched.push(path);
            return;
        }
        self.watchers.push(watcher);
    }

    /// The roots this source is not watching. Empty is the normal case.
    #[must_use]
    pub fn unwatched(&self) -> &[PathBuf] {
        &self.unwatched
    }

    /// Re-attempts every root that failed, returning how many are still unwatched. Whatever already
    /// succeeded keeps its watches, so a retry cannot disturb a workspace that is being watched.
    pub fn retry_unwatched(&mut self) -> usize {
        for path in std::mem::take(&mut self.unwatched) {
            self.subscribe(path);
        }
        self.unwatched.len()
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
