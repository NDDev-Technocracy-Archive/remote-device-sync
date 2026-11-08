//! Files explicitly selected by a native application. This grants only held
//! regular-file descriptors and opaque IDs, independently of configured sync
//! roots. There is no path-based request, chunk cache or manifest pre-read.
use std::collections::BTreeMap;
use std::fs::{File, Metadata};
use std::io;
use std::os::unix::fs::{FileExt, MetadataExt};
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use tokio::io::{AsyncRead, ReadBuf};

use rds_core::clipboard::{FileEntry, FileSelection, Limits};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Contract(#[from] rds_core::clipboard::Error),
    #[error("selected file I/O failed: {0}")]
    Io(#[from] io::Error),
    #[error("selection requires a regular file")]
    NotRegular,
    #[error("selected file changed during transfer")]
    Changed,
    #[error("file identity is absent from this selection")]
    Unknown,
}

/// No Debug implementation: native file capabilities are not diagnostic data.
pub struct Selection {
    manifest: FileSelection,
    files: BTreeMap<[u8; 16], Arc<SelectedFile>>,
    limits: Limits,
}

impl Selection {
    /// Local native selection only. Never feed a peer-supplied path to this
    /// method. FIFO/device/symlink leaves are refused without a blocking open.
    pub fn from_paths(
        paths: impl IntoIterator<Item = std::path::PathBuf>,
        limits: &Limits,
    ) -> Result<Self, Error> {
        Self::admit_paths(paths, limits, None)
    }

    /// Native resource URLs/portal selections may need their resource scope
    /// retained beyond the selection owner. Paths still come only from the
    /// local native selection, never a remote manifest or request.
    pub fn from_paths_with_authority(
        paths: impl IntoIterator<Item = std::path::PathBuf>,
        limits: &Limits,
        authority: Arc<dyn Send + Sync>,
    ) -> Result<Self, Error> {
        Self::admit_paths(paths, limits, Some(authority))
    }

    fn admit_paths(
        paths: impl IntoIterator<Item = std::path::PathBuf>,
        limits: &Limits,
        authority: Option<Arc<dyn Send + Sync>>,
    ) -> Result<Self, Error> {
        limits.validate()?;
        let mut files = Vec::new();
        for path in paths {
            if files.len() == limits.files as usize {
                return Err(rds_core::clipboard::Error::TooLarge.into());
            }
            if !path.is_absolute() {
                return Err(rds_core::clipboard::Error::FileName.into());
            }
            let name = path
                .file_name()
                .and_then(|name| name.to_str())
                .ok_or(rds_core::clipboard::Error::FileName)?
                .to_owned();
            let file = File::from(
                rustix::fs::open(
                    &path,
                    rustix::fs::OFlags::RDONLY
                        | rustix::fs::OFlags::NONBLOCK
                        | rustix::fs::OFlags::NOFOLLOW
                        | rustix::fs::OFlags::CLOEXEC,
                    rustix::fs::Mode::empty(),
                )
                .map_err(io::Error::from)?,
            );
            files.push((name, file));
        }
        Self::admit_files(authority, files, limits)
    }

    /// The native adapter opens the explicitly selected files. No file name
    /// supplied by a remote peer ever reaches an OS open operation here.
    /// Call on a native blocking owner: metadata can require filesystem I/O.
    pub fn from_files(files: Vec<(String, File)>, limits: &Limits) -> Result<Self, Error> {
        Self::admit_files(None, files, limits)
    }

    /// Retain the native selection's original resource and caller authority
    /// through every selected descriptor and reader, including blocking reads
    /// which outlive cancellation of their async waiter. This opaque owner
    /// supplies lifetime only; it never authorizes an additional path.
    pub fn from_files_with_authority(
        files: Vec<(String, File)>,
        limits: &Limits,
        authority: Arc<dyn Send + Sync>,
    ) -> Result<Self, Error> {
        Self::admit_files(Some(authority), files, limits)
    }

    fn admit_files(
        // Parameters drop in reverse order on early errors: close all input
        // descriptors before releasing their native selection authority.
        authority: Option<Arc<dyn Send + Sync>>,
        files: Vec<(String, File)>,
        limits: &Limits,
    ) -> Result<Self, Error> {
        limits.validate()?;
        if files.len() > limits.files as usize {
            return Err(rds_core::clipboard::Error::TooLarge.into());
        }
        let mut selection = Self {
            manifest: FileSelection {
                entries: Vec::with_capacity(files.len()),
            },
            files: BTreeMap::new(),
            limits: limits.clone(),
        };
        for (name, file) in files {
            let metadata = file.metadata()?;
            if !metadata.is_file() {
                return Err(Error::NotRegular);
            }
            let id = loop {
                let id = rand::random();
                if id != [0; 16] && !selection.files.contains_key(&id) {
                    break id;
                }
            };
            let entry = FileEntry {
                id,
                name,
                bytes: metadata.len(),
            };
            selection.manifest.entries.push(entry);
            selection.files.insert(
                id,
                Arc::new(SelectedFile {
                    file,
                    metadata,
                    _authority: authority.clone(),
                }),
            );
        }
        selection.manifest.validate(limits)?;
        Ok(selection)
    }

    pub fn manifest(&self) -> &FileSelection {
        &self.manifest
    }

    pub fn limits(&self) -> &Limits {
        &self.limits
    }

    pub fn file(&self, id: &[u8; 16]) -> Result<Arc<SelectedFile>, Error> {
        self.files.get(id).cloned().ok_or(Error::Unknown)
    }
}

pub struct SelectedFile {
    // Fields drop in this order: release the FD before its native authority.
    file: File,
    metadata: Metadata,
    _authority: Option<Arc<dyn Send + Sync>>,
}

impl SelectedFile {
    pub fn bytes(&self) -> u64 {
        self.metadata.len()
    }

    /// Verify before and after the admitted copy. Modification is a failed
    /// transfer, never a successful receipt for a silently changed source.
    pub fn verify(&self) -> Result<(), Error> {
        let now = self.file.metadata()?;
        if now.dev() != self.metadata.dev()
            || now.ino() != self.metadata.ino()
            || now.len() != self.metadata.len()
            || now.mtime() != self.metadata.mtime()
            || now.mtime_nsec() != self.metadata.mtime_nsec()
        {
            return Err(Error::Changed);
        }
        Ok(())
    }

    /// Positional reads never share an FD cursor between requests. The held
    /// descriptor remains authoritative when its original path is replaced.
    /// Caller runs this on a blocking owner retained until native I/O drains.
    pub fn read_chunk(
        &self,
        offset: u64,
        requested: usize,
        limits: &Limits,
    ) -> Result<Vec<u8>, Error> {
        limits.validate()?;
        if requested > limits.chunk_bytes as usize {
            return Err(rds_core::clipboard::Error::TooLarge.into());
        }
        if offset > self.bytes() {
            return Err(Error::Changed);
        }
        if offset == 0 {
            self.verify()?;
        }
        let length = (self.bytes() - offset).min(requested as u64) as usize;
        let mut bytes = vec![0; length];
        let mut read = 0;
        while read < length {
            let n = match self.file.read_at(&mut bytes[read..], offset + read as u64) {
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                result => result?,
            };
            if n == 0 {
                return Err(Error::Changed);
            }
            read += n;
        }
        if offset + length as u64 == self.bytes() {
            self.verify()?;
        }
        Ok(bytes)
    }

    pub async fn reader(
        self: Arc<Self>,
        limits: Limits,
        original: Arc<dyn Send + Sync>,
    ) -> Result<Reader, Error> {
        limits.validate()?;
        native(self.clone(), original.clone(), |file| file.verify())
            .await
            .map_err(|_| io::Error::other("selected-file native check ended"))??;
        Ok(Reader {
            file: self,
            limits,
            offset: 0,
            state: ReadState::Idle,
            original,
        })
    }
}

enum ReadState {
    Idle,
    Reading(tokio::task::JoinHandle<Result<Vec<u8>, Error>>),
    Ready { bytes: Vec<u8>, consumed: usize },
}

/// One positional native read in flight, at most one admitted chunk retained.
/// Dropping a reader never closes a descriptor underneath native I/O: the
/// blocking job retains the selected file until its current operation drains.
pub struct Reader {
    file: Arc<SelectedFile>,
    limits: Limits,
    offset: u64,
    state: ReadState,
    original: Arc<dyn Send + Sync>,
}

impl AsyncRead for Reader {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buffer: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        use std::future::Future;
        if buffer.remaining() == 0 || self.offset == self.file.bytes() {
            return Poll::Ready(Ok(()));
        }
        loop {
            match &mut self.state {
                ReadState::Idle => {
                    let limits = self.limits.clone();
                    let length = buffer.remaining().min(limits.chunk_bytes as usize);
                    let offset = self.offset;
                    self.state = ReadState::Reading(native(
                        self.file.clone(),
                        self.original.clone(),
                        move |file| file.read_chunk(offset, length, &limits),
                    ));
                }
                ReadState::Reading(task) => match Pin::new(task).poll(cx) {
                    Poll::Pending => return Poll::Pending,
                    Poll::Ready(Ok(Ok(bytes))) => {
                        self.state = ReadState::Ready { bytes, consumed: 0 }
                    }
                    Poll::Ready(Ok(Err(error))) => {
                        self.state = ReadState::Idle;
                        return Poll::Ready(Err(io::Error::other(error)));
                    }
                    Poll::Ready(Err(_)) => {
                        self.state = ReadState::Idle;
                        return Poll::Ready(Err(io::Error::other(
                            "selected-file native reader ended",
                        )));
                    }
                },
                ReadState::Ready { bytes, consumed } => {
                    let length = (bytes.len() - *consumed).min(buffer.remaining());
                    buffer.put_slice(&bytes[*consumed..*consumed + length]);
                    *consumed += length;
                    if *consumed == bytes.len() {
                        self.state = ReadState::Idle;
                    }
                    self.offset += length as u64;
                    return Poll::Ready(Ok(()));
                }
            }
        }
    }
}

fn native<T: Send + 'static>(
    file: Arc<SelectedFile>,
    original: Arc<dyn Send + Sync>,
    operation: impl FnOnce(&SelectedFile) -> Result<T, Error> + Send + 'static,
) -> tokio::task::JoinHandle<Result<T, Error>> {
    let owners = (file, original);
    tokio::task::spawn_blocking(move || {
        // Capture the whole pair, not just its file field: every actual
        // syscall retains the original read/paste and body reservation.
        let held = owners;
        operation(&held.0)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    struct Root(std::path::PathBuf);
    impl Drop for Root {
        fn drop(&mut self) {
            std::fs::remove_dir_all(&self.0).unwrap();
        }
    }
    fn root() -> Root {
        let path =
            std::env::temp_dir().join(format!("rds-selected-{:032x}", rand::random::<u128>()));
        std::fs::create_dir(&path).unwrap();
        Root(path)
    }

    #[tokio::test]
    async fn canceled_native_read_retains_its_file_and_body_reservation_until_return() {
        let root = root();
        let path = root.0.join("source");
        std::fs::write(&path, b"selected").unwrap();
        let selection = Selection::from_paths([path], &Limits::default()).unwrap();
        let file = selection.file(&selection.manifest().entries[0].id).unwrap();
        let budget = Arc::new(tokio::sync::Semaphore::new(1));
        let original = Arc::new(budget.clone().acquire_owned().await.unwrap());
        let retained = Arc::downgrade(&original);
        let (entered, ready) = tokio::sync::oneshot::channel();
        let (release, held) = std::sync::mpsc::channel();
        let (finished, drained) = tokio::sync::oneshot::channel();
        let task = native(file, original, move |file| {
            file.verify()?;
            entered.send(()).unwrap();
            held.recv_timeout(std::time::Duration::from_secs(3))
                .unwrap();
            assert_eq!(file.read_chunk(0, 8, &Limits::default())?, b"selected");
            finished.send(()).unwrap();
            Ok(())
        });
        ready.await.unwrap();
        task.abort();
        drop(task);
        drop(selection);
        assert!(retained.upgrade().is_some());
        assert!(budget.try_acquire().is_err());
        release.send(()).unwrap();
        drained.await.unwrap();
        let _drained = tokio::time::timeout(std::time::Duration::from_secs(2), budget.acquire())
            .await
            .unwrap()
            .unwrap();
        assert!(retained.upgrade().is_none());
    }

    #[test]
    fn a_replaced_source_path_never_grants_the_replacement_or_an_unknown_file() {
        let root = root();
        let path = root.0.join("original");
        std::fs::write(&path, b"selected").unwrap();
        let limits = Limits::default();
        let selection = Selection::from_files(
            vec![("copy.txt".into(), File::open(&path).unwrap())],
            &limits,
        )
        .unwrap();
        let id = selection.manifest().entries[0].id;
        let selected = selection.file(&id).unwrap();
        std::fs::rename(&path, root.0.join("moved")).unwrap();
        std::fs::write(&path, b"unselected private data").unwrap();
        selected.verify().unwrap();
        assert_eq!(selected.read_chunk(0, 128, &limits).unwrap(), b"selected");
        assert!(matches!(selection.file(&[0; 16]), Err(Error::Unknown)));
        assert_eq!(selected.read_chunk(3, 2, &limits).unwrap(), b"ec");
        assert_eq!(selected.read_chunk(0, 2, &limits).unwrap(), b"se");
        drop(selection); // A running native reader still owns its exact file.
        assert_eq!(selected.read_chunk(6, 2, &limits).unwrap(), b"ed");
    }

    #[tokio::test]
    async fn async_readers_keep_independent_offsets_and_obey_a_small_chunk_budget() {
        use tokio::io::AsyncReadExt;
        let root = root();
        let path = root.0.join("source");
        let value = "Привет 🖥️\n".repeat(1000).into_bytes();
        std::fs::write(&path, &value).unwrap();
        let limits = Limits {
            chunk_bytes: 7,
            ..Limits::default()
        };
        let authority = Arc::new(());
        let retained = Arc::downgrade(&authority);
        let selection = Selection::from_files_with_authority(
            vec![("source".into(), File::open(&path).unwrap())],
            &limits,
            authority,
        )
        .unwrap();
        let file = selection.file(&selection.manifest().entries[0].id).unwrap();
        let (mut first, mut second) = (
            file.clone()
                .reader(limits.clone(), Arc::new(()))
                .await
                .unwrap(),
            file.reader(limits, Arc::new(())).await.unwrap(),
        );
        drop(selection);
        assert!(
            retained.upgrade().is_some(),
            "reader lost original native authority"
        );
        let (mut a, mut b) = (Vec::new(), Vec::new());
        let (ra, rb) = tokio::join!(first.read_to_end(&mut a), second.read_to_end(&mut b));
        assert_eq!(ra.unwrap(), value.len());
        assert_eq!(rb.unwrap(), value.len());
        assert_eq!(a, value);
        assert_eq!(b, value);
        drop(first);
        assert!(
            retained.upgrade().is_some(),
            "second reader lost its native authority"
        );
        drop(second);
        assert!(
            retained.upgrade().is_none(),
            "drained readers retained native authority"
        );
        assert_eq!(std::fs::read_dir(&root.0).unwrap().count(), 1);
    }

    #[test]
    fn quotas_names_and_mutations_are_rejected_without_a_cache_or_pre_read() {
        let root = root();
        let path = root.0.join("source");
        std::fs::write(&path, b"selected").unwrap();
        let limits = Limits::default();
        assert!(
            Selection::from_files(
                vec![("../escape".into(), File::open(&path).unwrap())],
                &limits
            )
            .is_err()
        );
        assert!(
            Selection::from_files(
                vec![("source".into(), File::open(&path).unwrap())],
                &Limits {
                    selection_bytes: 7,
                    ..limits.clone()
                }
            )
            .is_err()
        );
        let selection =
            Selection::from_files(vec![("source".into(), File::open(&path).unwrap())], &limits)
                .unwrap();
        let selected = selection.file(&selection.manifest().entries[0].id).unwrap();
        selected.verify().unwrap();
        assert!(
            selected
                .read_chunk(0, limits.chunk_bytes as usize + 1, &limits)
                .is_err()
        );
        let mut writer = std::fs::OpenOptions::new()
            .write(true)
            .truncate(true)
            .open(&path)
            .unwrap();
        writer.write_all(b"new").unwrap();
        assert!(matches!(selected.verify(), Err(Error::Changed)));
        assert!(matches!(
            selected.read_chunk(0, 8, &limits),
            Err(Error::Changed)
        ));
        assert_eq!(std::fs::read_dir(&root.0).unwrap().count(), 1);
    }

    #[test]
    fn native_selection_rejects_relative_symlink_and_fifo_paths_before_content_io() {
        let root = root();
        let file = root.0.join("source");
        std::fs::write(&file, b"selected").unwrap();
        let link = root.0.join("link");
        std::os::unix::fs::symlink(&file, &link).unwrap();
        let fifo = root.0.join("fifo");
        // POSIX fixture on both macOS and Linux; no helper in product I/O.
        assert!(
            std::process::Command::new("mkfifo")
                .arg(&fifo)
                .status()
                .unwrap()
                .success()
        );
        for path in [
            std::path::PathBuf::from("relative"),
            link,
            fifo,
            root.0.clone(),
        ] {
            assert!(Selection::from_paths([path], &Limits::default()).is_err());
        }
    }
}
