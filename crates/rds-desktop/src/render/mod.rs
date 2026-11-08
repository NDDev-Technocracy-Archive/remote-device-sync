//! Native window presentation. Each session retains one newest BGRA frame;
//! one GPU surface presents the active session.
//! window/input handling stays on the OS main thread and networking stays async.

pub mod workspace;

#[cfg(feature = "viewer")]
mod clipboard;
#[cfg(feature = "viewer")]
mod design;
#[cfg(feature = "viewer")]
mod gpu;
#[cfg(feature = "viewer")]
mod input;
#[cfg(feature = "viewer")]
mod platform;
#[cfg(all(feature = "viewer", feature = "host", target_os = "macos"))]
pub use platform::native_clipboard_owner_probe;
#[cfg(feature = "viewer")]
pub use platform::{NativeWindowState, choose_resolution, native_clipboard_probe};
#[cfg(feature = "viewer")]
mod viewer;
#[cfg(feature = "viewer")]
mod visual_probe;
#[cfg(feature = "viewer")]
pub use viewer::{
    InputReceiver, Viewer, ViewerHandle, ViewerInput, ViewerReport, ViewerSnapshot, WorkspaceEvent,
    WorkspaceHandle, WorkspaceUpdate,
};
#[cfg(feature = "viewer")]
pub use visual_probe::{VisualProbeReport, VisualProbeSpec};

/// The native viewer was compiled; actual adapter/window creation can still fail.
pub fn available() -> bool {
    cfg!(feature = "viewer")
}
