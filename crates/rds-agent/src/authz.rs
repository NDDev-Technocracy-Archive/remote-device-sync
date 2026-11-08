//! Atomic grant admission and connection-owned authorization resources.

use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rds_core::HelloAck;
use rds_core::grant::{self, GrantId, VerifiedGrant};
use rds_discovery::clock::{Lease, Reading};
use rds_net::Connection;
use rds_net::write_frame;
use tokio::sync::watch;
use tokio::task::JoinHandle;

use crate::{AgentPolicy, lock};

enum State {
    Open,
    Pending,
    Authorizing(Option<GrantLease>),
    Granted(GrantLease),
    Renewing(GrantLease),
    Closed,
}

/// Reservation released on every error, cancellation and teardown path.
struct GrantSlot {
    id: GrantId,
    active: Arc<Mutex<HashSet<GrantId>>>,
}

impl GrantSlot {
    fn claim(policy: &AgentPolicy, id: GrantId) -> Result<Self, &'static str> {
        if !lock(&policy.active_grants).insert(id) {
            return Err("grant already in use");
        }
        Ok(Self {
            id,
            active: policy.active_grants.clone(),
        })
    }
}

impl Drop for GrantSlot {
    fn drop(&mut self) {
        lock(&self.active).remove(&self.id);
    }
}

#[derive(Clone)]
struct ActiveGrant {
    grant: Arc<VerifiedGrant>,
    validity: Lease,
}

impl ActiveGrant {
    fn new(grant: VerifiedGrant, now: Reading) -> Result<Self, &'static str> {
        if !grant.live_at(now.wall.as_secs()) {
            return Err("grant expired");
        }
        let validity = Lease::new(now.wall.as_secs(), grant.payload.expires_at, now)
            .map_err(|_| "grant lease invalid")?;
        Ok(Self {
            grant: Arc::new(grant),
            validity,
        })
    }
    fn live_at(&self, now: Reading) -> bool {
        self.grant.live_at(now.wall.as_secs()) && self.validity.valid_at(now)
    }
    fn live(&self) -> bool {
        Reading::now().is_ok_and(|now| self.live_at(now))
    }
}

struct GrantLease {
    current: Arc<ActiveGrant>,
    updates: watch::Sender<Arc<ActiveGrant>>,
    watcher: Option<JoinHandle<()>>,
    _slot: Arc<GrantSlot>,
}

impl Drop for GrantLease {
    fn drop(&mut self) {
        if let Some(watcher) = &self.watcher {
            watcher.abort();
        }
    }
}

/// One mutex owns the state, reservation and watchdog together. No await
/// occurs under it; a concurrent Authz can never overwrite a live lease.
pub(crate) struct ConnAuthz {
    audience: [u8; 32],
    state: Mutex<State>,
    changed: watch::Sender<()>,
    sync_busy: std::sync::atomic::AtomicBool,
    service_slots: Arc<tokio::sync::Semaphore>,
}

impl ConnAuthz {
    pub(crate) fn new(required: bool, audience: [u8; 32], streams: usize) -> Self {
        Self {
            audience,
            service_slots: Arc::new(tokio::sync::Semaphore::new(
                // One JoinSet lane is reserved whenever a control stream
                // could need it: grant mode must always admit renewal, and
                // any multi-stream connection must keep observability
                // (Ping/Info) reachable past a saturated data plane.
                streams.saturating_sub(usize::from(required || streams > 1)),
            )),
            state: Mutex::new(if required {
                State::Pending
            } else {
                State::Open
            }),
            changed: watch::channel(()).0,
            sync_busy: std::sync::atomic::AtomicBool::new(false),
        }
    }

    fn begin(&self) -> Result<(), &'static str> {
        let mut state = lock(&self.state);
        match *state {
            State::Pending => {
                *state = State::Authorizing(None);
                Ok(())
            }
            State::Open => Err("agent does not require grants"),
            State::Authorizing(_) | State::Granted(_) | State::Renewing(_) => {
                Err("already authorizing or authorized")
            }
            State::Closed => Err("connection closed"),
        }
    }

    fn install(
        &self,
        conn: &Connection,
        policy: &AgentPolicy,
        verified: VerifiedGrant,
    ) -> Result<(), &'static str> {
        let mut state = lock(&self.state);
        if !matches!(*state, State::Authorizing(None)) || conn.is_closed() {
            return Err("authorization interrupted");
        }
        // Subscribe before checking the current value. An update after
        // this read is either seen by the watchdog's initial check or
        // wakes changed(); there is no check/subscribe gap.
        let denied = policy.denylist.subscribe();
        if !denied.borrow().fresh() {
            return Err("revocation policy unavailable or stale");
        }
        if denied.borrow().contains(&verified.id) {
            return Err("grant revoked");
        }
        if verified.payload.audience != self.audience {
            return Err("grant destination mismatch");
        }
        let now = Reading::now().map_err(|_| "grant clock unavailable")?;
        let slot = GrantSlot::claim(policy, verified.id)?;
        let current = Arc::new(ActiveGrant::new(verified, now)?);
        let (updates, receiver) = watch::channel(current.clone());
        let watcher = tokio::spawn(watch_grant(conn.clone(), receiver, denied));
        *state = State::Authorizing(Some(GrantLease {
            current,
            updates,
            watcher: Some(watcher),
            _slot: Arc::new(slot),
        }));
        Ok(())
    }

    fn commit(&self, policy: &AgentPolicy, conn: &Connection) -> Result<(), &'static str> {
        let mut state = lock(&self.state);
        let result = match std::mem::replace(&mut *state, State::Closed) {
            State::Authorizing(Some(lease))
                if !conn.is_closed()
                    && lease.current.live()
                    && policy.revocations_permit(&lease.current.grant.id) =>
            {
                *state = State::Granted(lease);
                Ok(())
            }
            _ => Err("authorization interrupted"),
        };
        drop(state);
        self.changed.send_replace(());
        result
    }

    fn begin_renewal(&self) -> Result<(), &'static str> {
        let mut state = lock(&self.state);
        if !matches!(*state, State::Granted(_)) {
            return Err("renewal requires a granted idle authorization transaction");
        }
        if let State::Granted(lease) = std::mem::replace(&mut *state, State::Closed) {
            *state = State::Renewing(lease);
        }
        Ok(())
    }

    fn prepare_renewal(&self, next: VerifiedGrant) -> Result<Arc<ActiveGrant>, &'static str> {
        let state = lock(&self.state);
        let State::Renewing(lease) = &*state else {
            return Err("renewal interrupted");
        };
        let now = Reading::now().map_err(|_| "grant clock unavailable")?;
        if !lease.current.live_at(now) {
            return Err("previous grant expired");
        }
        let advance = lease
            .current
            .grant
            .permits_renewal(&next)
            .map_err(|_| "invalid grant renewal")?;
        if advance {
            Ok(Arc::new(ActiveGrant::new(next, now)?))
        } else {
            // Exact retry never resets the accepted wall/continuous deadline.
            Ok(lease.current.clone())
        }
    }

    fn commit_renewal(
        &self,
        next: Arc<ActiveGrant>,
        policy: &AgentPolicy,
        conn: &Connection,
    ) -> Result<(), &'static str> {
        let mut state = lock(&self.state);
        let result = match std::mem::replace(&mut *state, State::Closed) {
            State::Renewing(mut lease)
                if !conn.is_closed()
                    && lease.current.live()
                    && next.live()
                    && policy.revocations_permit(&next.grant.id) =>
            {
                // The watchdog holds its read guard through a synchronous close.
                // Updating its view and checking connection closure cannot race a
                // stale expiry decision into an accepted renewal.
                lease.updates.send_replace(next.clone());
                if conn.is_closed() {
                    Err("renewal interrupted")
                } else {
                    lease.current = next;
                    *state = State::Granted(lease);
                    Ok(())
                }
            }
            _ => Err("renewal interrupted"),
        };
        drop(state);
        self.changed.send_replace(());
        result
    }

    fn close(&self) {
        // Drop the lease outside the state lock: teardown releases the
        // connection's replay ownership and aborts its watchdog. Native work
        // may still hold that exact reservation until its I/O actually drains.
        let previous = std::mem::replace(&mut *lock(&self.state), State::Closed);
        drop(previous);
        self.changed.send_replace(());
    }

    /// Normal teardown joins the watchdog. The synchronous close/drop path
    /// still aborts it when the owning service future is itself canceled.
    pub(crate) async fn close_and_wait(&self) {
        let previous = std::mem::replace(&mut *lock(&self.state), State::Closed);
        self.changed.send_replace(());
        let watcher = match previous {
            State::Authorizing(Some(mut lease))
            | State::Granted(mut lease)
            | State::Renewing(mut lease) => lease.watcher.take(),
            _ => None,
        };
        // This lease's ownership dropped before await; admitted native guards
        // may still retain the exact replay reservation through their I/O.
        if let Some(watcher) = watcher {
            watcher.abort();
            if let Err(error) = watcher.await
                && !error.is_cancelled()
            {
                tracing::debug!(%error, "grant watchdog ended");
            }
        }
    }

    /// Pin the original grant and replay reservation through already-admitted
    /// native I/O. Closing still seals new admission and stops the watchdog;
    /// retained work cannot make that same grant concurrently reusable.
    #[cfg(feature = "desktop")]
    pub(crate) fn native_lifetime(&self) -> Option<rds_desktop::clipboard::NativeLifetime> {
        match &*lock(&self.state) {
            State::Granted(lease) | State::Renewing(lease) if lease.current.live() => {
                Some(Arc::new((lease._slot.clone(), lease.current.clone())))
            }
            _ => None,
        }
    }

    pub(crate) fn scope(
        &self,
        policy: &AgentPolicy,
    ) -> Result<Option<Arc<VerifiedGrant>>, ScopeError> {
        match &*lock(&self.state) {
            State::Open => Ok(None),
            State::Pending => Err(ScopeError::Pending),
            State::Authorizing(_) => Err(ScopeError::Authorizing),
            State::Closed => Err(ScopeError::Closed),
            State::Granted(lease) | State::Renewing(lease) => {
                if !lease.current.live() {
                    return Err(ScopeError::Expired);
                }
                let revocations = policy.denylist.borrow();
                if !revocations.fresh() {
                    return Err(ScopeError::PolicyStale);
                }
                if revocations.contains(&lease.current.grant.id) {
                    return Err(ScopeError::Revoked);
                }
                Ok(Some(lease.current.grant.clone()))
            }
        }
    }

    /// The peer may observe the successful ACK before the authorization task
    /// resumes to commit. Wait for that transaction without granting early or
    /// rejecting a correctly sequenced service on another QUIC stream.
    pub(crate) async fn service_scope(
        &self,
        policy: &AgentPolicy,
    ) -> Result<Option<Arc<VerifiedGrant>>, ScopeError> {
        // Subscribe before reading the state so commit/close cannot be missed.
        let mut changed = self.changed.subscribe();
        tokio::time::timeout(policy.timeouts.authz, async {
            loop {
                match self.scope(policy) {
                    Err(ScopeError::Authorizing) => {
                        if changed.changed().await.is_err() {
                            return Err(ScopeError::Closed);
                        }
                    }
                    result => return result,
                }
            }
        })
        .await
        .unwrap_or(Err(ScopeError::AuthorizationTimeout))
    }

    pub(crate) fn try_service_slot(&self) -> Option<tokio::sync::OwnedSemaphorePermit> {
        self.service_slots.clone().try_acquire_owned().ok()
    }

    pub(crate) fn try_sync_slot(&self) -> Option<SyncSlot<'_>> {
        if self
            .sync_busy
            .swap(true, std::sync::atomic::Ordering::AcqRel)
        {
            None
        } else {
            Some(SyncSlot(&self.sync_busy))
        }
    }
}

/// Release on every exit, including a failed hello ACK or task cancellation.
pub(crate) struct SyncSlot<'a>(&'a std::sync::atomic::AtomicBool);

impl Drop for SyncSlot<'_> {
    fn drop(&mut self) {
        self.0.store(false, std::sync::atomic::Ordering::Release);
    }
}

pub(crate) enum ScopeError {
    Pending,
    Authorizing,
    AuthorizationTimeout,
    Closed,
    Expired,
    Revoked,
    PolicyStale,
}

impl ScopeError {
    pub(crate) fn message(&self) -> &'static str {
        match self {
            Self::Pending => "grant required: send Authz first",
            Self::Authorizing => "authorization in progress",
            Self::AuthorizationTimeout => "authorization completion timed out",
            Self::Closed => "connection closed",
            Self::Expired => "grant expired",
            Self::Revoked => "grant revoked",
            Self::PolicyStale => "revocation policy unavailable or stale",
        }
    }

    pub(crate) fn terminal(&self) -> bool {
        !matches!(self, Self::Pending | Self::Authorizing)
    }

    /// Export-safe refusal reason for telemetry; the free-form `message()`
    /// stays local diagnostics.
    pub(crate) fn reason(&self) -> rds_observe::Reason {
        match self {
            Self::Pending | Self::Authorizing => rds_observe::Reason::Denied,
            Self::AuthorizationTimeout => rds_observe::Reason::Timeout,
            Self::Closed => rds_observe::Reason::PeerClosed,
            Self::Expired => rds_observe::Reason::Expired,
            Self::Revoked => rds_observe::Reason::Revoked,
            Self::PolicyStale => rds_observe::Reason::PolicyUnavailable,
        }
    }
}

/// Even aborting the connection service future runs authorization cleanup.
pub(crate) struct ConnectionLifetime {
    pub(crate) conn: Connection,
    pub(crate) authz: Arc<ConnAuthz>,
}

impl Drop for ConnectionLifetime {
    fn drop(&mut self) {
        self.authz.close();
        self.conn.close(0u32.into(), b"service ended");
    }
}

/// A canceled/failed ACK must not leave a usable grant or replay reservation.
struct Admission<'a> {
    conn: &'a Connection,
    authz: &'a ConnAuthz,
    committed: bool,
}

impl Drop for Admission<'_> {
    fn drop(&mut self) {
        if !self.committed {
            self.authz.close();
            self.conn.close(2u32.into(), b"authorization failed");
        }
    }
}

pub(crate) async fn authorize(
    conn: &Connection,
    mut send: rds_net::SendStream,
    grant: grant::Grant,
    policy: &AgentPolicy,
    authz: &ConnAuthz,
    renewal: bool,
) -> anyhow::Result<()> {
    let begin = if renewal {
        authz.begin_renewal()
    } else {
        authz.begin()
    };
    if let Err(message) = begin {
        rds_observe::request_refused(rds_observe::Reason::Denied);
        let _ = tokio::time::timeout(
            policy.timeouts.authz,
            write_frame(
                &mut send,
                &HelloAck::Error {
                    message: message.into(),
                },
            ),
        )
        .await;
        anyhow::bail!(message);
    }
    let mut admission = Admission {
        conn,
        authz,
        committed: false,
    };
    let verified = match grant.verify(
        &policy.issuers,
        conn.remote_id().as_bytes(),
        &authz.audience,
        policy.grant_max_ttl,
        grant::now_unix(),
    ) {
        Ok(verified) => verified,
        Err(error) => {
            rds_observe::request_refused(match error {
                grant::GrantError::Expired => rds_observe::Reason::Expired,
                _ => rds_observe::Reason::Denied,
            });
            return Err(error.into());
        }
    };
    // Deployment binding: when policy pins a tenant or a policy-revision
    // floor, the grant must carry matching v3 claims. Version-2 grants carry
    // no claims and therefore fail any pinned binding.
    if let Some(tenant) = &policy.tenant
        && verified.tenant() != Some(tenant.as_str())
    {
        rds_observe::request_refused(rds_observe::Reason::Denied);
        return Err(grant::GrantError::TenantMismatch.into());
    }
    if let Some(floor) = policy.min_policy_revision
        && verified.policy_revision().is_none_or(|r| r < floor)
    {
        rds_observe::request_refused(rds_observe::Reason::Denied);
        return Err(grant::GrantError::PolicyRevisionStale.into());
    }
    let id = verified.id;
    let next = if renewal {
        Some(
            authz
                .prepare_renewal(verified)
                .map_err(anyhow::Error::msg)?,
        )
    } else {
        authz
            .install(conn, policy, verified)
            .map_err(anyhow::Error::msg)?;
        None
    };
    // Watcher and reservation are already owned while the reply is in
    // flight. Service admission remains pending until this completes.
    tokio::time::timeout(policy.timeouts.authz, write_frame(&mut send, &HelloAck::Ok)).await??;
    if let Some(next) = next {
        authz
            .commit_renewal(next, policy, conn)
            .map_err(anyhow::Error::msg)?;
    } else {
        authz.commit(policy, conn).map_err(anyhow::Error::msg)?;
    }
    // The client's successful transaction includes response FIN. Publish the
    // authorization before FIN so an immediately subsequent renewal cannot see
    // its predecessor still pending. A failed FIN still closes via Admission.
    send.finish()?;
    admission.committed = true;
    tracing::info!(peer = %conn.remote_id(), grant = %blake3::Hash::from(id), renewal, "grant authorized");
    Ok(())
}

async fn watch_grant(
    conn: Connection,
    mut current: watch::Receiver<Arc<ActiveGrant>>,
    mut denied: watch::Receiver<Arc<crate::RevocationPolicy>>,
) {
    loop {
        let snapshot = denied.borrow_and_update().clone();
        let wait = {
            // Keep the watch read guard through any close, synchronizing that
            // decision with renewal publication. Never hold it across await.
            let active = current.borrow_and_update();
            if !snapshot.fresh() {
                conn.close(4u32.into(), b"revocation policy stale");
                return;
            }
            if snapshot.contains(&active.grant.id) {
                conn.close(4u32.into(), b"grant revoked");
                return;
            }
            let Ok(now) = Reading::now() else {
                conn.close(3u32.into(), b"grant clock unavailable");
                return;
            };
            if !active.live_at(now) {
                conn.close(3u32.into(), b"grant expired");
                return;
            }
            active
                .validity
                .remaining_at(now)
                .min(Duration::from_secs(1))
        };
        tokio::select! {
            _ = conn.wait_closed() => return,
            _ = tokio::time::sleep(wait) => {},
            changed = current.changed() => {
                if changed.is_err() {
                    conn.close(3u32.into(), b"grant owner unavailable");
                    return;
                }
            }
            changed = denied.changed() => {
                if changed.is_err() {
                    conn.close(4u32.into(), b"revocation policy unavailable");
                    return;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    #[tokio::test]
    async fn sync_slot_is_exclusive_and_released_on_error_and_cancellation() {
        let authz = Arc::new(ConnAuthz::new(false, [0; 32], 8));
        let slot = authz.try_sync_slot().unwrap();
        assert!(authz.try_sync_slot().is_none());
        assert!(
            authz.try_sync_slot().is_none(),
            "a refused acquisition must not release the owner"
        );
        drop(slot);
        let fail = || -> Result<(), ()> {
            let _slot = authz.try_sync_slot().unwrap();
            Err(())?;
            Ok(())
        };
        assert!(fail().is_err());
        let (ready, started) = tokio::sync::oneshot::channel();
        let shared = authz.clone();
        let task = tokio::spawn(async move {
            let _slot = shared.try_sync_slot().unwrap();
            ready.send(()).unwrap();
            std::future::pending::<()>().await;
        });
        started.await.unwrap();
        assert!(authz.try_sync_slot().is_none());
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        assert!(authz.try_sync_slot().is_some());
    }
    use super::*;

    async fn connection_pair() -> (rds_net::Endpoint, rds_net::Endpoint, Connection, Connection) {
        let config = rds_net::EndpointConfig {
            discovery: false,
            bind_addrs: vec!["127.0.0.1:0".parse().unwrap()],
            ..Default::default()
        };
        let server = rds_net::bind_endpoint(config.clone()).await.unwrap();
        let client = rds_net::bind_endpoint(config).await.unwrap();
        let (outgoing, incoming) =
            tokio::join!(client.connect(server.addr(), rds_core::ALPN), async {
                server.accept().await.unwrap().await.unwrap()
            });
        (server, client, incoming, outgoing.unwrap())
    }

    fn verified(
        policy: &AgentPolicy,
        subject: rds_net::EndpointId,
        audience: [u8; 32],
    ) -> VerifiedGrant {
        policy.use_local_revocations();
        let issuer = ed25519_dalek::SigningKey::from_bytes(&[12; 32]);
        let grant = grant::Grant::issue(
            &issuer,
            *subject.as_bytes(),
            audience,
            [1; 16],
            vec![rds_core::ServiceKind::Ping],
            Duration::from_secs(60),
            Default::default(),
        );
        grant
            .verify(
                &HashSet::from([issuer.verifying_key().to_bytes()]),
                subject.as_bytes(),
                &audience,
                policy.grant_max_ttl,
                grant::now_unix(),
            )
            .unwrap()
    }

    #[tokio::test]
    async fn service_during_authorization_reply_waits_for_commit() {
        let (server, client, conn, peer) = connection_pair().await;
        let policy = Arc::new(AgentPolicy::ssh_only(("127.0.0.1".into(), 9)));
        let authz = Arc::new(ConnAuthz::new(true, *server.id().as_bytes(), 64));
        authz.begin().unwrap();
        authz
            .install(
                &conn,
                &policy,
                verified(&policy, conn.remote_id(), authz.audience),
            )
            .unwrap();
        // Hold the real admission state in the reply/commit window. A service
        // can arrive on another QUIC stream before that task runs commit().
        let (mut send, mut recv) = peer.open_bi().await.unwrap();
        write_frame(&mut send, &rds_core::StreamHello::Ping { nonce: 41 })
            .await
            .unwrap();
        let (reply, request) = conn.accept_bi().await.unwrap();
        let task = tokio::spawn(crate::serve_stream(
            conn.clone(),
            reply,
            request,
            policy.clone(),
            authz.clone(),
            crate::DesktopBackend::default(),
        ));
        let ack = rds_net::read_frame::<_, HelloAck>(&mut recv);
        tokio::pin!(ack);
        let early = tokio::time::timeout(Duration::from_millis(50), &mut ack).await;
        // Never authorize a service before commit, and never race a premature
        // Pending rejection against the success reply already sent to the peer.
        assert!(
            early.is_err(),
            "service replied before admission committed: {early:?}"
        );
        authz.commit(&policy, &conn).unwrap();
        assert!(matches!(
            tokio::time::timeout(Duration::from_secs(2), ack)
                .await
                .unwrap()
                .unwrap(),
            HelloAck::Ok
        ));
        task.await.unwrap().unwrap();
        drop(ConnectionLifetime { conn, authz });
        client.close().await;
        server.close().await;
    }

    #[tokio::test]
    async fn waiting_service_wakes_on_aborted_or_revoked_admission() {
        for revoke in [false, true] {
            let (server, client, conn, _peer) = connection_pair().await;
            let policy = AgentPolicy::ssh_only(("127.0.0.1".into(), 9));
            let authz = ConnAuthz::new(true, *server.id().as_bytes(), 64);
            assert!(matches!(
                authz.service_scope(&policy).await,
                Err(ScopeError::Pending)
            ));
            authz.begin().unwrap();
            let grant = verified(&policy, conn.remote_id(), authz.audience);
            let id = grant.id;
            authz.install(&conn, &policy, grant).unwrap();
            let waiting = authz.service_scope(&policy);
            tokio::pin!(waiting);
            assert!(
                tokio::time::timeout(Duration::from_millis(20), &mut waiting)
                    .await
                    .is_err()
            );
            if revoke {
                policy.revoke(id);
                assert!(authz.commit(&policy, &conn).is_err());
            } else {
                authz.close();
            }
            assert!(matches!(
                tokio::time::timeout(Duration::from_secs(2), waiting)
                    .await
                    .unwrap(),
                Err(ScopeError::Closed)
            ));
            assert!(lock(&policy.active_grants).is_empty());
            conn.close(0u32.into(), b"done");
            client.close().await;
            server.close().await;
        }
    }

    #[tokio::test]
    async fn normal_teardown_joins_watchdog_and_releases_reservation() {
        let (server, client, conn, _peer) = connection_pair().await;
        let policy = AgentPolicy::ssh_only(("127.0.0.1".into(), 9));
        let authz = ConnAuthz::new(true, *server.id().as_bytes(), 64);
        authz.begin().unwrap();
        authz
            .install(
                &conn,
                &policy,
                verified(&policy, conn.remote_id(), authz.audience),
            )
            .unwrap();
        authz.commit(&policy, &conn).unwrap();
        let watcher = {
            let state = lock(&authz.state);
            let State::Granted(lease) = &*state else {
                panic!("missing committed grant");
            };
            lease.watcher.as_ref().unwrap().abort_handle()
        };
        assert!(!watcher.is_finished());
        authz.close_and_wait().await;
        assert!(watcher.is_finished());
        assert!(lock(&policy.active_grants).is_empty());
        assert!(matches!(
            authz.service_scope(&policy).await,
            Err(ScopeError::Closed)
        ));
        conn.close(0u32.into(), b"done");
        client.close().await;
        server.close().await;
    }

    #[tokio::test]
    async fn watchdog_checks_snapshot_before_waiting_for_changes() {
        let (server, client, conn, _peer) = connection_pair().await;
        let policy = AgentPolicy::ssh_only(("127.0.0.1".into(), 9));
        let grant = Arc::new(verified(&policy, conn.remote_id(), *server.id().as_bytes()));
        let now = Reading::now().unwrap();
        let validity = Lease::new(now.wall.as_secs(), grant.payload.expires_at, now).unwrap();
        policy.revoke(grant.id);
        // A fresh subscriber considers this existing snapshot seen;
        // waiting only on changed() would miss the revocation forever.
        let receiver = policy.denylist.subscribe();
        tokio::time::timeout(
            Duration::from_secs(2),
            watch_grant(
                conn.clone(),
                watch::channel(Arc::new(ActiveGrant { grant, validity })).1,
                receiver,
            ),
        )
        .await
        .unwrap();
        assert!(conn.is_closed());
        client.close().await;
        server.close().await;
    }

    #[tokio::test]
    async fn canceling_admission_releases_grant_and_never_reopens() {
        let (server, client, conn, _peer) = connection_pair().await;
        let policy = Arc::new(AgentPolicy::ssh_only(("127.0.0.1".into(), 9)));
        let authz = Arc::new(ConnAuthz::new(true, *server.id().as_bytes(), 64));
        let (ready, started) = tokio::sync::oneshot::channel();
        let task = tokio::spawn({
            let conn = conn.clone();
            let policy = policy.clone();
            let authz = authz.clone();
            async move {
                authz.begin().unwrap();
                let _admission = Admission {
                    conn: &conn,
                    authz: &authz,
                    committed: false,
                };
                authz
                    .install(
                        &conn,
                        &policy,
                        verified(&policy, conn.remote_id(), authz.audience),
                    )
                    .unwrap();
                ready.send(()).unwrap();
                std::future::pending::<()>().await;
            }
        });
        started.await.unwrap();
        assert_eq!(lock(&policy.active_grants).len(), 1);
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        assert!(conn.is_closed());
        assert!(lock(&policy.active_grants).is_empty());
        assert!(authz.begin().is_err());
        assert!(authz.commit(&policy, &conn).is_err());
        client.close().await;
        server.close().await;
    }

    #[tokio::test]
    async fn service_admission_rechecks_policy_and_connection_drop_releases_lease() {
        let (server, client, conn, _peer) = connection_pair().await;
        let policy = AgentPolicy::ssh_only(("127.0.0.1".into(), 9));
        let authz = Arc::new(ConnAuthz::new(true, *server.id().as_bytes(), 64));
        authz.begin().unwrap();
        let grant = verified(&policy, conn.remote_id(), authz.audience);
        let id = grant.id;
        authz.install(&conn, &policy, grant).unwrap();
        assert!(matches!(authz.scope(&policy), Err(ScopeError::Authorizing)));
        authz.commit(&policy, &conn).unwrap();
        assert!(authz.scope(&policy).is_ok());
        // No await: the watchdog has not run; service admission itself
        // must reject this revocation.
        policy.revoke(id);
        assert!(matches!(authz.scope(&policy), Err(ScopeError::Revoked)));
        policy.replace_denylist(HashSet::new());
        {
            let mut state = lock(&authz.state);
            let State::Granted(lease) = &mut *state else {
                panic!("missing grant")
            };
            Arc::make_mut(&mut Arc::make_mut(&mut lease.current).grant)
                .payload
                .expires_at = 0;
        }
        assert!(matches!(authz.scope(&policy), Err(ScopeError::Expired)));
        drop(ConnectionLifetime {
            conn: conn.clone(),
            authz: authz.clone(),
        });
        assert!(lock(&policy.active_grants).is_empty());
        assert!(authz.begin().is_err());
        client.close().await;
        server.close().await;
    }

    #[test]
    fn concurrent_authz_has_one_owner_and_cannot_reopen_after_close() {
        let authz = Arc::new(ConnAuthz::new(true, [8; 32], 64));
        let barrier = Arc::new(std::sync::Barrier::new(16));
        let successes = std::sync::atomic::AtomicUsize::new(0);
        std::thread::scope(|scope| {
            for _ in 0..16 {
                let barrier = barrier.clone();
                let authz = authz.clone();
                let successes = &successes;
                scope.spawn(move || {
                    barrier.wait();
                    if authz.begin().is_ok() {
                        successes.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    }
                });
            }
        });
        assert_eq!(successes.load(std::sync::atomic::Ordering::Relaxed), 1);
        authz.close();
        assert!(authz.begin().is_err());
    }

    #[cfg(feature = "desktop")]
    #[tokio::test]
    async fn admitted_native_work_pins_its_original_grant_after_connection_admission_closes() {
        let (server, client, conn, _peer) = connection_pair().await;
        let policy = AgentPolicy::ssh_only(("127.0.0.1".into(), 9));
        let authz = Arc::new(ConnAuthz::new(true, *server.id().as_bytes(), 64));
        authz.begin().unwrap();
        let grant = verified(&policy, conn.remote_id(), authz.audience);
        let id = grant.id;
        authz.install(&conn, &policy, grant).unwrap();
        authz.commit(&policy, &conn).unwrap();
        let native = authz.native_lifetime().unwrap();
        authz.close_and_wait().await;
        assert!(authz.native_lifetime().is_none());
        assert!(authz.begin().is_err());
        assert!(GrantSlot::claim(&policy, id).is_err());
        drop(native);
        assert!(lock(&policy.active_grants).is_empty());
        drop(GrantSlot::claim(&policy, id).unwrap());
        client.close().await;
        server.close().await;
    }

    #[test]
    fn reservation_drop_releases_only_its_own_slot() {
        let policy = AgentPolicy::ssh_only(("127.0.0.1".into(), 9));
        let first = GrantSlot::claim(&policy, [1; 32]).unwrap();
        let second = GrantSlot::claim(&policy, [2; 32]).unwrap();
        assert!(GrantSlot::claim(&policy, [1; 32]).is_err());
        drop(first);
        assert_eq!(*lock(&policy.active_grants), HashSet::from([[2; 32]]));
        drop(second);
        assert!(lock(&policy.active_grants).is_empty());
    }

    #[test]
    fn active_grant_expires_on_suspend_rollback_or_wall_deadline() {
        let issuer = ed25519_dalek::SigningKey::from_bytes(&[12; 32]);
        let grant = grant::Grant::issue_at(
            &issuer,
            grant::GrantPayload {
                version: grant::GRANT_VERSION,
                revision: 1,
                issuer: issuer.verifying_key().to_bytes(),
                subject: [7; 32],
                audience: [8; 32],
                nonce: [1; 16],
                services: vec![rds_core::ServiceKind::Ping],
                not_before: 100,
                expires_at: 160,
                constraints: Default::default(),
                tenant: None,
                policy_revision: None,
            },
        )
        .verify(
            &HashSet::from([issuer.verifying_key().to_bytes()]),
            &[7; 32],
            &[8; 32],
            Duration::from_secs(60),
            100,
        )
        .unwrap();
        let now = Reading {
            boot: [1; 16],
            wall: Duration::from_millis(100_500),
            continuous: Duration::from_secs(10),
        };
        let active = ActiveGrant::new(grant, now).unwrap();
        assert!(active.live_at(now));
        assert!(active.live_at(Reading {
            continuous: Duration::from_millis(69_499),
            ..now
        }));
        assert!(!active.live_at(Reading {
            continuous: Duration::from_millis(69_500),
            ..now
        }));
        assert!(!active.live_at(Reading {
            wall: Duration::from_millis(100_499),
            ..now
        }));
        assert!(!active.live_at(Reading {
            wall: Duration::from_secs(160),
            ..now
        }));
        assert!(!active.live_at(Reading {
            boot: [2; 16],
            ..now
        }));
    }

    #[tokio::test]
    async fn renewal_retry_retains_deadline_and_single_replay_owner() {
        let (server, client, conn, _peer) = connection_pair().await;
        let policy = AgentPolicy::ssh_only(("127.0.0.1".into(), 9));
        let authz = ConnAuthz::new(true, *server.id().as_bytes(), 64);
        authz.begin().unwrap();
        authz
            .install(
                &conn,
                &policy,
                verified(&policy, conn.remote_id(), authz.audience),
            )
            .unwrap();
        authz.commit(&policy, &conn).unwrap();
        let original = match &*lock(&authz.state) {
            State::Granted(lease) => lease.current.clone(),
            _ => unreachable!(),
        };
        authz.begin_renewal().unwrap();
        assert!(authz.begin_renewal().is_err());
        let retry = authz.prepare_renewal((*original.grant).clone()).unwrap();
        assert!(Arc::ptr_eq(&original, &retry));
        assert_eq!(lock(&policy.active_grants).len(), 1);
        authz.commit_renewal(retry, &policy, &conn).unwrap();
        assert!(authz.scope(&policy).is_ok());
        assert_eq!(lock(&policy.active_grants).len(), 1);
        authz.close_and_wait().await;
        assert!(lock(&policy.active_grants).is_empty());
        conn.close(0u32.into(), b"done");
        client.close().await;
        server.close().await;
    }

    #[tokio::test]
    async fn pending_renewal_never_disables_the_old_expiry_or_cancel_guard() {
        for cancel in [false, true] {
            let (server, client, conn, _peer) = connection_pair().await;
            let policy = AgentPolicy::ssh_only(("127.0.0.1".into(), 9));
            policy.use_local_revocations();
            let authz = ConnAuthz::new(true, *server.id().as_bytes(), 64);
            let issuer = ed25519_dalek::SigningKey::from_bytes(&[12; 32]);
            let first = grant::Grant::issue(
                &issuer,
                *conn.remote_id().as_bytes(),
                authz.audience,
                [1; 16],
                vec![rds_core::ServiceKind::Ping],
                Duration::from_secs(2),
                Default::default(),
            );
            let verify = |token: &grant::Grant| {
                token
                    .verify(
                        &HashSet::from([issuer.verifying_key().to_bytes()]),
                        conn.remote_id().as_bytes(),
                        &authz.audience,
                        Duration::from_secs(300),
                        grant::now_unix(),
                    )
                    .unwrap()
            };
            let initial = verify(&first);
            let mut next = initial.payload.clone();
            next.revision += 1;
            next.expires_at += 60;
            authz.begin().unwrap();
            authz.install(&conn, &policy, initial).unwrap();
            authz.commit(&policy, &conn).unwrap();
            authz.begin_renewal().unwrap();
            let guard = Admission {
                conn: &conn,
                authz: &authz,
                committed: false,
            };
            let next = authz
                .prepare_renewal(verify(&grant::Grant::issue_at(&issuer, next)))
                .unwrap();
            if !cancel {
                // Candidate verified, but its reply/commit has not completed.
                tokio::time::timeout(Duration::from_secs(3), conn.wait_closed())
                    .await
                    .unwrap();
                assert!(authz.commit_renewal(next, &policy, &conn).is_err());
            }
            drop(guard);
            assert!(conn.is_closed());
            assert!(lock(&policy.active_grants).is_empty());
            client.close().await;
            server.close().await;
        }
    }
}

#[cfg(test)]
mod reason_tests {
    use super::ScopeError;

    /// Every refusal surface exports a typed reason — the free-form
    /// `message()` stays local diagnostics and never reaches telemetry.
    #[test]
    fn scope_errors_map_to_export_safe_reasons() {
        let cases = [
            (ScopeError::Pending, rds_observe::Reason::Denied),
            (ScopeError::Authorizing, rds_observe::Reason::Denied),
            (
                ScopeError::AuthorizationTimeout,
                rds_observe::Reason::Timeout,
            ),
            (ScopeError::Closed, rds_observe::Reason::PeerClosed),
            (ScopeError::Expired, rds_observe::Reason::Expired),
            (ScopeError::Revoked, rds_observe::Reason::Revoked),
            (
                ScopeError::PolicyStale,
                rds_observe::Reason::PolicyUnavailable,
            ),
        ];
        for (error, want) in cases {
            assert_eq!(error.reason(), want, "{}", error.message());
        }
    }
}
