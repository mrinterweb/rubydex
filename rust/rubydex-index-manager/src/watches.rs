//! One watcher set and one indexer per store, rebuilt only when the sessions for that
//! store change.

use crate::builder::Builder;
use crate::fs_events::NotifySource;
use crate::registry::Session;

use std::path::PathBuf;
use std::time::Duration;
use std::time::Instant;

/// How often a root that could not be subscribed is retried. A spent inotify budget frees up when
/// the tools sharing it exit, so retrying recovers a workspace without a manager restart — but each
/// attempt walks the tree it could not watch, so it is an interval rather than every pass.
const WATCH_RETRY_INTERVAL: Duration = Duration::from_secs(60);

pub struct Watch {
    pub store: PathBuf,
    pub workspaces: Vec<PathBuf>,
    pub builder: Builder,
    pub source: NotifySource,
    pub manifest: usize,
    /// When this watch's unwatched roots may be retried. Only consulted when there are any.
    pub retry_at: Instant,
}

/// The sessions that share one store, which is what a watcher set is built from.
pub struct Group {
    pub store: PathBuf,
    pub workspaces: Vec<PathBuf>,
    pub builder: Vec<String>,
    pub manifest: usize,
}

#[must_use]
pub fn groups(sessions: &[Session]) -> Vec<Group> {
    let mut sorted = sessions.to_vec();
    sorted.sort();

    let mut grouped = Vec::<Group>::new();
    for session in sorted {
        if let Some(index) = grouped.iter().position(|group| group.store == session.store) {
            grouped[index].workspaces.push(session.workspace);
        } else {
            grouped.push(Group {
                store: session.store.clone(),
                workspaces: vec![session.workspace],
                builder: session.builder.clone(),
                manifest: session.docs,
            });
        }
    }
    for group in &mut grouped {
        group.workspaces.sort();
    }
    grouped
}

/// Reconcile the watcher sets with the current sessions. A store whose session set changed gets
/// its watchers and indexer **replaced**: appending would leave the stale watcher set running
/// alongside the new one, which grows one watcher set per change forever and lets several indexers
/// for one store run at once.
pub fn sync(watches: &mut Vec<Watch>, grouped: Vec<Group>, debounce: Duration) {
    watches.retain(|watch| grouped.iter().any(|group| group.store == watch.store));
    for group in grouped {
        if let Some(index) = watches.iter().position(|watch| watch.store == group.store) {
            if watches[index].workspaces != group.workspaces {
                watches[index] = watch_for(group, debounce);
            }
            continue;
        }
        watches.push(watch_for(group, debounce));
    }
    retry_unwatched_roots(watches);
}

/// A root can fail to subscribe when the per-user inotify budget is spent, which frees up later.
/// Those roots are retried on an interval so the workspace does not stay unwatched for the whole
/// lifetime of the manager; a watch with nothing unwatched is never touched.
fn retry_unwatched_roots(watches: &mut [Watch]) {
    let now = Instant::now();
    for watch in watches.iter_mut() {
        if watch.source.unwatched().is_empty() || now < watch.retry_at {
            continue;
        }
        watch.retry_at = now + WATCH_RETRY_INTERVAL;
        watch.source.retry_unwatched();
    }
}

#[must_use]
pub fn watch_for(group: Group, debounce: Duration) -> Watch {
    Watch {
        store: group.store.clone(),
        workspaces: group.workspaces.clone(),
        builder: Builder::new(group.builder),
        source: NotifySource::new(group.workspaces.as_ref(), debounce),
        manifest: group.manifest,
        retry_at: Instant::now() + WATCH_RETRY_INTERVAL,
    }
}
