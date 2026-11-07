//! Ambient probing cannot acquire or borrow macOS screen consent. Use the
//! locally prepared `MacDesktop` owner to serve ScreenCaptureKit frames.
use crate::{Capturer, DesktopError};

pub fn probe(_screen: u32) -> Result<Option<Box<dyn Capturer>>, DesktopError> {
    Err(DesktopError::Capture(
        "macOS requires a locally prepared MacDesktop owner".into(),
    ))
}
