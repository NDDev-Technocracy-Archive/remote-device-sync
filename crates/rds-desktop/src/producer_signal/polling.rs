//! The X11 capture socket and its original steering owner share one native poll.
use rustix::{
    event::{EventfdFlags, eventfd},
    io::{Errno, retry_on_intr},
};
use std::{
    os::fd::{AsFd, BorrowedFd, OwnedFd},
    sync::atomic::{AtomicI32, Ordering},
};

pub(crate) struct Pulse {
    fd: OwnedFd,
    failed: AtomicI32,
}
impl Pulse {
    pub(crate) fn new() -> std::io::Result<Self> {
        Ok(Self {
            fd: eventfd(0, EventfdFlags::CLOEXEC | EventfdFlags::NONBLOCK)?,
            failed: AtomicI32::new(0),
        })
    }
    pub(crate) fn signal(&self) {
        match retry_on_intr(|| rustix::io::write(&self.fd, &1u64.to_ne_bytes())) {
            Ok(8) | Err(Errno::AGAIN) => {}
            Err(error) => {
                self.failed.store(error.raw_os_error(), Ordering::Release);
            }
            Ok(_) => {
                self.failed
                    .store(Errno::IO.raw_os_error(), Ordering::Release);
            }
        }
    }
    pub(crate) fn drain(&self) -> std::io::Result<()> {
        let failed = self.failed.load(Ordering::Acquire);
        if failed != 0 {
            return Err(std::io::Error::from_raw_os_error(failed));
        }
        match retry_on_intr(|| rustix::io::read(&self.fd, &mut [0u8; 8])) {
            Ok(8) | Err(Errno::AGAIN) => Ok(()),
            Err(error) => Err(error.into()),
            Ok(_) => Err(std::io::Error::other("incomplete native producer wake")),
        }
    }
}
impl AsFd for Pulse {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.fd.as_fd()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{sync::Arc, time::Duration};
    #[tokio::test]
    async fn native_poll_is_level_triggered_reused_and_owned_through_cancellation() {
        let controls = crate::ProducerControls::new(1);
        controls.idr.store(false, Ordering::Release);
        let pulse = controls.wake.polling().unwrap();
        assert!(Arc::ptr_eq(&pulse, &controls.wake.polling().unwrap()));
        for flag in [&controls.idr, &controls.input_refresh_pending] {
            pulse.drain().unwrap();
            flag.store(true, Ordering::Release);
            let held = pulse.clone();
            let poll = tokio::task::spawn_blocking(move || {
                let mut fds = [rustix::event::PollFd::new(
                    &*held,
                    rustix::event::PollFlags::IN,
                )];
                assert_eq!(
                    rustix::event::poll(
                        &mut fds,
                        Some(&rustix::event::Timespec {
                            tv_sec: 5,
                            tv_nsec: 0
                        })
                    )
                    .unwrap(),
                    1
                );
                held.drain().unwrap();
            });
            tokio::time::timeout(Duration::from_millis(250), poll)
                .await
                .unwrap()
                .unwrap();
            assert!(flag.swap(false, Ordering::AcqRel));
        }
        let retained = Arc::downgrade(&pulse);
        let held = pulse.clone();
        let (entered, ready) = tokio::sync::oneshot::channel();
        let poll = tokio::task::spawn_blocking(move || {
            entered.send(()).unwrap();
            let mut fds = [rustix::event::PollFd::new(
                &*held,
                rustix::event::PollFlags::IN,
            )];
            assert_eq!(
                rustix::event::poll(
                    &mut fds,
                    Some(&rustix::event::Timespec {
                        tv_sec: 5,
                        tv_nsec: 0
                    })
                )
                .unwrap(),
                1
            );
            held.drain().unwrap();
        });
        ready.await.unwrap();
        drop(super::super::Stop(controls.wake.clone()));
        drop(pulse);
        tokio::time::timeout(Duration::from_millis(250), poll)
            .await
            .unwrap()
            .unwrap();
        assert!(
            retained.upgrade().is_none(),
            "a closed original owner retained its native wake FD"
        );
    }
}
