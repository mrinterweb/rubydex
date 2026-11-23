//! `notify`-backed file-system events for the disk-index manager.
//!
//! The manager batches changes through [`NotifySource`]: a burst of events inside the
//! debounce window collapses into one batch. See `docs/disk-persisted-index/fs-event-crate.md`
//! for the backend choice and its known limitations; swapping the event crate means
//! replacing `NotifySource`.

use notify::{Config, Event, EventHandler, EventKindMask, RecommendedWatcher, RecursiveMode, Result, Watcher};
use std::ffi::OsStr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Extensions whose writes can change the index. Mirrors `Graph::INDEXABLE_EXTENSIONS` plus the two
/// files the freshness signature reads; the manager has no dependency on the crate or the gem, so
/// the lists are kept in step by hand.
const INDEXABLE_EXTENSIONS: &[&str] = &["rb", "rake", "rbs", "ru"];
const INDEXABLE_FILES: &[&str] = &["Gemfile.lock", "rubydex.toml"];

/// Directories the indexer never lists. Mirrors `DEFAULT_EXCLUDED_DIRECTORIES` in
/// `rust/rubydex/src/config.rs`; a workspace's own `[graph] exclude` patterns are not known here, so
/// a `.rb` write under one of those still re-indexes (a spurious rebuild, never a stale store).
const EXCLUDED_DIRECTORIES: &[&str] = &[
    ".bundle",
    ".claude",
    ".git",
    ".github",
    ".ruby-lsp",
    ".vscode",
    "log",
    "node_modules",
    "tmp",
];

/// Batch shared between the watcher threads and the manager: the paths seen so far and when the
/// last one arrived. One lock for both, because they are only ever read and written together —
/// two mutexes meant every event and every drain took them twice.
type Batch = Arc<Mutex<(Vec<PathBuf>, Instant)>>;

/// Shared state that every watcher of one source reports into.
struct EventCollector {
    batch: Batch,
    roots: Arc<Vec<PathBuf>>,
    ignored: Arc<Vec<PathBuf>>,
}

impl EventHandler for EventCollector {
    fn handle_event(&mut self, event: Result<Event>) {
        if let Ok(change) = event {
            let paths: Vec<PathBuf> = change
                .paths
                .into_iter()
                .filter(|path| is_indexable(path, self.roots.as_slice(), self.ignored.as_slice()))
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

/// Whether an event can change the index: a Ruby source file, or a file the freshness signature
/// reads, outside the excluded directories and the store's own cache. Everything else — an editor's
/// swap file, `node_modules` churn, a log rotation — is dropped before it reaches the batch, so a
/// burst of unrelated writes cannot be mistaken for source changes and force a rebuild.
///
/// The exclusion is matched on the path relative to its workspace, top-level component only: the
/// listing excludes `<workspace>/tmp` (the directory) and no deeper `tmp` it happens to find, and a
/// workspace that itself lives under a directory called `tmp` must still be watched.
fn is_indexable(path: &std::path::Path, roots: &[PathBuf], ignored: &[PathBuf]) -> bool {
    if ignored.iter().any(|root| path.starts_with(root)) {
        return false;
    }
    let Some(relative) = roots.iter().find_map(|root| path.strip_prefix(root).ok()) else {
        return false;
    };
    if relative
        .components()
        .next()
        .is_some_and(|first| EXCLUDED_DIRECTORIES.iter().any(|dir| first.as_os_str() == *dir))
    {
        return false;
    }

    path.file_name().and_then(OsStr::to_str).is_some_and(|name| {
        INDEXABLE_FILES.contains(&name)
            || path
                .extension()
                .and_then(OsStr::to_str)
                .is_some_and(|extension| INDEXABLE_EXTENSIONS.contains(&extension))
    })
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
    /// Workspace roots, so an event's path can be judged relative to the workspace it came from.
    roots: Arc<Vec<PathBuf>>,
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
            roots: Arc::new(paths.to_vec()),
            ignored: Arc::new(ignored),
        };
        for path in paths {
            source.subscribe(path.clone());
        }
        source
    }

    /// Attempts to watch one root, recording it as unwatched when that fails.
    ///
    /// The root itself is watched without recursion and each top-level directory under it
    /// recursively. Recursing from the root spends the per-user inotify budget on `node_modules`,
    /// `tmp` and `log` — most of a large checkout's directory count — and a budget spent there makes
    /// the root's own watch fail, leaving the workspace unwatched entirely. The cost is that a
    /// top-level directory created after this call is only seen by the root's non-recursive watch,
    /// so a burst into it reaches the batch through the sessions' own freshness walk instead.
    fn subscribe(&mut self, path: PathBuf) {
        let collector = EventCollector {
            batch: Arc::clone(&self.batch),
            roots: Arc::clone(&self.roots),
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
        let mut outcome = watcher.watch(&path, RecursiveMode::NonRecursive);
        if outcome.is_ok()
            && let Ok(entries) = std::fs::read_dir(&path)
        {
            for entry in entries.flatten() {
                let child = entry.path();
                // Symlinked directories are skipped: they are not part of this workspace's tree.
                if !entry.file_type().is_ok_and(|kind| kind.is_dir())
                    || child
                        .file_name()
                        .is_some_and(|name| EXCLUDED_DIRECTORIES.iter().any(|dir| name == *dir))
                {
                    continue;
                }
                outcome = watcher.watch(&child, RecursiveMode::Recursive);
                if outcome.is_err() {
                    break;
                }
            }
        }
        if let Err(error) = outcome {
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
