//! This file provides the C API for Document accessors

use libc::c_char;
use std::ffi::CString;
use std::ptr;

use crate::definition_api::{DefinitionsIter, rdx_definitions_iter_new_from_ids};
use crate::graph_api::{GraphPointer, with_graph};
use crate::reference_api::{CMethodReference, MethodReferencesIter, NO_METHOD_REFERENCE_NAME};
use rubydex::model::ids::UriId;

#[derive(Debug)]
pub struct DocumentsIter {
    entries: Box<[u64]>,
    index: usize,
}

iterator!(DocumentsIter, entries: u64);

/// # Safety
/// `iter` must be a valid pointer previously returned by `DocumentsIter::new`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rdx_graph_documents_iter_len(iter: *const DocumentsIter) -> usize {
    unsafe { DocumentsIter::len(iter) }
}

/// # Safety
/// - `iter` must be a valid pointer previously returned by `DocumentsIter::new`.
/// - `out` must be a valid, writable pointer.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rdx_graph_documents_iter_next(iter: *mut DocumentsIter, out: *mut u64) -> bool {
    unsafe { DocumentsIter::next(iter, out) }
}

/// # Safety
/// - `iter` must be a pointer previously returned by `DocumentsIter::new`.
/// - `iter` must not be used after being freed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rdx_graph_documents_iter_free(iter: *mut DocumentsIter) {
    unsafe { DocumentsIter::free(iter) }
}

/// Returns the UTF-8 URI string for a document id.
/// Caller must free with `free_c_string`.
///
/// # Safety
///
/// Assumes pointer is valid.
///
/// # Panics
///
/// This function will panic if the URI pointer is invalid.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rdx_document_uri(pointer: GraphPointer, uri_id: u64) -> *const c_char {
    with_graph(pointer, |graph| {
        let uri_id = UriId::new(uri_id);
        // A URI read back from a store is untrusted: an interior NUL has no C string form, so the
        // caller gets null (and a Ruby nil) instead of a panic across the boundary.
        graph.document(uri_id).map_or(ptr::null(), |doc| {
            CString::new(doc.uri()).map_or(ptr::null(), |uri| uri.into_raw().cast_const())
        })
    })
}

/// An iterator over definition IDs and kinds for a given document (URI)
///
/// We snapshot the IDs at iterator creation so if the graph is modified, the iterator will not see the changes
// Use shared DefinitionsIter directly in signatures
/// Creates a new iterator over definition IDs for a given document by snapshotting the current set of IDs.
///
/// # Safety
///
/// - `pointer` must be a valid `GraphPointer` previously returned by this crate.
/// - The returned pointer must be freed with `rdx_document_definitions_iter_free`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rdx_document_definitions_iter_new(pointer: GraphPointer, uri_id: u64) -> *mut DefinitionsIter {
    // Snapshot the IDs and kinds at iterator creation to avoid borrowing across FFI calls
    with_graph(pointer, |graph| {
        let uri_id = UriId::new(uri_id);
        if let Some(doc) = graph.document(uri_id) {
            rdx_definitions_iter_new_from_ids(graph, doc.definitions())
        } else {
            DefinitionsIter::new(Vec::<_>::new().into_boxed_slice())
        }
    })
}

/// Returns the number of definitions recorded for a document, without materializing them. Counting
/// through the iterator would decode every definition of the document from the store first.
///
/// # Safety
///
/// - `pointer` must be a valid `GraphPointer` previously returned by this crate.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rdx_document_definitions_len(pointer: GraphPointer, uri_id: u64) -> usize {
    with_graph(pointer, |graph| {
        graph
            .document(UriId::new(uri_id))
            .map_or(0, |document| document.definitions().len())
    })
}

/// Creates a new iterator over method reference IDs for a given document by snapshotting the current set of IDs.
///
/// # Safety
///
/// - `pointer` must be a valid `GraphPointer` previously returned by this crate.
/// - The returned pointer must be freed with `rdx_method_references_iter_free`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rdx_document_method_references_iter_new(
    pointer: GraphPointer,
    uri_id: u64,
) -> *mut MethodReferencesIter {
    with_graph(pointer, |graph| {
        let uri_id = UriId::new(uri_id);
        // Layered lookup: an unedited store-backed document is not in the overlay, so reading only
        // the overlay map made every store document report zero method references.
        if let Some(doc) = graph.document(uri_id) {
            let entries: Vec<_> = doc
                .method_references()
                .iter()
                .map(|ref_id| CMethodReference {
                    id: **ref_id,
                    name_index: NO_METHOD_REFERENCE_NAME,
                })
                .collect();

            MethodReferencesIter::new(entries.into_boxed_slice())
        } else {
            MethodReferencesIter::new(Vec::<_>::new().into_boxed_slice())
        }
    })
}
