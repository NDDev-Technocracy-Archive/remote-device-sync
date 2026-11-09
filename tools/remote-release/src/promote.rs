//! Verify checked producer bytes before copying them into a publication directory.
use crate::{
    BINARIES, Error, LINUX, MACOS, REPOSITORY, Result,
    archive::{self, Member},
    design, digest, encoded, inventory, json as read_json, macos, new_directory, notice_hashes,
    read, release, require,
    runtime::{self, Build},
    write_new,
};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::Path,
};

fn checksum(directory: &Path, sums: &str, name: &str) -> Result<()> {
    require(
        read(&directory.join(sums), 1024)?
            == format!("{}  {name}\n", digest(&directory.join(name))?).as_bytes(),
        "desktop archive checksum mismatch",
    )
}
fn member<'a>(files: &'a BTreeMap<String, Member>, name: &str) -> Result<&'a Member> {
    files
        .get(name)
        .ok_or(Error::Refused("missing package member"))
}
fn metadata<T: serde::de::DeserializeOwned>(
    files: &BTreeMap<String, Member>,
    name: &str,
) -> Result<T> {
    let file = member(files, name)?;
    require(file.mode == 0o644, "package metadata mode mismatch")?;
    Ok(serde_json::from_slice(
        file.metadata
            .as_deref()
            .ok_or(Error::Refused("missing package metadata"))?,
    )?)
}

pub fn verify_runtime(
    root: &Path,
    directory: &Path,
    target: &str,
    version: &str,
    commit: &str,
    channel: &str,
    run: u64,
) -> Result<(String, Build)> {
    let name = format!("nddev-remote-{target}-{commit}.tar.gz");
    let mut expected =
        BTreeSet::from([name.clone(), "build-info.json".into(), "SHA256SUMS".into()]);
    if target == MACOS {
        expected.extend([
            format!("nddev-remote-macos-apps-{commit}.tar.gz"),
            "apps-build-info.json".into(),
            "apps-SHA256SUMS".into(),
        ]);
    }
    inventory(directory, &expected)?;
    checksum(directory, "SHA256SUMS", &name)?;
    let info: Build = read_json(&directory.join("build-info.json"))?;
    require(
        info.schema_version == 1
            && info.source == commit
            && info.version == version
            && info.target == target
            && info.features == runtime::features(target)?,
        "desktop build identity mismatch",
    )?;
    require(
        info.design_system == design::verify(root)?,
        "desktop design pin differs from source",
    )?;
    require(
        info.binaries.keys().map(String::as_str).eq(BINARIES),
        "desktop binary inventory mismatch",
    )?;
    require(
        info.notices == notice_hashes(root)?,
        "desktop notice inventory mismatch",
    )?;
    require(
        info.rustc.starts_with(&format!("rustc {channel} "))
            && info.rustc.contains(&format!("\nhost: {target}\n")),
        "desktop native compiler mismatch",
    )?;
    for (key, value) in [
        ("GITHUB_REPOSITORY", REPOSITORY.to_owned()),
        ("GITHUB_WORKFLOW", "desktop-runtime".into()),
        ("GITHUB_REF", "refs/heads/main".into()),
        ("GITHUB_RUN_ID", run.to_string()),
    ] {
        require(
            info.build.get(key) == Some(&value),
            "desktop producer mismatch",
        )?;
    }
    let files = archive::inspect(&directory.join(&name))?;
    let mut expected: BTreeSet<String> = BINARIES.into_iter().map(str::to_owned).collect();
    expected.extend(info.notices.keys().cloned());
    expected.insert("build-info.json".into());
    require(
        files.keys().cloned().collect::<BTreeSet<_>>() == expected,
        "desktop archive member mismatch",
    )?;
    require(
        metadata::<Build>(&files, "build-info.json")? == info,
        "desktop metadata copies differ",
    )?;
    for (name, digest) in &info.binaries {
        let file = member(&files, name)?;
        require(
            file.mode == 0o755 && file.digest == *digest,
            "desktop executable mode or digest mismatch",
        )?;
    }
    for (name, digest) in &info.notices {
        let file = member(&files, name)?;
        require(
            file.mode == 0o644 && file.digest == *digest,
            "desktop notice mode or digest mismatch",
        )?;
    }
    Ok((name, info))
}

pub fn verify_apps(
    root: &Path,
    directory: &Path,
    runtime: &Build,
    commit: &str,
    version: &str,
) -> Result<String> {
    let name = format!("nddev-remote-macos-apps-{commit}.tar.gz");
    checksum(directory, "apps-SHA256SUMS", &name)?;
    let settings = macos::settings(root)?;
    let resources = macos::resources(root, &settings)?;
    let info: macos::Receipt = read_json(&directory.join("apps-build-info.json"))?;
    require(
        info.schema_version == 1
            && &info.runtime == runtime
            && info.signing == "ad-hoc"
            && !info.notarized
            && info.builder_sha256 == digest(&root.join("tools/remote-release/src/macos.rs"))?
            && info.settings_sha256 == digest(&root.join("packaging/macos.json"))?,
        "macOS application identity mismatch",
    )?;
    let mut expected = BTreeSet::new();
    let mut applications = BTreeMap::new();
    for app in &settings.applications {
        let prefix = format!("{}.app/Contents/", app.name);
        expected.extend([
            format!("{prefix}Info.plist"),
            format!("{prefix}Resources/build-info.json"),
            format!("{prefix}_CodeSignature/CodeResources"),
            format!("{prefix}MacOS/{}", app.executable),
        ]);
        expected.extend(
            resources
                .keys()
                .map(|name| format!("{prefix}Resources/{name}")),
        );
        applications.insert(&app.name, app);
    }
    require(
        info.files.keys().cloned().collect::<BTreeSet<_>>() == expected,
        "macOS application file inventory mismatch",
    )?;
    require(
        info.applications.len() == applications.len()
            && info
                .applications
                .iter()
                .map(|r| &r.name)
                .collect::<BTreeSet<_>>()
                == applications.keys().copied().collect(),
        "macOS application inventory mismatch",
    )?;
    expected.insert("apps-build-info.json".into());
    let files = archive::inspect(&directory.join(&name))?;
    require(
        files.keys().cloned().collect::<BTreeSet<_>>() == expected,
        "macOS archive member mismatch",
    )?;
    require(
        metadata::<macos::Receipt>(&files, "apps-build-info.json")? == info,
        "macOS metadata copies differ",
    )?;
    for (name, selected) in &info.files {
        require(
            member(&files, name)?.digest == *selected,
            "macOS application file digest mismatch",
        )?;
    }
    for record in &info.applications {
        let app = applications
            .get(&record.name)
            .ok_or(Error::Refused("unknown macOS app"))?;
        let prefix = format!("{}.app/Contents/", app.name);
        let executable = format!("{prefix}MacOS/{}", app.executable);
        require(
            record.identifier == app.identifier
                && info.files.get(&executable) == Some(&record.executable_sha256)
                && member(&files, &executable)?.mode == 0o755,
            "macOS signed executable receipt mismatch",
        )?;
        require(
            metadata::<Build>(&files, &format!("{prefix}Resources/build-info.json"))? == *runtime,
            "macOS embedded runtime mismatch",
        )?;
        for (name, source) in &resources {
            require(
                info.files.get(&format!("{prefix}Resources/{name}"))
                    == Some(&digest(&root.join(source))?),
                "macOS source resource mismatch",
            )?;
        }
        let plist = member(&files, &format!("{prefix}Info.plist"))?;
        require(
            plist.bytes <= 64 * 1024 && plist.mode == 0o644,
            "oversized or nonregular application plist",
        )?;
        let plist = plist::Value::from_reader(std::io::Cursor::new(
            plist
                .metadata
                .as_deref()
                .ok_or(Error::Refused("missing app plist"))?,
        ))?;
        let expected = macos::info(&settings, app, version);
        let expected = expected
            .as_dictionary()
            .ok_or(Error::Refused("invalid expected plist"))?;
        let actual = plist
            .as_dictionary()
            .ok_or(Error::Refused("invalid app plist"))?;
        require(
            expected
                .iter()
                .all(|(key, value)| actual.get(key) == Some(value)),
            "macOS application plist mismatch",
        )?;
    }
    Ok(name)
}

pub fn finalize(
    root: &Path,
    version: &str,
    incoming: &Path,
    source: &Path,
    out: &Path,
    run: u64,
    tagged: bool,
) -> Result<()> {
    let (commit, channel) = release::contract(root, version, tagged)?;
    require(run > 0, "invalid native producer run ID")?;
    require(
        fs::symlink_metadata(incoming)?.is_dir(),
        "invalid desktop input directory",
    )?;
    let mut targets = BTreeSet::new();
    for entry in fs::read_dir(incoming)? {
        let entry = entry?;
        require(
            fs::symlink_metadata(entry.path())?.is_dir(),
            "invalid desktop target directory",
        )?;
        targets.insert(entry.file_name());
    }
    require(
        targets
            == [format!("desktop-{LINUX}"), format!("desktop-{MACOS}")]
                .into_iter()
                .map(std::ffi::OsString::from)
                .collect(),
        "desktop target inventory mismatch",
    )?;
    let mut selected = BTreeMap::new();
    let mut runtimes = BTreeMap::new();
    for target in [LINUX, MACOS] {
        let directory = incoming.join(format!("desktop-{target}"));
        let (name, info) =
            verify_runtime(root, &directory, target, version, &commit, &channel, run)?;
        selected.insert(name.clone(), directory.join(name));
        runtimes.insert(target.to_owned(), info);
    }
    let directory = incoming.join(format!("desktop-{MACOS}"));
    let name = verify_apps(
        root,
        &directory,
        runtimes
            .get(MACOS)
            .ok_or(Error::Refused("missing native runtime"))?,
        &commit,
        version,
    )?;
    selected.insert(name.clone(), directory.join(name));
    let name = format!("nddev-remote-{version}-source.tar.gz");
    inventory(
        source,
        &BTreeSet::from([name.clone(), "source-sbom.spdx.json".into()]),
    )?;
    let temporary = tempfile::Builder::new()
        .prefix("nddev-source-verify-")
        .tempdir()?;
    let expected = temporary.path().join("source.tar.gz");
    archive::source(root, version, &expected)?;
    require(
        digest(&source.join(&name))? == digest(&expected)?,
        "source archive mismatch",
    )?;
    let sbom: Value = crate::json_limit(&source.join("source-sbom.spdx.json"), 300 * 1024 * 1024)?;
    require(
        sbom["spdxVersion"]
            .as_str()
            .is_some_and(|v| v.starts_with("SPDX-2."))
            && sbom["packages"].as_array().is_some_and(|p| !p.is_empty()),
        "empty or invalid source SBOM",
    )?;
    selected.insert(name.clone(), source.join(name));
    selected.insert(
        "source-sbom.spdx.json".into(),
        source.join("source-sbom.spdx.json"),
    );
    new_directory(out)?;
    for (name, path) in selected {
        crate::copy_new(&path, &out.join(name))?;
    }
    write_new(
        &out.join("release-notes.md"),
        release::notes(root, version)?.as_bytes(),
    )?;
    let mut assets = BTreeMap::new();
    for entry in fs::read_dir(out)? {
        let path = entry?.path();
        let name = path
            .file_name()
            .and_then(|v| v.to_str())
            .ok_or(Error::Refused("invalid output file name"))?
            .to_owned();
        assets.insert(
            name,
            json!({"sha256":digest(&path)?,"size":fs::metadata(path)?.len()}),
        );
    }
    let tag = if tagged {
        Some(crate::command(
            root,
            "git",
            &["rev-parse", &format!("refs/tags/{version}")],
        )?)
    } else {
        None
    };
    let manifest = json!({"schema_version":1,"product":"nddev-remote","version":version,"source_commit":commit,"source_tag_object":tag,"prerelease":true,"runtime_run":run.to_string(),"runtimes":runtimes,"macos_signing":"ad-hoc","macos_notarized":false,"assets":assets});
    write_new(&out.join("release-manifest.json"), &encoded(&manifest)?)?;
    let mut sums = BTreeMap::new();
    for entry in fs::read_dir(out)? {
        let path = entry?.path();
        let name = path
            .file_name()
            .and_then(|v| v.to_str())
            .ok_or(Error::Refused("invalid checksum file name"))?
            .to_owned();
        sums.insert(name, digest(&path)?);
    }
    write_new(
        &out.join("SHA256SUMS"),
        sums.into_iter()
            .map(|(name, digest)| format!("{digest}  {name}\n"))
            .collect::<String>()
            .as_bytes(),
    )
}
