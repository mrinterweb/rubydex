//! Every node type must survive a store round trip without losing any field.
//!
//! Nodes come from a real indexed corpus rather than hand-written shapes, so the payloads exercise
//! every field the indexer actually produces. Nodes are compared field-wise, not byte-wise:
//! `IdentityHashMap`/`IdentityHashSet` iterate in insertion order, so a map-bearing node
//! serializes in a different order after a reload. Byte identity is not the contract; field
//! fidelity is.

#![cfg(feature = "redb-store")]

use std::collections::BTreeSet;

use rubydex::model::id::Id;
use rubydex::{
    indexing::{IndexerBackend, index_files},
    listing::collect_file_paths,
    model::{
        declaration::{Ancestor, Namespace},
        graph::Graph,
        ids::{DeclarationId, NameId},
        store::RedbStore,
    },
    resolution::Resolver,
};

fn corpus_dir() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/diff_corpus")
}

fn indexed_graph() -> Graph {
    let mut memory = Graph::new();
    let (files, _) = collect_file_paths(vec![corpus_dir().to_string_lossy().into_owned()], &memory.excluded_patterns());
    let _ = index_files(&mut memory, files, IndexerBackend::RubyIndexer);
    Resolver::new(&mut memory).resolve();
    memory
}

/// Renders a node's identity for failure messages.
fn describe(graph: &Graph, id: DeclarationId) -> String {
    graph
        .declaration(id)
        .map_or_else(|| format!("<missing {id}>"), |declaration| declaration.name().to_string())
}

/// Renders an ancestor chain by name, so complete ancestors compare across graphs and partial ones
/// (unresolved `NameId`s) compare by id.
fn ancestors_by_name(graph: &Graph, id: DeclarationId) -> Option<String> {
    graph.declaration(id)?.as_namespace().map(|namespace| {
        namespace
            .ancestors()
            .iter()
            .map(|ancestor| match ancestor {
                Ancestor::Complete(ancestor_id) => format!("c:{}", describe(graph, *ancestor_id)),
                Ancestor::Partial(name_id) => format!("p:{}", name_id.get()),
            })
            .collect::<Vec<_>>()
            .join(",")
    })
}

/// Field-wise comparison of one declaration: the in-memory copy against the store-loaded copy.
fn check_declaration(memory: &Graph, store_backed: &Graph, id: DeclarationId, failures: &mut Vec<String>) {
    let Some(before) = memory.declarations().get(&id) else {
        return;
    };
    let label = before.name().to_string();

    let Some(after) = store_backed.declaration(id) else {
        failures.push(format!("declaration {label} missing from the store"));
        return;
    };

    if before.name() != after.name() {
        failures.push(format!("declaration {label}: name differs ({} vs {})", before.name(), after.name()));
    }
    if before.kind() != after.kind() {
        failures.push(format!("declaration {label}: kind differs ({} vs {})", before.kind(), after.kind()));
    }
    if before.definitions() != after.definitions() {
        failures.push(format!("declaration {label}: definitions differ"));
    }
    if before.owner_id() != after.owner_id() {
        failures.push(format!("declaration {label}: owner differs"));
    }
    if before.as_namespace().map(Namespace::references) != after.as_namespace().map(Namespace::references) {
        failures.push(format!("declaration {label}: references differ"));
    }

    match (before.as_namespace(), after.as_namespace()) {
        (Some(expected), Some(actual)) => {
            // Strings are owned copies per graph, so render to owned text to compare across graphs.
            let members = |graph: &Graph, namespace: &rubydex::model::declaration::Namespace| {
                namespace
                    .members()
                    .iter()
                    .map(|(string_id, member_id)| {
                        (
                            graph.string(*string_id).map_or(String::new(), |string| string.as_str().to_string()),
                            member_id.get(),
                        )
                    })
                    .collect::<BTreeSet<_>>()
            };
            let expected_members = members(memory, expected);
            let actual_members = members(store_backed, actual);
            if expected_members != actual_members {
                failures.push(format!("declaration {label}: members differ"));
            }

            let descendants = |namespace: &rubydex::model::declaration::Namespace| namespace.descendants().iter().map(Id::get).collect::<BTreeSet<_>>();
            if descendants(expected) != descendants(actual) {
                failures.push(format!("declaration {label}: descendants differ"));
            }

            if ancestors_by_name(memory, id) != ancestors_by_name(store_backed, id) {
                failures.push(format!("declaration {label}: ancestors differ"));
            }

            if expected.singleton_class().copied() != actual.singleton_class().copied() {
                failures.push(format!("declaration {label}: singleton class differs"));
            }
        }
        (None, None) => {}
        (Some(_), None) | (None, Some(_)) => {
            failures.push(format!("declaration {label}: namespace-ness differs across the store"));
        }
    }
}

#[test]
fn every_node_type_round_trips_through_the_store() {
    let dir = tempfile::tempdir().expect("tempdir");
    let store_path = dir.path().join("roundtrip.redb");

    let memory = indexed_graph();
    RedbStore::build(&store_path, &memory).expect("build store");
    // One open per file: redb locks the file exclusively, so the graph and this test share it.
    let store = RedbStore::open(&store_path).expect("reopen store");
    let store_backed = Graph::with_store(store);

    let mut failures: Vec<String> = Vec::new();

    // Declarations (built-ins included: `Graph::new` seeds them, resolution mutates them).
    for id in memory.declarations().keys().copied().collect::<Vec<_>>() {
        check_declaration(&memory, &store_backed, id, &mut failures);
    }

    // Definitions.
    for (id, definition) in memory.definitions() {
        let Some(loaded) = store_backed.definition(*id) else {
            failures.push(format!("definition {} missing", id.get()));
            continue;
        };
        if postcard::to_allocvec(definition).expect("serialize in-memory definition")
            != postcard::to_allocvec(&*loaded).expect("re-serialize loaded definition")
        {
            failures.push(format!("definition {} differs", id.get()));
        }
    }

    // Strings (value + reference count).
    for (id, string) in memory.strings() {
        let Some(loaded) = store_backed.string(*id) else {
            failures.push(format!("string {} missing", string.as_str()));
            continue;
        };
        if string.as_str() != loaded.as_str() || string.ref_count() != loaded.ref_count() {
            failures.push(format!("string {} differs", string.as_str()));
        }
    }

    // Names.
    for (id, name) in memory.names() {
        let before = postcard::to_allocvec(name).expect("serialize in-memory name");
        let Some(loaded) = store_backed.name(*id) else {
            failures.push(format!("name {} missing", id.get()));
            continue;
        };
        if before != postcard::to_allocvec(&*loaded).expect("re-serialize loaded name") {
            failures.push(format!("name {} differs", id.get()));
        }
    }

    // References.
    for (id, reference) in memory.constant_references() {
        let before = postcard::to_allocvec(reference).expect("serialize in-memory reference");
        let Some(loaded) = store_backed.constant_reference(*id) else {
            failures.push(format!("constant_reference {} missing", id.get()));
            continue;
        };
        if before != postcard::to_allocvec(&*loaded).expect("re-serialize loaded reference") {
            failures.push(format!("constant_reference {} differs", id.get()));
        }
    }
    for (id, reference) in memory.method_references() {
        let before = postcard::to_allocvec(reference).expect("serialize in-memory reference");
        let Some(loaded) = store_backed.method_reference(*id) else {
            failures.push(format!("method_reference {} missing", id.get()));
            continue;
        };
        if before != postcard::to_allocvec(&*loaded).expect("re-serialize loaded reference") {
            failures.push(format!("method_reference {} differs", id.get()));
        }
    }

    // Documents. `LineIndex` is deliberately skipped by the store (it is rebuilt from source on
    // read), so compare the retained fields: URI, definition ids, and content hash.
    for (id, document) in memory.documents() {
        let Some(loaded) = store_backed.document(*id) else {
            failures.push(format!("document {} missing", document.uri()));
            continue;
        };
        if document.uri() != loaded.uri()
            || document.definitions() != loaded.definitions()
            || document.content_hash() != loaded.content_hash()
        {
            failures.push(format!("document {} differs", document.uri()));
        }
    }

    // Name dependents (reverse index used by invalidation). `Graph#name_dependents` serves the
    // in-memory overlay only; store-backed entries arrive via `materialize_name_dependents` when
    // invalidation needs them. Materialize everything, then compare entry by entry.
    let mut materialized = store_backed;
    let ids: Vec<NameId> = memory.name_dependents().keys().copied().collect();
    for id in &ids {
        materialized.materialize_name_dependents(*id);
    }
    for id in &ids {
        let Some(dependents) = memory.name_dependents().get(id) else { continue };
        let Some(loaded) = materialized.name_dependents().get(id) else {
            failures.push(format!("name_dependents {} missing", id.get()));
            continue;
        };
        if postcard::to_allocvec(dependents).expect("serialize in-memory dependents")
            != postcard::to_allocvec(loaded).expect("re-serialize loaded dependents")
        {
            failures.push(format!("name_dependents {} differs", id.get()));
        }
    }

    assert!(
        failures.is_empty(),
        "round-trip mismatches:\n{}",
        failures.join("\n")
    );
}

#[test]
fn built_ins_round_trip_through_the_store() {
    let dir = tempfile::tempdir().expect("tempdir");
    let store_path = dir.path().join("builtins.redb");

    let built_ins = Graph::new();
    RedbStore::build(&store_path, &built_ins).expect("build store");
    // One open per file: redb locks the file exclusively, so the graph and this test share it.
    let store = RedbStore::open(&store_path).expect("reopen store");
    let store_backed = Graph::with_store(store);

    let mut failures: Vec<String> = Vec::new();
    for id in built_ins.declarations().keys().copied().collect::<Vec<_>>() {
        check_declaration(&built_ins, &store_backed, id, &mut failures);
    }
    assert!(
        failures.is_empty(),
        "built-in round-trip mismatches:\n{}",
        failures.join("\n")
    );
}
