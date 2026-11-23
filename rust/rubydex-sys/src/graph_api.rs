//! This file provides the C API for the Graph object

use crate::config_api::ConfigPointer;
use crate::declaration_api::CDeclaration;
use crate::declaration_api::DeclarationsIter;
use crate::declaration_api::decl_id_from_char_ptr;
use crate::document_api::DocumentsIter;
use crate::reference_api::{
    CConstantReference, CMethodReference, ConstantReferencesIter, MethodReferencesIter, NO_METHOD_REFERENCE_NAME,
};
use crate::{name_api, utils};
use libc::{c_char, c_void};
use rubydex::config::Config;
use rubydex::indexing::LanguageId;
use rubydex::model::definitions::Definition;
use rubydex::model::encoding::Encoding;
use rubydex::model::graph::Graph;
use rubydex::model::identity_maps::IdentityHashMap;
use rubydex::model::ids::{DeclarationId, DefinitionId, NameId, StringId, UriId, declaration_id_from_lookup_name};
use rubydex::model::keywords;
use rubydex::model::name::NameRef;
use rubydex::model::visibility::Visibility;
use rubydex::query::{CompletionCandidate, CompletionContext, CompletionReceiver};
use rubydex::resolution::Resolver;
use rubydex::{indexing, integrity, listing, query};
use std::ffi::CString;
use std::path::PathBuf;
use std::{mem, ptr, sync::RwLock};

pub type GraphPointer = *mut c_void;

/// Creates a new graph wrapped in a `Box<RwLock<Graph>>` for thread-safe shared access across Ractors.
#[unsafe(no_mangle)]
pub extern "C" fn rdx_graph_new() -> GraphPointer {
    Box::into_raw(Box::new(RwLock::new(Graph::new()))) as GraphPointer
}

/// Frees a Graph through its pointer
#[unsafe(no_mangle)]
pub extern "C" fn rdx_graph_free(pointer: GraphPointer) {
    unsafe {
        let _ = Box::from_raw(pointer.cast::<RwLock<Graph>>());
    }
}

/// Runs `action` against the graph referenced by `pointer` under a read lock.
///
/// # Panics
///
/// Panics if the `RwLock` is poisoned (a writer panicked while holding the lock).
pub fn with_graph<F, T>(pointer: GraphPointer, action: F) -> T
where
    F: FnOnce(&Graph) -> T,
{
    let rwlock = unsafe { &*pointer.cast::<RwLock<Graph>>() };
    let guard = rwlock.read().unwrap();
    action(&guard)
}

fn with_mut_graph<F, T>(pointer: GraphPointer, action: F) -> T
where
    F: FnOnce(&mut Graph) -> T,
{
    let rwlock = unsafe { &*pointer.cast::<RwLock<Graph>>() };
    let mut guard = rwlock.write().unwrap();
    action(&mut guard)
}

/// Persists the resolved graph to an on-disk redb store at `path`, returning `true` on success.
/// Returns `false` if the path is invalid, the store cannot be built, or the `redb-store` feature
/// was not compiled in.
///
/// # Safety
///
/// `path` must be a valid NUL-terminated C string.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rdx_graph_build_store(pointer: GraphPointer, path: *const c_char) -> bool {
    let Ok(path) = (unsafe { utils::convert_char_ptr_to_string(path) }) else {
        return false;
    };
    with_graph(pointer, |graph| {
        #[cfg(feature = "redb-store")]
        {
            rubydex::model::store::RedbStore::build(std::path::Path::new(&path), graph).is_ok()
        }
        #[cfg(not(feature = "redb-store"))]
        {
            let _ = (graph, path);
            false
        }
    })
}

/// Switches an existing graph over to a prebuilt store as its disk-backed base layer (drops the
/// in-memory maps). Returns `true` on success. Used by the launcher after a short-lived child built
/// the store, so the long-lived server holds the bulk index off-heap.
///
/// # Safety
///
/// `path` must be a valid NUL-terminated C string.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rdx_graph_attach_store(pointer: GraphPointer, path: *const c_char) -> bool {
    let Ok(path) = (unsafe { utils::convert_char_ptr_to_string(path) }) else {
        return false;
    };
    with_mut_graph(pointer, |graph| {
        #[cfg(feature = "redb-store")]
        {
            match rubydex::model::store::RedbStore::open(std::path::Path::new(&path)) {
                Ok(store) => {
                    graph.attach_store(store);
                    true
                }
                Err(_) => false,
            }
        }
        #[cfg(not(feature = "redb-store"))]
        {
            let _ = (graph, path);
            false
        }
    })
}

/// Drops a prebuilt store and the overlay above it, returning the graph to a plain in-memory state.
/// Returns `true` on success, `false` when the `redb-store` feature was not compiled in. Used by the
/// Ruby layer when a store reports read failures, so the session can index in memory instead of
/// answering from the untrustworthy copy.
///
/// # Safety
///
/// - `pointer` must be a valid `GraphPointer` previously returned by this crate.
#[unsafe(no_mangle)]
pub extern "C" fn rdx_graph_detach_store(pointer: GraphPointer) -> bool {
    with_mut_graph(pointer, |graph| {
        #[cfg(feature = "redb-store")]
        {
            graph.detach_store();
            true
        }
        #[cfg(not(feature = "redb-store"))]
        {
            let _ = graph;
            false
        }
    })
}

/// Layout version of the on-disk store. Ruby mixes this into the freshness signature so a store
/// written by an incompatible layout is rebuilt instead of misread. The C extension always links
/// this symbol, so the build without the store feature keeps a stub: its signature is only mixed
/// in when the disk layer is enabled, which that build cannot do.
///
/// # Safety
///
/// - `pointer` must be a valid `GraphPointer` previously returned by this crate.
#[unsafe(no_mangle)]
#[cfg(feature = "redb-store")]
pub extern "C" fn rdx_store_format_version() -> u32 {
    rubydex::model::store::STORE_FORMAT_VERSION
}

#[unsafe(no_mangle)]
#[cfg(not(feature = "redb-store"))]
pub extern "C" fn rdx_store_format_version() -> u32 {
    0
}

/// Returns the number of store reads that failed since the graph was created or last attached a
/// store. Zero means the disk index answered everything; non-zero means it is corrupt or was
/// written by an incompatible layout, and the Ruby layer should fall back to the in-memory index.
///
/// # Safety
///
/// - `pointer` must be a valid `GraphPointer` previously returned by this crate.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rdx_graph_store_errors(pointer: GraphPointer) -> u64 {
    with_graph(pointer, |graph| {
        #[cfg(feature = "redb-store")]
        {
            graph.store_error_count() as u64
        }
        #[cfg(not(feature = "redb-store"))]
        {
            let _ = graph;
            0
        }
    })
}

/// Searches the graph using exact substring matching, returning every declaration whose name matches any of the
/// queries.
///
/// # Safety
///
/// Expects `pointer` to be a valid graph and `c_queries` to point to an array of `count` valid, NUL-terminated C
/// strings. Returns a null pointer when `count` is zero, `c_queries` is null, or any query is not valid UTF-8.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rdx_graph_declarations_search(
    pointer: GraphPointer,
    c_queries: *const *const c_char,
    count: usize,
) -> *mut DeclarationsIter {
    if count == 0 || c_queries.is_null() {
        return ptr::null_mut();
    }

    let Ok(queries) = (unsafe { utils::convert_double_pointer_to_vec(c_queries, count) }) else {
        return ptr::null_mut();
    };
    let query_refs: Vec<&str> = queries.iter().map(String::as_str).collect();

    let entries = with_graph(pointer, |graph| {
        query::declaration_search(graph, &query_refs, &query::MatchMode::Exact)
            .into_iter()
            .filter_map(|id| {
                let decl = graph.declaration(id)?;
                Some(CDeclaration::from_declaration(id, &decl))
            })
            .collect::<Vec<CDeclaration>>()
            .into_boxed_slice()
    });

    DeclarationsIter::new(entries)
}

/// Searches the graph using fuzzy matching, returning every declaration whose name matches any of the queries.
///
/// # Safety
///
/// Expects `pointer` to be a valid graph and `c_queries` to point to an array of `count` valid, NUL-terminated C
/// strings. Returns a null pointer when `count` is zero, `c_queries` is null, or any query is not valid UTF-8.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rdx_graph_declarations_fuzzy_search(
    pointer: GraphPointer,
    c_queries: *const *const c_char,
    count: usize,
) -> *mut DeclarationsIter {
    if count == 0 || c_queries.is_null() {
        return ptr::null_mut();
    }

    let Ok(queries) = (unsafe { utils::convert_double_pointer_to_vec(c_queries, count) }) else {
        return ptr::null_mut();
    };
    let query_refs: Vec<&str> = queries.iter().map(String::as_str).collect();

    let entries = with_graph(pointer, |graph| {
        query::declaration_search(graph, &query_refs, &query::MatchMode::Fuzzy)
            .into_iter()
            .filter_map(|id| {
                let decl = graph.declaration(id)?;
                Some(CDeclaration::from_declaration(id, &decl))
            })
            .collect::<Vec<CDeclaration>>()
            .into_boxed_slice()
    });

    DeclarationsIter::new(entries)
}

/// Returns an iterator over all dead code candidates in the graph.
///
/// # Safety
///
/// Expects `pointer` to be a valid graph. The returned iterator must be freed with `rdx_graph_declarations_iter_free`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rdx_graph_dead_code_candidates(pointer: GraphPointer) -> *mut DeclarationsIter {
    let entries = with_graph(pointer, |graph| {
        query::dead_code_candidates(graph)
            .into_iter()
            .filter_map(|id| {
                let decl = graph.declaration(id)?;
                Some(CDeclaration::from_declaration(id, &decl))
            })
            .collect::<Vec<CDeclaration>>()
            .into_boxed_slice()
    });

    DeclarationsIter::new(entries)
}

/// # Panics
///
/// Will panic if the nesting cannot be transformed into a vector of strings
///
/// # Safety
///
/// Assumes that the `const_name` and `nesting` pointer are valid
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rdx_graph_resolve_constant(
    pointer: GraphPointer,
    const_name: *const c_char,
    nesting: *const *const c_char,
    count: usize,
) -> *const CDeclaration {
    with_mut_graph(pointer, |graph| {
        let nesting: Vec<String> = unsafe { utils::convert_double_pointer_to_vec(nesting, count).unwrap() };
        let const_name: String = unsafe { utils::convert_char_ptr_to_string(const_name).unwrap() };

        // Store-backed graphs answer from the pre-resolved snapshot instead of running the in-memory
        // resolver, which materializes the name chain it resolves through. Both paths share one
        // resolution (`query::resolve_constant_read_only`), so the two cannot drift.
        #[cfg(feature = "redb-store")]
        if graph.is_store_backed() {
            let scopes = lexical_scope_ids(&nesting);
            return query::resolve_constant_read_only(graph, &const_name, &scopes).map_or(ptr::null(), |id| {
                graph.declaration(id).map_or(ptr::null(), |declaration| {
                    Box::into_raw(Box::new(CDeclaration::from_declaration(id, &declaration))).cast_const()
                })
            });
        }

        let Some(tracked_name) = name_api::name_from_nesting_names(graph, &const_name, nesting) else {
            return ptr::null();
        };

        resolve_constant_name(tracked_name)
    })
}

/// The declaration ids a `nesting` array names, outermost first. Nesting entries are written
/// relative to the entry before them (`["A", "B::C"]` is `A::B::C`), and a rooted entry (`"::A"`)
/// restarts at the top level. Used by the store-backed path, which resolves through declaration ids;
/// the in-memory path hands the names to the resolver instead.
#[cfg(feature = "redb-store")]
fn lexical_scope_ids(nesting: &[String]) -> Vec<DeclarationId> {
    let mut scopes = Vec::with_capacity(nesting.len());
    let mut prefix = String::new();
    for entry in nesting {
        let entry = if let Some(rooted) = entry.strip_prefix("::") {
            prefix.clear();
            rooted
        } else {
            entry.as_str()
        };
        if !prefix.is_empty() && !entry.is_empty() {
            prefix.push_str("::");
        }
        prefix.push_str(entry);
        scopes.push(declaration_id_from_lookup_name(&prefix));
    }
    scopes
}

/// The lexical scopes a name sits in, outermost first: the name's own declaration last, the `nesting`
/// chain it records above it (`class << Foo` inside `module Bar` is `[Bar, Foo::<Foo>]`). A name that
/// never resolved is skipped, since it cannot be a scope; the resolution then starts one level out.
#[cfg(feature = "redb-store")]
fn lexical_scopes_of_name(graph: &Graph, name_id: NameId) -> Vec<DeclarationId> {
    let mut scopes = Vec::new();
    let mut current = Some(name_id);
    while let Some(id) = current {
        let Some(name) = graph.name(id) else {
            break;
        };
        if let NameRef::Resolved(resolved) = &*name {
            scopes.push(*resolved.declaration_id());
        }
        current = *name.nesting();
    }
    scopes.reverse();
    scopes
}

/// Result of resolving a constant using a namespace definition as its lexical context.
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CDefinitionConstantResolution {
    Resolved = 0,
    NotFound = 1,
    DefinitionNotFound = 2,
    InvalidDefinitionKind = 3,
}

#[repr(C)]
#[derive(Debug)]
pub struct CDefinitionConstantResolutionResult {
    pub status: CDefinitionConstantResolution,
    pub declaration: *const CDeclaration,
}

/// Resolves a constant using a namespace definition as its lexical context.
///
/// # Panics
///
/// Panics if `const_name` cannot be converted to a string.
///
/// # Safety
///
/// Assumes that `pointer` and `const_name` are valid.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rdx_graph_resolve_constant_from_definition(
    pointer: GraphPointer,
    const_name: *const c_char,
    definition_id: u64,
) -> CDefinitionConstantResolutionResult {
    with_mut_graph(pointer, |graph| {
        let const_name: String = unsafe { utils::convert_char_ptr_to_string(const_name).unwrap() };
        let nesting = match class_or_module_definition_name_id(graph, DefinitionId::new(definition_id)) {
            Ok(nesting) => nesting,
            Err(status) => {
                return CDefinitionConstantResolutionResult {
                    status,
                    declaration: ptr::null(),
                };
            }
        };

        // Same store-backed path as `rdx_graph_resolve_constant`: one resolution for both entry
        // points, so a definition and a nesting cannot answer differently for the same scope.
        #[cfg(feature = "redb-store")]
        if graph.is_store_backed() {
            let scopes = lexical_scopes_of_name(graph, nesting);
            let declaration =
                query::resolve_constant_read_only(graph, &const_name, &scopes).map_or(ptr::null(), |id| {
                    graph.declaration(id).map_or(ptr::null(), |declaration| {
                        Box::into_raw(Box::new(CDeclaration::from_declaration(id, &declaration))).cast_const()
                    })
                });
            let status = if declaration.is_null() {
                CDefinitionConstantResolution::NotFound
            } else {
                CDefinitionConstantResolution::Resolved
            };
            return CDefinitionConstantResolutionResult { status, declaration };
        }

        let Some(tracked_name) = name_api::name_from_lexical_nesting(graph, &const_name, Some(nesting)) else {
            return CDefinitionConstantResolutionResult {
                status: CDefinitionConstantResolution::NotFound,
                declaration: ptr::null(),
            };
        };

        let declaration = resolve_constant_name(tracked_name);
        let status = if declaration.is_null() {
            CDefinitionConstantResolution::NotFound
        } else {
            CDefinitionConstantResolution::Resolved
        };

        CDefinitionConstantResolutionResult { status, declaration }
    })
}

fn class_or_module_definition_name_id(
    graph: &Graph,
    definition_id: DefinitionId,
) -> Result<NameId, CDefinitionConstantResolution> {
    // Layered lookup: an unedited store-backed definition is not in the overlay.
    let Some(definition) = graph.definition(definition_id) else {
        return Err(CDefinitionConstantResolution::DefinitionNotFound);
    };

    match &*definition {
        Definition::Class(definition) => Ok(*definition.name_id()),
        Definition::SingletonClass(definition) => Ok(*definition.name_id()),
        Definition::Module(definition) => Ok(*definition.name_id()),
        _ => Err(CDefinitionConstantResolution::InvalidDefinitionKind),
    }
}

fn resolve_constant_name(mut tracked_name: name_api::TrackedName<'_>) -> *const CDeclaration {
    let name_id = tracked_name.name_id();
    let declaration_id = Resolver::new(tracked_name.graph_mut()).resolve_constant(name_id);
    declaration_id.map_or(ptr::null(), |id| {
        let Some(declaration) = tracked_name.graph().declaration(id) else {
            return ptr::null();
        };
        Box::into_raw(Box::new(CDeclaration::from_declaration(id, &declaration))).cast_const()
    })
}

/// Adds glob patterns to exclude from file discovery during indexing.
///
/// # Panics
///
/// Will panic if the given array of C string patterns cannot be converted to a `Vec<String>`.
///
/// # Safety
///
/// - `pointer` must be a valid `GraphPointer` previously returned by this crate.
/// - `patterns` must be an array of `count` valid, null-terminated UTF-8 strings.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rdx_graph_exclude_patterns(
    pointer: GraphPointer,
    patterns: *const *const c_char,
    count: usize,
) {
    let patterns: Vec<String> = unsafe { utils::convert_double_pointer_to_vec(patterns, count).unwrap() };
    let entries: Vec<Box<str>> = patterns.into_iter().map(String::into_boxed_str).collect();
    with_mut_graph(pointer, |graph| graph.exclude_patterns(entries));
}

/// Returns the currently excluded glob patterns as an array of C strings. Writes the count to `out_count`. Returns NULL
/// if no patterns are excluded. Caller must free with `free_c_string_array`.
///
/// # Safety
///
/// - `pointer` must be a valid `GraphPointer` previously returned by this crate.
/// - `out_count` must be a valid, writable pointer.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rdx_graph_excluded_patterns(
    pointer: GraphPointer,
    out_count: *mut usize,
) -> *const *const c_char {
    with_graph(pointer, |graph| {
        let excluded = graph.excluded_patterns();

        if excluded.is_empty() {
            unsafe { *out_count = 0 };
            return ptr::null();
        }

        let c_strings: Vec<*const c_char> = excluded
            .iter()
            .filter_map(|pattern| {
                CString::new(pattern.as_ref())
                    .ok()
                    .map(|c_string| c_string.into_raw().cast_const())
            })
            .collect();

        unsafe { *out_count = c_strings.len() };

        let boxed = c_strings.into_boxed_slice();
        Box::into_raw(boxed).cast::<*const c_char>()
    })
}

/// Returns whether the workspace's `[disk_index]` section enabled the disk-backed index, as
/// configured by the file (the Ruby side layers the environment override on top).
///
/// # Safety
///
/// - `pointer` must be a valid `GraphPointer` previously returned by this crate.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rdx_graph_disk_index_enabled(pointer: GraphPointer) -> u64 {
    with_graph(pointer, |graph| u64::from(graph.disk_index_enabled()))
}

/// Returns whether the workspace opted into the machine-wide background manager.
///
/// # Safety
///
/// - `pointer` must be a valid `GraphPointer` previously returned by this crate.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rdx_graph_disk_index_manager(pointer: GraphPointer) -> u64 {
    with_graph(pointer, |graph| u64::from(graph.disk_index_manager()))
}

/// Returns where the store lives as configured: `"tmp"`, `"global"`, an absolute directory, or an
/// empty string when the workspace left it to the context-aware default. Caller must free with
/// `free_c_string`.
///
/// # Safety
///
/// - `pointer` must be a valid `GraphPointer` previously returned by this crate.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rdx_graph_disk_index_location(pointer: GraphPointer) -> *const c_char {
    with_graph(pointer, |graph| {
        CString::new(graph.disk_index_location()).map_or(ptr::null(), |c_string| c_string.into_raw().cast_const())
    })
}

/// Returns the workspace path as a C string. Caller must free with `free_c_string`.
///
/// # Safety
///
/// - `pointer` must be a valid `GraphPointer` previously returned by this crate.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rdx_graph_workspace_path(pointer: GraphPointer) -> *const c_char {
    with_graph(pointer, |graph| {
        CString::new(utils::interop_path(graph.workspace_path()))
            .map_or(ptr::null(), |c_string| c_string.into_raw().cast_const())
    })
}

/// Converts an absolute file path into the `file://` URI the graph stores documents under, so a Ruby-side
/// edit addresses the same document key the indexer used. Returns null when the path cannot be
/// represented. Caller must free with `free_c_string`.
///
/// # Safety
///
/// `path` must be a valid NUL-terminated C string.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rdx_path_to_uri(path: *const c_char) -> *const c_char {
    let Ok(path) = (unsafe { utils::convert_char_ptr_to_string(path) }) else {
        return ptr::null();
    };
    rubydex::model::document::path_to_uri(path.as_ref())
        .and_then(|uri| CString::new(uri).ok())
        .map_or(ptr::null(), |c_string| c_string.into_raw().cast_const())
}

/// Applies a parsed configuration file to the graph, which adopts the workspace it was loaded for along with the
/// settings of its `[graph]` section. This is the only way to point the graph at a workspace other than the current
/// directory, and it replaces any previously applied configuration. Tool-specific sections are ignored.
///
/// # Safety
///
/// - `pointer` must be a valid `GraphPointer` previously returned by this crate.
/// - `config` must be a valid `ConfigPointer` previously returned by `rdx_config_load`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rdx_graph_load_config(pointer: GraphPointer, config: ConfigPointer) {
    let config = unsafe { &*config.cast::<Config>() };
    with_mut_graph(pointer, |graph| graph.load_config(config));
}

/// Indexes all given file paths in parallel using the provided Graph pointer.
/// Returns an array of error message strings and writes the count to `out_error_count`.
/// Returns NULL if there are no errors. Caller must free with `free_c_string_array`.
///
/// # Panics
///
/// Will panic if the given array of C string file paths cannot be converted to a Vec<String>
///
/// # Safety
///
/// This function is unsafe because it dereferences raw pointers coming from C. The caller has to ensure that the Ruby
/// VM will not free the pointers related to the string array while they are in use by Rust
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rdx_index_all(
    pointer: GraphPointer,
    file_paths: *const *const c_char,
    count: usize,
    out_error_count: *mut usize,
) -> *const *const c_char {
    let file_paths: Vec<String> = unsafe { utils::convert_double_pointer_to_vec(file_paths, count).unwrap() };

    with_mut_graph(pointer, |graph| {
        // A store-backed graph is a pre-resolved snapshot; indexing into the overlay without
        // re-resolving would leave it inconsistent. Callers detach the store before falling back to
        // the in-memory index.
        #[cfg(feature = "redb-store")]
        if graph.is_store_backed() {
            unsafe { *out_error_count = 0 };
            return ptr::null();
        }

        let (file_paths, listing_errors) = listing::collect_file_paths(file_paths, &graph.excluded_patterns());
        let indexing_errors = indexing::index_files(graph, file_paths, indexing::IndexerBackend::RubyIndexer);

        let all_errors: Vec<String> = listing_errors
            .into_iter()
            .chain(indexing_errors)
            .map(|e| e.to_string())
            .collect();

        if all_errors.is_empty() {
            unsafe { *out_error_count = 0 };
            return ptr::null();
        }

        let c_strings: Vec<*const c_char> = all_errors
            .into_iter()
            .filter_map(|string| {
                CString::new(string)
                    .ok()
                    .map(|c_string| c_string.into_raw().cast_const())
            })
            .collect();

        unsafe { *out_error_count = c_strings.len() };

        let boxed = c_strings.into_boxed_slice();
        Box::into_raw(boxed).cast::<*const c_char>()
    })
}

/// Returns a pointer to the URI ID of the document identified by `uri`, or NULL if it doesn't exist.
/// Caller must free the returned pointer with `free_u64`.
///
/// # Safety
///
/// Expects both the graph pointer and uri string pointer to be valid
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rdx_graph_get_document(pointer: GraphPointer, uri: *const c_char) -> *const u64 {
    let Ok(uri_str) = (unsafe { utils::convert_char_ptr_to_string(uri) }) else {
        return ptr::null();
    };

    with_graph(pointer, |graph| {
        let uri_id = UriId::from(uri_str.as_str());

        // Layered lookup: in disk mode the document lives in the store, not the in-memory overlay.
        if graph.document(uri_id).is_some() {
            Box::into_raw(Box::new(*uri_id)).cast_const()
        } else {
            ptr::null()
        }
    })
}

/// Deletes a document and all of its definitions from the graph.
/// Returns a pointer to the URI ID if the document was found and removed, or NULL if it didn't exist.
/// Caller must free the returned pointer with `free_u64`.
///
/// # Safety
///
/// Expects both the graph pointer and uri string pointer to be valid
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rdx_graph_delete_document(pointer: GraphPointer, uri: *const c_char) -> *const u64 {
    let Ok(uri_str) = (unsafe { utils::convert_char_ptr_to_string(uri) }) else {
        return ptr::null();
    };

    with_mut_graph(pointer, |graph| match graph.delete_document(&uri_str) {
        Some(uri_id) => Box::into_raw(Box::new(*uri_id)),
        None => ptr::null(),
    })
}

/// Runs the resolver to compute declarations, ownership and related structures
#[unsafe(no_mangle)]
pub extern "C" fn rdx_graph_resolve(pointer: GraphPointer) {
    with_mut_graph(pointer, |graph| {
        let mut resolver = Resolver::new(graph);
        resolver.resolve();
    });
}

/// Checks the integrity of the graph and returns an array of error message strings. Returns NULL if there are no
/// errors. Caller must free with `free_c_string_array`.
///
/// # Safety
///
/// - `pointer` must be a valid `GraphPointer` previously returned by this crate.
/// - `out_error_count` must be a valid, writable pointer.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rdx_check_integrity(
    pointer: GraphPointer,
    out_error_count: *mut usize,
) -> *const *const c_char {
    with_graph(pointer, |graph| {
        let errors = integrity::check_integrity(graph);

        if errors.is_empty() {
            unsafe { *out_error_count = 0 };
            return ptr::null();
        }

        let c_strings: Vec<*const c_char> = errors
            .into_iter()
            .filter_map(|error| {
                CString::new(error.to_string())
                    .ok()
                    .map(|c_string| c_string.into_raw().cast_const())
            })
            .collect();

        unsafe { *out_error_count = c_strings.len() };

        let boxed = c_strings.into_boxed_slice();
        Box::into_raw(boxed).cast::<*const c_char>()
    })
}

/// # Safety
///
/// Expects both the graph pointer and encoding string pointer to be valid
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rdx_graph_set_encoding(pointer: GraphPointer, encoding_str: *const c_char) -> bool {
    let Ok(encoding) = (unsafe { utils::convert_char_ptr_to_string(encoding_str) }) else {
        return false;
    };

    let encoding_variant = match encoding.as_str() {
        "utf8" => Encoding::Utf8,
        "utf16" => Encoding::Utf16,
        "utf32" => Encoding::Utf32,
        _ => {
            return false;
        }
    };

    with_mut_graph(pointer, |graph| {
        graph.set_encoding(encoding_variant);
    });

    true
}

/// Creates a new iterator over declaration IDs by snapshotting the current set of IDs.
///
/// # Safety
///
/// - `pointer` must be a valid `GraphPointer` previously returned by this crate.
/// - The returned pointer must be freed with `rdx_graph_declarations_iter_free`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rdx_graph_declarations_iter_new(pointer: GraphPointer) -> *mut DeclarationsIter {
    // Snapshot the declarations at iterator creation to avoid borrowing across FFI calls
    let entries = with_graph(pointer, |graph| {
        let mut entries = graph
            .declarations()
            .iter()
            .map(|(id, decl)| CDeclaration::from_declaration(*id, decl))
            .collect::<Vec<CDeclaration>>();
        graph.for_each_store_declaration(|id, declaration| {
            entries.push(CDeclaration::from_declaration(id, &declaration));
        });
        entries.into_boxed_slice()
    });

    DeclarationsIter::new(entries)
}

/// Creates a new iterator over document (URI) IDs by snapshotting the current set of IDs.
///
/// # Safety
///
/// - `pointer` must be a valid `GraphPointer` previously returned by this crate.
/// - The returned pointer must be freed with `rdx_graph_documents_iter_free`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rdx_graph_documents_iter_new(pointer: GraphPointer) -> *mut DocumentsIter {
    let entries = with_graph(pointer, |graph| {
        let mut ids: Vec<u64> = graph.documents().keys().map(|uri_id| **uri_id).collect();
        ids.extend(
            graph
                .store_document_ids()
                .into_iter()
                .filter(|id| !graph.documents().contains_key(id))
                .map(|id| *id),
        );
        ids.into_boxed_slice()
    });

    DocumentsIter::new(entries)
}

/// Attempts to resolve a declaration from a fully-qualified name string.
/// Returns a `CDeclaration` pointer if it exists, or NULL if it does not.
///
/// # Safety
/// - `pointer` must be a valid `GraphPointer`
/// - `name` must be a valid, null-terminated UTF-8 string
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rdx_graph_get_declaration(pointer: GraphPointer, name: *const c_char) -> *const CDeclaration {
    let Ok(name_str) = (unsafe { utils::convert_char_ptr_to_string(name) }) else {
        return ptr::null();
    };

    with_graph(pointer, |graph| {
        let decl_id = declaration_id_from_lookup_name(&name_str);

        // Layered lookup: in-memory workspace first, then the disk-backed store.
        if let Some(decl) = graph.declaration(decl_id) {
            Box::into_raw(Box::new(CDeclaration::from_declaration(decl_id, &decl))).cast_const()
        } else {
            ptr::null()
        }
    })
}

/// Returns the number of constant references the graph can enumerate, without materializing them.
///
/// # Safety
/// - `pointer` must be a valid `GraphPointer` previously returned by this crate.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rdx_graph_constant_references_count(pointer: GraphPointer) -> usize {
    with_graph(pointer, Graph::constant_reference_count)
}

/// Returns the number of method references the graph can enumerate, without materializing them.
///
/// # Safety
/// - `pointer` must be a valid `GraphPointer` previously returned by this crate.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rdx_graph_method_references_count(pointer: GraphPointer) -> usize {
    with_graph(pointer, Graph::method_reference_count)
}

/// Creates a new iterator over constant references by snapshotting the current set of IDs.
///
/// # Safety
/// - `pointer` must be a valid `GraphPointer` previously returned by this crate.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rdx_graph_constant_references_iter_new(pointer: GraphPointer) -> *mut ConstantReferencesIter {
    with_graph(pointer, |graph| {
        let mut refs: Vec<_> = graph
            .constant_references()
            .iter()
            .map(|(id, cref)| {
                let declaration_id = graph
                    .name(*cref.name_id())
                    .and_then(|name_ref| match &*name_ref {
                        NameRef::Resolved(resolved) => Some(**resolved.declaration_id()),
                        NameRef::Unresolved(_) => None,
                    })
                    .unwrap_or(0);

                CConstantReference {
                    id: **id,
                    declaration_id,
                }
            })
            .collect();

        graph.for_each_store_constant_reference_with_name(|id, _cref, name_ref| {
            let declaration_id = name_ref
                .and_then(|name_ref| match name_ref {
                    NameRef::Resolved(resolved) => Some(**resolved.declaration_id()),
                    NameRef::Unresolved(_) => None,
                })
                .unwrap_or(0);
            refs.push(CConstantReference {
                id: id.get(),
                declaration_id,
            });
        });

        ConstantReferencesIter::new(refs.into_boxed_slice())
    })
}

/// Returns the graph's current mutation epoch, incremented whenever a document is deleted or the
/// backing store changes.
///
/// # Safety
/// - `pointer` must be a valid `GraphPointer` previously returned by this crate.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rdx_graph_mutation_epoch(pointer: GraphPointer) -> u64 {
    with_graph(pointer, Graph::mutation_epoch)
}

/// Creates a new iterator over method references by snapshotting the current set of IDs and
/// deduplicated method names so full scans avoid millions of per-reference store transactions.
///
/// # Safety
/// - `pointer` must be a valid `GraphPointer` previously returned by this crate.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rdx_graph_method_references_iter_new(pointer: GraphPointer) -> *mut MethodReferencesIter {
    with_graph(pointer, |graph| {
        let mut strings: Vec<Box<str>> = Vec::new();
        let mut string_indices: IdentityHashMap<StringId, u32> = IdentityHashMap::default();
        let mut refs: Vec<CMethodReference> = Vec::with_capacity(graph.method_references().len());

        for (id, reference) in graph.method_references() {
            let str_id = *reference.str();
            let name_index = if let Some(&idx) = string_indices.get(&str_id) {
                idx
            } else if let Some(string_ref) = graph.string(str_id) {
                let idx = u32::try_from(strings.len()).unwrap_or(NO_METHOD_REFERENCE_NAME);
                if idx < NO_METHOD_REFERENCE_NAME {
                    strings.push(string_ref.as_str().into());
                    string_indices.insert(str_id, idx);
                }
                idx
            } else {
                NO_METHOD_REFERENCE_NAME
            };
            refs.push(CMethodReference { id: **id, name_index });
        }

        graph.for_each_store_method_reference_with_name(&mut strings, &mut string_indices, |id, name_index| {
            refs.push(CMethodReference {
                id: id.get(),
                name_index,
            });
        });

        MethodReferencesIter::with_names(
            refs.into_boxed_slice(),
            strings.into_boxed_slice(),
            graph.mutation_epoch(),
        )
    })
}

/// Resolves a require path to its document URI ID.
/// Returns a pointer to the URI ID if found, or NULL if not found.
/// Caller must free with the returned pointer.
///
/// # Safety
/// - `pointer` must be a valid `GraphPointer` previously returned by this crate.
/// - `require_path` must be a valid, null-terminated UTF-8 string.
/// - `load_paths` must be an array of `load_paths_count` valid, null-terminated UTF-8 strings.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rdx_resolve_require_path(
    pointer: GraphPointer,
    require_path: *const c_char,
    load_paths: *const *const c_char,
    load_paths_count: usize,
) -> *const u64 {
    let Ok(path_str) = (unsafe { utils::convert_char_ptr_to_string(require_path) }) else {
        return ptr::null();
    };

    let Ok(paths_vec) = (unsafe { utils::convert_double_pointer_to_vec(load_paths, load_paths_count) }) else {
        return ptr::null();
    };
    let paths_vec = paths_vec.into_iter().map(PathBuf::from).collect::<Vec<_>>();

    with_graph(pointer, |graph| {
        query::resolve_require_path(graph, &path_str, &paths_vec).map_or(ptr::null(), |id| Box::into_raw(Box::new(*id)))
    })
}

/// Returns all require paths for completion.
/// Returns array of C strings and writes count to `out_count`.
/// Returns null if `load_path` contain invalid UTF-8.
/// Caller must free with `free_c_string_array`.
///
/// # Safety
/// - `pointer` must be a valid `GraphPointer` previously returned by this crate.
/// - `load_path` must be an array of `load_path_count` valid, null-terminated UTF-8 strings.
/// - `out_count` must be a valid, writable pointer.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rdx_require_paths(
    pointer: GraphPointer,
    load_path: *const *const c_char,
    load_path_count: usize,
    out_count: *mut usize,
) -> *const *const c_char {
    let Ok(paths_vec) = (unsafe { utils::convert_double_pointer_to_vec(load_path, load_path_count) }) else {
        return ptr::null_mut();
    };
    let paths_vec = paths_vec.into_iter().map(PathBuf::from).collect::<Vec<_>>();

    let results = with_graph(pointer, |graph| query::require_paths(graph, &paths_vec));

    let c_strings: Vec<*const c_char> = results
        .into_iter()
        .filter_map(|string| {
            CString::new(string)
                .ok()
                .map(|c_string| c_string.into_raw().cast_const())
        })
        .collect();

    unsafe { *out_count = c_strings.len() };

    let boxed = c_strings.into_boxed_slice();
    Box::into_raw(boxed).cast::<*const c_char>()
}

#[repr(C)]
pub enum IndexSourceResult {
    Success = 0,
    InvalidUri = 1,
    InvalidSource = 2,
    InvalidLanguageId = 3,
    UnsupportedLanguageId = 4,
}

/// Indexes source code from memory using the specified language.  Returns `IndexSourceResult::Success` on success
/// or a specific error variant if string conversion or language lookup fails.
///
/// # Safety
///
/// - `pointer` must be a valid `GraphPointer` previously returned by this crate.
/// - `uri` and `language_id` must be valid, null-terminated UTF-8 strings.
/// - `source` must point to a valid UTF-8 byte buffer of at least `source_len` bytes.
///   It may contain null bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rdx_index_source(
    pointer: GraphPointer,
    uri: *const c_char,
    source: *const c_char,
    source_len: usize,
    language_id: *const c_char,
) -> IndexSourceResult {
    let Ok(uri_str) = (unsafe { utils::convert_char_ptr_to_string(uri) }) else {
        return IndexSourceResult::InvalidUri;
    };

    let source_bytes = unsafe { std::slice::from_raw_parts(source.cast::<u8>(), source_len) };
    let Ok(source_str) = std::str::from_utf8(source_bytes) else {
        return IndexSourceResult::InvalidSource;
    };

    let Ok(language_id_str) = (unsafe { utils::convert_char_ptr_to_string(language_id) }) else {
        return IndexSourceResult::InvalidLanguageId;
    };

    let Ok(language) = LanguageId::from_language_id(&language_id_str) else {
        return IndexSourceResult::UnsupportedLanguageId;
    };

    with_mut_graph(pointer, |graph| {
        indexing::index_source(graph, uri_str.into(), source_str, &language);
        IndexSourceResult::Success
    })
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub enum CCompletionCandidateKind {
    Declaration = 0,
    Keyword = 1,
    KeywordParameter = 2,
}

#[repr(C)]
pub struct CCompletionCandidate {
    pub kind: CCompletionCandidateKind,
    /// Only valid when `kind == Declaration`; null otherwise.
    pub declaration: *const CDeclaration,
    pub name: *const c_char,
    pub documentation: *const c_char,
}

#[repr(C)]
pub struct CompletionCandidateArray {
    pub items: *mut CCompletionCandidate,
    pub len: usize,
}

impl CompletionCandidateArray {
    fn from_vec(entries: Vec<CCompletionCandidate>) -> *mut CompletionCandidateArray {
        let mut boxed = entries.into_boxed_slice();
        let len = boxed.len();
        let ptr = boxed.as_mut_ptr();
        mem::forget(boxed);
        Box::into_raw(Box::new(CompletionCandidateArray { items: ptr, len }))
    }
}

/// Frees a completion candidate array previously returned by a completion function.
///
/// # Safety
///
/// - `ptr` must be a valid pointer previously returned by a completion function.
/// - `ptr` must not be used after being freed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rdx_completion_candidates_free(ptr: *mut CompletionCandidateArray) {
    if ptr.is_null() {
        return;
    }

    let array = unsafe { Box::from_raw(ptr) };

    if !array.items.is_null() && array.len > 0 {
        let slice_ptr = ptr::slice_from_raw_parts_mut(array.items, array.len);
        let mut boxed_slice: Box<[CCompletionCandidate]> = unsafe { Box::from_raw(slice_ptr) };

        for entry in &mut *boxed_slice {
            if !entry.declaration.is_null() {
                let _ = unsafe { Box::from_raw(entry.declaration.cast_mut()) };
            }
            if !entry.name.is_null() {
                let _ = unsafe { CString::from_raw(entry.name.cast_mut()) };
            }
            if !entry.documentation.is_null() {
                let _ = unsafe { CString::from_raw(entry.documentation.cast_mut()) };
            }
        }
    }
}

/// Converts the nesting names into a tracked name.
/// The last nesting name is treated as the self type; if the array is empty, `"Object"` is used.
///
/// Returns `None` if the nesting array contains invalid UTF-8.
///
/// # Safety
///
/// `nesting` must point to `nesting_count` valid, null-terminated UTF-8 strings.
unsafe fn completion_nesting_name(
    graph: &mut Graph,
    nesting: *const *const c_char,
    nesting_count: usize,
) -> Option<name_api::TrackedName<'_>> {
    let mut nesting: Vec<String> = unsafe { utils::convert_double_pointer_to_vec(nesting, nesting_count).ok()? };

    // When serving completion in a bare script, the self (top level) context is Object
    let self_name = if nesting.is_empty() {
        "Object".to_string()
    } else {
        nesting.pop().unwrap()
    };

    name_api::name_from_nesting_names(graph, &self_name, nesting)
}

/// The result of a completion operation, carrying either a candidate array or an error message.
#[repr(C)]
pub struct CompletionResult {
    /// Non-null on success; null on error.
    pub candidates: *mut CompletionCandidateArray,
    /// Non-null on error; null on success. Caller must free with `free_c_string`.
    pub error: *const c_char,
}

impl CompletionResult {
    fn success(candidates: *mut CompletionCandidateArray) -> Self {
        Self {
            candidates,
            error: ptr::null(),
        }
    }

    fn error(message: &str) -> Self {
        Self {
            candidates: ptr::null_mut(),
            error: CString::new(message).map_or(ptr::null(), |s| s.into_raw().cast_const()),
        }
    }
}

/// Runs completion for the given receiver and returns a structured result with candidates or an error message
fn run_and_finalize_completion(graph: &Graph, receiver: CompletionReceiver) -> CompletionResult {
    let candidates = match query::completion_candidates(graph, CompletionContext::new(receiver)) {
        Ok(candidates) => candidates,
        Err(e) => return CompletionResult::error(&e.to_string()),
    };

    let entries: Vec<CCompletionCandidate> = candidates
        .into_iter()
        .filter_map(|candidate| match candidate {
            CompletionCandidate::Declaration(id) => {
                let decl = graph.declaration(id)?;
                Some(CCompletionCandidate {
                    kind: CCompletionCandidateKind::Declaration,
                    declaration: Box::into_raw(Box::new(CDeclaration::from_declaration(id, &decl))),
                    name: ptr::null(),
                    documentation: ptr::null(),
                })
            }
            CompletionCandidate::Keyword(kw) => Some(CCompletionCandidate {
                kind: CCompletionCandidateKind::Keyword,
                declaration: ptr::null(),
                name: CString::new(kw.name())
                    .expect("keyword name must not contain NUL")
                    .into_raw()
                    .cast_const(),
                documentation: CString::new(kw.documentation())
                    .expect("keyword documentation must not contain NUL")
                    .into_raw()
                    .cast_const(),
            }),
            CompletionCandidate::KeywordArgument(str_id) => {
                let name_str = graph.string(str_id)?;
                let c_name = CString::new(name_str.as_str()).ok()?.into_raw().cast_const();
                Some(CCompletionCandidate {
                    kind: CCompletionCandidateKind::KeywordParameter,
                    declaration: ptr::null(),
                    name: c_name,
                    documentation: ptr::null(),
                })
            }
        })
        .collect();

    CompletionResult::success(CompletionCandidateArray::from_vec(entries))
}

/// Returns expression completion candidates.
/// The caller must free candidates with `rdx_completion_candidates_free`
/// and the error string (if non-null) with `free_c_string`.
///
/// # Safety
///
/// - `pointer` must be a valid `GraphPointer` previously returned by this crate.
/// - `nesting` must point to `nesting_count` valid, null-terminated UTF-8 strings.
/// - `self_receiver` is the fully qualified name of the **type of `self`**
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rdx_graph_complete_expression(
    pointer: GraphPointer,
    nesting: *const *const c_char,
    nesting_count: usize,
    self_receiver: *const c_char,
) -> CompletionResult {
    with_mut_graph(pointer, |graph| {
        let Some(tracked_name) = (unsafe { completion_nesting_name(graph, nesting, nesting_count) }) else {
            return CompletionResult::success(ptr::null_mut());
        };
        let name_id = tracked_name.name_id();

        let self_decl_id = unsafe { decl_id_from_char_ptr(self_receiver) };

        run_and_finalize_completion(
            tracked_name.graph(),
            CompletionReceiver::Expression {
                self_decl_id,
                nesting_name_id: name_id,
            },
        )
    })
}

/// Returns namespace access completion candidates.
/// The caller must free candidates with `rdx_completion_candidates_free`
/// and the error string (if non-null) with `free_c_string`.
///
/// # Safety
///
/// - `pointer` must be a valid `GraphPointer` previously returned by this crate.
/// - `name` must be a valid, null-terminated UTF-8 string (FQN of the namespace).
/// - `self_receiver` must be null or a valid, null-terminated UTF-8 string. When non-null, it
///   is the caller's runtime self type (e.g., for filtering `private_class_method` visibility).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rdx_graph_complete_namespace_access(
    pointer: GraphPointer,
    name: *const c_char,
    self_receiver: *const c_char,
) -> CompletionResult {
    let Ok(name_str) = (unsafe { utils::convert_char_ptr_to_string(name) }) else {
        return CompletionResult::success(ptr::null_mut());
    };

    with_mut_graph(pointer, |graph| {
        let self_decl_id = unsafe { decl_id_from_char_ptr(self_receiver) };

        run_and_finalize_completion(
            graph,
            CompletionReceiver::NamespaceAccess {
                self_decl_id,
                namespace_decl_id: declaration_id_from_lookup_name(&name_str),
            },
        )
    })
}

/// Returns method call completion candidates.
/// The caller must free candidates with `rdx_completion_candidates_free`
/// and the error string (if non-null) with `free_c_string`.
///
/// # Safety
///
/// - `pointer` must be a valid `GraphPointer` previously returned by this crate.
/// - `name` must be a valid, null-terminated UTF-8 string (FQN of the receiver).
/// - `self_receiver` must be null or a valid, null-terminated UTF-8 string. When non-null, it
///   is the caller's runtime self type, used for MRI-style visibility checks.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rdx_graph_complete_method_call(
    pointer: GraphPointer,
    name: *const c_char,
    self_receiver: *const c_char,
) -> CompletionResult {
    let Ok(name_str) = (unsafe { utils::convert_char_ptr_to_string(name) }) else {
        return CompletionResult::success(ptr::null_mut());
    };

    with_mut_graph(pointer, |graph| {
        let self_decl_id = unsafe { decl_id_from_char_ptr(self_receiver) };

        run_and_finalize_completion(
            graph,
            CompletionReceiver::MethodCall {
                self_decl_id,
                receiver_decl_id: declaration_id_from_lookup_name(&name_str),
            },
        )
    })
}

/// Returns method argument completion candidates.
/// The caller must free candidates with `rdx_completion_candidates_free`
/// and the error string (if non-null) with `free_c_string`.
///
/// # Safety
///
/// - `pointer` must be a valid `GraphPointer` previously returned by this crate.
/// - `name` must be a valid, null-terminated UTF-8 string (FQN of the method).
/// - `nesting` must point to `nesting_count` valid, null-terminated UTF-8 strings.
/// - `self_receiver` must be null or a valid, null-terminated UTF-8 string. See
///   `rdx_graph_complete_expression` for semantics.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rdx_graph_complete_method_argument(
    pointer: GraphPointer,
    name: *const c_char,
    nesting: *const *const c_char,
    nesting_count: usize,
    self_receiver: *const c_char,
) -> CompletionResult {
    let Ok(name_str) = (unsafe { utils::convert_char_ptr_to_string(name) }) else {
        return CompletionResult::success(ptr::null_mut());
    };

    with_mut_graph(pointer, |graph| {
        let Some(tracked_name) = (unsafe { completion_nesting_name(graph, nesting, nesting_count) }) else {
            return CompletionResult::success(ptr::null_mut());
        };
        let nesting_name_id = tracked_name.name_id();

        let self_decl_id = unsafe { decl_id_from_char_ptr(self_receiver) };

        run_and_finalize_completion(
            tracked_name.graph(),
            CompletionReceiver::MethodArgument {
                self_decl_id,
                nesting_name_id,
                method_decl_id: declaration_id_from_lookup_name(&name_str),
            },
        )
    })
}

#[repr(C)]
pub struct CKeyword {
    name: *const c_char,
    documentation: *const c_char,
}

/// Looks up a Ruby keyword by its exact name.
/// Returns a heap-allocated `CKeyword` if found, or NULL if the name is not a keyword.
/// Caller must free with `rdx_keyword_free`.
///
/// # Safety
///
/// - `name` must be a valid, null-terminated UTF-8 string.
///
/// # Panics
///
/// Will panic if the keyword's name or documentation contains an internal NUL byte
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rdx_keyword_get(name: *const c_char) -> *const CKeyword {
    let Ok(name_str) = (unsafe { utils::convert_char_ptr_to_string(name) }) else {
        return ptr::null();
    };

    match keywords::get(&name_str) {
        Some(kw) => {
            let c_name = CString::new(kw.name())
                .expect("keyword name must not contain NUL")
                .into_raw()
                .cast_const();

            let c_doc = CString::new(kw.documentation())
                .expect("keyword documentation must not contain NUL")
                .into_raw()
                .cast_const();

            Box::into_raw(Box::new(CKeyword {
                name: c_name,
                documentation: c_doc,
            }))
            .cast_const()
        }
        None => ptr::null(),
    }
}

#[repr(u8)]
#[derive(Debug, Clone, Copy)]
pub enum CVisibility {
    Public = 0,
    Protected = 1,
    Private = 2,
}

/// Returns the visibility of a declaration (method, constant, class, or module) as a heap-allocated
/// `CVisibility`, or NULL when the declaration carries no visibility (e.g. variables, singleton
/// classes, todos). Caller must free the returned pointer with `free_c_visibility`.
///
/// # Safety
///
/// - `pointer` must be a valid `GraphPointer` previously returned by this crate.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rdx_graph_visibility(pointer: GraphPointer, declaration_id: u64) -> *const CVisibility {
    with_graph(pointer, |graph| {
        let Some(visibility) = graph.visibility(&DeclarationId::new(declaration_id)) else {
            return ptr::null();
        };

        let c_visibility = match visibility {
            Visibility::Public => CVisibility::Public,
            Visibility::Protected => CVisibility::Protected,
            Visibility::Private => CVisibility::Private,
            Visibility::ModuleFunction => {
                unimplemented!("module_function visibility translation is not implemented yet")
            }
        };

        Box::into_raw(Box::new(c_visibility)).cast_const()
    })
}

/// Frees a `CVisibility` previously returned by `rdx_graph_visibility`.
///
/// # Safety
///
/// - `ptr` must be a valid pointer previously returned by `rdx_graph_visibility`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn free_c_visibility(ptr: *const CVisibility) {
    unsafe {
        let _ = Box::from_raw(ptr.cast_mut());
    }
}

/// Frees a `CKeyword` previously returned by `rdx_keyword_get`.
///
/// # Safety
///
/// - `ptr` must be a valid pointer previously returned by `rdx_keyword_get`, or NULL.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rdx_keyword_free(ptr: *const CKeyword) {
    if ptr.is_null() {
        return;
    }

    let kw = unsafe { Box::from_raw(ptr.cast_mut()) };

    if !kw.name.is_null() {
        let _ = unsafe { CString::from_raw(kw.name.cast_mut()) };
    }

    if !kw.documentation.is_null() {
        let _ = unsafe { CString::from_raw(kw.documentation.cast_mut()) };
    }
}

#[cfg(test)]
mod tests {
    use crate::declaration_api;
    use rubydex::indexing::ruby_indexer::RubyIndexer;

    use super::*;

    /// A URI from a crafted store can hold an interior NUL, which has no C string form. It must read
    /// back as absent rather than panic across `extern "C"` and abort the host process.
    #[test]
    fn a_uri_with_an_interior_nul_is_absent_instead_of_panicking() {
        let mut graph = Graph::new();
        let uri = "file:///foo\u{0}bar.rb";
        indexing::index_source(&mut graph, uri.into(), "class Foo; end\n", &LanguageId::Ruby);
        let uri_id = UriId::from(uri);
        let definition_id = graph
            .definitions()
            .iter()
            .find(|(_, definition)| definition.uri_id().get() == uri_id.get())
            .map(|(id, _)| *id)
            .expect("a definition in the document");

        let graph_ptr = Box::into_raw(Box::new(std::sync::RwLock::new(graph))) as GraphPointer;
        unsafe {
            assert!(
                crate::document_api::rdx_document_uri(graph_ptr, uri_id.get()).is_null(),
                "a NUL URI has no C string form"
            );
            assert!(
                crate::definition_api::rdx_definition_location(graph_ptr, definition_id.get()).is_null(),
                "a location whose URI cannot be a C string is absent, not a crash"
            );
        }
        drop(unsafe { Box::from_raw(graph_ptr.cast::<std::sync::RwLock<Graph>>()) });
    }

    #[cfg(feature = "redb-store")]
    #[test]
    fn document_getters_find_store_backed_nodes() {
        use rubydex::model::store::RedbStore;

        let mut indexer = RubyIndexer::new(
            "file:///foo.rb".into(),
            "
            class Foo
              BAR = 1
              def bar
                BAR + baz
              end
            end
            ",
        );
        indexer.index();

        let mut graph = Graph::new();
        graph.consume_document_changes(indexer.local_graph());
        Resolver::new(&mut graph).resolve();

        let uri_id = UriId::from("file:///foo.rb");
        let def_id = graph
            .definitions()
            .iter()
            .find(|(_, def)| def.uri_id().get() == uri_id.get())
            .map(|(id, _)| *id)
            .expect("a definition in foo.rb");
        let const_ref_id = graph
            .constant_references()
            .iter()
            .find(|(_, reference)| reference.uri_id().get() == uri_id.get())
            .map(|(id, _)| *id)
            .expect("a constant reference in foo.rb");
        let method_ref_id = graph
            .method_references()
            .iter()
            .find(|(_, reference)| reference.uri_id().get() == uri_id.get())
            .map(|(id, _)| *id)
            .expect("a method reference in foo.rb");

        let dir = tempfile::tempdir().expect("tempdir");
        let store_path = dir.path().join("index.redb");
        RedbStore::build(&store_path, &graph).expect("build store");
        drop(graph); // the nodes live only in the store now

        let graph_ptr = Box::into_raw(Box::new(RwLock::new(Graph::with_store(
            RedbStore::open(&store_path).expect("open store"),
        )))) as GraphPointer;

        unsafe {
            // In-memory-only lookups: these returned NULL for store-backed nodes, which killed
            // hover, documentation and source for the whole disk-backed index.
            let doc = rdx_graph_get_document(graph_ptr, CString::new("file:///foo.rb").unwrap().as_ptr());
            assert!(
                !doc.is_null(),
                "rdx_graph_get_document returned NULL for a store-backed file"
            );
            assert_eq!(*doc, uri_id.get());
            drop(Box::from_raw(doc.cast_mut()));

            let doc = crate::definition_api::rdx_definition_document(graph_ptr, def_id.get());
            assert!(
                !doc.is_null(),
                "rdx_definition_document returned NULL for a store-backed definition"
            );
            assert_eq!(*doc, uri_id.get());
            drop(Box::from_raw(doc.cast_mut()));

            let doc = crate::reference_api::rdx_constant_reference_document(graph_ptr, const_ref_id.get());
            assert!(
                !doc.is_null(),
                "rdx_constant_reference_document returned NULL for a store-backed reference"
            );
            drop(Box::from_raw(doc.cast_mut()));

            let doc = crate::reference_api::rdx_method_reference_document(graph_ptr, method_ref_id.get());
            assert!(
                !doc.is_null(),
                "rdx_method_reference_document returned NULL for a store-backed reference"
            );
            drop(Box::from_raw(doc.cast_mut()));
        };

        drop(unsafe { Box::from_raw(graph_ptr.cast::<RwLock<Graph>>()) });
    }

    #[test]
    fn names_are_untracked_after_resolving_constant() {
        let mut indexer = RubyIndexer::new(
            "file:///foo.rb".into(),
            "
            class Foo
              BAR = 1
            end
            ",
        );
        indexer.index();

        let mut graph = Graph::new();
        graph.consume_document_changes(indexer.local_graph());
        let mut resolver = Resolver::new(&mut graph);
        resolver.resolve();

        assert_eq!(
            1,
            graph
                .names()
                .iter()
                .find_map(|(_, name)| {
                    if graph.strings().get(name.str()).unwrap().as_str() == "BAR" {
                        Some(name)
                    } else {
                        None
                    }
                })
                .unwrap()
                .ref_count()
        );

        let graph_ptr = Box::into_raw(Box::new(RwLock::new(graph))) as GraphPointer;

        // Build the nesting array: ["Foo"] since BAR is inside class Foo
        let nesting_strings = [CString::new("Foo").unwrap()];
        let nesting_ptrs: Vec<*const c_char> = nesting_strings.iter().map(|s| s.as_ptr()).collect();

        unsafe {
            let decl = rdx_graph_resolve_constant(
                graph_ptr,
                CString::new("BAR").unwrap().as_ptr(),
                nesting_ptrs.as_ptr(),
                nesting_ptrs.len(),
            );
            assert!(!decl.is_null());
            assert_eq!((*decl).id(), *DeclarationId::from("Foo::BAR"));
            declaration_api::free_c_declaration(decl);
        };

        let graph = unsafe { Box::from_raw(graph_ptr.cast::<RwLock<Graph>>()) };
        let graph = graph.read().unwrap();

        assert_eq!(
            1,
            graph
                .names()
                .iter()
                .find_map(|(_, name)| {
                    if graph.strings().get(name.str()).unwrap().as_str() == "BAR" {
                        Some(name)
                    } else {
                        None
                    }
                })
                .unwrap()
                .ref_count()
        );
    }

    #[test]
    fn names_are_untracked_after_completion_error() {
        let mut indexer = RubyIndexer::new(
            "file:///foo.rb".into(),
            "
            class Foo
            end
            ",
        );
        indexer.index();

        let mut graph = Graph::new();
        graph.consume_document_changes(indexer.local_graph());
        let mut resolver = Resolver::new(&mut graph);
        resolver.resolve();

        let (foo_name_id, foo_string_id, name_ref_count, string_ref_count) = graph
            .names()
            .iter()
            .find_map(|(name_id, name)| {
                let string_id = *name.str();
                if graph.strings().get(&string_id).unwrap().as_str() == "Foo"
                    && name.parent_scope().is_none()
                    && name.nesting().is_none()
                {
                    Some((
                        *name_id,
                        string_id,
                        name.ref_count(),
                        graph.strings().get(&string_id).unwrap().ref_count(),
                    ))
                } else {
                    None
                }
            })
            .unwrap();
        let name_count = graph.names().len();
        let string_count = graph.strings().len();

        let graph_ptr = Box::into_raw(Box::new(RwLock::new(graph))) as GraphPointer;
        let nesting_strings = [CString::new("Foo").unwrap()];
        let nesting_ptrs: Vec<*const c_char> = nesting_strings.iter().map(|s| s.as_ptr()).collect();
        let self_receiver = CString::new("Nonexistent").unwrap();

        let result = unsafe {
            rdx_graph_complete_expression(
                graph_ptr,
                nesting_ptrs.as_ptr(),
                nesting_ptrs.len(),
                self_receiver.as_ptr(),
            )
        };
        assert!(result.candidates.is_null());
        assert!(!result.error.is_null());
        utils::free_c_string(result.error);

        let graph = unsafe { Box::from_raw(graph_ptr.cast::<RwLock<Graph>>()) };
        let graph = graph.read().unwrap();

        assert_eq!(name_count, graph.names().len());
        assert_eq!(string_count, graph.strings().len());
        assert_eq!(name_ref_count, graph.names().get(&foo_name_id).unwrap().ref_count());
        assert_eq!(
            string_ref_count,
            graph.strings().get(&foo_string_id).unwrap().ref_count()
        );
    }

    #[test]
    fn ffi_lookups_return_not_found_for_missing_or_stale_ids() {
        let mut graph = Graph::new();
        let mut indexer = RubyIndexer::new("file:///foo.rb".into(), "class Foo\n  def bar; end\nend\n");
        indexer.index();
        graph.consume_document_changes(indexer.local_graph());
        Resolver::new(&mut graph).resolve();

        let missing_decl_id = DeclarationId::from("Nonexistent").get();
        let non_alias_def_id = *graph.definitions().keys().next().expect("a definition");
        let graph_ptr = Box::into_raw(Box::new(RwLock::new(graph))) as GraphPointer;

        let member_name = CString::new("bar").unwrap();
        let found = unsafe {
            crate::declaration_api::rdx_declaration_find_member(graph_ptr, missing_decl_id, member_name.as_ptr(), false)
        };
        assert!(found.is_null());

        let alias_result =
            unsafe { crate::definition_api::rdx_method_alias_definition_target(graph_ptr, non_alias_def_id.get()) };
        assert!(matches!(
            alias_result.status,
            crate::definition_api::CMethodAliasResolution::NotFound
        ));
        assert!(alias_result.declaration.is_null());

        drop(unsafe { Box::from_raw(graph_ptr.cast::<RwLock<Graph>>()) });
    }
}
