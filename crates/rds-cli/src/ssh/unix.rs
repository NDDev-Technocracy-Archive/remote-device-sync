//! Safe OS adapters shared by the supported Linux and macOS targets.
use rds_ssh::{Pty, Size, Terminal};
use rustix::{
    fs::{Mode, OFlags},
    termios::{self, OptionalActions, Termios},
};
use std::{
    fs::File,
    io,
    os::fd::AsFd,
    os::unix::fs::MetadataExt,
    pin::Pin,
    task::{Context, Poll},
};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf, unix::AsyncFd};

pub fn read_key(
    path: &std::path::Path,
    secret: bool,
) -> anyhow::Result<zeroize::Zeroizing<String>> {
    use io::Read as _;
    let file = File::from(rustix::fs::open(
        path,
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
        Mode::empty(),
    )?);
    let metadata = file.metadata()?;
    let uid = rustix::process::geteuid().as_raw();
    anyhow::ensure!(
        metadata.is_file() && metadata.len() <= 65536,
        "SSH key must be a regular file of at most 64 KiB"
    );
    if secret {
        anyhow::ensure!(
            metadata.uid() == uid
                && metadata.nlink() == 1
                && matches!(metadata.mode() & 0o7777, 0o600 | 0o400),
            "SSH private key must be owned by this user, mode 0600/0400, with one link"
        );
    } else {
        anyhow::ensure!(
            (metadata.uid() == uid || metadata.uid() == 0) && metadata.mode() & 0o022 == 0,
            "SSH public key must be owned by this user or root and not writable by others"
        );
    }
    let mut content = zeroize::Zeroizing::new(String::new());
    file.take(65537).read_to_string(&mut content)?;
    anyhow::ensure!(content.len() <= 65536, "SSH key exceeds 64 KiB");
    Ok(content)
}

pub fn require_terminal() -> anyhow::Result<()> {
    use io::IsTerminal as _;
    anyhow::ensure!(
        io::stdin().is_terminal() && io::stdout().is_terminal(),
        "PTY requires terminal stdin/stdout; use --no-pty for pipes"
    );
    Ok(())
}

pub fn size() -> io::Result<Size> {
    let size = termios::tcgetwinsize(io::stdin())?;
    Ok(Size {
        columns: u32::from(size.ws_col.max(1)),
        rows: u32::from(size.ws_row.max(1)),
    })
}

pub struct RawTerminal {
    fd: File,
    saved: Termios,
    active: bool,
}

impl RawTerminal {
    pub fn new() -> io::Result<Self> {
        let fd = File::from(rustix::io::dup(io::stdin())?);
        let saved = termios::tcgetattr(&fd)?;
        Ok(Self {
            fd,
            saved,
            active: false,
        })
    }
    pub fn request(&self) -> io::Result<Terminal> {
        Ok(Terminal {
            term: std::env::var("TERM").unwrap_or_else(|_| "xterm-256color".into()),
            size: size()?,
            modes: modes(&self.saved),
        })
    }
    pub fn enter(&mut self) -> io::Result<()> {
        let mut raw = self.saved.clone();
        raw.make_raw();
        termios::tcsetattr(&self.fd, OptionalActions::Now, &raw)?;
        self.active = true;
        Ok(())
    }
    pub fn restore(&mut self) -> io::Result<()> {
        if self.active {
            termios::tcsetattr(&self.fd, OptionalActions::Now, &self.saved)?;
            self.active = false;
        }
        Ok(())
    }
}

impl Drop for RawTerminal {
    fn drop(&mut self) {
        let _ = self.restore();
    }
}

fn modes(t: &Termios) -> Vec<(Pty, u32)> {
    use termios::{InputModes as I, LocalModes as L, OutputModes as O, SpecialCodeIndex as C};
    let mut modes = vec![
        (Pty::TTY_OP_ISPEED, t.input_speed()),
        (Pty::TTY_OP_OSPEED, t.output_speed()),
        (Pty::IUTF8, 1),
    ];
    for (pty, code) in [
        (Pty::VINTR, C::VINTR),
        (Pty::VQUIT, C::VQUIT),
        (Pty::VERASE, C::VERASE),
        (Pty::VKILL, C::VKILL),
        (Pty::VEOF, C::VEOF),
        (Pty::VSTART, C::VSTART),
        (Pty::VSTOP, C::VSTOP),
        (Pty::VSUSP, C::VSUSP),
        (Pty::VEOL, C::VEOL),
    ] {
        modes.push((pty, u32::from(t.special_codes[code])));
    }
    for (pty, flag) in [
        (Pty::ICRNL, I::ICRNL),
        (Pty::INLCR, I::INLCR),
        (Pty::IGNCR, I::IGNCR),
        (Pty::IXON, I::IXON),
        (Pty::IXOFF, I::IXOFF),
        (Pty::ISTRIP, I::ISTRIP),
    ] {
        modes.push((pty, u32::from(t.input_modes.contains(flag))));
    }
    for (pty, flag) in [
        (Pty::ISIG, L::ISIG),
        (Pty::ICANON, L::ICANON),
        (Pty::ECHO, L::ECHO),
        (Pty::ECHOE, L::ECHOE),
        (Pty::ECHOK, L::ECHOK),
        (Pty::ECHONL, L::ECHONL),
        (Pty::IEXTEN, L::IEXTEN),
        (Pty::NOFLSH, L::NOFLSH),
    ] {
        modes.push((pty, u32::from(t.local_modes.contains(flag))));
    }
    for (pty, flag) in [(Pty::OPOST, O::OPOST), (Pty::ONLCR, O::ONLCR)] {
        modes.push((pty, u32::from(t.output_modes.contains(flag))));
    }
    modes
}

/// Pipes/TTYs use cancellable readiness, never Tokio's uncancellable stdin
/// blocking reader. Preserve shared open-file-description flags on every exit.
pub struct Io {
    kind: Kind,
    flags: Option<(File, OFlags)>,
}
enum Kind {
    Ready(AsyncFd<File>),
    File(tokio::fs::File),
}

// Shells may duplicate the same open file description onto all three standard
// descriptors. Snapshot every original before any one is made nonblocking;
// restore after all adapters drop, including partially constructed ones.
pub struct StdioFlags(Vec<(File, OFlags)>);
impl StdioFlags {
    pub fn save() -> io::Result<Self> {
        let files = [
            File::from(rustix::io::dup(io::stdin())?),
            File::from(rustix::io::dup(io::stdout())?),
            File::from(rustix::io::dup(io::stderr())?),
        ];
        let mut flags = Vec::with_capacity(3);
        for file in files {
            let value = rustix::fs::fcntl_getfl(&file)?;
            flags.push((file, value));
        }
        Ok(Self(flags))
    }
}
impl Drop for StdioFlags {
    fn drop(&mut self) {
        for (file, flags) in &self.0 {
            let _ = rustix::fs::fcntl_setfl(file, *flags);
        }
    }
}

impl Io {
    pub fn new(fd: impl AsFd) -> io::Result<Self> {
        let file = File::from(rustix::io::dup(fd)?);
        if file.metadata()?.is_file() {
            return Ok(Self {
                kind: Kind::File(tokio::fs::File::from_std(file)),
                flags: None,
            });
        }
        let flags = rustix::fs::fcntl_getfl(&file)?;
        let registered = file.try_clone()?;
        rustix::fs::fcntl_setfl(&file, flags | OFlags::NONBLOCK)?;
        // SAFETY: this owned File keeps its descriptor open and unchanged for
        // AsyncFd's lifetime; readiness operations only borrow it via get_ref.
        #[allow(unsafe_code)]
        let registration = unsafe { AsyncFd::register(registered) }.map_err(io::Error::from);
        match registration {
            Ok(ready) => Ok(Self {
                kind: Kind::Ready(ready),
                flags: Some((file, flags)),
            }),
            Err(error) => {
                rustix::fs::fcntl_setfl(&file, flags)?;
                // Only /dev/null may use the character-device fallback. An
                // arbitrary device could block a Tokio filesystem worker.
                let metadata = file.metadata()?;
                if !metadata.file_type().is_char_device()
                    || metadata.rdev() != std::fs::metadata("/dev/null")?.rdev()
                {
                    return Err(error);
                }
                Ok(Self {
                    kind: Kind::File(tokio::fs::File::from_std(file)),
                    flags: None,
                })
            }
        }
    }
}
use std::os::unix::fs::FileTypeExt as _;

impl Drop for Io {
    fn drop(&mut self) {
        if let Some((file, flags)) = &self.flags {
            let _ = rustix::fs::fcntl_setfl(file, *flags);
        }
    }
}

impl AsyncRead for Io {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        match &mut self.kind {
            Kind::File(file) => Pin::new(file).poll_read(cx, buf),
            Kind::Ready(fd) => loop {
                let mut ready = std::task::ready!(fd.poll_read_ready(cx))?;
                match ready.try_io(|inner| {
                    rustix::io::read(inner.get_ref(), buf.initialize_unfilled()).map_err(Into::into)
                }) {
                    Ok(result) => {
                        buf.advance(result?);
                        return Poll::Ready(Ok(()));
                    }
                    Err(_) => continue,
                }
            },
        }
    }
}

impl AsyncWrite for Io {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        match &mut self.kind {
            Kind::File(file) => Pin::new(file).poll_write(cx, buf),
            Kind::Ready(fd) => loop {
                let mut ready = std::task::ready!(fd.poll_write_ready(cx))?;
                match ready
                    .try_io(|inner| rustix::io::write(inner.get_ref(), buf).map_err(Into::into))
                {
                    Ok(result) => return Poll::Ready(result),
                    Err(_) => continue,
                }
            },
        }
    }
    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match &mut self.kind {
            Kind::File(file) => Pin::new(file).poll_flush(cx),
            Kind::Ready(_) => Poll::Ready(Ok(())),
        }
    }
    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.poll_flush(cx)
    }
}

pub fn resize_signal() -> io::Result<tokio::signal::unix::Signal> {
    tokio::signal::unix::signal(tokio::signal::unix::SignalKind::window_change())
}

pub struct Signals {
    interrupt: tokio::signal::unix::Signal,
    terminate: tokio::signal::unix::Signal,
    hangup: tokio::signal::unix::Signal,
    quit: tokio::signal::unix::Signal,
    suspend: tokio::signal::unix::Signal,
}
impl Signals {
    pub fn new() -> io::Result<Self> {
        use tokio::signal::unix::{SignalKind as K, signal};
        Ok(Self {
            interrupt: signal(K::interrupt())?,
            terminate: signal(K::terminate())?,
            hangup: signal(K::hangup())?,
            quit: signal(K::quit())?,
            suspend: signal(K::from_raw(rustix::process::Signal::TSTP.as_raw()))?,
        })
    }
    pub async fn cancelled(&mut self) -> u8 {
        // SIGTSTP is 20 on Linux and 18 on Darwin; use the OS constant.
        tokio::select! { _ = self.interrupt.recv() => 130, _ = self.terminate.recv() => 143, _ = self.hangup.recv() => 129, _ = self.quit.recv() => 131, _ = self.suspend.recv() => 128 + rustix::process::Signal::TSTP.as_raw() as u8 }
    }
}

#[cfg(test)]
mod io_tests {
    use super::*;
    use std::io::Write as _;
    use std::os::unix::net::UnixStream;
    use tokio::io::AsyncReadExt as _;

    #[tokio::test]
    async fn registration_owns_its_descriptor_after_the_callers_file_closes_and_restores_flags() {
        let (reader, mut writer) = UnixStream::pair().unwrap();
        let shared = reader.try_clone().unwrap();
        let flags = rustix::fs::fcntl_getfl(&shared).unwrap();
        let mut registered = Io::new(&reader).unwrap();
        assert!(
            rustix::fs::fcntl_getfl(&shared)
                .unwrap()
                .contains(OFlags::NONBLOCK)
        );
        drop(reader);
        writer.write_all(b"owned").unwrap();
        let mut data = [0; 5];
        tokio::time::timeout(
            std::time::Duration::from_secs(1),
            registered.read_exact(&mut data),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(&data, b"owned");
        drop(registered);
        assert_eq!(rustix::fs::fcntl_getfl(&shared).unwrap(), flags);
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn registration_error_restores_flags_and_keeps_only_the_existing_dev_null_fallback() {
        let null = File::open("/dev/null").unwrap();
        let null_flags = rustix::fs::fcntl_getfl(&null).unwrap();
        let mut registered = Io::new(&null).unwrap();
        assert!(matches!(&registered.kind, Kind::File(_)));
        assert_eq!(rustix::fs::fcntl_getfl(&null).unwrap(), null_flags);
        assert_eq!(registered.read(&mut [0; 1]).await.unwrap(), 0);

        let zero = File::open("/dev/zero").unwrap();
        let zero_flags = rustix::fs::fcntl_getfl(&zero).unwrap();
        let error = Io::new(&zero)
            .err()
            .expect("non-null character device must fail");
        assert_eq!(
            error.raw_os_error(),
            Some(rustix::io::Errno::PERM.raw_os_error())
        );
        assert_eq!(rustix::fs::fcntl_getfl(&zero).unwrap(), zero_flags);
    }
}
