//! One watcher set and one indexer per store, rebuilt only when the sessions for that
//! store change.

use crate::builder::Builder;
use crate::fs_events::FsEventSource;
use crate::fs_events::default_source;
use crate::registry::Session;

use std::path::PathBuf;
use std::time::Duration;

pub struct Watch {
    pub store: PathBuf,
    pub workspaces: Vec<PathBuf>,
    pub builder: Builder,
    pub source: Box<dyn FsEventSource>,
    pub manifest: usize,
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
}

#[must_use]
pub fn watch_for(group: Group, debounce: Duration) -> Watch {
    Watch {
        store: group.store.clone(),
        workspaces: group.workspaces.clone(),
        builder: Builder::new(group.builder),
        source: default_source(group.workspaces.as_ref(), debounce),
        manifest: group.manifest,
    }
}
