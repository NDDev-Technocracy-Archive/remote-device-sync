//! Append-only machine-readable receipts (W0.6).
//!
//! One JSON object per line in `docs/receipts/rds-receipts.jsonl`. Each
//! receipt records the full commit SHA, dirty flag, digests of the bench
//! binary and `Cargo.lock`, the pinned toolchain channel, enabled
//! features, OS/arch, topology class, repetitions, observed failures and
//! skips, explicit budgets and content digests of the reports it cites —
//! everything needed to reproduce the evidence without private host
//! identifiers (no hostnames, addresses or user paths).
//!
//! Lines are hash-chained (`prev_hash` + `hash` over the canonical JSON)
//! so a tampered, reordered or dropped-middle line fails validation.
//! History is linked, never overwritten: the writer only appends.

use anyhow::{Context, bail};
use serde::{Deserialize, Serialize};
use std::path::Path;

pub const SCHEMA_VERSION: u16 = 1;
pub const DEFAULT_LOG: &str = "docs/receipts/rds-receipts.jsonl";
const GENESIS: &str = "genesis";
const STATUSES: [&str; 3] = ["pass", "fail", "partial"];

/// Digest of one report a receipt cites.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReportRef {
    pub path: String,
    pub blake3: String,
}

/// Everything except the line hash. Serialized as a `serde_json::Value`
/// for hashing: the default map sorts keys, so the digest input is
/// canonical regardless of struct field order.
#[derive(Debug, Clone, Serialize)]
struct ReceiptCore<'a> {
    schema_version: u16,
    kind: &'a str,
    subject: &'a str,
    status: &'a str,
    ts_unix: u64,
    commit_sha: &'a str,
    dirty: bool,
    binary_digest: &'a str,
    lockfile_digest: &'a str,
    toolchain: &'a str,
    features: &'a [String],
    os: &'a str,
    arch: &'a str,
    topology: &'a str,
    repetitions: Option<u32>,
    failures: Option<u32>,
    skips: Option<u32>,
    budgets: Option<&'a serde_json::Value>,
    reports: &'a [ReportRef],
    note: Option<&'a str>,
    prev_hash: &'a str,
}

/// Host-independent build identity for a run.
#[derive(Debug, Clone)]
pub struct Env {
    pub commit_sha: String,
    pub dirty: bool,
    pub binary_digest: String,
    pub lockfile_digest: String,
    pub toolchain: String,
    pub features: Vec<String>,
    pub os: String,
    pub arch: String,
}

/// Caller-supplied evidence fields.
#[derive(Debug, Clone, Default)]
pub struct Input {
    pub kind: String,
    pub subject: String,
    pub status: String,
    pub topology: String,
    pub repetitions: Option<u32>,
    pub failures: Option<u32>,
    pub skips: Option<u32>,
    pub budgets: Option<serde_json::Value>,
    pub reports: Vec<ReportRef>,
    pub note: Option<String>,
}

fn digest_file(path: &Path) -> anyhow::Result<String> {
    let bytes = std::fs::read(path).with_context(|| format!("digest {}", path.display()))?;
    Ok(blake3::hash(&bytes).to_hex().to_string())
}

fn git(args: &[&str]) -> Option<String> {
    std::process::Command::new("git")
        .args(args)
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
}

/// Build identity of the running bench binary plus the checkout it was
/// invoked in. `repo` is the workspace root (checkpoint.sh cds there).
pub fn capture_env(repo: &Path, features: Vec<String>) -> anyhow::Result<Env> {
    let commit_sha = git(&["-C", &repo.display().to_string(), "rev-parse", "HEAD"])
        .unwrap_or_else(|| "unknown".into());
    let dirty = git(&["-C", &repo.display().to_string(), "status", "--porcelain"])
        .map(|s| !s.is_empty())
        .unwrap_or(false);
    let binary_digest = digest_file(&std::env::current_exe()?)?;
    let lockfile_digest = digest_file(&repo.join("Cargo.lock"))?;
    let toolchain = std::fs::read_to_string(repo.join("rust-toolchain.toml"))
        .ok()
        .and_then(|t| {
            t.lines().find_map(|l| {
                l.split('=')
                    .nth(1)
                    .map(|v| v.trim().trim_matches('"').to_string())
            })
        })
        .unwrap_or_else(|| "unknown".into());
    Ok(Env {
        commit_sha,
        dirty,
        binary_digest,
        lockfile_digest,
        toolchain,
        features,
        os: std::env::consts::OS.into(),
        arch: std::env::consts::ARCH.into(),
    })
}

fn validate_fields(v: &serde_json::Value, line_no: usize) -> anyhow::Result<()> {
    let get = |k: &str| {
        v.get(k)
            .with_context(|| format!("line {line_no}: missing {k}"))
    };
    if get("schema_version")?.as_u64() != Some(SCHEMA_VERSION as u64) {
        bail!("line {line_no}: unsupported schema_version");
    }
    for k in ["kind", "subject", "status", "topology"] {
        if get(k)?.as_str().is_none_or(str::is_empty) {
            bail!("line {line_no}: {k} must be a non-empty string");
        }
    }
    if !STATUSES.contains(&v["status"].as_str().unwrap()) {
        bail!("line {line_no}: unknown status {:?}", v["status"]);
    }
    get("ts_unix")?
        .as_u64()
        .filter(|t| *t > 0)
        .with_context(|| format!("line {line_no}: ts_unix must be positive"))?;
    for k in [
        "commit_sha",
        "binary_digest",
        "lockfile_digest",
        "toolchain",
        "os",
        "arch",
    ] {
        get(k)?
            .as_str()
            .filter(|s| !s.is_empty())
            .with_context(|| format!("line {line_no}: {k} must be a non-empty string"))?;
    }
    for k in ["binary_digest", "lockfile_digest"] {
        let d = v[k].as_str().unwrap();
        if d.len() != 64 || !d.chars().all(|c| c.is_ascii_hexdigit()) {
            bail!("line {line_no}: {k} is not a blake3 hex digest");
        }
    }
    get("dirty")?
        .as_bool()
        .with_context(|| format!("line {line_no}: dirty must be bool"))?;
    get("features")?
        .as_array()
        .with_context(|| format!("line {line_no}: features must be an array"))?;
    get("reports")?
        .as_array()
        .with_context(|| format!("line {line_no}: reports must be an array"))?;
    if let Some(b) = v.get("budgets")
        && !b.is_null()
        && !b.is_object()
    {
        bail!("line {line_no}: budgets must be a JSON object");
    }
    let hash = get("hash")?.as_str().unwrap_or("");
    if hash.len() != 64 || !hash.chars().all(|c| c.is_ascii_hexdigit()) {
        bail!("line {line_no}: hash is not a blake3 hex digest");
    }
    Ok(())
}

fn hash_core(core: &ReceiptCore<'_>) -> anyhow::Result<String> {
    let v = serde_json::to_value(core)?;
    Ok(blake3::hash(v.to_string().as_bytes()).to_hex().to_string())
}

/// Append one receipt. Returns its line hash.
pub fn append(log: &Path, env: &Env, input: Input) -> anyhow::Result<String> {
    if input.kind.is_empty() || input.subject.is_empty() {
        bail!("receipt needs kind and subject");
    }
    if !STATUSES.contains(&input.status.as_str()) {
        bail!("status must be one of {STATUSES:?}");
    }
    if let Some(b) = &input.budgets
        && !b.is_object()
    {
        bail!("budgets must be a JSON object");
    }

    let prev_hash = last_hash(log)?;
    let core = ReceiptCore {
        schema_version: SCHEMA_VERSION,
        kind: &input.kind,
        subject: &input.subject,
        status: &input.status,
        ts_unix: crate::report::unix_ts(),
        commit_sha: &env.commit_sha,
        dirty: env.dirty,
        binary_digest: &env.binary_digest,
        lockfile_digest: &env.lockfile_digest,
        toolchain: &env.toolchain,
        features: &env.features,
        os: &env.os,
        arch: &env.arch,
        topology: &input.topology,
        repetitions: input.repetitions,
        failures: input.failures,
        skips: input.skips,
        budgets: input.budgets.as_ref(),
        reports: &input.reports,
        note: input.note.as_deref(),
        prev_hash: &prev_hash,
    };
    let hash = hash_core(&core)?;
    let mut v = serde_json::to_value(&core)?;
    v["hash"] = serde_json::Value::String(hash.clone());
    if let Some(parent) = log.parent() {
        std::fs::create_dir_all(parent)?;
    }
    use std::io::Write as _;
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log)?;
    writeln!(f, "{}", serde_json::to_string(&v)?)?;
    Ok(hash)
}

/// Digest of a report file to cite in a receipt.
pub fn report_ref(path: &Path) -> anyhow::Result<ReportRef> {
    Ok(ReportRef {
        path: path.to_string_lossy().into(),
        blake3: digest_file(path)?,
    })
}

/// Last line's `hash`, or GENESIS for an absent/empty log.
fn last_hash(log: &Path) -> anyhow::Result<String> {
    let text = match std::fs::read_to_string(log) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(GENESIS.into()),
        Err(e) => return Err(e).with_context(|| format!("read {}", log.display())),
    };
    let Some(last) = text.lines().rfind(|l| !l.trim().is_empty()) else {
        return Ok(GENESIS.into());
    };
    let v: serde_json::Value = serde_json::from_str(last)
        .with_context(|| format!("{}: last line is not JSON", log.display()))?;
    v.get("hash")
        .and_then(|h| h.as_str())
        .map(str::to_string)
        .with_context(|| format!("{}: last line has no hash", log.display()))
}

/// Validate the whole log: schema, required fields, and the hash chain.
/// Returns the number of receipts validated.
pub fn validate(log: &Path) -> anyhow::Result<usize> {
    let text = std::fs::read_to_string(log).with_context(|| format!("read {}", log.display()))?;
    let mut prev = GENESIS.to_string();
    let mut n = 0usize;
    for (i, line) in text.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let line_no = i + 1;
        let v: serde_json::Value =
            serde_json::from_str(line).with_context(|| format!("line {line_no}: invalid JSON"))?;
        validate_fields(&v, line_no)?;
        if v["prev_hash"].as_str() != Some(prev.as_str()) {
            bail!("line {line_no}: prev_hash does not match previous line");
        }
        let mut unsigned = v.clone();
        unsigned.as_object_mut().unwrap().remove("hash");
        let want = blake3::hash(unsigned.to_string().as_bytes())
            .to_hex()
            .to_string();
        if v["hash"].as_str() != Some(want.as_str()) {
            bail!("line {line_no}: hash mismatch (tampered or corrupted)");
        }
        prev = v["hash"].as_str().unwrap().to_string();
        n += 1;
    }
    Ok(n)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn tmp_log(tag: &str) -> PathBuf {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        std::env::temp_dir().join(format!(
            "rds-receipt-test-{}-{tag}-{}.jsonl",
            std::process::id(),
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ))
    }

    fn env() -> Env {
        Env {
            commit_sha: "a".repeat(40),
            dirty: false,
            binary_digest: blake3::hash(b"bench").to_hex().to_string(),
            lockfile_digest: blake3::hash(b"lock").to_hex().to_string(),
            toolchain: "1.98.1".into(),
            features: vec![],
            os: "linux".into(),
            arch: "x86_64".into(),
        }
    }

    fn input(subject: &str) -> Input {
        Input {
            kind: "gate".into(),
            subject: subject.into(),
            status: "pass".into(),
            topology: "loopback".into(),
            ..Default::default()
        }
    }

    #[test]
    fn append_then_validate_roundtrip() {
        let log = tmp_log("ok");
        let e = env();
        let h1 = append(&log, &e, input("c0")).unwrap();
        let h2 = append(&log, &e, input("c1")).unwrap();
        assert_ne!(h1, h2, "hash chain must link distinct receipts");
        assert_eq!(validate(&log).unwrap(), 2);
        std::fs::remove_file(&log).ok();
    }

    #[test]
    fn tampered_line_is_rejected() {
        let log = tmp_log("tamper");
        let e = env();
        append(&log, &e, input("c0")).unwrap();
        append(&log, &e, input("c1")).unwrap();
        let text = std::fs::read_to_string(&log).unwrap();
        let tampered = text.replacen("\"pass\"", "\"fail\"", 1);
        std::fs::write(&log, tampered).unwrap();
        let err = validate(&log).unwrap_err().to_string();
        assert!(err.contains("line 1"), "tamper must name the line: {err}");
        std::fs::remove_file(&log).ok();
    }

    #[test]
    fn reordered_lines_break_the_chain() {
        let log = tmp_log("reorder");
        let e = env();
        append(&log, &e, input("c0")).unwrap();
        append(&log, &e, input("c1")).unwrap();
        let text = std::fs::read_to_string(&log).unwrap();
        let mut lines: Vec<&str> = text.lines().collect();
        lines.swap(0, 1);
        std::fs::write(&log, lines.join("\n") + "\n").unwrap();
        assert!(validate(&log).is_err());
        std::fs::remove_file(&log).ok();
    }

    #[test]
    fn bad_status_and_budgets_refused() {
        let log = tmp_log("bad");
        let e = env();
        let mut bad = input("c0");
        bad.status = "green".into();
        assert!(append(&log, &e, bad).is_err());
        let mut bad = input("c0");
        bad.budgets = Some(serde_json::json!(["not", "an", "object"]));
        assert!(append(&log, &e, bad).is_err());
        std::fs::remove_file(&log).ok();
    }

    #[test]
    fn genesis_log_validates_empty() {
        let log = tmp_log("empty");
        std::fs::write(&log, "").unwrap();
        assert_eq!(validate(&log).unwrap(), 0);
        std::fs::remove_file(&log).ok();
    }
}
