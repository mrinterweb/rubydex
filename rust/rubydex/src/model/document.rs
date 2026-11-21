use std::borrow::Cow;
use std::path::PathBuf;

use line_index::LineIndex;
use url::Url;
use xxhash_rust::xxh3::xxh3_64;

use crate::assert_mem_size;
use crate::diagnostic::Diagnostic;
use crate::model::ids::{ConstantReferenceId, DefinitionId, MethodReferenceId};

// Represents a document currently loaded into memory. Identified by its unique URI, it holds the edges to all
// definitions and references discovered in it
#[derive(Debug)]
#[cfg_attr(feature = "redb-store", derive(serde::Serialize, serde::Deserialize))]
pub struct Document {
    uri: Box<str>,
    // The source is not retained in the document, so rebuild an empty index when loading from disk.
    #[cfg_attr(feature = "redb-store", serde(skip, default = "empty_line_index"))]
    line_index: LineIndex,
    definition_ids: Vec<DefinitionId>,
    method_reference_ids: Vec<MethodReferenceId>,
    constant_reference_ids: Vec<ConstantReferenceId>,
    diagnostics: Vec<Diagnostic>,
    content_hash: u64,
}
#[cfg(feature = "redb-store")]
fn empty_line_index() -> LineIndex {
    LineIndex::new("")
}

// shortcut: per-thread, unbounded memo of rebuilt LineIndexes keyed by document URI and content
// hash. Hover and Cypher queries that resolve many locations in the same store-loaded document
// would otherwise re-read and re-index the file on every call; add an LRU bound if a long-lived
// session touching many files makes memory growth matter.
#[cfg(feature = "redb-store")]
thread_local! {
    static REBUILT_LINE_INDEXES: std::cell::RefCell<
        std::collections::HashMap<(String, u64), LineIndex>,
    > = std::cell::RefCell::new(std::collections::HashMap::new());
}

#[cfg(feature = "redb-store")]
fn rebuilt_line_index(document: &Document) -> Option<LineIndex> {
    let key = (document.uri().to_string(), document.content_hash());
    if let Some(index) = REBUILT_LINE_INDEXES.with(|cache| cache.borrow().get(&key).cloned()) {
        return Some(index);
    }
    let path = document.file_path()?;
    let source = std::fs::read_to_string(path).ok()?;
    let index = LineIndex::new(&source);
    REBUILT_LINE_INDEXES.with(|cache| cache.borrow_mut().insert(key, index.clone()));
    Some(index)
}
assert_mem_size!(Document, 176);

impl Document {
    #[must_use]
    pub fn new(uri: Box<str>, source: &str) -> Self {
        Self {
            uri,
            line_index: LineIndex::new(source),
            definition_ids: Vec::new(),
            method_reference_ids: Vec::new(),
            constant_reference_ids: Vec::new(),
            diagnostics: Vec::new(),
            content_hash: xxh3_64(source.as_bytes()),
        }
    }

    #[must_use]
    pub fn uri(&self) -> &str {
        &self.uri
    }

    #[must_use]
    pub fn content_hash(&self) -> u64 {
        self.content_hash
    }

    #[must_use]
    pub fn line_index(&self) -> &LineIndex {
        &self.line_index
    }

    /// The line index to use for position lookups.
    ///
    /// A store-loaded document persists no source, so its index is the empty placeholder. Rebuilding it
    /// from the file on disk is the only way to answer positions for it, and it is confined to those
    /// documents: a live document's index reflects its in-memory (possibly unsaved) source, which a disk
    /// read would shift. Falls back to the document's own index when the source cannot be read back, so
    /// callers see the empty index rather than an error.
    #[must_use]
    pub fn line_index_for_positions(&self) -> Cow<'_, LineIndex> {
        #[cfg(feature = "redb-store")]
        if u32::from(self.line_index.len()) == 0
            && let Some(index) = rebuilt_line_index(self)
        {
            return Cow::Owned(index);
        }
        Cow::Borrowed(&self.line_index)
    }

    #[must_use]
    pub fn definitions(&self) -> &[DefinitionId] {
        &self.definition_ids
    }

    pub fn add_definition(&mut self, definition_id: DefinitionId) {
        debug_assert!(
            !self.definition_ids.contains(&definition_id),
            "Cannot add the same exact definition to a document twice. Duplicate definition IDs"
        );

        self.definition_ids.push(definition_id);
    }

    #[must_use]
    pub fn method_references(&self) -> &[MethodReferenceId] {
        &self.method_reference_ids
    }

    pub fn add_method_reference(&mut self, reference_id: MethodReferenceId) {
        self.method_reference_ids.push(reference_id);
    }

    #[must_use]
    pub fn constant_references(&self) -> &[ConstantReferenceId] {
        &self.constant_reference_ids
    }

    pub fn add_constant_reference(&mut self, reference_id: ConstantReferenceId) {
        self.constant_reference_ids.push(reference_id);
    }

    #[must_use]
    pub fn diagnostics(&self) -> &[Diagnostic] {
        &self.diagnostics
    }

    pub fn add_diagnostic(&mut self, diagnostic: Diagnostic) {
        self.diagnostics.push(diagnostic);
    }

    /// The file-system path of this document, decoded from its URI.
    ///
    /// Returns `None` when the URI is not a `file://` URL (e.g. the synthetic built-in document) or
    /// cannot be converted to a path. Uses `Url` so percent-encoding and platform-specific paths
    /// (including Windows drive paths) are handled correctly.
    #[must_use]
    pub fn file_path(&self) -> Option<PathBuf> {
        let url = Url::parse(&self.uri).ok()?;
        if url.scheme() != "file" {
            return None;
        }
        url.to_file_path().ok()
    }

    /// The base file name of this document (the last path segment), decoded from its URI.
    ///
    /// Prefers the platform file path, but falls back to the last URL path segment so it still works
    /// for `file://` URIs that don't convert to a local path on the current platform (e.g. a
    /// drive-less path like `file:///foo.rb` on Windows). Returns `None` only when the URI has no
    /// usable path segment (e.g. the synthetic built-in document).
    #[must_use]
    pub fn file_name(&self) -> Option<String> {
        if let Some(path) = self.file_path()
            && let Some(name) = path.file_name()
        {
            return Some(name.to_string_lossy().into_owned());
        }

        let url = Url::parse(&self.uri).ok()?;
        let segment = url.path_segments()?.rfind(|segment| !segment.is_empty())?;
        Some(segment.to_string())
    }

    /// Computes the require path for this document given load paths.
    ///
    /// Returns `None` if:
    /// - URI is not `file://` scheme
    /// - URI doesn't end with `.rb`
    /// - File path doesn't match any load path
    /// - `Url::to_file_path()` fails
    ///
    /// # Panics
    ///
    /// Panics if load path entries exceed u16.
    #[must_use]
    pub fn require_path(&self, load_paths: &[PathBuf]) -> Option<(String, u16)> {
        require_path_for_uri(&self.uri, load_paths)
    }
}

/// Converts an absolute file path into the `file://` URI form the graph stores documents under.
/// This is the same conversion the indexer uses, so a path addressed from the Ruby side resolves to
/// the same document key; re-implementing it in Ruby would drift on escaping.
#[must_use]
pub fn path_to_uri(path: &str) -> Option<String> {
    Url::from_file_path(PathBuf::from(&path))
        .map(|uri| uri.to_string())
        .ok()
}

/// Computes the require path for a document URI given load paths.
///
/// Shared by in-memory documents and store-backed documents, which are enumerated by URI alone
/// without deserializing the full document node.
///
/// Returns `None` if:
/// - URI is not `file://` scheme
/// - URI doesn't end with `.rb`
/// - File path doesn't match any load path
/// - `Url::to_file_path()` fails
///
/// # Panics
///
/// Panics if load path entries exceed u16.
#[must_use]
pub fn require_path_for_uri(uri: &str, load_paths: &[PathBuf]) -> Option<(String, u16)> {
    let url = Url::parse(uri).ok()?;
    if url.scheme() != "file" {
        return None;
    }
    let file_path = url.to_file_path().ok()?;
    if file_path.extension().is_none_or(|ext| ext != "rb") {
        return None;
    }

    for (load_path_index, load_path) in load_paths.iter().enumerate() {
        if let Ok(relative) = file_path.strip_prefix(load_path) {
            let file_path = relative
                .components()
                .filter_map(|c| c.as_os_str().to_str())
                .collect::<Vec<_>>()
                .join("/");

            let require_path = file_path.trim_end_matches(".rb").to_string();
            return Some((
                require_path,
                load_path_index.try_into().expect("Load path entries exceed u16"),
            ));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use super::*;

    fn test_root() -> PathBuf {
        let root = if cfg!(windows) { "C:\\" } else { "/" };
        PathBuf::from_str(root).unwrap()
    }

    #[test]
    #[should_panic(expected = "Cannot add the same exact definition to a document twice. Duplicate definition IDs")]
    fn inserting_duplicate_definitions() {
        let mut document = Document::new("file:///foo.rb".into(), "class Foo; end");
        let def_id = DefinitionId::new(123);

        document.add_definition(def_id);
        document.add_definition(def_id);

        assert_eq!(document.definitions().len(), 1);
    }

    #[test]
    fn tracking_references() {
        let mut document = Document::new("file:///foo.rb".into(), "class Foo; end");
        let method_ref = MethodReferenceId::new(1);
        let constant_ref = ConstantReferenceId::new(2);

        document.add_method_reference(method_ref);
        document.add_constant_reference(constant_ref);

        assert_eq!(document.method_references(), &[method_ref]);
        assert_eq!(document.constant_references(), &[constant_ref]);
    }

    #[test]
    fn require_path() {
        let root = test_root();
        let path = root.join("lib").join("foo").join("bar.rb");
        let uri = Url::from_file_path(path).unwrap().to_string().into();
        let load_paths = [root.join("lib")];

        let document = Document::new(uri, "");
        assert_eq!(Some(("foo/bar".to_string(), 0)), document.require_path(&load_paths));
    }

    #[test]
    fn require_path_first_load_path_wins() {
        let root = test_root();
        let path = root.join("a").join("b").join("foo.rb");
        let uri = Url::from_file_path(path).unwrap().to_string().into();
        let load_path_a = root.join("a");
        let load_path_ab = root.join("a").join("b");
        let load_paths = [load_path_a, load_path_ab];

        let document = Document::new(uri, "");
        assert_eq!(Some(("b/foo".to_string(), 0)), document.require_path(&load_paths));
    }

    #[test]
    fn require_path_returns_none() {
        let root = test_root();
        let load_paths = [root.join("lib")];

        // non-file URI
        let document = Document::new("untitled:Untitled-1".into(), "");
        assert!(document.require_path(&load_paths).is_none());

        // non-.rb extension
        let path = root.join("lib").join("foo.so");
        let uri = Url::from_file_path(path).unwrap().to_string().into();
        let document = Document::new(uri, "");
        assert!(document.require_path(&load_paths).is_none());

        // /libfoo is not under /lib - should not match due to partial directory name
        let path = root.join("libfoo").join("bar.rb");
        let uri = Url::from_file_path(path).unwrap().to_string().into();
        let document = Document::new(uri, "");
        assert!(document.require_path(&load_paths).is_none());
    }

    #[test]
    fn require_path_with_spaces() {
        let root = test_root();
        let path = root.join("lib").join("space foo").join("bar.rb");
        let uri = Url::from_file_path(path).unwrap().to_string().into();
        let load_paths = [root.join("lib")];

        let document = Document::new(uri, "");
        assert_eq!(
            Some(("space foo/bar".to_string(), 0)),
            document.require_path(&load_paths)
        );
    }

    #[cfg(feature = "redb-store")]
    #[test]
    fn line_index_for_positions_caches_per_document_version() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("a.rb");
        let uri = Url::from_file_path(&path).expect("file url").to_string();

        std::fs::write(&path, "class A\nend\n").expect("write");
        let mut first = Document::new(uri.clone().into_boxed_str(), "class A\nend\n");
        first.line_index = empty_line_index();
        assert_eq!(first.line_index_for_positions().line_col(0.into()).line, 0);

        // Removing the file after the first lookup proves the second call hits the memo instead of
        // re-reading disk on every location lookup in the same document version.
        std::fs::remove_file(&path).expect("remove");
        assert_eq!(
            first.line_index_for_positions().line_col(8.into()).line,
            1,
            "repeated lookups for the same version must hit the memo"
        );

        std::fs::write(&path, "\n\n\nclass A\nend\n").expect("rewrite");
        let mut second = Document::new(uri.into_boxed_str(), "\n\n\nclass A\nend\n");
        second.line_index = empty_line_index();
        assert_eq!(
            second.line_index_for_positions().line_col(3.into()).line,
            3,
            "a new content hash must not reuse the index from the previous version"
        );
    }
}
