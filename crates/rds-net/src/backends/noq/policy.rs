//! Path policy: candidate ordering, path opening, and path selection.
//!
//! The first cut is deliberately conservative: deduplicate, cap the
//! candidate set at the multipath path limit, open the first candidate as
//! the primary path, then offer the rest via `open_path_ensure`. Relay
//! candidates are ignored until the relay transport lands (WS2).
//!
//! `connection_driver` is the per-connection control loop: it consumes
//! QNT address advertisements (peer-learned candidates become paths) and
//! path events, and keeps exactly one lowest-RTT path `Available` while
//! the rest become `Backup`. noq's packet scheduler follows path
//! statuses; it does not migrate on its own — the selection policy is
//! ours, mirroring iroh's biased-RTT selector with stickiness.

use std::collections::BTreeSet;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::time::Duration;

use iroh::{EndpointAddr, TransportAddr};

/// Maximum address candidates considered per connect.
///
/// Matches `max_concurrent_multipath_paths` on the transport config —
/// opening more paths than the peer accepts is wasted work.
pub const MAX_CANDIDATES: usize = 8;

/// Minimum RTT improvement before switching the selected path.
///
/// Mirrors iroh's `RTT_SWITCHING_MIN`: without a hysteresis margin,
/// equal-cost paths flap the selection back and forth.
const RTT_SWITCHING_MIN: Duration = Duration::from_millis(5);

/// Periodic re-selection interval. Path events cover topology changes;
/// the tick covers RTT drift on existing paths (e.g. a path degrading
/// without abandoning).
const RESELECT_INTERVAL: Duration = Duration::from_secs(1);

/// Extract direct IP candidates from an advertised address.
///
/// Order is deterministic (sorted by addr) so benchmarks reproduce.
/// Relay addresses are skipped here; they enter through the socket mux
/// once `relay_link` lands.
pub fn ip_candidates(addr: &EndpointAddr) -> Vec<SocketAddr> {
    let set: BTreeSet<SocketAddr> = addr
        .addrs
        .iter()
        .filter_map(|a| match a {
            TransportAddr::Ip(sock) => Some(*sock),
            _ => None,
        })
        .collect();
    set.into_iter().take(MAX_CANDIDATES).collect()
}

/// Open additional paths for the remaining candidates.
///
/// `open_path_ensure` deduplicates against the path `connect` already
/// opened — passing the primary again returns its `PathId` — so the
/// returned list doubles as the seed set for `connection_driver`.
/// Failures are logged, not fatal: any established path keeps the
/// connection alive.
pub fn open_extra_paths(conn: &noq::Connection, candidates: &[SocketAddr]) -> Vec<noq::PathId> {
    let mut ids = Vec::new();
    for addr in candidates {
        let open = conn.open_path_ensure(*addr, noq::PathStatus::Available);
        match open.path_id() {
            Some(id) => {
                ids.push(id);
                tracing::debug!(%addr, ?id, "opening extra path");
            }
            None => tracing::debug!(%addr, "extra path rejected"),
        }
    }
    ids
}

/// Advertise `addrs` to the peer via QNT `ADD_ADDRESS` frames.
///
/// This is how the peer learns addresses beyond the one it dialed —
/// the prerequisite for opening direct paths it did not see in the
/// original endpoint record.
pub fn advertise_addrs(conn: &noq::Connection, addrs: &[SocketAddr]) {
    for addr in addrs {
        if let Err(e) = conn.add_nat_traversal_address(*addr) {
            tracing::debug!(%addr, "add_nat_traversal_address: {e}");
        }
    }
}

/// Kick a NAT traversal round: the peer learns our candidates, we learn
/// theirs. Only the client side may initiate — on a server connection
/// this is a no-op. A round that actually learned candidates counts as
/// a QNT attempt in the endpoint's metrics.
pub fn initiate_traversal_round(conn: &noq::Connection, metrics: &crate::metrics::Registry) {
    if !conn.side().is_client() {
        return;
    }
    match conn.initiate_nat_traversal_round() {
        Ok(addrs) if !addrs.is_empty() => {
            metrics.qnt_attempt();
            tracing::debug!(n = addrs.len(), "nat traversal round started")
        }
        Ok(_) => {}
        Err(e) => tracing::debug!("nat traversal round not started: {e}"),
    }
}

/// Per-connection control loop: QNT events plus path selection.
///
/// Holds only weak handles (`WeakConnectionHandle`, `WeakPathHandle`) so
/// the task never keeps a connection alive: in noq's lineage, dropping
/// the last `Connection` handle is what sends CONNECTION_CLOSE, and
/// `Path` objects hold a connection reference internally. Handles are
/// upgraded only inside event handling and selection, then dropped.
///
/// The weak `on_closed` notification ends the task even while closed handles
/// remain alive. It also observes implicit closure after the last I/O handle
/// drops. The interval cannot keep the task alive after either closure or both
/// event streams ending. `seed_paths` carries the `PathId`s open at wiring time
/// (their `Established` events fired before we subscribed).
pub async fn connection_driver(
    conn: noq::WeakConnectionHandle,
    mut qnt: noq::NatTraversalUpdates,
    mut path_events: noq::PathEvents,
    seed_paths: Vec<noq::PathId>,
    metrics: crate::metrics::Registry,
) {
    use tokio_stream::StreamExt;

    // Register without retaining a strong Connection across any await.
    let Some(closed) = conn.upgrade().map(|owner| owner.on_closed()) else {
        return;
    };
    tokio::pin!(closed);

    let mut paths: HashMap<noq::PathId, noq::WeakPathHandle> = HashMap::new();
    // Paths opened to QNT-learned candidates — an Established event on
    // one of these is a QNT success.
    let mut qnt_paths: std::collections::HashSet<noq::PathId> = std::collections::HashSet::new();
    let mut selected: Option<noq::PathId> = None;
    if let Some(conn) = conn.upgrade() {
        for id in seed_paths {
            if let Some(path) = conn.path(id) {
                paths.insert(id, path.weak_handle());
            }
        }
    }

    let mut qnt_open = true;
    let mut events_open = true;
    let mut tick = tokio::time::interval(RESELECT_INTERVAL);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        tokio::select! {
            _ = &mut closed => break,
            event = qnt.next(), if qnt_open => match event {
                Some(Ok(noq_proto::n0_nat_traversal::Event::AddressAdded(addr))) => {
                    if let Some(id) = open_learned_path(&conn, &mut paths, addr) {
                        metrics.qnt_attempt();
                        qnt_paths.insert(id);
                    }
                }
                Some(Ok(noq_proto::n0_nat_traversal::Event::AddressRemoved(addr))) => {
                    tracing::debug!(%addr, "peer withdrew candidate");
                }
                Some(Err(lagged)) => {
                    tracing::warn!("QNT update stream lagged by {}", lagged.0)
                }
                None => qnt_open = false,
            },
            event = path_events.next(), if events_open => match event {
                Some(Ok(noq::PathEvent::Established { id, .. })) => {
                    if qnt_paths.remove(&id) {
                        metrics.qnt_success();
                    }
                    if let Some(c) = conn.upgrade()
                        && let Some(path) = c.path(id)
                    {
                        paths.insert(id, path.weak_handle());
                    }
                }
                Some(Ok(noq::PathEvent::Abandoned { id, .. }))
                | Some(Ok(noq::PathEvent::Discarded { id, .. })) => {
                    paths.remove(&id);
                    qnt_paths.remove(&id);
                    if selected == Some(id) {
                        selected = None;
                    }
                }
                Some(Ok(_)) => {}
                Some(Err(lagged)) => {
                    tracing::warn!("path event stream lagged by {}", lagged.0)
                }
                None => events_open = false,
            },
            _ = tick.tick() => {}
            else => break,
        }
        if !qnt_open && !events_open {
            break;
        }
        let Some(owner) = conn.upgrade() else {
            break;
        };
        // Lost/lagged path events cannot accumulate stale QNT history.
        qnt_paths.retain(|id| owner.path(*id).is_some());
        drop(owner);
        reselect(&conn, &mut paths, &mut selected);
    }
}

/// Attempt a path to a peer-advertised address and track it. Returns
/// the `PathId` when the open was accepted so the caller can count the
/// attempt and match its Established event as a QNT success.
fn open_learned_path(
    conn: &noq::WeakConnectionHandle,
    paths: &mut HashMap<noq::PathId, noq::WeakPathHandle>,
    addr: SocketAddr,
) -> Option<noq::PathId> {
    let conn = conn.upgrade()?;
    let open = conn.open_path_ensure(addr, noq::PathStatus::Available);
    let Some(id) = open.path_id() else {
        tracing::debug!(%addr, "QNT-learned candidate path rejected");
        return None;
    };
    // `open` is dropped without awaiting: dropping does not cancel the
    // attempt — the Established event arrives on the path stream.
    if let Some(path) = conn.path(id) {
        paths.insert(id, path.weak_handle());
    }
    tracing::debug!(%addr, ?id, "opening path to QNT-learned candidate");
    Some(id)
}

/// Apply the biased-RTT selection: the lowest-RTT path becomes
/// `Available`, all others `Backup`. The current selection is kept
/// unless a candidate beats it by at least [`RTT_SWITCHING_MIN`].
fn reselect(
    conn: &noq::WeakConnectionHandle,
    paths: &mut HashMap<noq::PathId, noq::WeakPathHandle>,
    selected: &mut Option<noq::PathId>,
) {
    if !conn.is_alive() {
        return;
    }
    // A WeakPathHandle can upgrade even after its path closes: it retains
    // final statistics until dropped. Upgrade alone is not a liveness check.
    paths.retain(|_, weak| weak.upgrade().is_some_and(|path| path.status().is_ok()));
    if paths.is_empty() {
        *selected = None;
        return;
    }

    let rtts: Vec<(noq::PathId, Duration)> = paths
        .iter()
        .filter_map(|(id, weak)| weak.upgrade().map(|path| (*id, path.stats().rtt)))
        .collect();
    let Some((mut choice, best_rtt)) = rtts.iter().min_by_key(|(_, rtt)| *rtt).copied() else {
        return;
    };

    if let Some(current) = *selected
        && current != choice
        && let Some((_, cur_rtt)) = rtts.iter().find(|(id, _)| *id == current)
        && best_rtt + RTT_SWITCHING_MIN > *cur_rtt
    {
        choice = current;
    }

    for (id, weak) in paths.iter() {
        let Some(path) = weak.upgrade() else {
            continue;
        };
        let want = if *id == choice {
            noq::PathStatus::Available
        } else {
            noq::PathStatus::Backup
        };
        if path.status().ok() != Some(want)
            && let Ok(_) = path.set_status(want)
        {
            tracing::debug!(?id, ?want, "path status applied");
        }
    }
    *selected = Some(choice);
}
