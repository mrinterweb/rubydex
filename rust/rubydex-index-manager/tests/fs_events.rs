//! The adapter is the only thing the index manager talks to, so these tests are the
//! contract any future file-system event backend has to satisfy: watch a directory
//! recursively, report creation/modification/removal, and coalesce a burst of
//! events into one batch.

use std::path::Path;
use std::time::Duration;

use rubydex_index_manager::fs_events::{FsEventSource, NotifySource};

/// Drain one batch (waiting up to three seconds) and report whether `file` is in it.
fn batch_contains(source: &mut NotifySource, file: &Path) -> bool {
    let batch = source.try_next_batch(Duration::from_secs(3));
    batch.iter().any(|path| path == file)
}

#[test]
fn adapter_reports_a_created_file() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path().to_path_buf();
    let mut source = NotifySource::new(vec![root.clone()], Duration::from_millis(50));
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
    let mut source = NotifySource::new(vec![root.clone()], Duration::from_millis(50));
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
    let mut source = NotifySource::new(vec![root.clone()], Duration::from_millis(50));
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
    let mut source = NotifySource::new(vec![root.clone()], Duration::from_millis(200));
    let files = vec![root.join("a.rb"), root.join("b.rb"), root.join("c.rb")];
    for file in &files {
        std::fs::write(file, "class A; end").expect("write");
    }
    let batch = source.try_next_batch(Duration::from_secs(3));
    for file in &files {
        assert!(batch.contains(file), "burst split across batches: {file:?}");
    }
}
