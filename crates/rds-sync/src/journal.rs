//! Resumable receive state under an opened destination root.
//!
//! `.rds-sync/<root-hex>/parts/<chunk-hash-hex>` contains verified bytes,
//! including chunks reused from the old destination. Advisory `meta` holds
//! postcard + a BLAKE3 trailer. I/O uses directory capabilities, exclusive
//! staging files and atomic replacement; no descendant symlink is followed.
//! A receive lock per root prevents concurrent owners from sharing cleanup.

use std::collections::{HashMap, HashSet};
use std::ffi::OsString;
use std::fs::File;
use std::io;
use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::{
    AVG_CHUNK, ChunkHash, MAX_CHUNK, MIN_CHUNK, Manifest, SyncError,
    confined::{Directory, PENDING, ReceiveLock},
    fault::{Point, hit},
    proto::{check_manifest, check_rel_path},
};

/// Directory name (under the sync root) holding in-flight state.
pub const STATE_DIR: &str = ".rds-sync";
const ASSEMBLY: &str = "assembly";

#[derive(Serialize)]
struct Meta {
    rel_path: String,
    size: u64,
    root: ChunkHash,
}

/// Resumable state for one in-flight receive. A journal pins its root and
/// destination parent when opened. The destination path returned by assembly
/// is informational; later I/O must not use it as a confinement proof.
pub struct Journal {
    state: Directory,
    dir: Directory,
    parts: Directory,
    dest_parent: Directory,
    dest_state: Directory,
    dest_name: OsString,
    dest_path: PathBuf,
    _lock: ReceiveLock,
    _parent_lock: Option<ReceiveLock>,
    manifest: Manifest,
    have: HashSet<u32>,
    fetched: u64,
}

impl Journal {
    /// Validate the offer, pin directories, claim the root's receive lock,
    /// and re-verify existing parts. A root with an active receive is
    /// refused immediately; callers can retry once its transfer ends.
    pub fn open(dest_dir: &Path, rel_path: &str, manifest: &Manifest) -> Result<Self, SyncError> {
        check_manifest(manifest)?;
        let rel = check_rel_path(rel_path)?;
        let root = Directory::open_root(dest_dir, true)?;
        let state = root.child(STATE_DIR.as_ref(), true)?;
        state.make_private()?;
        // Serialize receives within a root, including different processes.
        // A string-keyed per-path lock is insufficient on case-insensitive or
        // normalization-insensitive filesystems. One persistent inode also
        // avoids accumulating a lock file for every historical destination.
        let receive_lock = state.lock("receive.lock".as_ref())?;
        let content_id = hex(&manifest.root);
        let (dest_parent, dest_name) = root.parent(&rel, true)?;
        // The destination's parent is the common ownership point even when
        // different configured roots overlap. Keep its assembly inode on the
        // same filesystem, including destinations below a mount point.
        let dest_state = dest_parent.child(STATE_DIR.as_ref(), true)?;
        dest_state.make_private()?;
        let parent_lock = if state.same_inode(&dest_state)? {
            None
        } else {
            Some(dest_state.lock("receive.lock".as_ref())?)
        };
        dest_state.discard_owned(ASSEMBLY.as_ref())?;
        // Refuse symlinks and special files even if they contain no reusable
        // bytes. NONBLOCK + fstat prevents a FIFO from blocking admission.
        let existing = match dest_parent.read_file(&dest_name) {
            Ok(file) => Some(file),
            Err(e) if e.kind() == io::ErrorKind::NotFound => None,
            Err(e) => return Err(e.into()),
        };
        let dir = state.child(content_id.as_ref(), true)?;
        let parts = dir.child("parts".as_ref(), true)?;
        parts.discard_owned(PENDING.as_ref())?;
        write_meta(
            &dir,
            &Meta {
                rel_path: rel.to_string_lossy().into_owned(),
                size: manifest.size,
                root: manifest.root,
            },
        )?;
        let mut journal = Self {
            state,
            dir,
            parts,
            dest_parent,
            dest_state,
            dest_name,
            dest_path: dest_dir.join(rel),
            _lock: receive_lock,
            _parent_lock: parent_lock,
            manifest: manifest.clone(),
            have: HashSet::new(),
            fetched: 0,
        };
        journal.rescan()?;
        if let Some(file) = existing {
            journal.seed_from_destination(file)?;
        }
        Ok(journal)
    }

    /// Reused chunks become immutable verified parts before advertising have.
    /// File size and original offsets do not constrain content-defined reuse.
    /// Reading remains bounded to one chunk plus the manifest's hash index.
    fn seed_from_destination(&mut self, file: File) -> Result<(), SyncError> {
        let mut wanted: HashMap<(ChunkHash, u32), Vec<u32>> = HashMap::new();
        for (i, c) in self.manifest.chunks.iter().enumerate() {
            if !self.have.contains(&(i as u32)) {
                wanted.entry((c.hash, c.len)).or_default().push(i as u32);
            }
        }
        if wanted.is_empty() {
            return Ok(());
        }
        for chunk in fastcdc::v2020::StreamCDC::new(
            file,
            MIN_CHUNK as usize,
            AVG_CHUNK as usize,
            MAX_CHUNK as usize,
        ) {
            let chunk = chunk.map_err(io::Error::from)?;
            let hash = *blake3::hash(&chunk.data).as_bytes();
            if let Some(indices) = wanted.remove(&(hash, chunk.length as u32)) {
                self.parts.write_state(hex(&hash).as_ref(), &chunk.data)?;
                self.have.extend(indices);
                if wanted.is_empty() {
                    break;
                }
            }
        }
        Ok(())
    }

    fn rescan(&mut self) -> Result<(), SyncError> {
        self.have.clear();
        for (i, c) in self.manifest.chunks.iter().enumerate() {
            let name = hex(&c.hash);
            let data = match self.parts.read_state(name.as_ref(), c.len as usize) {
                Ok(data) => data,
                Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
                Err(e) => return Err(e.into()),
            };
            if data.len() == c.len as usize && blake3::hash(&data).as_bytes() == &c.hash {
                self.have.insert(i as u32);
            } else {
                self.parts.unlink(name.as_ref(), false)?;
            }
        }
        Ok(())
    }

    /// Manifest indices still missing — what the receiver asks for.
    pub fn need(&self) -> Vec<u32> {
        (0..self.manifest.chunks.len() as u32)
            .filter(|i| !self.have.contains(i))
            .collect()
    }

    /// Verify and persist a received chunk before counting it as present.
    /// Returns false for a verified retransmission; local reuse is not fetched.
    pub fn store(&mut self, index: u32, data: &[u8]) -> Result<bool, SyncError> {
        let c = self
            .manifest
            .chunks
            .get(index as usize)
            .ok_or_else(|| SyncError::Manifest(format!("chunk index {index} out of range")))?;
        if data.len() != c.len as usize || blake3::hash(data).as_bytes() != &c.hash {
            return Err(SyncError::Manifest(format!(
                "chunk {index} failed BLAKE3 verification"
            )));
        }
        if self.have.contains(&index) {
            return Ok(false);
        }
        self.parts.write_state(hex(&c.hash).as_ref(), data)?;
        self.have.insert(index);
        self.fetched += 1;
        Ok(true)
    }

    /// True once every manifest chunk is present and verified.
    pub fn complete(&self) -> bool {
        self.have.len() == self.manifest.chunks.len()
    }

    /// Chunks fetched this session — resume-overhead accounting.
    pub fn fetched(&self) -> u64 {
        self.fetched
    }

    /// Indices whose parts verified on disk.
    pub fn have_set(&self) -> &HashSet<u32> {
        &self.have
    }

    /// Total chunks in the manifest.
    pub fn total(&self) -> usize {
        self.manifest.chunks.len()
    }

    /// Re-verify parts, concatenate into an exclusively owned staging file,
    /// check the root and atomically replace the pinned destination. Data and
    /// its parent are synced before success; failures before rename retain the
    /// old destination. A directory-sync error after rename is an uncertain
    /// commit and is returned, never acknowledged as complete.
    pub fn assemble(self) -> Result<PathBuf, SyncError> {
        self.assemble_cancellable(&|| false)
    }

    /// [`assemble`](Self::assemble) with a cancellation check between chunks.
    /// A running part read/write completes; `stop` is consulted once per
    /// manifest chunk, so the barrier granularity is one chunk.
    pub fn assemble_cancellable(self, stop: &dyn Fn() -> bool) -> Result<PathBuf, SyncError> {
        if !self.complete() {
            return Err(SyncError::Manifest("assemble before complete".into()));
        }
        let mut stage = self.dest_state.stage_named(ASSEMBLY.into())?;
        let mut root = blake3::Hasher::new();
        for c in &self.manifest.chunks {
            if stop() {
                return Err(SyncError::Manifest("assembly canceled".into()));
            }
            let data = self
                .parts
                .read_state(hex(&c.hash).as_ref(), c.len as usize)?;
            if data.len() != c.len as usize || blake3::hash(&data).as_bytes() != &c.hash {
                return Err(SyncError::Manifest("part changed before assembly".into()));
            }
            root.update(&data);
            crate::fault::write(&mut stage.file, &data)?;
        }
        if root.finalize().as_bytes() != &self.manifest.root {
            return Err(SyncError::Manifest(
                "assembled file failed root hash".into(),
            ));
        }
        hit(Point::Written)?;
        stage.install_in(&self.dest_parent, &self.dest_name)?;
        // Publication is durable now. A cleanup failure does not turn a
        // committed file into a failed transfer; the next open re-verifies
        // remaining state. Log only the error, never private filenames.
        if let Err(error) = self.cleanup() {
            tracing::warn!(%error, "sync committed; journal cleanup incomplete");
        }
        Ok(self.dest_path)
    }

    fn cleanup(&self) -> io::Result<()> {
        // Remove only names belonging to this manifest under held handles.
        // Never recursively traverse unknown entries or another transfer.
        for c in &self.manifest.chunks {
            remove_if_present(&self.parts, hex(&c.hash).as_ref(), false)?;
            hit(Point::PartRemoved)?;
        }
        self.parts.sync()?;
        remove_if_present(&self.dir, "meta".as_ref(), false)?;
        hit(Point::MetaRemoved)?;
        remove_if_present(&self.dir, "parts".as_ref(), true)?;
        self.dir.sync()?;
        hit(Point::PartsRemoved)?;
        remove_if_present(&self.state, hex(&self.manifest.root).as_ref(), true)?;
        self.state.sync()?;
        hit(Point::JournalRemoved)?;
        Ok(())
    }
}

fn remove_if_present(dir: &Directory, name: &std::ffi::OsStr, directory: bool) -> io::Result<()> {
    match dir.unlink(name, directory) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        result => result,
    }
}

fn write_meta(dir: &Directory, meta: &Meta) -> Result<(), SyncError> {
    let mut body = postcard::to_allocvec(meta)
        .map_err(|e| SyncError::Manifest(format!("meta encode: {e}")))?;
    let hash = blake3::hash(&body);
    body.extend_from_slice(hash.as_bytes());
    dir.write_state("meta".as_ref(), &body)?;
    Ok(())
}

fn hex(hash: &ChunkHash) -> String {
    blake3::Hash::from(*hash).to_hex().to_string()
}

#[cfg(test)]
mod tests;
