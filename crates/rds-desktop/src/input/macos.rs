//! CGEvent injection behind a prepared, display-bound local consent owner.

use super::apple_keys;
use crate::{
    DesktopError, InputSink,
    capture::{geometry::Geometry, sck_native},
};
use objc2_core_foundation::CGPoint;
use objc2_core_graphics::*;
use rds_core::{InputEvent, InputKind};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

fn position() -> Result<CGPoint, DesktopError> {
    let event = CGEvent::new(None).ok_or_else(error)?;
    Ok(CGEvent::location(Some(&event)))
}

fn error() -> DesktopError {
    DesktopError::Input("macOS input unavailable, permission withdrawn or display changed".into())
}
pub fn available() -> bool {
    CGPreflightPostEventAccess()
}

#[derive(Default)]
struct Holds {
    keys: BTreeMap<u32, usize>,
    buttons: BTreeMap<u32, usize>,
    caps: bool,
    click: Option<(Instant, CGPoint, i64, u32)>,
}
impl Holds {
    fn flags(&self) -> CGEventFlags {
        let mut flags = CGEventFlags::empty();
        if self.caps {
            flags |= CGEventFlags::MaskAlphaShift;
        }
        for code in self.keys.keys() {
            flags |= match code {
                42 | 54 => CGEventFlags::MaskShift,
                29 | 97 => CGEventFlags::MaskControl,
                56 | 100 => CGEventFlags::MaskAlternate,
                125 | 126 => CGEventFlags::MaskCommand,
                464 => CGEventFlags::MaskSecondaryFn,
                _ => CGEventFlags::empty(),
            };
        }
        flags
    }
    fn key(&self, code: u32, pressed: bool, repeat: bool) -> Result<(), DesktopError> {
        let key = apple_keys::key(code).ok_or_else(error)?;
        let source = CGEventSource::new(CGEventSourceStateID::Private).ok_or_else(error)?;
        let event = CGEvent::new_keyboard_event(Some(&source), key, pressed).ok_or_else(error)?;
        CGEvent::set_flags(Some(&event), self.flags());
        CGEvent::set_integer_value_field(
            Some(&event),
            CGEventField::KeyboardEventAutorepeat,
            i64::from(repeat),
        );
        CGEvent::post(CGEventTapLocation::HIDEventTap, Some(&event));
        Ok(())
    }
    fn mouse(
        &self,
        kind: CGEventType,
        button: u32,
        point: CGPoint,
        clicks: i64,
    ) -> Result<(), DesktopError> {
        let source = CGEventSource::new(CGEventSourceStateID::Private).ok_or_else(error)?;
        let event = CGEvent::new_mouse_event(Some(&source), kind, point, CGMouseButton(button))
            .ok_or_else(error)?;
        CGEvent::set_flags(Some(&event), self.flags());
        CGEvent::set_integer_value_field(Some(&event), CGEventField::MouseEventClickState, clicks);
        CGEvent::post(CGEventTapLocation::HIDEventTap, Some(&event));
        Ok(())
    }
}

pub(crate) struct Group {
    stopped: Arc<AtomicBool>,
    holds: Mutex<Holds>,
    leases: Arc<Semaphore>,
}
impl Group {
    pub fn new(stopped: Arc<AtomicBool>) -> Arc<Self> {
        Arc::new(Self {
            stopped,
            holds: Mutex::new(Holds::default()),
            leases: Arc::new(Semaphore::new(64)),
        })
    }
    pub fn sink(self: &Arc<Self>, geometry: Geometry) -> Result<Box<dyn InputSink>, DesktopError> {
        if self.stopped.load(Ordering::Acquire) {
            return Err(error());
        }
        Ok(Box::new(Sink {
            group: self.clone(),
            geometry,
            keys: BTreeSet::new(),
            buttons: BTreeSet::new(),
            scroll: (0.0, 0.0),
            _permit: self
                .leases
                .clone()
                .try_acquire_owned()
                .map_err(|_| error())?,
        }))
    }
    pub fn stop(&self) {
        self.stopped.store(true, Ordering::Release);
        if let Ok(mut holds) = self.holds.lock() {
            let keys = std::mem::take(&mut holds.keys);
            for code in keys.keys() {
                let _ = holds.key(*code, false, false);
            }
            let buttons = std::mem::take(&mut holds.buttons);
            let point = position().unwrap_or(CGPoint { x: 0.0, y: 0.0 });
            for button in buttons.keys() {
                let _ = holds.mouse(button_type(*button, false), *button, point, 0);
            }
        }
    }
}

fn button_type(button: u32, pressed: bool) -> CGEventType {
    match (button, pressed) {
        (0, true) => CGEventType::LeftMouseDown,
        (0, false) => CGEventType::LeftMouseUp,
        (1, true) => CGEventType::RightMouseDown,
        (1, false) => CGEventType::RightMouseUp,
        (_, true) => CGEventType::OtherMouseDown,
        (_, false) => CGEventType::OtherMouseUp,
    }
}

struct Sink {
    group: Arc<Group>,
    geometry: Geometry,
    keys: BTreeSet<u32>,
    buttons: BTreeSet<u32>,
    scroll: (f64, f64),
    _permit: OwnedSemaphorePermit,
}
impl Sink {
    fn key(&mut self, holds: &mut Holds, code: u32, down: bool) -> Result<(), DesktopError> {
        apple_keys::key(code).ok_or_else(error)?;
        if down {
            if self.keys.contains(&code) {
                return holds.key(code, true, true);
            }
            let count = holds.keys.entry(code).or_default();
            *count += 1;
            if *count == 1 {
                if code == 58 {
                    holds.caps = !holds.caps;
                }
                if let Err(e) = holds.key(code, true, false) {
                    holds.keys.remove(&code);
                    return Err(e);
                }
            }
            self.keys.insert(code);
        } else if self.keys.remove(&code)
            && let Some(count) = holds.keys.get_mut(&code)
        {
            *count -= 1;
            if *count == 0 {
                holds.keys.remove(&code);
                holds.key(code, false, false)?;
            }
        }
        Ok(())
    }
    fn button(&mut self, holds: &mut Holds, code: i32, down: bool) -> Result<(), DesktopError> {
        let button = u32::try_from(code)
            .ok()
            .and_then(|c| c.checked_sub(0x110))
            .filter(|b| *b < 5)
            .ok_or_else(error)?;
        if down == self.buttons.contains(&button) {
            return Ok(());
        }
        let point = position()?;
        if down {
            self.contains(point)?;
            let count = holds.buttons.entry(button).or_default();
            *count += 1;
            if *count == 1 {
                let clicks = holds
                    .click
                    .filter(|(time, old, _, b)| {
                        *b == button
                            && time.elapsed() < Duration::from_millis(500)
                            && (old.x - point.x).abs() <= 4.0
                            && (old.y - point.y).abs() <= 4.0
                    })
                    .map_or(1, |(_, _, c, _)| (c + 1).min(3));
                if let Err(e) = holds.mouse(button_type(button, true), button, point, clicks) {
                    holds.buttons.remove(&button);
                    return Err(e);
                }
                holds.click = Some((Instant::now(), point, clicks, button));
            }
            self.buttons.insert(button);
        } else {
            self.buttons.remove(&button);
            if let Some(count) = holds.buttons.get_mut(&button) {
                *count -= 1;
                if *count == 0 {
                    holds.buttons.remove(&button);
                    holds.mouse(button_type(button, false), button, point, 1)?;
                }
            }
        }
        Ok(())
    }
    fn contains(&self, p: CGPoint) -> Result<(), DesktopError> {
        let g = self.geometry;
        if !p.x.is_finite()
            || !p.y.is_finite()
            || p.x < g.x
            || p.x >= g.x + g.logical_width
            || p.y < g.y
            || p.y >= g.y + g.logical_height
        {
            return Err(error());
        }
        Ok(())
    }
}
impl InputSink for Sink {
    fn inject(&mut self, event: &InputEvent) -> Result<(), DesktopError> {
        let group = self.group.clone();
        let mut holds = group.holds.lock().map_err(|_| error())?;
        if group.stopped.load(Ordering::Acquire)
            || !available()
            || !CGPreflightScreenCaptureAccess()
            || event.display_id != self.geometry.id
            || sck_native::geometry(self.geometry.id).ok() != Some(self.geometry)
        {
            return Err(error());
        }
        match event.kind {
            InputKind::KeyDown { code } => self.key(&mut holds, code, true),
            InputKind::KeyUp { code } => self.key(&mut holds, code, false),
            InputKind::PointerButton { button, pressed } => {
                self.button(&mut holds, button, pressed)
            }
            InputKind::PointerMove { x, y } => {
                let (x, y) = self.geometry.point(x, y).ok_or_else(error)?;
                let (button, kind) = if holds.buttons.contains_key(&0) {
                    (0, CGEventType::LeftMouseDragged)
                } else if holds.buttons.contains_key(&1) {
                    (1, CGEventType::RightMouseDragged)
                } else if let Some(button) = holds.buttons.keys().next() {
                    (*button, CGEventType::OtherMouseDragged)
                } else {
                    (0, CGEventType::MouseMoved)
                };
                holds.mouse(kind, button, CGPoint { x, y }, 0)
            }
            InputKind::PointerMotion { .. } => Err(DesktopError::Input(
                "relative pointer input is not scoped to this macOS display".into(),
            )),
            InputKind::Scroll { dx, dy } => {
                if !dx.is_finite() || !dy.is_finite() {
                    return Err(error());
                }
                self.contains(position()?)?;
                self.scroll.0 += dx.clamp(-32.0, 32.0);
                self.scroll.1 += dy.clamp(-32.0, 32.0);
                let x = self.scroll.0.trunc() as i32;
                let y = self.scroll.1.trunc() as i32;
                if x == 0 && y == 0 {
                    return Ok(());
                }
                let source = CGEventSource::new(CGEventSourceStateID::Private).ok_or_else(error)?;
                let scroll = CGEvent::new_scroll_wheel_event2(
                    Some(&source),
                    CGScrollEventUnit::Line,
                    2,
                    y,
                    x,
                    0,
                )
                .ok_or_else(error)?;
                CGEvent::set_flags(Some(&scroll), holds.flags());
                CGEvent::post(CGEventTapLocation::HIDEventTap, Some(&scroll));
                self.scroll.0 -= f64::from(x);
                self.scroll.1 -= f64::from(y);
                Ok(())
            }
        }
    }
}
impl Drop for Sink {
    fn drop(&mut self) {
        let group = self.group.clone();
        if let Ok(mut holds) = group.holds.lock() {
            let keys = std::mem::take(&mut self.keys);
            for code in keys {
                self.keys.insert(code);
                let _ = self.key(&mut holds, code, false);
            }
            let buttons = std::mem::take(&mut self.buttons);
            for button in buttons {
                self.buttons.insert(button);
                let _ = self.button(&mut holds, (button + 0x110) as i32, false);
            }
        }
    }
}
