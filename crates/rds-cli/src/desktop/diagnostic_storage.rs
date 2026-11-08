//! Bounded retry ownership for private native incident files. This worker is
//! independent of input/media; failure is observable without dumping contents.
use std::{
    collections::VecDeque,
    io,
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Instant,
};

const MAX_PENDING: usize = 4;
const MAX_INCIDENT_BYTES: usize = 256 * 1024;

#[derive(Clone, Default, serde::Serialize)]
pub(super) struct Health {
    pub enabled: bool,
    pub live_query_available: bool,
    pub live_query_errors_total: u64,
    pub incident_admission_dropped_total: u64,
    pub snapshot_admission_dropped_total: u64,
    pub pending_operation_age_ms: Option<u64>,
    pub snapshot_writes_total: u64,
    pub snapshot_write_errors_total: u64,
    pub incident_writes_total: u64,
    pub incident_write_errors_total: u64,
    pub incident_queue_evicted_total: u64,
    pub incident_oversize_total: u64,
    pub incident_queue_pending: usize,
    pub last_snapshot_success_elapsed_ms: Option<u64>,
    pub last_incident_success_elapsed_ms: Option<u64>,
    pub last_error_kind: Option<&'static str>,
}

pub(super) struct Storage {
    directory: Option<PathBuf>,
    pending: VecDeque<Vec<u8>>,
    health: Health,
    snapshot_name: String,
    snapshot_inode: Option<(u64, u64)>,
    retained_incidents: usize,
}

impl Storage {
    pub(super) fn new(directory: Option<PathBuf>) -> Self {
        Self {
            health: Health {
                enabled: directory.is_some(),
                ..Health::default()
            },
            directory,
            pending: VecDeque::new(),
            snapshot_name: format!("state-{}.json", std::process::id()),
            snapshot_inode: None,
            retained_incidents: super::observation::Policy::default().retained_incidents,
        }
    }

    pub(super) fn scoped(directory: Option<PathBuf>, tab: u64, revision: u64) -> Self {
        let mut storage = Self::new(directory);
        storage.snapshot_name = format!("state-{}-tab-{tab}-{revision}.json", std::process::id());
        storage
    }

    pub(super) async fn remove_snapshot(&self) {
        let Some(inode) = self.snapshot_inode else {
            return;
        };
        let Some(directory) = &self.directory else {
            return;
        };
        let path = directory.join(&self.snapshot_name);
        let _ = tokio::task::spawn_blocking(move || -> io::Result<()> {
            use std::os::unix::fs::MetadataExt;
            let metadata = match std::fs::symlink_metadata(&path) {
                Ok(metadata) => metadata,
                Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
                Err(error) => return Err(error),
            };
            if metadata.is_file()
                && (metadata.dev(), metadata.ino()) == inode
                && metadata.nlink() == 1
                && metadata.mode() & 0o777 == 0o600
                && metadata.uid() == rustix::process::geteuid().as_raw()
            {
                std::fs::remove_file(path)?;
            }
            Ok(())
        })
        .await;
    }

    pub(super) fn health(&self) -> Health {
        self.health.clone()
    }

    pub(super) fn retain(&mut self, incidents: usize) {
        self.retained_incidents = incidents;
    }

    fn enqueue(&mut self, incident: Option<Vec<u8>>) {
        let Some(bytes) = incident.filter(|_| self.directory.is_some()) else {
            return;
        };
        if bytes.len() > MAX_INCIDENT_BYTES {
            self.health.incident_oversize_total += 1;
            return;
        }
        if self.pending.len() == MAX_PENDING {
            self.pending.pop_front();
            self.health.incident_queue_evicted_total += 1;
        }
        self.pending.push_back(bytes);
        self.health.incident_queue_pending = self.pending.len();
    }

    fn complete_incident(&mut self, result: io::Result<()>, elapsed_ms: u64) {
        match result {
            Ok(()) => {
                self.pending.pop_front();
                self.health.incident_writes_total += 1;
                self.health.last_incident_success_elapsed_ms = Some(elapsed_ms);
            }
            Err(error) => {
                self.health.incident_write_errors_total += 1;
                self.health.last_error_kind = Some(error_kind(&error));
                tracing::warn!(
                    error_kind = error_kind(&error),
                    pending = self.pending.len(),
                    "viewer incident retained for retry after storage failure"
                );
            }
        }
        self.health.incident_queue_pending = self.pending.len();
    }

    pub(super) async fn persist(
        &mut self,
        snapshot: Option<Vec<u8>>,
        incident: Option<Vec<u8>>,
        elapsed_ms: u64,
    ) {
        self.enqueue(incident);
        let Some(directory) = self.directory.clone() else {
            return;
        };
        if let Some(bytes) = snapshot {
            let path = directory.join(&self.snapshot_name);
            let result =
                tokio::task::spawn_blocking(move || crate::logging::viewer_snapshot(&path, &bytes))
                    .await;
            match result.unwrap_or_else(|_| Err(io::Error::other("snapshot worker ended"))) {
                Ok(inode) => {
                    self.snapshot_inode = Some(inode);
                    self.health.snapshot_writes_total += 1;
                    self.health.last_snapshot_success_elapsed_ms = Some(elapsed_ms);
                }
                Err(error) => {
                    self.health.snapshot_write_errors_total += 1;
                    self.health.last_error_kind = Some(error_kind(&error));
                    tracing::warn!(
                        error_kind = error_kind(&error),
                        "viewer snapshot write failed"
                    );
                }
            }
        }
        // One bounded attempt per diagnostic tick. A failed window remains
        // owned; the next tick retries rather than silently discarding it.
        if let Some(bytes) = self.pending.front().cloned() {
            let retained = self.retained_incidents;
            let result = tokio::task::spawn_blocking(move || {
                crate::logging::viewer_incident(&directory, &bytes, retained)
            })
            .await;
            self.complete_incident(
                result.unwrap_or_else(|_| Err(io::Error::other("incident worker ended"))),
                elapsed_ms,
            );
        }
    }
}

struct Work {
    snapshot: Option<Vec<u8>>,
    incident: Option<Vec<u8>>,
    elapsed_ms: u64,
}

#[derive(Default)]
struct Observed {
    storage: Health,
    query_available: bool,
    query_errors: u64,
    dropped_incidents: u64,
    dropped_snapshots: u64,
    queued_incidents: usize,
    active_incident: bool,
    writing: Option<Instant>,
    snapshot_expired: bool,
    remove_snapshot: bool,
}

impl Observed {
    fn health(&self) -> Health {
        let mut health = self.storage.clone();
        health.live_query_available = self.query_available;
        health.live_query_errors_total = self.query_errors;
        health.incident_admission_dropped_total = self.dropped_incidents;
        health.snapshot_admission_dropped_total = self.dropped_snapshots;
        health.pending_operation_age_ms = self
            .writing
            .map(|started| started.elapsed().as_millis().try_into().unwrap_or(u64::MAX));
        health.incident_queue_pending =
            (health.incident_queue_pending + usize::from(self.active_incident)).min(MAX_PENDING)
                + self.queued_incidents;
        health
    }
}

/// One queued work item, one active native write and Storage's four retained
/// incidents are the complete admission bound. Sampling never waits for disk.
pub(super) struct Writer {
    sender: tokio::sync::mpsc::Sender<Work>,
    observed: Arc<Mutex<Observed>>,
    task: tokio::task::JoinHandle<()>,
}

impl Writer {
    pub(super) fn start(mut storage: Storage) -> Self {
        let (sender, mut receiver) = tokio::sync::mpsc::channel::<Work>(1);
        let observed = Arc::new(Mutex::new(Observed {
            storage: storage.health(),
            ..Observed::default()
        }));
        let state = observed.clone();
        let task = tokio::spawn(async move {
            while let Some(mut work) = receiver.recv().await {
                {
                    let mut state = state.lock().unwrap_or_else(|e| e.into_inner());
                    state.queued_incidents = state
                        .queued_incidents
                        .saturating_sub(usize::from(work.incident.is_some()));
                    state.active_incident = work.incident.is_some();
                    state.writing = Some(Instant::now());
                    if state.snapshot_expired {
                        work.snapshot = None;
                    }
                }
                storage
                    .persist(work.snapshot, work.incident, work.elapsed_ms)
                    .await;
                let remove = {
                    let mut state = state.lock().unwrap_or_else(|e| e.into_inner());
                    std::mem::take(&mut state.remove_snapshot)
                };
                if remove {
                    storage.remove_snapshot().await;
                }
                let mut state = state.lock().unwrap_or_else(|e| e.into_inner());
                state.storage = storage.health();
                state.writing = None;
                state.active_incident = false;
            }
            storage.remove_snapshot().await;
        });
        Self {
            sender,
            observed,
            task,
        }
    }

    pub(super) fn health(&self) -> Health {
        self.observed
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .health()
    }

    pub(super) fn live_query_available(&self, available: bool) {
        self.observed
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .query_available = available;
    }

    pub(super) fn live_query_result(&self, result: anyhow::Result<()>) {
        if let Err(error) = result {
            let mut state = self.observed.lock().unwrap_or_else(|e| e.into_inner());
            if state.query_errors == 0 {
                tracing::warn!(%error,"live observation publication failed");
            }
            state.query_errors += 1;
        }
    }

    pub(super) fn expire_snapshot(&self, elapsed_ms: u64) {
        {
            let mut state = self.observed.lock().unwrap_or_else(|e| e.into_inner());
            state.snapshot_expired = true;
            state.remove_snapshot = true;
        }
        self.submit(None, None, elapsed_ms);
    }

    pub(super) fn submit(
        &self,
        snapshot: Option<Vec<u8>>,
        incident: Option<Vec<u8>>,
        elapsed_ms: u64,
    ) {
        let mut state = self.observed.lock().unwrap_or_else(|e| e.into_inner());
        if snapshot.is_none()
            && incident.is_none()
            && state.storage.incident_queue_pending == 0
            && !state.remove_snapshot
        {
            return;
        }
        let has_incident = incident.is_some();
        if let Err(error) = self.sender.try_send(Work {
            snapshot,
            incident,
            elapsed_ms,
        }) {
            if state.dropped_incidents == 0 && state.dropped_snapshots == 0 {
                let reason = match &error {
                    tokio::sync::mpsc::error::TrySendError::Full(_) => "capacity",
                    tokio::sync::mpsc::error::TrySendError::Closed(_) => "worker_ended",
                };
                tracing::warn!(reason, "diagnostic write admission failed");
            }
            let work = error.into_inner();
            state.dropped_incidents += u64::from(work.incident.is_some());
            state.dropped_snapshots += u64::from(work.snapshot.is_some());
        } else {
            state.queued_incidents += usize::from(has_incident);
        }
    }

    pub(super) async fn finish(self) -> Health {
        drop(self.sender);
        let _ = self.task.await;
        let state = self.observed.lock().unwrap_or_else(|e| e.into_inner());
        state.health()
    }
}

fn error_kind(error: &io::Error) -> &'static str {
    match error.kind() {
        io::ErrorKind::PermissionDenied => "permission_denied",
        io::ErrorKind::StorageFull => "storage_full",
        io::ErrorKind::NotFound => "not_found",
        io::ErrorKind::AlreadyExists => "already_exists",
        _ => "io_failure",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Scratch(PathBuf);
    impl Scratch {
        fn new() -> Self {
            use std::os::unix::fs::DirBuilderExt;
            let path = std::path::Path::new("/tmp")
                .canonicalize()
                .unwrap()
                .join(format!(
                    "nddev-observation-{}-{:x}",
                    std::process::id(),
                    rand::random::<u64>()
                ));
            std::fs::DirBuilder::new()
                .mode(0o700)
                .create(&path)
                .unwrap();
            Self(path)
        }
    }
    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    async fn until(mut condition: impl FnMut() -> bool) {
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while !condition() {
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn ordinary_observation_does_not_create_state_files() {
        let root = Scratch::new();
        let writer = Writer::start(Storage::new(Some(root.0.clone())));
        for elapsed in 0..20 {
            writer.submit(None, None, elapsed);
        }
        assert_eq!(writer.finish().await.snapshot_writes_total, 0);
        assert!(std::fs::read_dir(&root.0).unwrap().next().is_none());
    }

    #[tokio::test]
    async fn expired_capture_drops_queued_snapshots_and_cannot_recreate_its_file() {
        let root = Scratch::new();
        let storage = Storage::scoped(Some(root.0.clone()), 3, 2);
        let path = root.0.join(&storage.snapshot_name);
        let writer = Writer::start(storage);
        writer.submit(Some(b"{}".to_vec()), None, 0);
        until(|| path.exists()).await;
        writer.expire_snapshot(2_000);
        until(|| !path.exists()).await;
        writer.submit(Some(b"{\"obsolete\":true}".to_vec()), None, 3_000);
        let health = writer.finish().await;
        assert_eq!(health.snapshot_writes_total, 1);
        assert!(!path.exists());
    }

    #[tokio::test]
    async fn failed_disk_and_full_write_admission_do_not_hold_live_queries() {
        use super::super::live::{Run, Scope, directory, read};
        use clap::Parser;
        #[derive(Parser)]
        struct Args {
            #[command(flatten)]
            options: super::super::Options,
        }
        let root = Scratch::new();
        let blocked = root.0.join("not-a-directory");
        std::fs::write(&blocked, b"preserve").unwrap();
        let mut args = Args::parse_from(["test", "--diagnostics-dir", root.0.to_str().unwrap()]);
        let running = Run::start(&mut args.options).await.unwrap().unwrap();
        let handle = args.options.live_diagnostics.as_ref().unwrap();
        let registration = handle.register(Scope::Window).unwrap();
        until(|| handle.available()).await;
        let writer = Writer::start(Storage::new(Some(blocked.clone())));
        writer.submit(None, Some(b"{}".to_vec()), 0);
        writer.submit(None, Some(b"{}".to_vec()), 1);
        assert_eq!(writer.health().incident_admission_dropped_total, 1);
        until(|| writer.health().incident_write_errors_total > 0).await;
        let socket = directory(&root.0, std::process::id());
        for seq in 0..5 {
            registration
                .publish(format!("{{\"sample\":{seq}}}").as_bytes())
                .unwrap();
            writer.submit(None, None, 2_000 + seq);
            let current = read(&socket).await.unwrap();
            assert_eq!(current["scopes"][0]["snapshot"]["sample"], seq);
        }
        assert_eq!(std::fs::read(&blocked).unwrap(), b"preserve");
        let health = writer.finish().await;
        assert_eq!(health.incident_queue_pending, 1);
        assert_eq!(health.incident_admission_dropped_total, 1);
        drop(registration);
        running.close().await;
        assert!(!socket.exists());
    }

    #[test]
    fn disk_failure_retains_the_exact_incident_until_a_successful_retry() {
        let mut storage = Storage::new(Some(PathBuf::from("/unused")));
        storage.enqueue(Some(b"bounded synthetic evidence".to_vec()));
        storage.complete_incident(Err(io::Error::from_raw_os_error(28)), 2000);
        assert_eq!(
            storage.pending.front().unwrap(),
            b"bounded synthetic evidence"
        );
        assert_eq!(storage.health().incident_queue_pending, 1);
        assert_eq!(storage.health().incident_write_errors_total, 1);
        assert_eq!(storage.health().last_error_kind, Some("storage_full"));
        assert_eq!(storage.health().incident_writes_total, 0);
        storage.complete_incident(Ok(()), 4000);
        assert!(storage.pending.is_empty());
        assert_eq!(storage.health().incident_writes_total, 1);
        assert_eq!(
            storage.health().last_incident_success_elapsed_ms,
            Some(4000)
        );
    }

    #[test]
    fn outage_backlog_is_bounded_and_evictions_are_explicit() {
        let mut storage = Storage::new(Some(PathBuf::from("/unused")));
        for n in 0..6 {
            storage.enqueue(Some(vec![n; MAX_INCIDENT_BYTES]));
        }
        assert_eq!(storage.pending.len(), 4);
        assert_eq!(storage.pending.front().unwrap()[0], 2);
        assert_eq!(storage.health().incident_queue_evicted_total, 2);
        storage.enqueue(Some(vec![0; MAX_INCIDENT_BYTES + 1]));
        assert_eq!(storage.health().incident_oversize_total, 1);
        assert_eq!(storage.pending.len(), 4);
        let mut disabled = Storage::new(None);
        disabled.enqueue(Some(vec![0; 16]));
        assert!(disabled.pending.is_empty());
        assert!(!disabled.health().enabled);
    }
}
