//! Direct native destination writes keep the original transfer/paste owner.
use std::{fs::File, os::unix::fs::FileExt, sync::Arc};

use super::Error;
use crate::clipboard::NativeLifetime;

/// A newly opened destination supplied by the native paste operation. The
/// caller supplies that operation's original peer/service/purpose owner, not
/// a replacement scope. This type grants no filesystem or network permission.
/// No Debug: destination capabilities are not diagnostic data.
pub struct Destination(Arc<OwnedDestination>);
struct OwnedDestination {
    // Close the FD before releasing its original caller on every drop path.
    file: File,
    _lifetime: NativeLifetime,
}
impl Destination {
    pub fn new(file: File, original: NativeLifetime) -> Self {
        Self(Arc::new(OwnedDestination {
            file,
            _lifetime: original,
        }))
    }
    pub(crate) async fn validate_empty(&self) -> Result<(), Error> {
        self.run(|file| {
            let metadata = file.metadata()?;
            if !metadata.is_file() || metadata.len() != 0 {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "file promise requires a new empty regular destination",
                ));
            }
            Ok(())
        })
        .await
    }
    /// Move the one body chunk into its actual native write and return it for
    /// reuse. A canceled waiter cannot release the FD or original caller while
    /// that syscall runs, and no Tokio file buffer copies it.
    pub(crate) async fn write_chunk(
        &self,
        bytes: Vec<u8>,
        length: usize,
        offset: u64,
    ) -> Result<Vec<u8>, Error> {
        self.run(move |file| {
            file.write_all_at(&bytes[..length], offset)?;
            Ok(bytes)
        })
        .await
    }
    pub(super) async fn run<T: Send + 'static>(
        &self,
        operation: impl FnOnce(&File) -> std::io::Result<T> + Send + 'static,
    ) -> Result<T, Error> {
        let owner = self.0.clone();
        tokio::task::spawn_blocking(move || operation(&owner.file))
            .await
            .map_err(|_| Error::Unavailable)?
            .map_err(Error::Io)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use tokio::sync::oneshot;

    struct OriginalCaller(Option<oneshot::Sender<()>>);
    impl Drop for OriginalCaller {
        fn drop(&mut self) {
            if let Some(done) = self.0.take() {
                let _ = done.send(());
            }
        }
    }

    #[tokio::test]
    async fn canceled_waiter_keeps_native_destination_and_original_caller_until_io_returns() {
        let root = std::env::temp_dir().join(format!(
            "rds-native-destination-{:032x}",
            rand::random::<u128>()
        ));
        std::fs::create_dir(&root).unwrap();
        let output = std::fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(root.join("output"))
            .unwrap();
        let (done, drained) = oneshot::channel();
        let context = Arc::new(OriginalCaller(Some(done)));
        let retained = Arc::downgrade(&context);
        let destination = Destination::new(output, context);
        let (entered, ready) = oneshot::channel();
        let (release, held) = std::sync::mpsc::channel();
        let waiting = tokio::spawn(async move {
            destination
                .run(move |file| {
                    assert!(file.metadata()?.is_file());
                    let _ = entered.send(());
                    // A bounded native fixture owns the same real FD/work path.
                    // No sleep or retry can hide early authority release.
                    held.recv_timeout(Duration::from_secs(3))
                        .map_err(std::io::Error::other)?;
                    assert_eq!(file.metadata()?.len(), 0);
                    Ok(())
                })
                .await
        });
        tokio::time::timeout(Duration::from_secs(2), ready)
            .await
            .unwrap()
            .unwrap();
        waiting.abort();
        assert!(waiting.await.unwrap_err().is_cancelled());
        assert!(
            retained.upgrade().is_some(),
            "native I/O lost its original caller"
        );
        release.send(()).unwrap();
        tokio::time::timeout(Duration::from_secs(2), drained)
            .await
            .unwrap()
            .unwrap();
        assert!(retained.upgrade().is_none());
        std::fs::remove_dir_all(root).unwrap();
    }
}
