//! Native application activation is explicit for CLI and packaged launches.
#![cfg_attr(target_os = "macos", allow(unsafe_code))]
use winit::event_loop::EventLoop;

#[cfg(target_os = "linux")]
mod wayland;
#[cfg(target_os = "linux")]
pub(super) use wayland::KeyboardShortcuts;

#[cfg(not(target_os = "linux"))]
pub(super) struct KeyboardShortcuts;
#[cfg(not(target_os = "linux"))]
impl KeyboardShortcuts {
    pub fn new(
        _window: std::sync::Arc<winit::window::Window>,
        _display: winit::event_loop::OwnedDisplayHandle,
    ) -> Option<Self> {
        None
    }
    pub fn set_active(&mut self, _active: bool) {}
}

/// Own-window diagnostics, read on the OS main thread. No title/content/geometry.
#[derive(Clone, Copy, Debug, serde::Serialize)]
pub struct NativeWindowState {
    pub sampled_elapsed_ms: u64,
    pub application_active: bool,
    pub visible: bool,
    pub key_window: bool,
    pub on_active_space: bool,
    pub miniaturized: bool,
    pub occlusion_visible: bool,
}

#[cfg(target_os = "macos")]
pub(super) fn window_state(
    window: &winit::window::Window,
    elapsed_ms: u64,
) -> Option<NativeWindowState> {
    use objc2_app_kit::{NSApplication, NSView, NSWindowOcclusionState};
    use objc2_foundation::MainThreadMarker;
    use winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};
    let main = MainThreadMarker::new()?;
    let RawWindowHandle::AppKit(handle) = window.window_handle().ok()?.as_raw() else {
        return None;
    };
    // SAFETY: winit supplies this live NSView for our strongly owned Window.
    // The window and retained NSWindow outlive every read, and `main` verifies
    // AppKit's main-thread requirement. We only inspect our own window flags.
    let view = unsafe { &*handle.ns_view.as_ptr().cast::<NSView>() };
    let native = view.window()?;
    Some(NativeWindowState {
        sampled_elapsed_ms: elapsed_ms,
        application_active: NSApplication::sharedApplication(main).isActive(),
        visible: native.isVisible(),
        key_window: native.isKeyWindow(),
        on_active_space: native.isOnActiveSpace(),
        miniaturized: native.isMiniaturized(),
        occlusion_visible: native
            .occlusionState()
            .contains(NSWindowOcclusionState::Visible),
    })
}

#[cfg(not(target_os = "macos"))]
pub(super) fn window_state(
    _window: &winit::window::Window,
    _elapsed_ms: u64,
) -> Option<NativeWindowState> {
    None
}

#[cfg(target_os = "macos")]
pub(super) struct RemoteActivity(
    objc2::rc::Retained<objc2::runtime::ProtocolObject<dyn objc2_foundation::NSObjectProtocol>>,
);

#[cfg(target_os = "macos")]
impl Drop for RemoteActivity {
    fn drop(&mut self) {
        // SAFETY: the owned token came from this process's activity API and
        // ends once, when the native viewer event loop returns or unwinds.
        unsafe { objc2_foundation::NSProcessInfo::processInfo().endActivity(&self.0) };
    }
}

#[cfg(target_os = "macos")]
fn activity_options() -> objc2_foundation::NSActivityOptions {
    use objc2_foundation::NSActivityOptions;
    NSActivityOptions::UserInitiated
        | NSActivityOptions::LatencyCritical
        | NSActivityOptions::AutomaticTerminationDisabled
        | NSActivityOptions::SuddenTerminationDisabled
}

#[cfg(target_os = "macos")]
pub(super) fn remote_activity() -> RemoteActivity {
    use objc2_foundation::{NSProcessInfo, NSString};
    // Foundation owns the retained activity token; the RAII guard
    // pairs its begin/end. This user-requested stream must remain responsive
    // while covered and inactive. A session-scoped activity prevents idle
    // system sleep (network I/O cannot progress in suspend) and requests
    // latency-critical timer/I/O precision. Display sleep, screen locking,
    // explicit user sleep and lid-close policy remain under OS control.
    let token = NSProcessInfo::processInfo().beginActivityWithOptions_reason(
        activity_options(),
        &NSString::from_str("Remote desktop session"),
    );
    RemoteActivity(token)
}

#[cfg(not(target_os = "macos"))]
pub(super) struct RemoteActivity;

#[cfg(not(target_os = "macos"))]
pub(super) fn remote_activity() -> RemoteActivity {
    RemoteActivity
}

#[cfg(target_os = "macos")]
pub(super) fn paste_text() -> Result<Option<String>, crate::DesktopError> {
    read_pasteboard(&objc2_app_kit::NSPasteboard::generalPasteboard())
}

#[cfg(target_os = "macos")]
fn read_pasteboard(
    board: &objc2_app_kit::NSPasteboard,
) -> Result<Option<String>, crate::DesktopError> {
    use objc2_app_kit::NSPasteboardTypeString;
    use objc2_foundation::{MainThreadMarker, NSUTF8StringEncoding};
    let _main = MainThreadMarker::new().ok_or_else(|| {
        crate::DesktopError::Input("clipboard access requires the main thread".into())
    })?;
    // SAFETY: called for an explicit paste gesture on AppKit's verified main
    // thread. Retained Cocoa objects live through the copy; nothing is logged.
    let Some(text) = (unsafe { board.stringForType(NSPasteboardTypeString) }) else {
        return Ok(None);
    };
    // UTF-8 cannot use fewer bytes than UTF-16 code units. This cheap bound
    // also avoids scanning an arbitrarily large native string for conversion.
    if text.length() > crate::clipboard::MAX_TEXT_BYTES {
        return Err(crate::DesktopError::Input(
            "clipboard text exceeds 1 MiB".into(),
        ));
    }
    if !text.canBeConvertedToEncoding(NSUTF8StringEncoding) {
        return Err(crate::DesktopError::Input(
            "clipboard text cannot be represented as UTF-8".into(),
        ));
    }
    // Check Cocoa's byte count before allocating the Rust copy. A user may
    // have copied a native value much larger than the wire transfer ceiling.
    if text.lengthOfBytesUsingEncoding(NSUTF8StringEncoding) > crate::clipboard::MAX_TEXT_BYTES {
        return Err(crate::DesktopError::Input(
            "clipboard text exceeds 1 MiB".into(),
        ));
    }
    Ok(Some(text.to_string()))
}

#[cfg(target_os = "macos")]
pub(super) fn clipboard_generation() -> Result<i64, crate::DesktopError> {
    let _main = objc2_foundation::MainThreadMarker::new().ok_or_else(|| {
        crate::DesktopError::Input("clipboard access requires the main thread".into())
    })?;
    Ok(objc2_app_kit::NSPasteboard::generalPasteboard().changeCount() as i64)
}
#[cfg(not(target_os = "macos"))]
pub(super) fn clipboard_generation() -> Result<i64, crate::DesktopError> {
    Err(crate::DesktopError::Input(
        "clipboard publication unavailable".into(),
    ))
}

/// Publish a bounded UTF-8 value to the viewer's native clipboard.  This is
/// called only after a remote offer was explicitly requested and fully
/// reassembled, so a malformed peer cannot partially replace the user's
/// clipboard.
#[cfg(target_os = "macos")]
pub(super) fn publish_text(text: &str) -> Result<(), crate::DesktopError> {
    write_pasteboard(&objc2_app_kit::NSPasteboard::generalPasteboard(), text)
}
#[cfg(target_os = "macos")]
fn write_pasteboard(
    board: &objc2_app_kit::NSPasteboard,
    text: &str,
) -> Result<(), crate::DesktopError> {
    use objc2_app_kit::NSPasteboardTypeString;
    use objc2_foundation::{MainThreadMarker, NSString};
    let _main = MainThreadMarker::new().ok_or_else(|| {
        crate::DesktopError::Input("clipboard access requires the main thread".into())
    })?;
    if text.len() > crate::clipboard::MAX_TEXT_BYTES {
        return Err(crate::DesktopError::Input(
            "clipboard text exceeds 1 MiB".into(),
        ));
    }
    // SAFETY: AppKit's thread is verified and NSString remains retained for
    // the synchronous operation; no clipboard content is diagnostic output.
    unsafe {
        board.clearContents();
        if !board.setString_forType(&NSString::from_str(text), NSPasteboardTypeString) {
            return Err(crate::DesktopError::Input(
                "native clipboard publication refused".into(),
            ));
        }
    }
    Ok(())
}
/// Isolated AppKit qualification on a fresh named pasteboard, never the user's
/// general clipboard. Must run on the native main thread.
#[doc(hidden)]
#[cfg(target_os = "macos")]
pub fn native_clipboard_probe() -> Result<u32, crate::DesktopError> {
    use objc2_app_kit::{NSPasteboard, NSPasteboardTypeString};
    use objc2_foundation::NSString;
    let _main = objc2_foundation::MainThreadMarker::new().ok_or_else(|| {
        crate::DesktopError::Input("clipboard probe requires the main thread".into())
    })?;
    let board = NSPasteboard::pasteboardWithUniqueName();
    let result = (|| {
        for text in [
            String::new(),
            "Привет 🖥️\nHello".into(),
            "UTF-8 🖥️\n".repeat(8000),
        ] {
            let before = board.changeCount();
            write_pasteboard(&board, &text)?;
            // SAFETY: the isolated board and retained returned string are
            // accessed synchronously on the verified AppKit main thread.
            let read = read_pasteboard(&board)?;
            if read.as_deref() != Some(text.as_str()) || board.changeCount() == before {
                return Err(crate::DesktopError::Input(
                    "native clipboard probe mismatch".into(),
                ));
            }
        }
        let oversized = "x".repeat(crate::clipboard::MAX_TEXT_BYTES + 1);
        let before = board.changeCount();
        if write_pasteboard(&board, &oversized).is_ok() || board.changeCount() != before {
            return Err(crate::DesktopError::Input(
                "native clipboard oversized publication was not refused".into(),
            ));
        }
        // SAFETY: the probe owns this named board on the verified main thread.
        // Seed it directly to exercise the independent native-read bound.
        unsafe {
            board.clearContents();
            if !board.setString_forType(&NSString::from_str(&oversized), NSPasteboardTypeString) {
                return Err(crate::DesktopError::Input(
                    "native clipboard oversized fixture was refused".into(),
                ));
            }
        }
        if read_pasteboard(&board).is_ok() {
            return Err(crate::DesktopError::Input(
                "native clipboard oversized read was not refused".into(),
            ));
        }
        Ok(4)
    })();
    board.clearContents();
    result
}
#[doc(hidden)]
#[cfg(not(target_os = "macos"))]
pub fn native_clipboard_probe() -> Result<u32, crate::DesktopError> {
    Err(crate::DesktopError::Input(
        "native AppKit probe requires macOS".into(),
    ))
}
#[cfg(not(target_os = "macos"))]
pub(super) fn paste_text() -> Result<Option<String>, crate::DesktopError> {
    Ok(None)
}

#[cfg(not(target_os = "macos"))]
pub(super) fn publish_text(_text: &str) -> Result<(), crate::DesktopError> {
    Err(crate::DesktopError::Input(
        "native clipboard publication is unavailable on this viewer".into(),
    ))
}

#[cfg(target_os = "macos")]
pub fn choose_resolution() -> Option<u32> {
    use objc2_app_kit::{NSAlert, NSApplication};
    use objc2_foundation::{MainThreadMarker, NSString};
    let main = MainThreadMarker::new()?;
    let app = NSApplication::sharedApplication(main);
    // All AppKit operations run on its verified main thread, with
    // retained strings/buttons for the modal lifetime; no user data is parsed.
    let response = {
        let alert = NSAlert::new(main);
        app.setActivationPolicy(objc2_app_kit::NSApplicationActivationPolicy::Regular);
        alert.setMessageText(&NSString::from_str("RDS — Video quality"));
        alert.setInformativeText(&NSString::from_str("Full HD is the default. Choose 720p for a slower connection, or Original for the remote screen's native resolution."));
        for title in ["Full HD · 1080p", "HD · 720p", "Original", "Cancel"] {
            alert.addButtonWithTitle(&NSString::from_str(title));
        }
        activate_application();
        alert.runModal()
    };
    match response {
        1000 => Some(1080),
        1001 => Some(720),
        1002 => Some(0),
        _ => None,
    }
}

#[cfg(not(target_os = "macos"))]
pub fn choose_resolution() -> Option<u32> {
    Some(1080)
}

#[cfg(target_os = "macos")]
pub(super) fn event_loop() -> Result<EventLoop<()>, winit::error::EventLoopError> {
    use objc2::AnyThread;
    use objc2_app_kit::{NSApplication, NSImage};
    use objc2_foundation::{MainThreadMarker, NSData};
    use winit::platform::macos::{ActivationPolicy, EventLoopBuilderExtMacOS};
    let event_loop = EventLoop::with_user_event()
        .with_activation_policy(ActivationPolicy::Regular)
        .build()?;
    // Winit validates the OS main thread before constructing this loop.
    if let Some(main) = MainThreadMarker::new() {
        let data = NSData::with_bytes(include_bytes!("../../assets/app-icon.png"));
        if let Some(icon) = NSImage::initWithData(NSImage::alloc(), &data) {
            // SAFETY: `main` proves AppKit's main-thread requirement. The
            // embedded PNG is owned by NSData and AppKit retains the NSImage.
            unsafe { NSApplication::sharedApplication(main).setApplicationIconImage(Some(&icon)) };
        }
    }
    Ok(event_loop)
}

#[cfg(not(target_os = "macos"))]
pub(super) fn event_loop() -> Result<EventLoop<()>, winit::error::EventLoopError> {
    EventLoop::with_user_event().build()
}

#[cfg(target_os = "macos")]
pub(super) fn window_attributes() -> winit::window::WindowAttributes {
    use winit::platform::macos::{OptionAsAlt, WindowAttributesExtMacOS};
    winit::window::Window::default_attributes().with_option_as_alt(OptionAsAlt::Both)
}

#[cfg(not(target_os = "macos"))]
pub(super) fn window_attributes() -> winit::window::WindowAttributes {
    winit::window::Window::default_attributes()
}

#[cfg(target_os = "macos")]
pub(super) fn activate_application() {
    use objc2_app_kit::NSApplication;
    use objc2_foundation::MainThreadMarker;
    if let Some(main) = MainThreadMarker::new() {
        // SAFETY: explicit viewer launch on the verified AppKit main thread.
        // Request application activation after its window exists; window focus
        // alone does not activate a background application on current macOS.
        let app = NSApplication::sharedApplication(main);
        // The modern request exists on macOS 14+. Preserve the advertised
        // older system floor without sending an unsupported Objective-C selector.
        unsafe {
            let modern: bool = objc2::msg_send![&*app, respondsToSelector: objc2::sel!(activate)];
            if modern {
                app.activate();
            } else {
                // Required compatibility path only when the modern selector is absent.
                #[allow(deprecated)]
                app.activateIgnoringOtherApps(true);
            }
        }
    }
}

#[cfg(not(target_os = "macos"))]
pub(super) fn activate_application() {}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    #[test]
    fn session_prevents_idle_system_sleep_without_holding_the_display() {
        use objc2_foundation::NSActivityOptions;
        let options = super::activity_options();
        assert!(options.contains(NSActivityOptions::IdleSystemSleepDisabled));
        assert!(!options.contains(NSActivityOptions::IdleDisplaySleepDisabled));
        assert!(options.contains(NSActivityOptions::LatencyCritical));
    }
}
