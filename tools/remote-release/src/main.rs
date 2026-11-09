//! Native release automation entry point; no network or installation operations.
#![forbid(unsafe_code)]
use clap::{Args, Parser, Subcommand};
use nddev_remote_release::{design, macos, promote, release, runtime};
use std::{fs::OpenOptions, io::Write, path::PathBuf};

#[derive(Parser)]
#[command(about = "Offline source-bound Remote packaging and release verification")]
struct Cli {
    #[arg(long, global = true, default_value = ".")]
    root: PathBuf,
    #[command(subcommand)]
    command: Action,
}
#[derive(Args)]
struct Version {
    #[arg(long)]
    version: String,
    #[arg(long)]
    tagged: bool,
}
#[derive(Subcommand)]
enum Action {
    VerifyDesignSystem,
    Check {
        #[command(flatten)]
        version: Version,
    },
    Source {
        #[command(flatten)]
        version: Version,
        #[arg(long)]
        out: PathBuf,
    },
    Evidence {
        #[command(flatten)]
        version: Version,
        #[arg(long)]
        checks: PathBuf,
        #[arg(long)]
        analyses: PathBuf,
        #[arg(long)]
        alerts: PathBuf,
        #[arg(long)]
        github_output: Option<PathBuf>,
    },
    Runtime {
        #[arg(long)]
        binary_dir: PathBuf,
        #[arg(long)]
        target: String,
        #[arg(long)]
        features: String,
        #[arg(long)]
        out: PathBuf,
    },
    Macos {
        #[arg(long)]
        binary_dir: PathBuf,
        #[arg(long)]
        build_info: PathBuf,
        #[arg(long)]
        out: PathBuf,
        #[arg(long, default_value = "-")]
        sign_identity: String,
    },
    Promote {
        #[command(flatten)]
        version: Version,
        #[arg(long)]
        incoming: PathBuf,
        #[arg(long)]
        source: PathBuf,
        #[arg(long)]
        out: PathBuf,
        #[arg(long)]
        runtime_run: u64,
    },
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cli = Cli::parse();
    let root = cli.root.canonicalize()?;
    let summary = match cli.command {
        Action::VerifyDesignSystem => serde_json::to_value(design::verify(&root)?)?,
        Action::Check { version } => {
            let (commit, toolchain) = release::contract(&root, &version.version, version.tagged)?;
            serde_json::json!({"commit":commit,"toolchain":toolchain})
        }
        Action::Source { version, out } => {
            release::source(&root, &version.version, &out, version.tagged)?;
            serde_json::json!({"source":"verified"})
        }
        Action::Evidence {
            version,
            checks,
            analyses,
            alerts,
            github_output,
        } => {
            let (commit, run) = release::evidence(
                &root,
                &version.version,
                version.tagged,
                &checks,
                &analyses,
                &alerts,
            )?;
            if let Some(output) = github_output {
                writeln!(OpenOptions::new().append(true).open(output)?, "run={run}")?;
            }
            serde_json::json!({"commit":commit,"runtime_run":run,"ci_evidence":"verified"})
        }
        Action::Runtime {
            binary_dir,
            target,
            features,
            out,
        } => {
            runtime::package(&root, &binary_dir, &target, &features, &out)?;
            serde_json::json!({"target":target,"runtime":"packaged"})
        }
        Action::Macos {
            binary_dir,
            build_info,
            out,
            sign_identity,
        } => {
            macos::package(&root, &binary_dir, &build_info, &out, &sign_identity)?;
            serde_json::json!({"macos":"packaged","notarized":false})
        }
        Action::Promote {
            version,
            incoming,
            source,
            out,
            runtime_run,
        } => {
            promote::finalize(
                &root,
                &version.version,
                &incoming,
                &source,
                &out,
                runtime_run,
                version.tagged,
            )?;
            serde_json::json!({"release":"verified"})
        }
    };
    println!("{}", serde_json::to_string(&summary)?);
    Ok(())
}
