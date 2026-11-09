//! Offline packaging and fail-closed provenance checks. No enrollment or network.
#![forbid(unsafe_code)]

pub mod archive;
pub mod design;
pub mod macos;
pub mod promote;
pub mod release;
pub mod runtime;

use std::{collections::BTreeMap, fs, io::Read, path::Path, process::Command};

use serde::{Serialize, de::DeserializeOwned};
use sha2::{Digest, Sha256};

pub const REPOSITORY: &str = "NDDev-Technocracy-OpenNetwork/nddev-remote";
pub const BINARIES: [&str; 3] = ["nddev-remote", "rds", "rds-agent"];
pub const LINUX: &str = "x86_64-unknown-linux-gnu";
pub const MACOS: &str = "aarch64-apple-darwin";
pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("release policy refused: {0}")]
    Refused(&'static str),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error(transparent)]
    Toml(#[from] toml::de::Error),
    #[error(transparent)]
    Plist(#[from] plist::Error),
}

pub fn require(allowed: bool, reason: &'static str) -> Result<()> {
    if allowed {
        Ok(())
    } else {
        Err(Error::Refused(reason))
    }
}

pub fn hex_identity(value: &str, bytes: usize) -> bool {
    value.len() == bytes * 2
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

pub fn relative(value: &str) -> bool {
    !value.is_empty()
        && !value.starts_with('/')
        && !value.contains('\\')
        && value
            .split('/')
            .all(|p| !p.is_empty() && p != "." && p != "..")
}

pub fn regular(path: &Path, maximum: u64) -> Result<fs::Metadata> {
    let metadata = fs::symlink_metadata(path)?;
    require(
        metadata.is_file() && metadata.len() <= maximum,
        "expected bounded regular file",
    )?;
    Ok(metadata)
}

pub fn read(path: &Path, maximum: u64) -> Result<Vec<u8>> {
    regular(path, maximum)?;
    let mut data = Vec::new();
    fs::File::open(path)?
        .take(maximum + 1)
        .read_to_end(&mut data)?;
    require(data.len() as u64 <= maximum, "file grew beyond its bound")?;
    Ok(data)
}

pub fn json<T: DeserializeOwned>(path: &Path) -> Result<T> {
    json_limit(path, 128 * 1024)
}
pub fn json_limit<T: DeserializeOwned>(path: &Path, maximum: u64) -> Result<T> {
    Ok(serde_json::from_slice(&read(path, maximum)?)?)
}

pub fn encoded(value: &impl Serialize) -> Result<Vec<u8>> {
    let mut bytes = serde_json::to_vec_pretty(value)?;
    bytes.push(b'\n');
    Ok(bytes)
}

pub fn write_new(path: &Path, bytes: &[u8]) -> Result<()> {
    use std::io::Write;
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)?;
    file.write_all(bytes)?;
    Ok(())
}

pub fn copy_new(source: &Path, target: &Path) -> Result<()> {
    let size = regular(source, 300 * 1024 * 1024)?.len();
    let mut input = fs::File::open(source)?.take(300 * 1024 * 1024 + 1);
    let mut output = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(target)?;
    require(
        std::io::copy(&mut input, &mut output)? == size,
        "source changed while copying",
    )
}

pub fn digest(path: &Path) -> Result<String> {
    regular(path, 300 * 1024 * 1024)?;
    let mut hash = Sha256::new();
    let mut input = fs::File::open(path)?;
    let mut buffer = [0; 64 * 1024];
    let mut total = 0u64;
    loop {
        let count = input.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        total += count as u64;
        require(
            total <= 300 * 1024 * 1024,
            "file grew beyond its digest bound",
        )?;
        hash.update(&buffer[..count]);
    }
    Ok(hex::encode(hash.finalize()))
}

pub fn digest_bytes(value: &[u8]) -> String {
    hex::encode(Sha256::digest(value))
}

pub fn command(root: &Path, name: &str, args: &[&str]) -> Result<String> {
    let output = Command::new(name).args(args).current_dir(root).output()?;
    require(output.status.success(), "native command failed")?;
    require(
        output.stdout.len() <= 8 * 1024 * 1024,
        "native command output exceeded its bound",
    )?;
    Ok(String::from_utf8(output.stdout)
        .map_err(|_| Error::Refused("native command output was not UTF-8"))?
        .trim()
        .to_owned())
}

pub fn inventory(directory: &Path, expected: &std::collections::BTreeSet<String>) -> Result<()> {
    let metadata = fs::symlink_metadata(directory)?;
    require(metadata.is_dir(), "invalid artifact directory")?;
    let mut actual = std::collections::BTreeSet::new();
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        regular(&entry.path(), 300 * 1024 * 1024)?;
        actual.insert(
            entry
                .file_name()
                .into_string()
                .map_err(|_| Error::Refused("non-UTF-8 artifact name"))?,
        );
    }
    require(&actual == expected, "artifact inventory mismatch")
}

pub fn notice_hashes(root: &Path) -> Result<BTreeMap<String, String>> {
    let notices: BTreeMap<String, String> = json(&root.join("packaging/notices.json"))?;
    require(
        !notices.is_empty() && notices.len() <= 128,
        "invalid notice inventory",
    )?;
    notices
        .into_iter()
        .map(|(name, source)| {
            require(relative(&name) && relative(&source), "invalid notice path")?;
            regular(&root.join(&source), 128 * 1024)?;
            Ok((name, digest(&root.join(source))?))
        })
        .collect()
}

pub fn new_directory(path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::create_dir(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}
