//! Clipboard state belongs to one focused native window. Network workers only
//! stage offers/chunks; native reads and writes use an asynchronous owner.
use crate::{
    DesktopError,
    clipboard::{Assembly, MAX_TEXT_BYTES},
};
use rds_core::{ClipboardFormat, DesktopControl};
use std::{
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

const COPY_HANDOFF: Duration = Duration::from_secs(5);

#[derive(Clone, Copy)]
pub(super) struct CopySnapshot {
    pub epoch: u64,
    pub pending: bool,
    pub published_generation: Option<i64>,
}
#[derive(Default)]
struct CopyState {
    epoch: u64,
    status: Option<CopySnapshot>,
    scope: tokio_util::sync::CancellationToken,
}
#[derive(Default)]
pub(super) struct CopyFence {
    state: Mutex<CopyState>,
}
impl CopyFence {
    fn current(&self, claim: Option<u64>) -> bool {
        let state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        claim.is_none_or(|claim| claim == state.epoch)
    }
    fn scope(&self, claim: Option<u64>) -> tokio_util::sync::CancellationToken {
        let state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if claim == Some(state.epoch) {
            return state.scope.clone();
        }
        let canceled = tokio_util::sync::CancellationToken::new();
        canceled.cancel();
        canceled
    }
    fn claim(&self) -> u64 {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.epoch = state.epoch.wrapping_add(1);
        let epoch = state.epoch;
        let previous =
            std::mem::replace(&mut state.scope, tokio_util::sync::CancellationToken::new());
        state.status = Some(CopySnapshot {
            epoch,
            pending: true,
            published_generation: None,
        });
        drop(state);
        previous.cancel();
        epoch
    }
    fn fail(&self, claim: Option<u64>) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if claim != Some(state.epoch) {
            return;
        }
        state.status = Some(CopySnapshot {
            epoch: state.epoch,
            pending: false,
            published_generation: None,
        });
        let scope = state.scope.clone();
        drop(state);
        scope.cancel();
    }
    pub(super) fn snapshot(&self) -> Option<CopySnapshot> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .status
    }
    fn set(&self, claim: Option<u64>, pending: bool, published_generation: Option<i64>) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(epoch) = claim
            && epoch == state.epoch
        {
            state.status = Some(CopySnapshot {
                epoch,
                pending,
                published_generation,
            });
        }
    }
}

pub(super) struct Publication {
    pub text: String,
    pub generation: i64,
    pub claim: u64,
    pub scope: tokio_util::sync::CancellationToken,
}

#[derive(Default)]
pub(super) struct Clipboard {
    focused: bool,
    offered: Option<(u64, u32)>,
    requested: Option<(u64, u32, Instant)>,
    assembly: Assembly,
    complete: Option<String>,
    generation: Option<i64>,
    authorized: bool,
    copy_intent: Option<(Option<i64>, Instant)>,
    arbiter: Arc<CopyFence>,
    claim: Option<u64>,
    publishing: Option<u64>,
}
impl Clipboard {
    pub(super) fn with_arbiter(arbiter: Arc<CopyFence>) -> Self {
        Self {
            arbiter,
            ..Default::default()
        }
    }
    fn current(&self) -> bool {
        self.arbiter.current(self.claim)
    }
    fn claim(&mut self) {
        self.claim = Some(self.arbiter.claim());
    }
    pub(super) fn published(&self, generation: i64) {
        self.arbiter.set(self.claim, false, Some(generation));
    }
    pub(super) fn publication_failed(&self) {
        self.arbiter.fail(self.claim);
    }
    pub(super) fn needs_work(&self) -> bool {
        self.publishing.is_some()
            || self.complete.is_some()
            || ((self.focused || self.copy_pending())
                && self.offered.is_some()
                && self
                    .requested
                    .as_ref()
                    .is_none_or(|(_, _, at)| at.elapsed() > Duration::from_secs(30)))
    }
    /// Only already authorized work or one explicit Copy can outlive selection.
    /// Native ownership changes supersede both, without reading clipboard text.
    pub(super) fn handoff_pending(&mut self, generation: i64) -> bool {
        if !self.current() {
            self.reset();
            return false;
        }
        if let Some((before, at)) = self.copy_intent {
            if before.is_none_or(|before| before == generation) && at.elapsed() < COPY_HANDOFF {
                return true;
            }
            self.reset();
            return false;
        }
        if !self.authorized {
            return false;
        }
        let pending = self.generation == Some(generation)
            && (self.publishing.is_some()
                || self.complete.is_some()
                || self
                    .requested
                    .as_ref()
                    .is_some_and(|(_, _, at)| at.elapsed() < Duration::from_secs(30)));
        if !pending {
            self.reset();
        }
        pending
    }
    pub(super) fn focus(&mut self, focused: bool) {
        self.focused = focused;
        if !focused && !self.copy_pending() {
            self.offered = None;
        }
    }
    fn copy_pending(&self) -> bool {
        self.current()
            && self
                .copy_intent
                .is_some_and(|(_, at)| at.elapsed() < COPY_HANDOFF)
    }
    #[cfg(test)]
    pub(super) fn copying(&mut self, generation: i64) {
        self.begin_copy();
        self.copy_ready(generation);
    }
    pub(super) fn begin_copy(&mut self) {
        if self.focused {
            self.claim();
            self.offered = None;
            self.copy_intent = Some((None, Instant::now()));
        }
    }
    pub(super) fn copy_ready(&mut self, generation: i64) {
        if self.current()
            && let Some((before, _)) = &mut self.copy_intent
        {
            *before = Some(generation);
            self.offered = None;
        }
    }
    pub(super) fn reset(&mut self) {
        let focused = self.focused;
        self.publication_failed();
        *self = Self::with_arbiter(self.arbiter.clone());
        self.focused = focused;
    }
    pub(super) fn offer(&mut self, id: u64, format: ClipboardFormat, bytes: u32) {
        if (self.focused || self.copy_pending())
            && format == ClipboardFormat::TextUtf8
            && bytes as usize <= MAX_TEXT_BYTES
        {
            self.offered = Some((id, bytes));
        }
    }
    pub(super) fn wants_request(&self) -> bool {
        self.current()
            && (self.focused || self.copy_pending())
            && self.offered.is_some()
            && self.requested.is_none()
            && self.complete.is_none()
            && self.publishing.is_none()
            && self
                .copy_intent
                .is_none_or(|(generation, _)| generation.is_some())
    }
    pub(super) fn request(&mut self, generation: i64) -> Option<DesktopControl> {
        if !self.current() {
            self.reset();
            return None;
        }
        if self.copy_intent.is_some_and(|(before, at)| {
            before.is_some_and(|before| before != generation) || at.elapsed() >= COPY_HANDOFF
        }) {
            self.publication_failed();
            self.copy_intent = None;
            self.offered = None;
            return None;
        }
        if self.copy_intent.is_some_and(|(before, _)| before.is_none()) {
            return None;
        }
        if self
            .requested
            .as_ref()
            .is_some_and(|(_, _, at)| at.elapsed() > Duration::from_secs(30))
        {
            self.requested = None;
            self.assembly = Assembly::default();
        }
        if (!self.focused && !self.copy_pending())
            || self.requested.is_some()
            || self.complete.is_some()
            || self.publishing.is_some()
        {
            return None;
        }
        let (id, bytes) = self.offered.take()?;
        if self.copy_intent.is_none() {
            self.claim();
        }
        self.copy_intent = None;
        self.requested = Some((id, bytes, Instant::now()));
        self.generation = Some(generation);
        self.authorized = true;
        Some(DesktopControl::ClipboardRequest {
            id,
            format: ClipboardFormat::TextUtf8,
        })
    }
    pub(super) fn chunk(
        &mut self,
        id: u64,
        offset: u32,
        total: u32,
        data: Vec<u8>,
    ) -> Result<(), DesktopError> {
        if !self.focused && !self.authorized {
            return Ok(());
        }
        if self.requested.as_ref().is_none_or(|(expected, bytes, at)| {
            *expected != id || *bytes != total || at.elapsed() > Duration::from_secs(30)
        }) {
            return Err(DesktopError::Input(
                "unsolicited or expired clipboard transfer".into(),
            ));
        }
        match self.assembly.push(id, offset, total, data) {
            Ok(Some(text)) => {
                self.complete = Some(text);
                self.requested = None;
            }
            Ok(None) => {}
            Err(error) => {
                self.reset();
                return Err(error);
            }
        }
        Ok(())
    }
    pub(super) fn failed(&mut self, id: u64) {
        if self
            .requested
            .as_ref()
            .is_some_and(|(expected, _, _)| *expected == id)
        {
            self.publication_failed();
            self.requested = None;
            self.assembly = Assembly::default();
            self.generation = None;
            self.authorized = false;
        }
    }
    pub(super) fn prepare_publication(&mut self, generation: i64) -> Option<Publication> {
        let text = self.complete.take()?;
        let Some(claim) = self.claim else {
            self.authorized = false;
            self.generation = None;
            return None;
        };
        let scope = self.arbiter.scope(Some(claim));
        // Data, identity and cancellation belong to this exact admission.
        // Reusing the model cannot attach old bytes to a later copy scope.
        let permitted = self.authorized
            && self.current()
            && !scope.is_cancelled()
            && self.generation == Some(generation)
            && self.copy_intent.is_none();
        if !permitted {
            self.authorized = false;
            self.generation = None;
            if self.copy_intent.is_none() {
                self.publication_failed();
            }
            return None;
        }
        self.publishing = Some(claim);
        Some(Publication {
            text,
            generation,
            claim,
            scope,
        })
    }
    pub(super) fn arbiter(&self) -> Arc<CopyFence> {
        self.arbiter.clone()
    }
    #[cfg(test)]
    fn publication_scope(&self) -> (u64, tokio_util::sync::CancellationToken) {
        (
            self.claim.expect("fixture has a claimed copy"),
            self.arbiter.scope(self.claim),
        )
    }
    pub(super) fn owns_publication(&self, claim: u64) -> bool {
        self.claim == Some(claim) && self.current()
    }
    pub(super) fn publication_finished(&mut self, claim: u64, generation: Option<i64>) -> bool {
        if self.publishing != Some(claim) {
            return false;
        }
        self.publishing = None;
        self.authorized = false;
        self.generation = None;
        if !self.owns_publication(claim) {
            return false;
        }
        match generation {
            Some(generation) => self.published(generation),
            None => self.publication_failed(),
        }
        generation.is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn copy_baseline_must_complete_before_request_admission() {
        let mut clipboard = Clipboard::default();
        clipboard.focus(true);
        clipboard.begin_copy();
        clipboard.offer(7, ClipboardFormat::TextUtf8, 4);
        assert!(clipboard.request(1).is_none());
        clipboard.copy_ready(1);
        // Pre-gesture offers do not belong to the queued Copy input.
        assert!(clipboard.request(1).is_none());
        clipboard.offer(8, ClipboardFormat::TextUtf8, 4);
        assert!(matches!(
            clipboard.request(1),
            Some(DesktopControl::ClipboardRequest { id: 8, .. })
        ));
    }
    #[test]
    fn asynchronous_publication_retains_the_handoff_until_its_receipt() {
        let mut clipboard = Clipboard::default();
        clipboard.focus(true);
        clipboard.copying(1);
        clipboard.offer(8, ClipboardFormat::TextUtf8, 4);
        clipboard.request(1).unwrap();
        clipboard.chunk(8, 0, 4, b"text".to_vec()).unwrap();
        assert_eq!(
            clipboard
                .prepare_publication(1)
                .map(|publication| publication.text)
                .as_deref(),
            Some("text")
        );
        let (claim, scope) = clipboard.publication_scope();
        assert!(!scope.is_cancelled());
        assert!(clipboard.handoff_pending(1));
        assert!(clipboard.publication_finished(claim, Some(2)));
        assert_eq!(
            clipboard.arbiter.snapshot().unwrap().published_generation,
            Some(2)
        );
    }
    #[test]
    fn newer_copy_cancels_publication_without_canceling_the_new_owner() {
        let arbiter = Arc::new(CopyFence::default());
        let mut old = Clipboard::with_arbiter(arbiter.clone());
        let mut next = Clipboard::with_arbiter(arbiter);
        old.focus(true);
        old.copying(1);
        let (_, scope) = old.publication_scope();
        next.focus(true);
        next.begin_copy();
        assert!(scope.is_cancelled());
        let (_, current) = next.publication_scope();
        old.reset();
        assert!(!current.is_cancelled());
    }
    #[test]
    fn prepared_bytes_keep_their_original_scope_when_the_model_is_reused() {
        let mut clipboard = Clipboard::default();
        clipboard.focus(true);
        clipboard.copying(1);
        clipboard.offer(8, ClipboardFormat::TextUtf8, 4);
        clipboard.request(1).unwrap();
        clipboard.chunk(8, 0, 4, b"text".to_vec()).unwrap();
        let prepared = clipboard.prepare_publication(1).unwrap();
        clipboard.reset();
        clipboard.copying(2);
        let (new_claim, current) = clipboard.publication_scope();
        assert_ne!(prepared.claim, new_claim);
        assert!(prepared.scope.is_cancelled());
        assert!(!current.is_cancelled());
        assert_eq!(prepared.text, "text");
        assert!(!clipboard.publication_finished(prepared.claim, Some(3)));
        assert!(!current.is_cancelled());
    }
    #[test]
    fn copy_in_another_window_supersedes_a_pending_reply_without_native_change() {
        let arbiter = Arc::new(CopyFence::default());
        let mut old = Clipboard::with_arbiter(arbiter.clone());
        let mut new = Clipboard::with_arbiter(arbiter);
        old.focus(true);
        old.copying(17);
        old.offer(1, ClipboardFormat::TextUtf8, 3);
        assert!(old.request(17).is_some());
        old.focus(false);
        new.focus(true);
        new.copying(17);
        old.chunk(1, 0, 3, b"old".to_vec()).unwrap();
        assert!(old.prepare_publication(17).is_none());
        new.offer(2, ClipboardFormat::TextUtf8, 3);
        assert!(new.request(17).is_some());
        new.chunk(2, 0, 3, b"new".to_vec()).unwrap();
        assert_eq!(
            new.prepare_publication(17)
                .map(|publication| publication.text)
                .as_deref(),
            Some("new")
        );
        new.reset();
        new.focus(true);
        new.copying(18);
        old.reset();
        old.focus(true);
        old.copying(18);
        old.focus(false);
        new.copying(18);
        assert!(!old.handoff_pending(18));
    }
    #[test]
    fn hidden_tab_handoff_finishes_without_reselecting_and_never_rearms_itself() {
        let mut old = Clipboard::default();
        old.focus(true);
        old.copying(17);
        old.focus(false);
        assert!(old.handoff_pending(17));
        old.offer(1, ClipboardFormat::TextUtf8, 3);
        assert!(old.request(17).is_some());
        assert!(old.handoff_pending(17));
        old.chunk(1, 0, 3, b"abc".to_vec()).unwrap();
        assert!(old.handoff_pending(17));
        assert_eq!(
            old.prepare_publication(17)
                .map(|publication| publication.text)
                .as_deref(),
            Some("abc")
        );
        assert!(!old.handoff_pending(18));
        old.offer(2, ClipboardFormat::TextUtf8, 3);
        assert!(!old.needs_work());
    }
    #[test]
    fn superseded_native_or_remote_copy_cannot_publish_a_hidden_reply() {
        for newer_local in [false, true] {
            let mut old = Clipboard::default();
            old.focus(true);
            old.copying(17);
            old.offer(1, ClipboardFormat::TextUtf8, 3);
            assert!(old.request(17).is_some());
            old.focus(false);
            if newer_local {
                assert!(!old.handoff_pending(18));
            } else {
                old.reset(); // New explicit Copy in another tab revokes the old lease.
            }
            old.chunk(1, 0, 3, b"old".to_vec()).unwrap();
            assert!(old.prepare_publication(18).is_none());
            assert!(!old.handoff_pending(18));
        }
    }
    #[test]
    fn handoff_expiry_releases_ownership_but_does_not_discard_a_fresh_focused_offer() {
        let mut c = Clipboard::default();
        c.focus(true);
        c.offer(1, ClipboardFormat::TextUtf8, 3);
        assert!(!c.handoff_pending(17));
        assert!(c.request(17).is_some());
        c.focus(false);
        c.requested.as_mut().unwrap().2 = Instant::now() - Duration::from_secs(30);
        assert!(!c.handoff_pending(17));
        c.chunk(1, 0, 3, b"old".to_vec()).unwrap();
        assert!(c.prepare_publication(17).is_none());
        c.focus(true);
        c.copying(17);
        c.copy_intent.as_mut().unwrap().1 = Instant::now() - COPY_HANDOFF;
        assert!(!c.handoff_pending(17));
    }
    #[test]
    fn explicit_copy_accepts_one_delayed_offer_after_switching_apps() {
        let mut c = Clipboard::default();
        c.focus(true);
        c.copying(17);
        c.focus(false);
        c.offer(1, ClipboardFormat::TextUtf8, 3);
        assert!(c.needs_work());
        assert!(c.request(17).is_some());
        c.chunk(1, 0, 3, b"abc".to_vec()).unwrap();
        assert_eq!(
            c.prepare_publication(17)
                .map(|publication| publication.text),
            Some("abc".into())
        );
        assert!(c.needs_work());
        assert!(c.handoff_pending(17));
        let claim = c.publication_scope().0;
        assert!(c.publication_finished(claim, Some(18)));
        c.offer(2, ClipboardFormat::TextUtf8, 0);
        assert!(c.request(17).is_none());
        assert!(!c.needs_work());
    }
    #[test]
    fn copy_handoff_expires_and_cannot_replace_a_new_local_copy() {
        for expired in [false, true] {
            let mut c = Clipboard::default();
            c.focus(true);
            c.copying(17);
            c.focus(false);
            c.offer(1, ClipboardFormat::TextUtf8, 0);
            if expired {
                c.copy_intent = Some((Some(17), Instant::now() - COPY_HANDOFF));
            }
            assert!(c.request(if expired { 17 } else { 18 }).is_none());
            assert!(c.prepare_publication(17).is_none());
            c.copying(18); // Background callers cannot arm another handoff.
            c.offer(2, ClipboardFormat::TextUtf8, 0);
            assert!(c.request(18).is_none());
        }
    }
    #[test]
    fn newer_copy_drains_the_old_transfer_without_publishing_it() {
        let mut c = Clipboard::default();
        c.focus(true);
        c.offer(1, ClipboardFormat::TextUtf8, 3);
        assert!(c.request(9).is_some());
        c.copying(9);
        c.focus(false);
        c.offer(2, ClipboardFormat::TextUtf8, 3);
        c.chunk(1, 0, 3, b"old".to_vec()).unwrap();
        assert!(c.prepare_publication(9).is_none());
        assert!(matches!(
            c.request(9),
            Some(DesktopControl::ClipboardRequest { id: 2, .. })
        ));
        c.chunk(2, 0, 3, b"new".to_vec()).unwrap();
        assert_eq!(
            c.prepare_publication(9).map(|publication| publication.text),
            Some("new".into())
        );
    }
    #[test]
    fn background_windows_and_unsolicited_chunks_never_publish() {
        let mut c = Clipboard::default();
        c.offer(7, ClipboardFormat::TextUtf8, 3);
        assert!(c.request(1).is_none());
        c.focus(true);
        assert!(c.chunk(7, 0, 3, b"abc".to_vec()).is_err());
        assert!(c.prepare_publication(1).is_none());
        c.offer(7, ClipboardFormat::TextUtf8, 3);
        assert!(c.request(1).is_some());
        c.focus(false);
        c.chunk(7, 0, 3, b"abc".to_vec()).unwrap();
        assert_eq!(
            c.prepare_publication(1).map(|publication| publication.text),
            Some("abc".into())
        );
    }
    #[test]
    fn complete_utf8_requires_exact_offer_and_unchanged_native_generation() {
        let mut c = Clipboard::default();
        c.focus(true);
        for (id, generation, final_generation, result) in [(1, 9, 9, true), (2, 10, 11, false)] {
            c.offer(id, ClipboardFormat::TextUtf8, 4);
            assert!(c.request(generation).is_some());
            assert!(c.chunk(id + 90, 0, 4, b"fake".to_vec()).is_err());
            assert!(c.chunk(id, 0, 9, vec![0; 9]).is_err());
            c.chunk(id, 0, 4, "🖥".as_bytes()[..2].to_vec()).unwrap();
            c.chunk(id, 2, 4, "🖥".as_bytes()[2..].to_vec()).unwrap();
            assert_eq!(c.prepare_publication(final_generation).is_some(), result);
            assert!(c.prepare_publication(final_generation).is_none());
            if result {
                let claim = c.publication_scope().0;
                assert!(c.publication_finished(claim, Some(final_generation + 1)));
            }
        }
        c.offer(3, ClipboardFormat::TextUtf8, MAX_TEXT_BYTES as u32 + 1);
        assert!(c.request(1).is_none());
    }
    #[test]
    fn newer_offer_waits_for_requested_transfer_and_reconnect_clears_both() {
        let mut c = Clipboard::default();
        c.focus(true);
        c.offer(1, ClipboardFormat::TextUtf8, 0);
        assert!(c.request(8).is_some());
        c.offer(2, ClipboardFormat::TextUtf8, 0);
        assert!(c.request(8).is_none());
        c.chunk(1, 0, 0, vec![]).unwrap();
        assert_eq!(
            c.prepare_publication(8).map(|publication| publication.text),
            Some(String::new())
        );
        assert!(
            c.request(9).is_none(),
            "publication has no native receipt yet"
        );
        let claim = c.publication_scope().0;
        assert!(c.publication_finished(claim, Some(9)));
        assert!(matches!(
            c.request(9),
            Some(DesktopControl::ClipboardRequest { id: 2, .. })
        ));
        c.reset();
        assert!(c.request(9).is_none());
        assert!(c.prepare_publication(9).is_none());
    }
}
