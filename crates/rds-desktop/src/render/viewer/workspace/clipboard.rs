//! One native pasteboard, bounded per-session mailboxes and one explicit paste.
use super::*;
use std::time::Duration;

const PASTE_WAIT: Duration = Duration::from_secs(5);

pub(super) struct PendingPaste {
    tab: TabId,
    revision: u64,
    epoch: u64,
    generation: i64,
    shift: bool,
    deadline: Instant,
}
impl PendingPaste {
    fn valid(&self, tab: TabId, revision: u64, epoch: u64, now: Instant) -> bool {
        self.tab == tab && self.revision == revision && self.epoch == epoch && now < self.deadline
    }
}
impl Deck {
    pub(in super::super) fn cancel_clipboard_tab(&mut self, id: TabId) {
        if self.clipboard_owner == Some(id) {
            self.clipboard_owner = None;
            if self.pending_paste.take().is_some() {
                self.clipboard_message("Paste canceled: the source session changed");
            }
        }
        if self.pending_paste.as_ref().is_some_and(|p| p.tab == id) {
            self.pending_paste = None;
        }
    }
    pub(super) fn clipboard_message(&mut self, message: &str) {
        self.message = message.into();
        if let Some(chrome) = &mut self.chrome {
            chrome.message = self.message.clone();
        }
    }
}
impl App {
    pub(in super::super) fn workspace_clipboard_enabled(&self) -> bool {
        self.workspace.as_ref().is_none_or(|deck| {
            deck.active
                .and_then(|id| deck.model.tab(id))
                .is_some_and(|tab| tab.spec.profile.clipboard)
        })
    }
    pub(in super::super) fn workspace_copying(&mut self) {
        let Ok(generation) = super::super::super::platform::clipboard_generation() else {
            return;
        };
        if let Some(deck) = &mut self.workspace {
            // The newest explicit Copy wins even before native ownership changes.
            for session in deck.inactive.values() {
                lock(&session.handle.state).clipboard.reset();
            }
            deck.clipboard_owner = deck.active;
            deck.pending_paste = None;
        }
        lock(&self.session.handle.state)
            .clipboard
            .copying(generation);
    }
    pub(in super::super) fn workspace_defer_paste(&mut self, event_loop: &ActiveEventLoop) -> bool {
        if self.workspace.is_none() || !self.workspace_clipboard_enabled() {
            return false;
        }
        self.clipboard_work(event_loop);
        let Some(deck) = &mut self.workspace else {
            return false;
        };
        if deck.clipboard_owner.is_none() {
            return false;
        }
        let Some(tab) = deck.active.and_then(|id| deck.model.tab(id)) else {
            return true;
        };
        let Ok(generation) = super::super::super::platform::clipboard_generation() else {
            deck.clipboard_message("Paste canceled: native clipboard is unavailable");
            return true;
        };
        deck.pending_paste = Some(PendingPaste {
            tab: tab.id,
            revision: tab.revision,
            epoch: lock(&self.session.handle.state).report.session_epoch,
            generation,
            shift: self.session.keys.contains(&42) || self.session.keys.contains(&54),
            deadline: Instant::now() + PASTE_WAIT,
        });
        deck.clipboard_message("Waiting for copied text…");
        true
    }
    pub(in super::super) fn workspace_clipboard_work(
        &mut self,
        event_loop: &ActiveEventLoop,
    ) -> bool {
        let Some(mut deck) = self.workspace.take() else {
            return false;
        };
        let focused = self.window.as_ref().is_some_and(|w| w.has_focus())
            && !deck.chrome.as_ref().is_some_and(Chrome::editing);
        let enabled = deck
            .active
            .and_then(|id| deck.model.tab(id))
            .is_some_and(|tab| tab.spec.profile.clipboard);
        lock(&self.session.handle.state).clipboard.focus(
            focused
                && enabled
                && deck
                    .clipboard_owner
                    .is_none_or(|id| Some(id) == deck.active),
        );
        // The ordinary video tick must not poll the native pasteboard. AppKit
        // ownership is needed only for an offer, completed text or a handoff.
        if deck.clipboard_owner.is_none()
            && !lock(&self.session.handle.state).clipboard.needs_work()
        {
            self.workspace = Some(deck);
            return true;
        }
        let Ok(generation) = super::super::super::platform::clipboard_generation() else {
            self.workspace = Some(deck);
            return true;
        };
        if deck
            .pending_paste
            .as_ref()
            .is_some_and(|p| p.generation != generation)
        {
            deck.pending_paste = None;
            deck.clipboard_message("Paste canceled: clipboard ownership changed");
        }
        let mut published = false;
        if let Some(owner) = deck.clipboard_owner {
            let session = if deck.active == Some(owner) {
                Some(&mut self.session)
            } else {
                deck.inactive.get_mut(&owner)
            };
            let mut pending = false;
            if let Some(session) = session {
                pending = lock(&session.handle.state)
                    .clipboard
                    .handoff_pending(generation);
                if pending {
                    published = session.clipboard_work();
                    if !published {
                        pending = lock(&session.handle.state)
                            .clipboard
                            .handoff_pending(generation);
                    }
                }
            }
            if published || !pending {
                deck.clipboard_owner = None;
                if !published && deck.pending_paste.take().is_some() {
                    deck.clipboard_message("Paste canceled: copied text did not arrive");
                }
            }
        } else {
            self.session.clipboard_work();
            if let Ok(generation) = super::super::super::platform::clipboard_generation()
                && lock(&self.session.handle.state)
                    .clipboard
                    .handoff_pending(generation)
            {
                deck.clipboard_owner = deck.active;
            }
        }
        let paste = if published {
            deck.pending_paste.take()
        } else {
            None
        };
        let paste = paste.filter(|paste| {
            focused
                && enabled
                && deck
                    .active
                    .and_then(|id| deck.model.tab(id))
                    .is_some_and(|tab| {
                        paste.valid(
                            tab.id,
                            tab.revision,
                            lock(&self.session.handle.state).report.session_epoch,
                            Instant::now(),
                        )
                    })
        });
        if published {
            deck.clipboard_message("Copied text ready");
        }
        self.workspace = Some(deck);
        if let Some(paste) = paste {
            let chord =
                super::super::super::input::deferred_paste_chord(&self.session.keys, paste.shift);
            if self.paste_text(true, chord.len()) {
                for kind in chord {
                    self.input(event_loop, kind);
                }
            }
        }
        true
    }
    pub(in super::super) fn workspace_paste_deadline(&mut self) -> Option<Instant> {
        let deck = self.workspace.as_mut()?;
        if deck
            .pending_paste
            .as_ref()
            .is_some_and(|p| Instant::now() >= p.deadline)
        {
            deck.pending_paste = None;
            deck.clipboard_message("Paste canceled: copied text did not arrive in time");
            if let Some(window) = &self.window {
                window.request_redraw();
            }
        }
        deck.pending_paste.as_ref().map(|p| p.deadline)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn delayed_paste_is_bound_to_destination_revision_epoch_and_deadline() {
        let mut model = WorkspaceModel::default();
        let spec = TabSpec {
            device: "first".into(),
            label: "First".into(),
            display: 0,
            profile: Default::default(),
        };
        let (id, _) = model.open(spec.clone()).unwrap();
        let mut second = spec;
        second.display = 1;
        let (other, _) = model.open(second).unwrap();
        let now = Instant::now();
        let paste = PendingPaste {
            tab: id,
            revision: 1,
            epoch: 7,
            generation: 2,
            shift: true,
            deadline: now + PASTE_WAIT,
        };
        assert!(paste.valid(id, 1, 7, now));
        assert!(!paste.valid(other, 1, 7, now));
        assert!(!paste.valid(id, 2, 7, now));
        assert!(!paste.valid(id, 1, 8, now));
        assert!(!paste.valid(id, 1, 7, now + PASTE_WAIT));
    }
}
