//! One same-UID read interface per viewer; snapshots remain in bounded memory.
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use rds_client::local::socket;
use rds_net::{read_frame, write_frame};
use serde::{Deserialize, Serialize};
use tokio::{net::UnixListener, sync::Semaphore, task::JoinSet};
use tokio_util::sync::CancellationToken;

const VERSION: u16 = 1;
const MAX_SCOPES: usize = rds_desktop::render::workspace::MAX_TABS;
const MAX_SNAPSHOT_BYTES: usize = 60 * 1024;
const MAX_READERS: usize = 8;
const QUERY_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum Scope {
    Window,
    Tab { id: u64, revision: u64 },
}

impl From<Option<(u64, u64)>> for Scope {
    fn from(scope: Option<(u64, u64)>) -> Self {
        scope.map_or(Self::Window, |(id, revision)| Self::Tab { id, revision })
    }
}

#[derive(Serialize, Deserialize)]
enum Query {
    Scopes,
    Snapshot(Scope),
}

#[derive(Serialize, Deserialize)]
enum Reply {
    Scopes(Vec<Scope>),
    Snapshot(Option<Vec<u8>>),
}

#[derive(Serialize, Deserialize)]
struct Request {
    version: u16,
    query: Query,
}

#[derive(Serialize, Deserialize)]
struct Response {
    version: u16,
    reply: Reply,
}

type Samples = BTreeMap<Scope, Option<Arc<[u8]>>>;

#[derive(Clone, Default)]
pub struct Handle(Arc<State>);

#[derive(Default)]
struct State {
    samples: Mutex<Samples>,
    available: AtomicBool,
}

impl Handle {
    pub(super) fn available(&self) -> bool {
        self.0.available.load(Ordering::Relaxed)
    }
    pub(super) fn register(&self, scope: Scope) -> anyhow::Result<Registration> {
        let mut samples = self
            .0
            .samples
            .lock()
            .map_err(|_| anyhow::anyhow!("live observation lock failed"))?;
        anyhow::ensure!(
            samples.len() < MAX_SCOPES,
            "live observation capacity reached"
        );
        anyhow::ensure!(
            !samples.contains_key(&scope),
            "observation scope already owned"
        );
        samples.insert(scope, None);
        Ok(Registration {
            handle: self.clone(),
            scope,
        })
    }
}

pub(super) struct Registration {
    handle: Handle,
    scope: Scope,
}

impl Registration {
    pub(super) fn publish(&self, bytes: &[u8]) -> anyhow::Result<()> {
        anyhow::ensure!(
            bytes.len() <= MAX_SNAPSHOT_BYTES,
            "live observation exceeds its bound"
        );
        let mut samples = self
            .handle
            .0
            .samples
            .lock()
            .map_err(|_| anyhow::anyhow!("live observation lock failed"))?;
        let slot = samples
            .get_mut(&self.scope)
            .ok_or_else(|| anyhow::anyhow!("observation ended"))?;
        *slot = Some(Arc::from(bytes));
        Ok(())
    }
}

impl Drop for Registration {
    fn drop(&mut self) {
        self.handle
            .0
            .samples
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&self.scope);
    }
}

/// Keep this owner until all observation workers end. Drop cancels admission;
/// explicit close also waits for in-flight reads and removes only its socket.
pub(super) struct Run {
    stop: CancellationToken,
    task: Option<tokio::task::JoinHandle<anyhow::Result<()>>>,
}

pub fn directory(root: &Path, viewer: u32) -> PathBuf {
    root.join(format!("live-{viewer}"))
}

impl Run {
    pub(super) async fn start(options: &mut super::Options) -> anyhow::Result<Option<Self>> {
        options.diagnostics.validate()?;
        options
            .diagnostic_started
            .get_or_insert_with(std::time::Instant::now);
        if options.live_diagnostics.is_some() {
            return Ok(None);
        }
        let Some(root) = &options.diagnostics_dir else {
            return Ok(None);
        };
        let directory = directory(root, std::process::id());
        let handle = Handle::default();
        options.live_diagnostics = Some(handle.clone());
        let stop = CancellationToken::new();
        let worker_stop = stop.clone();
        let task = tokio::spawn(async move {
            // Keep native directory/lock work owned through cancellation. The
            // setup task never blocks the UI, and a late bind is closed below.
            let binding = socket::SocketOwner::bind(directory.clone()).await;
            let (owner, listener) = match binding {
                Ok(binding) => binding,
                Err(error) => {
                    tracing::warn!(%error,"live observation interface unavailable; desktop remains active");
                    return Ok(());
                }
            };
            use std::os::unix::fs::MetadataExt;
            let metadata = tokio::fs::symlink_metadata(&directory).await?;
            let inode = (metadata.dev(), metadata.ino());
            let result = serve(owner, listener, handle, worker_stop).await;
            if let Ok(metadata) = tokio::fs::symlink_metadata(&directory).await
                && metadata.is_dir()
                && (metadata.dev(), metadata.ino()) == inode
            {
                let _ = tokio::fs::remove_dir(&directory).await;
            }
            result
        });
        Ok(Some(Self {
            stop,
            task: Some(task),
        }))
    }

    pub(super) async fn close(mut self) {
        self.stop.cancel();
        if let Some(task) = self.task.take() {
            match task.await {
                Ok(Ok(())) => {}
                error => tracing::warn!(?error, "live observation interface ended"),
            }
        }
    }
}

impl Drop for Run {
    fn drop(&mut self) {
        self.stop.cancel();
    }
}

async fn serve(
    _owner: socket::SocketOwner,
    listener: UnixListener,
    samples: Handle,
    stop: CancellationToken,
) -> anyhow::Result<()> {
    struct Available(Handle);
    impl Drop for Available {
        fn drop(&mut self) {
            self.0.0.available.store(false, Ordering::Relaxed);
        }
    }
    let _available = Available(samples.clone());
    samples.0.available.store(true, Ordering::Relaxed);
    let readers = Arc::new(Semaphore::new(MAX_READERS));
    let mut tasks = JoinSet::new();
    loop {
        tokio::select! {
            biased;
            _ = stop.cancelled() => break,
            Some(_) = tasks.join_next(), if !tasks.is_empty() => {},
            accepted = listener.accept() => {
                let (mut stream, _) = accepted?;
                if socket::authenticate(&stream).is_err() { continue; }
                let Ok(permit) = readers.clone().try_acquire_owned() else { continue; };
                let samples = samples.clone();
                tasks.spawn(async move {
                    let _permit = permit;
                    let _ = tokio::time::timeout(QUERY_TIMEOUT, async {
                        let request=read_frame::<_, Request>(&mut stream).await?;
                        if request.version != VERSION {return Err(std::io::Error::other("unsupported observation protocol"));}
                        let reply = match request.query {
                            Query::Scopes => Reply::Scopes(samples.0.samples.lock()
                                .map_err(|_| std::io::Error::other("observation unavailable"))?
                                .keys().copied().collect()),
                            Query::Snapshot(scope) => {
                                let bytes = samples.0.samples.lock()
                                    .map_err(|_| std::io::Error::other("observation unavailable"))?
                                    .get(&scope).and_then(Clone::clone);
                                Reply::Snapshot(bytes.map(|b| b.to_vec()))
                            },
                        };
                        write_frame(&mut stream, &Response {version:VERSION,reply}).await
                    }).await;
                });
            }
        }
    }
    tasks.shutdown().await;
    Ok(())
}

async fn query(path: &Path, request: Query) -> anyhow::Result<Reply> {
    tokio::time::timeout(QUERY_TIMEOUT, async {
        let mut stream = socket::connect(path).await?;
        write_frame(
            &mut stream,
            &Request {
                version: VERSION,
                query: request,
            },
        )
        .await?;
        let response = read_frame::<_, Response>(&mut stream).await?;
        anyhow::ensure!(
            response.version == VERSION,
            "unsupported observation protocol"
        );
        Ok::<_, anyhow::Error>(response.reply)
    })
    .await
    .map_err(|_| anyhow::anyhow!("live observation query timed out"))?
}

/// Reads current metadata only. No key, endpoint, remote command or clipboard
/// content is loaded; absent/expired scopes remain explicitly unavailable.
pub async fn read(path: &Path) -> anyhow::Result<serde_json::Value> {
    let Reply::Scopes(scopes) = query(path, Query::Scopes).await? else {
        anyhow::bail!("unexpected live observation reply")
    };
    anyhow::ensure!(
        scopes.len() <= MAX_SCOPES,
        "invalid observation scope count"
    );
    let mut values = Vec::with_capacity(scopes.len());
    for scope in scopes {
        let Reply::Snapshot(bytes) = query(path, Query::Snapshot(scope)).await? else {
            anyhow::bail!("unexpected live observation reply")
        };
        let value = bytes
            .map(|bytes| {
                anyhow::ensure!(
                    bytes.len() <= MAX_SNAPSHOT_BYTES,
                    "oversized live observation"
                );
                Ok::<_, anyhow::Error>(serde_json::from_slice::<serde_json::Value>(&bytes)?)
            })
            .transpose()?;
        values.push(serde_json::json!({"scope":scope,"snapshot":value}));
    }
    Ok(serde_json::json!({"schema_version":VERSION,"scopes":values}))
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Scratch(PathBuf);
    impl Scratch {
        fn new() -> Self {
            let p = Path::new("/tmp").canonicalize().unwrap().join(format!(
                "nddev-live-{}-{:x}",
                std::process::id(),
                rand::random::<u64>()
            ));
            use std::os::unix::fs::DirBuilderExt;
            std::fs::DirBuilder::new().mode(0o700).create(&p).unwrap();
            Self(p)
        }
    }
    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[tokio::test]
    async fn native_same_uid_reader_observes_memory_and_closed_scope_disappears() {
        let scratch = Scratch::new();
        let (owner, listener) = socket::SocketOwner::bind(scratch.0.clone()).await.unwrap();
        let handle = Handle::default();
        let registration = handle.register(Scope::Tab { id: 2, revision: 7 }).unwrap();
        registration
            .publish(br#"{"elapsed_ms":20,"status":"Connected"}"#)
            .unwrap();
        let stop = CancellationToken::new();
        let task = tokio::spawn(serve(owner, listener, handle.clone(), stop.clone()));
        let before = std::fs::read_dir(&scratch.0)
            .unwrap()
            .map(|p| p.unwrap().file_name())
            .collect::<Vec<_>>();
        let value = read(&scratch.0).await.unwrap();
        assert_eq!(value["scopes"][0]["snapshot"]["status"], "Connected");
        let mut incompatible = socket::connect(&scratch.0).await.unwrap();
        write_frame(
            &mut incompatible,
            &Request {
                version: VERSION + 1,
                query: Query::Scopes,
            },
        )
        .await
        .unwrap();
        assert!(read_frame::<_, Response>(&mut incompatible).await.is_err());
        assert_eq!(
            before,
            std::fs::read_dir(&scratch.0)
                .unwrap()
                .map(|p| p.unwrap().file_name())
                .collect::<Vec<_>>()
        );
        assert!(handle.register(Scope::Tab { id: 2, revision: 7 }).is_err());
        assert!(
            registration
                .publish(&vec![b' '; MAX_SNAPSHOT_BYTES + 1])
                .is_err()
        );
        drop(registration);
        assert!(
            read(&scratch.0).await.unwrap()["scopes"]
                .as_array()
                .unwrap()
                .is_empty()
        );
        stop.cancel();
        task.await.unwrap().unwrap();
        assert!(!scratch.0.join(socket::SOCKET).exists());
    }
}
