//! The event source is the only thing the index manager talks to, so these tests are
//! the contract a file-system event backend has to satisfy: watch a directory
//! recursively, report creation/modification/removal, and coalesce a burst of
//! events into one batch.

use std::path::Path;
use std::time::Duration;

use rubydex_index_manager::fs_events::NotifySource;

/// Drain one batch (waiting up to three seconds) and report whether `file` is in it.
fn batch_contains(source: &mut NotifySource, file: &Path) -> bool {
    let batch = source.try_next_batch(Duration::from_secs(3));
    batch.iter().any(|path| path == file)
}

#[test]
fn adapter_reports_a_created_file() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path().to_path_buf();
    let mut source = NotifySource::new(std::slice::from_ref(&root), Duration::from_millis(50));
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
    let mut source = NotifySource::new(std::slice::from_ref(&root), Duration::from_millis(50));
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
    let mut source = NotifySource::new(std::slice::from_ref(&root), Duration::from_millis(50));
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
    let mut source = NotifySource::new(std::slice::from_ref(&root), Duration::from_millis(200));
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
    let mut source = NotifySource::new(std::slice::from_ref(&missing), Duration::from_millis(50));

    assert_eq!(
        source.unwatched(),
        std::slice::from_ref(&missing),
        "a root that could not be watched has to be reported"
    );
    assert_eq!(source.retry_unwatched(), 1, "and retried while it is still missing");

    std::fs::create_dir(&missing).expect("create");
    assert_eq!(source.retry_unwatched(), 0, "once it exists the retry must succeed");
    assert!(source.unwatched().is_empty());

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
    let mut source = NotifySource::new(&paths, Duration::from_millis(50));
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
