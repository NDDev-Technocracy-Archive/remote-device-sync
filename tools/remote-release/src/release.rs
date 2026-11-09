//! Exact version/source/tag contract and complete, paginated CI evidence.
use crate::{Error, Result, archive, command, hex_identity, new_directory, read, require};
use serde_json::Value;
use std::{fs, path::Path};

pub fn numeric_version(value: &str) -> bool {
    let parts: Vec<_> = value.split('.').collect();
    parts.len() == 3
        && parts.iter().all(|p| {
            !p.is_empty()
                && p.bytes().all(|b| b.is_ascii_digit())
                && (p.len() == 1 || !p.starts_with('0'))
        })
}

pub fn notes(root: &Path, version: &str) -> Result<String> {
    let content = String::from_utf8(read(&root.join("CHANGELOG.md"), 1024 * 1024)?)
        .map_err(|_| Error::Refused("invalid changelog encoding"))?;
    let mut selected = Vec::new();
    let mut active = false;
    let mut count = 0;
    for line in content.lines() {
        if let Some(rest) = line.strip_prefix("## [") {
            let (heading, _) = rest
                .split_once(']')
                .ok_or(Error::Refused("invalid release heading"))?;
            active = heading == version;
            if active {
                count += 1;
            }
        } else if active {
            selected.push(line);
        }
    }
    let notes = selected.join("\n").trim().to_owned();
    require(
        count == 1
            && !notes.is_empty()
            && notes.to_ascii_lowercase().contains("engineering preview"),
        "expected one engineering preview release heading",
    )?;
    Ok(notes + "\n")
}

fn toml(root: &Path, path: &str) -> Result<toml::Value> {
    Ok(toml::from_str(
        &String::from_utf8(read(&root.join(path), 8 * 1024 * 1024)?)
            .map_err(|_| Error::Refused("invalid TOML encoding"))?,
    )?)
}

pub fn contract(root: &Path, version: &str, tagged: bool) -> Result<(String, String)> {
    contract_with_ref(
        root,
        version,
        tagged,
        std::env::var("GITHUB_REF").ok().as_deref(),
    )
}

pub fn contract_with_ref(
    root: &Path,
    version: &str,
    tagged: bool,
    reference: Option<&str>,
) -> Result<(String, String)> {
    require(numeric_version(version), "expected numeric SemVer")?;
    require(
        read(&root.join("VERSION"), 128)? == format!("{version}\n").as_bytes(),
        "VERSION mismatch",
    )?;
    let manifest = toml(root, "Cargo.toml")?;
    let workspace = manifest
        .get("workspace")
        .ok_or(Error::Refused("missing workspace"))?;
    require(
        workspace
            .get("package")
            .and_then(|p| p.get("version"))
            .and_then(toml::Value::as_str)
            == Some(version),
        "Cargo version mismatch",
    )?;
    let lock = toml(root, "Cargo.lock")?;
    let packages = lock
        .get("package")
        .and_then(toml::Value::as_array)
        .ok_or(Error::Refused("missing lock packages"))?;
    for member in workspace
        .get("members")
        .and_then(toml::Value::as_array)
        .ok_or(Error::Refused("missing workspace members"))?
    {
        let member = member
            .as_str()
            .filter(|v| crate::relative(v))
            .ok_or(Error::Refused("invalid workspace member"))?;
        let definition = toml(root, &format!("{member}/Cargo.toml"))?;
        let name = definition
            .get("package")
            .and_then(|p| p.get("name"))
            .and_then(toml::Value::as_str)
            .ok_or(Error::Refused("missing member name"))?;
        let matches: Vec<_> = packages
            .iter()
            .filter(|p| {
                p.get("name").and_then(toml::Value::as_str) == Some(name)
                    && p.get("source").is_none()
            })
            .collect();
        require(
            matches.len() == 1
                && matches[0].get("version").and_then(toml::Value::as_str) == Some(version),
            "workspace lock version mismatch",
        )?;
    }
    let toolchain = toml(root, "rust-toolchain.toml")?;
    let channel = toolchain
        .get("toolchain")
        .and_then(|t| t.get("channel"))
        .and_then(toml::Value::as_str)
        .ok_or(Error::Refused("missing Rust toolchain"))?;
    require(numeric_version(channel), "Rust toolchain must be pinned")?;
    notes(root, version)?;
    let commit = command(root, "git", &["rev-parse", "HEAD"])?;
    require(hex_identity(&commit, 20), "invalid source OID")?;
    require(
        command(
            root,
            "git",
            &["status", "--porcelain", "--untracked-files=no"],
        )?
        .is_empty(),
        "dirty tracked source",
    )?;
    if tagged {
        require(
            reference == Some(&format!("refs/tags/{version}")),
            "release must run from tag ref",
        )?;
        require(
            command(
                root,
                "git",
                &["cat-file", "-t", &format!("refs/tags/{version}")],
            )? == "tag",
            "annotated tag required",
        )?;
        require(
            command(
                root,
                "git",
                &["rev-parse", &format!("refs/tags/{version}^{{commit}}")],
            )? == commit,
            "tag/source mismatch",
        )?;
    }
    Ok((commit, channel.to_owned()))
}

pub fn verify_ci(
    commit: &str,
    runs: &[Value],
    analyses: &[Value],
    alerts: &[Value],
) -> Result<u64> {
    let mut producer = None;
    for workflow in [
        "ci.yml",
        "supply-chain.yml",
        "security.yml",
        "desktop-runtime.yml",
    ] {
        let selected: Vec<_> = runs
            .iter()
            .filter(|r| {
                r["path"] == format!(".github/workflows/{workflow}")
                    && r["head_sha"] == commit
                    && r["event"] == "push"
                    && r["head_branch"] == "main"
            })
            .collect();
        require(
            selected.len() == 1
                && selected[0]["status"] == "completed"
                && selected[0]["conclusion"] == "success",
            "missing/ambiguous/unsuccessful exact-commit workflow",
        )?;
        if workflow == "desktop-runtime.yml" {
            producer = selected[0]["id"].as_u64();
        }
    }
    for language in ["rust", "actions"] {
        let selected: Vec<_> = analyses
            .iter()
            .filter(|a| {
                a["ref"] == "refs/heads/main"
                    && a["category"] == format!("/language:{language}")
                    && a["analysis_key"] == ".github/workflows/security.yml:codeql"
                    && a["tool"]["name"] == "CodeQL"
            })
            .collect();
        require(
            selected
                .iter()
                .filter(|a| a["commit_sha"] == commit)
                .count()
                == 1
                && selected[0]["commit_sha"] == commit
                && selected[0]["error"] == ""
                && selected[0]["warning"] == "",
            "missing/ambiguous/incomplete exact-commit CodeQL analysis",
        )?;
    }
    require(alerts.is_empty(), "unresolved code-scanning alerts on main")?;
    producer
        .filter(|id| *id > 0)
        .ok_or(Error::Refused("missing native producer run ID"))
}

pub fn evidence(
    root: &Path,
    version: &str,
    tagged: bool,
    checks: &Path,
    analyses: &Path,
    alerts: &Path,
) -> Result<(String, u64)> {
    let (commit, _) = contract(root, version, tagged)?;
    let pages: Vec<Value> = crate::json_limit(checks, 32 * 1024 * 1024)?;
    let mut runs = Vec::new();
    for page in pages {
        runs.extend(
            page.get("workflow_runs")
                .and_then(Value::as_array)
                .ok_or(Error::Refused("invalid CI run page"))?
                .iter()
                .cloned(),
        );
    }
    let analyses: Vec<Vec<Value>> = crate::json_limit(analyses, 32 * 1024 * 1024)?;
    let alerts: Vec<Vec<Value>> = crate::json_limit(alerts, 32 * 1024 * 1024)?;
    let producer = verify_ci(
        &commit,
        &runs,
        &analyses.into_iter().flatten().collect::<Vec<_>>(),
        &alerts.into_iter().flatten().collect::<Vec<_>>(),
    )?;
    Ok((commit, producer))
}

pub fn source(root: &Path, version: &str, out: &Path, tagged: bool) -> Result<()> {
    contract(root, version, tagged)?;
    new_directory(out)?;
    let path = out.join(format!("nddev-remote-{version}-source.tar.gz"));
    archive::source(root, version, &path)?;
    archive::extract_source(&path, &out.join("source"))?;
    require(
        fs::read_dir(out.join("source"))?.next().is_some(),
        "empty source archive",
    )
}
