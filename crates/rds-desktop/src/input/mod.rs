//! Input injection backends, probed per platform/session type.
//!
//! Linux order: `portal` (libei/EIS via `reis`, compositor-respecting)
//! → `wlr` virtual protocols → `uinput` (privileged virtual device,
//! unattended/headless) → `x11` (XTEST). macOS: `macos` (CGEvent).

#[cfg(all(target_os = "linux", feature = "x11"))]
pub mod x11;

#[cfg(target_os = "linux")]
pub mod portal;
#[cfg(target_os = "linux")]
pub mod uinput;
#[cfg(target_os = "linux")]
pub mod wlr;

#[cfg(any(test, all(target_os = "macos", feature = "host")))]
mod apple_keys;
#[cfg(all(target_os = "macos", feature = "host"))]
pub mod macos;

use crate::{DesktopError, InputSink};

pub(crate) mod worker;

/// First usable input backend on this machine.
pub fn probe() -> Result<Box<dyn InputSink>, DesktopError> {
    probe_for_display(0)
}

pub(crate) fn probe_for_display(_display: u32) -> Result<Box<dyn InputSink>, DesktopError> {
    #[cfg(all(target_os = "linux", feature = "x11"))]
    if std::env::var_os("DISPLAY").is_some() && !crate::wayland_session() {
        return Ok(Box::new(x11::XtestInput::for_display(_display)?));
    }
    Err(DesktopError::Input(
        "no input backend available on this host".into(),
    ))
}
