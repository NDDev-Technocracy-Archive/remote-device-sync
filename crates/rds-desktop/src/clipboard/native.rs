//! Native clipboard ownership, independent of the viewing/serving role. OS
//! adapters serialize their native objects and return futures; the UI never
//! waits for another application's selection provider or a filesystem call.
use std::{future::Future, pin::Pin, sync::Arc};

use bytes::Bytes;
use rds_core::clipboard::{Capabilities, FileSelection, Format, Limits, Representation};

use super::image::Png;
mod destination;
pub use destination::Destination;
mod remote;
pub use remote::RemoteFiles;

pub type Operation<'a, T> = Pin<Box<dyn Future<Output = Result<T, Error>> + Send + 'a>>;

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Policy {
    pub limits: Limits,
    pub watch_interval_ms: u64,
    pub operation_timeout_ms: u64,
    pub native_uri_bytes: u64,
    pub native_types: usize,
    pub pending_operations: usize,
}
impl Default for Policy {
    fn default() -> Self {
        Self {
            limits: Limits::default(),
            watch_interval_ms: 100,
            operation_timeout_ms: 2000,
            native_uri_bytes: rds_core::clipboard::TEXT_BYTES,
            native_types: 64,
            pending_operations: 8,
        }
    }
}
impl Policy {
    pub fn validate(&self) -> Result<(), Error> {
        self.limits.validate()?;
        if !(5..=1000).contains(&self.watch_interval_ms)
            || !(100..=30_000).contains(&self.operation_timeout_ms)
            || !(1..=16 * rds_core::clipboard::TEXT_BYTES).contains(&self.native_uri_bytes)
            || !(1..=4096).contains(&self.native_types)
            || !(1..=64).contains(&self.pending_operations)
        {
            return Err(Error::Settings);
        }
        Ok(())
    }
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
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
    fn read(&self, generation: u64, format: Format) -> Operation<'_, Value>;
    fn publish(&self, expected_generation: u64, value: Publication) -> Operation<'_, Snapshot>;
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
