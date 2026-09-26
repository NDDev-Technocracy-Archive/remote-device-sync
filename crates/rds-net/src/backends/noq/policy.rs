//! Path policy: candidate ordering, path opening, and path selection.
//!
//! The first cut is deliberately conservative: deduplicate, cap the
//! direct candidate set at the multipath path limit, race initial handshakes
//! in `dial`, then offer additional paths via `open_path_ensure`. An attached
//! relay contributes one additional initial candidate through the socket mux.
//!
//! `connection_driver` is the per-connection control loop: it consumes
//! QNT address advertisements (peer-learned candidates become paths) and
//! path events, and prefers one lowest-RTT validated path among the observed
//! set. Other application-opened paths stay `Backup`. noq's scheduler follows path
//! statuses; it does not migrate on its own — the selection policy is
//! ours, mirroring iroh's biased-RTT selector with stickiness.

use std::collections::BTreeSet;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::time::Duration;

use super::candidates::{Origin, Pending, canonical};
use iroh::{EndpointAddr, TransportAddr};
use tokio::time::Instant;

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
/// Each family is sorted, then the families alternate under the shared cap.
/// Deterministic ordering keeps benchmarks reproducible without one family
/// consuming the entire budget. Mapped IPv4 aliases share their native slot.
/// Relay addresses are skipped here; the endpoint separately adds its attached
/// relay through the socket mux.
pub fn ip_candidates(addr: &EndpointAddr) -> Vec<SocketAddr> {
    matching_candidates(addr, |_| true)
}

pub(super) fn dial_candidates(addr: &EndpointAddr, local_addrs: &[SocketAddr]) -> Vec<SocketAddr> {
    matching_candidates(addr, |remote| supports_candidate(local_addrs, remote))
}

fn matching_candidates(
    addr: &EndpointAddr,
    supported: impl Fn(SocketAddr) -> bool,
) -> Vec<SocketAddr> {
    let mut families: [BTreeSet<SocketAddr>; 2] = Default::default();
    for remote in addr.addrs.iter().filter_map(|a| match a {
        TransportAddr::Ip(sock) => Some(canonical(*sock)),
        _ => None,
    }) {
        if super::relay::is_synthetic(remote) || !supported(remote) {
            continue;
        }
        let family = &mut families[usize::from(remote.is_ipv6())];
        family.insert(remote);
        // Keep only the smallest possible winners, so extracting from a large
        // caller-provided record does not duplicate its entire address set.
        if family.len() > MAX_CANDIDATES {
            family.pop_last();
        }
    }
    let [v4, v6] = families;
    let mut families = [v4.into_iter(), v6.into_iter()];
    let mut chosen = Vec::with_capacity(MAX_CANDIDATES);
    for index in 0..MAX_CANDIDATES {
        let family = index % 2;
        let next = families[family]
            .next()
            .or_else(|| families[1 - family].next());
        let Some(next) = next else { break };
        chosen.push(next);
    }
    chosen
}

/// A path needs a locally bound transport of the same family. Synthetic
/// destinations additionally require an attached relay, not an ordinary IPv4
/// socket. This also filters QNT advertisements before they can cause a fatal
/// send on an unsupported socket family.
pub(super) fn supports_candidate(local_addrs: &[SocketAddr], remote: SocketAddr) -> bool {
    if super::relay::is_synthetic(remote) {
        return local_addrs
            .iter()
            .any(|addr| super::relay::is_synthetic(*addr));
    }
    local_addrs.iter().any(|addr| {
        !super::relay::is_synthetic(*addr)
            && addr.ip().to_canonical().is_ipv4() == remote.ip().to_canonical().is_ipv4()
    })
}

/// Open additional paths for the remaining candidates.
///
/// `open_path_ensure` deduplicates against the path `connect` already
/// opened — passing the primary again returns its `PathId`. Returned IDs
/// include pending paths and must never be treated as validation evidence.
/// Newly opened paths remain Backup until the driver's Established event.
/// Failures are logged, not fatal: an established path keeps the connection alive.
pub fn open_extra_paths(conn: &noq::Connection, candidates: &[SocketAddr]) -> Vec<noq::PathId> {
    let mut ids = Vec::new();
    for addr in candidates {
        let open = conn.open_path_ensure(*addr, noq::PathStatus::Backup);
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
/// event streams ending. Only the completed handshake's PathId::ZERO is seeded;
/// other paths become eligible on Established, never merely on path creation.
/// The caller must subscribe before opening additional paths or starting QNT.
#[allow(clippy::too_many_arguments)]
pub async fn connection_driver(
    conn: noq::WeakConnectionHandle,
    qnt: noq::NatTraversalUpdates,
    path_events: noq::PathEvents,
    metrics: crate::metrics::Registry,
    local_addrs: Vec<SocketAddr>,
    initial_candidates: Vec<SocketAddr>,
    relay: Option<super::relay::RelayHandle>,
    allow_direct: bool,
) {
    let Some(telemetry) = conn.upgrade().map(|c| super::telemetry::Telemetry::new(&c)) else {
        return;
    };
    let _guard = telemetry.guard();
    connection_driver_observed(
        conn,
        qnt,
        Observer {
            events: path_events,
            telemetry,
            transport: None,
        },
        metrics,
        local_addrs,
        initial_candidates,
        relay,
        allow_direct,
    )
    .await;
}

pub(super) struct Observer {
    pub events: noq::PathEvents,
    pub telemetry: std::sync::Arc<super::telemetry::Telemetry>,
    pub transport: Option<super::socket::Health>,
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn connection_driver_observed(
    conn: noq::WeakConnectionHandle,
    mut qnt: noq::NatTraversalUpdates,
    observer: Observer,
    metrics: crate::metrics::Registry,
    local_addrs: Vec<SocketAddr>,
    initial_candidates: Vec<SocketAddr>,
    relay: Option<super::relay::RelayHandle>,
    allow_direct: bool,
) {
    use tokio_stream::StreamExt;
    let Observer {
        events: mut path_events,
        telemetry,
        transport,
    } = observer;
    let mut transport_changes = transport.as_ref().map(super::socket::Health::changes);
    let mut local_addrs = local_addrs;

    // Register without retaining a strong Connection across any await.
    let Some(closed) = conn.upgrade().map(|owner| owner.on_closed()) else {
        return;
    };
    tokio::pin!(closed);
    let mut relay_down = relay.as_ref().is_some_and(|handle| !handle.is_available());
    let health = relay.clone();
    let relay_unavailable = async move {
        match health {
            Some(mut handle) => handle.unavailable().await,
            None => std::future::pending().await,
        }
    };
    tokio::pin!(relay_unavailable);
    let mut relay_withdrawn = false;

    let mut paths = telemetry.paths();
    // Paths opened to QNT-learned candidates — an Established event on
    // one of these is a QNT success.
    let mut qnt_paths: std::collections::HashSet<noq::PathId> = std::collections::HashSet::new();
    let mut selected: Option<noq::PathId> = None;

    let mut pending = Pending::default();
    // `Transports::RelayOnly` endpoints admit only synthetic relay
    // candidates — direct addrs the peer advertises in-band can never
    // become paths, regardless of what the ticket or QNT carried.
    let admit = |address: SocketAddr| allow_direct || super::relay::is_synthetic(address);
    for address in initial_candidates {
        if admit(address) && supports_candidate(&local_addrs, address) {
            pending.offer(address, Origin::Ticket, Instant::now());
        }
    }
    // Subscriptions were created by the caller before this snapshot. Reconcile
    // addresses learned during TLS as well as later broadcast updates.
    reconcile_candidates(&conn, &local_addrs, &mut pending, allow_direct);

    let mut qnt_open = true;
    let mut events_open = true;
    let mut tick = tokio::time::interval(RESELECT_INTERVAL);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        let wake = pending.next_wake();
        tokio::select! {
            _ = &mut closed => break,
            _ = std::future::poll_fn(|cx| match &mut transport_changes {
                Some(changes) => changes.poll_changed(cx),
                None => std::task::Poll::Pending,
            }) => {},
            _ = &mut relay_unavailable, if !relay_down => { relay_down = true; },
            _ = tokio::time::sleep_until(wake.unwrap_or_else(Instant::now)), if wake.is_some() => {},
            event = qnt.next(), if qnt_open => match event {
                Some(Ok(noq_proto::n0_nat_traversal::Event::AddressAdded(addr))) => {
                    if admit(addr) && supports_candidate(&local_addrs, addr) {
                        pending.offer(addr, Origin::Advertisement, Instant::now());
                    }
                }
                Some(Ok(noq_proto::n0_nat_traversal::Event::AddressRemoved(addr))) => {
                    pending.withdraw(addr);
                    tracing::debug!(%addr, "peer withdrew candidate");
                }
                Some(Err(lagged)) => {
                    tracing::warn!("QNT update stream lagged by {}", lagged.0);
                    reconcile_candidates(&conn, &local_addrs, &mut pending, allow_direct);
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
                    telemetry.lagged(lagged.0);
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
        if let Some(health) = &transport {
            if health.all_failed() {
                // Fatal I/O can stop the engine before it closes held handles.
                // Policy owns an explicit close, independent of engine polling.
                owner.close(0u32.into(), b"all local transports failed");
                break;
            }
            local_addrs.retain(|address| {
                if health.advertisement_available(*address) {
                    return true;
                }
                let _ = owner.remove_nat_traversal_address(*address);
                false
            });
            pending.retain(|remote| health.path_available(remote, None));
            for weak in paths.values() {
                if let Some(path) = weak.upgrade()
                    && let Ok(network) = path.network_path()
                    && !health.path_available(network.remote(), network.local_ip())
                {
                    let _ = path.set_status(noq::PathStatus::Backup);
                    let _ = path.close();
                }
            }
        }
        if relay_down {
            pending.retain(|address| !super::relay::is_synthetic(address));
            if !relay_withdrawn {
                for address in &local_addrs {
                    if super::relay::is_synthetic(*address) {
                        let _ = owner.remove_nat_traversal_address(*address);
                    }
                }
                relay_withdrawn = true;
            }
        }
        for opened in pending.open_due(&owner, Instant::now()) {
            if opened.learned && !paths.contains_key(&opened.id) && qnt_paths.insert(opened.id) {
                metrics.qnt_attempt();
            }
        }
        // Lost/lagged path events cannot accumulate stale QNT history.
        qnt_paths.retain(|id| owner.path(*id).is_some());
        drop(owner);
        reselect(
            &conn,
            &mut paths,
            &mut selected,
            relay_down,
            transport.as_ref(),
        );
        telemetry.publish(&paths, selected);
    }
}

/// Snapshot only candidate advertisements, never infer path validation from
/// address presence. The current noq API has no validated-path snapshot.
fn reconcile_candidates(
    conn: &noq::WeakConnectionHandle,
    local_addrs: &[SocketAddr],
    pending: &mut Pending,
    allow_direct: bool,
) {
    if let Some(owner) = conn.upgrade()
        && let Ok(addresses) = owner.get_remote_nat_traversal_addresses()
    {
        pending.reconcile(
            addresses.into_iter().filter(|addr| {
                (allow_direct || super::relay::is_synthetic(*addr))
                    && supports_candidate(local_addrs, *addr)
            }),
            Instant::now(),
        );
    }
}

/// Apply the biased-RTT selection: the lowest-RTT path becomes
/// `Available`, all others `Backup`. The current selection is kept
/// unless a candidate beats it by at least [`RTT_SWITCHING_MIN`].
fn reselect(
    conn: &noq::WeakConnectionHandle,
    paths: &mut HashMap<noq::PathId, noq::WeakPathHandle>,
    selected: &mut Option<noq::PathId>,
    relay_down: bool,
    transport: Option<&super::socket::Health>,
) {
    if !conn.is_alive() {
        return;
    }
    // A WeakPathHandle can upgrade even after its path closes: it retains
    // final statistics until dropped. Upgrade alone is not a liveness check.
    paths.retain(|_, weak| weak.upgrade().is_some_and(|path| path.status().is_ok()));
    // Tunnel loss is authoritative local link state; stale RTT must not keep
    // a dead relay selected over a validated direct path. Close only known
    // validated paths here; unobserved engine/QNT paths remain its responsibility.
    if relay_down {
        for weak in paths.values() {
            if let Some(path) = weak.upgrade()
                && path.remote_address().is_ok_and(super::relay::is_synthetic)
            {
                let _ = path.set_status(noq::PathStatus::Backup);
                let _ = path.close();
            }
        }
        paths.retain(|_, weak| weak.upgrade().is_some_and(|path| path.status().is_ok()));
    }
    if paths.is_empty() {
        *selected = None;
        return;
    }

    let rtts: Vec<(noq::PathId, Duration)> = paths
        .iter()
        .filter_map(|(id, weak)| {
            let path = weak.upgrade()?;
            // Noq may refuse to close its last path. A retained path handle
            // must not turn a terminal local transport back into a selection.
            if let Some(health) = transport {
                let network = path.network_path().ok()?;
                if !health.path_available(network.remote(), network.local_ip()) {
                    return None;
                }
            }
            if relay_down && path.remote_address().is_ok_and(super::relay::is_synthetic) {
                return None;
            }
            Some((*id, path.stats().rtt))
        })
        .collect();
    let Some((mut choice, best_rtt)) = rtts.iter().min_by_key(|(_, rtt)| *rtt).copied() else {
        *selected = None;
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
    // A close or engine error can race selection. Publish only a successfully
    // applied choice, never an attempted set_status.
    *selected = paths
        .get(&choice)
        .and_then(|weak| weak.upgrade())
        .filter(|path| path.status().ok() == Some(noq::PathStatus::Available))
        .map(|_| choice);
}

#[cfg(test)]
mod candidate_tests {
    use super::*;
    use std::net::{Ipv6Addr, SocketAddrV6};
    fn target(addresses: impl IntoIterator<Item = SocketAddr>) -> EndpointAddr {
        EndpointAddr {
            id: crate::SecretKey::from_bytes(&[114; 32]).public(),
            addrs: addresses.into_iter().map(TransportAddr::Ip).collect(),
        }
    }
    fn v4(port: u16) -> SocketAddr {
        ([192, 0, 2, 1], port).into()
    }
    fn v6(port: u16) -> SocketAddr {
        SocketAddr::new("2001:db8::1".parse().unwrap(), port)
    }

    #[test]
    fn long_candidate_lists_share_the_budget_between_families() {
        let input = target((10000..10020).flat_map(|port| [v4(port), v6(port)]));
        let expected: Vec<_> = (10000..10004)
            .flat_map(|port| [v4(port), v6(port)])
            .collect();
        assert_eq!(ip_candidates(&input), expected);
    }
    #[test]
    fn unused_family_slots_are_filled_by_the_other_family() {
        for sparse_v4 in [false, true] {
            let one = if sparse_v4 { v4(9000) } else { v6(9000) };
            let input = target(
                std::iter::once(one)
                    .chain((10000..10020).map(|port| if sparse_v4 { v6(port) } else { v4(port) })),
            );
            let selected = ip_candidates(&input);
            assert_eq!(selected.len(), MAX_CANDIDATES);
            assert!(selected.contains(&one));
            assert_eq!(
                selected.iter().filter(|a| a.is_ipv4() == sparse_v4).count(),
                1
            );
        }
        let selected = ip_candidates(&target((10000..10020).map(v4)));
        assert_eq!(selected, (10000..10008).map(v4).collect::<Vec<_>>());
    }
    #[test]
    fn mapped_aliases_share_a_slot_and_native_ipv6_scope_survives() {
        let ip: Ipv6Addr = "fe80::1".parse().unwrap();
        let one = SocketAddr::V6(SocketAddrV6::new(ip, 4433, 0, 1));
        let two = SocketAddr::V6(SocketAddrV6::new(ip, 4433, 0, 2));
        let selected = ip_candidates(&target([
            v4(1000),
            "[::ffff:192.0.2.1]:1000".parse().unwrap(),
            one,
            two,
        ]));
        assert_eq!(selected.len(), 3);
        assert!(selected.contains(&v4(1000)));
        assert!(selected.contains(&one));
        assert!(selected.contains(&two));
    }
    #[test]
    fn family_filtering_precedes_budget_and_normalizes_mapped_addresses() {
        let input = target((10000..10020).map(v4).chain([v6(4433)]));
        assert_eq!(
            dial_candidates(&input, &["[::1]:0".parse().unwrap()]),
            vec![v6(4433)]
        );
        let mapped = target(["[::ffff:192.0.2.1]:1000".parse().unwrap()]);
        assert_eq!(
            dial_candidates(&mapped, &["127.0.0.1:0".parse().unwrap()]),
            vec![v4(1000)]
        );
    }
}
