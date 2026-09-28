//! Frame presentation.
//!
//! `wgpu` surface with our own YUV→RGB conversion; policy is
//! newest-frame-only (decode and present the freshest frame, drop the
//! rest). Hardware decode lands as `wgpu::Texture` from the Vulkan
//! Video path; the software path uploads BGRA via `queue.write_texture`.
//!
//! Optional console client: DRM atomic direct present on Linux.
//!
//! Interim viewer (`viewer` feature): a `winit` window presenting decoded
//! BGRA frames through `softbuffer` and forwarding input as the evdev-coded
//! `InputKind` wire protocol. Built for macOS/Windows/X11 client surfaces
//! where no hardware decode path exists yet.

/// True when the `viewer` feature is compiled in.
pub fn available() -> bool {
    cfg!(feature = "viewer")
}

#[cfg(feature = "viewer")]
pub use imp::{RenderError, run};

#[cfg(feature = "viewer")]
mod imp {
    use crate::RawFrame;
    use rds_core::{DesktopControl, InputEvent, InputKind};
    use softbuffer::{Context, Surface};
    use std::num::NonZeroU32;
    use std::rc::Rc;
    use std::sync::mpsc::{Receiver, Sender};
    use std::time::{SystemTime, UNIX_EPOCH};
    use thiserror::Error;
    use winit::application::ApplicationHandler;
    use winit::event::{ElementState, MouseButton, MouseScrollDelta, WindowEvent};
    use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
    use winit::keyboard::ModifiersState;
    use winit::keyboard::{KeyCode, PhysicalKey};
    use winit::window::{Fullscreen, Window, WindowAttributes, WindowId};

    #[derive(Debug, Error)]
    pub enum RenderError {
        #[error("event loop: {0}")]
        EventLoop(#[from] winit::error::EventLoopError),
        #[error("OS window: {0}")]
        Os(#[from] winit::error::OsError),
        #[error("present: {0}")]
        Present(#[from] softbuffer::SoftBufferError),
    }

    /// Letterbox fit of the remote frame inside the window. Pointer input
    /// maps through the same transform.
    #[derive(Clone, Copy)]
    struct Fit {
        x: f64,
        y: f64,
        w: f64,
        h: f64,
    }

    fn fit(sw: u32, sh: u32, rw: u32, rh: u32) -> Fit {
        if sw == 0 || sh == 0 || rw == 0 || rh == 0 {
            return Fit {
                x: 0.0,
                y: 0.0,
                w: sw as f64,
                h: sh as f64,
            };
        }
        let scale = (sw as f64 / rw as f64).min(sh as f64 / rh as f64);
        let w = rw as f64 * scale;
        let h = rh as f64 * scale;
        Fit {
            x: (sw as f64 - w) / 2.0,
            y: (sh as f64 - h) / 2.0,
            w,
            h,
        }
    }

    /// Nearest-neighbour letterbox blit into a window-sized surface
    /// buffer. Bars stay whatever the buffer already holds (black).
    fn blit(surface: &mut Surface<Rc<Window>, Rc<Window>>, window: &Window, frame: &RawFrame) {
        let (rw, rh) = (frame.width as usize, frame.height as usize);
        if rw == 0 || rh == 0 {
            return;
        }
        let size = window.inner_size();
        let (sw, sh) = (size.width as usize, size.height as usize);
        if sw == 0 || sh == 0 {
            return;
        }
        let _ = surface.resize(
            NonZeroU32::new(size.width.max(1)).unwrap(),
            NonZeroU32::new(size.height.max(1)).unwrap(),
        );
        let Ok(mut buf) = surface.buffer_mut() else {
            eprintln!("dbg blit: buffer_mut failed");
            return;
        };
        let rect = fit(size.width, size.height, frame.width, frame.height);
        let (ox, oy) = (rect.x as usize, rect.y as usize);
        let (dw, dh) = (
            (rect.w.max(1.0) as usize).min(sw.saturating_sub(ox)),
            (rect.h.max(1.0) as usize).min(sh.saturating_sub(oy)),
        );
        if dw == 0 || dh == 0 {
            eprintln!("dbg blit: zero dst {dw}x{dh} win {sw}x{sh} remote {rw}x{rh}");
            return;
        }
        let src = &frame.data[..];
        let stride = frame.stride as usize;
        for dy in 0..dh {
            let sy = dy * rh / dh;
            let srow = sy * stride;
            let drow = (oy + dy) * sw + ox;
            for dx in 0..dw {
                let sx = dx * rw / dw;
                let i = srow + sx * 4;
                if i + 2 >= src.len() {
                    break;
                }
                buf[drow + dx] =
                    ((src[i + 2] as u32) << 16) | ((src[i + 1] as u32) << 8) | src[i] as u32;
            }
        }
        if let Err(e) = buf.present() {
            eprintln!("dbg blit: present failed: {e}");
        }
    }

    struct App {
        ctrl: Sender<DesktopControl>,
        display_id: u32,
        seq: u64,
        window: Option<Rc<Window>>,
        surface: Option<Surface<Rc<Window>, Rc<Window>>>,
        /// Freshest decoded frame not yet presented — user events keep
        /// overwriting it so only the newest survives to `about_to_wait`.
        incoming: Option<RawFrame>,
        /// Last presented frame, retained for resize re-blit.
        shown: Option<RawFrame>,
        remote_w: u32,
        remote_h: u32,
        /// Native display size input coordinates target. Frames may be
        /// downscaled server-side while the X screen stays native, so
        /// pointer mapping uses these, not the decoded frame size.
        input_w: u32,
        input_h: u32,
        stats_frames: u64,
        stats_start: std::time::Instant,
        modifiers: ModifiersState,
        last_move: std::time::Instant,
    }

    impl App {
        fn now_ms() -> u64 {
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0)
        }

        fn send(&mut self, kind: InputKind) {
            self.seq += 1;
            if self.seq <= 3 || self.seq.is_multiple_of(200) {
                eprintln!("dbg: viewer input seq={} kind={:?}", self.seq, kind);
            }
            let ev = InputEvent {
                seq: self.seq,
                event_ts_ms: Self::now_ms(),
                display_id: self.display_id,
                kind,
            };
            let _ = self.ctrl.send(DesktopControl::Input(ev));
        }

        /// Move the newest incoming frame to `shown` and present it.
        fn present_newest(&mut self) {
            if let Some(f) = self.incoming.take() {
                self.remote_w = f.width;
                self.remote_h = f.height;
                self.shown = Some(f);
            }
            self.reblit();
        }

        fn reblit(&mut self) {
            let (Some(shown), Some(surface), Some(window)) = (
                self.shown.as_ref(),
                self.surface.as_mut(),
                self.window.as_ref(),
            ) else {
                return;
            };
            blit(surface, window, shown);
            self.stats_frames += 1;
        }

        /// Window coordinates → remote frame coordinates through the same
        /// letterbox transform used by `blit`.
        fn to_remote(&self, px: f64, py: f64) -> Option<(f64, f64)> {
            let w = self.window.as_ref()?;
            let s = w.inner_size();
            let rect = fit(s.width, s.height, self.remote_w, self.remote_h);
            let (iw, ih) = (self.input_w.max(1), self.input_h.max(1));
            if rect.w <= 0.0 || rect.h <= 0.0 || self.remote_w == 0 {
                return None;
            }
            let x = (px - rect.x) * iw as f64 / rect.w;
            let y = (py - rect.y) * ih as f64 / rect.h;
            if x < 0.0 || y < 0.0 || x >= iw as f64 || y >= ih as f64 {
                return None;
            }
            Some((x, y))
        }

        fn toggle_fullscreen(&self) {
            if let Some(w) = &self.window {
                let next = if w.fullscreen().is_some() {
                    None
                } else {
                    Some(Fullscreen::Borderless(None))
                };
                w.set_fullscreen(next);
            }
        }

        /// Paste the OS clipboard as synthesized keystrokes — remote
        /// desktops get no clipboard event through the wire protocol, so
        /// text is typed character by character (US layout).
        fn paste_clipboard(&mut self) {
            let Some(text) = clipboard_text() else {
                return;
            };
            let mut shift_down = false;
            for ch in text.chars().take(4096) {
                let Some((code, shift)) = char_evdev(ch) else {
                    continue;
                };
                if shift && !shift_down {
                    self.send(InputKind::KeyDown { code: 42 });
                    shift_down = true;
                } else if !shift && shift_down {
                    self.send(InputKind::KeyUp { code: 42 });
                    shift_down = false;
                }
                self.send(InputKind::KeyDown { code });
                self.send(InputKind::KeyUp { code });
            }
            if shift_down {
                self.send(InputKind::KeyUp { code: 42 });
            }
        }
    }

    /// Read the OS clipboard without extra dependencies.
    fn clipboard_text() -> Option<String> {
        #[cfg(target_os = "macos")]
        let (prog, args): (&str, &[&str]) = ("pbpaste", &[]);
        #[cfg(all(target_os = "linux", not(target_os = "macos")))]
        let (prog, args): (&str, &[&str]) = ("wl-paste", &["-n"]);
        #[cfg(not(any(target_os = "macos", target_os = "linux")))]
        return None;
        #[cfg(any(target_os = "macos", target_os = "linux"))]
        std::process::Command::new(prog)
            .args(args)
            .output()
            .ok()
            .and_then(|o| o.status.success().then_some(o.stdout))
            .and_then(|b| String::from_utf8(b).ok())
    }

    /// US-layout character → (evdev code, needs-LeftShift). `\n` maps to
    /// Enter; characters without a plain key are dropped.
    fn char_evdev(ch: char) -> Option<(u32, bool)> {
        Some(match ch {
            'a'..='z' => (
                match ch {
                    'q' => 16,
                    'w' => 17,
                    'e' => 18,
                    'r' => 19,
                    't' => 20,
                    'y' => 21,
                    'u' => 22,
                    'i' => 23,
                    'o' => 24,
                    'p' => 25,
                    'a' => 30,
                    's' => 31,
                    'd' => 32,
                    'f' => 33,
                    'g' => 34,
                    'h' => 35,
                    'j' => 36,
                    'k' => 37,
                    'l' => 38,
                    'z' => 44,
                    'x' => 45,
                    'c' => 46,
                    'v' => 47,
                    'b' => 48,
                    'n' => 49,
                    'm' => 50,
                    _ => unreachable!(),
                },
                false,
            ),
            'A'..='Z' => {
                let (code, _) = char_evdev(ch.to_ascii_lowercase())?;
                (code, true)
            }
            '1'..='9' => (ch as u32 - '1' as u32 + 2, false),
            '0' => (11, false),
            '!' => (2, true),
            '@' => (3, true),
            '#' => (4, true),
            '$' => (5, true),
            '%' => (6, true),
            '^' => (7, true),
            '&' => (8, true),
            '*' => (9, true),
            '(' => (10, true),
            ')' => (11, true),
            '-' => (12, false),
            '_' => (12, true),
            '=' => (13, false),
            '+' => (13, true),
            '[' => (26, false),
            '{' => (26, true),
            ']' => (27, false),
            '}' => (27, true),
            ';' => (39, false),
            ':' => (39, true),
            '\'' => (40, false),
            '"' => (40, true),
            '`' => (41, false),
            '~' => (41, true),
            '\\' => (43, false),
            '|' => (43, true),
            ',' => (51, false),
            '<' => (51, true),
            '.' => (52, false),
            '>' => (52, true),
            '/' => (53, false),
            '?' => (53, true),
            ' ' => (57, false),
            '\n' | '\r' => (28, false),
            '\t' => (15, false),
            _ => return None,
        })
    }

    impl ApplicationHandler<RawFrame> for App {
        fn resumed(&mut self, el: &ActiveEventLoop) {
            if self.window.is_none() {
                let attrs = WindowAttributes::default()
                    .with_title("RDS Desktop")
                    .with_maximized(true);
                match el.create_window(attrs) {
                    Ok(w) => {
                        let w = Rc::new(w);
                        match Context::new(w.clone()).and_then(|ctx| Surface::new(&ctx, w.clone()))
                        {
                            Ok(s) => {
                                self.surface = Some(s);
                                self.window = Some(w);
                            }
                            Err(e) => {
                                tracing::error!("softbuffer surface init failed: {e}");
                                el.exit();
                            }
                        }
                    }
                    Err(e) => {
                        tracing::error!("window init failed: {e}");
                        el.exit();
                    }
                }
            }
        }

        /// One user event per decoded frame, sent by the pump thread.
        /// Multiple queued events collapse here: only the newest survives
        /// into `incoming` and gets presented once.
        fn user_event(&mut self, el: &ActiveEventLoop, frame: RawFrame) {
            if self.stats_frames == 0 && self.window.is_some() && frame.width > 0 {
                eprintln!("dbg: first frame -> incoming {}x{}", frame.width, frame.height);
            }
            if frame.width == 0 || frame.height == 0 {
                // Stream ended — close instead of freezing the last frame.
                el.exit();
                return;
            }
            self.incoming = Some(frame);
        }

        fn window_event(&mut self, el: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
            match event {
                WindowEvent::CloseRequested => el.exit(),
                WindowEvent::CursorMoved { position, .. } => {
                    // ~120 Hz cap: macOS can deliver far more move events
                    // than the control channel or the remote input queue
                    // can usefully consume.
                    if self.last_move.elapsed().as_millis() < 8 {
                        return;
                    }
                    if let Some((x, y)) = self.to_remote(position.x, position.y) {
                        self.last_move = std::time::Instant::now();
                        self.send(InputKind::PointerMove { x, y });
                    }
                }
                WindowEvent::MouseInput { state, button, .. } => {
                    let code = match button {
                        MouseButton::Left => 272,
                        MouseButton::Right => 273,
                        MouseButton::Middle => 274,
                        MouseButton::Back => 275,
                        MouseButton::Forward => 276,
                        MouseButton::Other(b) => b as i32,
                    };
                    self.send(InputKind::PointerButton {
                        button: code,
                        pressed: state == ElementState::Pressed,
                    });
                }
                WindowEvent::MouseWheel { delta, .. } => {
                    let (dx, dy) = match delta {
                        MouseScrollDelta::LineDelta(x, y) => (-x as f64, y as f64),
                        MouseScrollDelta::PixelDelta(p) => (-p.x / 40.0, p.y / 40.0),
                    };
                    if dx != 0.0 || dy != 0.0 {
                        self.send(InputKind::Scroll { dx, dy });
                    }
                }
                WindowEvent::ModifiersChanged(m) => {
                    self.modifiers = m.state();
                }
                WindowEvent::KeyboardInput { event, .. } => {
                    if let PhysicalKey::Code(code) = event.physical_key {
                        if code == KeyCode::F11 && event.state == ElementState::Pressed {
                            self.toggle_fullscreen();
                            return;
                        }
                        if code == KeyCode::KeyV
                            && event.state == ElementState::Pressed
                            && (self.modifiers.super_key()
                                || (self.modifiers.control_key() && self.modifiers.shift_key()))
                        {
                            // Cmd+V / Ctrl+Shift+V → type the clipboard
                            // remotely; the keys themselves are NOT
                            // forwarded so the remote doesn't also see V.
                            self.paste_clipboard();
                            return;
                        }
                        if let Some(ev) = evdev(code) {
                            let kind = if event.state == ElementState::Pressed {
                                InputKind::KeyDown { code: ev }
                            } else {
                                InputKind::KeyUp { code: ev }
                            };
                            self.send(kind);
                        }
                    }
                }
                WindowEvent::Resized(_) => self.reblit(),
                _ => {}
            }
        }

        fn about_to_wait(&mut self, _el: &ActiveEventLoop) {
            self.present_newest();
            if self.stats_frames.is_multiple_of(120) && self.stats_frames > 0 {
                let secs = self.stats_start.elapsed().as_secs_f64();
                let fps = self.stats_frames as f64 / secs;
                if let Some(w) = &self.window {
                    w.set_title(&format!(
                        "RDS Desktop — {}x{} {:.1} fps",
                        self.remote_w, self.remote_h, fps
                    ));
                }
            }
        }
    }

    /// Linux evdev key codes for the wire `InputKind` protocol.
    fn evdev(code: KeyCode) -> Option<u32> {
        Some(match code {
            KeyCode::Escape => 1,
            KeyCode::Digit1 => 2,
            KeyCode::Digit2 => 3,
            KeyCode::Digit3 => 4,
            KeyCode::Digit4 => 5,
            KeyCode::Digit5 => 6,
            KeyCode::Digit6 => 7,
            KeyCode::Digit7 => 8,
            KeyCode::Digit8 => 9,
            KeyCode::Digit9 => 10,
            KeyCode::Digit0 => 11,
            KeyCode::Minus => 12,
            KeyCode::Equal => 13,
            KeyCode::Backspace => 14,
            KeyCode::Tab => 15,
            KeyCode::KeyQ => 16,
            KeyCode::KeyW => 17,
            KeyCode::KeyE => 18,
            KeyCode::KeyR => 19,
            KeyCode::KeyT => 20,
            KeyCode::KeyY => 21,
            KeyCode::KeyU => 22,
            KeyCode::KeyI => 23,
            KeyCode::KeyO => 24,
            KeyCode::KeyP => 25,
            KeyCode::BracketLeft => 26,
            KeyCode::BracketRight => 27,
            KeyCode::Enter => 28,
            KeyCode::ControlLeft => 29,
            KeyCode::KeyA => 30,
            KeyCode::KeyS => 31,
            KeyCode::KeyD => 32,
            KeyCode::KeyF => 33,
            KeyCode::KeyG => 34,
            KeyCode::KeyH => 35,
            KeyCode::KeyJ => 36,
            KeyCode::KeyK => 37,
            KeyCode::KeyL => 38,
            KeyCode::Semicolon => 39,
            KeyCode::Quote => 40,
            KeyCode::Backquote => 41,
            KeyCode::ShiftLeft => 42,
            KeyCode::Backslash => 43,
            KeyCode::KeyZ => 44,
            KeyCode::KeyX => 45,
            KeyCode::KeyC => 46,
            KeyCode::KeyV => 47,
            KeyCode::KeyB => 48,
            KeyCode::KeyN => 49,
            KeyCode::KeyM => 50,
            KeyCode::Comma => 51,
            KeyCode::Period => 52,
            KeyCode::Slash => 53,
            KeyCode::ShiftRight => 54,
            KeyCode::NumpadMultiply => 55,
            KeyCode::AltLeft => 56,
            KeyCode::Space => 57,
            KeyCode::CapsLock => 58,
            KeyCode::F1 => 59,
            KeyCode::F2 => 60,
            KeyCode::F3 => 61,
            KeyCode::F4 => 62,
            KeyCode::F5 => 63,
            KeyCode::F6 => 64,
            KeyCode::F7 => 65,
            KeyCode::F8 => 66,
            KeyCode::F9 => 67,
            KeyCode::F10 => 68,
            KeyCode::NumLock => 69,
            KeyCode::ScrollLock => 70,
            KeyCode::Numpad7 => 71,
            KeyCode::Numpad8 => 72,
            KeyCode::Numpad9 => 73,
            KeyCode::NumpadSubtract => 74,
            KeyCode::Numpad4 => 75,
            KeyCode::Numpad5 => 76,
            KeyCode::Numpad6 => 77,
            KeyCode::NumpadAdd => 78,
            KeyCode::Numpad1 => 79,
            KeyCode::Numpad2 => 80,
            KeyCode::Numpad3 => 81,
            KeyCode::Numpad0 => 82,
            KeyCode::NumpadDecimal => 83,
            KeyCode::IntlBackslash => 86,
            KeyCode::F11 => 87,
            KeyCode::F12 => 88,
            KeyCode::NumpadEqual => 117,
            KeyCode::F13 => 183,
            KeyCode::F14 => 184,
            KeyCode::F15 => 185,
            KeyCode::ControlRight => 97,
            KeyCode::NumpadDivide => 98,
            KeyCode::AltRight => 100,
            KeyCode::Home => 102,
            KeyCode::ArrowUp => 103,
            KeyCode::PageUp => 104,
            KeyCode::ArrowLeft => 105,
            KeyCode::ArrowRight => 106,
            KeyCode::End => 107,
            KeyCode::ArrowDown => 108,
            KeyCode::PageDown => 109,
            KeyCode::Insert => 110,
            KeyCode::Delete => 111,
            KeyCode::NumpadEnter => 96,
            KeyCode::SuperLeft => 125,
            KeyCode::SuperRight => 126,
            KeyCode::ContextMenu => 127,
            _ => return None,
        })
    }

    /// Blocking viewer loop: must run on the main thread (macOS windowing).
    /// `frames` receives decoded BGRA frames from the pump task; `ctrl` is
    /// the session's control sender for input injection.
    pub fn run(
        frames: Receiver<RawFrame>,
        ctrl: Sender<DesktopControl>,
        display_id: u32,
        input_size: (u32, u32),
    ) -> Result<(), RenderError> {
        let el: EventLoop<RawFrame> = EventLoop::with_user_event().build()?;
        // `Wait` + proxy wake: the loop sleeps between frames instead of
        // spinning a core at 100%.
        el.set_control_flow(ControlFlow::Wait);
        let proxy = el.create_proxy();
        std::thread::spawn(move || {
            while let Ok(f) = frames.recv() {
                if proxy.send_event(f).is_err() {
                    break;
                }
            }
        });
        let mut app = App {
            ctrl,
            display_id,
            seq: 0,
            window: None,
            surface: None,
            incoming: None,
            shown: None,
            remote_w: 0,
            remote_h: 0,
            input_w: input_size.0,
            input_h: input_size.1,
            stats_frames: 0,
            stats_start: std::time::Instant::now(),
            modifiers: ModifiersState::default(),
            last_move: std::time::Instant::now(),
        };
        el.run_app(&mut app)?;
        Ok(())
    }
}
