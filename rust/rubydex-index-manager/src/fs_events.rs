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

/// Batch shared between the watcher threads and the manager: the paths seen so far and when the
/// last one arrived. One lock for both, because they are only ever read and written together —
/// two mutexes meant every event and every drain took them twice.
type Batch = Arc<Mutex<(Vec<PathBuf>, Instant)>>;

/// Shared state that every watcher of one source reports into.
struct EventCollector {
    batch: Batch,
    ignored: Arc<Vec<PathBuf>>,
}

impl EventHandler for EventCollector {
    fn handle_event(&mut self, event: Result<Event>) {
        if let Ok(change) = event {
            let paths: Vec<PathBuf> = change
                .paths
                .into_iter()
                .filter(|path| !is_ignored(path, self.ignored.as_slice()))
                .collect();
            if paths.is_empty() {
                return;
            }
            let mut batch = self.batch.lock().unwrap();
            batch.0.extend(paths);
            batch.1 = Instant::now();
        }
    }
}

/// `.git` internals and the store's own cache directory are writes the index does not need to react
/// to. Watching the store made every build re-arm another indexer; git metadata churns constantly.
fn is_ignored(path: &std::path::Path, ignored: &[PathBuf]) -> bool {
    path.components().any(|component| component.as_os_str() == ".git")
        || ignored.iter().any(|root| path.starts_with(root))
}

/// `notify`-backed source: Linux inotify, macOS `FSEvents`, Windows
/// `ReadDirectoryChangesW`, polling fallback elsewhere.
pub struct NotifySource {
    watchers: Vec<RecommendedWatcher>,
    batch: Batch,
    debounce: Duration,
    /// Roots that could not be subscribed when they were tried. A per-user inotify budget is spent
    /// for minutes at a time (editors and other tools share it), so the manager keeps these to retry
    /// rather than leaving the workspace unwatched for its whole lifetime.
    unwatched: Vec<PathBuf>,
    /// Directories whose events are discarded (the store's own cache).
    ignored: Arc<Vec<PathBuf>>,
}

impl NotifySource {
    /// Subscribe to `paths` (watched recursively). Only creation, modification and
    /// removal events are collected; access events are ignored. A path that cannot be
    /// watched (a workspace that went away, or a spent inotify budget) is reported and
    /// kept in [`Self::unwatched`], never fatal: the manager keeps serving the other
    /// sessions.
    ///
    /// Events under `ignored` are discarded (any `.git` metadata directory is always discarded).
    /// The store lives inside the workspace when `location = "tmp"`; without this, writing the store
    /// re-triggers an indexer.
    #[must_use]
    pub fn new(paths: &[PathBuf], ignored: Vec<PathBuf>, debounce: Duration) -> NotifySource {
        let mut source = NotifySource {
            watchers: Vec::new(),
            batch: Arc::new(Mutex::new((Vec::<PathBuf>::new(), Instant::now()))),
            debounce,
            unwatched: Vec::new(),
            ignored: Arc::new(ignored),
        };
        for path in paths {
            source.subscribe(path.clone());
        }
        source
    }

    /// Attempts to watch one root recursively, recording it as unwatched when that fails.
    fn subscribe(&mut self, path: PathBuf) {
        let collector = EventCollector {
            batch: Arc::clone(&self.batch),
            ignored: Arc::clone(&self.ignored),
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

    /// Returns the settled batch of changed paths and empties it, or an empty batch when there is
    /// nothing to hand out yet. Never blocks: the manager drains every workspace in one pass and
    /// sleeps once, so a per-workspace debounce wait used to make one pass cost N debounce windows.
    ///
    /// A burst is only ready once it has been quiet for the debounce window, which is what collapses
    /// it into one batch.
    ///
    /// # Panics
    /// Panics if the shared batch lock is poisoned (a watcher thread panicked while holding it).
    #[must_use]
    pub fn take_ready_batch(&self) -> Vec<PathBuf> {
        let mut batch = self.batch.lock().unwrap();
        if batch.0.is_empty() || batch.1.elapsed() < self.debounce {
            return Vec::<PathBuf>::new();
        }
        let mut paths = std::mem::take(&mut batch.0);
        // Sort outside the lock: the watcher thread must never wait on this.
        drop(batch);
        paths.sort();
        paths.dedup();
        paths
    }

    /// Polls [`Self::take_ready_batch`] until a batch is ready or `timeout` elapses, so a caller that
    /// wants to wait on one workspace keeps working (and so a broken backend fails instead of
    /// hanging).
    ///
    /// # Panics
    /// Panics if the shared batch lock is poisoned (a watcher thread panicked while holding it).
    #[must_use]
    pub fn try_next_batch(&self, timeout: Duration) -> Vec<PathBuf> {
        let started = Instant::now();
        loop {
            let batch = self.take_ready_batch();
            if !batch.is_empty() {
                return batch;
            }
            if started.elapsed() >= timeout {
                return Vec::<PathBuf>::new();
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}
