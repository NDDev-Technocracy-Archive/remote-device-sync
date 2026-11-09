//! Native clipboard ownership, independent of the viewing/serving role. OS
//! adapters serialize their native objects and return futures; the UI never
//! waits for another application's selection provider or a filesystem call.
use std::{future::Future, pin::Pin, sync::Arc};

use bytes::Bytes;
use rds_core::clipboard::{Capabilities, FileSelection, Format, Limits, Representation};

use super::image::Png;
mod destination;
pub use destination::Destination;
#[cfg(any(
    all(target_os = "linux", feature = "portal"),
    all(target_os = "macos", feature = "host")
))]
mod legacy;
#[cfg(any(
    all(target_os = "linux", feature = "portal"),
    all(target_os = "macos", feature = "host")
))]
pub(crate) use legacy::Legacy;
mod remote;
pub use remote::RemoteFiles;

#[cfg(all(target_os = "linux", feature = "x11"))]
pub use super::x11::Prepared as X11;

/// Prepare the explicit local X11 clipboard edge under its original physical
/// application/backend owner. This never probes Wayland or opens OS consent.
/// Each admitted session then creates its own in-memory `X11::provider` lease.
#[cfg(all(target_os = "linux", feature = "x11"))]
pub async fn prepare_x11(policy: Policy, original: super::NativeLifetime) -> Result<X11, Error> {
    super::x11::prepare(policy, original).await
}

pub type Operation<'a, T> = Pin<Box<dyn Future<Output = Result<T, Error>> + Send + 'a>>;

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Policy {
    pub schema_version: u32,
    pub limits: Limits,
    pub watch_interval_ms: u64,
    pub operation_timeout_ms: u64,
    pub native_uri_bytes: u64,
    pub native_types: usize,
    pub pending_operations: usize,
    /// Process-wide retained native selection FDs, independent of file count
    /// per manifest and simultaneous wire body transfers. New owners only.
    pub selected_file_descriptors: usize,
    /// Concurrent native body jobs, independent of transport stream credits.
    pub native_body_operations: usize,
    pub peer: PeerPolicy,
    pub portal: PortalPolicy,
    pub x11: X11Policy,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PortalPolicy {
    pub selection_chunk_bytes: u32,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PeerPolicy {
    pub metadata_messages: usize,
    pub startup_timeout_ms: u64,
    pub write_timeout_ms: u64,
}
impl PeerPolicy {
    pub fn validate(&self) -> Result<(), Error> {
        if !(1..=64).contains(&self.metadata_messages)
            || !(100..=30_000).contains(&self.startup_timeout_ms)
            || !(100..=30_000).contains(&self.write_timeout_ms)
        {
            return Err(Error::Settings);
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct X11Policy {
    pub selection_transfers: usize,
    pub selection_chunk_bytes: u32,
    pub selection_transfer_timeout_ms: u64,
    pub timestamp_timeout_ms: u64,
    pub legacy_owner_interval_ms: u64,
    pub event_batch: usize,
}
impl Policy {
    /// The standalone common profile is versioned with this application. No
    /// environment lookup, native I/O or second editable default is involved.
    pub fn shipped() -> Result<Self, Error> {
        let policy: Self =
            serde_json::from_str(include_str!("../../config/native-clipboard.json"))?;
        policy.validate()?;
        Ok(policy)
    }
    pub fn validate(&self) -> Result<(), Error> {
        self.limits.validate()?;
        self.peer.validate()?;
        if self.schema_version != 4
            || !(5..=1000).contains(&self.watch_interval_ms)
            || !(100..=30_000).contains(&self.operation_timeout_ms)
            || !(1..=16 * rds_core::clipboard::TEXT_BYTES).contains(&self.native_uri_bytes)
            || !(1..=4096).contains(&self.native_types)
            || !(1..=64).contains(&self.pending_operations)
            || !(1..=4096).contains(&self.selected_file_descriptors)
            || !(1..=64).contains(&self.native_body_operations)
            || !(1024..=1024 * 1024).contains(&self.portal.selection_chunk_bytes)
            || !(1..=64).contains(&self.x11.selection_transfers)
            || !(1024..=1024 * 1024).contains(&self.x11.selection_chunk_bytes)
            || !(100..=30_000).contains(&self.x11.selection_transfer_timeout_ms)
            || !(100..=30_000).contains(&self.x11.timestamp_timeout_ms)
            || !(5..=1000).contains(&self.x11.legacy_owner_interval_ms)
            || !(1..=4096).contains(&self.x11.event_batch)
        {
            return Err(Error::Settings);
        }
        Ok(())
    }
    /// All viewing/serving providers in this process share one physical FD
    /// allowance. Held files keep it alive beyond a native owner replacement.
    pub fn selection_budget(&self) -> Result<Arc<rds_sync::selection::DescriptorBudget>, Error> {
        self.validate()?;
        static FILES: std::sync::OnceLock<Selections> = std::sync::OnceLock::new();
        FILES
            .get_or_init(Selections::default)
            .get(self.selected_file_descriptors)
    }
}

#[derive(Default)]
struct Selections(std::sync::Mutex<std::sync::Weak<rds_sync::selection::DescriptorBudget>>);
impl Selections {
    fn get(&self, maximum: usize) -> Result<Arc<rds_sync::selection::DescriptorBudget>, Error> {
        let mut held = self.0.lock().map_err(|_| Error::Unavailable)?;
        if let Some(budget) = held.upgrade() {
            if budget.maximum() != maximum {
                return Err(Error::Settings);
            }
            return Ok(budget);
        }
        let budget = rds_sync::selection::DescriptorBudget::new(maximum)?;
        *held = Arc::downgrade(&budget);
        Ok(budget)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("invalid native clipboard configuration: {0}")]
    Configuration(#[from] serde_json::Error),
    #[error("invalid native clipboard policy")]
    Settings,
    #[error("native clipboard is unavailable")]
    Unavailable,
    #[error("native clipboard ownership changed")]
    Changed,
    #[error("native clipboard does not offer the requested representation")]
    Unsupported,
    #[error("native clipboard publication was refused")]
    Publication,
    #[error("clipboard source integrity proof failed")]
    Integrity,
    #[error(transparent)]
    Contract(#[from] rds_core::clipboard::Error),
    #[error(transparent)]
    Image(#[from] super::image::Error),
    #[error(transparent)]
    File(#[from] rds_sync::selection::Error),
    #[error("native clipboard I/O failed: {0}")]
    Io(#[from] std::io::Error),
}

#[derive(Clone, Debug)]
pub struct Snapshot {
    pub generation: u64,
    pub owned: bool,
    pub representations: Vec<Representation>,
}

#[derive(Clone)]
pub struct Text(Bytes);
impl Text {
    pub fn new(bytes: Bytes, limits: &Limits) -> Result<Self, Error> {
        limits.validate_value(Format::TextUtf8, &bytes)?;
        Ok(Self(bytes))
    }
    pub fn encoded(&self) -> &Bytes {
        &self.0
    }
    pub fn as_str(&self) -> &str {
        // The private constructor validates UTF-8; Bytes stays immutable.
        std::str::from_utf8(&self.0).expect("validated native UTF-8 remains immutable")
    }
}

/// Native read result. Selected files are held descriptors, never a remote
/// path or an authorization to a whole synchronization root.
pub enum Value {
    Text(Text),
    Image(Png),
    Files(Arc<dyn Files>),
}
impl Value {
    pub fn format(&self) -> Format {
        match self {
            Self::Text(_) => Format::TextUtf8,
            Self::Image(_) => Format::ImagePng,
            Self::Files(_) => Format::Files,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub struct FileReceipt {
    pub bytes: u64,
    pub digest: [u8; 32],
}

impl std::fmt::Debug for FileReceipt {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FileReceipt")
            .field("bytes", &self.bytes)
            .finish_non_exhaustive()
    }
}

/// A native file promise can ask only for an item in its pinned manifest. Its
/// owned local destination came from the receiving application's native paste
/// operation. The implementation retains its exact session/authority and all
/// native I/O until completion or cancellation drain.
pub trait Files: Send + Sync {
    fn selection(&self) -> &FileSelection;
    fn limits(&self) -> &Limits;
    /// Bind this read's original paste/forwarding owner before any native I/O.
    /// Native adapters keep it, including budget reservations, in actual
    /// blocking operations after an async waiter has been canceled.
    fn open(&self, file: [u8; 16], original: super::NativeLifetime) -> Operation<'_, FileRead>;
    fn write(&self, file: [u8; 16], destination: Destination) -> Operation<'_, FileReceipt> {
        Box::pin(write_file(self, file, destination))
    }
}

/// A file can originate locally or in another remote tab. Forwarding its reader
/// never requires a local staging copy. After the exact body is consumed,
/// `complete` must verify its native version or upstream integrity and drain
/// its owner before a downstream stream/native destination is committed.
pub struct FileRead {
    pub bytes: u64,
    pub reader: Pin<Box<dyn tokio::io::AsyncRead + Send>>,
    pub complete: Operation<'static, ()>,
}

/// Publication is separate from a source read: file promises write directly to
/// a native destination; adapters which require URIs own their bounded staging.
pub enum Publication {
    Text(Text),
    Image(Png),
    Files(Arc<dyn Files>),
}

pub trait Provider: Send + Sync {
    /// Seal admission and drain actual native operations for this lease. The
    /// owner drops offered file capabilities before awaiting this boundary.
    fn close(&self) -> super::ClipboardFuture<'_, ()>;
    fn capabilities(&self) -> Capabilities;
    fn current(&self) -> Operation<'_, Snapshot>;
    /// The actual native call retains this operation's original owner. A
    /// canceled waiter cannot release its transport/native admission early.
    fn read(
        &self,
        generation: u64,
        format: Format,
        original: super::NativeLifetime,
    ) -> Operation<'_, Value>;
    fn publish(
        &self,
        expected_generation: u64,
        value: Publication,
        original: super::NativeLifetime,
    ) -> Operation<'_, Snapshot>;
    fn changed(&self, after_generation: u64) -> Operation<'_, Snapshot>;
}

impl Files for rds_sync::selection::Selection {
    fn selection(&self) -> &FileSelection {
        self.manifest()
    }
    fn limits(&self) -> &Limits {
        self.limits()
    }
    fn open(&self, file: [u8; 16], original: super::NativeLifetime) -> Operation<'_, FileRead> {
        Box::pin(async move {
            let selected = self.file(&file)?;
            let bytes = selected.bytes();
            let reader = selected
                .clone()
                .reader(self.limits().clone(), original.clone())
                .await?;
            Ok(FileRead {
                bytes,
                reader: Box::pin(reader),
                complete: Box::pin(async move {
                    let owners = (selected, original);
                    tokio::task::spawn_blocking(move || {
                        let held = owners;
                        held.0.verify()
                    })
                    .await
                    .map_err(|_| Error::Unavailable)??;
                    Ok(())
                }),
            })
        })
    }
}

async fn write_file<F: Files + ?Sized>(
    source: &F,
    file: [u8; 16],
    destination: Destination,
) -> Result<FileReceipt, Error> {
    let limits = source.limits();
    source.selection().validate(limits)?;
    let expected = source
        .selection()
        .entries
        .iter()
        .find(|entry| entry.id == file)
        .ok_or(rds_sync::selection::Error::Unknown)?
        .bytes;
    destination.validate_empty().await?;
    let mut read = source.open(file, destination.lifetime()).await?;
    if read.bytes != expected {
        return Err(rds_sync::selection::Error::Changed.into());
    }
    let mut bytes = vec![0; limits.chunk_bytes as usize];
    let mut total = 0u64;
    let mut hash = blake3::Hasher::new();
    use tokio::io::AsyncReadExt;
    while total < expected {
        let maximum = (expected - total).min(bytes.len() as u64) as usize;
        let length = read.reader.read(&mut bytes[..maximum]).await?;
        if length == 0 {
            return Err(rds_sync::selection::Error::Changed.into());
        }
        hash.update(&bytes[..length]);
        bytes = destination.write_chunk(bytes, length, total).await?;
        total += length as u64;
    }
    if read.reader.read(&mut [0u8; 1]).await? != 0 {
        return Err(rds_sync::selection::Error::Changed.into());
    }
    read.complete.await?;
    Ok(FileReceipt {
        bytes: total,
        digest: *hash.finalize().as_bytes(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replacement_native_owners_share_the_retained_descriptor_allowance() {
        let pool = Selections::default();
        let budget = pool.get(1).unwrap();
        let root = std::env::temp_dir().join(format!(
            "rds-selection-budget-{:032x}",
            rand::random::<u128>()
        ));
        std::fs::create_dir(&root).unwrap();
        let path = root.join("selected");
        std::fs::write(&path, b"native selection").unwrap();
        let selection =
            rds_sync::selection::Selection::from_paths([path], &Limits::default(), &budget)
                .unwrap();
        let file = selection.file(&selection.manifest().entries[0].id).unwrap();
        drop(selection);
        drop(budget);
        assert_eq!(pool.get(1).unwrap().available(), 0);
        assert!(matches!(pool.get(2), Err(Error::Settings)));
        file.verify().unwrap();
        drop(file);
        assert_eq!(pool.get(2).unwrap().available(), 2);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn native_settings_are_complete_versioned_and_independent_of_wire_limits() {
        let policy = Policy::shipped().unwrap();
        let value = serde_json::to_value(&policy).unwrap();
        for field in value.as_object().unwrap().keys() {
            let mut absent = value.clone();
            absent.as_object_mut().unwrap().remove(field);
            assert!(
                serde_json::from_value::<Policy>(absent).is_err(),
                "missing {field}"
            );
        }
        for field in value["limits"].as_object().unwrap().keys() {
            let mut absent = value.clone();
            absent["limits"].as_object_mut().unwrap().remove(field);
            assert!(
                serde_json::from_value::<Policy>(absent).is_err(),
                "missing limit {field}"
            );
        }
        for field in value["x11"].as_object().unwrap().keys() {
            let mut absent = value.clone();
            absent["x11"].as_object_mut().unwrap().remove(field);
            assert!(
                serde_json::from_value::<Policy>(absent).is_err(),
                "missing X11 setting {field}"
            );
        }
        let mut unknown = value.clone();
        unknown["x11"]["unknown"] = serde_json::Value::Bool(true);
        assert!(serde_json::from_value::<Policy>(unknown).is_err());
        let mut absent = value.clone();
        absent["portal"]
            .as_object_mut()
            .unwrap()
            .remove("selection_chunk_bytes");
        assert!(serde_json::from_value::<Policy>(absent).is_err());
        let mut unknown = value.clone();
        unknown["portal"]["unknown"] = serde_json::Value::Bool(true);
        assert!(serde_json::from_value::<Policy>(unknown).is_err());
        let mut selected = policy.clone();
        selected.selected_file_descriptors = 1;
        selected.native_body_operations = 3;
        selected.validate().unwrap();
        assert_eq!(selected.limits, policy.limits);
        selected.schema_version = 5;
        assert!(matches!(selected.validate(), Err(Error::Settings)));
        selected.schema_version = 3;
        assert!(matches!(selected.validate(), Err(Error::Settings)));
        for field in value["peer"].as_object().unwrap().keys() {
            let mut absent = value.clone();
            absent["peer"].as_object_mut().unwrap().remove(field);
            assert!(serde_json::from_value::<Policy>(absent).is_err());
        }
        let mut unknown = value.clone();
        unknown["peer"]["unknown"] = serde_json::Value::Bool(true);
        assert!(serde_json::from_value::<Policy>(unknown).is_err());
        selected = policy.clone();
        selected.peer.metadata_messages = 0;
        assert!(matches!(selected.validate(), Err(Error::Settings)));
        selected = policy.clone();
        selected.x11.event_batch = 0;
        assert!(matches!(selected.validate(), Err(Error::Settings)));
        selected = policy.clone();
        selected.x11.selection_chunk_bytes = 0;
        assert!(matches!(selected.validate(), Err(Error::Settings)));
        selected = policy.clone();
        selected.portal.selection_chunk_bytes = 0;
        assert!(matches!(selected.validate(), Err(Error::Settings)));
    }

    #[tokio::test]
    async fn native_file_promise_writes_only_its_selected_object_to_one_destination() {
        let root =
            std::env::temp_dir().join(format!("rds-file-promise-{:032x}", rand::random::<u128>()));
        std::fs::create_dir(&root).unwrap();
        let source = root.join("source");
        let destination = root.join("pasted");
        let value = "Привет 🖥️\n".repeat(10000).into_bytes();
        std::fs::write(&source, &value).unwrap();
        let selected = rds_sync::selection::Selection::from_paths(
            [source],
            &Limits {
                chunk_bytes: 4096,
                ..Limits::default()
            },
            &rds_sync::selection::DescriptorBudget::new(1).unwrap(),
        )
        .unwrap();
        let file = selected.manifest().entries[0].id;
        let output = std::fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&destination)
            .unwrap();
        let receipt = selected
            .write(file, Destination::new(output, Arc::new(())))
            .await
            .unwrap();
        assert_eq!(receipt.bytes, value.len() as u64);
        assert_eq!(receipt.digest, *blake3::hash(&value).as_bytes());
        assert_eq!(std::fs::read(&destination).unwrap(), value);
        let existing = std::fs::OpenOptions::new()
            .write(true)
            .open(&destination)
            .unwrap();
        assert!(
            selected
                .write(file, Destination::new(existing, Arc::new(())))
                .await
                .is_err()
        );
        assert_eq!(std::fs::read_dir(&root).unwrap().count(), 2);
        drop(selected);
        std::fs::remove_dir_all(root).unwrap();
    }
}
