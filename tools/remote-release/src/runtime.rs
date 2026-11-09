//! Stream already built native executables into their reproducible package.
use crate::{
    BINARIES, LINUX, MACOS, Result, archive, command, design, digest, encoded, hex_identity, json,
    new_directory, notice_hashes, read, regular, relative, require, write_new,
};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, fs, path::Path};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Build {
    pub design_system: design::Receipt,
    pub schema_version: u32,
    pub source: String,
    pub version: String,
    pub target: String,
    pub features: Vec<String>,
    pub rustc: String,
    pub binaries: BTreeMap<String, String>,
    pub notices: BTreeMap<String, String>,
    pub build: BTreeMap<String, String>,
}

pub fn features(target: &str) -> Result<Vec<String>> {
    require(
        [LINUX, MACOS].contains(&target),
        "unsupported native target",
    )?;
    let mut values = vec![
        "rds-cli/desktop",
        "rds-cli/transport-noq",
        "rds-agent/transport-noq",
        if target == LINUX {
            "rds-agent/portal"
        } else {
            "rds-agent/desktop"
        },
    ]
    .into_iter()
    .map(str::to_owned)
    .collect::<Vec<_>>();
    values.sort();
    Ok(values)
}

pub fn package(
    root: &Path,
    binaries: &Path,
    target: &str,
    features: &str,
    out: &Path,
) -> Result<()> {
    let host = match (std::env::consts::OS, std::env::consts::ARCH) {
        ("linux", "x86_64") => LINUX,
        ("macos", "aarch64") => MACOS,
        _ => "unsupported",
    };
    require(target == host, "package on the selected native executor")?;
    let source = command(root, "git", &["rev-parse", "HEAD"])?;
    require(hex_identity(&source, 20), "unsupported source identity")?;
    require(
        command(
            root,
            "git",
            &["status", "--porcelain", "--untracked-files=no"],
        )?
        .is_empty(),
        "tracked source changed during build",
    )?;
    let mut selected = features.split(',').map(str::to_owned).collect::<Vec<_>>();
    selected.sort();
    selected.dedup();
    require(
        !selected.is_empty()
            && selected.iter().all(|f| {
                f.split_once('/').is_some_and(|(a, b)| {
                    !a.is_empty()
                        && !b.is_empty()
                        && [a, b].iter().all(|p| {
                            p.bytes().all(|v| {
                                v.is_ascii_lowercase() || v.is_ascii_digit() || b"_-".contains(&v)
                            })
                        })
                })
            }),
        "invalid selected feature list",
    )?;
    require(
        selected.iter().any(|f| f == "rds-cli/desktop")
            && selected
                .iter()
                .any(|f| f == "rds-agent/desktop" || f == "rds-agent/portal"),
        "desktop runtime requires viewer and agent features",
    )?;
    let mut files = BTreeMap::new();
    let mut hashes = BTreeMap::new();
    for name in BINARIES {
        let path = binaries.join(name);
        let metadata = regular(&path, 256 * 1024 * 1024)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            require(
                metadata.permissions().mode() & 0o111 != 0,
                "native input is not executable",
            )?;
        }
        require(metadata.len() > 0, "empty native executable")?;
        hashes.insert(name.to_owned(), digest(&path)?);
        files.insert(
            name.to_owned(),
            archive::Input {
                path,
                executable: true,
            },
        );
    }
    let notices: BTreeMap<String, String> = json(&root.join("packaging/notices.json"))?;
    let notice_digests = notice_hashes(root)?;
    for (name, source) in notices {
        require(
            relative(&name)
                && relative(&source)
                && !files.contains_key(&name)
                && name != "build-info.json",
            "invalid package notice",
        )?;
        files.insert(
            name,
            archive::Input {
                path: root.join(source),
                executable: false,
            },
        );
    }
    let build = Build {
        design_system: design::verify(root)?,
        schema_version: 1,
        source: source.clone(),
        version: String::from_utf8(read(&root.join("VERSION"), 128)?)
            .map_err(|_| crate::Error::Refused("invalid version encoding"))?
            .trim()
            .to_owned(),
        target: target.to_owned(),
        features: selected,
        rustc: command(root, "rustc", &["--version", "--verbose"])?,
        binaries: hashes,
        notices: notice_digests,
        build: [
            "GITHUB_REPOSITORY",
            "GITHUB_WORKFLOW",
            "GITHUB_RUN_ID",
            "GITHUB_RUN_ATTEMPT",
            "GITHUB_REF",
        ]
        .into_iter()
        .filter_map(|key| std::env::var(key).ok().map(|value| (key.to_owned(), value)))
        .collect(),
    };
    let metadata = encoded(&build)?;
    require(metadata.len() <= 128 * 1024, "oversized runtime metadata")?;
    new_directory(out)?;
    write_new(&out.join("build-info.json"), &metadata)?;
    files.insert(
        "build-info.json".into(),
        archive::Input {
            path: out.join("build-info.json"),
            executable: false,
        },
    );
    let name = format!("nddev-remote-{target}-{source}.tar.gz");
    let path = out.join(&name);
    archive::bundle(&path, &files)?;
    write_new(
        &out.join("SHA256SUMS"),
        format!("{}  {name}\n", digest(&path)?).as_bytes(),
    )?;
    require(fs::metadata(path)?.len() > 0, "empty native package")
}
