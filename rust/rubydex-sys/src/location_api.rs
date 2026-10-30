//! Location-related C API and structs

use libc::c_char;
#[cfg(feature = "redb-store")]
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

// ponytail: per-thread, unbounded memo of rebuilt LineIndexes keyed by document URI and content
// hash. A hover that computes several locations re-reads the file and reparses it on every call
// without this; with it, the file read + LineIndex::new happen once per document version per thread
// (LineIndex is Rc-based, so the cache hit is a cheap clone). The content hash is the version: after
// a store rebuild or a live edit the document has a new hash and the old index is not served.
// Unbounded — fine for the opt-in disk path; add an LRU bound if a workspace with many touched files
// ever makes it matter.
#[cfg(feature = "redb-store")]
thread_local! {
    static REBUILT_LINE_INDEXES: std::cell::RefCell<
        std::collections::HashMap<(String, u64), (LineIndex, usize)>,
    > = std::cell::RefCell::new(std::collections::HashMap::new());
}

/// Returns the rebuilt `(LineIndex, source_len)` for a store-loaded document, serving from the
/// thread-local memo on hit and reading+parsing from disk on miss. Returns `None` if the source can
/// not be read.
#[cfg(feature = "redb-store")]
fn rebuilt_line_index(document: &Document) -> Option<(LineIndex, usize)> {
    let key = (document.uri().to_string(), document.content_hash());
    if let Some((index, len)) = REBUILT_LINE_INDEXES.with(|cache| cache.borrow().get(&key).cloned()) {
        return Some((index, len));
    }
    let source = read_source(&key.0)?;
    let len = source.len();
    let index = LineIndex::new(&source);
    REBUILT_LINE_INDEXES.with(|cache| cache.borrow_mut().insert(key, (index.clone(), len)));
    Some((index, len))
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
    // Store-loaded documents carry the empty placeholder `LineIndex` (source is discarded on
    // serialize); only for those, rebuild the index from the file on disk. Live documents keep their
    // own index, which reflects the in-memory (possibly unsaved) source — rebuilding from disk would
    // shift every reported position. Offsets are always clamped to the length of the index in use so
    // a stale offset degrades to a clamped position instead of panicking across the FFI boundary.
    #[cfg(feature = "redb-store")]
    let (line_index, max_offset): (LineIndex, u32) = if u32::from(document.line_index().len()) == 0 {
        match rebuilt_line_index(document) {
            Some((index, len)) => (index, u32::try_from(len).unwrap_or(u32::MAX)),
            // Unreadable source: clamp to 0 on the empty index — `line_col(0)` is valid (0:0).
            None => (document.line_index().clone(), 0),
        }
    } else {
        let len = u32::from(document.line_index().len());
        (document.line_index().clone(), len)
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

#[cfg(all(test, feature = "redb-store"))]
mod tests {
    use super::*;

    #[test]
    fn rebuilt_line_index_tracks_the_document_version() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("a.rb");
        let uri = url::Url::from_file_path(&path).expect("file url").to_string();

        std::fs::write(&path, "class A\nend\n").expect("write");
        let first = Document::new(uri.clone().into_boxed_str(), "class A\nend\n");
        let (first_index, _) = rebuilt_line_index(&first).expect("first index");
        assert_eq!(first_index.line_col(0.into()).line, 0);

        std::fs::write(&path, "\n\n\nclass A\nend\n").expect("rewrite");
        let second = Document::new(uri.into_boxed_str(), "\n\n\nclass A\nend\n");
        let (second_index, _) = rebuilt_line_index(&second).expect("second index");
        assert_eq!(
            second_index.line_col(3.into()).line,
            3,
            "a new content hash must not reuse the index from the previous version"
        );
    }
}
