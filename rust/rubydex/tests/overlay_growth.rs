//! The in-memory overlay must not grow without bound under live edits.
//!
//! A store-backed session materializes nodes on demand and keeps every live-edited node in the
//! overlay forever. This test rewrites files many times and asserts the overlay stays bounded —
//! the ship plan's "overlay ceiling" decision input.

#![cfg(feature = "redb-store")]

use rubydex::{
    indexing::{IndexerBackend, LanguageId, index_files, index_source},
    model::{
        built_in::{BASIC_OBJECT_ID, CLASS_ID, KERNEL_ID, MODULE_ID, OBJECT_ID},
        graph::Graph,
        store::RedbStore,
    },
    resolution::Resolver,
};

#[test]
fn repeated_edits_of_the_same_files_do_not_grow_the_overlay() {
    let dir = tempfile::tempdir().expect("tempdir");
    let store_path = dir.path().join("overlay.redb");

    // A small workspace: two files, a handful of declarations each.
    let a = dir.path().join("a.rb");
    let b = dir.path().join("b.rb");
    std::fs::write(&a, "class Alpha\n  def alpha_method; end\nend\n").expect("write a");
    std::fs::write(&b, "class Beta\n  def beta_method; end\nend\n").expect("write b");

    let mut graph = Graph::new();
    let _ = index_files(&mut graph, vec![a.clone(), b.clone()], IndexerBackend::RubyIndexer);
    Resolver::new(&mut graph).resolve();
    RedbStore::build(&store_path, &graph).expect("build store");
    drop(graph);

    let mut graph = Graph::with_store(RedbStore::open(&store_path).expect("reopen store"));
    // The boot memory is the resident built-ins (attach keeps them for the hot completion
    // walks) — nothing else from the store may leak into the overlay.
    let built_ins = [*BASIC_OBJECT_ID, *KERNEL_ID, *OBJECT_ID, *MODULE_ID, *CLASS_ID];
    assert!(
        graph.declarations().keys().all(|id| built_ins.contains(id)),
        "memory layer starts with only the resident built-ins"
    );

    // 500 edit rounds over the same two files, each round changing the method name so every edit
    // is real work: a new definition, a removed one, and a re-resolution.
    let uri_a = url::Url::from_file_path(&a).unwrap().to_string();
    let uri_b = url::Url::from_file_path(&b).unwrap().to_string();
    for round in 0..500 {
        index_source(
            &mut graph,
            uri_a.clone().into(),
            &format!("class Alpha\n  def method_{round}; end\nend\n"),
            &LanguageId::Ruby,
        );
        index_source(
            &mut graph,
            uri_b.clone().into(),
            &format!("class Beta\n  def method_{round}; end\nend\n"),
            &LanguageId::Ruby,
        );
        Resolver::new(&mut graph).resolve();
    }

    let declarations = graph.declarations().len();
    let definitions = graph.definitions().len();
    let strings = graph.strings().len();
    let names = graph.names().len();
    let references = graph.constant_references().len();
    println!(
        "OVERLAY after 500 rounds: declarations={declarations} definitions={definitions} \
         strings={strings} names={names} references={references}"
    );

    // Each round edits the same two declarations. The overlay should hold roughly: the two edited
    // declarations, their definitions, and the strings/names they touch — not 500 generations.
    // 500 rounds × 2 files would be ~1000+ generations if the overlay never pruned.
    assert!(
        declarations <= 20,
        "overlay retained {declarations} declarations after 500 rounds of editing two files"
    );
    assert!(
        definitions <= 60,
        "overlay retained {definitions} definitions after 500 rounds"
    );
}
