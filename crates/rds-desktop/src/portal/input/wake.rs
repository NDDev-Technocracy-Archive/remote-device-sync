//! Coalesced local notification; carries no input or authorization payload.
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::sync::Arc;

use crate::DesktopError;

#[derive(Clone)]
pub(super) struct Wake(Arc<OwnedFd>);

impl Wake {
    pub(super) fn new() -> Result<Self, DesktopError> {
        rustix::event::eventfd(
            0,
            rustix::event::EventfdFlags::CLOEXEC | rustix::event::EventfdFlags::NONBLOCK,
        )
        .map(|fd| Self(Arc::new(fd)))
        .map_err(|_| super::error())
    }

    pub(super) fn notify(&self) -> Result<(), DesktopError> {
        let signal = 1_u64.to_ne_bytes();
        loop {
            match rustix::io::write(self, &signal) {
                Ok(count) if count == signal.len() => return Ok(()),
                // A saturated eventfd already has an outstanding notification.
                Err(rustix::io::Errno::AGAIN) => return Ok(()),
                Err(rustix::io::Errno::INTR) => {}
                _ => return Err(super::error()),
            }
        }
    }

    pub(super) fn drain(&self) -> Result<(), DesktopError> {
        let mut signal = 0_u64.to_ne_bytes();
        loop {
            match rustix::io::read(self, &mut signal) {
                Ok(count) if count == signal.len() => return Ok(()),
                Err(rustix::io::Errno::AGAIN) => return Ok(()),
                Err(rustix::io::Errno::INTR) => {}
                _ => return Err(super::error()),
            }
        }
    }
}

impl AsFd for Wake {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.0.as_ref().as_fd()
    }
}
