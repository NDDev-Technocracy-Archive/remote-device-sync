//! One conservative session selection boundary for Linux capture and input.
pub(crate) fn is_wayland(socket_present: bool, session_type: Option<&str>) -> bool {
    socket_present || session_type.is_some_and(|kind| kind.eq_ignore_ascii_case("wayland"))
}

/// Xwayland's DISPLAY is not proof that the full screen is an X11 desktop.
pub fn wayland_session() -> bool {
    is_wayland(
        std::env::var_os("WAYLAND_DISPLAY").is_some(),
        std::env::var("XDG_SESSION_TYPE").ok().as_deref(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn either_wayland_signal_prevents_xwayland_fallback() {
        assert!(is_wayland(true, Some("x11")));
        assert!(is_wayland(false, Some("Wayland")));
        assert!(is_wayland(true, None));
        assert!(!is_wayland(false, Some("x11")));
        assert!(!is_wayland(false, None));
    }
}
