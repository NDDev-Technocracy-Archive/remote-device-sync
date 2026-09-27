//! Application admission limits, independent of transport flow-control credit.

use std::num::NonZeroU16;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

/// Per-agent connection slots and per-connection service tasks, plus an
/// optional process resource ceiling. The constructor requires positive
/// values; callers may choose smaller budgets for their host.
#[derive(Clone, Copy, Debug)]
pub struct AgentLimits {
    connections: usize,
    streams: usize,
    max_fds: Option<u64>,
    max_rss_bytes: Option<u64>,
}

impl Default for AgentLimits {
    fn default() -> Self {
        Self {
            connections: 32,
            streams: 64,
            max_fds: None,
            max_rss_bytes: None,
        }
    }
}

impl AgentLimits {
    pub fn new(connections: NonZeroU16, streams: NonZeroU16) -> Self {
        Self {
            connections: usize::from(connections.get()),
            streams: usize::from(streams.get()),
            ..Default::default()
        }
    }

    /// Process-level ceiling: refuse admissions once the process holds
    /// `max_fds` descriptors or `max_rss_mb` resident MiB. `None` leaves
    /// that quantity ungated.
    pub fn with_process_budget(mut self, max_fds: Option<u64>, max_rss_mb: Option<u64>) -> Self {
        self.max_fds = max_fds;
        self.max_rss_bytes = max_rss_mb.and_then(|mb| mb.checked_mul(1024 * 1024));
        self
    }

    /// Configured fd ceiling, if any.
    pub fn max_fds(self) -> Option<u64> {
        self.max_fds
    }

    /// Configured resident-set ceiling in bytes, if any.
    pub fn max_rss_bytes(self) -> Option<u64> {
        self.max_rss_bytes
    }

    /// Pending handshakes plus admitted connections, shared by run and serve.
    pub fn connections(self) -> usize {
        self.connections
    }

    /// Concurrent tasks, including hello/Authz I/O. Grant mode requires at
    /// least two and reserves one from long-lived service bodies for renewal.
    pub fn streams(self) -> usize {
        self.streams
    }
}

#[derive(Clone, Default)]
pub(super) struct StreamCounter(Arc<AtomicUsize>);

impl StreamCounter {
    pub fn active(&self) -> usize {
        self.0.load(Ordering::Relaxed)
    }

    pub fn enter(&self) -> StreamTask {
        self.0.fetch_add(1, Ordering::Relaxed);
        StreamTask(self.0.clone())
    }
}

pub(super) struct StreamTask(Arc<AtomicUsize>);

impl Drop for StreamTask {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::Relaxed);
    }
}

/// How often the process budget re-observes the kernel's view. Re-statting
/// per accepted connection would put procfs/`dev` scans on the hot path.
const SAMPLE_INTERVAL: std::time::Duration = std::time::Duration::from_millis(200);

/// Process-level admission ceiling. Ungated (`None` limits) or
/// unobservable (platform reports neither fds nor RSS) configurations
/// never refuse — a bound only exists where the kernel actually reports it.
pub(super) struct ResourceGate {
    max_fds: Option<u64>,
    max_rss_bytes: Option<u64>,
    cache: std::sync::Mutex<(std::time::Instant, bool)>,
}

impl ResourceGate {
    pub fn new(limits: AgentLimits) -> Option<Self> {
        if limits.max_fds.is_none() && limits.max_rss_bytes.is_none() {
            return None;
        }
        Some(Self {
            max_fds: limits.max_fds,
            max_rss_bytes: limits.max_rss_bytes,
            cache: std::sync::Mutex::new((std::time::Instant::now() - 2 * SAMPLE_INTERVAL, true)),
        })
    }

    /// True while the observed process usage fits the configured budget.
    /// An unobservable quantity contributes nothing — the gate reports
    /// only what the kernel actually showed it.
    pub fn allows(&self) -> bool {
        let mut cache = crate::lock(&self.cache);
        let (at, verdict) = *cache;
        if at.elapsed() < SAMPLE_INTERVAL {
            return verdict;
        }
        let mut ok = true;
        let mut observed = false;
        if let Some(max) = self.max_fds
            && let Some(fds) = crate::sys::open_fds()
        {
            observed = true;
            ok &= (fds as u64) < max;
        }
        if let Some(max) = self.max_rss_bytes
            && let Some(rss) = crate::sys::rss_bytes()
        {
            observed = true;
            ok &= rss < max;
        }
        // Nothing observable: keep serving rather than gate on a guess.
        let verdict = ok || !observed;
        *cache = (std::time::Instant::now(), verdict);
        verdict
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_budget_means_no_gate() {
        assert!(ResourceGate::new(AgentLimits::default()).is_none());
        assert!(
            ResourceGate::new(AgentLimits::default().with_process_budget(None, None)).is_none()
        );
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn impossible_fd_ceiling_refuses() {
        let limits = AgentLimits::default().with_process_budget(Some(1), None);
        let gate = ResourceGate::new(limits).expect("budget configured");
        assert!(!gate.allows());
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn generous_ceiling_allows() {
        let limits = AgentLimits::default().with_process_budget(Some(u64::MAX), Some(u64::MAX));
        let gate = ResourceGate::new(limits).expect("budget configured");
        assert!(gate.allows());
    }
}
