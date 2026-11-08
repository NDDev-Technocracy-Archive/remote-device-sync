//! A single deferred paste across native Spaces, fenced to its exact destination.
use super::*;
use std::time::Duration;

pub(super) struct PendingPaste {
    tab: TabId,
    revision: u64,
    session_epoch: u64,
    copy_epoch: u64,
    generation: i64,
    shift: bool,
    pub(super) deadline: Instant,
}
impl Screens {
    pub(super) fn paste_event(
        &mut self,
        event_loop: &ActiveEventLoop,
        window: WindowId,
        event: &WindowEvent,
    ) -> bool {
        if event_loop.exiting() {
            self.pending_paste = None;
            return false;
        }
        let destination = self
            .panes
            .iter()
            .find_map(|(id, app)| {
                app.window
                    .as_ref()
                    .is_some_and(|w| w.id() == window)
                    .then_some(*id)
            })
            .flatten();
        if matches!(
            event,
            WindowEvent::Focused(false)
                | WindowEvent::CursorMoved { .. }
                | WindowEvent::MouseInput { .. }
                | WindowEvent::MouseWheel { .. }
        ) || matches!(event,WindowEvent::KeyboardInput {event,..} if event.state == ElementState::Pressed && !event.repeat)
        {
            self.pending_paste = None;
        }
        let Some(id) = destination else {
            return false;
        };
        let Some(app) = self.panes.get_mut(&Some(id)) else {
            return false;
        };
        let gesture = matches!(event,WindowEvent::KeyboardInput {event,is_synthetic:false,..}
            if event.state == ElementState::Pressed && !event.repeat && event.physical_key == PhysicalKey::Code(winit::keyboard::KeyCode::KeyV))
            && (app.modifiers.state().control_key()
                || (cfg!(target_os = "macos") && app.modifiers.state().super_key()));
        let Some(tab) = self.model.tab(id) else {
            return false;
        };
        let enabled = tab.spec.profile.interactive
            && tab.spec.profile.clipboard
            && app.window.as_ref().is_some_and(|w| w.has_focus())
            && !app
                .workspace
                .as_ref()
                .and_then(|deck| deck.chrome.as_ref())
                .is_some_and(|chrome| chrome.editing());
        if !gesture || !enabled {
            return false;
        }
        let Some(copy) = self.clipboard.snapshot().filter(|copy| copy.pending) else {
            return false;
        };
        let Ok(generation) = super::super::super::platform::clipboard_generation() else {
            return true;
        };
        self.pending_paste = Some(PendingPaste {
            tab: id,
            revision: tab.revision,
            session_epoch: lock(&app.session.handle.state).report.session_epoch,
            copy_epoch: copy.epoch,
            generation,
            shift: app.modifiers.state().shift_key(),
            deadline: Instant::now() + Duration::from_secs(5),
        });
        // Reconcile held flags now, but never forward V before its text arrives.
        app.sync_modifiers(event_loop);
        self.message("Waiting for copied text…");
        true
    }
    pub(super) fn check_paste(&mut self, event_loop: &ActiveEventLoop) {
        if event_loop.exiting() {
            self.pending_paste = None;
            return;
        }
        let Some(paste) = self.pending_paste.take() else {
            return;
        };
        let copy = self.clipboard.snapshot();
        let generation = super::super::super::platform::clipboard_generation().ok();
        let Some(app) = self.panes.get_mut(&Some(paste.tab)) else {
            return;
        };
        let valid = self.model.tab(paste.tab).is_some_and(|tab| {
            tab.revision == paste.revision
                && tab.spec.profile.clipboard
                && tab.spec.profile.interactive
        }) && lock(&app.session.handle.state).report.session_epoch
            == paste.session_epoch
            && app.window.as_ref().is_some_and(|w| w.has_focus())
            && !app
                .workspace
                .as_ref()
                .and_then(|deck| deck.chrome.as_ref())
                .is_some_and(|chrome| chrome.editing())
            && Instant::now() < paste.deadline;
        if !valid || copy.is_none_or(|copy| copy.epoch != paste.copy_epoch) {
            self.message("Paste canceled: the source or destination changed");
            return;
        }
        let copy = copy.expect("validated copy snapshot");
        if copy.pending && generation == Some(paste.generation) {
            self.pending_paste = Some(paste);
            return;
        }
        if copy.published_generation.is_some() && copy.published_generation == generation {
            let chord =
                super::super::super::input::deferred_paste_chord(&app.session.keys, paste.shift);
            if app.paste_text(true, chord.len()) {
                for kind in chord {
                    app.input(event_loop, kind);
                }
            }
            self.message("Copied text ready");
        } else {
            self.message(
                "Paste canceled: copied text did not arrive or clipboard ownership changed",
            );
        }
    }
}
