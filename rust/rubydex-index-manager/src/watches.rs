//! One watcher set and one indexer per store, rebuilt only when the sessions for that
//! store change.

use crate::builder::Builder;
use crate::fs_events::NotifySource;
use crate::registry::Session;

use std::path::{Path, PathBuf};
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
            // The burst threshold is judged against the manifest, and the sessions of one store may
            // disagree (a workspace that grew since it registered). The largest is the one a rebuild
            // would produce, so it is the one that decides full-versus-surgical.
            grouped[index].manifest = grouped[index].manifest.max(session.docs);
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
        // Two sessions of one store in one workspace are one workspace to watch. Without this the
        // list differs from the watch's on every sibling join or exit, which replaces the watcher set
        // (and, before the builder was preserved, its indexer) for nothing.
        group.workspaces.dedup();
    }
    grouped
}

/// Reconcile the watcher sets with the current sessions. A store whose workspace set changed gets
/// its watchers **rebuilt**: appending would leave the stale watcher set running alongside the new
/// one, which grows one watcher set per change forever.
///
/// Only the watchers are rebuilt: the indexer is kept, because dropping a `Builder` drops its child
/// un-reaped (a zombie on Unix), forgets the burst counts that decide full-versus-surgical, and lets a
/// second indexer for the same store start before the first one exits.
pub fn sync(watches: &mut Vec<Watch>, grouped: Vec<Group>, debounce: Duration) {
    watches.retain(|watch| grouped.iter().any(|group| group.store == watch.store));
    for group in grouped {
        if let Some(index) = watches.iter().position(|watch| watch.store == group.store) {
            let watch = &mut watches[index];
            if watch.workspaces != group.workspaces {
                watch.source = source_for(&group, debounce);
                watch.workspaces = group.workspaces;
            }
            // The manifest count is what decides full-vs-surgical for a burst, and a session that
            // re-registered with a grown workspace reports a new one. Keeping the boot-time count
            // would compare every later burst against a stale baseline.
            watch.manifest = group.manifest;
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

/// The event source for a group: its workspaces, with the store's own directory ignored.
fn source_for(group: &Group, debounce: Duration) -> NotifySource {
    let ignored = group.store.parent().into_iter().map(Path::to_path_buf).collect();
    NotifySource::new(group.workspaces.as_ref(), ignored, debounce)
}

#[must_use]
pub fn watch_for(group: Group, debounce: Duration) -> Watch {
    let source = source_for(&group, debounce);

    Watch {
        store: group.store.clone(),
        workspaces: group.workspaces.clone(),
        builder: Builder::new(group.builder),
        source,
        manifest: group.manifest,
        retry_at: Instant::now() + WATCH_RETRY_INTERVAL,
    }
}
