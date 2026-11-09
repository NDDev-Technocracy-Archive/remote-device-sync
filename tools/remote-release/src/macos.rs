//! Native macOS bundle assembly; signs only with the explicitly selected identity.
use crate::{
    BINARIES, MACOS, Result, archive, command, design, digest, encoded, hex_identity, json,
    notice_hashes, regular, relative, require, runtime::Build, write_new,
};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, fs, path::Path};

#[derive(Deserialize)]
pub struct Settings {
    pub applications: Vec<Application>,
    pub icon: Icon,
    pub minimum_os: String,
}
#[derive(Deserialize)]
pub struct Application {
    pub name: String,
    pub identifier: String,
    pub executable: String,
    pub background: bool,
}
#[derive(Deserialize)]
pub struct Icon {
    pub name: String,
    pub source: String,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ApplicationReceipt {
    pub name: String,
    pub identifier: String,
    pub executable_sha256: String,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Receipt {
    pub schema_version: u32,
    pub runtime: Build,
    pub applications: Vec<ApplicationReceipt>,
    pub files: BTreeMap<String, String>,
    pub builder_sha256: String,
    pub settings_sha256: String,
    pub signing: String,
    pub notarized: bool,
}

pub fn settings(root: &Path) -> Result<Settings> {
    let settings: Settings = json(&root.join("packaging/macos.json"))?;
    require(
        !settings.applications.is_empty()
            && settings.applications.len() <= 8
            && relative(&settings.icon.name)
            && !settings.icon.name.contains('/')
            && relative(&settings.icon.source),
        "invalid macOS settings",
    )?;
    let mut names = std::collections::BTreeSet::new();
    for app in &settings.applications {
        require(
            !app.name.is_empty()
                && app.name.len() <= 128
                && app
                    .name
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b" ._-".contains(&b))
                && !app.name.starts_with('.')
                && names.insert(&app.name)
                && BINARIES.contains(&app.executable.as_str()),
            "invalid macOS application inventory",
        )?;
    }
    Ok(settings)
}

pub fn resources(root: &Path, settings: &Settings) -> Result<BTreeMap<String, String>> {
    let mut resources =
        BTreeMap::from([(settings.icon.name.clone(), settings.icon.source.clone())]);
    let notices: BTreeMap<String, String> = json(&root.join("packaging/notices.json"))?;
    for (name, path) in notices {
        require(
            relative(&name) && relative(&path),
            "invalid macOS resource path",
        )?;
        require(
            resources.insert(format!("Notices/{name}"), path).is_none(),
            "duplicate macOS resource",
        )?;
    }
    Ok(resources)
}

pub fn info(settings: &Settings, app: &Application, version: &str) -> plist::Value {
    let mut fields = plist::Dictionary::new();
    for (key, value) in [
        ("CFBundleIdentifier", app.identifier.as_str()),
        ("CFBundleName", &app.name),
        ("CFBundleDisplayName", &app.name),
        ("CFBundleExecutable", &app.executable),
        ("CFBundleIconFile", &settings.icon.name),
        ("CFBundleVersion", version),
        ("CFBundleShortVersionString", version),
        ("CFBundlePackageType", "APPL"),
        ("LSMinimumSystemVersion", &settings.minimum_os),
    ] {
        fields.insert(key.to_owned(), plist::Value::String(value.to_owned()));
    }
    fields.insert("LSUIElement".into(), plist::Value::Boolean(app.background));
    fields.insert(
        "NSHighResolutionCapable".into(),
        plist::Value::Boolean(true),
    );
    plist::Value::Dictionary(fields)
}

fn collect(root: &Path, path: &Path, files: &mut BTreeMap<String, archive::Input>) -> Result<()> {
    for entry in fs::read_dir(path)? {
        let entry = entry?;
        let path = entry.path();
        let metadata = fs::symlink_metadata(&path)?;
        if metadata.is_dir() {
            collect(root, &path, files)?;
        } else {
            regular(&path, 256 * 1024 * 1024)?;
            let name = path
                .strip_prefix(root)
                .map_err(|_| crate::Error::Refused("invalid bundle path"))?
                .to_str()
                .ok_or(crate::Error::Refused("invalid bundle encoding"))?
                .to_owned();
            require(
                relative(&name) && files.len() < 4096,
                "invalid bundle inventory",
            )?;
            let executable = name.contains("/MacOS/");
            files.insert(name, archive::Input { path, executable });
        }
    }
    Ok(())
}

pub fn package(
    root: &Path,
    binaries: &Path,
    build_info: &Path,
    out: &Path,
    identity: &str,
) -> Result<()> {
    require(
        std::env::consts::OS == "macos" && std::env::consts::ARCH == "aarch64",
        "bundle on a native macOS arm64 executor",
    )?;
    require(
        identity == "-" || identity.len() == 40 && identity.bytes().all(|v| v.is_ascii_hexdigit()),
        "select an existing signing identity by certificate digest",
    )?;
    let base: Build = json(build_info)?;
    require(
        base.schema_version == 1
            && base.target == MACOS
            && hex_identity(&base.source, 20)
            && base.binaries.keys().map(String::as_str).eq(BINARIES),
        "unsupported input build identity",
    )?;
    let version = base.version.split(['-', '+']).next().unwrap_or("");
    require(
        crate::release::numeric_version(version),
        "unsupported application version",
    )?;
    for (name, selected) in &base.binaries {
        let path = binaries.join(name);
        let metadata = regular(&path, 256 * 1024 * 1024)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            require(
                metadata.permissions().mode() & 0o111 != 0,
                "input executable mode changed",
            )?;
        }
        require(
            metadata.len() > 0 && digest(&path)? == *selected,
            "input executable differs from build metadata",
        )?;
        let path = path
            .to_str()
            .ok_or(crate::Error::Refused("invalid executable path"))?;
        let dependencies = command(root, "otool", &["-L", path])?;
        require(
            dependencies.lines().skip(1).all(|line| {
                let path = line.trim().split(" (").next().unwrap_or("");
                path.starts_with("/System/Library/") || path.starts_with("/usr/lib/")
            }),
            "runtime depends on an unbundled library",
        )?;
    }
    let settings = settings(root)?;
    require(
        base.design_system == design::verify(root)?,
        "native design pin differs from source",
    )?;
    require(
        base.notices == notice_hashes(root)?,
        "native notices differ from source",
    )?;
    let resources = resources(root, &settings)?;
    fs::create_dir_all(out)?;
    let name = format!("nddev-remote-macos-apps-{}.tar.gz", base.source);
    require(
        !out.join(&name).try_exists()? && !out.join("apps-build-info.json").try_exists()?,
        "retain existing package; select fresh output",
    )?;
    let temporary = tempfile::Builder::new()
        .prefix("nddev-remote-apps-")
        .tempdir()?;
    let mut applications = Vec::new();
    for app in &settings.applications {
        let bundle = temporary.path().join(format!("{}.app", app.name));
        let contents = bundle.join("Contents");
        fs::create_dir_all(contents.join("MacOS"))?;
        fs::create_dir(contents.join("Resources"))?;
        let executable = contents.join("MacOS").join(&app.executable);
        fs::copy(binaries.join(&app.executable), &executable)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&executable, fs::Permissions::from_mode(0o755))?;
        }
        info(&settings, app, version).to_writer_xml(
            fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(contents.join("Info.plist"))?,
        )?;
        write_new(
            &contents.join("Resources/build-info.json"),
            &encoded(&base)?,
        )?;
        for (name, path) in &resources {
            regular(&root.join(path), 16 * 1024 * 1024)?;
            require(
                relative(name) && relative(path),
                "invalid application resource",
            )?;
            let destination = contents.join("Resources").join(name);
            fs::create_dir_all(
                destination
                    .parent()
                    .ok_or(crate::Error::Refused("missing resource parent"))?,
            )?;
            fs::copy(root.join(path), destination)?;
        }
        let bundle_path = bundle
            .to_str()
            .ok_or(crate::Error::Refused("invalid bundle encoding"))?;
        command(
            root,
            "codesign",
            &[
                "--force",
                "--sign",
                identity,
                "--options",
                "runtime",
                "--timestamp=none",
                bundle_path,
            ],
        )?;
        command(
            root,
            "codesign",
            &["--verify", "--deep", "--strict", bundle_path],
        )?;
        command(
            root,
            executable
                .to_str()
                .ok_or(crate::Error::Refused("invalid executable encoding"))?,
            &["--help"],
        )?;
        applications.push(ApplicationReceipt {
            name: app.name.clone(),
            identifier: app.identifier.clone(),
            executable_sha256: digest(&executable)?,
        });
    }
    let mut files = BTreeMap::new();
    collect(temporary.path(), temporary.path(), &mut files)?;
    let hashes = files
        .iter()
        .map(|(name, file)| Ok((name.clone(), digest(&file.path)?)))
        .collect::<Result<_>>()?;
    let receipt = Receipt {
        schema_version: 1,
        runtime: base.clone(),
        applications,
        files: hashes,
        builder_sha256: digest(&root.join("tools/remote-release/src/macos.rs"))?,
        settings_sha256: digest(&root.join("packaging/macos.json"))?,
        signing: if identity == "-" {
            "ad-hoc"
        } else {
            "local-certificate"
        }
        .to_owned(),
        notarized: false,
    };
    let metadata = encoded(&receipt)?;
    require(metadata.len() <= 128 * 1024, "oversized apps metadata")?;
    write_new(&temporary.path().join("apps-build-info.json"), &metadata)?;
    files.insert(
        "apps-build-info.json".into(),
        archive::Input {
            path: temporary.path().join("apps-build-info.json"),
            executable: false,
        },
    );
    archive::bundle(&out.join(&name), &files)?;
    write_new(&out.join("apps-build-info.json"), &metadata)?;
    write_new(
        &out.join("apps-SHA256SUMS"),
        format!("{}  {name}\n", digest(&out.join(&name))?).as_bytes(),
    )
}
