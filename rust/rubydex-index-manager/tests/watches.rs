//! The watcher sets stay one per store as sessions join, leave and change.

use rubydex_index_manager::registry::Session;
use rubydex_index_manager::watches::Watch;
use rubydex_index_manager::watches::groups;
use rubydex_index_manager::watches::sync;

use std::path::PathBuf;
use std::time::Duration;
use std::time::Instant;

fn debounce() -> Duration {
    Duration::from_millis(200)
}

fn session(store: &str, workspace: &str) -> Session {
    Session {
        store: PathBuf::from(store),
        workspace: PathBuf::from(workspace),
        builder: ["ruby", "index_session.rb"]
            .iter()
            .map(std::string::ToString::to_string)
            .collect(),
        docs: 100,
    }
}

#[test]
fn a_store_that_gains_a_session_keeps_one_watcher_set() {
    let store = "/ws/index.redb";
    let mut watches: Vec<Watch> = vec![];
    sync(&mut watches, groups(&[session(store, "/ws/a")]), debounce());
    sync(
        &mut watches,
        groups(&[session(store, "/ws/a"), session(store, "/ws/b")]),
        debounce(),
    );

    assert_eq!(
        watches.len(),
        1,
        "one watcher set per store, not one per session change"
    );
    assert_eq!(
        watches[0].workspaces.len(),
        2,
        "the watcher set was rebuilt in place, so it covers both workspaces"
    );
}

/// Two sessions of one store in one workspace are one workspace to watch, however many of them
/// register: a duplicated entry would differ from the watch's own list on every sibling join and exit
/// and rebuild the watcher set for nothing.
#[test]
fn sessions_sharing_a_workspace_are_deduplicated() {
    let store = "/ws/index.redb";
    let mut first = session(store, "/ws/a");
    first.docs = 100;
    let mut second = session(store, "/ws/a");
    second.docs = 250;

    let mut watches: Vec<Watch> = vec![];
    sync(&mut watches, groups(&[first.clone(), second]), debounce());

    assert_eq!(watches.len(), 1, "one store, one watcher set");
    assert_eq!(
        watches[0].workspaces,
        vec![PathBuf::from("/ws/a")],
        "the shared workspace is listed once"
    );
    assert_eq!(
        watches[0].manifest, 250,
        "the largest manifest of the group is the one a rebuild would produce"
    );

    // The sibling leaves. The workspace list does not change, so nothing is rebuilt and only the
    // manifest follows the sessions that are left.
    sync(&mut watches, groups(&[first]), debounce());
    assert_eq!(watches[0].workspaces, vec![PathBuf::from("/ws/a")]);
    assert_eq!(watches[0].manifest, 100);
}

/// A workspace change rebuilds the watchers but must not drop the indexer: a dropped `Builder` leaves
/// its child un-reaped on Unix, forgets the recorded burst, and forgets that a spawn just failed.
#[test]
fn a_gained_workspace_keeps_the_indexer() {
    let store = "/ws/index.redb";
    let mut first = session(store, "/ws/a");
    // Exits at once, so the test holds a builder that has spawned without leaving a child behind.
    first.builder = ["ruby", "-e", "exit 0"]
        .iter()
        .map(std::string::ToString::to_string)
        .collect();

    let mut watches: Vec<Watch> = vec![];
    sync(&mut watches, groups(&[first.clone()]), debounce());
    watches[0].builder.record(1);
    assert!(
        watches[0].builder.start(10).is_some(),
        "the first burst starts an indexer"
    );
    reap(&mut watches[0]);
    watches[0].builder.record(1);

    sync(&mut watches, groups(&[first, session(store, "/ws/b")]), debounce());

    assert_eq!(
        watches[0].workspaces.len(),
        2,
        "the new watcher set covers both workspaces"
    );
    assert!(
        watches[0].builder.start(10).is_some(),
        "the burst recorded before the change must survive it; a fresh builder would forget it"
    );
    reap(&mut watches[0]);
}

/// `Builder::reap` is non-blocking, so a test that wants the child collected has to poll for it.
fn reap(watch: &mut Watch) {
    let deadline = Instant::now() + Duration::from_secs(30);
    while watch.builder.is_running() && Instant::now() < deadline {
        watch.builder.reap();
        std::thread::sleep(Duration::from_millis(1));
    }
    assert!(!watch.builder.is_running(), "the indexer must be collected");
}

#[test]
fn a_store_that_loses_its_last_session_loses_its_watchers() {
    let mut watches: Vec<Watch> = vec![];
    sync(
        &mut watches,
        groups(&[session("/a/index.redb", "/a"), session("/b/index.redb", "/b")]),
        debounce(),
    );
    assert_eq!(watches.len(), 2, "one watcher set per store");
    sync(&mut watches, groups(&[session("/a/index.redb", "/a")]), debounce());

    assert_eq!(watches.len(), 1, "the store with no sessions left has no watchers");
    assert_eq!(
        watches[0].workspaces,
        vec![PathBuf::from("/a")],
        "the surviving watcher set is untouched"
    );
}

/// Retrying an unwatched root costs a tree walk, so it happens on an interval rather than on every
/// pass; a watch with nothing unwatched is never retried at all.
#[test]
fn an_unwatched_root_is_not_retried_on_every_pass() {
    let store = "/ws/index.redb";
    let dir = tempfile::tempdir().expect("tempdir");
    let missing = dir.path().join("not-yet");
    let mut watches: Vec<Watch> = vec![];
    sync(
        &mut watches,
        groups(&[session(store, missing.to_str().unwrap())]),
        debounce(),
    );

    assert_eq!(
        watches[0].source.unwatched().len(),
        1,
        "a workspace that could not be watched must be visible in the watch"
    );
    let retry_at = watches[0].retry_at;

    sync(
        &mut watches,
        groups(&[session(store, missing.to_str().unwrap())]),
        debounce(),
    );
    assert_eq!(retry_at, watches[0].retry_at, "the interval must not have been reset");
    assert_eq!(
        watches[0].source.unwatched().len(),
        1,
        "a retry inside the interval would walk the tree again for nothing"
    );
}

/// A burst is judged against the manifest's file count, and a session re-registers with a new one
/// when its workspace grows; keeping the boot-time count would compare every later burst to it.
#[test]
fn a_re_registered_session_refreshes_the_manifest_count() {
    let store = "/ws/index.redb";
    let mut watches: Vec<Watch> = vec![];
    let mut first = session(store, "/ws/a");
    first.docs = 100;
    sync(&mut watches, groups(&[first]), debounce());
    assert_eq!(watches[0].manifest, 100);

    let mut grown = session(store, "/ws/a");
    grown.docs = 250;
    sync(&mut watches, groups(&[grown]), debounce());

    assert_eq!(
        watches[0].manifest, 250,
        "the same watcher set must carry the current manifest, not the boot-time one"
    );
}
