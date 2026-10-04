//! Disk-persisted, low-resident-memory backing store for the graph.
//!
//! The graph's node maps are keyed by `Id<T>` (a `NonZeroU64` content hash). That maps directly onto
//! an embedded key-value store: `u64 -> serialized_node_bytes`. This module persists the full
//! resolved graph to a redb database and serves reads back through the `Graph`'s layered accessors
//! (`Graph::with_store` / `Graph::attach_store`), keeping the bulk index off the heap.

use std::path::Path;

use redb::{Database, ReadableDatabase, TableDefinition};

use serde::de::DeserializeOwned;

use crate::model::declaration::Declaration;
use crate::model::definitions::Definition;
use crate::model::document::Document;
use crate::model::graph::{Graph, NameDependent};
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

/// Declaration FQN names for search: `declaration_id -> FQN string`. A compact projection so
/// name-based search scans short strings instead of deserializing full declaration nodes.
const SEARCH_NAMES: TableDefinition<u64, &[u8]> = TableDefinition::new("search_names");

/// Document URIs for require-path resolution: `uri_id -> URI string`. A compact projection so
/// require completion can enumerate file paths without deserializing full document nodes.
const DOCUMENT_URIS: TableDefinition<u64, &[u8]> = TableDefinition::new("document_uris");

/// Failure modes of the on-disk store.
///
/// A corrupt store is recoverable: the caller counts the failure and falls back to the in-memory
/// index. It must never panic — a panic inside `extern "C"` cannot unwind, so it aborts the host
/// process instead of surfacing an error to ruby-lsp.
#[derive(Debug)]
pub enum StoreError {
    /// The redb database could not be opened, or a transaction failed.
    Open(redb::Error),
    /// A node could not be encoded for writing. A programming error rather than a data condition:
    /// every persisted node type is plain owned data.
    Encode(postcard::Error),
    /// A node's bytes could not be decoded as its node type: the store is corrupt or was written
    /// by an incompatible layout.
    Corrupt {
        /// Logical table the node was read from.
        table: &'static str,
        /// Id of the node whose bytes failed to decode.
        id: u64,
        /// The underlying decode failure.
        source: postcard::Error,
    },
}

impl std::fmt::Display for StoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StoreError::Open(error) => write!(f, "store unavailable: {error}"),
            StoreError::Encode(error) => write!(f, "failed to encode node: {error}"),
            StoreError::Corrupt { table, id, source } => {
                write!(f, "corrupt node in table `{table}` (id {id}): {source}")
            }
        }
    }
}

// redb's umbrella `Error` is built from these per-operation error types; the write path surfaces
// them directly, so funnel each into `StoreError::Open`.
macro_rules! store_error_from_redb {
    ($($error:ty),+ $(,)?) => {
        $(impl From<$error> for StoreError {
            fn from(error: $error) -> Self {
                StoreError::Open(error.into())
            }
        })+
    };
}
store_error_from_redb!(
    redb::StorageError,
    redb::TableError,
    redb::DatabaseError,
    redb::SavepointError,
    redb::TransactionError,
    redb::CommitError,
    redb::SetDurabilityError,
    redb::CompactionError,
);

impl std::error::Error for StoreError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            StoreError::Open(error) => Some(error),
            StoreError::Encode(source) | StoreError::Corrupt { source, .. } => Some(source),
        }
    }
}

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
    pub fn build(path: &Path, graph: &Graph) -> Result<Self, StoreError> {
        // `Database::create` opens an existing file rather than truncating it, which would merge
        // stale nodes from a previous build into the new store. Remove any existing file first so
        // the build always replaces.
        if let Err(err) = std::fs::remove_file(path)
            && err.kind() != std::io::ErrorKind::NotFound
        {
            return Err(StoreError::Open(redb::Error::Io(err)));
        }
        let db = Database::create(path)?;
        {
            let write_txn = db.begin_write()?;
            // Serialize each `(id, node)` pair into the node's table. The `{{ }}` scoping drops each
            // table guard before the next table is opened within the same transaction.
            macro_rules! write_map {
                ($table:expr, $map:expr) => {{
                    let mut table = write_txn.open_table($table)?;
                    for (id, value) in $map {
                        let bytes = postcard::to_allocvec(value).map_err(StoreError::Encode)?;
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
            {
                let mut table = write_txn.open_table(SEARCH_NAMES)?;
                for (id, declaration) in graph.declarations() {
                    table.insert(id.get(), declaration.name().as_bytes())?;
                }
            }
            {
                let mut table = write_txn.open_table(DOCUMENT_URIS)?;
                for (id, document) in graph.documents() {
                    table.insert(id.get(), document.uri().as_bytes())?;
                }
            }

            write_txn.commit()?;
        }
        Ok(Self { db })
    }

    /// Creates (or opens) a redb database at `path`.
    ///
    /// # Errors
    /// Returns an error if the database file cannot be created or opened.
    pub fn create(path: &Path) -> Result<Self, redb::Error> {
        Ok(Self {
            db: Database::create(path)?,
        })
    }

    /// Inserts or replaces a node of type `V` in `table` at `key`. redb is mutable in place, so this
    /// is how an incremental edit updates a node — no immutable-snapshot + overlay machinery needed.
    ///
    /// # Errors
    /// Returns an error if serialization or the redb transaction fails.
    pub(crate) fn put_node<V: serde::Serialize>(
        &self,
        table: TableDefinition<u64, &[u8]>,
        key: u64,
        value: &V,
    ) -> Result<(), StoreError> {
        let bytes = postcard::to_allocvec(value).map_err(StoreError::Encode)?;
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
    pub(crate) fn delete_node(&self, table: TableDefinition<u64, &[u8]>, key: u64) -> Result<bool, StoreError> {
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
    pub fn put_string(&self, id: StringId, value: &StringRef) -> Result<(), StoreError> {
        self.put_node(STRINGS, id.get(), value)
    }

    /// Inserts or replaces a single declaration node.
    ///
    /// # Errors
    /// Returns an error if serialization or the redb transaction fails.
    pub fn put_declaration(&self, id: DeclarationId, value: &Declaration) -> Result<(), StoreError> {
        self.put_node(DECLARATIONS, id.get(), value)
    }

    /// Inserts or replaces a single document node.
    ///
    /// # Errors
    /// Returns an error if serialization or the redb transaction fails.
    pub fn put_document(&self, id: UriId, value: &Document) -> Result<(), StoreError> {
        self.put_node(DOCUMENTS, id.get(), value)
    }

    /// Removes a declaration node, returning whether it existed.
    ///
    /// # Errors
    /// Returns an error if the redb transaction fails.
    pub fn delete_declaration(&self, id: DeclarationId) -> Result<bool, StoreError> {
        self.delete_node(DECLARATIONS, id.get())
    }

    /// Removes a document node, returning whether it existed.
    ///
    /// # Errors
    /// Returns an error if the redb transaction fails.
    pub fn delete_document(&self, id: UriId) -> Result<bool, StoreError> {
        self.delete_node(DOCUMENTS, id.get())
    }

    /// Reads and deserializes a node of type `V` from `table` by its `u64` key, if present.
    ///
    /// # Errors
    /// Returns an error if the redb read transaction fails.
    fn get_node<V: DeserializeOwned>(
        &self,
        table: TableDefinition<u64, &[u8]>,
        name: &'static str,
        key: u64,
    ) -> Result<Option<V>, StoreError> {
        let read_txn = self.db.begin_read()?;
        let table = read_txn.open_table(table)?;
        match table.get(key)? {
            Some(guard) => postcard::from_bytes::<V>(guard.value())
                .map(Some)
                .map_err(|source| StoreError::Corrupt {
                    table: name,
                    id: key,
                    source,
                }),
            None => Ok(None),
        }
    }

    /// Reads a single interned string, if present.
    ///
    /// # Errors
    /// Returns an error if the redb read transaction fails.
    pub fn get_string(&self, id: StringId) -> Result<Option<StringRef>, StoreError> {
        self.get_node(STRINGS, "strings", id.get())
    }

    /// Reads a single declaration node, if present.
    ///
    /// # Errors
    /// Returns an error if the redb read transaction fails.
    pub fn get_declaration(&self, id: DeclarationId) -> Result<Option<Declaration>, StoreError> {
        self.get_node(DECLARATIONS, "declarations", id.get())
    }

    /// Reads all `(declaration_id, FQN name)` pairs for name-based search.
    ///
    /// # Errors
    /// Returns an error if the redb read transaction or table iteration fails.
    pub fn search_names(&self) -> Result<Vec<(DeclarationId, String)>, redb::Error> {
        let read_txn = self.db.begin_read()?;
        let table = read_txn.open_table(SEARCH_NAMES)?;
        table
            .range(..u64::MAX)?
            .map(|entry| -> Result<(DeclarationId, String), redb::Error> {
                let (id, name) = entry?;
                Ok((
                    DeclarationId::new(id.value()),
                    String::from_utf8_lossy(name.value()).into_owned(),
                ))
            })
            .collect()
    }

    /// Reads all `(uri_id, URI)` pairs for require-path resolution.
    ///
    /// # Errors
    /// Returns an error if the redb read transaction or table iteration fails.
    pub fn document_uris(&self) -> Result<Vec<(UriId, String)>, redb::Error> {
        let read_txn = self.db.begin_read()?;
        let table = read_txn.open_table(DOCUMENT_URIS)?;
        table
            .range(..u64::MAX)?
            .map(|entry| -> Result<(UriId, String), redb::Error> {
                let (id, uri) = entry?;
                Ok((
                    UriId::new(id.value()),
                    String::from_utf8_lossy(uri.value()).into_owned(),
                ))
            })
            .collect()
    }

    /// Streams the `SEARCH_NAMES` table, returning the ids whose FQN passes `predicate`.
    /// Filtering during the scan avoids materializing the full name table (hundreds of MB
    /// on large corpora) for a single search.
    ///
    /// # Errors
    /// Returns an error if the redb read transaction or table iteration fails.
    pub fn declaration_ids_matching(
        &self,
        predicate: &dyn Fn(&DeclarationId, &str) -> bool,
    ) -> Result<Vec<DeclarationId>, redb::Error> {
        let read_txn = self.db.begin_read()?;
        let table = read_txn.open_table(SEARCH_NAMES)?;
        let ids = table
            .range(..u64::MAX)?
            .filter_map(|entry| {
                let (id, name) = entry.ok()?;
                let id = DeclarationId::new(id.value());
                let name = String::from_utf8_lossy(name.value());
                predicate(&id, &name).then_some(id)
            })
            .collect::<Vec<_>>();
        Ok(ids)
    }

    /// Reads all definition node ids for enumeration (keys only, no deserialization).
    ///
    /// # Errors
    /// Returns an error if the redb read transaction or table iteration fails.
    pub fn definition_ids(&self) -> Result<Vec<DefinitionId>, redb::Error> {
        let read_txn = self.db.begin_read()?;
        let table = read_txn.open_table(DEFINITIONS)?;
        table
            .range(..u64::MAX)?
            .map(|entry| -> Result<DefinitionId, redb::Error> { Ok(DefinitionId::new(entry?.0.value())) })
            .collect()
    }

    /// Reads a single definition node, if present.
    ///
    /// # Errors
    /// Returns an error if the redb read transaction fails.
    pub fn get_definition(&self, id: DefinitionId) -> Result<Option<Definition>, StoreError> {
        self.get_node(DEFINITIONS, "definitions", id.get())
    }

    /// Reads a single name node, if present.
    ///
    /// # Errors
    /// Returns an error if the redb read transaction fails.
    pub fn get_name(&self, id: NameId) -> Result<Option<NameRef>, StoreError> {
        self.get_node(NAMES, "names", id.get())
    }

    /// Reads a single constant reference node, if present.
    ///
    /// # Errors
    /// Returns an error if the redb read transaction fails.
    pub fn get_constant_reference(&self, id: ConstantReferenceId) -> Result<Option<ConstantReference>, StoreError> {
        self.get_node(CONSTANT_REFERENCES, "constant_references", id.get())
    }

    /// Reads a single method reference node, if present.
    ///
    /// # Errors
    /// Returns an error if the redb read transaction fails.
    pub fn get_method_reference(&self, id: MethodReferenceId) -> Result<Option<MethodRef>, StoreError> {
        self.get_node(METHOD_REFERENCES, "method_references", id.get())
    }

    /// Reads a single document node, if present.
    ///
    /// # Errors
    /// Returns an error if the redb read transaction fails.
    pub fn get_document(&self, id: UriId) -> Result<Option<Document>, StoreError> {
        self.get_node(DOCUMENTS, "documents", id.get())
    }

    /// Reads the dependents of a single name, if present.
    ///
    /// # Errors
    /// Returns an error if the redb read transaction fails.
    pub fn get_name_dependents(&self, id: NameId) -> Result<Option<Vec<NameDependent>>, StoreError> {
        self.get_node(NAME_DEPENDENTS, "name_dependents", id.get())
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
        assert_roundtrip!(graph.name_dependents(), get_name_dependents);
    }

    #[test]
    fn untracking_overlay_names_does_not_tombstone_store_nodes() {
        use crate::model::name::ParentScope;

        // StringId, NameId and DeclarationId all derive from name hashes, so their raw u64
        // spaces overlap: StringId::from("Object") == DeclarationId::from("Object"). In a
        // store-backed graph, refcount cleanup of a transient overlay name must not tombstone
        // ids the store still holds — the store's copy is the snapshot of record and has no
        // overlay refcount. (Tombstoning here made the store's Object declaration unresolvable
        // after the first `Graph#resolve_constant("Object")` FFI call, aborting the process.)
        let base = Graph::new();
        let s = StringId::from("Object");
        let d = DeclarationId::from("Object");
        assert_eq!(s.get(), d.get(), "test relies on the shared raw id space");
        assert!(
            base.declarations().contains_key(&d),
            "built-in seed should include the Object declaration"
        );

        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("graph.redb");
        let store = RedbStore::build(&path, &base).expect("build store");
        let mut graph = Graph::with_store(store);

        // One FFI resolve_constant cycle: intern the string, register the name, untrack it.
        let sid = graph.intern_string("Object".to_string());
        let name_id = graph.add_name(sid, ParentScope::None, None);
        graph.untrack_name(name_id);

        assert!(
            graph.declaration(d).is_some(),
            "store-backed declaration must survive overlay refcount cleanup"
        );
        // Repeat: the transient name is recreated on every query and must stay harmless.
        for _ in 0..3 {
            let sid = graph.intern_string("Object".to_string());
            let name_id = graph.add_name(sid, ParentScope::None, None);
            graph.untrack_name(name_id);
        }
        assert!(
            graph.declaration(d).is_some(),
            "store-backed declaration must stay resolvable"
        );
    }

    #[test]
    fn attach_store_then_live_edit_then_resolve_does_not_crash() {
        use crate::indexing::{IndexerBackend, LanguageId, index_files, index_source};
        use crate::resolution::Resolver;

        let dir = tempfile::tempdir().expect("tempdir");
        let rb_path = dir.path().join("foo.rb");
        std::fs::write(&rb_path, "class Foo\n  def bar; end\nend\n").expect("write");

        let mut graph = Graph::new();
        let _ = index_files(&mut graph, vec![rb_path.clone()], IndexerBackend::RubyIndexer);
        Resolver::new(&mut graph).resolve();
        let store_path = dir.path().join("index.redb");
        RedbStore::build(&store_path, &graph).expect("build store");
        drop(graph);

        // Mirrors the FFI attach path: a fresh graph (built-ins in memory) whose maps are
        // cleared and backed by the store, then a live edit + resolve (what Graph#resolve does).
        let mut graph = Graph::new();
        graph.attach_store(RedbStore::open(&store_path).expect("open store"));

        let uri = url::Url::from_file_path(&rb_path).unwrap().to_string();
        index_source(
            &mut graph,
            uri.into(),
            "class Foo\n  def baz; end\nend\n",
            &LanguageId::Ruby,
        );
        Resolver::new(&mut graph).resolve();

        let foo = graph
            .declaration(DeclarationId::from("Foo"))
            .expect("Foo after live edit");
        let ns = foo.as_namespace().expect("namespace");
        assert!(
            ns.member(&StringId::from("baz()")).is_some(),
            "edited member baz resolves"
        );
        assert!(
            ns.member(&StringId::from("bar()")).is_none(),
            "removed member bar is gone"
        );
    }

    #[test]
    #[ignore = "benchmark: needs RUBYDEX_BENCH_CORPUS"]
    fn bench_build_write_vs_drop() {
        use crate::indexing::{IndexerBackend, index_files};
        use crate::listing::collect_file_paths;
        use crate::resolution::Resolver;

        let corpus = std::env::var("RUBYDEX_BENCH_CORPUS").expect("set RUBYDEX_BENCH_CORPUS");
        let corpus = std::path::PathBuf::from(corpus);
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("bench.redb");

        let t0 = std::time::Instant::now();
        let mut graph = Graph::new();
        let (files, _) = collect_file_paths(vec![corpus.to_string_lossy().into_owned()], &graph.excluded_patterns());
        let _ = index_files(&mut graph, files, IndexerBackend::RubyIndexer);
        let indexed = t0.elapsed();
        let t1 = std::time::Instant::now();
        Resolver::new(&mut graph).resolve();
        let resolved = t1.elapsed();

        let t2 = std::time::Instant::now();
        let store = RedbStore::build(&path, &graph).expect("build store");
        let written = t2.elapsed();
        let size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
        let t3 = std::time::Instant::now();
        drop(store);
        let dropped = t3.elapsed();

        println!("BENCH index={indexed:?} resolve={resolved:?} write={written:?} drop={dropped:?} size={size}");
    }

    #[test]
    fn declaration_ids_matching_filters_during_scan() {
        let graph = Graph::new();
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("graph.redb");
        let store = RedbStore::build(&path, &graph).expect("build store");

        let all = store.search_names().expect("search_names");
        assert!(!all.is_empty(), "built-in declarations should populate SEARCH_NAMES");

        // Subset predicate (first character of the first FQN guarantees a non-empty match):
        // streaming results must equal filtering the materialized table.
        let first_char = all[0].1.chars().next().expect("FQN non-empty");
        let subset = store
            .declaration_ids_matching(&|_id, name| name.starts_with(first_char))
            .expect("streaming subset");
        let expected: Vec<DeclarationId> = all
            .iter()
            .filter(|(_, name)| name.starts_with(first_char))
            .map(|(id, _)| *id)
            .collect();
        assert_eq!(subset, expected);

        // Always-true returns every id; never-true returns none.
        let everything = store
            .declaration_ids_matching(&|_id, _name| true)
            .expect("streaming all");
        assert_eq!(everything.len(), all.len());
        let nothing = store
            .declaration_ids_matching(&|_id, _name| false)
            .expect("streaming none");
        assert!(nothing.is_empty());
    }

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
        assert_eq!(&*loaded, "ActiveRecord::Base");
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
        let class = ClassDeclaration::new("Foo".to_string(), DeclarationId::from("Object"));
        let mut namespace = Namespace::Class(Box::new(class));
        namespace.add_member(StringId::from("bar"), DeclarationId::from("Foo::bar"));
        let decl = Declaration::Namespace(namespace);

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
            .put_declaration(
                foo_id,
                &Declaration::Namespace(Namespace::Class(Box::new(class("Foo")))),
            )
            .expect("put Foo");
        store
            .put_declaration(
                bar_id,
                &Declaration::Namespace(Namespace::Class(Box::new(class("Bar")))),
            )
            .expect("put Bar");

        // Update Foo in place (add a member) and delete Bar.
        let foo = class("Foo");
        let mut namespace = Namespace::Class(Box::new(foo));
        namespace.add_member(StringId::from("baz"), DeclarationId::from("Foo::baz"));
        let foo_updated = Declaration::Namespace(namespace);
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
        assert!(
            !store.delete_declaration(bar_id).expect("re-delete Bar"),
            "Bar already gone"
        );
    }

    #[test]
    fn materialize_declaration_pulls_store_node_into_overlay() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("base.redb");

        // Build a store with built-in declarations, then drop the in-memory graph.
        let base = Graph::new();
        let (sample_id, sample_name) = base
            .declarations()
            .iter()
            .next()
            .map(|(id, declaration)| (*id, declaration.name().to_string()))
            .expect("built-in declarations exist");
        RedbStore::build(&path, &base).expect("build store");
        drop(base);

        // A store-backed graph has empty in-memory maps.
        let mut graph = Graph::with_store(RedbStore::open(&path).expect("open store"));
        assert!(graph.declarations().get(&sample_id).is_none(), "memory layer is empty");

        // Materialize pulls the node into memory so it can be mutated.
        graph.materialize_declaration(sample_id);
        let declaration = graph.declarations().get(&sample_id).expect("now in memory");
        assert_eq!(declaration.name(), sample_name);
    }

    #[test]
    fn add_member_materializes_store_backed_owner() {
        use crate::model::ids::StringId;

        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("base.redb");

        let base = Graph::new();
        // Find a namespace declaration (class/module) from the built-ins to use as the owner.
        let owner_id = base
            .declarations()
            .iter()
            .find(|(_, decl)| decl.as_namespace().is_some())
            .map(|(id, _)| *id)
            .expect("built-in namespace exists");
        RedbStore::build(&path, &base).expect("build store");
        drop(base);

        let mut graph = Graph::with_store(RedbStore::open(&path).expect("open store"));
        let member_id = DeclarationId::from("TestMember");
        let member_str = StringId::from("TestMember");

        // add_member on a store-backed owner must materialize it first, not silently skip.
        graph.add_member(&owner_id, member_id, member_str);

        // The owner is now in the overlay with the new member attached.
        let owner = graph.declarations().get(&owner_id).expect("owner materialized");
        let members = owner.as_namespace().expect("owner is a namespace").members();
        assert!(members.values().any(|&id| id == member_id), "member was added");
    }

    #[test]
    fn consume_document_changes_replaces_store_backed_document() {
        use crate::indexing::{IndexerBackend, index_files};
        use crate::resolution::Resolver;

        let dir = tempfile::tempdir().expect("tempdir");
        let rb_path = dir.path().join("foo.rb");
        std::fs::write(&rb_path, "class Foo\n  def bar; end\nend\n").expect("write rb v1");

        // Index + resolve + persist.
        let mut graph = Graph::new();
        let _ = index_files(&mut graph, vec![rb_path.clone()], IndexerBackend::RubyIndexer);
        Resolver::new(&mut graph).resolve();
        let store_path = dir.path().join("index.redb");
        RedbStore::build(&store_path, &graph).expect("build store");
        drop(graph);

        // Reopen store-backed. The in-memory documents map is empty.
        let mut graph = Graph::with_store(RedbStore::open(&store_path).expect("open store"));

        // Re-index the same file with a changed body. consume_document_changes must find the old
        // Document (from the store), invalidate it, and apply the new one — not panic.
        let new_source = "class Foo\n  def baz; end\nend\n";
        let uri = url::Url::from_file_path(&rb_path).unwrap().to_string();
        crate::indexing::index_source(
            &mut graph,
            uri.clone().into(),
            new_source,
            &crate::indexing::LanguageId::Ruby,
        );

        // The new method definition (baz) must be visible in the overlay. Method definitions store
        // their name as a str_id (unresolved at this stage), so check via the string table.
        let has_baz = graph.definitions().values().any(|d| {
            matches!(d, crate::model::definitions::Definition::Method(m)
                if graph.strings().get(m.str_id()).is_some_and(|s| s.as_str().contains("baz")))
        });
        assert!(has_baz, "new definition 'baz' should be in the overlay after re-index");
    }

    #[test]
    fn resolve_applies_live_edits_on_store_backed_graph() {
        use crate::indexing::{IndexerBackend, LanguageId, index_files, index_source};
        use crate::resolution::Resolver;

        let dir = tempfile::tempdir().expect("tempdir");
        let rb_path = dir.path().join("foo.rb");
        std::fs::write(&rb_path, "class Foo\n  def bar; end\nend\n").expect("write rb v1");

        // Index + resolve + persist a graph containing `class Foo; def bar; end`.
        let mut graph = Graph::new();
        let _ = index_files(&mut graph, vec![rb_path.clone()], IndexerBackend::RubyIndexer);
        Resolver::new(&mut graph).resolve();
        let store_path = dir.path().join("index.redb");
        RedbStore::build(&store_path, &graph).expect("build store");
        drop(graph);

        // Reopen store-backed: `Foo` reads from disk with `bar` as a member.
        let mut graph = Graph::with_store(RedbStore::open(&store_path).expect("open store"));
        assert!(graph.declarations().is_empty(), "memory layer is empty");
        let foo_id = DeclarationId::from("Foo");
        let removed_member = StringId::from("bar()");
        let added_member = StringId::from("baz()");
        {
            let foo = graph.declaration(foo_id).expect("Foo from store");
            let namespace = foo.as_namespace().expect("Foo is a namespace");
            assert!(namespace.member(&removed_member).is_some(), "store has bar");
        }

        // Live edit: replace bar with baz, then resolve. The resolver must see the overlay
        // document and rewrite the store-backed declaration through the layered accessors.
        let uri = url::Url::from_file_path(&rb_path).unwrap().to_string();
        index_source(
            &mut graph,
            uri.clone().into(),
            "class Foo\n  def baz; end\nend\n",
            &LanguageId::Ruby,
        );
        Resolver::new(&mut graph).resolve();

        let foo = graph.declaration(foo_id).expect("Foo after edit");
        let namespace = foo.as_namespace().expect("Foo is a namespace");
        assert!(namespace.member(&added_member).is_some(), "edited member baz resolves");
        assert!(
            namespace.member(&removed_member).is_none(),
            "removed member bar is gone"
        );
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

    #[test]
    fn expression_completion_on_store_backed_graph() {
        use crate::indexing::{IndexerBackend, index_files};
        use crate::query::{CompletionCandidate, CompletionContext, CompletionReceiver, completion_candidates};
        use crate::resolution::Resolver;

        let dir = tempfile::tempdir().expect("tempdir");
        let rb_path = dir.path().join("animal.rb");
        std::fs::write(&rb_path, "class Animal\n  def speak; end\nend\n").expect("write rb");

        let mut graph = Graph::new();
        let _ = index_files(&mut graph, vec![rb_path], IndexerBackend::RubyIndexer);
        Resolver::new(&mut graph).resolve();

        // Grab the `Animal` class definition's NameId while the in-memory graph is alive; a NameId
        // encodes its nesting scope, so it cannot be derived from the string alone.
        let animal_name_id: NameId = graph
            .definitions()
            .values()
            .find_map(|d| match d {
                Definition::Class(c) => Some(*c.name_id()),
                _ => None,
            })
            .expect("Animal class definition");

        let store_path = dir.path().join("index.redb");
        RedbStore::build(&store_path, &graph).expect("build store");
        drop(graph); // the answer must come from disk, not in-memory maps

        // A fresh store-backed graph has empty in-memory maps, so expression completion (the
        // lexical-scope path) can only work by reading through the layered accessors.
        let graph = Graph::with_store(RedbStore::open(&store_path).expect("open store"));
        assert!(graph.declarations().is_empty(), "memory layer is empty");

        let receiver = CompletionReceiver::Expression {
            self_decl_id: Some(DeclarationId::from("Animal")),
            nesting_name_id: animal_name_id,
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
            "expected `speak` from store-backed expression completion, got {names:?}"
        );
    }

    #[test]
    fn follow_method_alias_on_store_backed_graph() {
        use crate::indexing::{IndexerBackend, index_files};
        use crate::query::follow_method_alias;
        use crate::resolution::Resolver;

        let dir = tempfile::tempdir().expect("tempdir");
        let rb_path = dir.path().join("animal.rb");
        std::fs::write(&rb_path, "class Animal\n  def speak; end\n  alias talk speak\nend\n").expect("write rb");

        let mut graph = Graph::new();
        let _ = index_files(&mut graph, vec![rb_path], IndexerBackend::RubyIndexer);
        Resolver::new(&mut graph).resolve();

        // Grab the alias's DefinitionId while the in-memory graph is alive.
        let alias_def_id = graph
            .definitions()
            .iter()
            .find_map(|(id, def)| matches!(def, Definition::MethodAlias(_)).then_some(*id))
            .expect("alias definition");

        let store_path = dir.path().join("index.redb");
        RedbStore::build(&store_path, &graph).expect("build store");
        drop(graph); // the alias and its target live only in the store now

        let graph = Graph::with_store(RedbStore::open(&store_path).expect("open store"));
        assert!(graph.declarations().is_empty(), "memory layer is empty");

        // Aliasing a store-only method used to panic in the in-memory-only member lookup; it must
        // resolve to the real method's declaration through the layered accessors.
        assert_eq!(
            follow_method_alias(&graph, alias_def_id),
            Ok(DeclarationId::from("Animal#speak()"))
        );
    }

    #[test]
    fn declaration_search_on_store_backed_graph() {
        use crate::indexing::{IndexerBackend, index_files};
        use crate::query::{MatchMode, declaration_search};
        use crate::resolution::Resolver;

        let dir = tempfile::tempdir().expect("tempdir");
        let rb_path = dir.path().join("animal.rb");
        std::fs::write(&rb_path, "class Animal\n  def speak; end\nend\n").expect("write rb");

        let mut graph = Graph::new();
        let _ = index_files(&mut graph, vec![rb_path], IndexerBackend::RubyIndexer);
        Resolver::new(&mut graph).resolve();

        let store_path = dir.path().join("index.redb");
        RedbStore::build(&store_path, &graph).expect("build store");
        drop(graph); // the declarations live only in the store now

        let graph = Graph::with_store(RedbStore::open(&store_path).expect("open store"));
        assert!(graph.declarations().is_empty(), "memory layer is empty");

        let animal_id = DeclarationId::from("Animal");

        // Search used to scan only the in-memory overlay and silently returned nothing in disk mode.
        let found = declaration_search(&graph, &["Animal"], &MatchMode::Exact);
        assert!(
            found.contains(&animal_id),
            "exact search found no store declarations: {found:?}"
        );

        let found = declaration_search(&graph, &["anml"], &MatchMode::Fuzzy);
        assert!(
            found.contains(&animal_id),
            "fuzzy search found no store declarations: {found:?}"
        );

        // An empty query returns all declarations, including store-backed ones.
        let all = declaration_search(&graph, &[""], &MatchMode::Exact);
        assert!(
            all.len() > graph.declarations().len(),
            "empty query returned only in-memory declarations: {}",
            all.len()
        );
    }

    #[test]
    fn live_edit_tombstones_removed_nodes() {
        use crate::indexing::ruby_indexer::RubyIndexer;
        use crate::indexing::{IndexerBackend, index_files};
        use crate::model::definitions::Definition;
        use crate::resolution::Resolver;

        let dir = tempfile::tempdir().expect("tempdir");
        let a_path = dir.path().join("a.rb");
        std::fs::write(&a_path, "class A\n  def go\n    B + baz\n  end\nend\n").expect("write a");
        let b_path = dir.path().join("b.rb");
        std::fs::write(&b_path, "class B; end\n").expect("write b");

        let mut graph = Graph::new();
        let _ = index_files(
            &mut graph,
            vec![a_path.clone(), b_path.clone()],
            IndexerBackend::RubyIndexer,
        );
        Resolver::new(&mut graph).resolve();

        // Capture a.rb's node IDs while they are in memory.
        let a_uri = url::Url::from_file_path(&a_path).expect("url").to_string();
        let a_uri_id = UriId::from(a_uri.as_str());
        // The method definition, not the class namespace one — the latter survives the edit
        // (same content, same ID) and is legitimately re-inserted.
        let old_def_id = graph
            .definitions()
            .iter()
            .find(|(_, def)| def.uri_id().get() == a_uri_id.get() && matches!(def, Definition::Method(_)))
            .map(|(id, _)| *id)
            .expect("a method definition in a.rb");
        let old_mref_id = graph
            .method_references()
            .iter()
            .find(|(_, reference)| reference.uri_id().get() == a_uri_id.get())
            .map(|(id, _)| *id)
            .expect("a method reference in a.rb");
        let old_const_ref_id = graph
            .constant_references()
            .iter()
            .find(|(_, reference)| reference.uri_id().get() == a_uri_id.get())
            .map(|(id, _)| *id)
            .expect("a constant reference in a.rb");

        let store_path = dir.path().join("index.redb");
        RedbStore::build(&store_path, &graph).expect("build store");
        drop(graph); // the nodes live only in the store now

        let mut graph = Graph::with_store(RedbStore::open(&store_path).expect("open store"));

        // Sanity: the pre-edit nodes are served by the store.
        assert!(
            graph.definition(old_def_id).is_some(),
            "store should serve the pre-edit definition"
        );
        assert!(
            graph.method_reference(old_mref_id).is_some(),
            "store should serve the pre-edit method reference"
        );

        // Edit a.rb away the method, so its old nodes are removed from the overlay.
        std::fs::write(&a_path, "class A\nend\n").expect("rewrite a");
        let mut indexer = RubyIndexer::new(a_uri.into(), "class A\nend\n");
        indexer.index();
        graph.consume_document_changes(indexer.local_graph());
        Resolver::new(&mut graph).resolve();

        // The layered getters must not resurrect the store's stale copies of the removed nodes.
        assert!(
            graph.definition(old_def_id).is_none(),
            "removed definition was resurrected from the store"
        );
        assert!(
            graph.method_reference(old_mref_id).is_none(),
            "removed method reference was resurrected from the store"
        );
        assert!(
            graph.constant_reference(old_const_ref_id).is_none(),
            "removed constant reference was resurrected from the store"
        );
    }

    #[test]
    fn require_resolution_on_store_backed_graph() {
        use crate::indexing::{IndexerBackend, index_files};
        use crate::query::{require_paths, resolve_require_path};
        use crate::resolution::Resolver;

        let dir = tempfile::tempdir().expect("tempdir");
        let lib = dir.path().join("lib");
        let file = lib.join("foo").join("bar.rb");
        std::fs::create_dir_all(file.parent().expect("parent")).expect("mkdir");
        std::fs::write(&file, "class Bar; end\n").expect("write rb");

        let mut graph = Graph::new();
        let _ = index_files(&mut graph, vec![file.clone()], IndexerBackend::RubyIndexer);
        Resolver::new(&mut graph).resolve();

        let store_path = dir.path().join("index.redb");
        RedbStore::build(&store_path, &graph).expect("build store");
        drop(graph); // the documents live only in the store now

        let graph = Graph::with_store(RedbStore::open(&store_path).expect("open store"));
        assert!(graph.documents().is_empty(), "memory layer is empty");

        let uri_id = UriId::from(url::Url::from_file_path(&file).expect("url").as_str());

        // In-memory-only lookups: go-to-definition on require and require/require_relative
        // completion were both dead in disk mode.
        assert_eq!(
            resolve_require_path(&graph, "foo/bar", std::slice::from_ref(&lib)),
            Some(uri_id)
        );
        let paths = require_paths(&graph, &[lib]);
        assert!(
            paths.contains(&"foo/bar".to_string()),
            "require_paths found no store documents: {paths:?}"
        );
    }
}
