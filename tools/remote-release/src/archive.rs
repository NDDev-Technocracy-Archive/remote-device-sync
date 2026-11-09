//! Streaming deterministic archives and bounded, unique regular member inventory.
use std::{
    collections::BTreeMap,
    fs,
    io::{Read, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

use flate2::{Compression, GzBuilder, read::GzDecoder};
use sha2::{Digest, Sha256};

use crate::{Error, Result, regular, relative, require};

pub struct Input {
    pub path: PathBuf,
    pub executable: bool,
}
pub struct Member {
    pub digest: String,
    pub mode: u32,
    pub bytes: u64,
    pub metadata: Option<Vec<u8>>,
}

pub fn bundle(path: &Path, files: &BTreeMap<String, Input>) -> Result<()> {
    let output = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)?;
    let zipped = GzBuilder::new()
        .mtime(0)
        .operating_system(255)
        .write(output, Compression::default());
    let mut tar = tar::Builder::new(zipped);
    for (name, file) in files {
        require(relative(name), "unsafe archive path")?;
        let size = regular(&file.path, 256 * 1024 * 1024)?.len();
        require(size > 0, "empty package member")?;
        let mut header = tar::Header::new_ustar();
        header.set_size(size);
        header.set_mode(if file.executable { 0o755 } else { 0o644 });
        header.set_uid(0);
        header.set_gid(0);
        header.set_mtime(0);
        header.set_cksum();
        tar.append_data(&mut header, name, fs::File::open(&file.path)?)?;
    }
    tar.into_inner()?.finish()?.flush()?;
    Ok(())
}

pub fn inspect(path: &Path) -> Result<BTreeMap<String, Member>> {
    regular(path, 300 * 1024 * 1024)?;
    let mut tar = tar::Archive::new(GzDecoder::new(fs::File::open(path)?));
    let mut files = BTreeMap::new();
    let mut total = 0u64;
    for entry in tar.entries()? {
        let mut entry = entry?;
        let name = entry
            .path()?
            .to_str()
            .ok_or(Error::Refused("non-UTF-8 package path"))?
            .to_owned();
        require(
            relative(&name) && entry.header().entry_type().is_file(),
            "unsafe package member",
        )?;
        let bytes = entry.size();
        require(
            bytes > 0 && bytes <= 256 * 1024 * 1024 && files.len() < 4096,
            "oversized package member inventory",
        )?;
        total = total
            .checked_add(bytes)
            .ok_or(Error::Refused("package size overflow"))?;
        require(total <= 1024 * 1024 * 1024, "oversized package expansion")?;
        let mode = entry.header().mode()?;
        let mut hash = Sha256::new();
        let mut buffer = [0; 64 * 1024];
        let mut metadata = (name.ends_with(".json") || name.ends_with(".plist")).then(Vec::new);
        if metadata.is_some() {
            require(bytes <= 128 * 1024, "oversized package metadata")?;
        }
        let mut read = 0;
        loop {
            let count = entry.read(&mut buffer)?;
            if count == 0 {
                break;
            }
            read += count as u64;
            hash.update(&buffer[..count]);
            if let Some(body) = &mut metadata {
                body.extend_from_slice(&buffer[..count]);
            }
        }
        require(read == bytes, "truncated package member")?;
        require(
            files
                .insert(
                    name,
                    Member {
                        digest: hex::encode(hash.finalize()),
                        mode,
                        bytes,
                        metadata,
                    },
                )
                .is_none(),
            "duplicate package member",
        )?;
    }
    Ok(files)
}

pub fn source(root: &Path, version: &str, path: &Path) -> Result<()> {
    let listing = crate::command(root, "git", &["ls-files", "--stage"])?;
    require(
        listing
            .lines()
            .all(|line| matches!(line.split_whitespace().next(), Some("100644" | "100755"))),
        "source contains a link or gitlink",
    )?;
    let output = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)?;
    let mut zipped = GzBuilder::new()
        .mtime(0)
        .operating_system(255)
        .write(output, Compression::default());
    let mut child = Command::new("git")
        .args([
            "archive",
            "--format=tar",
            &format!("--prefix=nddev-remote-{version}/"),
            "HEAD",
        ])
        .current_dir(root)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()?;
    let copied = std::io::copy(
        &mut child
            .stdout
            .take()
            .ok_or(Error::Refused("missing source archive pipe"))?,
        &mut zipped,
    );
    if copied.is_err() {
        let _ = child.kill();
    }
    let status = child.wait()?;
    copied?;
    require(status.success(), "git archive failed")?;
    zipped.finish()?.flush()?;
    Ok(())
}

pub fn extract_source(path: &Path, destination: &Path) -> Result<()> {
    fs::create_dir(destination)?;
    let mut tar = tar::Archive::new(GzDecoder::new(fs::File::open(path)?));
    for entry in tar.entries()? {
        let mut entry = entry?;
        require(
            entry.header().entry_type().is_file() || entry.header().entry_type().is_dir(),
            "nonregular source member",
        )?;
        require(
            entry.unpack_in(destination)?,
            "source archive escaped destination",
        )?;
    }
    Ok(())
}
