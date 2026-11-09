//! Verify every byte of the released, unmodified native design package.
use std::{collections::BTreeMap, fs, path::Path};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{Error, Result, digest, hex_identity, json, relative, require};

#[derive(Deserialize)]
struct Pin {
    schema_version: u32,
    repository: String,
    tag: String,
    commit: String,
    subdirectory: String,
    git_tree: String,
    directory: String,
    directory_sha256: String,
    files: Vec<File>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct File {
    path: String,
    sha256: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Receipt {
    pub repository: String,
    pub tag: String,
    pub commit: String,
    pub subdirectory: String,
    pub git_tree: String,
    pub directory_sha256: String,
}

fn collect(root: &Path, path: &Path, files: &mut BTreeMap<String, String>) -> Result<()> {
    require(files.len() <= 4096, "oversized design inventory")?;
    for entry in fs::read_dir(path)? {
        let entry = entry?;
        let path = entry.path();
        let metadata = fs::symlink_metadata(&path)?;
        if metadata.is_dir() {
            collect(root, &path, files)?;
        } else {
            require(
                metadata.is_file(),
                "design package contains a nonregular source",
            )?;
            let name = path
                .strip_prefix(root)
                .map_err(|_| Error::Refused("invalid design path"))?
                .to_str()
                .ok_or(Error::Refused("non-UTF-8 design path"))?
                .to_owned();
            require(relative(&name), "invalid design file path")?;
            require(
                files.insert(name, digest(&path)?).is_none(),
                "duplicate design path",
            )?;
            require(files.len() <= 4096, "oversized design inventory")?;
        }
    }
    Ok(())
}

pub fn verify(root: &Path) -> Result<Receipt> {
    let pin: Pin = json(&root.join("packaging/design-system.json"))?;
    let tag = pin
        .tag
        .strip_prefix('v')
        .ok_or(Error::Refused("invalid design tag"))?;
    let (version, suffix) = tag.split_once('-').unwrap_or((tag, ""));
    require(
        crate::release::numeric_version(version)
            && (suffix.is_empty()
                || suffix
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'.')),
        "invalid design tag",
    )?;
    require(
        pin.schema_version == 2
            && pin.repository == "NDDev-Technocracy-OpenNetwork/nddev-opennetwork-design-system"
            && pin.subdirectory == "packages/rust"
            && pin.directory == "vendor/nddev-opennetwork-design-tokens"
            && hex_identity(&pin.commit, 20)
            && hex_identity(&pin.git_tree, 20)
            && hex_identity(&pin.directory_sha256, 32),
        "invalid released design pin",
    )?;
    require(
        (1..=4096).contains(&pin.files.len()),
        "invalid design file records",
    )?;
    let mut expected = BTreeMap::new();
    for file in pin.files {
        require(
            relative(&file.path) && hex_identity(&file.sha256, 32),
            "invalid design file record",
        )?;
        require(
            expected.insert(file.path, file.sha256).is_none(),
            "duplicate design file record",
        )?;
    }
    let directory = root.join(pin.directory);
    require(
        fs::symlink_metadata(&directory)?.is_dir(),
        "design package is not a local directory",
    )?;
    let mut files = BTreeMap::new();
    collect(&directory, &directory, &mut files)?;
    require(
        files == expected,
        "released design inventory or bytes changed",
    )?;
    let mut hash = Sha256::new();
    for (name, value) in files {
        hash.update(name.as_bytes());
        hash.update([0]);
        hash.update(hex::decode(value).map_err(|_| Error::Refused("invalid design digest"))?);
    }
    require(
        hex::encode(hash.finalize()) == pin.directory_sha256,
        "design directory digest changed",
    )?;
    Ok(Receipt {
        repository: pin.repository,
        tag: pin.tag,
        commit: pin.commit,
        subdirectory: pin.subdirectory,
        git_tree: pin.git_tree,
        directory_sha256: pin.directory_sha256,
    })
}
