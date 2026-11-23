//! The event source is the only thing the index manager talks to, so these tests are
//! the contract a file-system event backend has to satisfy: watch a directory
//! recursively, report creation/modification/removal, and coalesce a burst of
//! events into one batch.

use std::path::Path;
use std::time::{Duration, Instant};

use rubydex_index_manager::fs_events::NotifySource;

/// Drain one batch (waiting up to three seconds) and report whether `file` is in it.
fn batch_contains(source: &mut NotifySource, file: &Path) -> bool {
    let batch = source.try_next_batch(Duration::from_secs(3));
    batch.iter().any(|path| path == file)
}

/// A source watching `root` with no ignored directories.
fn event_source(root: &Path, debounce_ms: u64) -> NotifySource {
    NotifySource::new(&[root.to_path_buf()], Vec::new(), Duration::from_millis(debounce_ms))
}

#[test]
fn take_ready_batch_does_not_block_and_waits_out_the_debounce() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path().to_path_buf();
    let source = event_source(&root, 200);

    // Idle: the manager's loop cannot afford to wait a debounce window per workspace.
    let started = Instant::now();
    assert!(
        source.take_ready_batch().is_empty(),
        "an idle source has nothing to hand out"
    );
    assert!(
        started.elapsed() < Duration::from_millis(50),
        "an idle source must answer immediately, took {:?}",
        started.elapsed()
    );

    let file = root.join("a.rb");
    std::fs::write(&file, "class A; end\n").expect("write");
    // The burst is still settling, so nothing is handed out yet.
    assert!(
        source.take_ready_batch().is_empty(),
        "a burst inside the debounce window is not ready"
    );

    // Poll until the OS delivers the event and the debounce window elapses (macOS FSEvents and
    // busy CI runners can take well over 50ms just to deliver the write, and may report parent
    // directory paths alongside the file).
    let deadline = Instant::now() + Duration::from_secs(3);
    let batch = loop {
        let ready = source.take_ready_batch();
        if !ready.is_empty() || Instant::now() >= deadline {
            break ready;
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    assert!(
        batch.contains(&file),
        "the settled burst must contain {file:?}, got {batch:?}"
    );
    assert!(
        source.take_ready_batch().is_empty(),
        "a drained batch is not served twice"
    );
}

#[test]
fn adapter_reports_a_created_file() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path().to_path_buf();
    let mut source = event_source(&root, 50);
    let file = root.join("a.rb");
    std::fs::write(&file, "class A; end").expect("write");
    assert!(
        batch_contains(&mut source, &file),
        "the adapter never reported the created file"
    );
}

#[test]
fn adapter_reports_a_modified_file() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path().to_path_buf();
    let mut source = event_source(&root, 50);
    let file = root.join("a.rb");
    std::fs::write(&file, "class A; end").expect("write");
    assert!(
        batch_contains(&mut source, &file),
        "setup: creation must be reported first"
    );
    std::fs::write(&file, "class A; def b; end").expect("write");
    assert!(
        batch_contains(&mut source, &file),
        "the adapter never reported the modified file"
    );
}

#[test]
fn adapter_reports_a_deleted_file() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path().to_path_buf();
    let mut source = event_source(&root, 50);
    let file = root.join("a.rb");
    std::fs::write(&file, "class A; end").expect("write");
    assert!(
        batch_contains(&mut source, &file),
        "setup: creation must be reported first"
    );
    std::fs::remove_file(&file).expect("remove");
    assert!(
        batch_contains(&mut source, &file),
        "the adapter never reported the deleted file"
    );
}

#[test]
fn adapter_coalesces_a_burst_into_one_batch() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path().to_path_buf();
    let source = event_source(&root, 200);
    let files = vec![root.join("a.rb"), root.join("b.rb"), root.join("c.rb")];
    for file in &files {
        std::fs::write(file, "class A; end").expect("write");
    }
    let batch = source.try_next_batch(Duration::from_secs(3));
    for file in &files {
        assert!(batch.contains(file), "burst split across batches: {file:?}");
    }
}

/// A root that cannot be watched must be reported rather than silently dropped, and retried: the
/// per-user inotify budget that caused the failure frees up later, and without a retry the workspace
/// stays unwatched for the manager's whole lifetime.
#[test]
fn an_unwatchable_root_is_reported_and_can_recover() {
    let dir = tempfile::tempdir().expect("tempdir");
    let missing = dir.path().join("not-yet");
    let mut source = event_source(&missing, 50);

    assert_eq!(
        source.unwatched(),
        std::slice::from_ref(&missing),
        "a root that could not be watched has to be reported"
    );
    assert_eq!(source.retry_unwatched(), 1, "and retried while it is still missing");

    std::fs::create_dir(&missing).expect("create");
    assert_eq!(source.retry_unwatched(), 0, "once it exists the retry must succeed");
    assert_eq!(
        source.unwatched(),
        [] as [std::path::PathBuf; 0],
        "the retried root must no longer be listed as unwatched"
    );

    let file = missing.join("a.rb");
    std::fs::write(&file, "class A; end").expect("write");
    assert!(
        batch_contains(&mut source, &file),
        "a retried root must actually deliver events"
    );
}

/// A retry must not disturb the roots that are already being watched.
#[test]
fn retrying_one_root_keeps_the_others_watched() {
    let dir = tempfile::tempdir().expect("tempdir");
    let good = dir.path().to_path_buf();
    let missing = dir.path().join("not-yet");
    let paths = [good.clone(), missing.clone()];
    let mut source = NotifySource::new(&paths, Vec::new(), Duration::from_millis(50));
    assert_eq!(
        source.unwatched(),
        std::slice::from_ref(&missing),
        "only the missing root failed"
    );

    std::fs::create_dir(&missing).expect("create");
    assert_eq!(source.retry_unwatched(), 0);

    let file = good.join("a.rb");
    std::fs::write(&file, "class A; end").expect("write");
    assert!(
        batch_contains(&mut source, &file),
        "the healthy root stopped delivering"
    );
}

/// The store and `.git` live inside the watched workspace; their writes must not look like source
/// changes, or every build re-arms another indexer and every git command wakes the manager.
#[test]
fn the_store_cache_and_git_metadata_are_ignored() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path().to_path_buf();
    let store = root.join("tmp").join("rubydex");
    std::fs::create_dir_all(store.join("nested")).expect("store dir");
    let source = NotifySource::new(
        std::slice::from_ref(&root),
        vec![store.clone()],
        Duration::from_millis(50),
    );

    std::fs::write(store.join("index.redb"), "store").expect("store write");
    std::fs::write(store.join("nested").join("index.redb.building"), "temp").expect("temp write");
    let git_dir = root.join(".git");
    std::fs::create_dir(&git_dir).expect("git dir");
    std::fs::write(git_dir.join("index"), "git").expect("git write");

    // A real source event is the control: the batch must still be delivered without the noise.
    let file = root.join("a.rb");
    std::fs::write(&file, "class A; end").expect("write");

    let batch = source.try_next_batch(Duration::from_secs(3));
    assert!(batch.contains(&file), "a source change must still wake the manager");
    assert!(
        !batch.iter().any(|path| path.starts_with(&store)),
        "store writes must not feed back as source events: {batch:?}"
    );
    assert!(
        !batch.iter().any(|path| path.starts_with(&git_dir)),
        "git metadata must not feed back as source events: {batch:?}"
    );
}

/// Only a write that can change the index reaches the batch. A burst of unrelated files — an
/// editor's swap file, `node_modules` churn, a log rotation, a `.rb` under `tmp` — otherwise counts
/// towards the rebuild threshold and forces a rebuild of a workspace nobody edited.
#[test]
fn only_writes_that_can_change_the_index_are_reported() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path().to_path_buf();
    for excluded in ["node_modules/pkg", "log", "tmp", "assets"] {
        std::fs::create_dir_all(root.join(excluded)).expect("dirs");
    }
    let source = event_source(&root, 50);

    for noise in [
        "README.md",
        "assets/app.css",
        "node_modules/pkg/index.js",
        "log/development.log",
        "tmp/cache.rb",
    ] {
        std::fs::write(root.join(noise), "ignored").expect("write");
    }
    let source_file = root.join("a.rb");
    let lockfile = root.join("Gemfile.lock");
    std::fs::write(&source_file, "class A; end").expect("write");
    std::fs::write(&lockfile, "GEM\n").expect("write");

    let batch = source.try_next_batch(Duration::from_secs(3));
    assert!(
        batch.contains(&source_file),
        "a source write must be reported: {batch:?}"
    );
    assert!(
        batch.contains(&lockfile),
        "a lockfile write must be reported: {batch:?}"
    );
    assert!(
        batch.iter().all(|path| path
            .extension()
            .is_some_and(|extension| { extension == "rb" || extension == "lock" })),
        "unrelated writes must not reach the batch: {batch:?}"
    );
}

/// The root is watched without recursion and its top-level directories with it, so a write deeper
/// in the tree still arrives.
#[test]
fn a_write_in_a_subdirectory_is_reported() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path().to_path_buf();
    std::fs::create_dir_all(root.join("app/models")).expect("dirs");
    let mut source = event_source(&root, 50);

    let file = root.join("app/models/user.rb");
    std::fs::write(&file, "class User; end").expect("write");

    assert!(
        batch_contains(&mut source, &file),
        "a nested source write must be reported"
    );
}
