//! Display capture backends, probed in preference order per platform.
//!
//! Linux order (docs/platforms.md): `image_copy`
//! (ext-image-copy-capture-v1, privileged clients) → `kms` (DRM/KMS
//! scanout, unattended/headless) → `pipewire` (XDG portal, consented
//! sessions) → `x11`. macOS: `sck` (ScreenCaptureKit).
//!
//! Each backend exposes `probe(screen) -> Result<Option<Capturer>>`:
//! `None` means unavailable on this host/session, `Err` means present
//! but broken. `probe()` returns the first usable capturer.

#[cfg(all(target_os = "linux", feature = "x11"))]
pub mod x11;
#[cfg(all(target_os = "linux", feature = "x11"))]
pub(crate) mod x11_displays;

#[cfg(target_os = "linux")]
pub mod image_copy;
#[cfg(target_os = "linux")]
pub mod kms;
#[cfg(target_os = "linux")]
pub mod pipewire;

#[cfg(any(test, all(target_os = "macos", feature = "host")))]
pub(crate) mod geometry;
#[cfg(all(target_os = "macos", feature = "host"))]
pub(crate) mod macos;
#[cfg(target_os = "macos")]
pub mod sck;
#[cfg(all(target_os = "macos", feature = "host"))]
pub(crate) mod sck_native;

use crate::{Capturer, DesktopError};

type Probe = Result<Option<Box<dyn Capturer>>, DesktopError>;

/// First usable capture backend on this machine, in preference order.
///
/// Unimplemented backends probe `None` and are skipped, so the order is
/// safe to ship incrementally.
pub fn probe(screen: u32) -> Result<Box<dyn Capturer>, DesktopError> {
    let candidates: &[fn(u32) -> Probe] = &[
        #[cfg(target_os = "linux")]
        image_copy::probe,
        #[cfg(target_os = "linux")]
        kms::probe,
        #[cfg(target_os = "linux")]
        pipewire::probe,
        #[cfg(all(target_os = "linux", feature = "x11"))]
        |screen| {
            if std::env::var_os("DISPLAY").is_none() {
                return Ok(None);
            }
            x11::X11Capturer::new(screen).map(|c| Some(Box::new(c) as Box<dyn Capturer>))
        },
        #[cfg(target_os = "macos")]
        sck::probe,
    ];
    for probe in candidates {
        if let Some(capturer) = probe(screen)? {
            return Ok(capturer);
        }
    }
    let _ = screen;
    Err(DesktopError::Capture(
        "no capture backend available on this host".into(),
    ))
}

#[cfg(any(test, all(target_os = "linux", feature = "portal"),))]
pub(crate) mod frame;
#[cfg(any(
    all(target_os = "linux", feature = "portal"),
    all(target_os = "macos", feature = "host")
))]
pub(crate) mod latest;
#[cfg(any(
    all(target_os = "linux", feature = "portal"),
    all(target_os = "macos", feature = "host")
))]
pub(crate) mod producer;
