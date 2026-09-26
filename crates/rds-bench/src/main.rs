//! `rds-bench` — run transport scenarios, emit JSON + markdown reports,
//! or compare two suite runs for the reproducibility gate.

use std::path::PathBuf;

use anyhow::Context;
use clap::{Parser, Subcommand};
use rds_bench::impair::Impairment;
use rds_bench::report::{BenchSuite, CompareOpts, compare, unix_ts};

fn compare_opts(
    tol: f64,
    latency_floor_ms: f64,
    throughput_floor: f64,
    min_samples: usize,
) -> CompareOpts {
    CompareOpts {
        tol,
        latency_floor_ms,
        throughput_floor,
        min_samples,
        ..CompareOpts::default()
    }
}
use rds_bench::scenario::{Params, Scenario};

#[derive(Parser)]
#[command(name = "rds-bench", about = "rds transport benchmark harness")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Qualify a full synthetic directory: durable renewals, deletion and reopen.
    DirectoryCapacity {
        /// New private directory; existing paths are refused and never removed.
        #[arg(long)]
        state_dir: PathBuf,
        /// Even number of full shrink/expand renewal passes, 2..=64.
        #[arg(long, default_value_t = 2)]
        rounds: u32,
        #[arg(long)]
        json: Option<PathBuf>,
        #[arg(long)]
        md: Option<PathBuf>,
    },
    /// Run a scenario (or `all`) and write a report.
    Run {
        #[arg(long, value_enum)]
        scenario: Scenario,
        /// Iterations/probes per latency scenario.
        #[arg(long, default_value_t = 100)]
        iterations: usize,
        /// MiB moved by the `transfer` scenario.
        #[arg(long, default_value_t = 32)]
        transfer_mib: u64,
        /// Per-attempt timeout, seconds.
        #[arg(long, default_value_t = 15)]
        timeout_s: u64,
        /// Datagram loss probability for impaired scenarios (0..1).
        #[arg(long, default_value_t = 0.05)]
        loss: f64,
        /// Fixed one-way delay for impaired scenarios, ms.
        #[arg(long, default_value_t = 50)]
        delay_ms: u64,
        /// Uniform jitter for impaired scenarios, ms.
        #[arg(long, default_value_t = 30)]
        jitter_ms: u64,
        /// Rate cap for impaired scenarios, Mbps.
        #[arg(long)]
        rate_mbps: Option<f64>,
        /// Impairment PRNG seed.
        #[arg(long, default_value_t = 1)]
        seed: u64,
        /// Backend label recorded in the report.
        #[arg(long, default_value = "iroh")]
        backend: String,
        /// Write the suite JSON here.
        #[arg(long)]
        json: Option<PathBuf>,
        /// Write a markdown report here.
        #[arg(long)]
        md: Option<PathBuf>,
    },
    /// Compare two suite JSONs; exit 1 on drift or a comparability fault.
    Compare {
        a: PathBuf,
        b: PathBuf,
        /// Relative tolerance, e.g. 0.15 = ±15%.
        #[arg(long, default_value_t = 0.15)]
        tol: f64,
        /// Absolute latency floor in ms — sub-floor moves are noise.
        #[arg(long, default_value_t = 10.0)]
        latency_floor_ms: f64,
        /// Absolute throughput floor in MiB/s.
        #[arg(long, default_value_t = 3.0)]
        throughput_floor: f64,
        /// Minimum samples per side before percentile claims count.
        #[arg(long, default_value_t = 3)]
        min_samples: usize,
    },
}

#[tokio::main(flavor = "multi_thread", worker_threads = 4)]
async fn main() -> std::process::ExitCode {
    let cli = Cli::parse();
    rds_observe::run_main(rds_observe::Service::Bench, "warn", run(cli)).await
}

async fn run(cli: Cli) -> anyhow::Result<()> {
    match cli.cmd {
        Cmd::DirectoryCapacity {
            state_dir,
            rounds,
            json,
            md,
        } => {
            let reports =
                tokio::task::spawn_blocking(move || rds_bench::capacity::run(&state_dir, rounds))
                    .await??;
            let suite = BenchSuite {
                tool: concat!("rds-bench ", env!("CARGO_PKG_VERSION")).into(),
                unix_ts: unix_ts(),
                git: rds_bench::report::git_sha(),
                reports,
            };
            println!("{}", suite.to_markdown());
            if let Some(p) = json {
                std::fs::write(&p, serde_json::to_string_pretty(&suite)?)
                    .with_context(|| format!("write {p:?}"))?;
            }
            if let Some(p) = md {
                std::fs::write(&p, suite.to_markdown()).with_context(|| format!("write {p:?}"))?;
            }
            Ok(())
        }
        Cmd::Run {
            scenario,
            iterations,
            transfer_mib,
            timeout_s,
            loss,
            delay_ms,
            jitter_ms,
            rate_mbps,
            seed,
            backend,
            json,
            md,
        } => {
            let params = Params {
                iterations,
                transfer_mib,
                timeout: std::time::Duration::from_secs(timeout_s),
                impairment: Impairment {
                    loss,
                    delay_ms,
                    jitter_ms,
                    rate_mbps,
                    seed,
                },
                backend,
            };
            let reports = rds_bench::scenario::run(scenario, &params).await?;
            let suite = BenchSuite {
                tool: concat!("rds-bench ", env!("CARGO_PKG_VERSION")).into(),
                unix_ts: unix_ts(),
                git: rds_bench::report::git_sha(),
                reports,
            };
            println!("{}", suite.to_markdown());
            if let Some(p) = json {
                std::fs::write(&p, serde_json::to_string_pretty(&suite)?)
                    .with_context(|| format!("write {p:?}"))?;
                eprintln!("wrote {p:?}");
            }
            if let Some(p) = md {
                std::fs::write(&p, suite.to_markdown()).with_context(|| format!("write {p:?}"))?;
                eprintln!("wrote {p:?}");
            }
            let failed = suite
                .reports
                .iter()
                .filter(|r| r.notes.iter().any(|n| n.starts_with("SCENARIO FAILED")))
                .count();
            if failed > 0 {
                anyhow::bail!("{failed} scenario(s) failed — see report");
            }
            Ok(())
        }
        Cmd::Compare {
            a,
            b,
            tol,
            latency_floor_ms,
            throughput_floor,
            min_samples,
        } => {
            let load = |p: &PathBuf| -> anyhow::Result<BenchSuite> {
                let text = std::fs::read_to_string(p).with_context(|| format!("read {p:?}"))?;
                serde_json::from_str(&text).with_context(|| format!("parse {p:?}"))
            };
            let a = load(&a)?;
            let b = load(&b)?;
            let opts = compare_opts(tol, latency_floor_ms, throughput_floor, min_samples);
            let drift = compare(&a, &b, &opts);
            if drift.is_empty() {
                println!(
                    "suites comparable within ±{:.0}% (floor {:.0}ms/{:.0}MiB/s)",
                    opts.tol * 100.0,
                    opts.latency_floor_ms,
                    opts.throughput_floor
                );
                return Ok(());
            }
            for d in &drift {
                if d.metric == "scenario" {
                    println!("MISSING scenario in second suite: {}", d.scenario);
                } else if d.ratio.is_nan() {
                    println!("FAULT {}: {}", d.scenario, d.metric);
                } else {
                    println!(
                        "DRIFT {} {}: {:.2} → {:.2} (×{:.2})",
                        d.scenario, d.metric, d.a, d.b, d.ratio
                    );
                }
            }
            anyhow::bail!("{} drift(s)/fault(s)", drift.len())
        }
    }
}
