//! C API for exposing references through ID handles (like definitions)

use std::ffi::CString;
use std::ptr;

use crate::declaration_api::CDeclaration;
use crate::graph_api::{GraphPointer, with_graph};
use crate::location_api::{Location, create_location_for_uri_and_offset};
use crate::utils::OwnedString;
use libc::c_char;
use rubydex::model::graph::Graph;
use rubydex::model::ids::{ConstantReferenceId, MethodReferenceId};
use rubydex::model::name::NameRef;

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct CConstantReference {
    pub id: u64,
    pub declaration_id: u64,
}

impl CConstantReference {
    /// Build a `CConstantReference` from a graph and reference ID. Sets `declaration_id` to 0 when the
    /// reference cannot be read or is unresolved.
    #[must_use]
    pub fn from_id(graph: &Graph, ref_id: ConstantReferenceId) -> Self {
        let declaration_id = graph
            .constant_reference(ref_id)
            .and_then(|reference| graph.name(*reference.name_id()))
            .and_then(|name_ref| match &*name_ref {
                NameRef::Resolved(resolved) => Some(**resolved.declaration_id()),
                NameRef::Unresolved(_) => None,
            })
            .unwrap_or(0);

        Self {
            id: *ref_id,
            declaration_id,
        }
    }
}

#[derive(Debug)]
pub struct ConstantReferencesIter {
    entries: Box<[CConstantReference]>,
    index: usize,
}

iterator!(ConstantReferencesIter, entries: CConstantReference);

/// # Safety
/// `iter` must be a valid pointer previously returned by `ConstantReferencesIter::new`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rdx_constant_references_iter_len(iter: *const ConstantReferencesIter) -> usize {
    unsafe { ConstantReferencesIter::len(iter) }
}

/// # Safety
/// - `iter` must be a valid pointer previously returned by `ConstantReferencesIter::new`.
/// - `out` must be a valid, writable pointer.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rdx_constant_references_iter_next(
    iter: *mut ConstantReferencesIter,
    out: *mut CConstantReference,
) -> bool {
    unsafe { ConstantReferencesIter::next(iter, out) }
}

/// # Safety
/// - `iter` must be a pointer previously returned by `ConstantReferencesIter::new`.
/// - `iter` must not be used after being freed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rdx_constant_references_iter_free(iter: *mut ConstantReferencesIter) {
    unsafe { ConstantReferencesIter::free(iter) }
}

pub const NO_METHOD_REFERENCE_NAME: u32 = u32::MAX;

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct CMethodReference {
    pub id: u64,
    pub name_index: u32,
}

#[derive(Debug)]
pub struct MethodReferencesIter {
    entries: Box<[CMethodReference]>,
    strings: Box<[Box<str>]>,
    epoch: u64,
    index: usize,
}

impl MethodReferencesIter {
    #[must_use]
    pub fn new(entries: Box<[CMethodReference]>) -> *mut Self {
        Self::with_names(entries, Box::default(), 0)
    }

    #[must_use]
    pub fn with_names(entries: Box<[CMethodReference]>, strings: Box<[Box<str>]>, epoch: u64) -> *mut Self {
        Box::into_raw(Box::new(Self {
            entries,
            strings,
            epoch,
            index: 0,
        }))
    }

    /// # Safety
    /// `iter` must be a valid pointer returned by `new`/`with_names`, or null.
    #[must_use]
    pub unsafe fn len(iter: *const Self) -> usize {
        if iter.is_null() {
            return 0;
        }
        unsafe { (&*iter).entries.len() }
    }

    /// # Safety
    /// - `iter` must be a valid pointer returned by `new`/`with_names`, or null.
    /// - `out` must be a valid, writable pointer, or null.
    pub unsafe fn next(iter: *mut Self, out: *mut CMethodReference) -> bool {
        if iter.is_null() || out.is_null() {
            return false;
        }
        let it = unsafe { &mut *iter };
        if it.index >= it.entries.len() {
            return false;
        }
        let entry = it.entries[it.index];
        it.index += 1;
        unsafe {
            *out = entry;
        }
        true
    }

    /// # Safety
    /// `iter` must be a valid pointer returned by `new`/`with_names`, or null.
    pub unsafe fn free(iter: *mut Self) {
        if !iter.is_null() {
            unsafe { drop(Box::from_raw(iter)) };
        }
    }
}

/// # Safety
/// `iter` must be a valid pointer previously returned by `MethodReferencesIter::new`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rdx_method_references_iter_len(iter: *const MethodReferencesIter) -> usize {
    unsafe { MethodReferencesIter::len(iter) }
}

/// Returns the number of deduplicated method names attached to `iter`.
///
/// # Safety
/// `iter` must be a valid pointer previously returned by `MethodReferencesIter::new`, or null.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rdx_method_references_iter_strings_len(iter: *const MethodReferencesIter) -> usize {
    if iter.is_null() {
        return 0;
    }
    unsafe { (&*iter).strings.len() }
}

/// Borrows the UTF-8 bytes of the `index`-th deduplicated method name on `iter` (valid until
/// `rdx_method_references_iter_free`), writing its byte length to `out_len`.
///
/// # Safety
/// - `iter` must be a valid pointer previously returned by `MethodReferencesIter::new`, or null.
/// - `out_len` must be a valid writable pointer.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rdx_method_references_iter_string(
    iter: *const MethodReferencesIter,
    index: usize,
    out_len: *mut usize,
) -> *const c_char {
    if iter.is_null() || out_len.is_null() {
        return ptr::null();
    }
    let strings = unsafe { &(&*iter).strings };
    let Some(string) = strings.get(index) else {
        return ptr::null();
    };
    unsafe {
        *out_len = string.len();
    }
    string.as_ptr().cast::<c_char>()
}

/// Returns the graph mutation epoch captured when `iter` was created.
///
/// # Safety
/// `iter` must be a valid pointer previously returned by `MethodReferencesIter::new`, or null.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rdx_method_references_iter_epoch(iter: *const MethodReferencesIter) -> u64 {
    if iter.is_null() {
        return 0;
    }
    unsafe { (&*iter).epoch }
}

/// # Safety
/// - `iter` must be a valid pointer previously returned by `MethodReferencesIter::new`.
/// - `out` must be a valid, writable pointer.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rdx_method_references_iter_next(
    iter: *mut MethodReferencesIter,
    out: *mut CMethodReference,
) -> bool {
    unsafe { MethodReferencesIter::next(iter, out) }
}

/// # Safety
/// - `iter` must be a pointer previously returned by `MethodReferencesIter::new`.
/// - `iter` must not be used after being freed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rdx_method_references_iter_free(iter: *mut MethodReferencesIter) {
    unsafe { MethodReferencesIter::free(iter) }
}

/// Returns the UTF-8 name string for a constant reference id, or NULL if the reference cannot be found.
/// Caller must free with `free_c_string`.
///
/// # Safety
///
/// Assumes pointer is valid.
///
/// # Panics
///
/// This function will panic if the reference's name cannot be found.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rdx_constant_reference_name(pointer: GraphPointer, reference_id: u64) -> *const c_char {
    with_graph(pointer, |graph| {
        let ref_id = ConstantReferenceId::new(reference_id);
        let Some(reference) = graph.constant_reference(ref_id) else {
            return ptr::null();
        };
        let Some(name) = graph.name(*reference.name_id()) else {
            return ptr::null();
        };
        let Some(name_string) = graph.string(*name.str()) else {
            return ptr::null();
        };
        CString::new(name_string.to_string()).map_or(ptr::null(), |s| s.into_raw().cast_const())
    })
}

/// Returns the UTF-8 name string for a method reference id, or a null string if the reference cannot be found.
/// Caller must free with `free_owned_string`.
///
/// # Safety
///
/// Assumes pointer is valid.
///
/// # Panics
///
/// This function will panic if the reference's name cannot be found.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rdx_method_reference_name(pointer: GraphPointer, reference_id: u64) -> OwnedString {
    with_graph(pointer, |graph| {
        let ref_id = MethodReferenceId::new(reference_id);
        let Some(reference) = graph.method_reference(ref_id) else {
            return OwnedString::null();
        };
        let Some(name) = graph.string(*reference.str()) else {
            return OwnedString::null();
        };
        OwnedString::new(name.as_str())
    })
}

/// Returns a newly allocated `Location` for the given constant reference id, or NULL if the reference cannot be found.
/// Caller must free the returned pointer with `rdx_location_free`.
///
/// # Safety
///
/// - `pointer` must be a valid pointer previously returned by `rdx_graph_new`.
/// - `reference_id` must be a valid reference id.
///
/// # Panics
///
/// This function will panic if the reference's document cannot be found.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rdx_constant_reference_location(pointer: GraphPointer, reference_id: u64) -> *mut Location {
    with_graph(pointer, |graph| {
        let ref_id = ConstantReferenceId::new(reference_id);
        let Some(reference) = graph.constant_reference(ref_id) else {
            return ptr::null_mut();
        };
        let Some(document) = graph.document(reference.uri_id()) else {
            return ptr::null_mut();
        };

        create_location_for_uri_and_offset(graph, &document, reference.offset())
    })
}

/// Returns the declaration that the given resolved constant reference points to. Returns NULL if the reference is
/// unresolved. Caller must free with `free_c_declaration`.
///
/// # Safety
///
/// Assumes pointer is valid.
///
/// # Panics
///
/// This function will panic if the reference cannot be found.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rdx_resolved_constant_reference_declaration(
    pointer: GraphPointer,
    reference_id: u64,
) -> *const CDeclaration {
    with_graph(pointer, |graph| {
        let ref_id = ConstantReferenceId::new(reference_id);
        let Some(reference) = graph.constant_reference(ref_id) else {
            return ptr::null();
        };
        let Some(name_ref) = graph.name(*reference.name_id()) else {
            return ptr::null();
        };

        match &*name_ref {
            NameRef::Resolved(resolved) => {
                let decl_id = *resolved.declaration_id();
                let Some(decl) = graph.declaration(decl_id) else {
                    return ptr::null();
                };
                Box::into_raw(Box::new(CDeclaration::from_declaration(decl_id, &decl))).cast_const()
            }
            NameRef::Unresolved(_) => ptr::null(),
        }
    })
}

/// Returns a pointer to the URI ID of the document a constant reference belongs
/// to, or NULL if the reference cannot be found. Caller must free the returned
/// pointer with `free_u64`.
///
/// # Safety
/// - `pointer` must be a valid pointer previously returned by `rdx_graph_new`.
/// - `reference_id` must be a valid reference id.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rdx_constant_reference_document(pointer: GraphPointer, reference_id: u64) -> *const u64 {
    with_graph(pointer, |graph| {
        let ref_id = ConstantReferenceId::new(reference_id);
        // Layered lookup: in disk mode the reference lives in the store, not the in-memory overlay.
        if let Some(reference) = graph.constant_reference(ref_id) {
            Box::into_raw(Box::new(*reference.uri_id())).cast_const()
        } else {
            ptr::null()
        }
    })
}

/// Returns the declaration of the resolved receiver for the given method reference. Returns NULL when the method
/// reference has no tracked receiver or when the receiver could not be resolved. Caller must free with
/// `free_c_declaration`.
///
/// # Safety
///
/// Assumes pointer is valid.
///
/// # Panics
///
/// This function will panic if the reference cannot be found.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rdx_method_reference_receiver_declaration(
    pointer: GraphPointer,
    reference_id: u64,
) -> *const CDeclaration {
    with_graph(pointer, |graph| {
        let ref_id = MethodReferenceId::new(reference_id);
        let Some(reference) = graph.method_reference(ref_id) else {
            return ptr::null();
        };

        let Some(name_id) = reference.receiver() else {
            return ptr::null();
        };

        let Some(name_ref) = graph.name(name_id) else {
            return ptr::null();
        };

        match &*name_ref {
            NameRef::Resolved(resolved) => {
                let decl_id = *resolved.declaration_id();
                let Some(decl) = graph.declaration(decl_id) else {
                    return ptr::null();
                };
                Box::into_raw(Box::new(CDeclaration::from_declaration(decl_id, &decl))).cast_const()
            }
            NameRef::Unresolved(_) => ptr::null(),
        }
    })
}

/// Returns a newly allocated `Location` for the given method reference id, or NULL if the reference cannot be found.
/// Caller must free the returned pointer with `rdx_location_free`.
///
/// # Safety
///
/// - `pointer` must be a valid pointer previously returned by `rdx_graph_new`.
/// - `reference_id` must be a valid reference id.
///
/// # Panics
///
/// This function will panic if the reference's document cannot be found.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rdx_method_reference_location(pointer: GraphPointer, reference_id: u64) -> *mut Location {
    with_graph(pointer, |graph| {
        let ref_id = MethodReferenceId::new(reference_id);
        let Some(reference) = graph.method_reference(ref_id) else {
            return ptr::null_mut();
        };
        let Some(document) = graph.document(reference.uri_id()) else {
            return ptr::null_mut();
        };

        create_location_for_uri_and_offset(graph, &document, reference.offset())
    })
}

/// Returns a pointer to the URI ID of the document a method reference belongs
/// to, or NULL if the reference cannot be found. Caller must free the returned
/// pointer with `free_u64`.
///
/// # Safety
/// - `pointer` must be a valid pointer previously returned by `rdx_graph_new`.
/// - `reference_id` must be a valid reference id.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rdx_method_reference_document(pointer: GraphPointer, reference_id: u64) -> *const u64 {
    with_graph(pointer, |graph| {
        let ref_id = MethodReferenceId::new(reference_id);
        // Layered lookup: in disk mode the reference lives in the store, not the in-memory overlay.
        if let Some(reference) = graph.method_reference(ref_id) {
            Box::into_raw(Box::new(*reference.uri_id())).cast_const()
        } else {
            ptr::null()
        }
    })
}

/// Frees a `CConstantReference` previously returned by an FFI function.
///
/// # Safety
/// - `ptr` must be a valid pointer previously returned by an FFI function that allocates a `CConstantReference`, or
///   NULL.
/// - `ptr` must not be used after being freed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn free_c_constant_reference(ptr: *const CConstantReference) {
    if !ptr.is_null() {
        unsafe {
            let _ = Box::from_raw(ptr.cast_mut());
        }
    }
}
