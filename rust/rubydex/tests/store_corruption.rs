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

/// Overwrites every bucket row of `table_name` with a structurally valid bucket holding `id` and a
/// payload that is not a node, standing in for a store written by an incompatible layout or damaged
/// in transit. Writing it to every row avoids the test needing to know the store's bucket bit count.
fn corrupt_node(table_name: &str, id: u64, path: &std::path::Path) {
    let mut bad_bucket = Vec::new();
    bad_bucket.extend_from_slice(&1_u32.to_le_bytes()); // one entry
    bad_bucket.extend_from_slice(&id.to_le_bytes());
    bad_bucket.extend_from_slice(&0_u32.to_le_bytes()); // payload offset of entry 0
    bad_bucket.extend_from_slice(&3_u32.to_le_bytes()); // payload end
    bad_bucket.extend_from_slice(&[0xff, 0xff, 0xff]);

    let db = redb::Database::builder().open(path).expect("open for corruption");
    let write_txn = db.begin_write().expect("write txn");
    {
        use redb::ReadableTable;
        let mut table = write_txn
            .open_table(redb::TableDefinition::<u32, &[u8]>::new(table_name))
            .expect("table");
        let keys: Vec<u32> = table
            .range(0..=u32::MAX)
            .expect("range")
            .map(|entry| entry.expect("entry").0.value())
            .collect();
        for key in keys {
            table.insert(key, bad_bucket.as_slice()).expect("overwrite");
        }
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
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .open(path)
        .expect("open for damage");
    let size = file.metadata().expect("metadata").len();
    file.seek(SeekFrom::Start(from)).expect("seek");
    let chunk = vec![0xA5u8; 64 * 1024];
    for offset in (from..size).step_by(chunk.len()) {
        let length = usize::try_from(size - offset)
            .expect("length fits usize")
            .min(chunk.len());
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
    assert!(
        result.is_err(),
        "a store damaged past its header must not open: {result:?}"
    );
}

#[test]
fn corrupt_store_queries_and_live_edits_do_not_panic() {
    use rubydex::{
        model::ids::StringId,
        query::{self, CompletionContext, CompletionReceiver, FindMemberError},
    };

    let mut base = Graph::new();
    index_source(
        &mut base,
        "file:///foo.rb".into(),
        "class Foo\n  def bar; end\n  alias baz bar\nend\n",
        &LanguageId::Ruby,
    );
    index_source(
        &mut base,
        "file:///other.rb".into(),
        "class Other; end\n",
        &LanguageId::Ruby,
    );
    Resolver::new(&mut base).resolve();

    let foo_id = DeclarationId::from("Foo");
    let bar_id = StringId::from("bar");
    let alias_def_id = base
        .definitions()
        .iter()
        .find_map(|(id, def)| matches!(def, rubydex::model::definitions::Definition::MethodAlias(_)).then_some(*id))
        .expect("method alias definition");

    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("corrupt-queries.redb");
    RedbStore::build(&path, &base).expect("build store");

    // Corrupt declarations, definitions, and names buckets so store-backed lookups fail to decode
    // and return None after incrementing store_errors.
    corrupt_node("declarations", foo_id.get(), &path);
    corrupt_node("definitions", alias_def_id.get(), &path);
    corrupt_node("names", 0, &path);

    let mut graph = Graph::with_store(RedbStore::open(&path).expect("open store"));

    assert_eq!(
        query::find_member_in_ancestors(&graph, foo_id, bar_id, false),
        Err(FindMemberError::DeclarationNotFound)
    );
    assert!(query::follow_method_alias(&graph, alias_def_id).is_err());

    let candidates = query::completion_candidates(
        &graph,
        CompletionContext::new(CompletionReceiver::MethodCall {
            self_decl_id: None,
            receiver_decl_id: foo_id,
        }),
    )
    .expect("completion returns Ok(empty) rather than panicking");
    assert!(candidates.is_empty());

    // Live edit on a corrupt store must not panic in Resolver::prepare_units or remove_document_data.
    index_source(
        &mut graph,
        "file:///new.rb".into(),
        "class New; end\n",
        &LanguageId::Ruby,
    );
    Resolver::new(&mut graph).resolve();
    graph.delete_document("file:///foo.rb");

    assert!(graph.store_error_count() > 0);
}
