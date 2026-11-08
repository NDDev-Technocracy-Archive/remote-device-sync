//! A new, private, size-capped file for one CLI process. Never append to or
//! replace an existing path. Remote stdout/stderr never enter this writer.
use rustix::fs::{Mode, OFlags, openat};
use std::{
    fs::File,
    io::{self, Write},
    os::unix::fs::MetadataExt,
    path::{Component, Path},
};

const MAX_BYTES: usize = 8 * 1024 * 1024;

pub struct PrivateLog {
    file: File,
    written: usize,
}

impl PrivateLog {
    pub fn create(path: &Path) -> io::Result<Self> {
        let denied = || {
            io::Error::new(
                io::ErrorKind::PermissionDenied,
                "log requires an absolute path in an existing private directory without symlink components",
            )
        };
        if !path.is_absolute() {
            return Err(denied());
        }
        let parts: Vec<_> = path.components().collect();
        if parts.len() < 3 {
            return Err(denied());
        }
        let mut directory = File::open("/")?;
        let uid = rustix::process::geteuid().as_raw();
        for (i, part) in parts.iter().enumerate().skip(1).take(parts.len() - 2) {
            let Component::Normal(name) = part else {
                return Err(denied());
            };
            directory = File::from(openat(
                &directory,
                *name,
                OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::empty(),
            )?);
            let metadata = directory.metadata()?;
            let mode = metadata.mode();
            let owner = metadata.uid();
            if i == parts.len() - 2 {
                if owner != uid || mode & 0o777 != 0o700 {
                    return Err(denied());
                }
            } else if (owner != 0 && owner != uid)
                || (mode & 0o022 != 0 && !(owner == 0 && mode & 0o1000 != 0))
            {
                return Err(denied());
            }
        }
        let Some(Component::Normal(name)) = parts.last() else {
            return Err(denied());
        };
        let file = File::from(openat(
            &directory,
            *name,
            OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::RUSR | Mode::WUSR,
        )?);
        Ok(Self { file, written: 0 })
    }
}

impl Write for PrivateLog {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.len() > MAX_BYTES - self.written {
            return Err(io::Error::other(
                "per-process log file reached its 8 MiB limit",
            ));
        }
        let count = self.file.write(bytes)?;
        self.written += count;
        Ok(count)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.file.flush()
    }
}

/// Automatic viewer diagnostics: private files, 8 MiB per part and at most
/// ten generated log parts in the directory. The bounded telemetry adapter
/// calls this writer off the UI/network threads.
pub struct ViewerLog {
    current: PrivateLog,
    directory: std::path::PathBuf,
}

impl ViewerLog {
    pub fn create() -> io::Result<(Self, std::path::PathBuf)> {
        use std::os::unix::fs::DirBuilderExt;
        let key = rds_net::default_key_path()
            .ok_or_else(|| io::Error::other("no viewer state directory"))?;
        let directory = key.with_file_name("viewer-logs");
        match std::fs::DirBuilder::new().mode(0o700).create(&directory) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(e),
        }
        let current = Self::part(&directory)?;
        // Preserve a bounded set of crash snapshots as well as log parts.
        let mut snapshots = Vec::new();
        for entry in std::fs::read_dir(&directory)? {
            let entry = entry?;
            let name = entry.file_name();
            let Some(name) = name.to_str() else {
                continue;
            };
            let Some(pid) = name
                .strip_prefix("state-")
                .and_then(|s| s.strip_suffix(".json"))
            else {
                continue;
            };
            if pid.parse::<u32>().is_err() {
                continue;
            }
            let meta = std::fs::symlink_metadata(entry.path())?;
            if meta.is_file()
                && meta.uid() == rustix::process::geteuid().as_raw()
                && meta.mode() & 0o777 == 0o600
                && meta.nlink() == 1
            {
                snapshots.push((meta.modified()?, entry.path(), meta.dev(), meta.ino()));
            }
        }
        snapshots.sort_by_key(|v| v.0);
        let remove = snapshots.len().saturating_sub(9);
        for (_, path, dev, ino) in snapshots.into_iter().take(remove) {
            let meta = std::fs::symlink_metadata(&path)?;
            if meta.is_file() && meta.dev() == dev && meta.ino() == ino && meta.nlink() == 1 {
                std::fs::remove_file(path)?;
            }
        }
        Ok((
            Self {
                current,
                directory: directory.clone(),
            },
            directory,
        ))
    }

    fn part(directory: &Path) -> io::Result<PrivateLog> {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(io::Error::other)?
            .as_nanos();
        let path = directory.join(format!("viewer-{stamp}-{}.log", std::process::id()));
        // This validates directory ownership/modes and every component before
        // retention examines anything. Existing files are never appended to.
        let file = PrivateLog::create(&path)?;
        let uid = rustix::process::geteuid().as_raw();
        let mut owned = Vec::new();
        for entry in std::fs::read_dir(directory)? {
            let entry = entry?;
            let name = entry.file_name();
            let Some(name) = name.to_str() else {
                continue;
            };
            let Some(rest) = name
                .strip_prefix("viewer-")
                .and_then(|n| n.strip_suffix(".log"))
            else {
                continue;
            };
            let Some((stamp, pid)) = rest.split_once('-') else {
                continue;
            };
            let (Ok(stamp), Ok(_pid)) = (stamp.parse::<u128>(), pid.parse::<u32>()) else {
                continue;
            };
            let meta = std::fs::symlink_metadata(entry.path())?;
            if meta.is_file()
                && meta.uid() == uid
                && meta.mode() & 0o777 == 0o600
                && meta.nlink() == 1
            {
                owned.push((stamp, entry.path(), meta.dev(), meta.ino()));
            }
        }
        owned.sort_by_key(|v| v.0);
        let remove = owned.len().saturating_sub(10);
        for (_, path, dev, ino) in owned.into_iter().take(remove) {
            let meta = std::fs::symlink_metadata(&path)?;
            if meta.is_file() && meta.dev() == dev && meta.ino() == ino && meta.nlink() == 1 {
                std::fs::remove_file(path)?;
            }
        }
        Ok(file)
    }
}

impl Write for ViewerLog {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.len() > MAX_BYTES - self.current.written {
            self.current.flush()?;
            self.current = Self::part(&self.directory)?;
        }
        self.current.write(bytes)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.current.flush()
    }
}

/// Publish a small live snapshot atomically. Never truncate an existing inode
/// (including a hard link) or follow a caller-provided symlink.
pub fn viewer_snapshot(path: &Path, bytes: &[u8]) -> io::Result<(u64, u64)> {
    if bytes.len() > 65536 {
        return Err(io::Error::other("viewer snapshot exceeds 64 KiB"));
    }
    if let Ok(meta) = std::fs::symlink_metadata(path)
        && (!meta.is_file()
            || meta.uid() != rustix::process::geteuid().as_raw()
            || meta.mode() & 0o777 != 0o600
            || meta.nlink() != 1)
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "unsafe existing viewer snapshot",
        ));
    }
    publish_private(path, true, |file| file.write_all(bytes))
}

/// An incomplete write never occupies the final JSON name. Failed writes clean
/// their owned temporary inode; new incidents use no-replace publication.
fn publish_private(
    path: &Path,
    replace: bool,
    write: impl FnOnce(&mut PrivateLog) -> io::Result<()>,
) -> io::Result<(u64, u64)> {
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(io::Error::other)?
        .as_nanos();
    let temporary = path.with_extension(format!("{stamp}.pending"));
    let mut file = PrivateLog::create(&temporary)?;
    let metadata = file.file.metadata()?;
    struct Pending {
        path: std::path::PathBuf,
        dev: u64,
        ino: u64,
    }
    impl Drop for Pending {
        fn drop(&mut self) {
            if let Ok(metadata) = std::fs::symlink_metadata(&self.path)
                && metadata.is_file()
                && metadata.dev() == self.dev
                && metadata.ino() == self.ino
            {
                let _ = std::fs::remove_file(&self.path);
            }
        }
    }
    let pending = Pending {
        path: temporary,
        dev: metadata.dev(),
        ino: metadata.ino(),
    };
    write(&mut file)?;
    file.flush()?;
    if replace {
        std::fs::rename(&pending.path, path)?;
    } else {
        std::fs::hard_link(&pending.path, path)?;
    }
    Ok((metadata.dev(), metadata.ino()))
}

/// Keep ten completed fault windows independently of ordinary log rotation.
pub fn viewer_incident(directory: &Path, bytes: &[u8], retained: usize) -> io::Result<()> {
    if !(1..=crate::desktop::observation::MAX_RETAINED_INCIDENTS).contains(&retained) {
        return Err(io::Error::other("invalid viewer incident retention"));
    }
    if bytes.len() > 256 * 1024 {
        return Err(io::Error::other("viewer incident exceeds 256 KiB"));
    }
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(io::Error::other)?
        .as_nanos();
    let path = directory.join(format!("incident-{stamp}-{}.json", std::process::id()));
    publish_private(&path, false, |file| file.write_all(bytes))?;
    let mut owned = Vec::new();
    for entry in std::fs::read_dir(directory)? {
        let entry = entry?;
        let name = entry.file_name();
        let Some((stamp, pid)) = name
            .to_str()
            .and_then(|n| n.strip_prefix("incident-"))
            .and_then(|n| n.strip_suffix(".json"))
            .and_then(|n| n.split_once('-'))
        else {
            continue;
        };
        let (Ok(stamp), Ok(_)) = (stamp.parse::<u128>(), pid.parse::<u32>()) else {
            continue;
        };
        let meta = std::fs::symlink_metadata(entry.path())?;
        if meta.is_file()
            && meta.uid() == rustix::process::geteuid().as_raw()
            && meta.mode() & 0o777 == 0o600
            && meta.nlink() == 1
        {
            owned.push((stamp, entry.path(), meta.dev(), meta.ino()));
        }
    }
    owned.sort_by_key(|item| item.0);
    let remove = owned.len().saturating_sub(retained);
    for (_, path, dev, ino) in owned.into_iter().take(remove) {
        let meta = std::fs::symlink_metadata(&path)?;
        if meta.is_file() && meta.dev() == dev && meta.ino() == ino && meta.nlink() == 1 {
            std::fs::remove_file(path)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::{DirBuilderExt, symlink};

    #[test]
    fn a_failed_partial_publication_leaves_no_json_or_pending_file() {
        let root = Path::new("/tmp")
            .canonicalize()
            .unwrap()
            .join(format!("rds-partial-{}", std::process::id()));
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&root)
            .unwrap();
        let path = root.join("incident.json");
        let failed = publish_private(&path, false, |file| {
            file.write_all(b"{\"partial\":")?;
            Err(io::Error::from_raw_os_error(28))
        });
        assert_eq!(failed.unwrap_err().kind(), io::ErrorKind::StorageFull);
        assert_eq!(std::fs::read_dir(&root).unwrap().count(), 0);
        publish_private(&path, false, |file| file.write_all(b"{\"complete\":true}")).unwrap();
        assert!(publish_private(&path, false, |file| file.write_all(b"overwrite")).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), b"{\"complete\":true}");
        assert_eq!(std::fs::metadata(&path).unwrap().nlink(), 1);
        assert_eq!(std::fs::read_dir(&root).unwrap().count(), 1);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn incident_retention_is_bounded_and_preserves_unowned_entries() {
        let root = Path::new("/tmp")
            .canonicalize()
            .unwrap()
            .join(format!("rds-incidents-{}", std::process::id()));
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&root)
            .unwrap();
        let sentinel = root.join("sentinel");
        std::fs::write(&sentinel, b"retain").unwrap();
        let alias = root.join("incident-0-1.json");
        symlink(&sentinel, &alias).unwrap();
        for _ in 0..12 {
            viewer_incident(&root, b"{}", 10).unwrap();
        }
        assert_eq!(
            std::fs::read_dir(&root)
                .unwrap()
                .filter(|entry| {
                    let entry = entry.as_ref().unwrap();
                    entry.file_name().to_string_lossy().starts_with("incident-")
                        && std::fs::symlink_metadata(entry.path()).unwrap().is_file()
                })
                .count(),
            10
        );
        assert_eq!(std::fs::read(&alias).unwrap(), b"retain");
        assert!(viewer_incident(&root, &vec![0; 256 * 1024 + 1], 10).is_err());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn log_is_private_capped_and_never_replaces_existing_entries() {
        let root = Path::new("/tmp")
            .canonicalize()
            .unwrap()
            .join(format!("rds-log-{}", std::process::id()));
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&root)
            .unwrap();
        struct Cleanup(std::path::PathBuf);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        let _cleanup = Cleanup(root.clone());
        let path = root.join("log");
        let mut writer = PrivateLog::create(&path).unwrap();
        writer.write_all(&vec![b'x'; MAX_BYTES]).unwrap();
        assert!(writer.write_all(b"one byte too far").is_err());
        assert_eq!(std::fs::metadata(&path).unwrap().len(), MAX_BYTES as u64);
        assert_eq!(std::fs::metadata(&path).unwrap().mode() & 0o777, 0o600);
        assert!(PrivateLog::create(&path).is_err());
        let link = root.join("link");
        symlink(&path, &link).unwrap();
        assert!(PrivateLog::create(&link).is_err());
        let alias = root.join("alias");
        symlink(&root, &alias).unwrap();
        assert!(PrivateLog::create(&alias.join("other")).is_err());
        assert!(!root.join("other").exists());
        assert!(PrivateLog::create(Path::new("relative.jsonl")).is_err());
    }
    #[test]
    fn snapshot_never_truncates_a_hardlink_or_follows_a_symlink() {
        use std::os::unix::fs::DirBuilderExt;
        let root = Path::new("/tmp")
            .canonicalize()
            .unwrap()
            .join(format!("rds-snapshot-{}", std::process::id()));
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&root)
            .unwrap();
        let original = root.join("original");
        let mut writer = PrivateLog::create(&original).unwrap();
        writer.write_all(b"preserve me").unwrap();
        let snapshot = root.join("state.json");
        std::fs::hard_link(&original, &snapshot).unwrap();
        assert!(viewer_snapshot(&snapshot, b"new state").is_err());
        assert_eq!(std::fs::read(&original).unwrap(), b"preserve me");
        std::fs::remove_file(&snapshot).unwrap();
        symlink(&original, &snapshot).unwrap();
        assert!(viewer_snapshot(&snapshot, b"new state").is_err());
        std::fs::remove_file(&snapshot).unwrap();
        viewer_snapshot(&snapshot, b"first").unwrap();
        let prior = File::open(&snapshot).unwrap();
        viewer_snapshot(&snapshot, b"second").unwrap();
        assert_eq!(std::fs::read(&snapshot).unwrap(), b"second");
        use std::io::Read;
        let mut bytes = Vec::new();
        let mut prior = prior;
        prior.read_to_end(&mut bytes).unwrap();
        assert_eq!(bytes, b"first");
        assert_eq!(std::fs::metadata(&snapshot).unwrap().mode() & 0o777, 0o600);
        std::fs::remove_dir_all(root).unwrap();
    }
}
