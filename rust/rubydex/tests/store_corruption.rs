//! A corrupt store must be reported, not fatal. Two failure shapes have to be absorbed, because a
//! panic inside `extern "C"` cannot unwind and so aborts the host process (ruby-lsp):
//!
//! 1. a node whose bytes no longer decode as its node type;
//! 2. damaged B-tree pages, where redb asserts inside its own code instead of returning an error.

#![cfg(feature = "redb-store")]

use std::io::{Seek, SeekFrom, Write};

use rubydex::{
    indexing::{LanguageId, index_source},
    model::{graph::Graph, ids::DeclarationId, store::RedbStore},
    resolution::Resolver,
};

fn indexed_graph() -> Graph {
    let mut graph = Graph::new();
    index_source(
        &mut graph,
        "file:///foo.rb".into(),
        "class Foo\n  BAR = 1\nend\n",
        &LanguageId::Ruby,
    );
    Resolver::new(&mut graph).resolve();
    graph
}

/// Overwrites one node's bytes with a structurally valid postcard payload of the wrong shape,
/// standing in for a store written by an incompatible layout or damaged in transit.
fn corrupt_node(table_name: &str, id: u64, path: &std::path::Path) {
    let db = redb::Database::builder().open(path).expect("open for corruption");
    let write_txn = db.begin_write().expect("write txn");
    {
        let mut table =
            write_txn.open_table(redb::TableDefinition::<u64, &[u8]>::new(table_name)).expect("table");
        let wrong_shape = postcard::to_allocvec(&vec![1u8, 2, 3]).expect("encode");
        table.insert(id, wrong_shape.as_slice()).expect("overwrite");
    }
    write_txn.commit().expect("commit corruption");
}

#[test]
fn corrupt_node_returns_error_instead_of_aborting() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("corrupt.redb");
    RedbStore::build(&path, &indexed_graph()).expect("build store");

    corrupt_node("declarations", DeclarationId::from("Foo").get(), &path);

    // The database is structurally valid, so it still opens; decoding the damaged node must report
    // an error rather than panicking.
    let store = RedbStore::open(&path).expect("store still opens");
    let result = store.get_declaration(DeclarationId::from("Foo"));
    assert!(
        matches!(result, Err(rubydex::model::store::StoreError::Corrupt { .. })),
        "expected a corruption error, got {result:?}"
    );
}

/// Overwrites `[from, end)` of the store with garbage, keeping everything before it intact.
fn damage_range(path: &std::path::Path, from: u64) {
    let mut file = std::fs::OpenOptions::new().write(true).open(path).expect("open for damage");
    let size = file.metadata().expect("metadata").len();
    file.seek(SeekFrom::Start(from)).expect("seek");
    let chunk = vec![0xA5u8; 64 * 1024];
    for offset in (from..size).step_by(chunk.len()) {
        let length = usize::try_from(size - offset).expect("length fits usize").min(chunk.len());
        file.write_all(&chunk[..length]).expect("write damage");
    }
    file.sync_all().expect("sync");
}

#[test]
fn damaged_store_fails_to_open_instead_of_aborting() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("header-damaged.redb");
    RedbStore::build(&path, &indexed_graph()).expect("build store");

    // Damage from just past the header, which takes redb's own allocator-state table with it.
    damage_range(&path, 4096);

    // redb asserts inside its B-tree code on such a page; opening must report it, not abort.
    let result = RedbStore::open(&path);
    assert!(result.is_err(), "a store damaged past its header must not open: {result:?}");
}
