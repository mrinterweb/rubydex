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

use serde::de::DeserializeOwned;

use crate::model::declaration::Declaration;
use crate::model::definitions::Definition;
use crate::model::document::Document;
use crate::model::graph::{Graph, NameDependent};
use crate::model::ids::{
    ConstantReferenceId, DeclarationId, DefinitionId, MethodReferenceId, NameId, StringId, UriId,
};
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
            Some(guard) => Ok(Some(postcard::from_bytes::<V>(guard.value()).expect("node should deserialize"))),
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

    /// Reads the dependents of a single name, if present.
    ///
    /// # Errors
    /// Returns an error if the redb read transaction fails.
    pub fn get_name_dependents(&self, id: NameId) -> Result<Option<Vec<NameDependent>>, redb::Error> {
        self.get_node(NAME_DEPENDENTS, id.get())
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
        assert_roundtrip!(graph.name_dependents(), get_name_dependents);
    }
}
