//! A corrupt store must be reported, not fatal. Decoding a damaged node used to `.expect()`,
//! and a panic inside `extern "C"` cannot unwind, so it aborted the host process (ruby-lsp).

#![cfg(feature = "redb-store")]

use rubydex::{
    indexing::{LanguageId, index_source},
    model::{graph::Graph, ids::DeclarationId, store::RedbStore},
    resolution::Resolver,
};

/// Overwrites one node's bytes with a structurally valid postcard payload of the wrong shape,
/// standing in for a store written by an incompatible layout or damaged in transit.
fn corrupt_node(table_name: &str, id: u64, path: &std::path::Path) {
    let db = redb::Database::builder().open(path).expect("open for corruption");
    let write_txn = db.begin_write().expect("write txn");
    {
        let mut table = write_txn
            .open_table(redb::TableDefinition::<u64, &[u8]>::new(table_name))
            .expect("table");
        let wrong_shape = postcard::to_allocvec(&vec![1u8, 2, 3]).expect("encode");
        table.insert(id, wrong_shape.as_slice()).expect("overwrite");
    }
    write_txn.commit().expect("commit corruption");
}

#[test]
fn corrupt_node_returns_error_instead_of_aborting() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("corrupt.redb");

    let mut graph = Graph::new();
    index_source(
        &mut graph,
        "file:///foo.rb".into(),
        "class Foo\n  BAR = 1\nend\n",
        &LanguageId::Ruby,
    );
    Resolver::new(&mut graph).resolve();
    RedbStore::build(&path, &graph).expect("build store");

    corrupt_node("declarations", DeclarationId::from("Foo").get(), &path);

    // The database is structurally valid, so it still opens; decoding the damaged node must
    // report an error instead of panicking.
    let store = RedbStore::open(&path).expect("store still opens");
    let result = store.get_declaration(DeclarationId::from("Foo"));
    assert!(
        matches!(result, Err(rubydex::model::store::StoreError::Corrupt { .. })),
        "expected StoreError::Corrupt, got {result:?}"
    );
}
