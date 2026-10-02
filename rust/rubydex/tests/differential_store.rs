//! Differential tests: every query API must return identical results on the in-memory
//! graph and on the store-backed graph built from the same corpus. The in-memory graph
//! is the reference; the store path is only correct if it reproduces it exactly.

#![cfg(feature = "redb-store")]

use rubydex::{
    indexing::{index_files, IndexerBackend},
    listing::collect_file_paths,
    model::{
        graph::Graph,
        ids::NameId,
        store::RedbStore,
    },
    resolution::Resolver,
};
use std::path::{Path, PathBuf};

fn corpus_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/diff_corpus")
}

/// Indexes + resolves every Ruby file under `root` into a fresh in-memory graph.
fn build_graph_from(root: &Path) -> Graph {
    let mut graph = Graph::new();
    graph.set_workspace_path(root.to_path_buf());
    let (files, _errors) = collect_file_paths(
        vec![root.to_string_lossy().into_owned()],
        &graph.excluded_patterns(),
    );
    let _ = index_files(&mut graph, files, IndexerBackend::RubyIndexer);
    Resolver::new(&mut graph).resolve();
    graph
}

/// Builds the store from a corpus rooted at `root` and reopens it store-backed.
fn build_store_graph_from(root: &Path, store_dir: &Path) -> Graph {
    let memory = build_graph_from(root);
    let path = store_dir.join("index.redb");
    let store = RedbStore::build(&path, &memory).expect("build store");
    drop(memory);
    Graph::with_store(store)
}

/// Runs the full probe battery. Probes are added by later tasks; entries are
/// `(case, normalized_value)` pairs.
fn probe_all(graph: &Graph, name_ids: &[NameId], out: &mut Vec<(String, String)>) {
    let _ = (graph, name_ids, out);
}

fn normalize(entries: &mut Vec<(String, String)>) {
    entries.sort_by(|a, b| a.0.cmp(&b.0));
}

/// The net must catch a known divergence: the store graph is built from a corpus with an
/// extra file, so it must probe differently than the in-memory graph.
#[test]
fn harness_detects_divergence() {
    let dir = tempfile::tempdir().expect("tempdir");
    let extended = dir.path().join("extended_corpus");
    std::fs::create_dir_all(&extended).expect("mkdir");
    for entry in std::fs::read_dir(corpus_dir()).expect("read corpus") {
        let entry = entry.expect("dir entry");
        std::fs::copy(entry.path(), extended.join(entry.file_name())).expect("copy fixture");
    }
    std::fs::write(extended.join("extra.rb"), "class Extra\n  def extra_method; end\nend\n")
        .expect("write extra");

    let memory = build_graph_from(&corpus_dir());
    let store_graph = build_store_graph_from(&extended, dir.path());
    let name_ids: Vec<NameId> = memory.names().keys().copied().collect();

    let mut mem_probe = Vec::new();
    probe_all(&memory, &name_ids, &mut mem_probe);
    let mut store_probe = Vec::new();
    probe_all(&store_graph, &name_ids, &mut store_probe);
    normalize(&mut mem_probe);
    normalize(&mut store_probe);

    assert_ne!(
        mem_probe, store_probe,
        "harness failed to detect a known divergence (extra.rb present only in the store graph)"
    );
}

#[test]
fn differential_memory_vs_store() {
    let dir = tempfile::tempdir().expect("tempdir");
    let memory = build_graph_from(&corpus_dir());
    let store_graph = build_store_graph_from(&corpus_dir(), dir.path());
    let name_ids: Vec<NameId> = memory.names().keys().copied().collect();

    let mut mem_probe = Vec::new();
    probe_all(&memory, &name_ids, &mut mem_probe);
    let mut store_probe = Vec::new();
    probe_all(&store_graph, &name_ids, &mut store_probe);
    normalize(&mut mem_probe);
    normalize(&mut store_probe);

    assert_eq!(mem_probe.len(), store_probe.len(), "probe case count differs");
    for (m, s) in mem_probe.iter().zip(store_probe.iter()) {
        assert_eq!(m.0, s.0, "probe case sets differ");
        assert_eq!(m.1, s.1, "divergence in case {}", m.0);
    }
}
