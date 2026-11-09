//! Adversarial fixtures never publish, enroll a device or change global state.
mod support;
use nddev_remote_release::{self as tool, archive, design, promote, release};
use serde_json::{Value, json};
use std::{fs, io::Write, path::Path};
use support::{Fixture, RUN, RUST, VERSION};

#[test]
fn version_lock_source_and_annotated_tag_are_bound() {
    let mut f = Fixture::new();
    assert_eq!(
        release::contract_with_ref(&f.root, VERSION, true, Some("refs/tags/0.1.0")).unwrap(),
        (f.commit.clone(), RUST.into())
    );
    assert!(release::contract_with_ref(&f.root, VERSION, true, Some("refs/heads/main")).is_err());
    for (name, value) in [
        ("VERSION", "0.1.0"),
        (
            "Cargo.lock",
            "[[package]]\nname='example'\nversion='9.0.0'\n",
        ),
        ("rust-toolchain.toml", "[toolchain]\nchannel='stable'\n"),
        ("LICENSE", "changed"),
    ] {
        let path = f.root.join(name);
        let previous = fs::read(&path).unwrap();
        fs::write(&path, value).unwrap();
        assert!(
            release::contract(&f.root, VERSION, false).is_err(),
            "accepted changed {name}"
        );
        fs::write(&path, previous).unwrap();
    }
    f.git(&["tag", "-d", VERSION]);
    f.git(&["-c", "tag.gpgSign=false", "tag", VERSION]);
    assert!(release::contract_with_ref(&f.root, VERSION, true, Some("refs/tags/0.1.0")).is_err());
    f.git(&["tag", "-d", VERSION]);
    f.git(&[
        "-c",
        "tag.gpgSign=false",
        "tag",
        "-a",
        VERSION,
        "-m",
        "synthetic",
    ]);
    fs::write(f.root.join("new-source"), "new").unwrap();
    f.commit();
    assert!(release::contract_with_ref(&f.root, VERSION, true, Some("refs/tags/0.1.0")).is_err());
}

#[test]
fn source_includes_new_tracked_roots_and_rejects_index_links() {
    let mut f = Fixture::new();
    fs::create_dir(f.root.join("native-build")).unwrap();
    fs::write(
        f.root.join("native-build/required.h"),
        "required native bytes\n",
    )
    .unwrap();
    f.commit();
    let out = f.temp.path().join("source");
    release::source(&f.root, VERSION, &out, false).unwrap();
    assert_eq!(
        fs::read(out.join("source/nddev-remote-0.1.0/native-build/required.h")).unwrap(),
        b"required native bytes\n"
    );
    f.git(&[
        "update-index",
        "--add",
        "--cacheinfo",
        &format!(
            "120000,{},synthetic-link",
            f.git(&["rev-parse", "HEAD:LICENSE"])
        ),
    ]);
    assert!(archive::source(&f.root, VERSION, &f.temp.path().join("refused.tar.gz")).is_err());
}

#[test]
fn released_design_rejects_modified_extra_duplicate_or_foreign_files() {
    let f = Fixture::new();
    let expected = design::verify(&f.root).unwrap();
    let file = f
        .root
        .join("vendor/nddev-opennetwork-design-tokens/src/lib.rs");
    let original = fs::read(&file).unwrap();
    fs::write(&file, b"modified").unwrap();
    assert!(design::verify(&f.root).is_err());
    fs::write(&file, original).unwrap();
    let extra = file.with_file_name("extra.rs");
    fs::write(&extra, b"unreleased").unwrap();
    assert!(design::verify(&f.root).is_err());
    fs::remove_file(extra).unwrap();
    let pin = f.root.join("packaging/design-system.json");
    let original = fs::read(&pin).unwrap();
    let mut value: Value = serde_json::from_slice(&original).unwrap();
    let first = value["files"][0].clone();
    value["files"].as_array_mut().unwrap().push(first);
    fs::write(&pin, serde_json::to_vec(&value).unwrap()).unwrap();
    assert!(design::verify(&f.root).is_err());
    fs::write(pin, original).unwrap();
    assert_eq!(design::verify(&f.root).unwrap(), expected);
    #[cfg(unix)]
    {
        let link = file.with_file_name("foreign.rs");
        std::os::unix::fs::symlink("/etc/passwd", &link).unwrap();
        assert!(design::verify(&f.root).is_err());
    }
}

fn evidence(commit: &str) -> (Vec<Value>, Vec<Value>) {
    let runs=["ci.yml","supply-chain.yml","security.yml","desktop-runtime.yml"].into_iter().map(|name|json!({"id":RUN,"path":format!(".github/workflows/{name}"),"head_sha":commit,"event":"push","head_branch":"main","status":"completed","conclusion":"success"})).collect();
    let analyses=["rust","actions"].into_iter().map(|language|json!({"ref":"refs/heads/main","category":format!("/language:{language}"),"analysis_key":".github/workflows/security.yml:codeql","tool":{"name":"CodeQL"},"commit_sha":commit,"error":"","warning":""})).collect();
    (runs, analyses)
}

#[test]
fn green_workflows_do_not_substitute_for_complete_scans_or_resolved_alerts() {
    let commit = "a".repeat(40);
    let (runs, analyses) = evidence(&commit);
    assert_eq!(
        release::verify_ci(&commit, &runs, &analyses, &[]).unwrap(),
        RUN
    );
    assert!(release::verify_ci(&commit, &runs, &analyses, &[json!({"number":1})]).is_err());
    for (field, value) in [
        ("head_sha", json!("b".repeat(40))),
        ("event", json!("pull_request")),
        ("head_branch", json!("topic")),
        ("status", json!("in_progress")),
        ("conclusion", json!("failure")),
    ] {
        let mut mutated = runs.clone();
        mutated[0][field] = value;
        assert!(release::verify_ci(&commit, &mutated, &analyses, &[]).is_err());
    }
    let mut duplicate = runs.clone();
    duplicate.push(runs[0].clone());
    assert!(release::verify_ci(&commit, &duplicate, &analyses, &[]).is_err());
    assert!(release::verify_ci(&commit, &runs[..3], &analyses, &[]).is_err());
    for (field, value) in [
        ("commit_sha", json!("b".repeat(40))),
        ("ref", json!("refs/pull/1/merge")),
        ("category", json!("other")),
        ("analysis_key", json!("foreign")),
        ("tool", json!({"name":"other"})),
        ("error", json!("incomplete extraction")),
        ("warning", json!("partial results")),
    ] {
        let mut mutated = analyses.clone();
        mutated[0][field] = value;
        assert!(release::verify_ci(&commit, &runs, &mutated, &[]).is_err());
    }
    let mut duplicate = analyses.clone();
    duplicate.push(analyses[0].clone());
    assert!(release::verify_ci(&commit, &runs, &duplicate, &[]).is_err());
    assert!(release::verify_ci(&commit, &runs, &analyses[..1], &[]).is_err());
    let mut later = analyses[0].clone();
    later["commit_sha"] = json!("b".repeat(40));
    let mut older = analyses.clone();
    older.push(later.clone());
    assert!(release::verify_ci(&commit, &runs, &older, &[]).is_ok());
    older.insert(0, later);
    assert!(release::verify_ci(&commit, &runs, &older, &[]).is_err());
}

#[test]
fn paginated_evidence_retains_late_alerts() {
    let f = Fixture::new();
    let (runs, analyses) = evidence(&f.commit);
    let checks = f.temp.path().join("checks");
    let scans = f.temp.path().join("scans");
    let alerts = f.temp.path().join("alerts");
    fs::write(
        &checks,
        json!([{"workflow_runs":&runs[..1]},{"workflow_runs":&runs[1..]}]).to_string(),
    )
    .unwrap();
    fs::write(&scans, json!([&analyses[..1], &analyses[1..]]).to_string()).unwrap();
    fs::write(&alerts, "[[], []]").unwrap();
    assert!(release::evidence(&f.root, VERSION, false, &checks, &scans, &alerts).is_ok());
    fs::write(&alerts, "[[], [{\"number\":101}]]").unwrap();
    assert!(release::evidence(&f.root, VERSION, false, &checks, &scans, &alerts).is_err());
}

#[test]
fn archive_inventory_refuses_links_duplicates_and_oversized_declarations() {
    for kind in ["link", "duplicate", "oversize"] {
        let temporary = tempfile::tempdir().unwrap();
        let path = temporary.path().join("bad.tar.gz");
        let output = fs::File::create(&path).unwrap();
        let zipped = flate2::GzBuilder::new()
            .mtime(0)
            .write(output, flate2::Compression::default());
        let mut tar = tar::Builder::new(zipped);
        let mut header = tar::Header::new_ustar();
        header.set_path("body").unwrap();
        header.set_mode(0o644);
        header.set_size(1);
        if kind == "link" {
            header.set_entry_type(tar::EntryType::Symlink);
            header.set_link_name("/etc/passwd").unwrap();
            header.set_size(0);
            header.set_cksum();
            tar.append(&header, std::io::empty()).unwrap();
        } else if kind == "duplicate" {
            header.set_cksum();
            tar.append(&header, &b"a"[..]).unwrap();
            tar.append(&header, &b"b"[..]).unwrap();
        } else {
            let mut zipped = tar.into_inner().unwrap();
            header.set_size(256 * 1024 * 1024 + 1);
            header.set_cksum();
            zipped.write_all(header.as_bytes()).unwrap();
            zipped.write_all(&[0; 1024]).unwrap();
            zipped.finish().unwrap();
            assert!(archive::inspect(&path).is_err());
            continue;
        }
        tar.into_inner().unwrap().finish().unwrap();
        assert!(archive::inspect(&path).is_err(), "accepted {kind}");
    }
}

#[test]
fn native_runtime_packaging_is_deterministic_and_never_reuses_output() {
    let f = Fixture::new();
    let binaries = f.temp.path().join("binaries");
    fs::create_dir(&binaries).unwrap();
    for name in tool::BINARIES {
        let path = binaries.join(name);
        fs::write(&path, b"synthetic executable bytes").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
        }
    }
    let target = if std::env::consts::OS == "macos" {
        tool::MACOS
    } else {
        tool::LINUX
    };
    let features = tool::runtime::features(target).unwrap().join(",");
    let one = f.temp.path().join("one");
    let two = f.temp.path().join("two");
    tool::runtime::package(&f.root, &binaries, target, &features, &one).unwrap();
    tool::runtime::package(&f.root, &binaries, target, &features, &two).unwrap();
    let name = format!("nddev-remote-{target}-{}.tar.gz", f.commit);
    assert_eq!(
        fs::read(one.join(&name)).unwrap(),
        fs::read(two.join(&name)).unwrap()
    );
    assert!(tool::runtime::package(&f.root, &binaries, target, &features, &one).is_err());
    #[cfg(unix)]
    {
        let path = binaries.join("rds");
        fs::remove_file(&path).unwrap();
        std::os::unix::fs::symlink("/etc/passwd", path).unwrap();
        assert!(
            tool::runtime::package(
                &f.root,
                &binaries,
                target,
                &features,
                &f.temp.path().join("unsafe")
            )
            .is_err()
        );
    }
}

fn tamper_metadata(directory: &Path, name: &str, key: &str, value: Value) {
    let path = directory.join(name);
    let mut info: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    info[key] = value;
    fs::write(path, serde_json::to_vec(&info).unwrap()).unwrap();
}

#[test]
fn promotion_checks_every_target_source_notice_and_signed_bundle_receipt() {
    let f = Fixture::new();
    let incoming = f.temp.path().join("incoming");
    let linux = incoming.join(format!("desktop-{}", tool::LINUX));
    let mac = incoming.join(format!("desktop-{}", tool::MACOS));
    let runtime = f.runtime(&linux, tool::LINUX);
    let mac_runtime = f.runtime(&mac, tool::MACOS);
    let source = f.temp.path().join("source");
    f.source(&source);
    let out = f.temp.path().join("dist");
    promote::finalize(&f.root, VERSION, &incoming, &source, &out, RUN, false).unwrap();
    let manifest: Value = tool::json(&out.join("release-manifest.json")).unwrap();
    for (name, asset) in manifest["assets"].as_object().unwrap() {
        assert_eq!(
            tool::digest(&out.join(name)).unwrap(),
            asset["sha256"].as_str().unwrap()
        );
    }
    assert!(promote::finalize(&f.root, VERSION, &incoming, &source, &out, RUN, false).is_err());
    let original = fs::read(linux.join("build-info.json")).unwrap();
    for (field, value) in [
        ("source", json!("b".repeat(40))),
        ("target", json!(tool::MACOS)),
        ("features", json!([])),
        ("notices", json!({})),
        ("build", json!({"GITHUB_RUN_ID":"999"})),
        ("unexpected", json!(true)),
    ] {
        tamper_metadata(&linux, "build-info.json", field, value);
        assert!(
            promote::verify_runtime(&f.root, &linux, tool::LINUX, VERSION, &f.commit, RUST, RUN)
                .is_err(),
            "accepted changed {field}"
        );
        fs::write(linux.join("build-info.json"), &original).unwrap();
    }
    assert_eq!(runtime.source, f.commit);
    let original = fs::read(mac.join("apps-build-info.json")).unwrap();
    for (field, value) in [
        ("signing", json!("local-certificate")),
        ("builder_sha256", json!("f".repeat(64))),
        ("files", json!({})),
        ("applications", json!([])),
        ("notarized", json!(true)),
    ] {
        tamper_metadata(&mac, "apps-build-info.json", field, value);
        assert!(
            promote::verify_apps(&f.root, &mac, &mac_runtime, &f.commit, VERSION).is_err(),
            "accepted changed {field}"
        );
        fs::write(mac.join("apps-build-info.json"), &original).unwrap();
    }
    fs::write(
        source.join("source-sbom.spdx.json"),
        "{\"spdxVersion\":\"SPDX-2.3\",\"packages\":[]}",
    )
    .unwrap();
    assert!(
        promote::finalize(
            &f.root,
            VERSION,
            &incoming,
            &source,
            &f.temp.path().join("bad-source"),
            RUN,
            false
        )
        .is_err()
    );
    fs::write(linux.join("extra"), b"unrequested").unwrap();
    assert!(
        promote::verify_runtime(&f.root, &linux, tool::LINUX, VERSION, &f.commit, RUST, RUN)
            .is_err()
    );
}
