//! Native NSURL selection authority follows every selected FD and reader.
use std::{os::unix::ffi::OsStrExt, path::PathBuf, ptr::NonNull, sync::Arc};

use objc2::rc::Retained;
use objc2_foundation::NSURL;

use crate::clipboard::{
    NativeLifetime,
    native::{Error, Policy, Value},
};

// NSURL is immutable and the binding implements Send + Sync. Unlike mutable
// NSPasteboard, its balanced resource scope may follow an owned file reader.
struct Resource {
    url: Retained<NSURL>,
    accessing: bool,
}
impl Drop for Resource {
    fn drop(&mut self) {
        if self.accessing {
            // SAFETY: exactly one successful start belongs to this retained
            // URL; every descriptor/native reader drops before this scope.
            unsafe { self.url.stopAccessingSecurityScopedResource() };
        }
    }
}
struct Authority {
    resources: Vec<Resource>,
    _caller: Option<NativeLifetime>,
}

pub(super) fn select(
    urls: Vec<Retained<NSURL>>,
    policy: &Policy,
    caller: Option<NativeLifetime>,
) -> Result<Value, Error> {
    if urls.is_empty() || urls.len() > policy.limits.files as usize {
        return Err(rds_core::clipboard::Error::TooLarge.into());
    }
    let mut authority = Authority {
        resources: Vec::with_capacity(urls.len()),
        _caller: caller,
    };
    for url in urls {
        if !url.isFileURL() {
            return Err(Error::Unsupported);
        }
        // SAFETY: this NSURL comes from native pasteboard object reading. A
        // successful start is balanced by Resource::drop, including errors.
        // Unscoped file URLs use the process's ordinary filesystem authority.
        let accessing = unsafe { url.startAccessingSecurityScopedResource() };
        authority.resources.push(Resource { url, accessing });
    }
    let maximum = usize::try_from(policy.native_uri_bytes).map_err(|_| Error::Settings)?;
    let mut buffer = vec![0u8; maximum + 1];
    let mut paths = Vec::with_capacity(authority.resources.len());
    let mut total = 0usize;
    for resource in &authority.resources {
        let pointer = NonNull::new(buffer.as_mut_ptr().cast()).ok_or(Error::Settings)?;
        // SAFETY: buffer is live, writable and exactly the provided length.
        // Foundation converts its native URL without a string-URL fallback.
        if !unsafe {
            resource
                .url
                .getFileSystemRepresentation_maxLength(pointer, buffer.len())
        } {
            return Err(rds_core::clipboard::Error::TooLarge.into());
        }
        let length = buffer
            .iter()
            .position(|byte| *byte == 0)
            .ok_or(Error::Unavailable)?;
        total = total
            .checked_add(length)
            .ok_or(rds_core::clipboard::Error::TooLarge)?;
        if total > maximum {
            return Err(rds_core::clipboard::Error::TooLarge.into());
        }
        paths.push(PathBuf::from(std::ffi::OsStr::from_bytes(
            &buffer[..length],
        )));
    }
    let selection = rds_sync::selection::Selection::from_paths_with_authority(
        paths,
        &policy.limits,
        Arc::new(authority),
    )?;
    Ok(Value::Files(Arc::new(selection)))
}

#[cfg(all(feature = "viewer", feature = "host"))]
pub(super) async fn probe() -> Result<u32, Error> {
    use objc2::runtime::ProtocolObject;
    use objc2_app_kit::NSPasteboardWriting;
    use objc2_foundation::{NSArray, NSString};
    use tokio::io::AsyncReadExt;

    struct Directory(PathBuf);
    impl Drop for Directory {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    let directory = Directory(std::env::temp_dir().join(format!(
        "nddev-clipboard-file-{:032x}",
        rand::random::<u128>()
    )));
    std::fs::create_dir(&directory.0)?;
    let source_path = directory.0.join("Привет.txt");
    let expected = "Native selected file 🖥️\n".repeat(1000).into_bytes();
    std::fs::write(&source_path, &expected)?;
    let board = super::Board::isolated();
    let policy = Policy::default();
    let url = NSURL::fileURLWithPath(&NSString::from_str(
        source_path.to_str().ok_or(Error::Unavailable)?,
    ));
    let object: &ProtocolObject<dyn NSPasteboardWriting> = ProtocolObject::from_ref(&*url);
    let _access = super::ACCESS.lock().map_err(|_| Error::Unavailable)?;
    board.board.clearContents();
    if !board.board.writeObjects(&NSArray::from_slice(&[object])) {
        return Err(Error::Publication);
    }
    drop(_access);
    let generation = board.generation()?;
    let authority = Arc::new(());
    let retained = Arc::downgrade(&authority);
    let Value::Files(files) = board.read(
        generation,
        rds_core::clipboard::Format::Files,
        &policy,
        Some(authority),
    )?
    else {
        return Err(Error::Unsupported);
    };
    if files.selection().entries.len() != 1 || files.selection().entries[0].name != "Привет.txt"
    {
        return Err(Error::Publication);
    }
    let id = files.selection().entries[0].id;
    let mut read = files.open(id).await?;
    drop(files);
    // Moving/replacing the pathname must neither change the selected object
    // nor discard the native scope while its opened reader still runs.
    std::fs::rename(&source_path, directory.0.join("moved"))?;
    std::fs::write(&source_path, b"unselected replacement")?;
    drop(board);
    if retained.upgrade().is_none() {
        return Err(Error::Publication);
    }
    let mut actual = Vec::new();
    read.reader.read_to_end(&mut actual).await?;
    read.complete.await?;
    drop(read.reader);
    if actual != expected || retained.upgrade().is_some() {
        return Err(Error::Publication);
    }
    Ok(1)
}
