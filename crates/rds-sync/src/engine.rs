//! Transfer engine: the three session roles over `rds-net` streams.
//!
//! - [`serve`] — the agent side: drives whichever direction the peer
//!   asks for on the control stream (`Offer` = push to us, `Request`
//!   = pull from us), rooted at the configured sync dir.
//! - [`send_file`] — CLI push: offer + stream missing chunks.
//! - [`recv_file`] — CLI pull: request + need + collect chunks.
//!
//! Chunk payloads ride uni-directional streams opened by the holder —
//! [`FETCH_STREAMS`] at a time, each carrying disjoint batches — so a
//! dead stream only stalls its own indices and a dropped connection
//! resumes from the receiver's journal.

use std::fs::File;
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::{Context, bail};
use rds_net::{Connection, RecvStream, SendStream};
use rds_net::{read_frame, write_frame as write_raw_frame};
use tokio::sync::mpsc;

use crate::proto::{
    CHUNKSET_BATCH, FETCH_STREAMS, MANIFEST_BATCH, MAX_CHUNKS, SESSION_VERSION, SessionLimits,
    SessionMsg, SyncMsg, bits_to_indices, check_manifest, check_rel_path, need_bits,
};
use crate::{MAX_CHUNK, Manifest, manifest_of_reader_cancellable};
use crate::{confined::Directory, journal::Journal};

/// No protocol read may stall longer than this — a peer that is alive
/// but silent still must not hang a transfer forever. Generous because
/// reads gate on the peer's disk work (manifest scans, journal
/// rescans); a dead connection ends them regardless. This is the
/// `Progress` deadline class (`rds_net::DeadlinePolicy`, W2.6).
const READ_STALL: Duration = Duration::from_secs(300);
/// Stall bound for frames that gate on the peer's own heavy local work:
/// manifest hashing before `Offer`, journal walk before `Need`,
/// assembly+verify before `Done`. Those legitimately outgrow the
/// per-frame stall on large transfers; the session deadline still
/// bounds a peer that never finishes.
const PHASE_STALL: Duration = Duration::from_secs(900);
/// Default absolute transfer budget, including local scans and all protocol I/O.
/// Call the `*_with_timeout` entry points to select a shorter or longer budget.
pub const TRANSFER_TIMEOUT: Duration = Duration::from_secs(3600);

/// Process-wide bound on blocking filesystem work (W2.5). Waiting for a
/// permit happens on the async side, so a scan/store storm queues inside
/// this crate instead of filling Tokio's blocking pool ahead of identity,
/// revocation and announcement work. Generous enough that real transfers
/// never serialize on it.
const MAX_DISK_JOBS: usize = 32;
static DISK_JOBS: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(MAX_DISK_JOBS);

/// Run `f` on the blocking pool once a disk-job permit frees. Cancelling
/// the future before a permit never reaches the blocking pool.
async fn disk_job<F, R>(f: F) -> Result<R, tokio::task::JoinError>
where
    F: FnOnce() -> R + Send + 'static,
    R: Send + 'static,
{
    let _permit = DISK_JOBS
        .acquire()
        .await
        .expect("disk-job semaphore never closes");
    tokio::task::spawn_blocking(f).await
}

/// Agent-side permissions, checked before any path or filesystem operation.
/// Read means download from the agent; write means upload to the agent.
#[derive(Debug, Clone, Default)]
pub struct Access {
    pub read: bool,
    pub write: bool,
    /// Signed grant path scope (grant v3 `sync_paths`): `Some` restricts
    /// transfers to the listed subtrees under the sync root, `None` leaves
    /// the whole root open. Entries are stored already-normalized.
    pub paths: Option<Arc<[PathBuf]>>,
}

impl Access {
    /// Compatibility policy for callers that already authorize both directions.
    pub const READ_WRITE: Self = Self {
        read: true,
        write: true,
        paths: None,
    };

    /// Whether a normalized `check_rel_path` result is inside the granted
    /// path scope — the path itself or a descendant of a listed subtree.
    pub fn permits_path(&self, rel: &Path) -> bool {
        match &self.paths {
            Some(paths) => paths
                .iter()
                .any(|scope| rel == scope.as_path() || rel.starts_with(scope)),
            None => true,
        }
    }
}

/// Progress/counters a completed (or interrupted) transfer reports.
#[derive(Debug, Default, Clone)]
pub struct Stats {
    /// Chunks fetched from the wire this session.
    pub fetched: u64,
    /// Total chunks the file needed.
    pub total: u64,
    /// Payload bytes received and verified.
    pub bytes: u64,
}

/// A transfer's uni-stream route. IDs must be freshly generated for each
/// greeting, including retries; legacy service-kind routing is compatibility
/// only and is never used by the local session manager.
#[derive(Clone, Copy)]
pub struct Transfer(rds_core::UniHello);

/// Wire profile of one transfer. v1 speaks bare `SyncMsg` frames (legacy
/// `Sync` and managed `SyncTransfer` routes). v2 — the `SyncTransferV2`
/// route — wraps every frame in a `Session` envelope bound to the minted
/// transfer ID and runs at the negotiated limits. A v1 peer cannot decode
/// a `Session` frame at all, so version mismatches refuse at greeting.
#[derive(Clone, Copy)]
struct Wire {
    transfer_id: Option<[u8; 16]>,
    limits: SessionLimits,
}

impl Wire {
    const V1: Self = Self {
        transfer_id: None,
        limits: SessionLimits::LOCAL,
    };

    fn v2(transfer_id: [u8; 16], limits: SessionLimits) -> Self {
        Self {
            transfer_id: Some(transfer_id),
            limits,
        }
    }

    fn is_v2(&self) -> bool {
        self.transfer_id.is_some()
    }

    async fn write<S, M>(&self, stream: &mut S, msg: &M) -> std::io::Result<()>
    where
        S: tokio::io::AsyncWrite + Unpin,
        M: serde::Serialize,
    {
        write_frame(stream, msg).await
    }

    /// Write a protocol message in this wire profile. v2 translates into
    /// the session envelope; a v1 peer receives `Cancel` as `Refuse` —
    /// both refuse the transfer, the typed distinction is v2-only.
    async fn send<S>(&self, stream: &mut S, msg: &SyncMsg) -> std::io::Result<()>
    where
        S: tokio::io::AsyncWrite + Unpin,
    {
        let frame = match self.transfer_id {
            Some(transfer_id) => SyncMsg::Session {
                transfer_id,
                // Session/Hello frames never nest; anything untranslatable
                // surfaces as a refusal rather than a nested envelope.
                msg: to_session(msg).unwrap_or(SessionMsg::Refuse {
                    reason: "internal protocol error".into(),
                }),
            },
            None => match msg {
                SyncMsg::Cancel { reason } => SyncMsg::Refuse {
                    reason: format!("transfer aborted: {reason}"),
                },
                // Session envelopes must never leak onto a v1 stream.
                SyncMsg::Session { .. } => SyncMsg::Refuse {
                    reason: "internal protocol error".into(),
                },
                msg => msg.clone(),
            },
        };
        self.write(stream, &frame).await
    }

    /// Read one protocol message. v2 asserts the envelope's transfer ID and
    /// surfaces `Cancel`/`Refuse` as ordinary messages for the caller.
    async fn recv<R>(&self, stream: &mut R) -> anyhow::Result<SyncMsg>
    where
        R: tokio::io::AsyncRead + Unpin,
    {
        self.recv_within(stream, READ_STALL).await
    }

    /// `recv` with a caller-chosen stall bound; see [`PHASE_STALL`].
    async fn recv_within<R>(&self, stream: &mut R, stall: Duration) -> anyhow::Result<SyncMsg>
    where
        R: tokio::io::AsyncRead + Unpin,
    {
        let msg: SyncMsg = read_timed_within(stream, stall).await?;
        let Some(transfer_id) = self.transfer_id else {
            return Ok(msg);
        };
        match msg {
            SyncMsg::Session {
                transfer_id: got,
                msg,
            } if got == transfer_id => match msg {
                SessionMsg::Hello { .. } | SessionMsg::HelloAck { .. } => {
                    bail!("session greeting repeated mid-transfer")
                }
                SessionMsg::Offer {
                    rel_path,
                    size,
                    root,
                    chunk_count,
                } => Ok(SyncMsg::Offer {
                    rel_path,
                    size,
                    root,
                    chunk_count,
                }),
                SessionMsg::Request { rel_path } => Ok(SyncMsg::Request { rel_path }),
                SessionMsg::Refuse { reason } => Ok(SyncMsg::Refuse { reason }),
                SessionMsg::Cancel { reason } => Ok(SyncMsg::Cancel { reason }),
                SessionMsg::ManifestPart { chunks } => Ok(SyncMsg::ManifestPart { chunks }),
                SessionMsg::Need { bits } => Ok(SyncMsg::Need { bits }),
                SessionMsg::Done { root } => Ok(SyncMsg::Done { root }),
                SessionMsg::ChunkSet { indices } => Ok(SyncMsg::ChunkSet { indices }),
                SessionMsg::ChunkHdr { index, hash, len } => {
                    Ok(SyncMsg::ChunkHdr { index, hash, len })
                }
                SessionMsg::SetDone => Ok(SyncMsg::SetDone),
            },
            SyncMsg::Session { .. } => bail!("sync frame belongs to a different transfer"),
            _ => bail!("sync session expected an envelope-bound frame"),
        }
    }
}

/// One-to-one translation of transfer bodies into v2 session messages.
/// Returns `None` for frames that cannot appear inside a session
/// (`Session` itself).
fn to_session(msg: &SyncMsg) -> Option<SessionMsg> {
    Some(match msg {
        SyncMsg::Offer {
            rel_path,
            size,
            root,
            chunk_count,
        } => SessionMsg::Offer {
            rel_path: rel_path.clone(),
            size: *size,
            root: *root,
            chunk_count: *chunk_count,
        },
        SyncMsg::Request { rel_path } => SessionMsg::Request {
            rel_path: rel_path.clone(),
        },
        SyncMsg::Refuse { reason } => SessionMsg::Refuse {
            reason: reason.clone(),
        },
        SyncMsg::Cancel { reason } => SessionMsg::Cancel {
            reason: reason.clone(),
        },
        SyncMsg::ManifestPart { chunks } => SessionMsg::ManifestPart {
            chunks: chunks.clone(),
        },
        SyncMsg::Need { bits } => SessionMsg::Need { bits: bits.clone() },
        SyncMsg::Done { root } => SessionMsg::Done { root: *root },
        SyncMsg::ChunkSet { indices } => SessionMsg::ChunkSet {
            indices: indices.clone(),
        },
        SyncMsg::ChunkHdr { index, hash, len } => SessionMsg::ChunkHdr {
            index: *index,
            hash: *hash,
            len: *len,
        },
        SyncMsg::SetDone => SessionMsg::SetDone,
        SyncMsg::Session { .. } => return None,
    })
}

/// Control-opener side of a v2 session: declare limits, await the
/// responder's declaration, run at the pairwise minimum. Version or
/// transfer-ID mismatches fail before any filesystem operation.
async fn session_open<W, R>(
    transfer_id: [u8; 16],
    send: &mut W,
    recv: &mut R,
) -> anyhow::Result<SessionLimits>
where
    W: tokio::io::AsyncWrite + Unpin,
    R: tokio::io::AsyncRead + Unpin,
{
    write_frame(
        send,
        &SyncMsg::Session {
            transfer_id,
            msg: SessionMsg::Hello {
                version: SESSION_VERSION,
                limits: SessionLimits::LOCAL,
            },
        },
    )
    .await?;
    match read_timed::<_, SyncMsg>(recv).await? {
        SyncMsg::Session {
            transfer_id: got,
            msg: SessionMsg::HelloAck { version, limits },
        } if got == transfer_id && version == SESSION_VERSION => {
            Ok(SessionLimits::negotiate(SessionLimits::LOCAL, limits)?)
        }
        SyncMsg::Session {
            transfer_id: got,
            msg: SessionMsg::HelloAck { version, .. },
        } if got == transfer_id => {
            bail!("peer sync session version {version} unsupported (want {SESSION_VERSION})")
        }
        SyncMsg::Session {
            msg: SessionMsg::HelloAck { .. },
            ..
        } => bail!("sync session transfer ID mismatch"),
        SyncMsg::Session {
            msg: SessionMsg::Refuse { reason },
            ..
        } => bail!("sync session refused: {reason}"),
        other => bail!("expected sync session HelloAck, got {other:?}"),
    }
}

/// Responder side of a v2 session: assert the Hello's transfer ID equals
/// the route tag, refuse mismatched versions with a clear error, then
/// declare local limits.
async fn session_accept<W, R>(
    transfer_id: [u8; 16],
    send: &mut W,
    recv: &mut R,
) -> anyhow::Result<SessionLimits>
where
    W: tokio::io::AsyncWrite + Unpin,
    R: tokio::io::AsyncRead + Unpin,
{
    let (version, limits) = match read_timed::<_, SyncMsg>(recv).await? {
        SyncMsg::Session {
            transfer_id: got,
            msg: SessionMsg::Hello { version, limits },
        } if got == transfer_id => (version, limits),
        SyncMsg::Session { .. } => bail!("sync session transfer ID mismatch"),
        other => bail!("expected sync session Hello, got {other:?}"),
    };
    if version != SESSION_VERSION {
        let _ = write_frame(
            send,
            &SyncMsg::Session {
                transfer_id,
                msg: SessionMsg::Refuse {
                    reason: format!(
                        "peer sync session version {version} unsupported (want {SESSION_VERSION})"
                    ),
                },
            },
        )
        .await;
        bail!("peer sync session version {version} unsupported (want {SESSION_VERSION})");
    }
    write_frame(
        send,
        &SyncMsg::Session {
            transfer_id,
            msg: SessionMsg::HelloAck {
                version: SESSION_VERSION,
                limits: SessionLimits::LOCAL,
            },
        },
    )
    .await?;
    Ok(SessionLimits::negotiate(SessionLimits::LOCAL, limits)?)
}

/// Best-effort typed abort: v2 peers learn the transfer was deliberately
/// cancelled rather than crashed; v1 peers get `Refuse`. Cancellation by
/// dropping the future cannot write — the peer still sees stream reset.
async fn cancel_transfer(wire: Wire, send: &mut SendStream, reason: &str) {
    let _ = wire
        .send(
            send,
            &SyncMsg::Cancel {
                reason: reason.to_string(),
            },
        )
        .await;
}

impl Transfer {
    const LEGACY: Self = Self(rds_core::UniHello::Sync);

    pub fn new(id: [u8; 16]) -> Self {
        Self(rds_core::UniHello::SyncTransfer { id })
    }

    /// Negotiated v2 session route. Fresh ID per greeting; both chunk
    /// streams and control frames carry it.
    pub fn new_v2(id: [u8; 16]) -> Self {
        Self(rds_core::UniHello::SyncTransferV2 { id })
    }

    /// The negotiated-session route variant's transfer ID, when any.
    fn session_id(&self) -> Option<[u8; 16]> {
        match self.0 {
            rds_core::UniHello::SyncTransferV2 { id } => Some(id),
            _ => None,
        }
    }

    pub async fn serve(
        self,
        conn: Connection,
        streams: (SendStream, RecvStream),
        dir: PathBuf,
        access: Access,
        timeout: Duration,
    ) -> anyhow::Result<()> {
        let mut control = Control::new(streams);
        let stopped = control.send.stopped();
        session(timeout, async {
            // v2 negotiates before the filesystem is touched; a version or
            // transfer-ID mismatch refuses at the greeting.
            let wire = match self.session_id() {
                Some(id) => Wire::v2(
                    id,
                    session_accept(id, &mut control.send, &mut control.recv).await?,
                ),
                None => Wire::V1,
            };
            let result = tokio::select! {
                biased;
                _ = stopped, if self.0 != rds_core::UniHello::Sync => bail!("sync caller stopped receiving"),
                result = serve_inner(conn, &mut control.send, &mut control.recv, dir, access, self.0, wire) => result,
            };
            if let Err(error) = result {
                // Preserve an explicitly written Refuse frame. A finish
                // failure here must not mask the transfer error that
                // brought us here — report it and keep the real cause.
                if let Err(finish) = control.finish(false).await {
                    tracing::warn!("sync control finish after error failed: {finish:#}");
                }
                return Err(error);
            }
            if self.0 != rds_core::UniHello::Sync {
                // Client finishes after Done; server FIN is the barrier that
                // all client control bytes were consumed before slot release.
                tokio::time::timeout(READ_STALL, control.recv.read_to_end(0))
                    .await.context("sync caller completion stalled")??;
            }
            control.finish(false).await
        }).await
    }

    pub async fn send_file(
        self,
        conn: &Connection,
        path: &Path,
        streams: (SendStream, RecvStream),
        timeout: Duration,
    ) -> anyhow::Result<Stats> {
        self.send_file_cancel(conn, path, streams, timeout, None)
            .await
    }

    /// Push with a caller cancellation token. A token abort writes a typed
    /// `Cancel` on the control stream so the peer stops its side
    /// deterministically instead of inferring crash from a reset.
    pub async fn send_file_cancel(
        self,
        conn: &Connection,
        path: &Path,
        streams: (SendStream, RecvStream),
        timeout: Duration,
        cancel: Option<tokio_util::sync::CancellationToken>,
    ) -> anyhow::Result<Stats> {
        let mut control = Control::new(streams);
        let stopped = control.send.stopped();
        let (cancel_flag, flag_watcher) = cancel_flag_watcher(&cancel);
        let result = session(timeout, async {
            let wire = match self.session_id() {
                Some(id) => Wire::v2(
                    id,
                    session_open(id, &mut control.send, &mut control.recv).await?,
                ),
                None => Wire::V1,
            };
            let mut cancelled = std::pin::pin!(async {
                match &cancel {
                    Some(token) => token.cancelled().await,
                    None => std::future::pending().await,
                }
            });
            let stats = tokio::select! {
                biased;
                _ = stopped, if self.0 != rds_core::UniHello::Sync => bail!("sync receiver stopped reading"),
                _ = &mut cancelled => {
                    cancel_transfer(wire, &mut control.send, "caller canceled").await;
                    bail!("sync transfer canceled by caller")
                }
                result = send_file_inner(conn, path, &mut control.send, &mut control.recv, self.0, wire, cancel_flag) => result?,
            };
            control.finish(self.0 != rds_core::UniHello::Sync).await?;
            Ok(stats)
        }).await;
        if let Some(watcher) = flag_watcher {
            watcher.abort();
        }
        result
    }

    pub async fn recv_file(
        self,
        conn: &Connection,
        rel_path: &str,
        dest_dir: &Path,
        streams: (SendStream, RecvStream),
        timeout: Duration,
    ) -> anyhow::Result<(PathBuf, Stats)> {
        self.recv_file_cancel(conn, rel_path, dest_dir, streams, timeout, None)
            .await
    }

    /// Pull with a caller cancellation token; see [`send_file_cancel`].
    pub async fn recv_file_cancel(
        self,
        conn: &Connection,
        rel_path: &str,
        dest_dir: &Path,
        streams: (SendStream, RecvStream),
        timeout: Duration,
        cancel: Option<tokio_util::sync::CancellationToken>,
    ) -> anyhow::Result<(PathBuf, Stats)> {
        let mut control = Control::new(streams);
        let stopped = control.send.stopped();
        let (cancel_flag, flag_watcher) = cancel_flag_watcher(&cancel);
        let result = session(timeout, async {
            let wire = match self.session_id() {
                Some(id) => Wire::v2(
                    id,
                    session_open(id, &mut control.send, &mut control.recv).await?,
                ),
                None => Wire::V1,
            };
            let mut cancelled = std::pin::pin!(async {
                match &cancel {
                    Some(token) => token.cancelled().await,
                    None => std::future::pending().await,
                }
            });
            let result = tokio::select! {
                biased;
                _ = stopped, if self.0 != rds_core::UniHello::Sync => bail!("sync sender stopped reading"),
                _ = &mut cancelled => {
                    cancel_transfer(wire, &mut control.send, "caller canceled").await;
                    bail!("sync transfer canceled by caller")
                }
                result = recv_file_inner(conn, rel_path, dest_dir, &mut control.send, &mut control.recv, self.0, wire, cancel_flag) => result?,
            };
            control.finish(self.0 != rds_core::UniHello::Sync).await?;
            Ok(result)
        }).await;
        if let Some(watcher) = flag_watcher {
            watcher.abort();
        }
        result
    }
}

/// Project a caller cancellation token into the blocking-work barriers
/// (W1.9): a running syscall cannot be aborted, so manifest scans and
/// journal assembly consult this flag once per chunk and stop at the next
/// chunk boundary instead of finishing the whole file first. The spawned
/// watcher is aborted by the caller once the transfer future resolves.
fn cancel_flag_watcher(
    cancel: &Option<tokio_util::sync::CancellationToken>,
) -> (Option<Arc<AtomicBool>>, Option<tokio::task::JoinHandle<()>>) {
    match cancel {
        Some(token) => {
            let flag = Arc::new(AtomicBool::new(false));
            let f = flag.clone();
            let t = token.clone();
            let watcher = tokio::spawn(async move {
                t.cancelled().await;
                f.store(true, Ordering::Release);
            });
            (Some(flag), Some(watcher))
        }
        None => (None, None),
    }
}

/// Reset abandoned bodies instead of implicitly finishing buffered writes.
/// Cancellation is scoped to this transfer; other services keep their streams.
struct Control {
    send: SendStream,
    recv: RecvStream,
    complete: bool,
}

impl Control {
    fn new((send, recv): (SendStream, RecvStream)) -> Self {
        Self {
            send,
            recv,
            complete: false,
        }
    }

    async fn finish(&mut self, wait_peer: bool) -> anyhow::Result<()> {
        self.send.finish()?;
        if wait_peer {
            tokio::time::timeout(READ_STALL, self.recv.read_to_end(0))
                .await
                .context("sync completion stalled")??;
        }
        self.complete = true;
        Ok(())
    }
}

impl Drop for Control {
    fn drop(&mut self) {
        if !self.complete {
            let _ = self.send.reset(0u32.into());
            let _ = self.recv.stop(0u32.into());
        }
    }
}

/// Agent-side entry: one control stream, whichever direction the peer
/// picks. `dir` is the agent's sync root — every path is validated
/// under it.
pub async fn serve(
    conn: Connection,
    send: SendStream,
    recv: RecvStream,
    dir: PathBuf,
) -> anyhow::Result<()> {
    serve_with_timeout(conn, send, recv, dir, TRANSFER_TIMEOUT).await
}

/// Serve with an explicit absolute budget. Cancellation cannot interrupt a
/// filesystem syscall already running; a complete disk commit may be uncertain
/// on timeout and must be reconciled before retrying or claiming rollback.
pub async fn serve_with_timeout(
    conn: Connection,
    send: SendStream,
    recv: RecvStream,
    dir: PathBuf,
    timeout: Duration,
) -> anyhow::Result<()> {
    serve_with_access(conn, send, recv, dir, Access::READ_WRITE, timeout).await
}

/// Serve an authorized connection with explicit directional permissions and
/// an absolute transfer budget. A refusal does not touch the sync root.
pub async fn serve_with_access(
    conn: Connection,
    send: SendStream,
    recv: RecvStream,
    dir: PathBuf,
    access: Access,
    timeout: Duration,
) -> anyhow::Result<()> {
    Transfer::LEGACY
        .serve(conn, (send, recv), dir, access, timeout)
        .await
}

async fn serve_inner(
    conn: Connection,
    send: &mut SendStream,
    recv: &mut RecvStream,
    dir: PathBuf,
    access: Access,
    route: rds_core::UniHello,
    wire: Wire,
) -> anyhow::Result<()> {
    // An `Offer` first frame gates on the caller's manifest build —
    // hashing a large source legitimately outlives the per-frame stall.
    let first = wire.recv_within(recv, PHASE_STALL).await?;
    match first {
        SyncMsg::Offer {
            rel_path,
            size,
            root,
            chunk_count,
        } => {
            if !access.write {
                refuse(wire, send, "sync write not granted").await?;
                bail!("sync write not granted");
            }
            let rel = match check_rel_path(&rel_path) {
                Ok(r) => r,
                Err(e) => {
                    refuse(wire, send, &e.to_string()).await?;
                    bail!("offer refused: {e}");
                }
            };
            if !access.permits_path(&rel) {
                refuse(wire, send, "sync path outside granted scope").await?;
                bail!("offer refused: path outside granted scope");
            }
            // Preserve early refusal before requesting a manifest. This is
            // only a preflight: Journal::open independently pins and checks
            // every handle again before any state or destination I/O.
            let preflight = {
                let (dir, rel) = (dir.clone(), rel.clone());
                disk_job(move || {
                    match Directory::open_root(&dir, false).and_then(|root| root.read_path(&rel)) {
                        Ok(_) => Ok(()),
                        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
                        Err(e) => Err(e),
                    }
                })
                .await
                .context("destination preflight task")?
            };
            if let Err(e) = preflight {
                // Wire reasons are coarse by contract: host errno details
                // stay in the local log, not on the peer's error chain.
                refuse(wire, send, "sync destination not writable").await?;
                bail!("offer refused: {e}");
            }
            let manifest = match read_manifest(recv, size, root, chunk_count, wire).await {
                Ok(m) => m,
                Err(e) => {
                    refuse(wire, send, "invalid sync manifest").await?;
                    return Err(e);
                }
            };
            tracing::info!(
                peer = %conn.remote_id(),
                rel = %rel.display(),
                size,
                chunks = manifest.chunks.len(),
                "sync push accepted"
            );
            let (_dest, stats) = receive(
                &conn,
                send,
                recv,
                &dir,
                &rel.to_string_lossy(),
                &manifest,
                route,
                wire,
                None,
            )
            .await?;
            tracing::info!(?stats, "push receive complete");
            Ok(())
        }
        SyncMsg::Request { rel_path } => {
            if !access.read {
                refuse(wire, send, "sync read not granted").await?;
                bail!("sync read not granted");
            }
            let rel = match check_rel_path(&rel_path) {
                Ok(r) => r,
                Err(e) => {
                    refuse(wire, send, &e.to_string()).await?;
                    bail!("request refused: {e}");
                }
            };
            if !access.permits_path(&rel) {
                refuse(wire, send, "sync path outside granted scope").await?;
                bail!("request refused: path outside granted scope");
            }
            // Pin the source once. Both manifest and chunk reads use this
            // same inode, even if the path is replaced after the offer.
            let source = {
                let rel = rel.clone();
                disk_job(move || Directory::open_root(&dir, false)?.read_path(&rel))
                    .await
                    .context("open source task")?
            };
            let source = match source {
                Ok(file) => Arc::new(file),
                Err(_) => {
                    refuse(wire, send, "no such file").await?;
                    bail!("requested file absent or outside root: {}", rel.display());
                }
            };
            let manifest = manifest_from_file(source.clone(), None).await?;
            tracing::info!(
                peer = %conn.remote_id(),
                rel = %rel.display(),
                size = manifest.size,
                chunks = manifest.chunks.len(),
                "sync pull serving"
            );
            send_manifest(wire, send, &rel.to_string_lossy(), &manifest).await?;
            // `Need` gates on the receiver's journal open — a walk over
            // every stored part — not on wire speed.
            let SyncMsg::Need { bits } = wire.recv_within(recv, PHASE_STALL).await? else {
                bail!("expected Need");
            };
            let indices = bits_to_indices(&bits, manifest.chunks.len())?;
            let early = push_chunks(&conn, source, &manifest, &indices, route, wire, recv).await?;
            recv_done(early, wire, recv, manifest.root).await?;
            tracing::info!(sent = indices.len(), "sync pull complete");
            Ok(())
        }
        SyncMsg::Cancel { reason } => bail!("caller canceled before transfer started: {reason}"),
        other => bail!("unexpected first sync message {other:?}"),
    }
}

/// CLI push: offer `path` to the peer under its file name, then stream
/// whatever chunks the receiver still needs.
pub async fn send_file(
    conn: &Connection,
    path: &Path,
    send: SendStream,
    recv: RecvStream,
) -> anyhow::Result<Stats> {
    send_file_with_timeout(conn, path, send, recv, TRANSFER_TIMEOUT).await
}

/// Push with an absolute budget; see [`serve_with_timeout`] for disk cancellation.
pub async fn send_file_with_timeout(
    conn: &Connection,
    path: &Path,
    send: SendStream,
    recv: RecvStream,
    timeout: Duration,
) -> anyhow::Result<Stats> {
    Transfer::LEGACY
        .send_file(conn, path, (send, recv), timeout)
        .await
}

async fn send_file_inner(
    conn: &Connection,
    path: &Path,
    send: &mut SendStream,
    recv: &mut RecvStream,
    route: rds_core::UniHello,
    wire: Wire,
    cancel_flag: Option<Arc<AtomicBool>>,
) -> anyhow::Result<Stats> {
    let rel = path
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| anyhow::anyhow!("source requires a UTF-8 file name"))?
        .to_owned();
    if check_rel_path(&rel)? != Path::new(&rel) {
        bail!("source file name has an ambiguous sync spelling");
    }
    let path = path.to_path_buf();
    let source = Arc::new(
        disk_job(move || -> anyhow::Result<File> {
            use rustix::fs::{Mode, OFlags};
            let file = File::from(rustix::fs::open(
                &path,
                OFlags::RDONLY | OFlags::NONBLOCK | OFlags::CLOEXEC,
                Mode::empty(),
            )?);
            anyhow::ensure!(file.metadata()?.is_file(), "source must be a regular file");
            Ok(file)
        })
        .await
        .context("open source task")??,
    );
    let manifest = match manifest_from_file(source.clone(), cancel_flag.clone()).await {
        Ok(manifest) => manifest,
        Err(error) => {
            if cancel_flag
                .as_ref()
                .is_some_and(|f| f.load(Ordering::Acquire))
            {
                bail!("sync transfer canceled by caller");
            }
            return Err(error);
        }
    };
    // A negotiated chunk bound below the local cut makes this transfer
    // impossible; refuse before any chunk leaves the source.
    if wire.is_v2()
        && manifest
            .chunks
            .iter()
            .any(|c| c.len > wire.limits.max_chunk)
    {
        let _ = refuse(wire, send, "peer chunk bound below manifest chunk size").await;
        bail!("peer chunk bound below manifest chunk size");
    }
    tracing::info!(
        peer = %conn.remote_id(),
        rel = %rel,
        size = manifest.size,
        chunks = manifest.chunks.len(),
        "sync push start"
    );
    send_manifest(wire, send, &rel, &manifest).await?;
    // `Need` gates on the receiver's journal open — a walk over every
    // stored part — not on wire speed.
    let indices = match wire.recv_within(recv, PHASE_STALL).await? {
        SyncMsg::Need { bits } => bits_to_indices(&bits, manifest.chunks.len())?,
        SyncMsg::Refuse { reason } => bail!("offer refused: {reason}"),
        SyncMsg::Cancel { reason } => bail!("receiver canceled before chunks: {reason}"),
        other => bail!("expected Need, got {other:?}"),
    };
    let early = push_chunks(conn, source, &manifest, &indices, route, wire, recv).await?;
    recv_done(early, wire, recv, manifest.root).await?;
    let stats = Stats {
        fetched: indices.len() as u64,
        total: manifest.chunks.len() as u64,
        bytes: indices
            .iter()
            .map(|i| u64::from(manifest.chunks[*i as usize].len))
            .sum(),
    };
    tracing::info!(?stats, "sync push complete");
    Ok(stats)
}

/// CLI pull: request `rel_path` from the peer into `dest_dir`,
/// resuming from the journal.
pub async fn recv_file(
    conn: &Connection,
    rel_path: &str,
    dest_dir: &Path,
    send: SendStream,
    recv: RecvStream,
) -> anyhow::Result<(PathBuf, Stats)> {
    recv_file_with_timeout(conn, rel_path, dest_dir, send, recv, TRANSFER_TIMEOUT).await
}

/// Pull with an absolute budget; see [`serve_with_timeout`] for disk cancellation.
pub async fn recv_file_with_timeout(
    conn: &Connection,
    rel_path: &str,
    dest_dir: &Path,
    send: SendStream,
    recv: RecvStream,
    timeout: Duration,
) -> anyhow::Result<(PathBuf, Stats)> {
    Transfer::LEGACY
        .recv_file(conn, rel_path, dest_dir, (send, recv), timeout)
        .await
}

#[allow(clippy::too_many_arguments)]
async fn recv_file_inner(
    conn: &Connection,
    rel_path: &str,
    dest_dir: &Path,
    send: &mut SendStream,
    recv: &mut RecvStream,
    route: rds_core::UniHello,
    wire: Wire,
    cancel_flag: Option<Arc<AtomicBool>>,
) -> anyhow::Result<(PathBuf, Stats)> {
    let requested = check_rel_path(rel_path)?;
    wire.send(
        send,
        &SyncMsg::Request {
            rel_path: rel_path.to_string(),
        },
    )
    .await?;
    // The answer gates on the server's manifest build, not wire speed.
    let (rel, size, root, chunk_count) = match wire.recv_within(recv, PHASE_STALL).await? {
        SyncMsg::Offer {
            rel_path,
            size,
            root,
            chunk_count,
        } => (
            check_rel_path(&rel_path).map_err(|e| anyhow::anyhow!("{e}"))?,
            size,
            root,
            chunk_count,
        ),
        SyncMsg::Refuse { reason } => bail!("request refused: {reason}"),
        SyncMsg::Cancel { reason } => bail!("sender canceled before offer: {reason}"),
        other => bail!("expected Offer, got {other:?}"),
    };
    if rel != requested {
        bail!("offered path differs from requested path");
    }
    let manifest = read_manifest(recv, size, root, chunk_count, wire).await?;
    let (dest, stats) = receive(
        conn,
        send,
        recv,
        dest_dir,
        &rel.to_string_lossy(),
        &manifest,
        route,
        wire,
        cancel_flag,
    )
    .await?;
    tracing::info!(rel = %rel.display(), ?stats, "sync pull complete");
    Ok((dest, stats))
}

/// Frame read bounded by the read-stall bound — a peer that stops talking
/// mid-transfer aborts instead of parking the session forever.
async fn read_timed<S, T>(stream: &mut S) -> anyhow::Result<T>
where
    S: tokio::io::AsyncRead + Unpin,
    T: serde::de::DeserializeOwned,
{
    read_timed_within(stream, READ_STALL).await
}

/// `read_timed` with a caller-chosen stall bound.
async fn read_timed_within<S, T>(stream: &mut S, stall: Duration) -> anyhow::Result<T>
where
    S: tokio::io::AsyncRead + Unpin,
    T: serde::de::DeserializeOwned,
{
    match tokio::time::timeout(stall, read_frame(stream)).await {
        Ok(r) => r.map_err(Into::into),
        Err(_) => bail!("peer stalled mid-transfer"),
    }
}

async fn session<T>(
    timeout: Duration,
    work: impl Future<Output = anyhow::Result<T>>,
) -> anyhow::Result<T> {
    let deadline = tokio::time::Instant::now()
        .checked_add(timeout)
        .filter(|_| !timeout.is_zero())
        .ok_or_else(|| anyhow::anyhow!("invalid transfer timeout"))?;
    tokio::time::timeout_at(deadline, work)
        .await
        .context("sync transfer deadline exceeded; a started disk commit may still complete")?
}

async fn write_frame<S, M>(stream: &mut S, message: &M) -> std::io::Result<()>
where
    S: tokio::io::AsyncWrite + Unpin,
    M: serde::Serialize,
{
    tokio::time::timeout(READ_STALL, write_raw_frame(stream, message))
        .await
        .map_err(|_| {
            std::io::Error::new(std::io::ErrorKind::TimedOut, "sync frame write stalled")
        })?
}

/// Manifest of a pinned file on the blocking pool — chunking + hashing a
/// large file must not park an async worker. `cancel_flag` is the W1.9
/// caller-token projection into the blocking scan: it is consulted once
/// per produced chunk, so a cancel can no longer wait for an entire
/// manifest scan of a large source before the transfer aborts.
async fn manifest_from_file(
    file: Arc<File>,
    cancel_flag: Option<Arc<AtomicBool>>,
) -> anyhow::Result<Manifest> {
    disk_job(move || {
        manifest_of_reader_cancellable(&*file, &move || {
            cancel_flag
                .as_ref()
                .is_some_and(|f| f.load(Ordering::Acquire))
        })
    })
    .await
    .context("manifest task")?
    .context("build manifest")
}

/// Filesystem-bound chunk store: writes run on a dedicated blocking
/// task so disk latency never parks an async worker mid-transfer, and
/// the wire read pipeline never waits on a flush. Owns the journal;
/// closing `jobs` ends it and hands the journal back for assembly.
struct JournalSink {
    // Fields drop in declaration order: publish cancellation before closing
    // the sender wakes the blocking receiver with its remaining queued data.
    cancel: StoreCancellation,
    jobs: mpsc::Sender<(u32, Vec<u8>, tokio::sync::SemaphorePermit<'static>)>,
    /// First store failure, for error reporting across the task split.
    error: Arc<std::sync::Mutex<Option<String>>>,
    task: tokio::task::JoinHandle<Result<Journal, crate::SyncError>>,
}

/// A running filesystem syscall cannot be aborted. Stop between stores and
/// also cancel a blocking task that has not started. The guard remains owned
/// while finish awaits the task, so canceling finish has the same semantics.
struct StoreCancellation {
    canceled: Arc<AtomicBool>,
    task: tokio::task::AbortHandle,
}

impl Drop for StoreCancellation {
    fn drop(&mut self) {
        self.canceled.store(true, Ordering::Release);
        self.task.abort();
    }
}

impl JournalSink {
    async fn start(mut journal: Journal) -> (Self, tokio::sync::oneshot::Receiver<()>) {
        let (jobs, mut job_rx) =
            mpsc::channel::<(u32, Vec<u8>, tokio::sync::SemaphorePermit<'static>)>(
                FETCH_STREAMS * 4,
            );
        let (finished, stopped) = tokio::sync::oneshot::channel();
        let error = Arc::new(std::sync::Mutex::new(None));
        let error_w = error.clone();
        let canceled = Arc::new(AtomicBool::new(false));
        let canceled_w = canceled.clone();
        // The disk-job permit rides with each queued chunk (acquired in
        // `put`), so a sink parked waiting for network data holds no slot.
        // Holding one per sink lifetime let N concurrent receives starve
        // every one-shot disk job in the process.
        let task = tokio::task::spawn_blocking(move || {
            // Drop signals every exit, including panic, without depending on
            // a reader already waiting. Normal exit requires closing jobs.
            let _finished = finished;
            while let Some((index, data, _permit)) = job_rx.blocking_recv() {
                if canceled_w.load(Ordering::Acquire) {
                    break;
                }
                match journal.store(index, &data) {
                    Ok(_) => {}
                    Err(e) => {
                        *error_w.lock().unwrap_or_else(|p| p.into_inner()) = Some(e.to_string());
                        return Err(e);
                    }
                }
            }
            Ok(journal)
        });
        let cancel = StoreCancellation {
            canceled,
            task: task.abort_handle(),
        };
        (
            Self {
                jobs,
                error,
                task,
                cancel,
            },
            stopped,
        )
    }

    /// Queue one bounded chunk for verification and storage; backpressures when the
    /// disk side falls behind.
    async fn put(&self, index: u32, data: Vec<u8>) -> anyhow::Result<()> {
        // Permit per queued store, taken on the async side like every
        // other disk job — it releases when the worker finishes the item.
        let permit = DISK_JOBS
            .acquire()
            .await
            .expect("disk-job semaphore never closes");
        self.jobs.send((index, data, permit)).await.map_err(|_| {
            let why = self
                .error
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .take()
                .unwrap_or_default();
            anyhow::anyhow!("journal writer died {why}")
        })
    }

    /// Drain queued stores and take the journal back.
    async fn finish(self) -> anyhow::Result<Journal> {
        let Self {
            jobs,
            task,
            cancel,
            error: _,
        } = self;
        drop(jobs);
        let result = task.await;
        drop(cancel);
        match result {
            Ok(Ok(j)) => Ok(j),
            Ok(Err(e)) => Err(anyhow::anyhow!("chunk store failed: {e}")),
            Err(e) => Err(anyhow::anyhow!("journal task join: {e}")),
        }
    }
}

/// Receiver half, shared by push and pull: journal the offer, answer
/// `Need`, collect chunk streams until complete, assemble, `Done`.
/// Returns the destination's informational path; I/O stays on held handles.
/// v2 additionally watches the control stream for a typed `Cancel`, so a
/// peer's deliberate abort stops collection deterministically rather than
/// waiting for chunk-stream errors.
#[allow(clippy::too_many_arguments)]
async fn receive(
    conn: &Connection,
    send: &mut SendStream,
    recv: &mut RecvStream,
    dir: &Path,
    rel: &str,
    manifest: &Manifest,
    route: rds_core::UniHello,
    wire: Wire,
    cancel_flag: Option<Arc<AtomicBool>>,
) -> anyhow::Result<(PathBuf, Stats)> {
    // Journal open walks and re-verifies every stored part — disk-bound
    // work belongs on the blocking pool, not an async worker.
    let journal = {
        let (dir, rel, manifest) = (dir.to_path_buf(), rel.to_string(), manifest.clone());
        disk_job(move || Journal::open(&dir, &rel, &manifest))
            .await
            .context("journal open task")?
    };
    let journal = match journal {
        Ok(journal) => journal,
        Err(e) => {
            refuse(wire, send, "cannot open transfer journal").await?;
            return Err(e.into());
        }
    };
    // Claim before sending Need: the peer may send immediately. This also
    // refuses a second live receive on the same route. Managed operations
    // use a unique transfer ID; direct compatibility retains the Sync tag.
    let uni = conn.uni_streams(route).context("claim sync uni streams")?;
    let bits = need_bits(journal.total(), journal.have_set());
    wire.send(send, &SyncMsg::Need { bits }).await?;

    // Shared stop for the blocking assembly inside `receive_chunks`: set by
    // the peer's typed Cancel or a dead control stream here, and by the
    // caller's cancellation token through `cancel_flag`. A canceled receive
    // must never publish the destination after reporting the abort.
    let assemble_stop = Arc::new(AtomicBool::new(false));
    // v2 control read: only Cancel is valid mid-receive; any other frame
    // is a protocol violation.
    let cancelled = async {
        if wire.is_v2() {
            wire.recv(recv).await
        } else {
            std::future::pending().await
        }
    };
    let result = tokio::select! {
        result = receive_chunks(uni, journal, manifest, wire, cancel_flag, assemble_stop.clone()) => result?,
        _ = send.stopped() => {
            assemble_stop.store(true, Ordering::Release);
            bail!("sync control stream closed during receive")
        }
        msg = cancelled => match msg {
            Ok(SyncMsg::Cancel { reason }) => {
                assemble_stop.store(true, Ordering::Release);
                bail!("peer canceled transfer: {reason}")
            }
            Ok(other) => bail!("unexpected control frame during receive: {other:?}"),
            Err(e) => return Err(e).context("sync control read during receive"),
        },
    };
    wire.send(
        send,
        &SyncMsg::Done {
            root: manifest.root,
        },
    )
    .await?;
    Ok(result)
}

async fn receive_chunks(
    mut uni: rds_net::UniStreams,
    journal: Journal,
    manifest: &Manifest,
    wire: Wire,
    cancel_flag: Option<Arc<AtomicBool>>,
    assemble_stop: Arc<AtomicBool>,
) -> anyhow::Result<(PathBuf, Stats)> {
    let total = journal.total() as u64;

    // Chunk streams arrive on the already claimed transfer/service route;
    // neither other services nor different transfer IDs can consume them.
    let mut requested: std::collections::HashSet<u32> = journal.need().into_iter().collect();
    let (sink, stopped) = JournalSink::start(journal).await;
    // Remove only requested unique indices from the bounded set. Disk stores
    // remain asynchronous; after draining the sink, require complete verified
    // journal state before assembly or a success response.
    let collecting = async {
        let mut streams = 0;
        let mut fetched_bytes = 0u64;
        while !requested.is_empty() {
            streams += 1;
            if streams > wire.limits.fetch_streams as usize {
                bail!("too many chunk streams");
            }
            // `recv` only parks once every routed stream is consumed — a
            // stall here means the holder under-delivered.
            let mut stream = match tokio::time::timeout(READ_STALL, uni.recv()).await {
                Ok(Some(s)) => s,
                Ok(None) => {
                    bail!("chunk streams ended before transfer completed")
                }
                Err(_) => {
                    bail!("chunk streams stalled")
                }
            };
            let mut stream_chunks = 0;
            loop {
                match wire.recv(&mut stream).await? {
                    SyncMsg::ChunkSet { indices } => {
                        if indices.is_empty() || indices.len() > CHUNKSET_BATCH {
                            bail!("invalid ChunkSet batch length");
                        }
                        for index in indices {
                            if !requested.remove(&index) {
                                bail!("duplicate or unrequested chunk {index}");
                            }
                            let SyncMsg::ChunkHdr {
                                index: i,
                                len,
                                hash,
                            } = wire.recv(&mut stream).await?
                            else {
                                bail!("expected ChunkHdr");
                            };
                            if i != index {
                                bail!("chunk stream out of order: {i} != {index}");
                            }
                            // The wire len is untrusted: validate it against
                            // the manifest before it sizes the receive
                            // buffer (a forged u32 len would otherwise force
                            // a multi-GiB allocation).
                            match manifest.chunks.get(i as usize) {
                                Some(c) if c.len == len && c.hash == hash => {}
                                _ => bail!("chunk {i} header disagrees with manifest"),
                            }
                            let mut buf = vec![0u8; len as usize];
                            match tokio::time::timeout(READ_STALL, stream.read_exact(&mut buf))
                                .await
                            {
                                Ok(Ok(())) => {}
                                Ok(Err(e)) => bail!("chunk body read: {e}"),
                                Err(_) => bail!("chunk body stalled"),
                            }
                            sink.put(i, buf).await?;
                            stream_chunks += 1;
                            fetched_bytes += u64::from(len);
                        }
                    }
                    SyncMsg::SetDone if stream_chunks > 0 => break,
                    SyncMsg::SetDone => bail!("empty chunk stream"),
                    other => bail!("unexpected chunk-stream message {other:?}"),
                }
            }
        }
        Ok::<_, anyhow::Error>(fetched_bytes)
    };
    let collected = tokio::select! {
        result = collecting => Some(result),
        _ = stopped => None,
    };
    let fetched_bytes = match collected {
        Some(result) => result?,
        None => {
            // Retrieve the real storage/panic error, even when the peer stops
            // sending immediately after the corrupt chunk. Do not wait for its
            // next frame or the unrelated network stall deadline.
            sink.finish().await?;
            bail!("journal writer exited before collection completed");
        }
    };
    let journal = sink.finish().await?;
    if !journal.complete() {
        bail!("chunk writer did not verify every requested index");
    }
    let fetched = journal.fetched();
    // Assembly concatenates and rehashes every part — blocking pool. Either
    // cancel source stops it at the next chunk boundary instead of letting
    // a canceled transfer publish the destination in the background.
    let dest = {
        disk_job(move || {
            journal.assemble_cancellable(&move || {
                assemble_stop.load(Ordering::Acquire)
                    || cancel_flag
                        .as_ref()
                        .is_some_and(|f| f.load(Ordering::Acquire))
            })
        })
        .await
        .context("assemble task")?
        .map_err(|e| anyhow::anyhow!("{e}"))?
    };
    tracing::debug!(?dest, "sync file assembled");
    Ok((
        dest,
        Stats {
            fetched,
            total,
            bytes: fetched_bytes,
        },
    ))
}

/// Final `Done` read after a chunk push. `early` is the root the push
/// watcher consumed while chunk tasks were still draining — validating
/// it here keeps the root check in one place for both arrival orders.
/// The receiver's `Done` gates on its store drain + assemble, so the
/// read uses the phase bound.
async fn recv_done(
    early: Option<crate::ChunkHash>,
    wire: Wire,
    recv: &mut RecvStream,
    expected: crate::ChunkHash,
) -> anyhow::Result<()> {
    let msg = match early {
        Some(root) => SyncMsg::Done { root },
        None => wire.recv_within(recv, PHASE_STALL).await?,
    };
    match msg {
        SyncMsg::Done { root } if root == expected => Ok(()),
        SyncMsg::Done { .. } => bail!("receiver acknowledged a different manifest root"),
        SyncMsg::Refuse { reason } => bail!("receiver refused: {reason}"),
        SyncMsg::Cancel { reason } => bail!("receiver canceled transfer: {reason}"),
        other => bail!("expected Done, got {other:?}"),
    }
}

/// Holder half: open the negotiated number of uni streams (v1 uses
/// [`FETCH_STREAMS`]), each walking an interleaved share of `indices` in
/// `CHUNKSET_BATCH` batches. v2 watches the control stream so a typed
/// `Cancel` stops chunk production immediately instead of surfacing as
/// write errors on half-closed streams. Returns the receiver's `Done`
/// root when it was consumed while chunk tasks were still draining.
async fn push_chunks(
    conn: &Connection,
    file: Arc<File>,
    manifest: &Manifest,
    indices: &[u32],
    route: rds_core::UniHello,
    wire: Wire,
    recv: &mut RecvStream,
) -> anyhow::Result<Option<crate::ChunkHash>> {
    let width = wire.limits.fetch_streams as usize;
    let mut tasks = tokio::task::JoinSet::new();
    for k in 0..width {
        let conn = conn.clone();
        let file = file.clone();
        let manifest = manifest.clone();
        let mine: Vec<u32> = indices.iter().copied().skip(k).step_by(width).collect();
        tasks.spawn(async move {
            if mine.is_empty() {
                return Ok::<(), anyhow::Error>(());
            }
            let mut stream = tokio::time::timeout(READ_STALL, conn.open_uni())
                .await
                .context("opening sync chunk stream stalled")??;
            // First frame on every uni stream is its UniHello tag —
            // the receiver's demux routes on it.
            write_frame(&mut stream, &route).await?;
            // One scratch per stream — chunks are ≤256 KiB, so this is
            // a single allocation rather than one per chunk.
            let mut buf = Vec::with_capacity(MAX_CHUNK as usize);
            for batch in mine.chunks(CHUNKSET_BATCH) {
                wire.send(
                    &mut stream,
                    &SyncMsg::ChunkSet {
                        indices: batch.to_vec(),
                    },
                )
                .await?;
                for &index in batch {
                    let c = manifest.chunks[index as usize];
                    buf.clear();
                    buf.resize(c.len as usize, 0);
                    // Positioned reads do not share a seek cursor across
                    // streams and never reopen the peer-controlled path.
                    let source = file.clone();
                    buf = disk_job(move || {
                        source.read_exact_at(&mut buf, c.offset)?;
                        Ok::<_, std::io::Error>(buf)
                    })
                    .await
                    .context("chunk read task")??;
                    wire.send(
                        &mut stream,
                        &SyncMsg::ChunkHdr {
                            index,
                            hash: c.hash,
                            len: c.len,
                        },
                    )
                    .await?;
                    tokio::time::timeout(READ_STALL, stream.write_all(&buf))
                        .await
                        .context("sync chunk write stalled")??;
                }
            }
            wire.send(&mut stream, &SyncMsg::SetDone).await?;
            stream.finish()?;
            Ok(())
        });
    }
    let mut cancelled = std::pin::pin!(async {
        if wire.is_v2() {
            wire.recv(recv).await
        } else {
            std::future::pending().await
        }
    });
    // The receiver's `Done` can legitimately arrive while our last chunk
    // streams are still finishing: its reads complete on stream FIN, not
    // on this task draining. An early Done is consumed here and returned
    // for the caller to validate — it must not fall into the abort path
    // as an "unexpected control frame".
    let mut early_done = None;
    loop {
        tokio::select! {
            result = tasks.join_next() => match result {
                None => break,
                Some(result) => result??,
            },
            msg = &mut cancelled => {
                match msg {
                    Ok(SyncMsg::Done { root }) => {
                        early_done = Some(root);
                    }
                    Ok(SyncMsg::Cancel { reason }) => {
                        tasks.abort_all();
                        return Err(anyhow::anyhow!("receiver aborted chunk push: {reason}"));
                    }
                    Ok(other) => {
                        tasks.abort_all();
                        return Err(anyhow::anyhow!(
                            "receiver aborted chunk push: unexpected control frame {other:?}"
                        ));
                    }
                    Err(e) => {
                        tasks.abort_all();
                        return Err(anyhow::anyhow!("receiver aborted chunk push: control read failed: {e}"));
                    }
                }
                break;
            }
        }
    }
    // Whether the watcher ended on an early Done or tasks drained first,
    // every spawned stream still finishes its own writes before we leave.
    while let Some(result) = tasks.join_next().await {
        result??;
    }
    Ok(early_done)
}

/// Write `Offer` + `ManifestPart` frames for a built manifest.
async fn send_manifest(
    wire: Wire,
    send: &mut SendStream,
    rel: &str,
    manifest: &Manifest,
) -> anyhow::Result<()> {
    wire.send(
        send,
        &SyncMsg::Offer {
            rel_path: rel.to_string(),
            size: manifest.size,
            root: manifest.root,
            chunk_count: manifest.chunks.len() as u32,
        },
    )
    .await?;
    for batch in manifest.chunks.chunks(MANIFEST_BATCH) {
        wire.send(
            send,
            &SyncMsg::ManifestPart {
                chunks: batch.to_vec(),
            },
        )
        .await?;
    }
    Ok(())
}

/// Read `ManifestPart` frames until `chunk_count` entries land;
/// reassemble and validate. v2 additionally enforces the negotiated
/// manifest and per-chunk bounds — tighter than the wire ceilings.
async fn read_manifest(
    recv: &mut RecvStream,
    size: u64,
    root: crate::ChunkHash,
    chunk_count: u32,
    wire: Wire,
) -> anyhow::Result<Manifest> {
    if chunk_count as usize > MAX_CHUNKS {
        bail!("manifest too large: {chunk_count}");
    }
    if wire.is_v2() && chunk_count > wire.limits.max_chunks {
        bail!("manifest exceeds negotiated chunk bound: {chunk_count}");
    }
    let mut chunks = Vec::with_capacity(chunk_count as usize);
    while chunks.len() < chunk_count as usize {
        match wire.recv(recv).await? {
            SyncMsg::ManifestPart { chunks: part } => {
                if part.is_empty()
                    || part.len() > MANIFEST_BATCH
                    || part.len() > chunk_count as usize - chunks.len()
                {
                    bail!("invalid ManifestPart batch length");
                }
                chunks.extend(part);
            }
            SyncMsg::Refuse { reason } => bail!("refused: {reason}"),
            SyncMsg::Cancel { reason } => bail!("sender canceled mid-manifest: {reason}"),
            other => bail!("expected ManifestPart, got {other:?}"),
        }
    }
    if chunks.len() != chunk_count as usize {
        bail!("manifest overrun: {} > {chunk_count}", chunks.len());
    }
    let m = Manifest { size, root, chunks };
    check_manifest(&m).map_err(|e| anyhow::anyhow!("{e}"))?;
    if wire.is_v2() && m.chunks.iter().any(|c| c.len > wire.limits.max_chunk) {
        bail!("manifest chunk exceeds negotiated bound");
    }
    Ok(m)
}

async fn refuse(wire: Wire, send: &mut SendStream, reason: &str) -> anyhow::Result<()> {
    wire.send(
        send,
        &SyncMsg::Refuse {
            reason: reason.to_string(),
        },
    )
    .await
    .context("send refuse")
}

#[cfg(test)]
mod tests;
