//! Compositor-approved shortcuts inhibition for this owned window, never a global grab.
#![allow(unsafe_code)]
use std::sync::Arc;
use wayland_backend::sys::client::{Backend, ObjectId};
use wayland_client::{
    Connection, Dispatch, EventQueue, Proxy, QueueHandle,
    globals::{GlobalList, GlobalListContents, registry_queue_init},
    protocol::{wl_registry, wl_seat::WlSeat, wl_surface::WlSurface},
};
use wayland_protocols::wp::keyboard_shortcuts_inhibit::zv1::client::{
    zwp_keyboard_shortcuts_inhibit_manager_v1::ZwpKeyboardShortcutsInhibitManagerV1 as Manager,
    zwp_keyboard_shortcuts_inhibitor_v1::{self, ZwpKeyboardShortcutsInhibitorV1 as Inhibitor},
};
use winit::{
    event_loop::OwnedDisplayHandle,
    raw_window_handle::{HasDisplayHandle, HasWindowHandle, RawDisplayHandle, RawWindowHandle},
    window::Window,
};

pub(crate) struct KeyboardShortcuts {
    state: ProtocolState,
    _globals: GlobalList,
    queue: EventQueue<ProtocolState>,
    connection: Connection,
    // These owners outlive every foreign proxy/backend above (field drop order).
    _window: Arc<Window>,
    _display: OwnedDisplayHandle,
}
struct ProtocolState {
    manager: Manager,
    surface: WlSurface,
    seats: Vec<(u32, WlSeat)>,
    inhibitors: Vec<(u32, Inhibitor)>,
    active: bool,
}
impl ProtocolState {
    fn add_seat(&mut self, name: u32, seat: WlSeat, qh: &QueueHandle<Self>) {
        if self.active {
            self.inhibitors.push((
                name,
                self.manager.inhibit_shortcuts(&self.surface, &seat, qh, ()),
            ));
        }
        self.seats.push((name, seat));
    }
    fn remove_seat(&mut self, name: u32) {
        if let Some(index) = self.inhibitors.iter().position(|(id, _)| *id == name) {
            self.inhibitors.swap_remove(index).1.destroy();
        }
        if let Some(index) = self.seats.iter().position(|(id, _)| *id == name) {
            let seat = self.seats.swap_remove(index).1;
            if seat.version() >= 5 {
                seat.release();
            }
        }
    }
    fn deactivate(&mut self) {
        for (_, inhibitor) in self.inhibitors.drain(..) {
            inhibitor.destroy();
        }
        self.active = false;
    }
}
impl KeyboardShortcuts {
    pub fn new(window: Arc<Window>, display: OwnedDisplayHandle) -> Option<Self> {
        let RawDisplayHandle::Wayland(raw_display) = display.display_handle().ok()?.as_raw() else {
            return None;
        };
        let RawWindowHandle::Wayland(raw_window) = window.window_handle().ok()?.as_raw() else {
            return None;
        };
        // SAFETY: both raw pointers come from our live Winit window/display.
        // Strong owners remain after all proxies/queue/backend in field drop order.
        // The guest backend never disconnects or takes ownership of Winit's display;
        // the surface is only borrowed, never destroyed or reassigned by this adapter.
        let backend = unsafe { Backend::from_foreign_display(raw_display.display.as_ptr().cast()) };
        let connection = Connection::from_backend(backend);
        // SAFETY: the same strongly owned window keeps this exact wl_surface live.
        let id = unsafe {
            ObjectId::from_ptr(WlSurface::interface(), raw_window.surface.as_ptr().cast())
        }
        .ok()?;
        let surface = WlSurface::from_id(&connection, id).ok()?;
        let (globals, queue) = registry_queue_init::<ProtocolState>(&connection).ok()?;
        let qh = queue.handle();
        let manager = match globals.bind::<Manager, _, _>(&qh, 1..=1, ()) {
            Ok(manager) => manager,
            Err(error) => {
                tracing::info!(%error, "native Wayland shortcuts inhibition unavailable");
                return None;
            }
        };
        let mut state = ProtocolState {
            manager,
            surface,
            seats: Vec::new(),
            inhibitors: Vec::new(),
            active: false,
        };
        for global in globals.contents().clone_list() {
            if global.interface == WlSeat::interface().name {
                let seat = globals
                    .registry()
                    .bind(global.name, global.version.min(7), &qh, ());
                state.add_seat(global.name, seat, &qh);
            }
        }
        Some(Self {
            state,
            _globals: globals,
            queue,
            connection,
            _window: window,
            _display: display,
        })
    }
    pub fn set_active(&mut self, active: bool) {
        if active != self.state.active {
            if active {
                for (name, seat) in &self.state.seats {
                    self.state.inhibitors.push((
                        *name,
                        self.state.manager.inhibit_shortcuts(
                            &self.state.surface,
                            seat,
                            &self.queue.handle(),
                            (),
                        ),
                    ));
                }
                self.state.active = true;
            } else {
                self.state.deactivate();
            }
            if let Err(error) = self.connection.flush() {
                tracing::warn!(%error, "native Wayland shortcuts request could not be flushed");
            }
        }
        // Winit reads the shared display; dispatch only our own guest event queue.
        if let Err(error) = self.queue.dispatch_pending(&mut self.state) {
            tracing::warn!(%error, "native Wayland shortcuts event dispatch failed");
            self.state.deactivate();
        }
    }
}
impl Drop for KeyboardShortcuts {
    fn drop(&mut self) {
        self.state.deactivate();
        while let Some((name, _)) = self.state.seats.last() {
            self.state.remove_seat(*name);
        }
        self.state.manager.destroy();
        let _ = self.connection.flush();
    }
}
impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for ProtocolState {
    fn event(
        state: &mut Self,
        registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _: &GlobalListContents,
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        match event {
            wl_registry::Event::Global {
                name,
                interface,
                version,
            } if interface == WlSeat::interface().name => {
                state.add_seat(name, registry.bind(name, version.min(7), qh, ()), qh);
            }
            wl_registry::Event::GlobalRemove { name } => state.remove_seat(name),
            _ => {}
        }
    }
}
impl Dispatch<WlSeat, ()> for ProtocolState {
    fn event(
        _: &mut Self,
        _: &WlSeat,
        _: wayland_client::protocol::wl_seat::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}
impl Dispatch<Manager, ()> for ProtocolState {
    fn event(
        _: &mut Self,
        _: &Manager,
        _: wayland_protocols::wp::keyboard_shortcuts_inhibit::zv1::client::zwp_keyboard_shortcuts_inhibit_manager_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}
impl Dispatch<Inhibitor, ()> for ProtocolState {
    fn event(
        _: &mut Self,
        _: &Inhibitor,
        event: zwp_keyboard_shortcuts_inhibitor_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            zwp_keyboard_shortcuts_inhibitor_v1::Event::Active => {
                tracing::info!("native Wayland remote keyboard shortcuts active")
            }
            zwp_keyboard_shortcuts_inhibitor_v1::Event::Inactive => tracing::info!(
                "native Wayland remote keyboard shortcuts inactive (compositor/user choice)"
            ),
            _ => {}
        }
    }
}
