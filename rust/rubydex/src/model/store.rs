//! Disk-persisted, low-resident-memory backing store for the graph.
//!
//! Design docs and benchmarks: `docs/disk-persisted-index/`.
//!
//! The graph's node maps are keyed by `Id<T>` (a `NonZeroU64` content hash). That maps directly onto
//! an embedded key-value store: `u64 -> serialized_node_bytes`. This module persists the full
//! resolved graph to a redb database and serves reads back through the `Graph`'s layered accessors
//! (`Graph::with_store` / `Graph::attach_store`), keeping the bulk index off the heap.

use std::path::Path;

use redb::{Builder, Database, ReadOnlyDatabase, ReadableDatabase, TableDefinition};

use serde::de::DeserializeOwned;

use crate::diagnostic::Diagnostic;
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

/// Declaration FQN names for search, as `shard index -> front-coded (declaration_id, FQN) block`.
/// A compact projection so name-based search scans short strings instead of deserializing full
/// declaration nodes. Sharding it gives the parallel scan `SEARCH_SHARD_COUNT` independent units
/// and front-coding removes the shared prefix of neighbouring FQNs (`ActiveRecord::Associations::
/// CollectionAssociation` and `...CollectionProxy` share most of their bytes).
///
/// Written sorted by FQN, so contiguous shard ranges decode to the lexicographic order.
const SEARCH_NAMES: TableDefinition<u32, &[u8]> = TableDefinition::new("search_names");

/// How many contiguous blocks `SEARCH_NAMES` is split into. A fixed count (rather than one shard
/// per thread) keeps the store layout independent of the machine that built it.
const SEARCH_SHARD_COUNT: usize = 32;

/// Document URIs for require-path resolution: key `0` -> front-coded `(uri_id, URI)` block. A compact
/// projection so require completion can enumerate file paths without deserializing full document
/// nodes.
const DOCUMENT_URIS: TableDefinition<u32, &[u8]> = TableDefinition::new("document_uris");
const STORE_METADATA: TableDefinition<u8, u32> = TableDefinition::new("store_metadata");

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
    /// The store was written with a different node layout.
    IncompatibleFormat(u32),
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
    /// A packed block or shard failed structural validation: a truncated header, an out-of-range
    /// offset, an invalid UTF-8 sequence, or a bad length.
    CorruptBlock {
        /// Logical table the block was read from.
        table: &'static str,
        /// Shard or bucket key of the block.
        key: u32,
        /// What was wrong with the block.
        reason: &'static str,
    },
}

impl std::fmt::Display for StoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StoreError::Open(error) => write!(f, "store unavailable: {error}"),
            StoreError::Encode(error) => write!(f, "failed to encode node: {error}"),
            StoreError::IncompatibleFormat(version) => write!(f, "unsupported store format version {version}"),
            StoreError::Corrupt { table, id, source } => {
                write!(f, "corrupt node in table `{table}` (id {id}): {source}")
            }
            StoreError::CorruptBlock { table, key, reason } => {
                write!(f, "corrupt block in table `{table}` (key {key}): {reason}")
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
            StoreError::IncompatibleFormat(_) | StoreError::CorruptBlock { .. } => None,
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Front-coded blocks
//
// A block packs a lexicographically sorted run of `(text, id)` pairs, storing each entry as its
// shared prefix length with the previous entry, then only the differing suffix. Two FQNs in the
// same namespace share almost everything, and so do two URIs in the same directory, which is why
// the shared prefix is worth its two varints.
//
// Wire format:
//   [entry_count: u32 LE]
//   entry_count times: [shared_prefix_len: LEB128 u32][suffix_len: LEB128 u32][suffix][id: u64 LE]
// ---------------------------------------------------------------------------------------------

#[allow(clippy::cast_possible_truncation)] // masked to 7 bits, so the value always fits a byte
fn write_varint_u32(out: &mut Vec<u8>, mut value: u32) {
    while value >= 0x80 {
        out.push((value & 0x7f) as u8 | 0x80);
        value >>= 7;
    }
    out.push(value as u8);
}

fn read_varint_u32(bytes: &[u8], pos: &mut usize) -> Option<u32> {
    let mut result: u32 = 0;
    let mut shift: u32 = 0;
    loop {
        let byte = *bytes.get(*pos)?;
        *pos += 1;
        // Reject a varint that would overflow `u32` (or run past 5 bytes) instead of wrapping.
        if shift >= 32 || (shift == 28 && byte > 0x0f) {
            return None;
        }
        result |= u32::from(byte & 0x7f) << shift;
        if byte & 0x80 == 0 {
            return Some(result);
        }
        shift += 7;
    }
}

/// Encodes `entries` into `out`. The caller must have sorted them by `text` ascending: front-coding
/// is only meaningful neighbour-to-neighbour, and the readers rely on the order.
fn encode_front_coded(entries: &[(&[u8], u64)], out: &mut Vec<u8>) -> Result<(), StoreError> {
    out.clear();
    let count = u32::try_from(entries.len()).map_err(|_| StoreError::CorruptBlock {
        table: "front_coded",
        key: 0,
        reason: "block entry count exceeds u32::MAX",
    })?;
    out.extend_from_slice(&count.to_le_bytes());
    let mut previous: &[u8] = &[];
    let too_long = |reason| StoreError::CorruptBlock {
        table: "front_coded",
        key: 0,
        reason,
    };
    for &(text, id) in entries {
        let shared = previous
            .iter()
            .zip(text)
            .take_while(|(left, right)| left == right)
            .count();
        let shared = u32::try_from(shared).map_err(|_| too_long("shared prefix exceeds u32::MAX"))?;
        let suffix = text.get(shared as usize..).unwrap_or_default();
        let suffix_len = u32::try_from(suffix.len()).map_err(|_| too_long("suffix exceeds u32::MAX"))?;
        write_varint_u32(out, shared);
        write_varint_u32(out, suffix_len);
        out.extend_from_slice(suffix);
        out.extend_from_slice(&id.to_le_bytes());
        previous = text;
    }
    Ok(())
}

/// Calls `visit(id, text)` for every entry of a front-coded block.
///
/// Never panics on a corrupt block: every read is bounds-checked and every assembled string is
/// validated as UTF-8. A bad block returns [`StoreError::CorruptBlock`], which the caller records
/// and degrades on rather than aborting the host process.
fn decode_front_coded(
    table: &'static str,
    key: u32,
    bytes: &[u8],
    mut visit: impl FnMut(u64, &str),
) -> Result<(), StoreError> {
    let error = |reason| StoreError::CorruptBlock { table, key, reason };
    let count_bytes: [u8; 4] = bytes
        .get(0..4)
        .ok_or_else(|| error("truncated header"))?
        .try_into()
        .unwrap();
    let count = u32::from_le_bytes(count_bytes);
    let mut pos = 4usize;
    // Reused across entries: one allocation per block rather than one per string.
    let mut assembled = Vec::<u8>::with_capacity(256);

    for _ in 0..count {
        let shared = read_varint_u32(bytes, &mut pos).ok_or_else(|| error("truncated shared prefix length"))? as usize;
        let suffix_len = read_varint_u32(bytes, &mut pos).ok_or_else(|| error("truncated suffix length"))? as usize;
        if shared > assembled.len() {
            return Err(error("shared prefix exceeds the previous entry"));
        }
        let end = pos
            .checked_add(suffix_len)
            .ok_or_else(|| error("suffix length overflow"))?;
        let suffix = bytes.get(pos..end).ok_or_else(|| error("truncated suffix"))?;
        pos = end;
        let id_end = pos.checked_add(8).ok_or_else(|| error("id offset overflow"))?;
        let id_bytes: [u8; 8] = bytes
            .get(pos..id_end)
            .ok_or_else(|| error("truncated id"))?
            .try_into()
            .unwrap();
        pos = id_end;

        // Truncating a `Vec<u8>` at a multi-byte character boundary is fine: the suffix completes
        // the sequence before `from_utf8` looks at it. `String::truncate` would panic here.
        assembled.truncate(shared);
        assembled.extend_from_slice(suffix);
        let text = std::str::from_utf8(&assembled).map_err(|_| error("invalid UTF-8 in entry"))?;
        visit(u64::from_le_bytes(id_bytes), text);
    }
    Ok(())
}

/// Encodes `entries` into up to [`SEARCH_SHARD_COUNT`] front-coded blocks, one per shard, encoding
/// on all available cores. Shards stay in ascending FQN order.
fn encode_search_shards(entries: &[(&[u8], u64)]) -> Result<Vec<Vec<u8>>, StoreError> {
    if entries.is_empty() {
        return Ok(Vec::new());
    }
    let chunk_size = entries.len().div_ceil(SEARCH_SHARD_COUNT).max(1);
    let chunks: Vec<&[(&[u8], u64)]> = entries.chunks(chunk_size).collect();
    let encode = |chunk: &[(&[u8], u64)]| -> Result<Vec<u8>, StoreError> {
        let mut out = Vec::with_capacity(chunk.len() * 24);
        encode_front_coded(chunk, &mut out)?;
        Ok(out)
    };
    if chunks.len() < 2 {
        return Ok(vec![encode(chunks[0])?]);
    }
    std::thread::scope(|scope| {
        let handles: Vec<_> = chunks
            .into_iter()
            .map(|chunk| scope.spawn(move || encode(chunk)))
            .collect();
        handles
            .into_iter()
            .map(|handle| handle.join().expect("front-coding worker panicked"))
            .collect()
    })
}

/// Layout version of the persisted store. Bump this when a node's serialized shape, a table's key
/// scheme, or the set of tables changes — anything that makes an older store unreadable or
/// semantically stale. The gem version deliberately does NOT participate: a release that does not
/// touch the layout would otherwise force every user to re-index.
pub const STORE_FORMAT_VERSION: u32 = 3;

/// Page cache the builder runs with. redb defaults to 1 GiB, which a build fills with pages it
/// never reads back. Measured on the Ruby 4.0 gems tree (28k files, 1.1 GB store): 1 GiB = 7.1 s /
/// 1609 MB peak, 128 MiB = 8.1 s / 1245 MB, 8 MiB = 10.8 s / 1179 MB. 128 MiB is the knee: a
/// seventh of the peak-RSS cut for a seventh of the slowdown.
const BUILD_CACHE_SIZE: usize = 128 * 1024 * 1024;

/// Which redb handle backs a store. The writable handle exists only for the builder child, which
/// writes and exits; readers open read-only, so multiple sessions (two editor windows on one
/// worktree) can share one store. redb's read-write open takes an exclusive lock and answers
/// `DatabaseAlreadyOpen` to a second process, which used to leave that session on the in-memory
/// index.
enum StoreDb {
    Writable(Database),
    ReadOnly(ReadOnlyDatabase),
}

/// A redb-backed node store.
pub struct RedbStore {
    db: StoreDb,
}

impl std::fmt::Debug for RedbStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("RedbStore { .. }")
    }
}

impl RedbStore {
    /// Begins a read transaction on whichever handle backs the store.
    fn begin_read(&self) -> Result<redb::ReadTransaction, redb::TransactionError> {
        match &self.db {
            StoreDb::Writable(db) => db.begin_read(),
            StoreDb::ReadOnly(db) => db.begin_read(),
        }
    }

    /// Begins a write transaction. Only the builder's writable handle can take one; a read-only
    /// store answering with an error is what keeps a write attempt from aborting the host.
    #[cfg(test)]
    fn begin_write(&self) -> Result<redb::WriteTransaction, redb::TransactionError> {
        match &self.db {
            StoreDb::Writable(db) => db.begin_write(),
            StoreDb::ReadOnly(_) => Err(redb::TransactionError::Storage(redb::StorageError::Io(
                std::io::Error::other("the store is opened read-only"),
            ))),
        }
    }

    /// Opens an existing redb store at `path` read-only — e.g. a server reading a prebuilt gem/stdlib
    /// index without holding the graph in memory.
    ///
    /// # Errors
    /// Returns an error if the database cannot be opened.
    pub fn open(path: &Path) -> Result<Self, StoreError> {
        // Cap redb's in-heap page cache so a long-lived server stays lean; the OS still caches the
        // file, so a cache miss is a RAM hit (not disk) and costs ~0.15 ms. 8 MiB is the measured
        // sweet spot (~26 MB less RSS than 32 MiB, negligible latency).
        //
        // Opening reads redb's internal allocator state, and its B-tree code asserts (`unreachable!`)
        // on a damaged page rather than returning an error. That panic must not escape: this runs
        // under `extern "C"` at the Ruby boundary, where an unwind turns into a process abort.
        let opened = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            Builder::new().set_cache_size(8 * 1024 * 1024).open_read_only(path)
        }));
        match opened {
            Ok(Ok(db)) => {
                let version = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| -> Result<_, StoreError> {
                    let txn = db.begin_read()?;
                    let table = txn.open_table(STORE_METADATA)?;
                    Ok(table.get(0)?.map(|entry| entry.value()))
                }))
                .map_err(|_| StoreError::Open(redb::Error::Io(std::io::Error::other("store metadata is damaged"))))??;
                match version {
                    Some(STORE_FORMAT_VERSION) => Ok(Self {
                        db: StoreDb::ReadOnly(db),
                    }),
                    Some(version) => Err(StoreError::IncompatibleFormat(version)),
                    None => Err(StoreError::Open(redb::Error::Io(std::io::Error::other(
                        "store format version missing",
                    )))),
                }
            }
            Ok(Err(error)) => Err(StoreError::Open(error.into())),
            Err(_) => Err(StoreError::Open(redb::Error::Io(std::io::Error::other(
                "redb panicked while opening the store; the file is damaged or not a rubydex store",
            )))),
        }
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
        let db = Builder::new().set_cache_size(BUILD_CACHE_SIZE).create(path)?;
        {
            let write_txn = db.begin_write()?;
            write_txn.open_table(STORE_METADATA)?.insert(0, STORE_FORMAT_VERSION)?;
            // Serialize each `(id, node)` pair into the node's table. The `{{ }}` scoping drops each
            // table guard before the next table is opened within the same transaction.
            macro_rules! write_map {
                ($table:expr, $map:expr) => {{
                    // Ascending key order: the maps iterate in hash order, and redb splits a B-tree
                    // leaf wherever the next key lands, so random inserts thrash the page cache.
                    let mut entries: Vec<_> = $map.iter().map(|(id, value)| (id.get(), value)).collect();
                    entries.sort_unstable_by_key(|(key, _)| *key);
                    let mut table = write_txn.open_table($table)?;
                    for (key, value) in entries {
                        let bytes = postcard::to_allocvec(value).map_err(StoreError::Encode)?;
                        table.insert(key, bytes.as_slice())?;
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
                // Sorted by FQN, not by id: front-coding pays off between neighbours, and the
                // search scan reads the block in this order anyway.
                let mut entries: Vec<(&[u8], u64)> = graph
                    .declarations()
                    .iter()
                    .map(|(id, declaration)| (declaration.name().as_bytes(), id.get()))
                    .collect();
                entries.sort_unstable_by_key(|(name, _)| *name);
                let shards = encode_search_shards(&entries)?;
                let mut table = write_txn.open_table(SEARCH_NAMES)?;
                for (shard, blob) in shards.iter().enumerate() {
                    let shard = u32::try_from(shard).map_err(|_| StoreError::CorruptBlock {
                        table: "search_names",
                        key: 0,
                        reason: "shard count exceeds u32::MAX",
                    })?;
                    table.insert(shard, blob.as_slice())?;
                }
            }
            {
                let mut entries: Vec<(&[u8], u64)> = graph
                    .documents()
                    .iter()
                    .map(|(id, document)| (document.uri().as_bytes(), id.get()))
                    .collect();
                entries.sort_unstable_by_key(|(uri, _)| *uri);
                let mut blob = Vec::with_capacity(entries.len() * 32);
                encode_front_coded(&entries, &mut blob)?;
                write_txn.open_table(DOCUMENT_URIS)?.insert(0, blob.as_slice())?;
            }

            write_txn.commit()?;
        }
        Ok(Self {
            db: StoreDb::Writable(db),
        })
    }

    /// Creates (or opens) a redb database at `path`.
    ///
    /// Test-only: production writes the store once via [`Self::build`] and never mutates it (live
    /// edits land in the graph's overlay). If overlay writes are ever persisted back, bring this
    /// back together with a batched write API — one write transaction per batch, not per node.
    ///
    /// # Errors
    /// Returns an error if the database file cannot be created or opened.
    #[cfg(test)]
    pub fn create(path: &Path) -> Result<Self, redb::Error> {
        let db = Database::create(path)?;
        let txn = db.begin_write()?;
        txn.open_table(STORE_METADATA)?.insert(0, STORE_FORMAT_VERSION)?;
        txn.commit()?;
        Ok(Self {
            db: StoreDb::Writable(db),
        })
    }

    /// Inserts or replaces a node of type `V` in `table` at `key`. redb is mutable in place, so this
    /// is how an incremental edit updates a node — no immutable-snapshot + overlay machinery needed.
    ///
    /// # Errors
    /// Returns an error if serialization or the redb transaction fails.
    #[cfg(test)]
    pub(crate) fn put_node<V: serde::Serialize>(
        &self,
        table: TableDefinition<u64, &[u8]>,
        key: u64,
        value: &V,
    ) -> Result<(), StoreError> {
        let bytes = postcard::to_allocvec(value).map_err(StoreError::Encode)?;
        let write_txn = self.begin_write()?;
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
    #[cfg(test)]
    pub(crate) fn delete_node(&self, table: TableDefinition<u64, &[u8]>, key: u64) -> Result<bool, StoreError> {
        let write_txn = self.begin_write()?;
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
    #[cfg(test)]
    pub fn put_string(&self, id: StringId, value: &StringRef) -> Result<(), StoreError> {
        self.put_node(STRINGS, id.get(), value)
    }

    /// Inserts or replaces a single declaration node.
    ///
    /// # Errors
    /// Returns an error if serialization or the redb transaction fails.
    #[cfg(test)]
    pub fn put_declaration(&self, id: DeclarationId, value: &Declaration) -> Result<(), StoreError> {
        self.put_node(DECLARATIONS, id.get(), value)
    }

    /// Removes a declaration node, returning whether it existed.
    ///
    /// # Errors
    /// Returns an error if the redb transaction fails.
    #[cfg(test)]
    pub fn delete_declaration(&self, id: DeclarationId) -> Result<bool, StoreError> {
        self.delete_node(DECLARATIONS, id.get())
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
        let read_txn = self.begin_read()?;
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

    /// Streams every declaration node through `visit` in a single read transaction.
    ///
    /// Whole-table consumers (label scans, dead-code analysis, the FFI iterators) fetch their
    /// nodes one id at a time through [`Self::get_declaration`], which costs one read transaction
    /// per node.
    ///
    /// # Errors
    /// Returns an error if the read transaction or table iteration fails, or if a node's bytes do
    /// not decode.
    pub fn for_each_declaration(&self, visit: &mut dyn FnMut(DeclarationId, Declaration)) -> Result<(), StoreError> {
        let read_txn = self.begin_read()?;
        let table = read_txn.open_table(DECLARATIONS)?;
        for entry in table.range(..=u64::MAX)? {
            let (key, value) = entry?;
            let id = key.value();
            let declaration =
                postcard::from_bytes::<Declaration>(value.value()).map_err(|source| StoreError::Corrupt {
                    table: "declarations",
                    id,
                    source,
                })?;
            visit(DeclarationId::new(id), declaration);
        }
        Ok(())
    }

    /// Streams every constant reference together with its name, joining `NAMES` in the same read
    /// transaction so a whole-table consumer does not pay one transaction per reference.
    ///
    /// # Errors
    /// Returns an error if the read transaction or table iteration fails, or if a node's bytes do
    /// not decode.
    pub fn for_each_constant_reference_with_name(
        &self,
        visit: &mut dyn FnMut(ConstantReferenceId, ConstantReference, Option<NameRef>),
    ) -> Result<(), StoreError> {
        let read_txn = self.begin_read()?;
        let references = read_txn.open_table(CONSTANT_REFERENCES)?;
        let names = read_txn.open_table(NAMES)?;
        for entry in references.range(..=u64::MAX)? {
            let (key, value) = entry?;
            let id = key.value();
            let reference =
                postcard::from_bytes::<ConstantReference>(value.value()).map_err(|source| StoreError::Corrupt {
                    table: "constant_references",
                    id,
                    source,
                })?;
            let name_id = reference.name_id().get();
            let name = match names.get(name_id)? {
                Some(guard) => {
                    Some(
                        postcard::from_bytes::<NameRef>(guard.value()).map_err(|source| StoreError::Corrupt {
                            table: "names",
                            id: name_id,
                            source,
                        })?,
                    )
                }
                None => None,
            };
            visit(ConstantReferenceId::new(id), reference, name);
        }
        Ok(())
    }

    /// Reads all `(declaration_id, FQN name)` pairs for name-based search.
    ///
    /// Test-only: production search streams the table ([`Self::declaration_ids_matching`]) and node
    /// enumeration takes keys ([`Self::declaration_ids`]), so nothing should materialize every name.
    /// The test keeps it as the reference the cheaper scans are checked against.
    ///
    /// # Errors
    /// Returns an error if the redb read transaction or table iteration fails.
    #[cfg(test)]
    pub fn search_names(&self) -> Result<Vec<(DeclarationId, String)>, StoreError> {
        let read_txn = self.begin_read()?;
        let table = read_txn.open_table(SEARCH_NAMES)?;
        let mut names = Vec::new();
        for entry in table.range(0..=u32::MAX)? {
            let (shard, blob) = entry?;
            decode_front_coded("search_names", shard.value(), blob.value(), |id, name| {
                names.push((DeclarationId::new(id), name.to_owned()));
            })?;
        }
        Ok(names)
    }

    /// Reads all `(uri_id, URI)` pairs for require-path resolution.
    ///
    /// # Errors
    /// Returns an error if the redb read transaction or table iteration fails.
    pub fn document_uris(&self) -> Result<Vec<(UriId, String)>, StoreError> {
        let read_txn = self.begin_read()?;
        let table = read_txn.open_table(DOCUMENT_URIS)?;
        let Some(blob) = table.get(0)? else {
            return Ok(Vec::new());
        };
        let mut uris = Vec::new();
        decode_front_coded("document_uris", 0, blob.value(), |id, uri| {
            uris.push((UriId::new(id), uri.to_owned()));
        })?;
        Ok(uris)
    }

    /// Streams `range` of the `SEARCH_NAMES` table, returning the ids whose FQN passes `predicate`.
    /// Filtering during the scan avoids materializing the full name table (hundreds of MB
    /// on large corpora) for a single search.
    ///
    /// # Errors
    /// Returns an error if the redb read transaction or table iteration fails.
    fn matching_ids(
        &self,
        shards: std::ops::Range<u32>,
        predicate: &dyn Fn(&DeclarationId, &str) -> bool,
    ) -> Result<Vec<DeclarationId>, StoreError> {
        let read_txn = self.begin_read()?;
        let table = read_txn.open_table(SEARCH_NAMES)?;
        let mut ids = Vec::new();
        for shard in shards {
            let Some(blob) = table.get(shard)? else {
                continue;
            };
            decode_front_coded("search_names", shard, blob.value(), |raw_id, name| {
                let id = DeclarationId::new(raw_id);
                if predicate(&id, name) {
                    ids.push(id);
                }
            })?;
        }
        Ok(ids)
    }

    /// Streams the whole `SEARCH_NAMES` table — see [`Self::matching_ids`].
    ///
    /// Test-only: production search always shards the keyspace
    /// ([`Self::declaration_ids_matching_parallel`]); the single pass stays as the reference the
    /// parallel scan is checked against.
    ///
    /// # Errors
    /// Returns an error if the redb read transaction or table iteration fails.
    #[cfg(test)]
    pub fn declaration_ids_matching(
        &self,
        predicate: &dyn Fn(&DeclarationId, &str) -> bool,
    ) -> Result<Vec<DeclarationId>, StoreError> {
        self.matching_ids(0..u32::try_from(SEARCH_SHARD_COUNT).unwrap_or(u32::MAX), predicate)
    }

    /// Streams `SEARCH_NAMES` across `available_parallelism()` key ranges, each on its
    /// own read transaction and thread, and merges the results.
    ///
    /// Search walks every row of `SEARCH_NAMES` whatever the query is (`Exact` is a substring test,
    /// `Fuzzy` a subsequence score), so the floor is one full-table pass; splitting the keyspace is the
    /// only lever that shortens it without changing match semantics.
    ///
    /// # Errors
    /// Returns an error if any redb read transaction or table iteration fails.
    ///
    /// # Panics
    ///
    /// Panics if one of the search threads panics
    pub fn declaration_ids_matching_parallel(
        &self,
        predicate: &(dyn Fn(&DeclarationId, &str) -> bool + Sync),
    ) -> Result<Vec<DeclarationId>, StoreError> {
        let shard_count = u32::try_from(SEARCH_SHARD_COUNT).unwrap_or(u32::MAX);
        let workers = std::thread::available_parallelism().map_or(4, std::num::NonZero::get);
        // Contiguous shard ranges, so joining the workers in order yields each shard's ascending
        // FQN order back with no re-sort and no duplicate.
        let mut ranges = Vec::with_capacity(workers);
        let mut start = 0_u32;
        for worker in 0..workers {
            let remaining_workers = u32::try_from(workers - worker).unwrap_or(1);
            let take = (shard_count - start).div_ceil(remaining_workers);
            ranges.push(start..start + take);
            start += take;
        }
        let results = std::thread::scope(|scope| {
            let handles: Vec<_> = ranges
                .into_iter()
                .filter(|range| !range.is_empty())
                .map(|range| scope.spawn(move || self.matching_ids(range, predicate)))
                .collect::<Vec<_>>();
            handles
                .into_iter()
                .map(|handle| handle.join().expect("search shard panicked"))
                .collect::<Result<Vec<_>, _>>()
        });
        // Sorted by id, not by the FQN order the shards are written in: the caller fetches each
        // match's declaration node from `DECLARATIONS`, which is keyed by id, so id order turns
        // ~48k random point reads into a sequential walk (measured: 77ms -> 59ms on the gems
        // corpus). The sort itself is ~1ms.
        let mut ids = results?.concat();
        ids.sort_unstable_by_key(DeclarationId::get);
        Ok(ids)
    }

    /// Every key of `table`, as ids. Enumeration needs the ids only and throws the values away, so
    /// deserializing them was one allocation per node per query.
    ///
    /// # Errors
    /// Returns an error if the redb read transaction or table iteration fails.
    fn node_ids<T>(&self, table: TableDefinition<u64, &[u8]>, id: fn(u64) -> T) -> Result<Vec<T>, redb::Error> {
        let read_txn = self.begin_read()?;
        let table = read_txn.open_table(table)?;
        table
            .range(..=u64::MAX)?
            .map(|entry| -> Result<T, redb::Error> { Ok(id(entry?.0.value())) })
            .collect()
    }

    /// Reads all definition node ids for enumeration.
    ///
    /// # Errors
    /// Returns an error if the redb read transaction or table iteration fails.
    pub fn definition_ids(&self) -> Result<Vec<DefinitionId>, redb::Error> {
        self.node_ids(DEFINITIONS, DefinitionId::new)
    }

    /// Reads all declaration node ids for enumeration.
    ///
    /// # Errors
    /// Returns an error if the redb read transaction or table iteration fails.
    pub fn declaration_ids(&self) -> Result<Vec<DeclarationId>, redb::Error> {
        self.node_ids(DECLARATIONS, DeclarationId::new)
    }

    /// Reads all document node ids for enumeration.
    ///
    /// # Errors
    /// Returns an error if the redb read transaction or table iteration fails.
    pub fn document_ids(&self) -> Result<Vec<UriId>, redb::Error> {
        self.node_ids(DOCUMENTS, UriId::new)
    }

    /// Reads all constant reference ids for enumeration.
    ///
    /// # Errors
    /// Returns an error if the redb read transaction or table iteration fails.
    pub fn constant_reference_ids(&self) -> Result<Vec<ConstantReferenceId>, redb::Error> {
        self.node_ids(CONSTANT_REFERENCES, ConstantReferenceId::new)
    }

    /// Reads all method reference ids for enumeration.
    ///
    /// # Errors
    /// Returns an error if the redb read transaction or table iteration fails.
    pub fn method_reference_ids(&self) -> Result<Vec<MethodReferenceId>, redb::Error> {
        self.node_ids(METHOD_REFERENCES, MethodReferenceId::new)
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

    /// Streams the `DOCUMENTS` table, returning every `(uri_id, diagnostic)` the store recorded,
    /// so the caller can apply its overlay/tombstone shadowing per document.
    ///
    /// # Errors
    /// Returns an error if the redb read transaction, table iteration, or a node decode fails.
    pub fn document_diagnostics(&self) -> Result<Vec<(UriId, Diagnostic)>, StoreError> {
        let read_txn = self.begin_read()?;
        let table = read_txn.open_table(DOCUMENTS)?;
        let mut diagnostics = Vec::new();
        for entry in table.range(..=u64::MAX)? {
            let (id, bytes) = entry?;
            let uri_id = UriId::new(id.value());
            let document: Document = postcard::from_bytes(bytes.value()).map_err(|source| StoreError::Corrupt {
                table: "documents",
                id: id.value(),
                source,
            })?;
            diagnostics.extend(
                document
                    .diagnostics()
                    .iter()
                    .map(|diagnostic| (uri_id, diagnostic.clone())),
            );
        }
        Ok(diagnostics)
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
    fn open_rejects_a_store_with_an_incompatible_layout() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("graph.redb");
        drop(RedbStore::build(&path, &Graph::new()).expect("build store"));

        let db = Database::builder().open(&path).expect("open for version change");
        let txn = db.begin_write().expect("write transaction");
        {
            let mut metadata = txn
                .open_table(TableDefinition::<u8, u32>::new("store_metadata"))
                .expect("metadata table");
            metadata.insert(0, STORE_FORMAT_VERSION + 1).expect("wrong version");
        }
        txn.commit().expect("commit version change");
        drop(db);

        assert!(RedbStore::open(&path).is_err(), "an incompatible store must not open");
    }

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
    fn removing_a_referenced_declaration_does_not_crash() {
        use crate::indexing::{IndexerBackend, LanguageId, index_files, index_source};
        use crate::resolution::Resolver;

        let dir = tempfile::tempdir().expect("tempdir");
        let base = dir.path().join("base.rb");
        let a = dir.path().join("a.rb");
        let b = dir.path().join("b.rb");
        std::fs::write(&base, "class Base\n  def call; end\nend\n").expect("write");
        std::fs::write(
            &a,
            "module M\n  class A < Base\n    def self.target; B; end\n  end\nend\n",
        )
        .expect("write");
        std::fs::write(&b, "module M\n  class B < Base\n    def call; end\n  end\nend\n").expect("write");

        let mut graph = Graph::new();
        let _ = index_files(
            &mut graph,
            vec![base, a.clone(), b.clone()],
            IndexerBackend::RubyIndexer,
        );
        Resolver::new(&mut graph).resolve();
        let store_path = dir.path().join("index.redb");
        RedbStore::build(&store_path, &graph).expect("build store");
        drop(graph);

        let mut graph = Graph::new();
        graph.attach_store(RedbStore::open(&store_path).expect("open store"));

        // The referrer is edited first, then the file that defines the declaration it resolved
        // against loses that declaration. `Graph#apply_changes` does the same for a changed set.
        index_source(
            &mut graph,
            url::Url::from_file_path(&a).unwrap().to_string().into(),
            "module M\n  class A < Base\n    def self.target; B; end\n  end\nend\n# edited\n",
            &LanguageId::Ruby,
        );
        index_source(
            &mut graph,
            url::Url::from_file_path(&b).unwrap().to_string().into(),
            "# gone\n",
            &LanguageId::Ruby,
        );
        Resolver::new(&mut graph).resolve();

        assert!(
            graph.declaration(DeclarationId::from("M::A")).is_some(),
            "referrer survives the soak"
        );
    }

    #[test]
    fn parallel_scan_matches_the_serial_scan() {
        use std::collections::BTreeSet;

        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("parallel.redb");
        RedbStore::build(&path, &Graph::new()).expect("build store");
        let store = RedbStore::open(&path).expect("open store");

        for query in ["Object", "Kernel", "String", "no-such-thing"] {
            let serial = store
                .declaration_ids_matching(&|_, name| name.contains(query))
                .expect("serial scan");
            let parallel = store
                .declaration_ids_matching_parallel(&|_, name| name.contains(query))
                .expect("parallel scan");
            // The parallel scan sorts ids for sequential point reads; the serial one keeps the
            // shard order. Same set, no duplicates.
            let serial: BTreeSet<_> = serial.into_iter().map(|id| id.get()).collect();
            let unique: BTreeSet<_> = parallel.iter().map(DeclarationId::get).collect();
            assert_eq!(serial, unique, "scans disagree for query {query:?}");
            assert_eq!(
                unique.len(),
                parallel.len(),
                "a shard boundary listed a node twice (query {query:?})"
            );
        }
    }

    #[test]
    fn for_each_store_declaration_skips_overlay_and_tombstones() {
        use crate::indexing::{IndexerBackend, index_files};
        use crate::resolution::Resolver;

        let dir = tempfile::tempdir().expect("tempdir");
        let foo = dir.path().join("foo.rb");
        let bar = dir.path().join("bar.rb");
        std::fs::write(&foo, "class Foo\nend\n").expect("write foo");
        std::fs::write(&bar, "class Bar\nend\n").expect("write bar");

        let mut base = Graph::new();
        let _ = index_files(&mut base, vec![foo, bar.clone()], IndexerBackend::RubyIndexer);
        Resolver::new(&mut base).resolve();
        let store_path = dir.path().join("index.redb");
        drop(RedbStore::build(&store_path, &base).expect("build store"));
        drop(base);

        let mut graph = Graph::with_store(RedbStore::open(&store_path).expect("open store"));
        // Overlay: Foo was pulled into memory by a live edit.
        graph.materialize_declaration(DeclarationId::from("Foo"));
        // Tombstone: Bar's document was deleted by a live edit.
        let bar_uri = url::Url::from_file_path(&bar).expect("file url").to_string();
        assert!(
            graph.delete_document(&bar_uri).is_some(),
            "Bar's document is in the store"
        );

        let mut seen = Vec::new();
        graph.for_each_store_declaration(|id, _declaration| seen.push(id));

        assert!(
            !seen.contains(&DeclarationId::from("Foo")),
            "an overlay declaration must not be streamed from the store"
        );
        assert!(
            !seen.contains(&DeclarationId::from("Bar")),
            "a tombstoned declaration must not be resurrected by the scan"
        );
        assert!(
            !seen.is_empty(),
            "the scan still visits the store's remaining declarations"
        );
    }

    #[test]
    fn for_each_store_declaration_records_a_corrupt_node_without_panicking() {
        let graph = Graph::new();
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("graph.redb");
        let store = RedbStore::build(&path, &graph).expect("build store");
        let id = store
            .declaration_ids()
            .expect("declaration ids")
            .into_iter()
            .next()
            .expect("the built-in seed has declarations");
        drop(store);

        let db = Database::builder().open(&path).expect("open for corruption");
        let txn = db.begin_write().expect("write transaction");
        {
            let mut table = txn.open_table(DECLARATIONS).expect("declarations table");
            table
                .insert(id.get(), &[0xff_u8, 0xff, 0xff][..])
                .expect("corrupt node");
        }
        txn.commit().expect("commit corruption");
        drop(db);

        let graph = Graph::with_store(RedbStore::open(&path).expect("open store"));
        graph.for_each_store_declaration(|_, _| {});

        assert!(
            graph.store_error_count() > 0,
            "a corrupt node must be recorded, not panicked on"
        );
    }

    #[test]
    fn for_each_store_constant_reference_with_name_resolves_through_the_name() {
        use crate::indexing::{IndexerBackend, index_files};
        use crate::resolution::Resolver;

        let dir = tempfile::tempdir().expect("tempdir");
        let rb = dir.path().join("foo.rb");
        std::fs::write(&rb, "class Foo\nend\nFoo\n").expect("write");

        let mut base = Graph::new();
        let _ = index_files(&mut base, vec![rb.clone()], IndexerBackend::RubyIndexer);
        Resolver::new(&mut base).resolve();
        let store_path = dir.path().join("index.redb");
        drop(RedbStore::build(&store_path, &base).expect("build store"));
        drop(base);

        let foo = DeclarationId::from("Foo");
        let mut graph = Graph::with_store(RedbStore::open(&store_path).expect("open store"));

        let mut resolves_foo = false;
        graph.for_each_store_constant_reference_with_name(|id, cref, name_ref| {
            assert_eq!(id, cref.id(), "the streamed id and the node's own id must agree");
            if name_ref.is_some_and(
                |name_ref| matches!(name_ref, NameRef::Resolved(resolved) if **resolved.declaration_id() == foo.get()),
            ) {
                resolves_foo = true;
            }
        });
        assert!(
            resolves_foo,
            "the store's name table must be joined into the streamed reference"
        );

        // A live edit's deletion tombstones the file's references; the scan must not resurrect them.
        let uri = url::Url::from_file_path(&rb).expect("file url").to_string();
        let uri_id = UriId::from(&uri);
        let mut before = 0;
        graph.for_each_store_constant_reference_with_name(|_, cref, _| {
            if cref.uri_id() == uri_id {
                before += 1;
            }
        });
        assert!(before > 0, "the store holds the file's references");
        assert!(graph.delete_document(&uri).is_some(), "the document is in the store");

        graph.for_each_store_constant_reference_with_name(|id, cref, _| {
            assert_ne!(
                cref.uri_id(),
                uri_id,
                "a tombstoned reference must not be streamed back ({id:?})"
            );
        });
    }

    #[test]
    fn for_each_store_constant_reference_with_name_prefers_overlay_name() {
        use crate::indexing::{IndexerBackend, LanguageId, index_files, index_source};
        use crate::resolution::Resolver;

        let dir = tempfile::tempdir().expect("tempdir");
        let ref_file = dir.path().join("ref.rb");
        std::fs::write(&ref_file, "LaterDefined\n").expect("write");

        let mut base = Graph::new();
        let _ = index_files(&mut base, vec![ref_file.clone()], IndexerBackend::RubyIndexer);
        Resolver::new(&mut base).resolve();
        let store_path = dir.path().join("index.redb");
        drop(RedbStore::build(&store_path, &base).expect("build store"));
        drop(base);

        let ref_uri_id = UriId::from(&url::Url::from_file_path(&ref_file).expect("url").to_string());
        let mut graph = Graph::with_store(RedbStore::open(&store_path).expect("open store"));

        // Before the live edit, the store holds an unresolved name for `LaterDefined`.
        let mut saw_unresolved = false;
        graph.for_each_store_constant_reference_with_name(|_, cref, name_ref| {
            if cref.uri_id() == ref_uri_id {
                assert!(
                    matches!(name_ref, Some(NameRef::Unresolved(_))),
                    "store copy should start unresolved, got {name_ref:?}"
                );
                saw_unresolved = true;
            }
        });
        assert!(saw_unresolved, "the store holds the reference from ref.rb");

        // A live edit adds `class LaterDefined; end` in another file: the reference stays in the
        // store, while its `NameRef` resolves in the overlay `graph.names`.
        let def_uri = url::Url::from_file_path(dir.path().join("def.rb"))
            .expect("url")
            .to_string();
        index_source(
            &mut graph,
            def_uri.into(),
            "class LaterDefined; end\n",
            &LanguageId::Ruby,
        );
        Resolver::new(&mut graph).resolve();

        let target = DeclarationId::from("LaterDefined");
        let mut saw_resolved = false;
        graph.for_each_store_constant_reference_with_name(|_, cref, name_ref| {
            if cref.uri_id() == ref_uri_id {
                assert!(
                    matches!(
                        name_ref,
                        Some(NameRef::Resolved(resolved)) if **resolved.declaration_id() == target.get()
                    ),
                    "overlay resolution must win over the stale store name, got {name_ref:?}"
                );
                saw_resolved = true;
            }
        });
        assert!(
            saw_resolved,
            "the store reference must still be streamed with its overlay name"
        );
    }

    #[test]
    fn front_coded_blocks_round_trip_multibyte_prefixes() {
        // These pairs share leading bytes *inside* a multi-byte character (`Caf` + `0xC3`),
        // which is exactly where a `String::truncate`-based decoder would panic.
        let entries: Vec<(&[u8], u64)> = vec![
            ("Café".as_bytes(), 1),
            ("Cafè".as_bytes(), 2),
            ("Cafê::Inner".as_bytes(), 3),
            ("日本語::A".as_bytes(), 4),
            ("日本語::B".as_bytes(), 5),
            ("日本語::B::Nested".as_bytes(), 6),
        ];
        let mut block = Vec::new();
        encode_front_coded(&entries, &mut block).expect("encode");

        let mut decoded = Vec::new();
        decode_front_coded("test", 0, &block, |id, text| decoded.push((id, text.to_owned()))).expect("decode");
        let expected: Vec<(u64, String)> = entries
            .iter()
            .map(|(text, id)| (*id, String::from_utf8(text.to_vec()).expect("utf-8")))
            .collect();
        assert_eq!(decoded, expected);
        assert!(
            block.len() < entries.iter().map(|(text, _)| text.len() + 10).sum::<usize>(),
            "front-coding must be smaller than storing the strings in full"
        );
    }

    #[test]
    fn corrupt_front_coded_blocks_are_errors_not_panics() {
        let cases: Vec<(&[u8], &str)> = vec![
            (&[], "truncated header"),
            (&[1, 0, 0, 0], "truncated shared prefix length"),
            // count 1, shared 0, suffix length 4, only 2 suffix bytes, no id.
            (&[1, 0, 0, 0, 0, 4, b'a', b'b'], "truncated suffix"),
            // count 1, shared 5 but the previous entry is empty.
            (&[1, 0, 0, 0, 5, 1, b'a'], "shared prefix exceeds the previous entry"),
            // A complete entry whose suffix is not valid UTF-8.
            (
                &[1, 0, 0, 0, 0, 1, 0xff, 0, 0, 0, 0, 0, 0, 0, 1],
                "invalid UTF-8 in entry",
            ),
        ];
        for (block, expected) in cases {
            let error = decode_front_coded("test", 0, block, |_, _| {}).expect_err("corrupt block");
            assert!(
                matches!(error, StoreError::CorruptBlock { reason, .. } if reason == expected),
                "expected {expected:?}, got {error}"
            );
        }
    }

    #[test]
    fn corrupt_search_shard_is_recorded_without_panicking() {
        let graph = Graph::new();
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("graph.redb");
        drop(RedbStore::build(&path, &graph).expect("build store"));

        let db = Database::builder().open(&path).expect("open for corruption");
        let txn = db.begin_write().expect("write transaction");
        {
            let mut table = txn.open_table(SEARCH_NAMES).expect("search names table");
            // A header claiming one entry, with no entry behind it.
            table.insert(0, &[1_u8, 0, 0, 0][..]).expect("corrupt shard");
        }
        txn.commit().expect("commit corruption");
        drop(db);

        let graph = Graph::with_store(RedbStore::open(&path).expect("open store"));
        let found = graph.store_declaration_ids_matching(&|_, _| true);
        assert!(found.is_empty(), "a corrupt shard answers nothing");
        assert!(
            graph.store_error_count() > 0,
            "the corrupt shard must be recorded, not panicked on"
        );
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
        assert_eq!(nothing.len(), 0);
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
        drop(store);
        // The re-delete needs a writable handle: `open` is read-only, which is what every reader
        // (server, second session) gets.
        let store = RedbStore::create(&path).expect("reopen writable store");
        assert!(
            !store.delete_declaration(bar_id).expect("re-delete Bar"),
            "Bar already gone"
        );
    }

    #[test]
    fn keys_only_scans_agree_with_the_materialized_ones() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("base.redb");

        let base = Graph::new();
        let store = RedbStore::build(&path, &base).expect("build store");

        // The keys-only scans exist so cypher can enumerate nodes without deserializing (or
        // allocating a String for) every name and URI in the store. They must agree exactly.
        let keys: std::collections::BTreeSet<u64> = store
            .declaration_ids()
            .expect("declaration_ids")
            .into_iter()
            .map(|id| id.get())
            .collect();
        let names: std::collections::BTreeSet<u64> = store
            .search_names()
            .expect("search_names")
            .into_iter()
            .map(|(id, _name)| id.get())
            .collect();
        assert_eq!(keys, names, "declaration ids must match the name table");

        let doc_keys: std::collections::BTreeSet<u64> = store
            .document_ids()
            .expect("document_ids")
            .into_iter()
            .map(|id| id.get())
            .collect();
        let uris: std::collections::BTreeSet<u64> = store
            .document_uris()
            .expect("document_uris")
            .into_iter()
            .map(|(id, _uri)| id.get())
            .collect();
        assert_eq!(doc_keys, uris, "document ids must match the URI table");
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
    fn detach_store_leaves_an_empty_graph_that_indexes_in_memory() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("base.redb");

        let base = Graph::new();
        let (sample_id, _sample_name) = base
            .declarations()
            .iter()
            .next()
            .map(|(id, declaration)| (*id, declaration.name().to_string()))
            .expect("built-in declarations exist");
        RedbStore::build(&path, &base).expect("build store");
        drop(base);

        let mut graph = Graph::with_store(RedbStore::open(&path).expect("open store"));
        assert!(graph.declarations().is_empty(), "overlay is empty");
        assert!(graph.declaration(sample_id).is_some(), "the store answers the node");

        // Detaching is the corrupt-store fallback: the session must end up with no store and an
        // empty overlay so it can index in memory. A store that stays attached would keep serving
        // the copy that was just declared untrustworthy.
        graph.detach_store();

        assert!(!graph.is_store_backed(), "no store behind the graph");
        assert_eq!(graph.store_error_count(), 0, "the error counter is reset");
        assert!(
            graph.declaration(DeclarationId::from("NothingLeft")).is_none(),
            "the store answers nothing"
        );

        // Back to a plain in-memory graph: the built-ins are there (resolution seeds itself from
        // `Object`, so without them the next resolve panics) and re-indexing resolves cleanly.
        assert!(
            graph.declaration(*crate::model::built_in::OBJECT_ID).is_some(),
            "the built-in declarations are back"
        );
        crate::resolution::Resolver::new(&mut graph).resolve();
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
    #[test]
    fn all_diagnostics_includes_store_backed_documents() {
        use crate::test_utils::GraphTest;

        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("index.redb");
        let mut context = GraphTest::new();
        // Unterminated class: the indexer records a parse diagnostic on the document.
        context.index_uri("file:///foo.rb", "class Foo");
        RedbStore::build(&path, context.graph()).expect("build store");

        let mut graph = Graph::new();
        graph.attach_store(RedbStore::open(&path).expect("open store"));
        assert!(
            !graph.all_diagnostics().is_empty(),
            "store-backed document diagnostics must surface after attach"
        );
    }
}
