//! Filesystem capabilities for Linux/macOS sync roots. Every untrusted path
//! component is opened relative to a held directory with NOFOLLOW. A renamed
//! directory remains the same capability; paths are never re-resolved for I/O.
//! The configured root's ancestors and processes with the same OS identity are
//! trusted. This is not a sandbox against a local process moving held inodes.

use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::io::{self, Read};
use std::os::unix::fs::PermissionsExt;
use std::path::{Component, Path};
use std::sync::Arc;

use rustix::fs::{AtFlags, Mode, OFlags, mkdirat, openat, renameat, unlinkat};

use crate::fault::{Point, hit};

/// Reserved temporary name, used only below private state while locked.
pub(crate) const PENDING: &str = "pending";

#[derive(Clone)]
pub(crate) struct Directory(Arc<File>);

pub(crate) struct ReceiveLock(File);
impl Drop for ReceiveLock {
    fn drop(&mut self) {
        // Scope ownership must end even while a concurrent fork holds a
        // temporary descriptor alias before exec closes its CLOEXEC files.
        let _ = self.0.unlock();
    }
}

impl Directory {
    /// Only the configured, trusted root is opened by an absolute path.
    pub(crate) fn open_root(path: &Path, create: bool) -> io::Result<Self> {
        if create {
            std::fs::create_dir_all(path)?;
        }
        let fd = rustix::fs::open(
            path,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )?;
        Ok(Self(Arc::new(File::from(fd))))
    }

    pub(crate) fn child(&self, name: &OsStr, create: bool) -> io::Result<Self> {
        component(name)?;
        if create {
            match mkdirat(&*self.0, name, Mode::RUSR | Mode::WUSR | Mode::XUSR) {
                Ok(()) => self.sync()?,
                Err(rustix::io::Errno::EXIST) => {}
                Err(e) => return Err(e.into()),
            }
        }
        let fd = openat(
            &*self.0,
            name,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )?;
        Ok(Self(Arc::new(File::from(fd))))
    }

    pub(crate) fn parent(&self, rel: &Path, create: bool) -> io::Result<(Self, OsString)> {
        let mut parts = rel.components().peekable();
        let mut dir = self.clone();
        while let Some(part) = parts.next() {
            let Component::Normal(name) = part else {
                return Err(io::Error::other(
                    "sync path must be relative and normalized",
                ));
            };
            if parts.peek().is_none() {
                return Ok((dir, name.to_owned()));
            }
            let next = dir.child(name, create)?;
            {
                // Lexical ASCII checks cannot model every filesystem's case
                // or Unicode equivalence rules. Compare opened inodes before
                // traversing a directory alias of private state at any depth.
                match dir.child(crate::journal::STATE_DIR.as_ref(), false) {
                    Ok(state) => {
                        let actual = rustix::fs::fstat(&*next.0)?;
                        let reserved = rustix::fs::fstat(&*state.0)?;
                        if actual.st_dev == reserved.st_dev && actual.st_ino == reserved.st_ino {
                            return Err(io::Error::new(
                                io::ErrorKind::PermissionDenied,
                                "sync path enters the private journal",
                            ));
                        }
                    }
                    Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                    Err(e) => return Err(e),
                }
            }
            dir = next;
        }
        Err(io::Error::other("empty sync path"))
    }

    pub(crate) fn read_file(&self, name: &OsStr) -> io::Result<File> {
        component(name)?;
        // NONBLOCK prevents a planted FIFO from hanging before fstat.
        let fd = openat(
            &*self.0,
            name,
            OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
            Mode::empty(),
        )?;
        let file = File::from(fd);
        regular(&file)?;
        Ok(file)
    }

    pub(crate) fn read_path(&self, rel: &Path) -> io::Result<File> {
        let (parent, name) = self.parent(rel, false)?;
        parent.read_file(&name)
    }

    pub(crate) fn make_private(&self) -> io::Result<()> {
        // Existing pre-remediation journals may have inherited a permissive
        // umask. Change the opened internal directory, never a path lookup.
        self.0
            .set_permissions(std::fs::Permissions::from_mode(0o700))?;
        self.sync()
    }

    /// Entry names in this pinned directory — names only; nothing is
    /// resolved, opened or followed. The caller decides which names are
    /// attributable; `.`/`..` are never reported.
    pub(crate) fn children(&self) -> io::Result<Vec<OsString>> {
        use std::os::unix::ffi::OsStrExt;
        let mut dir = rustix::fs::Dir::read_from(&*self.0)?;
        let mut out = Vec::new();
        while let Some(entry) = dir.read() {
            let entry = entry?;
            let name = OsStr::from_bytes(entry.file_name().to_bytes());
            if name != "." && name != ".." {
                out.push(name.to_os_string());
            }
        }
        Ok(out)
    }

    pub(crate) fn same_inode(&self, other: &Self) -> io::Result<bool> {
        let a = rustix::fs::fstat(&*self.0)?;
        let b = rustix::fs::fstat(&*other.0)?;
        Ok(a.st_dev == b.st_dev && a.st_ino == b.st_ino)
    }

    /// State files cannot alias another file via a hard link either.
    pub(crate) fn read_state(&self, name: &OsStr, limit: usize) -> io::Result<Vec<u8>> {
        let file = self.read_file(name)?;
        single_link(&file)?;
        let mut bytes = Vec::with_capacity(limit);
        file.take(limit as u64 + 1).read_to_end(&mut bytes)?;
        Ok(bytes)
    }

    /// The lock inode is never removed: deleting a held lock would let a new
    /// opener create a second inode and bypass the existing owner's lock.
    pub(crate) fn lock(&self, name: &OsStr) -> io::Result<ReceiveLock> {
        component(name)?;
        let file = File::from(openat(
            &*self.0,
            name,
            OFlags::RDWR | OFlags::CREATE | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
            Mode::RUSR | Mode::WUSR,
        )?);
        regular(&file)?;
        single_link(&file)?;
        file.try_lock().map_err(io::Error::from)?;
        Ok(ReceiveLock(file))
    }

    /// Discard only a reserved private transaction name. Never sweep user
    /// directories, unknown entries or legacy random staging names. The caller
    /// must hold the receive lock that owns this directory.
    pub(crate) fn discard_owned(&self, name: &OsStr) -> io::Result<()> {
        match self.read_file(name) {
            Ok(file) => {
                single_link(&file)?;
                self.unlink(name, false)?;
                self.sync()
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e),
        }
    }

    pub(crate) fn stage_named(&self, name: OsString) -> io::Result<StagedFile> {
        component(&name)?;
        let file = File::from(openat(
            &*self.0,
            &name,
            OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::RUSR | Mode::WUSR,
        )?);
        let stage = StagedFile {
            file,
            dir: self.clone(),
            name,
            installed: false,
        };
        hit(Point::Created)?;
        Ok(stage)
    }

    pub(crate) fn write_state(&self, name: &OsStr, data: &[u8]) -> io::Result<()> {
        self.discard_owned(PENDING.as_ref())?;
        let mut stage = self.stage_named(PENDING.into())?;
        crate::fault::write(&mut stage.file, data)?;
        hit(Point::Written)?;
        stage.install(name)
    }

    pub(crate) fn unlink(&self, name: &OsStr, directory: bool) -> io::Result<()> {
        component(name)?;
        unlinkat(
            &*self.0,
            name,
            if directory {
                AtFlags::REMOVEDIR
            } else {
                AtFlags::empty()
            },
        )?;
        Ok(())
    }

    pub(crate) fn sync(&self) -> io::Result<()> {
        self.0.sync_all()
    }
}

/// Exclusively created staging inode in the final parent. Drop only unlinks
/// the name this instance created; collisions never remove existing entries.
pub(crate) struct StagedFile {
    pub(crate) file: File,
    dir: Directory,
    name: OsString,
    installed: bool,
}

impl StagedFile {
    pub(crate) fn install(self, name: &OsStr) -> io::Result<()> {
        let dir = self.dir.clone();
        self.install_in(&dir, name)
    }

    pub(crate) fn install_in(mut self, destination: &Directory, name: &OsStr) -> io::Result<()> {
        component(name)?;
        self.file.sync_all()?;
        hit(Point::FileSynced)?;
        renameat(&*self.dir.0, &self.name, &*destination.0, name)?;
        self.installed = true;
        hit(Point::Renamed)?;
        destination.sync()?;
        hit(Point::DestinationSynced)?;
        if !self.dir.same_inode(destination)? {
            self.dir.sync()?;
            hit(Point::SourceSynced)?;
        }
        Ok(())
    }
}

impl Drop for StagedFile {
    fn drop(&mut self) {
        if !self.installed {
            let _ = self.dir.unlink(&self.name, false);
            let _ = self.dir.sync();
        }
    }
}

fn regular(file: &File) -> io::Result<()> {
    if !file.metadata()?.is_file() {
        return Err(io::Error::other("sync requires a regular file"));
    }
    Ok(())
}

fn single_link(file: &File) -> io::Result<()> {
    if rustix::fs::fstat(file)?.st_nlink != 1 {
        return Err(io::Error::other("sync state must have one link"));
    }
    Ok(())
}

fn component(name: &OsStr) -> io::Result<()> {
    let mut components = Path::new(name).components();
    if !matches!(components.next(), Some(Component::Normal(n)) if n == name)
        || components.next().is_some()
    {
        return Err(io::Error::other("invalid filesystem component"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn receive_owner_drop_unlocks_despite_a_fork_style_descriptor_alias() {
        let path =
            std::env::temp_dir().join(format!("rds-receive-lock-{:032x}", rand::random::<u128>()));
        let dir = Directory::open_root(&path, true).unwrap();
        let name = OsStr::new("receive.lock");
        let owner = dir.lock(name).unwrap();
        let inherited = owner.0.try_clone().unwrap();
        assert!(dir.lock(name).is_err());
        drop(owner);
        let successor = dir.lock(name).unwrap();
        drop(inherited);
        assert!(dir.lock(name).is_err());
        drop(successor);
        assert!(
            path.join(name).exists(),
            "persistent lock inode must not be unlinked"
        );
        std::fs::remove_dir_all(path).unwrap();
    }

    #[test]
    fn collision_and_drop_do_not_unlink_other_entries() {
        let path =
            std::env::temp_dir().join(format!("rds-stage-test-{:032x}", rand::random::<u128>()));
        let dir = Directory::open_root(&path, true).unwrap();
        std::fs::write(path.join("occupied"), b"owned by user").unwrap();
        assert!(dir.stage_named("occupied".into()).is_err());
        assert_eq!(
            std::fs::read(path.join("occupied")).unwrap(),
            b"owned by user"
        );
        let stage = dir.stage_named(PENDING.into()).unwrap();
        let own = path.join(&stage.name);
        assert!(own.is_file());
        drop(stage);
        assert!(!own.exists());
        std::fs::remove_dir_all(path).unwrap();
    }

    #[test]
    fn opened_journal_directory_is_not_a_user_path() {
        let path = std::env::temp_dir().join(format!(
            "rds-namespace-test-{:032x}",
            rand::random::<u128>()
        ));
        let dir = Directory::open_root(&path, true).unwrap();
        dir.child(crate::journal::STATE_DIR.as_ref(), true).unwrap();
        // Exercise the handle-level check independently of lexical validation.
        assert!(
            dir.parent(Path::new(".rds-sync/receive.lock"), false)
                .is_err()
        );
        // This also exercises the alias on case-insensitive filesystems;
        // case-sensitive filesystems refuse the absent spelling instead.
        assert!(
            dir.parent(Path::new(".RDS-SYNC/receive.lock"), false)
                .is_err()
        );
        assert!(dir.parent(Path::new("ordinary/data.bin"), true).is_ok());
        std::fs::remove_dir_all(path).unwrap();
    }
}
