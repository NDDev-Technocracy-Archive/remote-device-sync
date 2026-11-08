//! Validated observation settings shared by every native viewer entry point.
use serde::{Deserialize, Serialize};

const DEFAULT_INTERVAL_MS: u64 = 2_000;
const DEFAULT_RETAINED_INCIDENTS: usize = 10;
pub(crate) const MAX_RETAINED_INCIDENTS: usize = 1_000;

#[derive(Clone, clap::Args, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Policy {
    /// In-memory health sampling cadence, in milliseconds (100..60000).
    #[arg(long = "diagnostic-interval-ms", default_value_t = DEFAULT_INTERVAL_MS)]
    pub sample_interval_ms: u64,
    /// Also publish live JSON files for this many seconds, then return to memory.
    #[arg(long = "diagnostic-snapshot-seconds", default_value_t = 0)]
    pub snapshot_window_seconds: u64,
    /// Maximum completed fault windows retained in the private directory.
    #[arg(long = "diagnostic-retained-incidents", default_value_t = DEFAULT_RETAINED_INCIDENTS)]
    pub retained_incidents: usize,
}

impl Default for Policy {
    fn default() -> Self {
        Self {
            sample_interval_ms: DEFAULT_INTERVAL_MS,
            snapshot_window_seconds: 0,
            retained_incidents: DEFAULT_RETAINED_INCIDENTS,
        }
    }
}

impl Policy {
    pub fn validate(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            (100..=60_000).contains(&self.sample_interval_ms),
            "diagnostic interval must be 100..60000 milliseconds"
        );
        anyhow::ensure!(
            self.snapshot_window_seconds <= 3_600,
            "live diagnostic files require a window of at most 3600 seconds"
        );
        anyhow::ensure!(
            (1..=MAX_RETAINED_INCIDENTS).contains(&self.retained_incidents),
            "diagnostic incident retention must be 1..1000"
        );
        Ok(())
    }

    #[cfg(any(feature = "desktop", test))]
    pub(crate) fn writes_snapshot(&self, elapsed_ms: u64) -> bool {
        self.snapshot_window_seconds != 0
            && elapsed_ms < self.snapshot_window_seconds.saturating_mul(1_000)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ordinary_observation_never_selects_live_disk_capture() {
        let mut policy = Policy::default();
        policy.validate().unwrap();
        assert!(!policy.writes_snapshot(0));
        policy.snapshot_window_seconds = 3;
        assert!(policy.writes_snapshot(2_999));
        assert!(!policy.writes_snapshot(3_000));
        policy.sample_interval_ms = 0;
        assert!(policy.validate().is_err());
        policy.sample_interval_ms = DEFAULT_INTERVAL_MS;
        policy.snapshot_window_seconds = 3_601;
        assert!(policy.validate().is_err());
        policy.snapshot_window_seconds = 0;
        policy.retained_incidents = 0;
        assert!(policy.validate().is_err());
    }
}
