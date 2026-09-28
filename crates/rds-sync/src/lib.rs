//! File synchronization for rds: content-defined chunking, BLAKE3
//! addressing, resumable transfer.
//!
//! Design: a file maps to a [`Manifest`] of variable-length chunks cut
//! by FastCDC (rolling-hash boundaries survive inserts/edits, so changed
//! files share most chunks). Each chunk is addressed by its BLAKE3
//! digest — transfer, resume and verification all key on content, not
//! offsets. Reconciliation and transfer run over rds streams; the sync
//! engine is independent of the interactive desktop path on purpose.
//!
//! Scope: the content model (chunking, manifesting, hashing) plus the
//! transfer protocol — `proto` frames, `journal` resumable state, and
//! `engine` driving send/receive/serve over `rds-net` streams.

pub mod engine;
pub mod journal;
pub mod proto;

mod confined;
mod fault;

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// BLAKE3 digest of a chunk's content.
pub type ChunkHash = [u8; 32];

/// Default chunk sizing: 16 KiB min, 64 KiB average, 256 KiB max.
pub const MIN_CHUNK: u32 = 16 * 1024;
pub const AVG_CHUNK: u32 = 64 * 1024;
pub const MAX_CHUNK: u32 = 256 * 1024;

/// One content-addressed chunk.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Chunk {
    /// BLAKE3 digest of the chunk bytes.
    pub hash: ChunkHash,
    /// Byte offset within the file.
    pub offset: u64,
    /// Chunk length in bytes.
    pub len: u32,
}

/// A file as a set of content-addressed chunks.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Manifest {
    /// Total file length in bytes.
    pub size: u64,
    /// BLAKE3 digest of the whole file (fast equality check).
    pub root: ChunkHash,
    /// Chunks in file order.
    pub chunks: Vec<Chunk>,
}

#[derive(Debug, Error)]
pub enum SyncError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("manifest malformed: {0}")]
    Manifest(String),
}

/// Build the manifest for `data` using FastCDC 2020 chunking.
/// Loads nothing extra but requires the whole input in memory — engine
/// paths over files use [`manifest_of_reader`] instead.
pub fn manifest_of(data: &[u8]) -> Manifest {
    manifest_with(data, MIN_CHUNK, AVG_CHUNK, MAX_CHUNK)
}

/// Build the manifest with explicit FastCDC size bounds.
pub fn manifest_with(data: &[u8], min: u32, avg: u32, max: u32) -> Manifest {
    let chunks = fastcdc::v2020::FastCDC::new(data, min as usize, avg as usize, max as usize)
        .map(|c| Chunk {
            hash: *blake3::hash(&data[c.offset..c.offset + c.length]).as_bytes(),
            offset: c.offset as u64,
            len: c.length as u32,
        })
        .collect();
    Manifest {
        size: data.len() as u64,
        root: *blake3::hash(data).as_bytes(),
        chunks,
    }
}

/// Build the manifest by streaming `r` — the same FastCDC cut points
/// as [`manifest_of`] but with memory bounded at one max-size chunk,
/// which is what the engine needs for files too large to load whole.
pub fn manifest_of_reader(r: impl std::io::Read) -> std::io::Result<Manifest> {
    manifest_reader_with(r, MIN_CHUNK, AVG_CHUNK, MAX_CHUNK)
}

/// Streaming manifest with a cancellation check between yielded chunks.
/// A running read or hash cannot be aborted; `stop` is consulted once per
/// produced chunk, so the barrier granularity is one max-size chunk.
/// Returns `ErrorKind::Interrupted` when `stop` reports cancellation.
pub fn manifest_of_reader_cancellable(
    r: impl std::io::Read,
    stop: &dyn Fn() -> bool,
) -> std::io::Result<Manifest> {
    manifest_reader_cancellable_with(r, MIN_CHUNK, AVG_CHUNK, MAX_CHUNK, stop)
}

/// Streaming manifest with explicit FastCDC size bounds.
pub fn manifest_reader_with(
    r: impl std::io::Read,
    min: u32,
    avg: u32,
    max: u32,
) -> std::io::Result<Manifest> {
    manifest_reader_cancellable_with(r, min, avg, max, &|| false)
}

/// Streaming manifest with explicit bounds and a per-chunk stop check.
pub fn manifest_reader_cancellable_with(
    r: impl std::io::Read,
    min: u32,
    avg: u32,
    max: u32,
    stop: &dyn Fn() -> bool,
) -> std::io::Result<Manifest> {
    let chunker = fastcdc::v2020::StreamCDC::new(r, min as usize, avg as usize, max as usize);
    let mut root = blake3::Hasher::new();
    let mut size = 0u64;
    let mut chunks = Vec::new();
    for c in chunker {
        if stop() {
            return Err(std::io::Error::from(std::io::ErrorKind::Interrupted));
        }
        let c = c.map_err(std::io::Error::from)?;
        root.update(&c.data);
        size += c.data.len() as u64;
        chunks.push(Chunk {
            hash: *blake3::hash(&c.data).as_bytes(),
            offset: c.offset,
            len: c.length as u32,
        });
        if chunks.len() > proto::MAX_CHUNKS {
            return Err(std::io::Error::other(format!(
                "file exceeds {} chunks",
                proto::MAX_CHUNKS
            )));
        }
    }
    Ok(Manifest {
        size,
        root: *root.finalize().as_bytes(),
        chunks,
    })
}

/// Manifest of a file on disk, streamed — O(max chunk) memory, the
/// engine's path for large files. Blocking fs work: call from
/// `spawn_blocking`, not an async worker.
pub fn manifest_of_path(path: &std::path::Path) -> std::io::Result<Manifest> {
    manifest_of_reader(std::fs::File::open(path)?)
}

/// Chunks in `b`'s manifest that `a`'s manifest lacks — the wire delta.
pub fn missing_chunks<'a>(have: &Manifest, want: &'a Manifest) -> Vec<&'a Chunk> {
    use std::collections::HashSet;
    let present: HashSet<ChunkHash> = have.chunks.iter().map(|c| c.hash).collect();
    want.chunks
        .iter()
        .filter(|c| !present.contains(&c.hash))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chunking_is_content_defined() {
        // Deterministic aperiodic data: a periodic source would make the
        // insert genuinely change subsequent content.
        // ~1 MiB → ~16 chunks at the default 64 KiB average, enough for
        // the sharing ratio to be meaningful.
        let mut a = vec![0u8; 1_000_000];
        let mut state = 0x9E3779B97F4A7C15u64;
        for b in &mut a {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            *b = state as u8;
        }
        // Same content with an insert near the front — later chunks
        // should still match despite shifted offsets.
        let mut b = a.clone();
        b.splice(10_000..10_000, vec![0xAA; 3000]);
        let ma = manifest_of(&a);
        let mb = manifest_of(&b);
        let common = ma
            .chunks
            .iter()
            .filter(|c| mb.chunks.iter().any(|d| d.hash == c.hash))
            .count();
        // The bulk of both files must share chunks.
        assert!(common as f64 > ma.chunks.len() as f64 * 0.8);
        // Delta is much smaller than the file.
        assert!(missing_chunks(&ma, &mb).len() < ma.chunks.len() / 4);
    }

    #[test]
    fn manifest_roundtrip() {
        let data = b"remote-device-sync sync engine";
        let m = manifest_of(data);
        assert_eq!(m.size as usize, data.len());
        assert_eq!(m.chunks.iter().map(|c| c.len as u64).sum::<u64>(), m.size);
    }

    #[test]
    fn cancellable_manifest_stops_at_chunk_boundary() {
        // W1.9: a canceled scan surfaces as Interrupted rather than
        // hashing the whole source first.
        let data = vec![0xABu8; 1_000_000];
        let err = manifest_reader_cancellable_with(
            std::io::Cursor::new(&data),
            MIN_CHUNK,
            AVG_CHUNK,
            MAX_CHUNK,
            &|| true,
        )
        .unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::Interrupted);
        // An un-set stop flag must not change the produced manifest.
        let ok = manifest_reader_cancellable_with(
            std::io::Cursor::new(&data),
            MIN_CHUNK,
            AVG_CHUNK,
            MAX_CHUNK,
            &|| false,
        )
        .unwrap();
        let expected = manifest_of(&data);
        assert_eq!(ok.root, expected.root);
        assert_eq!(ok.chunks.len(), expected.chunks.len());
    }

    #[test]
    fn streaming_manifest_matches_slice() {
        // The streamed and in-memory chunkers must cut identical
        // boundaries — resume/dedup correctness depends on it.
        let mut data = vec![0u8; 3_000_000];
        let mut state = 0x9E3779B97F4A7C15u64;
        for b in &mut data {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            *b = state as u8;
        }
        let a = manifest_of(&data);
        let b = manifest_of_reader(std::io::Cursor::new(&data)).unwrap();
        assert_eq!(a.size, b.size);
        assert_eq!(a.root, b.root);
        assert_eq!(a.chunks, b.chunks);
        // Edge: empty input streams to an empty manifest.
        let e = manifest_of_reader(std::io::Cursor::new(Vec::<u8>::new())).unwrap();
        assert_eq!(e.size, 0);
        assert!(e.chunks.is_empty());
    }
}
