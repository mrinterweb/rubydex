//! Location-related C API and structs

use libc::c_char;
use rubydex::model::document::Document;
use rubydex::model::graph::Graph;
use rubydex::offset::Offset;
use std::ffi::CString;
use std::ptr;

/// C-compatible struct representing a definition location with offsets and line/column positions.
#[repr(C)]
#[derive(Debug, Clone)]
pub struct Location {
    pub uri: *const c_char,
    pub start_line: u32,
    pub end_line: u32,
    pub start_column: u32,
    pub end_column: u32,
}

/// Helper to create a location for a given URI and byte-offset range.
/// Allocates and returns a pointer to `Location`. Caller must free with `rdx_location_free`.
///
/// # Panics
///
/// - If the URI cannot be converted to a file path.
/// - If the file cannot be read.
/// - If the offset cannot be converted to a position.
#[must_use]
pub(crate) fn create_location_for_uri_and_offset(graph: &Graph, document: &Document, offset: &Offset) -> *mut Location {
    // Reuses `Document::line_index_for_positions` (which caches rebuilt indexes for store-loaded
    // documents) and clamps offsets to the index length so a stale offset degrades instead of
    // panicking across the FFI boundary.
    let line_index = document.line_index_for_positions();
    let max_offset = u32::from(line_index.len());

    let start_pos = line_index.line_col(offset.start().min(max_offset).into());
    let end_pos = line_index.line_col(offset.end().min(max_offset).into());

    // A URI from the store is untrusted: one with an interior NUL cannot be a C string, and the
    // resulting location is reported as absent rather than taking the host process down with a panic
    // that has no way back across the boundary.
    let Ok(uri) = CString::new(document.uri()) else {
        return ptr::null_mut();
    };
    let uri = uri.into_raw().cast_const();

    let loc = if let Some(wide_encoding) = graph.encoding().to_wide() {
        let wide_start_pos = line_index.to_wide(wide_encoding, start_pos).unwrap();
        let wide_end_pos = line_index.to_wide(wide_encoding, end_pos).unwrap();

        Location {
            uri,
            start_line: wide_start_pos.line,
            end_line: wide_end_pos.line,
            start_column: wide_start_pos.col,
            end_column: wide_end_pos.col,
        }
    } else {
        Location {
            uri,
            start_line: start_pos.line,
            end_line: end_pos.line,
            start_column: start_pos.col,
            end_column: end_pos.col,
        }
    };

    Box::into_raw(Box::new(loc))
}

/// Frees a `Location` struct and its owned inner strings.
///
/// # Safety
///
/// - `ptr` must be a valid pointer previously returned by `create_location_for_uri_and_offset`.
/// - `ptr` must not be used after being freed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rdx_location_free(ptr: *mut Location) {
    if ptr.is_null() {
        return;
    }
    unsafe {
        // Take ownership of the box so we can free inner allocations first
        let boxed = Box::from_raw(ptr);

        if !boxed.uri.is_null() {
            let _ = CString::from_raw(boxed.uri.cast_mut());
        }

        // Box drops here, freeing the struct memory
    }
}
