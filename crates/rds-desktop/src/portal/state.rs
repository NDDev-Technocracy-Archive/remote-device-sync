//! Private persistent consent and display numbering; code never logs tokens.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::fs::File;
use std::io::{Read, Write};
use std::os::fd::OwnedFd;
use std::path::{Component, Path};

use rustix::fs::{AtFlags, FileType, FlockOperation, Mode, OFlags};
use serde::{Deserialize, Serialize};

use crate::DesktopError;

const MAX_BYTES: u64 = 32 * 1024;
const MAX_IDS: usize = 64;

#[derive(Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Saved {
    schema_version: u32,
    restore_token: Option<String>,
    displays: BTreeMap<String, u32>,
    next_index: u32,
}

pub(super) struct Store {
    dir: OwnedFd,
    name: OsString,
    _lock: File,
    saved: Saved,
    previous: Option<Vec<u8>>,
}

impl Drop for Store {
    fn drop(&mut self) {
        // flock follows the open file description. An unrelated concurrent
        // spawn may inherit it briefly before CLOEXEC runs, so closing only
        // this FD does not necessarily end this owner's lock lifetime.
        if let Err(error) = rustix::fs::flock(&self._lock, FlockOperation::Unlock) {
            tracing::warn!(%error, "Wayland permission lock release failed");
        }
    }
}

fn error() -> DesktopError {
    DesktopError::Capture("unsafe, busy or invalid Wayland permission state".into())
}

fn private_file(fd: &OwnedFd) -> Result<(), DesktopError> {
    let stat = rustix::fs::fstat(fd).map_err(|_| error())?;
    if FileType::from_raw_mode(stat.st_mode) != FileType::RegularFile
        || stat.st_nlink != 1
        || stat.st_uid != rustix::process::geteuid().as_raw()
        || stat.st_mode & 0o7777 != 0o600
    {
        return Err(error());
    }
    Ok(())
}

fn read(dir: &OwnedFd, name: &OsString) -> Result<Option<Vec<u8>>, DesktopError> {
    let fd = match rustix::fs::openat(
        dir,
        name,
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NONBLOCK,
        Mode::empty(),
    ) {
        Ok(fd) => fd,
        Err(rustix::io::Errno::NOENT) => return Ok(None),
        Err(_) => return Err(error()),
    };
    private_file(&fd)?;
    let mut bytes = Vec::new();
    File::from(fd)
        .take(MAX_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| error())?;
    if bytes.len() as u64 > MAX_BYTES {
        return Err(error());
    }
    Ok(Some(bytes))
}

impl Store {
    pub fn open(path: &Path) -> Result<Self, DesktopError> {
        if !path.is_absolute() {
            return Err(error());
        }
        let name = path.file_name().ok_or_else(error)?.to_owned();
        let printable = name.to_str().ok_or_else(error)?;
        if printable.ends_with(".lock") || printable.starts_with(".rds-portal-") {
            return Err(error());
        }
        let parent = path.parent().ok_or_else(error)?;
        let mut dir = rustix::fs::open(
            "/",
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(|_| error())?;
        for component in parent.components() {
            let Component::Normal(name) = component else {
                if component == Component::RootDir {
                    continue;
                }
                return Err(error());
            };
            let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
            let next = match rustix::fs::openat(&dir, name, flags, Mode::empty()) {
                Ok(fd) => fd,
                Err(rustix::io::Errno::NOENT) => {
                    rustix::fs::mkdirat(&dir, name, Mode::from_raw_mode(0o700))
                        .map_err(|_| error())?;
                    rustix::fs::openat(&dir, name, flags, Mode::empty()).map_err(|_| error())?
                }
                Err(_) => return Err(error()),
            };
            dir = next;
        }
        let stat = rustix::fs::fstat(&dir).map_err(|_| error())?;
        if stat.st_uid != rustix::process::geteuid().as_raw() || stat.st_mode & 0o7777 != 0o700 {
            return Err(error());
        }
        let mut lock_name = name.clone();
        lock_name.push(".lock");
        let fd = rustix::fs::openat(
            &dir,
            &lock_name,
            OFlags::RDWR | OFlags::CREATE | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NONBLOCK,
            Mode::from_raw_mode(0o600),
        )
        .map_err(|_| error())?;
        private_file(&fd)?;
        rustix::fs::flock(&fd, FlockOperation::NonBlockingLockExclusive).map_err(|_| error())?;
        let previous = read(&dir, &name)?;
        let saved = match previous.as_ref() {
            Some(bytes) => serde_json::from_slice::<Saved>(bytes).map_err(|_| error())?,
            None => Saved {
                schema_version: 1,
                ..Default::default()
            },
        };
        if saved.schema_version != 1
            || saved.displays.len() > MAX_IDS
            || saved
                .restore_token
                .as_ref()
                .is_some_and(|t| t.is_empty() || t.len() > 4096)
            || saved
                .displays
                .keys()
                .any(|id| id.is_empty() || id.len() > 256)
            || saved.displays.values().any(|id| *id >= saved.next_index)
            || saved
                .displays
                .values()
                .copied()
                .collect::<BTreeSet<_>>()
                .len()
                != saved.displays.len()
        {
            return Err(error());
        }
        Ok(Self {
            dir,
            name,
            _lock: File::from(fd),
            saved,
            previous,
        })
    }

    pub fn token(&self) -> Option<&str> {
        self.saved.restore_token.as_deref()
    }

    pub fn index(&mut self, id: &str) -> Result<u32, DesktopError> {
        if let Some(index) = self.saved.displays.get(id) {
            return Ok(*index);
        }
        if id.is_empty() || id.len() > 256 || self.saved.displays.len() >= MAX_IDS {
            return Err(error());
        }
        let index = self.saved.next_index;
        self.saved.next_index = index.checked_add(1).ok_or_else(error)?;
        self.saved.displays.insert(id.into(), index);
        Ok(index)
    }

    pub fn save(&mut self, token: String) -> Result<(), DesktopError> {
        if token.is_empty() || token.len() > 4096 {
            return Err(error());
        }
        if read(&self.dir, &self.name)? != self.previous {
            return Err(error());
        }
        self.saved.restore_token = Some(token);
        let bytes = serde_json::to_vec(&self.saved).map_err(|_| error())?;
        if bytes.len() as u64 > MAX_BYTES {
            return Err(error());
        }
        let temp = OsString::from(format!(
            ".rds-portal-{:032x}.pending",
            rand::random::<u128>()
        ));
        let mut created = false;
        let result: Result<(), DesktopError> = (|| {
            let fd = rustix::fs::openat(
                &self.dir,
                &temp,
                OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::from_raw_mode(0o600),
            )
            .map_err(|_| error())?;
            created = true;
            let mut file = File::from(fd);
            file.write_all(&bytes).map_err(|_| error())?;
            file.sync_all().map_err(|_| error())?;
            rustix::fs::renameat(&self.dir, &temp, &self.dir, &self.name).map_err(|_| error())?;
            rustix::fs::fsync(&self.dir).map_err(|_| error())?;
            Ok(())
        })();
        if result.is_err() && created {
            let _ = rustix::fs::unlinkat(&self.dir, &temp, AtFlags::empty());
        }
        result?;
        self.previous = Some(bytes);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::{PermissionsExt, symlink};
    struct Directory(std::path::PathBuf);
    impl Directory {
        fn new() -> Self {
            let path = std::env::temp_dir()
                .join(format!("rds-portal-state-{:032x}", rand::random::<u128>()));
            std::fs::create_dir(&path).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
            Self(path)
        }
        fn state(&self) -> std::path::PathBuf {
            self.0.join("permission.json")
        }
    }
    impl Drop for Directory {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    #[test]
    fn restore_rotation_keeps_monitor_numbers_and_excludes_other_writers() {
        let dir = Directory::new();
        let mut state = Store::open(&dir.state()).unwrap();
        assert_eq!(state.index("4k").unwrap(), 0);
        assert_eq!(state.index("fhd").unwrap(), 1);
        state.save("first-test-token".into()).unwrap();
        assert!(Store::open(&dir.state()).is_err());
        drop(state);
        let mut state = Store::open(&dir.state()).unwrap();
        assert_eq!(state.token(), Some("first-test-token"));
        assert_eq!(state.index("fhd").unwrap(), 1);
        assert_eq!(state.index("4k").unwrap(), 0);
        assert_eq!(state.index("new-monitor").unwrap(), 2);
        state.save("rotated-test-token".into()).unwrap();
        drop(state);
        let state = Store::open(&dir.state()).unwrap();
        assert_eq!(state.token(), Some("rotated-test-token"));
        assert_eq!(
            std::fs::metadata(dir.state()).unwrap().permissions().mode() & 0o7777,
            0o600
        );
    }
    #[test]
    fn ending_the_store_releases_its_lock_despite_a_transient_inherited_descriptor() {
        let dir = Directory::new();
        let mut state = Store::open(&dir.state()).unwrap();
        state.save("retained-fixture-token".into()).unwrap();
        // A concurrent process spawn can inherit this open file description
        // until exec closes CLOEXEC descriptors. Duplication models that
        // native lifetime without a fork inside the multithreaded test runner.
        let inherited = state._lock.try_clone().unwrap();
        assert!(Store::open(&dir.state()).is_err());
        drop(state);
        let reopened = Store::open(&dir.state()).unwrap();
        assert_eq!(reopened.token(), Some("retained-fixture-token"));
        drop(inherited);
    }
    #[test]
    fn state_aliases_and_unexpected_changes_are_never_overwritten() {
        let dir = Directory::new();
        let mut state = Store::open(&dir.state()).unwrap();
        state.save("test-token".into()).unwrap();
        std::fs::write(dir.state(), b"unexpected").unwrap();
        assert!(state.save("next-token".into()).is_err());
        assert_eq!(std::fs::read(dir.state()).unwrap(), b"unexpected");
        drop(state);
        std::fs::remove_file(dir.state()).unwrap();
        symlink("elsewhere", dir.state()).unwrap();
        assert!(Store::open(&dir.state()).is_err());
        std::fs::remove_file(dir.state()).unwrap();
        let mut state = Store::open(&dir.state()).unwrap();
        state.save("test-token".into()).unwrap();
        drop(state);
        std::fs::hard_link(dir.state(), dir.0.join("alias")).unwrap();
        assert!(Store::open(&dir.state()).is_err());
    }
    #[test]
    fn private_directory_modes_and_reserved_lock_names_are_required() {
        let dir = Directory::new();
        assert!(Store::open(&dir.0.join("permission.lock")).is_err());
        assert!(Store::open(&dir.0.join(".rds-portal-test.pending")).is_err());
        std::fs::set_permissions(&dir.0, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(Store::open(&dir.state()).is_err());
    }
}
