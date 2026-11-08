//! Unix filesystem and peer-credential boundary; no custom unsafe code.
use std::fs::File;
use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};
use std::path::{Component, Path, PathBuf};

use rustix::fs::{Mode, OFlags, openat};
use tokio::net::{UnixListener, UnixStream};

use super::Error;

pub const SOCKET: &str = "control.sock";

/// All components must be real directories owned by root or this user. A
/// root-owned sticky ancestor (e.g. /tmp) is allowed; the leaf must be private.
pub(super) fn directory(path: &Path, create: bool) -> Result<File, Error> {
    if !path.is_absolute() || path == Path::new("/") {
        return Err(Error::UnsafeDirectory);
    }
    let mut current = File::open("/")?;
    let parts: Vec<_> = path.components().collect();
    for (i, part) in parts.iter().enumerate().skip(1) {
        let Component::Normal(name) = part else {
            return Err(Error::UnsafeDirectory);
        };
        let last = i + 1 == parts.len();
        if create {
            match rustix::fs::mkdirat(&current, *name, Mode::RWXU) {
                Ok(()) | Err(rustix::io::Errno::EXIST) => {}
                Err(e) => return Err(std::io::Error::from(e).into()),
            }
        }
        current = File::from(
            openat(
                &current,
                *name,
                OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::empty(),
            )
            .map_err(std::io::Error::from)?,
        );
        let metadata = current.metadata()?;
        let uid = rustix::process::geteuid().as_raw();
        let owner = metadata.uid();
        let mode = metadata.mode();
        if last {
            if owner != uid || mode & 0o777 != 0o700 {
                return Err(Error::UnsafeDirectory);
            }
        } else if (owner != 0 && owner != uid)
            || (mode & 0o022 != 0 && !(owner == 0 && mode & 0o1000 != 0))
        {
            return Err(Error::UnsafeDirectory);
        }
    }
    Ok(current)
}

fn socket_meta(path: &Path) -> Result<std::fs::Metadata, Error> {
    let m = std::fs::symlink_metadata(path)?;
    if !m.file_type().is_socket() || m.uid() != rustix::process::geteuid().as_raw() {
        return Err(Error::UnsafeSocket);
    }
    Ok(m)
}

pub fn authenticate(stream: &UnixStream) -> Result<(), Error> {
    if stream.peer_cred()?.uid() != rustix::process::geteuid().as_raw() {
        return Err(Error::PeerIdentity);
    }
    Ok(())
}

/// Dedicated directory ownership serializes servers and stale-socket recovery.
pub struct SocketOwner {
    directory: File,
    path: PathBuf,
    inode: Option<(u64, u64)>,
}

impl SocketOwner {
    pub async fn bind(path: PathBuf) -> Result<(Self, UnixListener), Error> {
        // Reject unsupported addresses before creating a directory or lock.
        std::os::unix::net::SocketAddr::from_pathname(path.join(SOCKET))?;
        let checked = path.clone();
        let directory = tokio::task::spawn_blocking(move || {
            let directory = directory(&checked, true)?;
            directory.try_lock().map_err(|_| Error::AlreadyRunning)?;
            Ok::<_, Error>(directory)
        })
        .await
        .map_err(Error::Worker)??;
        let mut owner = Self {
            directory,
            path: path.join(SOCKET),
            inode: None,
        };
        match socket_meta(&owner.path) {
            Ok(old) => {
                // A lock-unaware live listener is not ours to unlink either.
                match tokio::time::timeout(super::PRELUDE_TIMEOUT, UnixStream::connect(&owner.path))
                    .await
                {
                    Ok(Err(e)) if e.kind() == std::io::ErrorKind::ConnectionRefused => {}
                    _ => return Err(Error::AlreadyRunning),
                }
                let now = socket_meta(&owner.path)?;
                if (old.dev(), old.ino()) != (now.dev(), now.ino()) {
                    return Err(Error::UnsafeSocket);
                }
                std::fs::remove_file(&owner.path)?;
            }
            Err(Error::Io(e)) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
        let listener = UnixListener::bind(&owner.path)?;
        let metadata = socket_meta(&owner.path)?;
        owner.inode = Some((metadata.dev(), metadata.ino()));
        std::fs::set_permissions(&owner.path, std::fs::Permissions::from_mode(0o600))?;
        Ok((owner, listener))
    }
}

impl Drop for SocketOwner {
    fn drop(&mut self) {
        if let Ok(m) = socket_meta(&self.path)
            && self.inode == Some((m.dev(), m.ino()))
        {
            let _ = std::fs::remove_file(&self.path);
        }
        let _ = self.directory.unlock();
    }
}

pub async fn connect(path: &Path) -> Result<UnixStream, Error> {
    let checked = path.to_path_buf();
    tokio::task::spawn_blocking(move || {
        let _directory = directory(&checked, false)?;
        let m = socket_meta(&checked.join(SOCKET))?;
        if m.mode() & 0o777 != 0o600 {
            return Err(Error::UnsafeSocket);
        }
        Ok::<_, Error>(())
    })
    .await
    .map_err(Error::Worker)??;
    let stream = UnixStream::connect(path.join(SOCKET)).await?;
    authenticate(&stream)?;
    Ok(stream)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn unsupported_socket_address_creates_no_partial_directory() {
        let root = Path::new("/tmp")
            .canonicalize()
            .unwrap()
            .join(format!("nddev-address-{:x}", rand::random::<u64>()));
        let path = root.join("long".repeat(32));
        assert!(SocketOwner::bind(path.clone()).await.is_err());
        assert!(!root.exists());
        assert!(!path.exists());
    }
}
