//! Desktop session pipeline for rds.
//!
//! Server side: `serve_desktop` captures a display, encodes frames and
//! streams each frame on its own uni-directional QUIC stream — stale
//! frames are reset rather than retransmitted, following the MoQ
//! pattern. Client side: `client` decodes the newest frames and sends
//! input events back on the control stream.
//!
//! # Module map
//!
//! - [`capture`] — frame sources per platform (probe order in the
//!   module docs; see `docs/platforms.md`).
//! - [`codec`] — encoder/decoder backends (Vulkan Video, VA-API,
//!   VideoToolbox, software floor).
//! - [`input`] — injection backends (portal/EIS, uinput, XTEST, CGEvent).
//! - [`render`] — presentation (wgpu surface, newest-frame-only).
//! - [`session`] / [`client`] — serving and viewing sides of a session.

use bytes::Bytes;
use rds_core::{Codec, DesktopCaps, InputEvent};
use thiserror::Error;

pub mod capture;
pub mod client;
pub mod codec;
pub mod input;
pub mod mailbox;
pub mod render;
mod session;

#[cfg(feature = "x11")]
pub use codec::openh264::{H264Decoder, H264Encoder};
pub use session::{
    BitrateController, FrameProducer, NullProducer, Produced, ProducerControls, SessionClock,
    SessionConfig, SyntheticProducer, serve_desktop, serve_desktop_with,
};

/// One captured video frame, BGRA8 unless noted otherwise.
///
/// Hardware paths will carry GPU buffers instead of `Bytes`; the trait
/// surface is stable while `RawFrame` grows a surface variant.
#[derive(Debug)]
pub struct RawFrame {
    pub width: u32,
    pub height: u32,
    /// Bytes per row of `data`.
    pub stride: u32,
    pub data: Bytes,
}

/// Local software receive limit: up to 8K pixels, no side above 8192.
/// Check before allocating BGRA output; native codec internal memory is separate.
pub(crate) fn frame_bytes(width: usize, height: usize) -> Option<usize> {
    if width == 0 || height == 0 || width > 8192 || height > 8192 {
        return None;
    }
    let pixels = width.checked_mul(height)?;
    (pixels <= 7680 * 4320).then_some(pixels)?.checked_mul(4)
}

/// One encoded frame ready for the wire.
#[derive(Debug)]
pub struct EncodedFrame {
    pub codec: Codec,
    pub data: Bytes,
    pub keyframe: bool,
}

#[derive(Debug, Error)]
pub enum DesktopError {
    #[error("capture backend unavailable: {0}")]
    Capture(String),
    #[error("encode failed: {0}")]
    Encode(String),
    #[error("decode failed: {0}")]
    Decode(String),
    #[error("input injection failed: {0}")]
    Input(String),
    #[error("connection failed: {0}")]
    Connection(#[from] rds_net::ConnectionError),
    #[error("stream closed by peer")]
    Closed(#[from] rds_net::ClosedStream),
    #[error("stream write failed: {0}")]
    Write(#[from] rds_net::WriteError),
    #[error("stream read failed: {0}")]
    Read(#[from] rds_net::ReadError),
    #[error("stream read failed: {0}")]
    ReadExact(#[from] rds_net::ReadExactError),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

/// A source of display frames.
pub trait Capturer: Send + 'static {
    /// Next frame in display order; blocks the calling thread as needed.
    fn capture(&mut self) -> Result<RawFrame, DesktopError>;
    fn displays(&self) -> Vec<rds_core::DisplayInfo>;
    /// Whether display content changed since the previous `capture`.
    /// `true` is the conservative default — backends without damage
    /// tracking always report changed.
    fn changed(&mut self) -> bool {
        true
    }
}

/// Frame encoder. Implementations must emit Annex-B streams and honor
/// `request_idr` before the next frame.
pub trait Encoder: Send + 'static {
    fn encode(&mut self, frame: &RawFrame) -> Result<EncodedFrame, DesktopError>;
    fn request_idr(&mut self);
    fn set_bitrate(&mut self, bps: u32);
}

/// Frame decoder for the viewing side.
pub trait Decoder: Send + 'static {
    /// Returns the decoded frame, or `None` when the codec needs more data.
    fn decode(&mut self, frame: &EncodedFrame) -> Result<Option<RawFrame>, DesktopError>;
}

/// Injects remote input events into the serving session.
pub trait InputSink: Send + 'static {
    fn inject(&mut self, event: &InputEvent) -> Result<(), DesktopError>;
}

/// What this build can serve.
pub fn capabilities() -> Result<DesktopCaps, DesktopError> {
    #[cfg(all(target_os = "linux", feature = "x11"))]
    {
        capture::x11::capabilities()
    }
    #[cfg(not(all(target_os = "linux", feature = "x11")))]
    Err(DesktopError::Capture("no capture backend built".into()))
}
