use nddev_remote_release::{self as tool, archive, design, macos, runtime::Build};
use serde_json::json;
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
};
pub const VERSION: &str = "0.1.0";
pub const RUST: &str = "1.99.1";
pub const RUN: u64 = 431;
pub struct Fixture {
    pub temp: tempfile::TempDir,
    pub root: PathBuf,
    pub commit: String,
}
fn copy_tree(source: &Path, target: &Path) {
    fs::create_dir_all(target).unwrap();
    for entry in fs::read_dir(source).unwrap() {
        let entry = entry.unwrap();
        if entry.file_type().unwrap().is_dir() {
            copy_tree(&entry.path(), &target.join(entry.file_name()));
        } else {
            fs::copy(entry.path(), target.join(entry.file_name())).unwrap();
        }
    }
}
impl Fixture {
    pub fn new() -> Self {
        let temp = tempfile::Builder::new()
            .prefix("remote-release-fixture-")
            .tempdir()
            .unwrap();
        let root = temp.path().join("repo");
        fs::create_dir(&root).unwrap();
        let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        for path in [
            "packaging",
            "vendor/nddev-opennetwork-design-tokens",
            "crates/rds-desktop/assets/font-licenses",
        ] {
            copy_tree(&source.join(path), &root.join(path));
        }
        fs::copy(
            source.join("crates/rds-desktop/assets/app-icon.icns"),
            root.join("crates/rds-desktop/assets/app-icon.icns"),
        )
        .unwrap();
        fs::copy(source.join("LICENSE"), root.join("LICENSE")).unwrap();
        fs::create_dir_all(root.join("crates/example")).unwrap();
        fs::create_dir_all(root.join("tools/remote-release/src")).unwrap();
        for (path, body) in [
            ("VERSION", "0.1.0\n"),
            (
                "Cargo.toml",
                "[workspace]\nmembers=['crates/example']\n[workspace.package]\nversion='0.1.0'\n",
            ),
            (
                "Cargo.lock",
                "version=4\n[[package]]\nname='example'\nversion='0.1.0'\n",
            ),
            (
                "crates/example/Cargo.toml",
                "[package]\nname='example'\nversion='0.1.0'\n",
            ),
            ("CHANGELOG.md", "## [0.1.0]\nEngineering preview.\n"),
            ("rust-toolchain.toml", "[toolchain]\nchannel='1.99.1'\n"),
            (
                "tools/remote-release/src/macos.rs",
                "synthetic selected builder bytes\n",
            ),
        ] {
            fs::write(root.join(path), body).unwrap();
        }
        let mut f = Self {
            temp,
            root,
            commit: String::new(),
        };
        f.git(&["init", "-q"]);
        f.git(&["config", "user.name", "Release Fixture"]);
        f.git(&["config", "user.email", "fixture@example.invalid"]);
        f.commit();
        f.git(&[
            "-c",
            "tag.gpgSign=false",
            "tag",
            "-a",
            VERSION,
            "-m",
            "synthetic fixture",
        ]);
        f
    }
    pub fn git(&self, args: &[&str]) -> String {
        tool::command(&self.root, "git", args).unwrap()
    }
    pub fn commit(&mut self) {
        self.git(&["add", "."]);
        self.git(&[
            "-c",
            "commit.gpgsign=false",
            "-c",
            "core.hooksPath=/dev/null",
            "commit",
            "-qm",
            "test: retain synthetic fixture",
        ]);
        self.commit = self.git(&["rev-parse", "HEAD"]);
    }
    pub fn build(&self, target: &str) -> Build {
        Build {
            design_system: design::verify(&self.root).unwrap(),
            schema_version: 1,
            source: self.commit.clone(),
            version: VERSION.into(),
            target: target.into(),
            features: tool::runtime::features(target).unwrap(),
            rustc: format!("rustc {RUST} (fixture)\nhost: {target}\nLLVM version: 23\n"),
            binaries: tool::BINARIES
                .into_iter()
                .map(|b| (b.to_owned(), tool::digest_bytes(b.as_bytes())))
                .collect(),
            notices: tool::notice_hashes(&self.root).unwrap(),
            build: BTreeMap::from([
                ("GITHUB_REPOSITORY".into(), tool::REPOSITORY.into()),
                ("GITHUB_WORKFLOW".into(), "desktop-runtime".into()),
                ("GITHUB_REF".into(), "refs/heads/main".into()),
                ("GITHUB_RUN_ID".into(), RUN.to_string()),
            ]),
        }
    }
    pub fn runtime(&self, directory: &Path, target: &str) -> Build {
        fs::create_dir_all(directory).unwrap();
        let base = self.build(target);
        let mut files = BTreeMap::new();
        let payload = tempfile::tempdir().unwrap();
        for name in tool::BINARIES {
            let path = payload.path().join(name);
            fs::write(&path, name).unwrap();
            files.insert(
                name.to_owned(),
                archive::Input {
                    path,
                    executable: true,
                },
            );
        }
        let notices: BTreeMap<String, String> =
            tool::json(&self.root.join("packaging/notices.json")).unwrap();
        for (name, path) in notices {
            files.insert(
                name,
                archive::Input {
                    path: self.root.join(path),
                    executable: false,
                },
            );
        }
        let metadata = tool::encoded(&base).unwrap();
        fs::write(directory.join("build-info.json"), metadata).unwrap();
        files.insert(
            "build-info.json".into(),
            archive::Input {
                path: directory.join("build-info.json"),
                executable: false,
            },
        );
        let name = format!("nddev-remote-{target}-{}.tar.gz", self.commit);
        archive::bundle(&directory.join(&name), &files).unwrap();
        self.sums(directory, "SHA256SUMS", &name);
        if target == tool::MACOS {
            self.apps(directory, &base);
        }
        base
    }
    pub fn apps(&self, directory: &Path, base: &Build) {
        let settings = macos::settings(&self.root).unwrap();
        let resources = macos::resources(&self.root, &settings).unwrap();
        let staging = tempfile::tempdir().unwrap();
        let mut files = BTreeMap::new();
        let mut apps = Vec::new();
        for app in &settings.applications {
            let prefix = format!("{}.app/Contents/", app.name);
            let paths = [
                (format!("{prefix}Info.plist"), Vec::new()),
                (
                    format!("{prefix}Resources/build-info.json"),
                    tool::encoded(base).unwrap(),
                ),
                (
                    format!("{prefix}_CodeSignature/CodeResources"),
                    b"synthetic signed resource proof".to_vec(),
                ),
                (
                    format!("{prefix}MacOS/{}", app.executable),
                    b"synthetic signed Mach-O bytes".to_vec(),
                ),
            ];
            for (name, mut body) in paths {
                if name.ends_with("Info.plist") {
                    macos::info(&settings, app, VERSION)
                        .to_writer_xml(&mut body)
                        .unwrap();
                }
                let path = staging.path().join(&name);
                fs::create_dir_all(path.parent().unwrap()).unwrap();
                fs::write(&path, body).unwrap();
                let executable = name.contains("/MacOS/");
                files.insert(name, archive::Input { path, executable });
            }
            for (name, path) in &resources {
                files.insert(
                    format!("{prefix}Resources/{name}"),
                    archive::Input {
                        path: self.root.join(path),
                        executable: false,
                    },
                );
            }
            apps.push(macos::ApplicationReceipt {
                name: app.name.clone(),
                identifier: app.identifier.clone(),
                executable_sha256: tool::digest_bytes(b"synthetic signed Mach-O bytes"),
            });
        }
        let hashes = files
            .iter()
            .map(|(n, v)| (n.clone(), tool::digest(&v.path).unwrap()))
            .collect();
        let receipt = macos::Receipt {
            schema_version: 1,
            runtime: base.clone(),
            applications: apps,
            files: hashes,
            builder_sha256: tool::digest(&self.root.join("tools/remote-release/src/macos.rs"))
                .unwrap(),
            settings_sha256: tool::digest(&self.root.join("packaging/macos.json")).unwrap(),
            signing: "ad-hoc".into(),
            notarized: false,
        };
        fs::write(
            directory.join("apps-build-info.json"),
            tool::encoded(&receipt).unwrap(),
        )
        .unwrap();
        files.insert(
            "apps-build-info.json".into(),
            archive::Input {
                path: directory.join("apps-build-info.json"),
                executable: false,
            },
        );
        let name = format!("nddev-remote-macos-apps-{}.tar.gz", self.commit);
        archive::bundle(&directory.join(&name), &files).unwrap();
        self.sums(directory, "apps-SHA256SUMS", &name);
    }
    pub fn sums(&self, directory: &Path, sums: &str, name: &str) {
        fs::write(
            directory.join(sums),
            format!("{}  {name}\n", tool::digest(&directory.join(name)).unwrap()),
        )
        .unwrap();
    }
    pub fn source(&self, directory: &Path) {
        fs::create_dir_all(directory).unwrap();
        tool::archive::source(
            &self.root,
            VERSION,
            &directory.join(format!("nddev-remote-{VERSION}-source.tar.gz")),
        )
        .unwrap();
        fs::write(
            directory.join("source-sbom.spdx.json"),
            json!({"spdxVersion":"SPDX-2.3","packages":[{"name":"synthetic"}]}).to_string(),
        )
        .unwrap();
    }
}
