//! Disk-persisted, low-resident-memory backing store for the graph (Stage 0 spike).
//!
//! The graph's node maps are keyed by `Id<T>` (a `NonZeroU64` content hash). That maps directly onto
//! an embedded key-value store: `u64 -> serialized_node_bytes`. This module is the start of that
//! backing store. Stage 0 proves the pipeline end-to-end for the simplest node type (`StringRef`):
//! `u64` key + serde/postcard value + redb round-trip. Later stages generalize this to every node
//! map and route the `Graph`'s accessors through it.

// Spike-only: a (de)serialization failure here means the on-disk store is corrupt, which we treat as
// unrecoverable for now. Stage 1 replaces these `.expect()`s with a proper `StoreError` type.
#![allow(clippy::missing_panics_doc)]

use std::path::Path;

use redb::{Database, ReadableDatabase, TableDefinition};

use crate::model::declaration::Declaration;
use crate::model::graph::Graph;
use crate::model::ids::{DeclarationId, StringId};
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

/// A redb-backed node store. Stage 0: strings only.
pub struct RedbStore {
    db: Database,
}

impl RedbStore {
    /// Creates (or opens) a redb database at `path`.
    ///
    /// # Errors
    /// Returns an error if the database file cannot be created or opened.
    pub fn create(path: &Path) -> Result<Self, redb::Error> {
        Ok(Self {
            db: Database::create(path)?,
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

    /// Serializes and writes a single interned string.
    ///
    /// # Errors
    /// Returns an error if serialization or the redb transaction fails.
    pub fn put_string(&self, id: StringId, value: &StringRef) -> Result<(), redb::Error> {
        let bytes = postcard::to_allocvec(value).expect("StringRef should serialize");
        let write_txn = self.db.begin_write()?;
        {
            let mut table = write_txn.open_table(STRINGS)?;
            table.insert(id.get(), bytes.as_slice())?;
        }
        write_txn.commit()?;
        Ok(())
    }

    /// Reads and deserializes a single interned string, if present.
    ///
    /// # Errors
    /// Returns an error if the redb read transaction fails.
    pub fn get_string(&self, id: StringId) -> Result<Option<StringRef>, redb::Error> {
        let read_txn = self.db.begin_read()?;
        let table = read_txn.open_table(STRINGS)?;
        match table.get(id.get())? {
            Some(guard) => Ok(Some(
                postcard::from_bytes::<StringRef>(guard.value()).expect("StringRef should deserialize"),
            )),
            None => Ok(None),
        }
    }

    /// Serializes and writes a single declaration node.
    ///
    /// # Errors
    /// Returns an error if serialization or the redb transaction fails.
    pub fn put_declaration(&self, id: DeclarationId, value: &Declaration) -> Result<(), redb::Error> {
        let bytes = postcard::to_allocvec(value).expect("Declaration should serialize");
        let write_txn = self.db.begin_write()?;
        {
            let mut table = write_txn.open_table(DECLARATIONS)?;
            table.insert(id.get(), bytes.as_slice())?;
        }
        write_txn.commit()?;
        Ok(())
    }

    /// Reads and deserializes a single declaration node, if present.
    ///
    /// # Errors
    /// Returns an error if the redb read transaction fails.
    pub fn get_declaration(&self, id: DeclarationId) -> Result<Option<Declaration>, redb::Error> {
        let read_txn = self.db.begin_read()?;
        let table = read_txn.open_table(DECLARATIONS)?;
        match table.get(id.get())? {
            Some(guard) => Ok(Some(
                postcard::from_bytes::<Declaration>(guard.value()).expect("Declaration should deserialize"),
            )),
            None => Ok(None),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn string_round_trips_through_redb() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("spike.redb");
        let store = RedbStore::create(&path).expect("create store");

        let id = StringId::from("ActiveRecord::Base");
        let mut value = StringRef::new("ActiveRecord::Base".to_string());
        value.increment_ref_count(4); // ref_count: 1 + 4 = 5

        store.put_string(id, &value).expect("put");

        let loaded = store.get_string(id).expect("get").expect("present");
        assert_eq!(&**loaded, "ActiveRecord::Base");
        assert_eq!(loaded.ref_count(), 5);

        // Absent key returns None.
        let missing = StringId::from("Does::Not::Exist");
        assert!(store.get_string(missing).expect("get missing").is_none());
    }

    #[test]
    fn declaration_round_trips_through_redb() {
        use crate::model::declaration::{ClassDeclaration, Declaration, Namespace};

        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("decl.redb");
        let store = RedbStore::create(&path).expect("create store");

        // Build a class declaration with a member (exercises the nested enum + macro struct +
        // IdentityHashMap<StringId, DeclarationId> serialization, which is the real risk).
        let mut class = ClassDeclaration::new("Foo".to_string(), DeclarationId::from("Object"));
        class.add_member(StringId::from("bar"), DeclarationId::from("Foo::bar"));
        let decl = Declaration::Namespace(Namespace::Class(Box::new(class)));

        let id = DeclarationId::from("Foo");
        let before = postcard::to_allocvec(&decl).expect("serialize");
        store.put_declaration(id, &decl).expect("put");

        let loaded = store.get_declaration(id).expect("get").expect("present");
        // Round-trip fidelity: serialize -> store -> load -> serialize is byte-identical.
        let after = postcard::to_allocvec(&loaded).expect("re-serialize");
        assert_eq!(before, after);
        assert_eq!(loaded.name(), "Foo");
        assert_eq!(loaded.kind(), "Class");
    }

    #[test]
    fn build_persists_full_graph() {
        // `Graph::new()` seeds built-in data (Object, BasicObject, etc.), giving us a real,
        // non-empty graph to persist without running the indexer.
        let graph = Graph::new();
        assert!(!graph.declarations().is_empty(), "built-in data should populate the graph");

        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("graph.redb");
        let store = RedbStore::build(&path, &graph).expect("build store");

        // Every declaration in the graph must round-trip byte-identically through the built store.
        for (id, declaration) in graph.declarations() {
            let loaded = store
                .get_declaration(*id)
                .expect("get")
                .expect("declaration present in store");
            assert_eq!(
                postcard::to_allocvec(declaration).expect("serialize in-memory"),
                postcard::to_allocvec(&loaded).expect("serialize loaded"),
                "declaration {} did not round-trip",
                declaration.name(),
            );
        }
    }
}
