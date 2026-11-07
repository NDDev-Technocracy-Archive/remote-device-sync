//! Portable portal boundary checks; native session integration is separate.

#[cfg(all(target_os = "linux", feature = "portal"))]
mod capture;
#[cfg(all(target_os = "linux", feature = "portal"))]
mod clipboard;
mod geometry;
#[cfg(all(target_os = "linux", feature = "portal"))]
mod input;
#[cfg(all(target_os = "linux", feature = "portal"))]
mod session;
#[cfg(all(target_os = "linux", feature = "portal"))]
mod state;

#[cfg(all(target_os = "linux", feature = "portal"))]
mod backend;
#[cfg(all(target_os = "linux", feature = "portal"))]
pub use backend::WaylandDesktop;

#[cfg(all(test, target_os = "linux", feature = "portal"))]
mod input_tests;
