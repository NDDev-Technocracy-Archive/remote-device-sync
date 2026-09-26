//! Machine-readable bench reports: percentile math, JSON, markdown,
//! and run-to-run drift comparison used by the checkpoint gates.

use serde::{Deserialize, Serialize};

use crate::impair::Impairment;

/// Nearest-rank percentiles over a set of duration samples (ns).
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct Percentiles {
    pub count: usize,
    pub min_ns: u64,
    pub p50_ns: u64,
    pub p95_ns: u64,
    pub p99_ns: u64,
    pub max_ns: u64,
    pub mean_ns: f64,
}

impl Percentiles {
    /// `None` for empty input — callers must not fabricate stats.
    pub fn of(samples_ns: &[u64]) -> Option<Self> {
        if samples_ns.is_empty() {
            return None;
        }
        let mut s = samples_ns.to_vec();
        s.sort_unstable();
        let rank = |p: f64| -> u64 {
            let idx = ((p / 100.0) * s.len() as f64).ceil() as usize;
            s[idx.saturating_sub(1).min(s.len() - 1)]
        };
        Some(Self {
            count: s.len(),
            min_ns: s[0],
            p50_ns: rank(50.0),
            p95_ns: rank(95.0),
            p99_ns: rank(99.0),
            max_ns: *s.last().unwrap(),
            mean_ns: s.iter().sum::<u64>() as f64 / s.len() as f64,
        })
    }
}

/// What produced a report: enough to reproduce or refute it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BenchMeta {
    pub scenario: String,
    pub backend: String,
    pub path: String,
    pub impairment: Option<Impairment>,
    pub unix_ts: u64,
    pub git: Option<String>,
}

/// One scenario's result.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BenchReport {
    pub meta: BenchMeta,
    /// Latency-style result; `None` when the scenario measures throughput.
    pub rtt: Option<Percentiles>,
    /// Throughput result in MiB/s.
    pub throughput_mib_s: Option<f64>,
    /// Attempted vs succeeded for scenarios where failure is data
    /// (e.g. multiconnect under loss).
    pub attempts: Option<(u64, u64)>,
    /// Transport-registry snapshot read by the harness itself (G7):
    /// `client_*`/`agent_*` prefixed `rds_net_*` counter names — the
    /// same names `render_prometheus` exports.
    #[serde(default)]
    pub metrics: std::collections::BTreeMap<String, u64>,
    /// Free-form facts: proxy stats, errors seen, environment notes.
    pub notes: Vec<String>,
}

impl BenchReport {
    /// `p95` in milliseconds for drift comparison.
    pub fn p95_ms(&self) -> Option<f64> {
        self.rtt.map(|p| p.p95_ns as f64 / 1e6)
    }

    fn render_md(&self, out: &mut String) {
        use std::fmt::Write;
        let _ = writeln!(
            out,
            "## {} ({}, {})\n",
            self.meta.scenario, self.meta.backend, self.meta.path
        );
        if let Some(i) = &self.meta.impairment {
            let _ = writeln!(out, "impairment: `{}`\n", i.describe());
        }
        if let Some(p) = &self.rtt {
            let _ = writeln!(out, "| n | min | p50 | p95 | p99 | max | mean |");
            let _ = writeln!(out, "|---|-----|-----|-----|-----|-----|------|");
            let _ = writeln!(
                out,
                "| {} | {:.2}ms | {:.2}ms | {:.2}ms | {:.2}ms | {:.2}ms | {:.2}ms |\n",
                p.count,
                p.min_ns as f64 / 1e6,
                p.p50_ns as f64 / 1e6,
                p.p95_ns as f64 / 1e6,
                p.p99_ns as f64 / 1e6,
                p.max_ns as f64 / 1e6,
                p.mean_ns / 1e6,
            );
        }
        if let Some(t) = self.throughput_mib_s {
            let _ = writeln!(out, "throughput: **{t:.1} MiB/s**\n");
        }
        if let Some((ok, total)) = self.attempts {
            let _ = writeln!(out, "attempts: {ok}/{total} succeeded\n");
        }
        let nonzero: Vec<_> = self.metrics.iter().filter(|(_, v)| **v > 0).collect();
        if !nonzero.is_empty() {
            let _ = writeln!(out, "metrics:");
            for (k, v) in nonzero {
                let _ = writeln!(out, "- `{k}` = {v}");
            }
            let _ = writeln!(out);
        }
        for n in &self.notes {
            let _ = writeln!(out, "- {n}");
        }
        let _ = writeln!(out);
    }
}

/// A full run: one JSON file holds every scenario measured together.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BenchSuite {
    pub tool: String,
    pub unix_ts: u64,
    pub git: Option<String>,
    pub reports: Vec<BenchReport>,
}

impl BenchSuite {
    pub fn to_markdown(&self) -> String {
        let mut s = format!("# rds bench suite — {}\n\n", self.unix_ts);
        if let Some(g) = &self.git {
            s.push_str(&format!("commit: `{g}`\n\n"));
        }
        for r in &self.reports {
            r.render_md(&mut s);
        }
        s
    }
}

/// One metric that drifted beyond tolerance between two suite runs,
/// or a comparability violation that refuses the pairing.
#[derive(Debug)]
pub struct Drift {
    pub scenario: String,
    /// `p50`, `p95`, `throughput`, or a `fault:*` comparability violation.
    pub metric: &'static str,
    pub a: f64,
    pub b: f64,
    /// `b/a` for ratio metrics; NaN for comparability faults and a
    /// scenario missing in `b`.
    pub ratio: f64,
}

/// Reproducibility rule: a metric drifts when
/// `|b − a| > max(tol · a, floor)` — where `tol` is `tol` for the
/// median and `tol · tail_factor` for p95. Two realities drive the
/// shape: tails at n≤100 are dominated by scheduling noise (a p95 can
/// legitimately move 3× between identical runs), while a true
/// regression — wrong path, lost pacing — moves it by an order of
/// magnitude. The absolute floor covers sub-10 ms timer noise.
/// Latencies are compared in ms, throughput in MiB/s.
///
/// Comparison is refused rather than silent when a scenario is absent,
/// failed, recorded under a different backend/impairment profile, has
/// fewer than `min_samples` per side, or carries absent/nonfinite
/// metrics. The impairment seed is not part of the profile: the same
/// conditions under a different drop schedule are exactly what a
/// reproducibility gate exists to exercise.
pub struct CompareOpts {
    /// Relative tolerance for the median, e.g. 0.15 = ±15%.
    pub tol: f64,
    /// Extra multiplier applied to `tol` for tail percentiles.
    /// Default 3.0 reflects tail CI width at typical sample counts.
    pub tail_factor: f64,
    /// Absolute latency floor in ms (default 10: scheduler noise).
    pub latency_floor_ms: f64,
    /// Absolute throughput floor in MiB/s.
    pub throughput_floor: f64,
    /// Minimum samples per side before percentile claims count
    /// (default 3: fewer cannot support a percentile statement).
    pub min_samples: usize,
}

impl Default for CompareOpts {
    fn default() -> Self {
        Self {
            tol: 0.15,
            tail_factor: 3.0,
            latency_floor_ms: 10.0,
            throughput_floor: 3.0,
            min_samples: 3,
        }
    }
}

impl CompareOpts {
    fn drifts(&self, a: f64, b: f64, tol: f64, floor: f64) -> bool {
        (b - a).abs() > (tol * a).max(floor)
    }
}

fn fault(scenario: &str, metric: &'static str) -> Drift {
    Drift {
        scenario: scenario.into(),
        metric,
        a: 0.0,
        b: 0.0,
        ratio: f64::NAN,
    }
}

/// Same measurement conditions: impairment parameters must agree
/// exactly, except `seed`, which legitimately varies between runs of
/// the same profile.
fn same_profile(a: &BenchReport, b: &BenchReport) -> bool {
    match (&a.meta.impairment, &b.meta.impairment) {
        (None, None) => true,
        (Some(x), Some(y)) => {
            x.loss == y.loss
                && x.delay_ms == y.delay_ms
                && x.jitter_ms == y.jitter_ms
                && x.rate_mbps == y.rate_mbps
        }
        _ => false,
    }
}

fn scenario_failed(r: &BenchReport) -> bool {
    r.notes.iter().any(|n| n.starts_with("SCENARIO FAILED"))
        || matches!(r.attempts, Some((ok, total)) if ok < total)
}

/// Compare p50, p95 and throughput per scenario across two suites.
/// Every refusal or drift is returned — absence, failure, profile
/// mismatch, thin samples and nonfinite values are faults, not silence.
pub fn compare(a: &BenchSuite, b: &BenchSuite, opts: &CompareOpts) -> Vec<Drift> {
    let mut out = Vec::new();
    for ra in &a.reports {
        let Some(rb) = b
            .reports
            .iter()
            .find(|rb| rb.meta.scenario == ra.meta.scenario && rb.meta.path == ra.meta.path)
        else {
            out.push(fault(&ra.meta.scenario, "scenario"));
            continue;
        };
        if ra.meta.backend != rb.meta.backend {
            out.push(fault(&ra.meta.scenario, "fault:backend-mismatch"));
            continue;
        }
        if !same_profile(ra, rb) {
            out.push(fault(&ra.meta.scenario, "fault:impairment-mismatch"));
            continue;
        }
        if scenario_failed(ra) || scenario_failed(rb) {
            out.push(fault(&ra.meta.scenario, "fault:scenario-failed"));
            continue;
        }
        if let (Some(pa), Some(pb)) = (&ra.rtt, &rb.rtt)
            && pa.count.min(pb.count) < opts.min_samples
        {
            out.push(fault(&ra.meta.scenario, "fault:insufficient-samples"));
            continue;
        }
        for (metric, fa, fb, tol, floor) in [
            (
                "p50",
                ra.rtt.map(|p| p.p50_ns as f64 / 1e6),
                rb.rtt.map(|p| p.p50_ns as f64 / 1e6),
                opts.tol,
                opts.latency_floor_ms,
            ),
            (
                "p95",
                ra.rtt.map(|p| p.p95_ns as f64 / 1e6),
                rb.rtt.map(|p| p.p95_ns as f64 / 1e6),
                opts.tol * opts.tail_factor,
                opts.latency_floor_ms,
            ),
            (
                "throughput",
                ra.throughput_mib_s,
                rb.throughput_mib_s,
                opts.tol,
                opts.throughput_floor,
            ),
        ] {
            match (fa, fb) {
                (Some(_), None) | (None, Some(_)) => {
                    out.push(fault(&ra.meta.scenario, "fault:metric-absent"));
                }
                (Some(va), Some(vb)) => {
                    if !va.is_finite() || !vb.is_finite() {
                        out.push(fault(&ra.meta.scenario, "fault:nonfinite"));
                    } else if opts.drifts(va, vb, tol, floor) {
                        out.push(Drift {
                            scenario: ra.meta.scenario.clone(),
                            metric,
                            a: va,
                            b: vb,
                            ratio: vb / va.max(f64::EPSILON),
                        });
                    }
                }
                (None, None) => {}
            }
        }
    }
    out
}

/// Current unix timestamp without pulling in a time crate.
pub fn unix_ts() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// `git rev-parse --short HEAD`, best effort.
pub fn git_sha() -> Option<String> {
    std::process::Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percentiles_nearest_rank() {
        let samples: Vec<u64> = (1..=100).collect();
        let p = Percentiles::of(&samples).unwrap();
        assert_eq!(p.min_ns, 1);
        assert_eq!(p.p50_ns, 50);
        assert_eq!(p.p95_ns, 95);
        assert_eq!(p.p99_ns, 99);
        assert_eq!(p.max_ns, 100);
        assert!(p.mean_ns > 50.0 && p.mean_ns < 51.0);
    }

    #[test]
    fn empty_samples_give_no_stats() {
        assert!(Percentiles::of(&[]).is_none());
    }

    #[test]
    fn compare_flags_drift_and_missing() {
        let report = |name: &str, p95_ns: u64| BenchReport {
            meta: BenchMeta {
                scenario: name.into(),
                backend: "iroh".into(),
                path: "direct".into(),
                impairment: None,
                unix_ts: 0,
                git: None,
            },
            rtt: Some(Percentiles {
                count: 100,
                min_ns: 1,
                p50_ns: p95_ns / 2,
                p95_ns,
                p99_ns: p95_ns,
                max_ns: p95_ns,
                mean_ns: p95_ns as f64 / 2.0,
            }),
            throughput_mib_s: None,
            attempts: None,
            metrics: Default::default(),
            notes: vec![],
        };
        let a = BenchSuite {
            tool: "t".into(),
            unix_ts: 0,
            git: None,
            reports: vec![report("ping", 50_000_000), report("gone", 1_000_000)],
        };
        let b = BenchSuite {
            tool: "t".into(),
            unix_ts: 1,
            git: None,
            reports: vec![report("ping", 100_000_000)],
        };
        let drift = compare(&a, &b, &CompareOpts::default());
        // ping drifts on p50 (25→50ms > 15%) and p95 (50→100ms > 45%
        // tail band); `gone` is missing from b entirely.
        assert_eq!(drift.len(), 3);
        assert!(drift.iter().any(|d| d.metric == "p50" && d.ratio > 1.15));
        assert!(drift.iter().any(|d| d.metric == "p95" && d.ratio > 1.15));
        assert!(
            drift.iter().any(|d| d.ratio.is_nan()),
            "missing scenario must surface"
        );
    }

    #[test]
    fn compare_ignores_sub_floor_jitter() {
        // 8ms → 12ms p95: inside the 10ms absolute floor — scheduler
        // noise, not a regression.
        let report = |name: &str, p95_ns: u64| BenchReport {
            meta: BenchMeta {
                scenario: name.into(),
                backend: "iroh".into(),
                path: "direct".into(),
                impairment: None,
                unix_ts: 0,
                git: None,
            },
            rtt: Some(Percentiles {
                count: 50,
                min_ns: 1,
                p50_ns: p95_ns / 4,
                p95_ns,
                p99_ns: p95_ns,
                max_ns: p95_ns,
                mean_ns: p95_ns as f64 / 4.0,
            }),
            throughput_mib_s: None,
            attempts: None,
            metrics: Default::default(),
            notes: vec![],
        };
        let a = BenchSuite {
            tool: "t".into(),
            unix_ts: 0,
            git: None,
            reports: vec![report("ping", 8_000_000)],
        };
        let b = BenchSuite {
            tool: "t".into(),
            unix_ts: 1,
            git: None,
            reports: vec![report("ping", 12_000_000)],
        };
        assert!(compare(&a, &b, &CompareOpts::default()).is_empty());
    }

    fn suite_with(mut r: BenchReport) -> BenchSuite {
        r.meta.scenario = "s".into();
        BenchSuite {
            tool: "t".into(),
            unix_ts: 0,
            git: None,
            reports: vec![r],
        }
    }

    fn rtt_report(count: usize, p95_ns: u64) -> BenchReport {
        BenchReport {
            meta: BenchMeta {
                scenario: "s".into(),
                backend: "iroh".into(),
                path: "direct".into(),
                impairment: None,
                unix_ts: 0,
                git: None,
            },
            rtt: Some(Percentiles {
                count,
                min_ns: 1,
                p50_ns: p95_ns / 2,
                p95_ns,
                p99_ns: p95_ns,
                max_ns: p95_ns,
                mean_ns: p95_ns as f64 / 2.0,
            }),
            throughput_mib_s: None,
            attempts: None,
            metrics: Default::default(),
            notes: vec![],
        }
    }

    fn has_fault(drift: &[Drift], metric: &str) -> bool {
        drift.iter().any(|d| d.metric == metric)
    }

    #[test]
    fn compare_rejects_every_incomparable_profile() {
        let a = suite_with(rtt_report(50, 50_000_000));
        for mutate in [
            (|r: &mut BenchReport| r.meta.backend = "noq".into()) as fn(&mut BenchReport),
            |r| {
                r.meta.impairment = Some(Impairment {
                    loss: 0.05,
                    ..Impairment::default()
                })
            },
        ] {
            let mut b = suite_with(rtt_report(50, 50_000_000));
            mutate(&mut b.reports[0]);
            let drift = compare(&a, &b, &CompareOpts::default());
            assert!(
                has_fault(&drift, "fault:backend-mismatch")
                    || has_fault(&drift, "fault:impairment-mismatch"),
                "profile change must refuse comparison: {drift:?}"
            );
        }
        // Same impairment but a different seed is the same profile.
        let mut seeded = rtt_report(50, 50_000_000);
        seeded.meta.impairment = Some(Impairment {
            loss: 0.05,
            seed: 1,
            ..Impairment::default()
        });
        let mut reseeded = rtt_report(50, 50_000_000);
        reseeded.meta.impairment = Some(Impairment {
            loss: 0.05,
            seed: 2,
            ..Impairment::default()
        });
        assert!(
            compare(
                &suite_with(seeded),
                &suite_with(reseeded),
                &CompareOpts::default()
            )
            .is_empty(),
            "seed differs within one impairment profile: not a fault"
        );
    }

    #[test]
    fn compare_rejects_failed_thin_and_nonfinite_reports() {
        let a = suite_with(rtt_report(50, 50_000_000));

        let mut failed = suite_with(rtt_report(50, 50_000_000));
        failed.reports[0].attempts = Some((9, 50));
        assert!(has_fault(
            &compare(&a, &failed, &CompareOpts::default()),
            "fault:scenario-failed"
        ));

        let mut noted = suite_with(rtt_report(50, 50_000_000));
        noted.reports[0].notes.push("SCENARIO FAILED: x".into());
        assert!(has_fault(
            &compare(&a, &noted, &CompareOpts::default()),
            "fault:scenario-failed"
        ));

        let thin = suite_with(rtt_report(2, 50_000_000));
        assert!(has_fault(
            &compare(&a, &thin, &CompareOpts::default()),
            "fault:insufficient-samples"
        ));

        let mut nan = suite_with(rtt_report(50, 50_000_000));
        nan.reports[0].throughput_mib_s = Some(f64::NAN);
        let mut a_tp = suite_with(rtt_report(50, 50_000_000));
        a_tp.reports[0].throughput_mib_s = Some(40.0);
        assert!(has_fault(
            &compare(&a_tp, &nan, &CompareOpts::default()),
            "fault:nonfinite"
        ));
    }

    #[test]
    fn compare_rejects_metrics_present_on_one_side_only() {
        let mut ra = rtt_report(50, 50_000_000);
        ra.throughput_mib_s = Some(40.0);
        let rb = rtt_report(50, 50_000_000);
        let drift = compare(
            &suite_with(ra.clone()),
            &suite_with(rb),
            &CompareOpts::default(),
        );
        assert!(has_fault(&drift, "fault:metric-absent"));

        // Symmetric absence on both sides is not a fault.
        let clean = suite_with(rtt_report(50, 50_000_000));
        assert!(
            compare(
                &clean,
                &suite_with(rtt_report(50, 52_000_000)),
                &CompareOpts::default()
            )
            .is_empty()
        );
    }
}
