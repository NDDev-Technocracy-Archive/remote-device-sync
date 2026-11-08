//! X11 capture via `x11rb`: MIT-SHM shared-memory `GetImage` where the
//! server supports it, plain `GetImage` polling as the portable
//! fallback.
//!
//! MIT-SHM removes the dominant capture cost: the pixmap is written
//! into a shared segment the client mmaps, so a 1080p frame costs one
//! reply and a synchronous borrowed view instead of ~8 MiB serialized through
//! the X socket. `CreateSegment` (SHM ≥ 1.2 fd passing) is preferred —
//! the server allocates and owns nothing persists on the client side.
//! `ext-image-copy-capture` and the DRM/KMS tap remain scheduled behind
//! the same `Capturer` trait. Input injection lives in
//! `crate::input::x11`.
//!
//! `mmap` on the server-provided segment is the one unsafe call in this
//! module (workspace convention: FFI paths allow `unsafe_code` locally).
#![allow(unsafe_code)]

use std::borrow::Cow;
use std::os::fd::OwnedFd;

use bytes::Bytes;
use memmap2::{MmapMut, MmapOptions};
use rds_core::{DesktopCaps, DisplayInfo};
use x11rb::connection::{Connection as _, RequestConnection as _};
use x11rb::protocol::Event;
use x11rb::protocol::damage::{self, ConnectionExt as _, Damage, ReportLevel};
use x11rb::protocol::shm::{self, ConnectionExt as _, Seg};
use x11rb::protocol::xproto::{ConnectionExt, GetImageReply, ImageFormat};
use x11rb::rust_connection::RustConnection;

use crate::{BgraFrame, Capturer, DesktopError, RawFrame};

/// MIT-SHM segment the server fills in place — the reply is a
/// completion event, not a serialized pixmap.
struct ShmPath {
    seg: Seg,
    map: MmapMut,
    len: usize,
    /// The mapping stays valid without it, but keeping the fd makes
    /// ownership/lifetime explicit.
    _fd: OwnedFd,
}

/// Polls one X11 screen (root window) into `RawFrame`s.
pub struct X11Capturer {
    conn: RustConnection,
    root: x11rb::protocol::xproto::Window,
    x: i16,
    y: i16,
    width: u16,
    height: u16,
    displays: Vec<DisplayInfo>,
    selected: super::x11_displays::Display,
    topology_dirty: bool,
    shm: Option<ShmPath>,
    /// DAMAGE object on the root window — idle detection so a still
    /// desktop costs no capture/encode work at all.
    damage: Option<Damage>,
    /// Sticky flag: the first `changed` call must be true (the screen
    /// existed before the damage object did).
    dirty: bool,
}

impl X11Capturer {
    /// Wait for actual X damage or the original session's control/cancel wake.
    pub(crate) fn wait_for_change(
        &mut self,
        timeout: std::time::Duration,
        wake: std::os::fd::BorrowedFd<'_>,
    ) -> Result<bool, DesktopError> {
        if self.changed() {
            return Ok(true);
        }
        let mut fds = [
            rustix::event::PollFd::new(self.conn.stream(), rustix::event::PollFlags::IN),
            rustix::event::PollFd::new(&wake, rustix::event::PollFlags::IN),
        ];
        let timeout = rustix::event::Timespec {
            tv_sec: timeout.as_secs() as i64,
            tv_nsec: i64::from(timeout.subsec_nanos()),
        };
        match rustix::event::poll(&mut fds, Some(&timeout)) {
            Ok(_)
                if fds.iter().any(|fd| {
                    fd.revents().intersects(
                        rustix::event::PollFlags::ERR
                            | rustix::event::PollFlags::HUP
                            | rustix::event::PollFlags::NVAL,
                    )
                }) =>
            {
                Err(DesktopError::Capture(
                    "native capture poll owner ended".into(),
                ))
            }
            Ok(_) => Ok(self.changed()),
            Err(rustix::io::Errno::INTR) => Ok(false),
            Err(error) => Err(DesktopError::Io(error.into())),
        }
    }
    /// Select an X root by its legacy index or a RandR logical monitor by its
    /// high-bit ID from `capabilities()`. IDs never depend on list ordering.
    pub fn new(display_index: u32) -> Result<Self, DesktopError> {
        reject_wayland()?;
        let (conn, _) =
            RustConnection::connect(None).map_err(|e| DesktopError::Capture(e.to_string()))?;
        let displays = super::x11_displays::catalog(&conn)?;
        let geometry = displays
            .iter()
            .find(|display| display.id == display_index)
            .cloned()
            .ok_or_else(|| DesktopError::Capture("X11 display does not exist".into()))?;
        let root = geometry.root;
        if crate::frame_bytes(usize::from(geometry.width), usize::from(geometry.height)).is_none() {
            return Err(DesktopError::Capture(
                "selected display exceeds frame memory bounds".into(),
            ));
        }
        if !conn
            .setup()
            .pixmap_formats
            .iter()
            .any(|f| f.depth == geometry.root_depth && f.bits_per_pixel == 32)
        {
            return Err(DesktopError::Capture(
                "selected X11 root has no 32bpp format".into(),
            ));
        }
        super::x11_displays::subscribe(&conn, root)?;
        let shm = Self::try_shm(&conn, geometry.width, geometry.height);
        let damage = Self::try_damage(&conn, root);
        Ok(Self {
            conn,
            root,
            x: geometry.x,
            y: geometry.y,
            width: geometry.width,
            height: geometry.height,
            displays: displays
                .iter()
                .map(super::x11_displays::Display::info)
                .collect(),
            selected: geometry,
            topology_dirty: false,
            shm,
            damage,
            dirty: true,
        })
    }

    /// MIT-SHM ≥ 1.2 (fd passing): the server creates the segment and
    /// hands back an fd we mmap. `None` on older servers, remote
    /// displays, or any setup failure — the caller falls back to plain
    /// `GetImage`.
    fn try_shm(conn: &RustConnection, width: u16, height: u16) -> Option<ShmPath> {
        conn.extension_information(shm::X11_EXTENSION_NAME).ok()??;
        let ver = conn.shm_query_version().ok()?.reply().ok()?;
        if (ver.major_version, ver.minor_version) < (1, 2) {
            return None;
        }
        let len = usize::from(width) * usize::from(height) * 4;
        let seg = conn.generate_id().ok()?;
        let reply = conn
            .shm_create_segment(seg, len as u32, false)
            .ok()?
            .reply()
            .ok()?;
        let fd: OwnedFd = reply.shm_fd;
        // SAFETY: `fd` is a fresh server-created segment of exactly
        // `len` bytes; nothing else mutates its size.
        let map = unsafe { MmapOptions::new().len(len).map_mut(&fd) }.ok()?;
        Some(ShmPath {
            seg,
            map,
            len,
            _fd: fd,
        })
    }

    /// DAMAGE (XFixes ≥ 4): one object on the root window reporting
    /// `NON_EMPTY` transitions — enough for a changed/not-changed bit.
    /// `None` where the extension is absent.
    fn try_damage(conn: &RustConnection, root: x11rb::protocol::xproto::Window) -> Option<Damage> {
        conn.extension_information(damage::X11_EXTENSION_NAME)
            .ok()??;
        conn.damage_query_version(1, 1).ok()?.reply().ok()?;
        let dmg = conn.generate_id().ok()?;
        conn.damage_create(dmg, root, ReportLevel::NON_EMPTY)
            .ok()?
            .check()
            .ok()?;
        Some(dmg)
    }
}

impl X11Capturer {
    /// The synchronous callback is the lifetime boundary: the next capture
    /// cannot overwrite the mapping until conversion/encode returns. Owned
    /// capture consumers retain the separate snapshot-producing trait method.
    pub(crate) fn capture_with<T>(
        &mut self,
        process: impl FnOnce(BgraFrame<'_>) -> T,
    ) -> Result<T, DesktopError> {
        let (width, height) = (u32::from(self.width), u32::from(self.height));
        let pixels = self.capture_pixels()?;
        Ok(process(BgraFrame {
            width,
            height,
            stride: width * 4,
            data: pixels.as_ref(),
        }))
    }

    fn capture_pixels(&mut self) -> Result<Cow<'_, [u8]>, DesktopError> {
        let dirty = self.changed();
        self.dirty |= dirty;
        // Monitor objects can be edited without RandR event delivery on some
        // servers. One small geometry reply fences every monitor capture.
        if self.topology_dirty || self.selected.monitor.is_some() {
            super::x11_displays::still_matches(&self.conn, &self.selected)?;
            self.topology_dirty = false;
        }
        if let Some(shm) = &self.shm {
            // The GetImage reply arrives after the server has finished
            // writing the segment. No new request can reuse it while the
            // callback holds the capturer's exclusive borrow.
            let filled = self
                .conn
                .shm_get_image(
                    self.root,
                    self.x,
                    self.y,
                    self.width,
                    self.height,
                    !0,
                    ImageFormat::Z_PIXMAP.into(),
                    shm.seg,
                    0,
                )
                .map_err(|e| DesktopError::Capture(e.to_string()))
                .and_then(|c| c.reply().map_err(|e| DesktopError::Capture(e.to_string())));
            match filled {
                Ok(_) => {
                    let shm = self
                        .shm
                        .as_ref()
                        .expect("successful capture retains its segment");
                    return Ok(Cow::Borrowed(&shm.map[..shm.len]));
                }
                Err(e) => {
                    tracing::warn!("MIT-SHM capture failed ({e}); falling back to GetImage");
                }
            }
        }
        // Reached only when the shm attempt failed at runtime — stop
        // paying for attempts that can no longer succeed.
        self.shm = None;
        let reply: GetImageReply = self
            .conn
            .get_image(
                ImageFormat::Z_PIXMAP,
                self.root,
                self.x,
                self.y,
                self.width,
                self.height,
                !0,
            )
            .map_err(|e| DesktopError::Capture(e.to_string()))?
            .reply()
            .map_err(|e| DesktopError::Capture(e.to_string()))?;
        let expected = usize::from(self.width) * usize::from(self.height) * 4;
        if reply.data.len() != expected {
            return Err(DesktopError::Capture(format!(
                "X11 GetImage returned {} bytes for {}x{} (expected {expected})",
                reply.data.len(),
                self.width,
                self.height
            )));
        }
        Ok(Cow::Owned(reply.data))
    }
}

impl Capturer for X11Capturer {
    fn capture(&mut self) -> Result<RawFrame, DesktopError> {
        let (width, height) = (u32::from(self.width), u32::from(self.height));
        let pixels = self.capture_pixels()?;
        Ok(RawFrame {
            width,
            height,
            stride: width * 4,
            data: Bytes::from(pixels.into_owned()),
        })
    }

    fn displays(&self) -> Vec<DisplayInfo> {
        self.displays.clone()
    }

    /// Damage-driven change detection: drains pending events; any
    /// `DamageNotify` on our object marks the screen dirty. `NON_EMPTY`
    /// reports only the empty→non-empty transition, so a dirty read
    /// subtracts the region back to empty to re-arm notification.
    fn changed(&mut self) -> bool {
        let mut dirty = std::mem::take(&mut self.dirty);
        if self.damage.is_none() {
            dirty = true;
        }
        while let Ok(Some(ev)) = self.conn.poll_for_event() {
            if matches!(
                ev,
                Event::RandrScreenChangeNotify(_) | Event::RandrNotify(_)
            ) {
                self.topology_dirty = true;
                dirty = true;
            }
            if matches!(ev, Event::DamageNotify(e) if Some(e.damage) == self.damage) {
                dirty = true;
            }
        }
        if dirty && let Some(dmg) = self.damage {
            let _ = self.conn.damage_subtract(dmg, 0u32, 0u32);
        }
        dirty
    }
}

impl Drop for X11Capturer {
    fn drop(&mut self) {
        if let Some(dmg) = self.damage.take() {
            let _ = self.conn.damage_destroy(dmg);
        }
        if let Some(shm) = self.shm.take() {
            let _ = self.conn.shm_detach(shm.seg);
        }
    }
}

/// Displays visible over X11.
pub fn capabilities() -> Result<DesktopCaps, DesktopError> {
    reject_wayland()?;
    // Inventory must not allocate a full capture mapping or DAMAGE object.
    // Validate the same pixel/memory contract, then let the requested session
    // own its native capture resources once.
    let (conn, _) =
        RustConnection::connect(None).map_err(|e| DesktopError::Capture(e.to_string()))?;
    let displays = super::x11_displays::catalog(&conn)?
        .into_iter()
        .filter(|display| {
            crate::frame_bytes(usize::from(display.width), usize::from(display.height)).is_some()
                && conn
                    .setup()
                    .pixmap_formats
                    .iter()
                    .any(|format| format.depth == display.root_depth && format.bits_per_pixel == 32)
        })
        .map(|display| display.info())
        .collect::<Vec<_>>();
    if displays.is_empty() {
        return Err(DesktopError::Capture(
            "no display satisfies the capture memory/pixel contract".into(),
        ));
    }
    Ok(DesktopCaps {
        displays,
        codecs: vec![rds_core::Codec::H264],
    })
}

fn reject_wayland() -> Result<(), DesktopError> {
    if crate::wayland_session() {
        return Err(DesktopError::Capture(
            "Wayland requires an explicitly prepared portal backend".into(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[ignore = "requires a dedicated Xvfb server; configures two RandR monitors"]
    fn randr_monitor_capture_and_input_share_regions_and_removed_ids_fail() {
        use crate::{InputSink, input::x11::XtestInput};
        use x11rb::protocol::{
            randr::{ConnectionExt as _, MonitorInfo},
            xproto::{CreateGCAux, Rectangle},
        };
        let (conn, _) = RustConnection::connect(None).unwrap();
        let root = conn.setup().roots[0].root;
        let atom = |name: &[u8]| conn.intern_atom(false, name).unwrap().reply().unwrap().atom;
        let left = atom(b"RDS_TEST_LEFT");
        let right = atom(b"RDS_TEST_RIGHT");
        for (name, x) in [(left, 0), (right, 640)] {
            conn.randr_set_monitor(
                root,
                MonitorInfo {
                    name,
                    primary: name == left,
                    automatic: false,
                    x,
                    y: 0,
                    width: 640,
                    height: 720,
                    width_in_millimeters: 170,
                    height_in_millimeters: 190,
                    outputs: vec![],
                },
            )
            .unwrap()
            .check()
            .unwrap();
        }
        let catalog = super::super::x11_displays::catalog(&conn).unwrap();
        let selected = catalog.iter().find(|d| d.monitor == Some(right)).unwrap();
        let id = selected.id;
        let mut capture = X11Capturer::new(id).unwrap();
        let gc = conn.generate_id().unwrap();
        conn.create_gc(gc, root, &CreateGCAux::new().foreground(0x123456))
            .unwrap()
            .check()
            .unwrap();
        conn.poly_fill_rectangle(
            root,
            gc,
            &[Rectangle {
                x: 640,
                y: 0,
                width: 640,
                height: 720,
            }],
        )
        .unwrap()
        .check()
        .unwrap();
        let raw = capture.capture().unwrap();
        assert_eq!((raw.width, raw.height), (640, 720));
        assert_eq!(&raw.data[..3], &[0x56, 0x34, 0x12]);
        let mut input = XtestInput::for_display(id).unwrap();
        input
            .inject(&rds_core::InputEvent {
                seq: 0,
                event_ts_ms: 0,
                display_id: id,
                kind: rds_core::InputKind::PointerMove { x: 7., y: 9. },
            })
            .unwrap();
        let point = conn.query_pointer(root).unwrap().reply().unwrap();
        assert_eq!((point.root_x, point.root_y), (647, 9));
        conn.randr_delete_monitor(root, right)
            .unwrap()
            .check()
            .unwrap();
        assert!(X11Capturer::new(id).is_err());
        capture.topology_dirty = true;
        assert!(capture.capture().is_err());
        assert!(
            input
                .inject(&rds_core::InputEvent {
                    seq: 1,
                    event_ts_ms: 0,
                    display_id: id,
                    kind: rds_core::InputKind::PointerMove { x: 1., y: 1. }
                })
                .is_err()
        );
        conn.randr_delete_monitor(root, left)
            .unwrap()
            .check()
            .unwrap();
        conn.free_gc(gc).unwrap().check().unwrap();
    }
    /// Explicit native fixture: never silently succeeds without capture.
    #[test]
    #[ignore = "requires a dedicated Xvfb server; repaints its root"]
    fn capture_roundtrip() {
        let display = std::env::var_os("DISPLAY").expect("dedicated X11 server required");
        let local = display.to_string_lossy().starts_with(':');
        let mut cap = X11Capturer::new(0).expect("native X11 capture must open");
        if local {
            assert!(cap.shm.is_some(), "local X server without MIT-SHM 1.2?");
            assert!(cap.damage.is_some(), "local X server without DAMAGE?");
        }
        for _ in 0..3 {
            let frame = cap.capture().unwrap();
            assert_eq!(frame.data.len(), (frame.width * frame.height * 4) as usize);
            assert_eq!(frame.stride, frame.width * 4);
        }
        if !local {
            return;
        }
        // Damage bookkeeping: first read is dirty (screen predates the
        // damage object), a still screen then reports clean, and a
        // server-side repaint re-dirties it.
        assert!(cap.changed(), "first changed() must be dirty");
        assert!(!cap.changed(), "still screen reported damage");
        use x11rb::protocol::xproto::{CreateGCAux, Rectangle};
        let gc = cap.conn.generate_id().unwrap();
        cap.conn
            .create_gc(gc, cap.root, &CreateGCAux::new().foreground(0x12_34_56))
            .unwrap()
            .check()
            .unwrap();
        cap.conn
            .poly_fill_rectangle(
                cap.root,
                gc,
                &[Rectangle {
                    x: 0,
                    y: 0,
                    width: 100,
                    height: 100,
                }],
            )
            .unwrap()
            .check()
            .unwrap();
        assert!(cap.changed(), "root repaint produced no damage");
        let frame = cap.capture().unwrap();
        let pixel = 50 * frame.stride as usize + 50 * 4;
        assert_eq!(
            &frame.data[pixel..pixel + 3],
            &[0x56, 0x34, 0x12],
            "captured pixel did not match the native paint"
        );
        // The producer borrows the completed mapping directly; the owned
        // snapshot above must survive another capture and a different paint.
        let mapped = cap.shm.as_ref().unwrap().map.as_ptr();
        cap.conn
            .change_gc(
                gc,
                &x11rb::protocol::xproto::ChangeGCAux::new().foreground(0xAB_CD_EF),
            )
            .unwrap()
            .check()
            .unwrap();
        cap.conn
            .poly_fill_rectangle(
                cap.root,
                gc,
                &[Rectangle {
                    x: 0,
                    y: 0,
                    width: 100,
                    height: 100,
                }],
            )
            .unwrap()
            .check()
            .unwrap();
        cap.capture_with(|borrowed| {
            assert_eq!(
                borrowed.data.as_ptr(),
                mapped,
                "borrowed capture copied pixels"
            );
            assert_eq!(&borrowed.data[pixel..pixel + 3], &[0xEF, 0xCD, 0xAB]);
        })
        .unwrap();
        assert_eq!(&frame.data[pixel..pixel + 3], &[0x56, 0x34, 0x12]);

        // A server without usable SHM still provides the same borrowed input
        // contract from its owned GetImage reply; no extra staging copy.
        let shm = cap.shm.take().unwrap();
        cap.conn.shm_detach(shm.seg).unwrap().check().unwrap();
        drop(shm);
        cap.capture_with(|borrowed| {
            assert_eq!(&borrowed.data[pixel..pixel + 3], &[0xEF, 0xCD, 0xAB]);
        })
        .unwrap();
        assert_eq!(
            &cap.capture().unwrap().data[pixel..pixel + 3],
            &[0xEF, 0xCD, 0xAB]
        );
        cap.conn.free_gc(gc).unwrap().check().unwrap();
    }
}
