//! Transactional record/tombstone storage. Both mutation types share
//! signed publisher revisions and durable expiry floors.
use crate::{
    DeleteRequest, DiscoveryError, EndpointKey, EndpointRecord, RecordStore,
    clock::Reading,
    persist::{AtomicFile, regular},
};
use redb::{Database, Durability, ReadableDatabase, ReadableTable, TableDefinition};
use rustix::fs::{Mode, OFlags, openat};
use serde::{Deserialize, Serialize};
use std::{fs::File, io, os::unix::fs::FileExt, path::Path, sync::Mutex, time::Duration};

const RECORDS: TableDefinition<'_, &[u8], &[u8]> = TableDefinition::new("records-v3");
const META: TableDefinition<'_, u8, &[u8]> = TableDefinition::new("metadata-v3");
const MAX_CELL: usize = 256 * 1024;
/// Hard file length/offset bound, including database bookkeeping.
pub const MAX_DATABASE: u64 = 256 * 1024 * 1024;
/// Maximum retained identities, including tombstones and expiry floors.
pub const MAX_IDENTITIES: usize = 4096;
const GC_BATCH: usize = 64;
mod entry;
mod memory;
mod metrics;
use crate::observation::Published;
pub use metrics::{RecordMetrics, RecordSnapshot};
mod migration;
use entry::{Entry, Stored, decide, decode, exact, observe};
pub use memory::MemoryStore;
pub use migration::{MigrationReceipt, migrate_v2};

#[cfg(test)]
mod expiry_tests;
#[cfg(test)]
mod recovery_tests;
#[cfg(test)]
mod tests;
#[cfg(test)]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Phase {
    BeforeDatabase,
    AfterDatabase,
    AfterAnchor,
}

fn error(e: impl std::fmt::Display) -> DiscoveryError {
    DiscoveryError::Store(e.to_string())
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct Metadata {
    format: u16,
    database: [u8; 16],
    generation: u64,
    live: u64,
    identities: u64,
    wall_floor: Duration,
}

#[derive(Serialize, Deserialize)]
struct Anchor {
    metadata: Metadata,
    digest: [u8; 32],
}
fn encode_anchor(metadata: &Metadata) -> Result<Vec<u8>, DiscoveryError> {
    let bytes = postcard::to_stdvec(metadata).map_err(error)?;
    serde_json::to_vec(&Anchor {
        metadata: metadata.clone(),
        digest: *blake3::hash(&bytes).as_bytes(),
    })
    .map_err(error)
}
fn decode_anchor(bytes: &[u8]) -> Result<Metadata, DiscoveryError> {
    let anchor: Anchor = serde_json::from_slice(bytes).map_err(error)?;
    let digest = *blake3::hash(&postcard::to_stdvec(&anchor.metadata).map_err(error)?).as_bytes();
    if digest != anchor.digest {
        return Err(error("record commit anchor checksum mismatch"));
    }
    Ok(anchor.metadata)
}

/// A file descriptor opened relative to the protected directory. redb never
/// opens a path. The strict OS file lock is held for this backend's lifetime;
/// unsupported file locking is an error, never an unsafe fallback.
#[derive(Debug)]
struct BoundedFile {
    file: File,
    #[cfg(test)]
    fault: std::sync::Arc<std::sync::atomic::AtomicU8>,
}
impl Drop for BoundedFile {
    fn drop(&mut self) {
        // Release owned locking even if another thread just forked and its
        // pre-exec child temporarily inherited this open-file description.
        let _ = self.file.unlock();
    }
}
impl redb::StorageBackend for BoundedFile {
    fn len(&self) -> io::Result<u64> {
        Ok(self.file.metadata()?.len())
    }
    fn read(&self, offset: u64, out: &mut [u8]) -> io::Result<()> {
        bounds(offset, out.len())?;
        self.file.read_exact_at(out, offset)
    }
    fn set_len(&self, len: u64) -> io::Result<()> {
        if len > MAX_DATABASE {
            return Err(io::Error::other("record database capacity exceeded"));
        }
        self.file.set_len(len)
    }
    fn sync_data(&self) -> io::Result<()> {
        #[cfg(test)]
        if self
            .fault
            .compare_exchange(
                2,
                0,
                std::sync::atomic::Ordering::SeqCst,
                std::sync::atomic::Ordering::SeqCst,
            )
            .is_ok()
        {
            return Err(io::Error::other("injected database sync failure"));
        }
        self.file.sync_all()
    }
    fn write(&self, offset: u64, data: &[u8]) -> io::Result<()> {
        bounds(offset, data.len())?;
        #[cfg(test)]
        if self
            .fault
            .compare_exchange(
                1,
                0,
                std::sync::atomic::Ordering::SeqCst,
                std::sync::atomic::Ordering::SeqCst,
            )
            .is_ok()
        {
            self.file.write_all_at(&data[..data.len() / 2], offset)?;
            return Err(io::Error::other("injected partial database write"));
        }
        self.file.write_all_at(data, offset)
    }
}
fn bounds(offset: u64, len: usize) -> io::Result<()> {
    if offset
        .checked_add(len as u64)
        .is_none_or(|end| end > MAX_DATABASE)
    {
        Err(io::Error::other("record database capacity exceeded"))
    } else {
        Ok(())
    }
}

/// Protected embedded Rust database, including durable delete tombstones.
/// Legacy per-key JSON directories require explicit migration; they are never
/// ignored or overwritten. No database daemon or subprocess is used.
pub struct FileStore {
    inner: Mutex<Inner>,
    observed: Published<RecordSnapshot>,
}
struct Inner {
    db: Database,
    anchor: AtomicFile,
    metadata: Metadata,
    failed: bool,
    // Only an ENOSPC/quota failure after the database committed may retry
    // the exact anchor. Database I/O failures remain fail-closed until reopen.
    pending_anchor: Option<Metadata>,
    observed_wall: Duration,
    cursor: Option<[u8; 32]>,
    #[cfg(test)]
    fault: Option<(Phase, bool)>,
    #[cfg(test)]
    io_fault: std::sync::Arc<std::sync::atomic::AtomicU8>,
}

impl FileStore {
    pub fn new(dir: impl Into<std::path::PathBuf>) -> io::Result<Self> {
        Self::open(&dir.into()).map_err(io::Error::other)
    }
    fn open(path: &Path) -> Result<Self, DiscoveryError> {
        let anchor = AtomicFile::open_named(path, "records.anchor", "records.lock")?;
        Self::open_owned(anchor, false)
    }
    fn open_owned(anchor: AtomicFile, migrating: bool) -> Result<Self, DiscoveryError> {
        // This namespace is exclusively ours. Refuse legacy/unrecognized
        // files instead of silently initializing over unrelated history.
        for (index, entry) in rustix::fs::Dir::read_from(anchor.directory())
            .map_err(error)?
            .enumerate()
        {
            if index > 128 {
                return Err(error("record state directory requires maintenance"));
            }
            let entry = entry.map_err(error)?;
            let name = entry.file_name().to_bytes();
            if !matches!(
                name,
                b"." | b".." | b"records.lock" | b"records.anchor" | b"records.redb"
            ) && !(name.starts_with(b".policy-") && name.ends_with(b".tmp"))
                && !(migrating && name == b"migration.pending")
            {
                return Err(error(
                    "legacy or unknown record files require explicit migration",
                ));
            }
        }
        let stored_anchor = anchor
            .read()?
            .map(|bytes| decode_anchor(&bytes))
            .transpose()?;
        if stored_anchor.is_none() && anchor.initialized()? {
            return Err(error("record commit anchor missing"));
        }
        let mut flags = OFlags::RDWR | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC;
        if stored_anchor.is_none() {
            flags |= OFlags::CREATE;
        }
        let file = File::from(
            openat(
                anchor.directory(),
                "records.redb",
                flags,
                Mode::RUSR | Mode::WUSR,
            )
            .map_err(error)?,
        );
        regular(&file)?;
        file.try_lock()
            .map_err(|e| error(format!("record database lock: {e}")))?;
        let len = file.metadata().map_err(error)?.len();
        if len > MAX_DATABASE || (len == 0 && stored_anchor.is_some()) {
            return Err(error("record database missing, empty or oversized"));
        }
        #[cfg(test)]
        let io_fault = std::sync::Arc::new(std::sync::atomic::AtomicU8::new(0));
        let mut db = Database::builder()
            .set_cache_size(16 * 1024 * 1024)
            .create_with_backend(BoundedFile {
                file,
                #[cfg(test)]
                fault: io_fault.clone(),
            })
            .map_err(error)?;
        let metadata = {
            let read = db.begin_read().map_err(error)?;
            match read.open_table(META) {
                Ok(table) => {
                    let value = table
                        .get(0)
                        .map_err(error)?
                        .ok_or_else(|| error("record metadata missing"))?;
                    exact::<Metadata>(value.value())?
                }
                Err(redb::TableError::TableDoesNotExist(_)) if stored_anchor.is_none() => {
                    if read.list_tables().map_err(error)?.next().is_some() {
                        return Err(error("record metadata absent from nonempty database"));
                    }
                    drop(read);
                    let metadata = Metadata {
                        format: 3,
                        database: rand::random(),
                        generation: 1,
                        live: 0,
                        identities: 0,
                        wall_floor: Duration::ZERO,
                    };
                    let mut txn = db.begin_write().map_err(error)?;
                    txn.set_durability(Durability::Immediate).map_err(error)?;
                    txn.open_table(RECORDS).map_err(error)?;
                    txn.open_table(META)
                        .map_err(error)?
                        .insert(0, postcard::to_stdvec(&metadata).map_err(error)?.as_slice())
                        .map_err(error)?;
                    txn.commit().map_err(error)?;
                    metadata
                }
                Err(e) => return Err(error(e)),
            }
        };
        if metadata.format != 3
            || metadata.generation == 0
            || metadata.identities > MAX_IDENTITIES as u64
        {
            return Err(error("invalid record metadata"));
        }
        if let Some(previous) = &stored_anchor {
            if previous.format != metadata.format
                || previous.wall_floor > metadata.wall_floor
                || previous.database != metadata.database
                || metadata.generation < previous.generation
                || metadata.generation > previous.generation.saturating_add(1)
                || (metadata.generation == previous.generation && *previous != metadata)
            {
                return Err(error(
                    "record database rolled back or disagrees with durable commit anchor",
                ));
            }
        } else if metadata.generation != 1 || metadata.identities != 0 {
            return Err(error("record database lacks its commit anchor"));
        }
        // Validate the entire bounded catalog on open. A repair must not turn
        // corrupt entries into missing history or serve an unverified record.
        let mut identities = 0;
        let mut live = 0;
        {
            let read = db.begin_read().map_err(error)?;
            let table = read.open_table(RECORDS).map_err(error)?;
            for entry in table.iter().map_err(error)? {
                let (key, value) = entry.map_err(error)?;
                identities += 1;
                if identities > MAX_IDENTITIES as u64 {
                    return Err(error("record identity capacity exceeded"));
                }
                let key = EndpointKey(key.value().try_into().map_err(error)?);
                let stored = decode(value.value(), &key)?;
                if stored.accepted_wall() > metadata.wall_floor {
                    return Err(error("record lease exceeds catalog clock floor"));
                }
                live += u64::from(stored.live());
            }
        }
        if identities != metadata.identities || live != metadata.live {
            return Err(error("record metadata/catalog mismatch"));
        }
        if stored_anchor.as_ref() != Some(&metadata) {
            anchor.write(&encode_anchor(&metadata)?)?;
        }
        if !anchor.initialized()? {
            anchor.seal()?;
        }
        // Reclaim pages freed by expired/deleted records — without periodic
        // compaction the file only ever grows. Maintenance, not an
        // invariant: a failed compact must not refuse a valid store, and
        // this crate carries no logging facade to report it through.
        let _ = db.compact();
        let observed = Published::new(record_snapshot(&metadata, true));
        Ok(Self {
            observed,
            inner: Mutex::new(Inner {
                db,
                anchor,
                metadata,
                failed: false,
                pending_anchor: None,
                observed_wall: Duration::ZERO,
                cursor: None,
                #[cfg(test)]
                fault: None,
                #[cfg(test)]
                io_fault,
            }),
        })
    }
    fn access(
        &self,
        key: EndpointKey,
        entry: Option<Entry>,
        now: Option<Reading>,
    ) -> Result<Option<EndpointRecord>, DiscoveryError> {
        self.access_admitted(key, entry, now, &mut |_| Ok(()))
    }
    fn access_admitted(
        &self,
        key: EndpointKey,
        entry: Option<Entry>,
        now: Option<Reading>,
        admit: &mut dyn FnMut(bool) -> Result<(), DiscoveryError>,
    ) -> Result<Option<EndpointRecord>, DiscoveryError> {
        self.observed_access(|inner| {
            inner.healthy()?;
            let now = now.map(Ok).unwrap_or_else(Reading::now)?;
            let floor = inner.metadata.wall_floor;
            observe(floor, &mut inner.observed_wall, now)?;
            inner.failed = true;
            let old = {
                let txn = inner.db.begin_read().map_err(error)?;
                let table = txn.open_table(RECORDS).map_err(error)?;
                table
                    .get(key.0.as_slice())
                    .map_err(error)?
                    .map(|value| decode(value.value(), &key))
                    .transpose()?
            };
            inner.failed = false;
            let decision = decide(old.as_ref(), entry, &key, now)?;
            if let Some(replacement) = decision.replacement {
                if old.is_none() && inner.metadata.identities >= MAX_IDENTITIES as u64 {
                    return Err(error("record identity capacity exceeded"));
                }
                if matches!(replacement, Stored::Active { .. }) {
                    admit(old.is_some())?;
                }
                inner.commit(&[(key, replacement)], now)?;
            }
            decision.result
        })
    }
    fn collect_at(&self, now: Option<Reading>) -> Result<usize, DiscoveryError> {
        self.observed_access(|inner| {
            inner.healthy()?;
            let now = now.map(Ok).unwrap_or_else(Reading::now)?;
            let floor = inner.metadata.wall_floor;
            observe(floor, &mut inner.observed_wall, now)?;
            inner.failed = true;
            let mut changes = Vec::new();
            let mut scanned = 0;
            let mut last = None;
            {
                let txn = inner.db.begin_read().map_err(error)?;
                let table = txn.open_table(RECORDS).map_err(error)?;
                let start = inner
                    .cursor
                    .as_ref()
                    .map_or(std::ops::Bound::Unbounded, |key| {
                        std::ops::Bound::Excluded(key.as_slice())
                    });
                for item in table
                    .range::<&[u8]>((start, std::ops::Bound::Unbounded))
                    .map_err(error)?
                    .take(GC_BATCH)
                {
                    let (key, value) = item.map_err(error)?;
                    let key = EndpointKey(key.value().try_into().map_err(error)?);
                    if let Some(retired) = decode(value.value(), &key)?.retire(&key, now)? {
                        changes.push((key, retired));
                    }
                    scanned += 1;
                    last = Some(key.0);
                }
            }
            inner.failed = false;
            if !changes.is_empty() {
                inner.commit(&changes, now)?;
            }
            inner.cursor = if scanned == GC_BATCH { last } else { None };
            Ok(changes.len())
        })
    }

    fn observed_access<T>(
        &self,
        work: impl FnOnce(&mut Inner) -> Result<T, DiscoveryError>,
    ) -> Result<T, DiscoveryError> {
        let mut inner = self.inner.lock().map_err(|e| {
            self.observed.finish(|snapshot| snapshot.healthy = false);
            error(e)
        })?;
        self.observed.begin();
        let result = work(&mut inner);
        self.observed.finish(|snapshot| {
            *snapshot = record_snapshot(&inner.metadata, !inner.failed);
        });
        result
    }
}

fn record_snapshot(metadata: &Metadata, healthy: bool) -> RecordSnapshot {
    RecordSnapshot {
        healthy,
        durable: true,
        generation: Some(metadata.generation),
        records: metadata.live,
        identities: metadata.identities,
        capacity: MAX_IDENTITIES as u64,
    }
}

impl Inner {
    /// Called under the owner mutex. Both commits complete before any result,
    /// including an expiry refusal that caused content to be reclaimed.
    fn commit(
        &mut self,
        changes: &[(EndpointKey, Stored)],
        now: Reading,
    ) -> Result<(), DiscoveryError> {
        self.failed = true;
        let anchored = self
            .anchor
            .read()?
            .ok_or_else(|| error("record commit anchor disappeared"))?;
        if decode_anchor(&anchored)? != self.metadata {
            return Err(error(
                "record commit anchor changed during service lifetime",
            ));
        }
        let mut next = self.metadata.clone();
        next.generation = next
            .generation
            .checked_add(1)
            .ok_or_else(|| error("record commit generation exhausted"))?;
        next.wall_floor = next.wall_floor.max(now.wall);
        let mut txn = self.db.begin_write().map_err(error)?;
        txn.set_durability(Durability::Immediate).map_err(error)?;
        {
            let mut table = txn.open_table(RECORDS).map_err(error)?;
            for (key, replacement) in changes {
                let bytes = postcard::to_stdvec(replacement).map_err(error)?;
                if bytes.len() > MAX_CELL {
                    return Err(error("record exceeds storage limit"));
                }
                let old = table
                    .get(key.0.as_slice())
                    .map_err(error)?
                    .map(|value| decode(value.value(), key))
                    .transpose()?;
                if old.is_none() {
                    next.identities = next
                        .identities
                        .checked_add(1)
                        .filter(|n| *n <= MAX_IDENTITIES as u64)
                        .ok_or_else(|| error("record identity capacity exceeded"))?;
                }
                next.live = next
                    .live
                    .checked_sub(u64::from(old.as_ref().is_some_and(Stored::live)))
                    .and_then(|n| n.checked_add(u64::from(replacement.live())))
                    .ok_or_else(|| error("record count inconsistent"))?;
                table
                    .insert(key.0.as_slice(), bytes.as_slice())
                    .map_err(error)?;
            }
        }
        txn.open_table(META)
            .map_err(error)?
            .insert(0, postcard::to_stdvec(&next).map_err(error)?.as_slice())
            .map_err(error)?;
        #[cfg(test)]
        self.checkpoint(Phase::BeforeDatabase)?;
        txn.commit().map_err(error)?;
        #[cfg(test)]
        self.checkpoint(Phase::AfterDatabase)?;
        if let Err(error) = self.anchor.write(&encode_anchor(&next)?) {
            if matches!(error, DiscoveryError::StorageUnavailable(_)) {
                self.pending_anchor = Some(next);
            }
            return Err(error);
        }
        #[cfg(test)]
        self.checkpoint(Phase::AfterAnchor)?;
        self.metadata = next;
        self.failed = false;
        Ok(())
    }

    #[cfg(test)]
    fn checkpoint(&self, phase: Phase) -> Result<(), DiscoveryError> {
        if let Some((selected, terminate)) = self.fault
            && selected == phase
        {
            if terminate {
                std::process::exit(86);
            }
            return Err(error(format!("injected record failure at {phase:?}")));
        }
        Ok(())
    }
    fn healthy(&mut self) -> Result<(), DiscoveryError> {
        if self.failed
            && let Some(next) = self.pending_anchor.clone()
        {
            // Keep the original exclusive locks. A retry may finish only
            // this process's known database commit, never adopt other state.
            let recovered = (|| {
                let anchored = self
                    .anchor
                    .read()?
                    .ok_or_else(|| error("record commit anchor disappeared"))?;
                let anchored = decode_anchor(&anchored)?;
                if anchored != self.metadata && anchored != next {
                    return Err(error("record recovery anchor changed"));
                }
                let txn = self.db.begin_read().map_err(error)?;
                let table = txn.open_table(META).map_err(error)?;
                let value = table
                    .get(0)
                    .map_err(error)?
                    .ok_or_else(|| error("record metadata missing"))?;
                if exact::<Metadata>(value.value())? != next {
                    return Err(error("record recovery database changed"));
                }
                self.anchor.write(&encode_anchor(&next)?)
            })();
            if let Err(error) = recovered {
                if !matches!(error, DiscoveryError::StorageUnavailable(_)) {
                    self.pending_anchor = None;
                }
                return Err(error);
            }
            self.metadata = next;
            self.pending_anchor = None;
            self.failed = false;
        }
        if self.failed {
            Err(error("record commit outcome uncertain; reopen required"))
        } else {
            Ok(())
        }
    }
}
impl RecordStore for FileStore {
    fn metrics(&self) -> Option<RecordMetrics> {
        Some(RecordMetrics(self.observed.observer()))
    }
    fn put_admitted(
        &self,
        record: &EndpointRecord,
        admit: &mut dyn FnMut(bool) -> Result<(), DiscoveryError>,
    ) -> Result<(), DiscoveryError> {
        self.access_admitted(
            record.verify()?.key,
            Some(Entry::Record(record.clone())),
            None,
            admit,
        )
        .map(|_| ())
    }
    fn get(&self, key: &EndpointKey) -> Result<EndpointRecord, DiscoveryError> {
        self.access(*key, None, None)?
            .ok_or(DiscoveryError::NotFound)
    }
    fn remove_admitted(
        &self,
        tomb: &DeleteRequest,
        admit: &mut dyn FnMut(bool) -> Result<(), DiscoveryError>,
    ) -> Result<(), DiscoveryError> {
        self.access_admitted(
            tomb.verify()?.key,
            Some(Entry::Deleted(tomb.clone())),
            None,
            admit,
        )
        .map(|_| ())
    }
    fn collect_expired(&self) -> Result<usize, DiscoveryError> {
        self.collect_at(None)
    }
    fn len(&self) -> usize {
        self.inner
            .lock()
            .map(|inner| inner.metadata.live as usize)
            .unwrap_or(0)
    }
}
