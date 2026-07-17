//! Disk-persisted, low-resident-memory backing store for the graph.
//!
//! The graph's node maps are keyed by `Id<T>` (a `NonZeroU64` content hash). That maps directly onto
//! an embedded key-value store: `u64 -> serialized_node_bytes`. This module persists the full
//! resolved graph to a redb database and serves reads back through the `Graph`'s layered accessors
//! (`Graph::with_store` / `Graph::attach_store`), keeping the bulk index off the heap.

// Spike-only: a (de)serialization failure here means the on-disk store is corrupt, which we treat as
// unrecoverable for now. Stage 1 replaces these `.expect()`s with a proper `StoreError` type.
#![allow(clippy::missing_panics_doc)]

use std::path::Path;

use redb::{Database, ReadableDatabase, TableDefinition};

use serde::de::DeserializeOwned;

use crate::model::declaration::Declaration;
use crate::model::definitions::Definition;
use crate::model::document::Document;
use crate::model::graph::Graph;
use crate::model::ids::{ConstantReferenceId, DeclarationId, DefinitionId, MethodReferenceId, NameId, StringId, UriId};
use crate::model::name::NameRef;
use crate::model::references::{ConstantReference, MethodRef};
use crate::model::string_ref::StringRef;

// One redb table per graph node map, all keyed by the node's `u64` content-hash ID.
const STRINGS: TableDefinition<u64, &[u8]> = TableDefinition::new("strings");
const DECLARATIONS: TableDefinition<u64, &[u8]> = TableDefinition::new("declarations");
const DEFINITIONS: TableDefinition<u64, &[u8]> = TableDefinition::new("definitions");
const NAMES: TableDefinition<u64, &[u8]> = TableDefinition::new("names");
const CONSTANT_REFERENCES: TableDefinition<u64, &[u8]> = TableDefinition::new("constant_references");
const METHOD_REFERENCES: TableDefinition<u64, &[u8]> = TableDefinition::new("method_references");
const DOCUMENTS: TableDefinition<u64, &[u8]> = TableDefinition::new("documents");
const NAME_DEPENDENTS: TableDefinition<u64, &[u8]> = TableDefinition::new("name_dependents");

/// A redb-backed node store.
pub struct RedbStore {
    db: Database,
}

impl std::fmt::Debug for RedbStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("RedbStore { .. }")
    }
}

impl RedbStore {
    /// Opens an existing redb store at `path` — e.g. a server reading a prebuilt gem/stdlib index
    /// without holding the graph in memory.
    ///
    /// # Errors
    /// Returns an error if the database cannot be opened.
    pub fn open(path: &Path) -> Result<Self, redb::Error> {
        // Cap redb's in-heap page cache so a long-lived server stays lean; the OS still caches the
        // file, so a cache miss is a RAM hit (not disk) and costs ~0.15 ms. 8 MiB is the measured
        // sweet spot (~26 MB less RSS than 32 MiB, negligible latency).
        Ok(Self {
            db: Database::builder().set_cache_size(8 * 1024 * 1024).open(path)?,
        })
    }

    /// Builds an on-disk store at `path` from a fully-indexed, resolved graph, writing every node
    /// map into its table in a single write transaction.
    ///
    /// # Errors
    /// Returns an error if the database cannot be created or any redb transaction fails.
    pub fn build(path: &Path, graph: &Graph) -> Result<Self, redb::Error> {
        let db = Database::create(path)?;
        {
            let write_txn = db.begin_write()?;
            // Serialize each `(id, node)` pair into the node's table. The `{{ }}` scoping drops each
            // table guard before the next table is opened within the same transaction.
            macro_rules! write_map {
                ($table:expr, $map:expr) => {{
                    let mut table = write_txn.open_table($table)?;
                    for (id, value) in $map {
                        let bytes = postcard::to_allocvec(value).expect("node should serialize");
                        table.insert(id.get(), bytes.as_slice())?;
                    }
                }};
            }

            write_map!(DECLARATIONS, graph.declarations());
            write_map!(DEFINITIONS, graph.definitions());
            write_map!(STRINGS, graph.strings());
            write_map!(NAMES, graph.names());
            write_map!(CONSTANT_REFERENCES, graph.constant_references());
            write_map!(METHOD_REFERENCES, graph.method_references());
            write_map!(DOCUMENTS, graph.documents());
            write_map!(NAME_DEPENDENTS, graph.name_dependents());

            write_txn.commit()?;
        }
        Ok(Self { db })
    }

    /// Reads and deserializes a node of type `V` from `table` by its `u64` key, if present.
    ///
    /// # Errors
    /// Returns an error if the redb read transaction fails.
    fn get_node<V: DeserializeOwned>(
        &self,
        table: TableDefinition<u64, &[u8]>,
        key: u64,
    ) -> Result<Option<V>, redb::Error> {
        let read_txn = self.db.begin_read()?;
        let table = read_txn.open_table(table)?;
        match table.get(key)? {
            Some(guard) => Ok(Some(
                postcard::from_bytes::<V>(guard.value()).expect("node should deserialize"),
            )),
            None => Ok(None),
        }
    }

    /// Reads a single interned string, if present.
    ///
    /// # Errors
    /// Returns an error if the redb read transaction fails.
    pub fn get_string(&self, id: StringId) -> Result<Option<StringRef>, redb::Error> {
        self.get_node(STRINGS, id.get())
    }

    /// Reads a single declaration node, if present.
    ///
    /// # Errors
    /// Returns an error if the redb read transaction fails.
    pub fn get_declaration(&self, id: DeclarationId) -> Result<Option<Declaration>, redb::Error> {
        self.get_node(DECLARATIONS, id.get())
    }

    /// Reads a single definition node, if present.
    ///
    /// # Errors
    /// Returns an error if the redb read transaction fails.
    pub fn get_definition(&self, id: DefinitionId) -> Result<Option<Definition>, redb::Error> {
        self.get_node(DEFINITIONS, id.get())
    }

    /// Reads a single name node, if present.
    ///
    /// # Errors
    /// Returns an error if the redb read transaction fails.
    pub fn get_name(&self, id: NameId) -> Result<Option<NameRef>, redb::Error> {
        self.get_node(NAMES, id.get())
    }

    /// Reads a single constant reference node, if present.
    ///
    /// # Errors
    /// Returns an error if the redb read transaction fails.
    pub fn get_constant_reference(&self, id: ConstantReferenceId) -> Result<Option<ConstantReference>, redb::Error> {
        self.get_node(CONSTANT_REFERENCES, id.get())
    }

    /// Reads a single method reference node, if present.
    ///
    /// # Errors
    /// Returns an error if the redb read transaction fails.
    pub fn get_method_reference(&self, id: MethodReferenceId) -> Result<Option<MethodRef>, redb::Error> {
        self.get_node(METHOD_REFERENCES, id.get())
    }

    /// Reads a single document node, if present.
    ///
    /// # Errors
    /// Returns an error if the redb read transaction fails.
    pub fn get_document(&self, id: UriId) -> Result<Option<Document>, redb::Error> {
        self.get_node(DOCUMENTS, id.get())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_persists_full_graph() {
        // `Graph::new()` seeds built-in data (Object, BasicObject, etc.), giving us a real,
        // non-empty graph to persist without running the indexer.
        let graph = Graph::new();
        assert!(
            !graph.declarations().is_empty(),
            "built-in data should populate the graph"
        );

        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("graph.redb");
        let store = RedbStore::build(&path, &graph).expect("build store");

        // Every node in every map must round-trip byte-identically through the built store.
        macro_rules! assert_roundtrip {
            ($map:expr, $getter:ident) => {
                for (id, value) in $map {
                    let loaded = store.$getter(*id).expect("get").expect("node present in store");
                    assert_eq!(
                        postcard::to_allocvec(value).expect("serialize in-memory"),
                        postcard::to_allocvec(&loaded).expect("serialize loaded"),
                    );
                }
            };
        }

        assert_roundtrip!(graph.declarations(), get_declaration);
        assert_roundtrip!(graph.definitions(), get_definition);
        assert_roundtrip!(graph.names(), get_name);
        assert_roundtrip!(graph.strings(), get_string);
        assert_roundtrip!(graph.constant_references(), get_constant_reference);
        assert_roundtrip!(graph.method_references(), get_method_reference);
        assert_roundtrip!(graph.documents(), get_document);
    }

    #[test]
    fn layered_graph_reads_declaration_from_store() {
        use crate::model::graph::DeclRef;

        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("base.redb");

        // Build a store from a graph (its built-in declarations give us real nodes to read back).
        let base = Graph::new();
        let (sample_id, sample_name) = base
            .declarations()
            .iter()
            .next()
            .map(|(id, declaration)| (*id, declaration.name().to_string()))
            .expect("built-in declarations exist");
        RedbStore::build(&path, &base).expect("build store");
        drop(base);

        // A store-backed graph has empty in-memory maps; the lookup must come from disk.
        let graph = Graph::with_store(RedbStore::open(&path).expect("open store"));
        assert!(graph.declarations().get(&sample_id).is_none(), "memory layer is empty");

        let declaration = graph.declaration(sample_id).expect("declaration from store");
        assert!(matches!(declaration, DeclRef::Stored(_)), "should be store-backed");
        assert_eq!(declaration.name(), sample_name);
    }

    #[test]
    fn completion_surfaces_members_from_store() {
        use crate::indexing::{IndexerBackend, index_files};
        use crate::query::{CompletionCandidate, CompletionContext, CompletionReceiver, completion_candidates};
        use crate::resolution::Resolver;

        let dir = tempfile::tempdir().expect("tempdir");
        let rb_path = dir.path().join("animal.rb");
        std::fs::write(&rb_path, "class Animal\n  def speak; end\nend\n").expect("write rb");

        let mut graph = Graph::new();
        let _ = index_files(&mut graph, vec![rb_path], IndexerBackend::RubyIndexer);
        Resolver::new(&mut graph).resolve();

        let store_path = dir.path().join("index.redb");
        RedbStore::build(&store_path, &graph).expect("build store");
        drop(graph); // the answer must come from disk, not in-memory maps

        // A fresh store-backed graph has empty in-memory maps, so completing a method call on
        // `Animal` can only surface its `speak` member by reading through the layered accessor.
        // This is the gem-member completion path that previously degraded to empty.
        let graph = Graph::with_store(RedbStore::open(&store_path).expect("open store"));
        assert!(graph.declarations().is_empty(), "memory layer is empty");

        let receiver = CompletionReceiver::MethodCall {
            self_decl_id: None,
            receiver_decl_id: DeclarationId::from("Animal"),
        };
        let candidates = completion_candidates(&graph, CompletionContext::new(receiver)).expect("completion");

        let names: Vec<String> = candidates
            .iter()
            .filter_map(|c| match c {
                CompletionCandidate::Declaration(id) => Some(graph.declaration(*id)?.name().to_string()),
                _ => None,
            })
            .collect();
        assert!(
            names.iter().any(|n| n.contains("speak")),
            "expected `speak` from store-backed members, got {names:?}"
        );
    }
}
