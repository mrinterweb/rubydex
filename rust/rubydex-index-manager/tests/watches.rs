//! The watcher sets stay one per store as sessions join, leave and change.

use rubydex_index_manager::registry::Session;
use rubydex_index_manager::watches::Watch;
use rubydex_index_manager::watches::groups;
use rubydex_index_manager::watches::sync;

use std::path::PathBuf;
use std::time::Duration;

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
        "the watcher set was replaced, so it covers both workspaces"
    );
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
