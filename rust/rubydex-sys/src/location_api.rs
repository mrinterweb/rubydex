//! Location-related C API and structs

use libc::c_char;
use line_index::LineIndex;
use rubydex::model::document::Document;
use rubydex::model::graph::Graph;
use rubydex::offset::Offset;
use std::ffi::CString;

/// Reads a document's source from its `file://` URI. Used to rebuild a `LineIndex` for store-loaded
/// documents, which don't retain source (so their in-memory `LineIndex` is empty).
#[cfg(feature = "redb-store")]
fn read_source(uri: &str) -> Option<String> {
    let url = url::Url::parse(uri).ok()?;
    let path = url.to_file_path().ok()?;
    std::fs::read_to_string(path).ok()
}

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
    // Store-loaded documents have an empty `LineIndex` (source is discarded on serialize), so rebuild
    // it from the file and clamp offsets to avoid the "invalid offset" panic. Falls back to the
    // document's own index (and empty source) if the file can't be read.
    #[cfg(feature = "redb-store")]
    let rebuilt = read_source(document.uri()).map(|source| (LineIndex::new(&source), source.len()));
    #[cfg(feature = "redb-store")]
    let (line_index, max_offset) = match &rebuilt {
        Some((index, len)) => (index, u32::try_from(*len).unwrap_or(u32::MAX)),
        None => (document.line_index(), u32::MAX),
    };
    #[cfg(not(feature = "redb-store"))]
    let (line_index, max_offset) = (document.line_index(), u32::MAX);

    let start_pos = line_index.line_col(offset.start().min(max_offset).into());
    let end_pos = line_index.line_col(offset.end().min(max_offset).into());

    let loc = if let Some(wide_encoding) = graph.encoding().to_wide() {
        let wide_start_pos = line_index.to_wide(wide_encoding, start_pos).unwrap();
        let wide_end_pos = line_index.to_wide(wide_encoding, end_pos).unwrap();

        Location {
            uri: CString::new(document.uri()).unwrap().into_raw().cast_const(),
            start_line: wide_start_pos.line,
            end_line: wide_end_pos.line,
            start_column: wide_start_pos.col,
            end_column: wide_end_pos.col,
        }
    } else {
        Location {
            uri: CString::new(document.uri()).unwrap().into_raw().cast_const(),
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
