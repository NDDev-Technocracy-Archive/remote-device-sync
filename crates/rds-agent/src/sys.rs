//! Process resource observation for admission budgets (W2.5).
//!
//! Platform coverage is honest: an unobservable quantity reports `None`
//! and the corresponding gate stays open rather than pretending a bound.

/// Open file descriptors owned by this process, where the kernel exposes
/// them (`/proc/self/fd` on Linux, `/dev/fd` on macOS).
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub(crate) fn open_fds() -> Option<usize> {
    #[cfg(target_os = "linux")]
    const FD_DIR: &str = "/proc/self/fd";
    #[cfg(target_os = "macos")]
    const FD_DIR: &str = "/dev/fd";
    Some(std::fs::read_dir(FD_DIR).ok()?.count())
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub(crate) fn open_fds() -> Option<usize> {
    None
}

/// Resident set size in bytes.
#[cfg(target_os = "linux")]
pub(crate) fn rss_bytes() -> Option<u64> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    let line = status.lines().find(|l| l.starts_with("VmRSS:"))?;
    let kb: u64 = line
        .trim_start_matches("VmRSS:")
        .trim_end_matches("kB")
        .trim()
        .parse()
        .ok()?;
    kb.checked_mul(1024)
}

/// Resident set size in bytes via `proc_pidinfo` (`PROC_PIDTASKINFO`).
#[cfg(target_os = "macos")]
#[allow(unsafe_code)]
pub(crate) fn rss_bytes() -> Option<u64> {
    use std::mem::{MaybeUninit, size_of};
    let mut info = MaybeUninit::<libc::proc_taskinfo>::uninit();
    // SAFETY: `info` is live, correctly aligned taskinfo storage for the
    // PROC_PIDTASKINFO request; the call fills it on a full-size return.
    let size = unsafe {
        libc::proc_pidinfo(
            libc::getpid(),
            libc::PROC_PIDTASKINFO,
            0,
            info.as_mut_ptr().cast(),
            size_of::<libc::proc_taskinfo>() as i32,
        )
    };
    if size == size_of::<libc::proc_taskinfo>() as i32 {
        // SAFETY: proc_pidinfo wrote exactly one complete taskinfo record.
        Some(unsafe { info.assume_init() }.pti_resident_size)
    } else {
        None
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub(crate) fn rss_bytes() -> Option<u64> {
    None
}

#[cfg(all(test, any(target_os = "linux", target_os = "macos")))]
mod tests {
    use super::*;

    #[test]
    fn open_fds_reports_a_running_process() {
        // stdin/stdout/stderr at minimum; a test process holds more.
        assert!(open_fds().expect("kernel reports fds") >= 3);
    }

    #[test]
    fn rss_reports_a_running_process() {
        assert!(rss_bytes().expect("kernel reports RSS") > 0);
    }
}
