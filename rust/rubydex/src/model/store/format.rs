//! Binary wire formats packed into `redb` rows: front-coded string/id blocks and hash-prefix
//! buckets.
//!
//! Kept free of `redb` types so every encoder, decoder, and bounds check can be tested directly
//! against byte slices.

use super::StoreError;

/// How many contiguous blocks `SEARCH_NAMES` is split into. A fixed count (rather than one shard
/// per thread) keeps the store layout independent of the machine that built it.
pub(super) const SEARCH_SHARD_COUNT: usize = 32;

/// 65,536 buckets is far past the point where a bucket is one page; the cap only keeps `1 << bits`
/// and the shift arithmetic bounded.
pub(super) const MAX_BUCKET_BITS: u8 = 16;

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
pub(super) fn encode_front_coded(entries: &[(&[u8], u64)], out: &mut Vec<u8>) -> Result<(), StoreError> {
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
/// validated as `UTF-8`. A bad block returns [`StoreError::CorruptBlock`], which the caller records
/// and degrades on rather than aborting the host process.
pub(super) fn decode_front_coded(
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
pub(super) fn encode_search_shards(entries: &[(&[u8], u64)]) -> Result<Vec<Vec<u8>>, StoreError> {
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

// ---------------------------------------------------------------------------------------------
// Hash-prefix buckets
//
// A node table row is a *bucket*: the nodes whose ids share the top `bits` bits, keyed by that
// prefix. Wire format:
//
//   [count:   u32 LE]
//   [keys:    count x u64 LE, ascending]        (8 * count bytes)
//   [offsets: (count + 1) x u32 LE]             (byte offsets into the payload)
//   [payload: offsets[count] bytes]             (concatenated postcard-encoded values)
//
// A point read is then: one redb get, one binary search, one postcard decode. `offsets` is u32
// rather than u16 because a single bucket holds every node when `bits == 0` (small tables, and the
// test-only `RedbStore::create`), where 65,535 entries is reachable.
// ---------------------------------------------------------------------------------------------

/// The bucket count for a table of `count` nodes, as a bit count: `1 << bits` buckets.
#[must_use]
pub(super) fn bucket_bits_for_count(count: usize, target_per_bucket: usize) -> u8 {
    if count <= target_per_bucket {
        return 0;
    }
    let buckets = count.div_ceil(target_per_bucket).next_power_of_two();
    u8::try_from(buckets.trailing_zeros())
        .unwrap_or(MAX_BUCKET_BITS)
        .min(MAX_BUCKET_BITS)
}

/// The bucket an id belongs to: its top `bits` bits.
///
/// `bits == 0` must be handled before shifting: shifting a `u64` right by 64 panics in debug builds
/// and wraps in release builds.
#[must_use]
pub(super) fn bucket_id(key: u64, bits: u8) -> u32 {
    if bits == 0 {
        return 0;
    }
    u32::try_from(key >> (64 - u32::from(bits))).unwrap_or(u32::MAX)
}

/// The byte ranges of one bucket's header, validated against the block's length.
struct BucketView<'a> {
    count: usize,
    keys: &'a [u8],
    offsets: &'a [u8],
    payload: &'a [u8],
}

impl<'a> BucketView<'a> {
    fn new(table: &'static str, key: u32, bytes: &'a [u8]) -> Result<Self, StoreError> {
        let error = |reason| StoreError::CorruptBlock { table, key, reason };
        let count_bytes: [u8; 4] = bytes
            .get(0..4)
            .ok_or_else(|| error("truncated bucket header"))?
            .try_into()
            .unwrap();
        let count = usize::try_from(u32::from_le_bytes(count_bytes))
            .map_err(|_| error("bucket entry count does not fit in usize"))?;
        let keys_end = 4usize
            .checked_add(count.checked_mul(8).ok_or_else(|| error("bucket key count overflow"))?)
            .ok_or_else(|| error("bucket key count overflow"))?;
        let offsets_end = keys_end
            .checked_add(
                count
                    .checked_add(1)
                    .ok_or_else(|| error("bucket offset count overflow"))?
                    .checked_mul(4)
                    .ok_or_else(|| error("bucket offset count overflow"))?,
            )
            .ok_or_else(|| error("bucket offset count overflow"))?;
        Ok(Self {
            count,
            keys: bytes.get(4..keys_end).ok_or_else(|| error("truncated bucket keys"))?,
            offsets: bytes
                .get(keys_end..offsets_end)
                .ok_or_else(|| error("truncated bucket offsets"))?,
            payload: bytes
                .get(offsets_end..)
                .ok_or_else(|| error("truncated bucket payload"))?,
        })
    }

    fn key_at(&self, index: usize) -> u64 {
        let start = index * 8;
        let mut bytes = [0_u8; 8];
        // `BucketView::new` guarantees `keys` holds exactly `count * 8` bytes.
        bytes.copy_from_slice(&self.keys[start..start + 8]);
        u64::from_le_bytes(bytes)
    }

    /// The payload slice for entry `index`, or `None` if its offsets are out of range or reversed.
    fn value_at(&self, index: usize) -> Option<&'a [u8]> {
        let mut start_bytes = [0_u8; 4];
        start_bytes.copy_from_slice(self.offsets.get(index * 4..index * 4 + 4)?);
        let mut end_bytes = [0_u8; 4];
        end_bytes.copy_from_slice(self.offsets.get(index * 4 + 4..index * 4 + 8)?);
        let start = usize::try_from(u32::from_le_bytes(start_bytes)).ok()?;
        let end = usize::try_from(u32::from_le_bytes(end_bytes)).ok()?;
        if start > end {
            return None;
        }
        self.payload.get(start..end)
    }
}

/// The serialized value of `key` in `bucket`, if present. The bucket's keys must be ascending.
pub(super) fn lookup_in_bucket<'a>(
    table: &'static str,
    bucket_key: u32,
    bucket: &'a [u8],
    key: u64,
) -> Result<Option<&'a [u8]>, StoreError> {
    let view = BucketView::new(table, bucket_key, bucket)?;
    let error = |reason| StoreError::CorruptBlock {
        table,
        key: bucket_key,
        reason,
    };
    let mut low = 0;
    let mut high = view.count;
    while low < high {
        let mid = low + (high - low) / 2;
        match view.key_at(mid).cmp(&key) {
            std::cmp::Ordering::Less => low = mid + 1,
            std::cmp::Ordering::Greater => high = mid,
            std::cmp::Ordering::Equal => {
                return view
                    .value_at(mid)
                    .map(Some)
                    .ok_or_else(|| error("entry offsets out of range"));
            }
        }
    }
    Ok(None)
}

/// Calls `visit(key, value_bytes)` for every entry of `bucket`, in ascending key order.
pub(super) fn iter_bucket_entries<'a>(
    table: &'static str,
    bucket_key: u32,
    bucket: &'a [u8],
    mut visit: impl FnMut(u64, &'a [u8]) -> Result<(), StoreError>,
) -> Result<(), StoreError> {
    let view = BucketView::new(table, bucket_key, bucket)?;
    for index in 0..view.count {
        let value = view.value_at(index).ok_or_else(|| StoreError::CorruptBlock {
            table,
            key: bucket_key,
            reason: "entry offsets out of range",
        })?;
        visit(view.key_at(index), value)?;
    }
    Ok(())
}

/// Pushes every key of `bucket`, in ascending order.
pub(super) fn iter_bucket_keys(
    table: &'static str,
    bucket_key: u32,
    bucket: &[u8],
    out: &mut Vec<u64>,
) -> Result<(), StoreError> {
    let view = BucketView::new(table, bucket_key, bucket)?;
    out.extend((0..view.count).map(|index| view.key_at(index)));
    Ok(())
}

/// Writes the bucket header + payload for `count` ascending keys with the given payload offsets.
pub(super) fn write_bucket(count: u32, keys: &[u64], offsets: &[u32], payload: &[u8], out: &mut Vec<u8>) {
    out.clear();
    out.extend_from_slice(&count.to_le_bytes());
    for key in keys {
        out.extend_from_slice(&key.to_le_bytes());
    }
    for offset in offsets {
        out.extend_from_slice(&offset.to_le_bytes());
    }
    out.extend_from_slice(payload);
}

/// Reused per-worker scratch buffers so encoding a table allocates once per worker instead of
/// three times per bucket.
#[derive(Default)]
struct BucketScratch {
    keys: Vec<u64>,
    offsets: Vec<u32>,
    payload: Vec<u8>,
}

impl BucketScratch {
    fn encode_entries<T>(
        &mut self,
        entries: &[(u64, T)],
        out: &mut Vec<u8>,
        mut append_value: impl FnMut(&T, Vec<u8>) -> Result<Vec<u8>, StoreError>,
    ) -> Result<(), StoreError> {
        let too_long = |reason| StoreError::CorruptBlock {
            table: "buckets",
            key: 0,
            reason,
        };
        let count = u32::try_from(entries.len()).map_err(|_| too_long("bucket entry count exceeds u32::MAX"))?;
        self.keys.clear();
        self.offsets.clear();
        self.payload.clear();
        for (key, value) in entries {
            self.keys.push(*key);
            self.offsets
                .push(u32::try_from(self.payload.len()).map_err(|_| too_long("bucket payload exceeds u32::MAX"))?);
            let payload = std::mem::take(&mut self.payload);
            self.payload = append_value(value, payload)?;
        }
        self.offsets
            .push(u32::try_from(self.payload.len()).map_err(|_| too_long("bucket payload exceeds u32::MAX"))?);
        write_bucket(count, &self.keys, &self.offsets, &self.payload, out);
        Ok(())
    }
}

/// Encodes a bucket from already-serialized payloads. `entries` must be sorted by key.
#[cfg(test)]
pub(super) fn encode_bucket_raw(entries: &[(u64, Vec<u8>)], out: &mut Vec<u8>) -> Result<(), StoreError> {
    let mut scratch = BucketScratch::default();
    scratch.encode_entries(entries, out, |value, mut payload| {
        payload.extend_from_slice(value);
        Ok(payload)
    })
}

/// Encodes `buckets[first_bucket..]` into `(bucket key, bytes)`, skipping empty buckets and
/// reusing one [`BucketScratch`] across all buckets in the slice.
fn encode_buckets<V: serde::Serialize>(
    buckets: &[Vec<(u64, &V)>],
    first_bucket: u32,
) -> Result<Vec<(u32, Vec<u8>)>, StoreError> {
    let mut encoded = Vec::with_capacity(buckets.len());
    let mut scratch = BucketScratch::default();
    for (offset, entries) in buckets.iter().enumerate() {
        if entries.is_empty() {
            continue;
        }
        let key = first_bucket
            .checked_add(u32::try_from(offset).unwrap_or(u32::MAX))
            .ok_or(StoreError::CorruptBlock {
                table: "buckets",
                key: first_bucket,
                reason: "bucket key overflow",
            })?;
        let mut out = Vec::with_capacity(entries.len() * 24 + 16);
        scratch.encode_entries(entries, &mut out, |value, payload| {
            postcard::to_extend(value, payload).map_err(StoreError::Encode)
        })?;
        encoded.push((key, out));
    }
    Ok(encoded)
}

/// Encodes `buckets` (whose first bucket key is `first_bucket`) on all available cores.
pub(super) fn encode_buckets_parallel<V: serde::Serialize + Sync>(
    buckets: &[Vec<(u64, &V)>],
    first_bucket: u32,
) -> Result<Vec<(u32, Vec<u8>)>, StoreError> {
    let workers = std::thread::available_parallelism().map_or(1, std::num::NonZero::get);
    if buckets.len() < 64 || workers < 2 {
        return encode_buckets(buckets, first_bucket);
    }
    let slice_size = buckets.len().div_ceil(workers).max(1);
    std::thread::scope(|scope| {
        let handles: Vec<_> = buckets
            .chunks(slice_size)
            .enumerate()
            .map(|(index, slice)| {
                let offset = u32::try_from(index * slice_size).unwrap_or(u32::MAX);
                let start = first_bucket.saturating_add(offset);
                scope.spawn(move || encode_buckets(slice, start))
            })
            .collect::<Vec<_>>();
        handles
            .into_iter()
            .map(|handle| handle.join().expect("bucket encoder panicked"))
            .collect::<Result<Vec<_>, StoreError>>()
    })
    .map(|chunks| chunks.into_iter().flatten().collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bucket_ids_cover_the_whole_u64_key_space() {
        assert_eq!(bucket_id(0, 0), 0, "a table with one bucket hashes everything to 0");
        assert_eq!(bucket_id(u64::MAX, 0), 0);
        for bits in 1..=MAX_BUCKET_BITS {
            let buckets = 1_u64 << bits;
            for key in [0, 1, u64::MAX - 1, u64::MAX] {
                let bucket = u64::from(bucket_id(key, bits));
                assert!(bucket < buckets, "bucket {bucket} out of range for {bits} bits");
            }
            // The top bits are the bucket: two keys that differ only in the low bits collide.
            assert_eq!(bucket_id(u64::MAX, bits), bucket_id(u64::MAX - 1, bits));
            assert_eq!(bucket_id(0, bits), 0);
            assert_eq!(bucket_id(u64::MAX, bits), u32::from(u16::MAX >> (16 - bits)));
        }
    }

    #[test]
    fn bucket_bits_grow_with_the_entry_count() {
        assert_eq!(bucket_bits_for_count(0, 64), 0);
        assert_eq!(bucket_bits_for_count(64, 64), 0);
        assert_eq!(bucket_bits_for_count(65, 64), 1);
        assert_eq!(bucket_bits_for_count(192, 64), 2);
        assert!(bucket_bits_for_count(usize::MAX / 2, 64) <= MAX_BUCKET_BITS);
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
}
