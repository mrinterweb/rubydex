//! Session registry: one file per session, held under an exclusive OS lock for the
//! session's whole lifetime. The manager `try_lock`s each registry file to learn
//! whether the session is still alive: a successful lock means the process died and
//! the OS released the lock. No PIDs, no platform-specific process API, no PID-reuse
//! race. Ruby's `File#flock(File::LOCK_EX)` and Rust's `File::try_lock` both map to
//! `flock(2)` on Unix and `LockFileEx` on Windows.

use serde::{Deserialize, Serialize};
use serde_json::from_reader;
use std::fs::{File, remove_file};
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
pub struct Session {
    pub store: PathBuf,
    pub workspace: PathBuf,
    pub builder: Vec<String>,
    #[serde(default)]
    pub docs: usize,
}

/// List the sessions that still hold their lock, pruning the dead ones from disk.
#[must_use]
pub fn live_sessions(dir: &Path) -> Vec<Session> {
    let mut alive = Vec::<Session>::new();
    let Ok(entries) = dir.read_dir() else {
        return alive;
    };

    for result in entries {
        let Ok(entry) = result else {
            continue;
        };
        let path = entry.path();
        if path.extension().is_none_or(|ext| ext != "json") {
            continue;
        }

        let Ok(handle) = File::open(&path) else {
            continue;
        };
        if handle.try_lock().is_ok() {
            let _ = remove_file(&path);
            continue;
        }

        let Ok(session) = from_reader(handle) else {
            continue;
        };
        alive.push(session);
    }

    alive
}

#[test]
fn live_sessions_lists_a_session_holding_its_lock() {
    let dir = tempfile::tempdir().expect("tempdir");
    let file = dir.path().join("1-aabbccdd.json");
    std::fs::write(
        &file,
        "{\"workspace\":\"/ws\",\"store\":\"/ws/tmp/index.redb\",\"builder\":[\"ruby\"]}",
    )
    .expect("write");

    let handle = File::open(&file).expect("open");
    handle.try_lock().expect("lock");
    assert_eq!(
        1,
        live_sessions(dir.path()).len(),
        "a session holding its lock must be listed"
    );
}

#[test]
fn live_sessions_prunes_a_session_that_no_longer_holds_its_lock() {
    let dir = tempfile::tempdir().expect("tempdir");
    let file = dir.path().join("1-aabbccdd.json");
    std::fs::write(
        &file,
        "{\"workspace\":\"/ws\",\"store\":\"/ws/tmp/index.redb\",\"builder\":[\"ruby\"]}",
    )
    .expect("write");

    assert_eq!(
        Vec::<Session>::new(),
        live_sessions(dir.path()),
        "an unheld registry file is dead",
    );
    assert!(!file.exists(), "the dead registry file must be pruned from disk");
}
