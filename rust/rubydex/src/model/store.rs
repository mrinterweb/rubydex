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

use redb::{Database, MultimapTableDefinition, ReadableDatabase, TableDefinition};

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

// Secondary index for prefix search: lowercased short name -> declaration IDs (one short name can
// map to many declarations). redb keeps keys ordered, so a prefix is served by a range scan.
const SHORT_NAME_INDEX: MultimapTableDefinition<&str, u64> = MultimapTableDefinition::new("short_name_index");

/// The unqualified ("short") name of a fully qualified name: the segment after the last `::`, `#`,
/// or `.` separator. e.g. `Foo::Bar` -> `Bar`, `Foo#baz` -> `baz`.
fn short_name(fully_qualified_name: &str) -> &str {
    fully_qualified_name
        .rsplit([':', '#', '.'])
        .next()
        .unwrap_or(fully_qualified_name)
}

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

    /// Opens an existing redb store at `path` — e.g. a server reading a prebuilt gem/stdlib index
    /// without holding the graph in memory.
    ///
    /// # Errors
    /// Returns an error if the database cannot be opened.
    pub fn open(path: &Path) -> Result<Self, redb::Error> {
        Ok(Self {
            db: Database::open(path)?,
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

            // Secondary prefix-search index: short name -> declaration IDs.
            {
                let mut index = write_txn.open_multimap_table(SHORT_NAME_INDEX)?;
                for (id, declaration) in graph.declarations() {
                    let key = short_name(declaration.name()).to_lowercase();
                    index.insert(key.as_str(), id.get())?;
                }
            }

            write_txn.commit()?;
        }
        Ok(Self { db })
    }

    /// Inserts or replaces a node of type `V` in `table` at `key`. redb is mutable in place, so this
    /// is how an incremental edit updates a node — no immutable-snapshot + overlay machinery needed.
    ///
    /// # Errors
    /// Returns an error if serialization or the redb transaction fails.
    fn put_node<V: serde::Serialize>(
        &self,
        table: TableDefinition<u64, &[u8]>,
        key: u64,
        value: &V,
    ) -> Result<(), redb::Error> {
        let bytes = postcard::to_allocvec(value).expect("node should serialize");
        let write_txn = self.db.begin_write()?;
        {
            let mut table = write_txn.open_table(table)?;
            table.insert(key, bytes.as_slice())?;
        }
        write_txn.commit()?;
        Ok(())
    }

    /// Removes a node from `table` at `key`, returning whether it existed.
    ///
    /// # Errors
    /// Returns an error if the redb transaction fails.
    fn delete_node(&self, table: TableDefinition<u64, &[u8]>, key: u64) -> Result<bool, redb::Error> {
        let write_txn = self.db.begin_write()?;
        let existed = {
            let mut table = write_txn.open_table(table)?;
            table.remove(key)?.is_some()
        };
        write_txn.commit()?;
        Ok(existed)
    }

    /// Inserts or replaces a single interned string.
    ///
    /// # Errors
    /// Returns an error if serialization or the redb transaction fails.
    pub fn put_string(&self, id: StringId, value: &StringRef) -> Result<(), redb::Error> {
        self.put_node(STRINGS, id.get(), value)
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

    /// Inserts or replaces a single declaration node.
    ///
    /// # Errors
    /// Returns an error if serialization or the redb transaction fails.
    pub fn put_declaration(&self, id: DeclarationId, value: &Declaration) -> Result<(), redb::Error> {
        self.put_node(DECLARATIONS, id.get(), value)
    }

    /// Inserts or replaces a single document node.
    ///
    /// # Errors
    /// Returns an error if serialization or the redb transaction fails.
    pub fn put_document(&self, id: UriId, value: &Document) -> Result<(), redb::Error> {
        self.put_node(DOCUMENTS, id.get(), value)
    }

    /// Removes a declaration node, returning whether it existed.
    ///
    /// # Errors
    /// Returns an error if the redb transaction fails.
    pub fn delete_declaration(&self, id: DeclarationId) -> Result<bool, redb::Error> {
        self.delete_node(DECLARATIONS, id.get())
    }

    /// Removes a document node, returning whether it existed.
    ///
    /// # Errors
    /// Returns an error if the redb transaction fails.
    pub fn delete_document(&self, id: UriId) -> Result<bool, redb::Error> {
        self.delete_node(DOCUMENTS, id.get())
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

    /// Representative go-to-definition query, answered entirely from disk: resolves a fully qualified
    /// name to its first definition's location `(document uri, start byte offset)`. Walks
    /// declaration -> definition -> document, each a store read, with nothing held resident.
    ///
    /// # Errors
    /// Returns an error if any redb read transaction fails.
    pub fn definition_location(&self, fully_qualified_name: &str) -> Result<Option<(String, u32)>, redb::Error> {
        let Some(declaration) = self.get_declaration(DeclarationId::from(fully_qualified_name))? else {
            return Ok(None);
        };
        let Some(definition_id) = declaration.definitions().first().copied() else {
            return Ok(None);
        };
        let Some(definition) = self.get_definition(definition_id)? else {
            return Ok(None);
        };
        let Some(document) = self.get_document(*definition.uri_id())? else {
            return Ok(None);
        };
        Ok(Some((document.uri().to_string(), definition.offset().start())))
    }

    /// Workspace-symbol-style prefix search answered from disk: returns the fully qualified names of
    /// declarations whose short name starts with `prefix` (case-insensitive), up to `limit`. Uses the
    /// ordered `short_name_index` so only the matching key range is scanned, not the whole graph.
    ///
    /// # Errors
    /// Returns an error if any redb read transaction fails.
    pub fn search_prefix(&self, prefix: &str, limit: usize) -> Result<Vec<String>, redb::Error> {
        let lower = prefix.to_lowercase();
        let read_txn = self.db.begin_read()?;
        let index = read_txn.open_multimap_table(SHORT_NAME_INDEX)?;

        let mut results = Vec::new();
        for entry in index.range(lower.as_str()..)? {
            let (key, ids) = entry?;
            // The range is unbounded above, so stop once keys no longer share the prefix.
            if !key.value().starts_with(&lower) {
                break;
            }
            for id in ids {
                if let Some(declaration) = self.get_declaration(DeclarationId::new(id?.value()))? {
                    results.push(declaration.name().to_string());
                    if results.len() >= limit {
                        return Ok(results);
                    }
                }
            }
        }
        Ok(results)
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

    #[test]
    fn definition_location_answered_from_reopened_store() {
        use crate::indexing::{IndexerBackend, index_files};
        use crate::resolution::Resolver;

        let dir = tempfile::tempdir().expect("tempdir");
        let rb_path = dir.path().join("foo.rb");
        std::fs::write(&rb_path, "class Foo\n  def bar; end\nend\n").expect("write rb");

        // Index + resolve the file into an in-memory graph, then persist it.
        let mut graph = Graph::new();
        let _ = index_files(&mut graph, vec![rb_path.clone()], IndexerBackend::RubyIndexer);
        Resolver::new(&mut graph).resolve();

        let store_path = dir.path().join("index.redb");
        RedbStore::build(&store_path, &graph).expect("build store");
        drop(graph); // ensure the answer comes from disk, not the in-memory graph

        // Reopen as a fresh handle (simulates a server that only reads the prebuilt store).
        let store = RedbStore::open(&store_path).expect("open store");
        let (uri, _start) = store
            .definition_location("Foo")
            .expect("query")
            .expect("Foo should be located from the store");
        assert!(uri.ends_with("foo.rb"), "unexpected uri: {uri}");
    }

    #[test]
    fn prefix_search_from_store() {
        use crate::indexing::{IndexerBackend, index_files};
        use crate::resolution::Resolver;

        let dir = tempfile::tempdir().expect("tempdir");
        let rb_path = dir.path().join("shapes.rb");
        std::fs::write(&rb_path, "class Circle\nend\nclass Cylinder\nend\nclass Square\nend\n").expect("write rb");

        let mut graph = Graph::new();
        let _ = index_files(&mut graph, vec![rb_path], IndexerBackend::RubyIndexer);
        Resolver::new(&mut graph).resolve();

        let store_path = dir.path().join("index.redb");
        RedbStore::build(&store_path, &graph).expect("build store");
        drop(graph);

        let store = RedbStore::open(&store_path).expect("open store");

        assert_eq!(store.search_prefix("cy", 10).expect("search"), vec!["Cylinder".to_string()]);
        assert_eq!(store.search_prefix("ci", 10).expect("search"), vec!["Circle".to_string()]);

        // Case-insensitive prefix "c" matches both shapes (and possibly built-ins) but not Square.
        let c = store.search_prefix("C", 50).expect("search");
        assert!(c.contains(&"Circle".to_string()));
        assert!(c.contains(&"Cylinder".to_string()));
        assert!(!c.contains(&"Square".to_string()));
    }

    #[test]
    fn incremental_mutation_persists() {
        use crate::model::declaration::{ClassDeclaration, Declaration, Namespace};

        fn class(name: &str) -> ClassDeclaration {
            ClassDeclaration::new(name.to_string(), DeclarationId::from("Object"))
        }

        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("mut.redb");
        let store = RedbStore::create(&path).expect("create store");

        let foo_id = DeclarationId::from("Foo");
        let bar_id = DeclarationId::from("Bar");
        store
            .put_declaration(foo_id, &Declaration::Namespace(Namespace::Class(Box::new(class("Foo")))))
            .expect("put Foo");
        store
            .put_declaration(bar_id, &Declaration::Namespace(Namespace::Class(Box::new(class("Bar")))))
            .expect("put Bar");

        // Update Foo in place (add a member) and delete Bar.
        let mut foo = class("Foo");
        foo.add_member(StringId::from("baz"), DeclarationId::from("Foo::baz"));
        let foo_updated = Declaration::Namespace(Namespace::Class(Box::new(foo)));
        store.put_declaration(foo_id, &foo_updated).expect("update Foo");
        assert!(store.delete_declaration(bar_id).expect("delete Bar"), "Bar existed");

        // Reopen as a fresh handle: the in-place mutations must have persisted.
        drop(store);
        let store = RedbStore::open(&path).expect("reopen store");
        let loaded_foo = store.get_declaration(foo_id).expect("get Foo").expect("Foo present");
        assert_eq!(
            postcard::to_allocvec(&foo_updated).expect("serialize updated"),
            postcard::to_allocvec(&loaded_foo).expect("serialize loaded"),
        );
        assert!(store.get_declaration(bar_id).expect("get Bar").is_none(), "Bar deleted");
        assert!(!store.delete_declaration(bar_id).expect("re-delete Bar"), "Bar already gone");
    }
}
