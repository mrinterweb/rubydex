use crate::assert_mem_size;
use crate::offset::Offset;

#[derive(Debug, Clone)]
#[cfg_attr(feature = "redb-store", derive(serde::Serialize, serde::Deserialize))]
pub struct Comment {
    offset: Offset,
    // The text is a copy of a byte range the source already holds, so a store-backed comment keeps
    // only the pointer (`offset`) and re-reads the text from disk on demand. `Option<Box<str>>`
    // keeps the struct at 24 bytes through the null-pointer niche.
    #[cfg_attr(feature = "redb-store", serde(skip))]
    string: Option<Box<str>>,
}
assert_mem_size!(Comment, 24);

impl Comment {
    #[must_use]
    pub fn new(offset: Offset, string: Box<str>) -> Self {
        Self {
            offset,
            string: Some(string),
        }
    }

    #[must_use]
    pub fn offset(&self) -> &Offset {
        &self.offset
    }

    /// The comment text held in memory. Empty for a comment loaded from the store, which keeps the
    /// offset only — use [`Self::resolve_text`] there.
    #[must_use]
    pub fn string(&self) -> &str {
        self.string.as_deref().unwrap_or("")
    }

    /// Whether this comment carries its text in memory, as opposed to being a store-loaded pointer.
    #[must_use]
    pub fn has_inline_text(&self) -> bool {
        self.string.is_some()
    }

    /// The comment's text length in bytes, without materializing a store-loaded comment's text.
    #[must_use]
    pub fn byte_len(&self) -> usize {
        self.string.as_deref().map_or_else(
            || self.offset.end().saturating_sub(self.offset.start()) as usize,
            str::len,
        )
    }

    /// The comment's text, preferring the in-memory copy and falling back to slicing the source at
    /// this comment's offset. Returns `""` when the source is unavailable or the offset does not
    /// land on character boundaries (the file changed since the store was built, so the offsets are
    /// stale and any slice would be wrong or panic).
    #[must_use]
    pub fn resolve_text<'a>(&'a self, source: Option<&'a str>) -> &'a str {
        if let Some(inline) = self.string.as_deref() {
            return inline;
        }
        let Some(src) = source else {
            return "";
        };
        let start = self.offset.start() as usize;
        let end = self.offset.end() as usize;
        if start <= end && end <= src.len() && src.is_char_boundary(start) && src.is_char_boundary(end) {
            src[start..end].trim()
        } else {
            ""
        }
    }
}
