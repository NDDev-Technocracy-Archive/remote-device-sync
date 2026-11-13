//! Directory announce loop (WS3 D2).
//!
//! Publishes this endpoint's signed [`EndpointRecord`] on start, then
//! refreshes it at `ttl/3`, and re-publishes promptly when the
//! advertised address set changes (observed external addr, home relay,
//! new socket). Publish failures are logged and retried on the next
//! tick. Disk-full publication pauses retain the transport and retry only after
//! reconciling the exact owned commit. Fatal history or protocol errors reach `Announce::wait`
//! so the agent supervisor can close the endpoint and report failure.

use std::time::Duration;

use rand::RngExt;

use crate::{EndpointAddr, TransportAddr};
use rds_discovery::client::Client as DirectoryClient;
use rds_discovery::publisher::{RecordDraft, RecordIssuer};
use rds_discovery::{
    DiscoveryError, EndpointRecord, MAX_DIRECT_ADDRS, MAX_RECORD_TTL, MAX_RELAY_URLS, Service,
};

use crate::Endpoint;

/// Parameters for [`announce`].
pub struct AnnounceConfig {
    /// Single-owner issuer matching the endpoint; durable in production.
    pub issuer: RecordIssuer,
    /// Directory to publish into.
    pub directory: DirectoryClient,
    /// Services this endpoint serves.
    pub services: Vec<Service>,
    /// Record TTL; the loop refreshes at `ttl/3`.
    pub ttl: Duration,
    /// Publish-failure retry bounds.
    pub retry: RetryPolicy,
}

/// Bounded exponential backoff for retried directory publishes.
///
/// Delays grow `base × 2^failures` up to `cap`; the applied sleep uses
/// equal jitter — uniform inside `[delay/2, delay]` — so the minimum
/// cadence remains provable (`base/2`) while fleet retries decorrelate.
/// The healthy poll cadence is untouched: backoff replaces one poll only
/// after a retryable failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetryPolicy {
    /// Delay used by the first retry after a failure.
    pub base: Duration,
    /// Longest delay between retries.
    pub cap: Duration,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            base: Duration::from_secs(1),
            cap: Duration::from_secs(30),
        }
    }
}

impl RetryPolicy {
    /// Sleep duration for the `failures`-th consecutive retryable
    /// failure (1-indexed): `min(cap, base × 2^(failures-1))` with equal
    /// jitter inside `[delay/2, delay]`. Public because it is the
    /// shared bounded-backoff primitive ([`crate::deadline::retry_wait`]
    /// builds the cancellable sleep on it).
    pub fn delay(&self, failures: u32) -> Duration {
        let shift = failures.saturating_sub(1).min(16);
        let delay = self.base.saturating_mul(1 << shift).min(self.cap);
        let half_ms = (delay.as_millis() / 2).min(u64::MAX as u128) as u64;
        Duration::from_millis(half_ms)
            + Duration::from_millis(rand::rng().random_range(0..=half_ms))
    }
}

/// Running announce task. `Drop` stops publishing; already-published
/// records expire naturally.
pub struct Announce {
    task: tokio::task::JoinHandle<Result<(), DiscoveryError>>,
}

impl Announce {
    /// Observe fatal issuer/history failures. Network and disk-full outages
    /// retry without replacing the endpoint; invalid history remains fatal.
    pub async fn wait(&mut self) -> Result<(), DiscoveryError> {
        (&mut self.task)
            .await
            .map_err(|e| DiscoveryError::Store(e.to_string()))?
    }
}

impl Drop for Announce {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// Start the announce loop. The first publish happens inside the
/// returned task; the loop re-checks the advertised address set at
/// `min(1s, ttl/6)` so observed-addr changes publish promptly.
pub fn announce(endpoint: Endpoint, config: AnnounceConfig) -> Result<Announce, DiscoveryError> {
    if config.issuer.key().0 != *endpoint.id().as_bytes() {
        return Err(DiscoveryError::BadSignature);
    }
    if config.ttl.as_secs() == 0
        || config.ttl.as_secs() > MAX_RECORD_TTL
        || config.ttl.subsec_nanos() != 0
    {
        return Err(DiscoveryError::Configuration(
            "record TTL must be 1..=3600 whole seconds".into(),
        ));
    }
    if config.retry.base.is_zero() || config.retry.base > config.retry.cap {
        return Err(DiscoveryError::Configuration(
            "publish retry needs 0 < base <= cap".into(),
        ));
    }
    let task = tokio::spawn(async move {
        let AnnounceConfig {
            mut issuer,
            directory,
            services,
            ttl,
            retry,
        } = config;
        let poll = (ttl / 6).clamp(Duration::from_millis(250), Duration::from_secs(1));
        let mut last: Option<EndpointRecord> = None;
        let mut renew = false;
        let mut failures = 0u32;
        let mut storage_full = false;
        let mut storage_pause_started: Option<tokio::time::Instant> = None;
        let mut last_omitted = 0usize;
        loop {
            let (addrs, relay_urls, omitted) = split_addrs(&endpoint.addr());
            if omitted != last_omitted {
                if omitted > 0 {
                    tracing::warn!(
                        omitted,
                        kept = addrs.len(),
                        "more direct addresses than a record carries; publishing the most reachable"
                    );
                }
                last_omitted = omitted;
            }
            let draft = RecordDraft {
                addrs,
                relay_urls,
                services: services.clone(),
                ttl,
            };
            // At most one disk job. Cancellation can let that job finish a
            // local commit, but the canceled task cannot publish its result.
            let (returned, result) = tokio::task::spawn_blocking(move || {
                let result = rds_discovery::now_unix().and_then(|now| {
                    if storage_full {
                        issuer.recover_storage_full(now)?;
                    }
                    if renew {
                        issuer.renew_record(draft, now)
                    } else {
                        issuer.record(draft, now)
                    }
                });
                (issuer, result)
            })
            .await
            .map_err(|e| DiscoveryError::Store(e.to_string()))?;
            issuer = returned;
            renew = false;
            let record = match result {
                Err(DiscoveryError::StorageUnavailable(_)) => {
                    storage_full = true;
                    let started =
                        storage_pause_started.get_or_insert_with(tokio::time::Instant::now);
                    failures = failures.saturating_add(1);
                    let delay = retry.delay(failures);
                    tracing::warn!(
                        failures,
                        pause_ms = started.elapsed().as_millis(),
                        retry_delay_ms = delay.as_millis(),
                        "directory publication paused: durable storage is full; transport retained"
                    );
                    tokio::time::sleep(delay).await;
                    continue;
                }
                other => {
                    storage_full = false;
                    let record = other?;
                    if let Some(started) = storage_pause_started.take() {
                        tracing::info!(
                            pause_ms = started.elapsed().as_millis(),
                            "publisher storage recovered; durable directory publication resumed"
                        );
                    }
                    record
                }
            };
            if last.as_ref() != Some(&record) {
                match directory.publish(&record).await {
                    Ok(()) => {
                        last = Some(record);
                        failures = 0;
                        tracing::debug!("endpoint record published");
                    }
                    Err(DiscoveryError::Http { status: 410, .. }) => {
                        renew = true;
                        tracing::debug!("directory lease expired; allocating a local successor");
                    }
                    Err(
                        e @ DiscoveryError::Http {
                            status: 400..=407 | 409 | 411..=428 | 430..=499,
                            ..
                        },
                    ) => return Err(e),
                    Err(e) => {
                        tracing::warn!("directory publish failed: {e}");
                        failures = failures.saturating_add(1);
                        // Bounded backoff, not the healthy poll: a sustained
                        // outage must not publish-storm the directory.
                        tokio::time::sleep(retry.delay(failures)).await;
                        continue;
                    }
                }
            }
            tokio::time::sleep(poll).await;
        }
    });
    Ok(Announce { task })
}

/// Split an [`EndpointAddr`] into the direct socket addrs and relay urls a
/// signed record can carry, and count the direct addrs left out.
///
/// A record holds at most [`MAX_DIRECT_ADDRS`] direct addresses, and the
/// issuer refuses a larger set as a fatal error. A host with many bridges
/// (Docker, libvirt, Incus) reports more than that, so keep the most reachable:
/// public addresses, then private IPv4 with home networks before the ranges
/// container engines take, then IPv6 unique-local, then link-local. The
/// address set is ordered numerically, so a plain cut kept bridge gateways
/// such as `10.210.x.1` and dropped the LAN address. Addresses a record
/// refuses (port 0, unspecified, multicast) are never advertised.
fn split_addrs(addr: &EndpointAddr) -> (Vec<std::net::SocketAddr>, Vec<String>, usize) {
    let mut addrs = Vec::new();
    let mut relays = Vec::new();
    for t in &addr.addrs {
        match t {
            TransportAddr::Ip(sock)
                if sock.port() != 0 && !sock.ip().is_unspecified() && !sock.ip().is_multicast() =>
            {
                addrs.push(*sock)
            }
            TransportAddr::Relay(url) => relays.push(url.to_string()),
            _ => {}
        }
    }
    addrs.sort_by_key(|sock| reach(sock.ip()));
    let omitted = addrs.len().saturating_sub(MAX_DIRECT_ADDRS);
    addrs.truncate(MAX_DIRECT_ADDRS);
    relays.truncate(MAX_RELAY_URLS);
    (addrs, relays, omitted)
}

/// Rank of an address for peers on other hosts: lower is more reachable.
fn reach(ip: std::net::IpAddr) -> u8 {
    match ip {
        std::net::IpAddr::V4(v4) => {
            let [a, b, ..] = v4.octets();
            if v4.is_link_local() {
                6
            } else if a == 192 && b == 168 {
                1
            } else if a == 172 && (16..32).contains(&b) {
                2
            } else if a == 10 || (a == 100 && (64..128).contains(&b)) {
                3
            } else {
                0
            }
        }
        std::net::IpAddr::V6(v6) => {
            let first = v6.segments()[0];
            if first & 0xffc0 == 0xfe80 {
                7
            } else if first & 0xfe00 == 0xfc00 {
                4
            } else {
                0
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn endpoint_addr(addrs: impl IntoIterator<Item = &'static str>) -> EndpointAddr {
        let mut addr = EndpointAddr::new(crate::SecretKey::generate().public());
        for a in addrs {
            addr.addrs.insert(TransportAddr::Ip(a.parse().unwrap()));
        }
        addr
    }

    #[test]
    fn a_host_with_many_bridges_publishes_its_lan_and_public_addresses_first() {
        // The Ubuntu PC of 13 November: 38 Docker bridges in 10.210.0.0/16 with
        // IPv6 link-local twins, its LAN address and its public address.
        let bridges: Vec<&'static str> = (0..38)
            .flat_map(|n| {
                [
                    Box::leak(format!("10.210.{n}.1:3345").into_boxed_str()) as &str,
                    Box::leak(format!("[fe80::{n:x}:1]:3345").into_boxed_str()) as &str,
                ]
            })
            .collect();
        let addr = endpoint_addr(bridges.into_iter().chain([
            "192.168.31.221:3345",
            "95.31.0.7:3345",
            "0.0.0.0:3345",
        ]));

        let (addrs, _, omitted) = split_addrs(&addr);

        assert_eq!(addrs.len(), MAX_DIRECT_ADDRS);
        assert_eq!(omitted, 76 + 2 - MAX_DIRECT_ADDRS);
        assert_eq!(addrs[0], "95.31.0.7:3345".parse().unwrap());
        assert_eq!(addrs[1], "192.168.31.221:3345".parse().unwrap());
        assert!(addrs.iter().all(|a| !a.ip().is_unspecified()));
        assert!(addrs[2..].iter().all(|a| a.ip().is_ipv4()));
    }

    #[test]
    fn a_small_host_keeps_every_address_it_reports() {
        let addr = endpoint_addr(["192.168.1.5:3345", "[fe80::1]:3345", "203.0.113.9:3345"]);

        let (addrs, _, omitted) = split_addrs(&addr);

        assert_eq!(omitted, 0);
        assert_eq!(addrs.len(), 3);
        assert_eq!(addrs[2], "[fe80::1]:3345".parse().unwrap());
    }

    #[test]
    fn retry_delay_stays_inside_equal_jitter_bounds() {
        let policy = RetryPolicy {
            base: Duration::from_millis(100),
            cap: Duration::from_secs(2),
        };
        // Pre-jitter delays: 100ms, 200ms, 400ms, 800ms, 1600ms, then capped 2s.
        for (failures, expected) in [
            (1, 100u64),
            (2, 200),
            (3, 400),
            (4, 800),
            (5, 1600),
            (6, 2000),
            (60, 2000),
        ] {
            let nominal = Duration::from_millis(expected);
            for _ in 0..64 {
                let delay = policy.delay(failures);
                assert!(
                    delay >= nominal / 2 && delay <= nominal,
                    "failures={failures}: {delay:?} outside [nominal/2, nominal]"
                );
            }
        }
    }

    #[test]
    fn retry_delay_never_exceeds_cap_even_after_many_failures() {
        let policy = RetryPolicy {
            base: Duration::from_secs(1),
            cap: Duration::from_secs(30),
        };
        for failures in (0..=u32::MAX).step_by(1 << 20) {
            assert!(policy.delay(failures) <= policy.cap);
        }
    }
}
