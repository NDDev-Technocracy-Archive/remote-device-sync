//! One explicit native clipboard gesture holds subsequent input in order until
//! its asynchronous snapshot/read is ready. Sequence numbers are assigned only
//! when input is released to the exact original session.
use super::super::native_clipboard::Pending;
use super::*;

enum Operation {
    Copy(Pending<i64>),
    Paste(Pending<Option<String>>),
}

pub(super) struct Gesture {
    operation: Operation,
    epoch: u64,
    tab: Option<crate::render::workspace::TabId>,
    revision: Option<u64>,
    queued: VecDeque<Held>,
    paste_key: PasteKey,
    started: Instant,
}

struct Held {
    kind: InputKind,
    paste: bool,
}
#[derive(Default)]
enum PasteKey {
    #[default]
    Waiting,
    Pressed,
    Complete,
}

fn queue_input(
    queue: &mut VecDeque<Held>,
    paste_key: &mut PasteKey,
    paste: bool,
    kind: InputKind,
) -> Result<(), DesktopError> {
    if matches!(&kind, InputKind::PointerMove { .. })
        && queue
            .back()
            .is_some_and(|last| matches!(&last.kind, InputKind::PointerMove { .. }))
    {
        queue.pop_back();
    }
    if queue.len() >= INPUT_QUEUE_CAPACITY {
        return Err(DesktopError::Input(
            "clipboard gesture input queue is full".into(),
        ));
    }
    let paste = if !paste {
        false
    } else {
        match (&kind, &*paste_key) {
            (InputKind::KeyDown { code: 47 }, PasteKey::Waiting | PasteKey::Pressed) => {
                *paste_key = PasteKey::Pressed;
                true
            }
            (InputKind::KeyUp { code: 47 }, PasteKey::Pressed) => {
                *paste_key = PasteKey::Complete;
                true
            }
            _ => false,
        }
    };
    queue.push_back(Held { kind, paste });
    Ok(())
}
impl Gesture {
    fn push(&mut self, kind: InputKind) -> Result<(), DesktopError> {
        let paste = self.paste();
        queue_input(&mut self.queued, &mut self.paste_key, paste, kind)
    }
    fn paste(&self) -> bool {
        matches!(&self.operation, Operation::Paste(_))
    }
}

impl App {
    fn native_gesture_revision(&self) -> Option<u64> {
        self.workspace.as_ref().and_then(|deck| {
            deck.active
                .and_then(|id| deck.model.tab(id))
                .map(|tab| tab.revision)
        })
    }
    pub(super) fn native_copy(&mut self) {
        if self.native_gesture.is_some() {
            return;
        }
        self.workspace_copying();
        let pending = self
            .session
            .native
            .as_ref()
            .ok_or(crate::clipboard::native::Error::Unavailable)
            .and_then(|native| native.refresh());
        match pending {
            Ok(pending) => {
                self.native_gesture = Some(Gesture {
                    operation: Operation::Copy(pending),
                    epoch: lock(&self.session.handle.state).report.session_epoch,
                    tab: self.session.tab,
                    revision: self.native_gesture_revision(),
                    queued: VecDeque::new(),
                    paste_key: PasteKey::Waiting,
                    started: Instant::now(),
                });
            }
            Err(error) => {
                lock(&self.session.handle.state).clipboard.reset();
                if let Some(deck) = &mut self.workspace {
                    deck.clipboard_owner = None;
                }
                self.session
                    .handle
                    .notice(format!("Clipboard unavailable: {error}"));
            }
        }
    }

    pub(super) fn native_paste(&mut self) -> bool {
        if self.native_gesture.is_some() {
            self.session
                .handle
                .notice("Clipboard is busy; try pasting again");
            return false;
        }
        let pending = self
            .session
            .native
            .as_ref()
            .ok_or(crate::clipboard::native::Error::Unavailable)
            .and_then(|native| native.read(self.session.native_purpose()));
        match pending {
            Ok(pending) => {
                self.native_gesture = Some(Gesture {
                    operation: Operation::Paste(pending),
                    epoch: lock(&self.session.handle.state).report.session_epoch,
                    tab: self.session.tab,
                    revision: self.native_gesture_revision(),
                    queued: VecDeque::new(),
                    paste_key: PasteKey::Waiting,
                    started: Instant::now(),
                });
                true
            }
            Err(error) => {
                self.session
                    .handle
                    .notice(format!("Clipboard unavailable: {error}"));
                false
            }
        }
    }

    pub(super) fn defer_native_input(
        &mut self,
        kind: InputKind,
    ) -> Result<Option<InputKind>, DesktopError> {
        match &mut self.native_gesture {
            Some(gesture) => {
                gesture.push(kind)?;
                Ok(None)
            }
            None => Ok(Some(kind)),
        }
    }

    pub(super) fn native_gesture_work(&mut self, event_loop: &ActiveEventLoop) {
        let Some(mut gesture) = self.native_gesture.take() else {
            return;
        };
        if gesture.epoch != lock(&self.session.handle.state).report.session_epoch {
            self.session
                .handle
                .notice("Clipboard gesture canceled: session changed");
            return;
        }
        if gesture.tab != self.session.tab
            || gesture.revision != self.native_gesture_revision()
            || !self
                .window
                .as_ref()
                .is_some_and(|window| window.has_focus())
            || !self.workspace_clipboard_enabled()
        {
            self.native_gesture = Some(gesture);
            self.cancel_native_gesture(event_loop);
            self.session
                .handle
                .notice("Clipboard gesture canceled: destination changed");
            return;
        }
        let result = match &mut gesture.operation {
            Operation::Copy(pending) => pending.poll().map(|result| {
                result.map(|generation| {
                    lock(&self.session.handle.state)
                        .clipboard
                        .copy_ready(generation);
                    None
                })
            }),
            Operation::Paste(pending) => pending.poll(),
        };
        let Some(result) = result else {
            self.native_gesture = Some(gesture);
            return;
        };
        let paste = gesture.paste();
        let mut delivered = !paste;
        match result {
            Ok(Some(text)) => {
                let id = rand::random();
                let bytes = text.len() as u32;
                if self
                    .session
                    .input
                    .clipboard(clipboard_text(id, &text), gesture.queued.len())
                    .is_ok()
                {
                    delivered = true;
                    lock(&self.session.handle.state).clipboard_latency.sent(
                        id,
                        bytes,
                        self.session.handle.started.elapsed().as_millis() as u64,
                    );
                } else {
                    self.session
                        .handle
                        .notice("Clipboard is busy; try pasting again");
                }
            }
            Ok(None) if paste => self
                .session
                .handle
                .notice("Clipboard has no supported text"),
            Ok(None) => {}
            Err(error) => {
                if !paste {
                    lock(&self.session.handle.state).clipboard.reset();
                }
                self.session
                    .handle
                    .notice(format!("Clipboard unavailable: {error}"));
            }
        }
        tracing::debug!(
            paste,
            wait_us = gesture.started.elapsed().as_micros(),
            queued_inputs = gesture.queued.len(),
            delivered,
            "native clipboard gesture completed"
        );
        for held in gesture.queued {
            if !delivered && held.paste {
                continue;
            }
            self.input(event_loop, held.kind);
        }
    }

    pub(super) fn cancel_native_gesture(&mut self, event_loop: &ActiveEventLoop) {
        let Some(gesture) = self.native_gesture.take() else {
            return;
        };
        let paste = gesture.paste();
        drop(gesture.operation);
        if !paste {
            lock(&self.session.handle.state).clipboard.reset();
        }
        // Restore already admitted non-paste input before the normal release
        // fence. A canceled read can never send V using the old remote board.
        for held in gesture.queued {
            if paste && held.paste {
                continue;
            }
            self.input(event_loop, held.kind);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn refused_paste_preserves_the_later_literal_v_and_modifier_release() {
        let mut held = VecDeque::new();
        let mut paste = PasteKey::Waiting;
        for kind in [
            InputKind::KeyDown { code: 47 },
            InputKind::KeyDown { code: 47 },
            InputKind::KeyUp { code: 47 },
            InputKind::KeyUp { code: 29 },
            InputKind::KeyDown { code: 47 },
            InputKind::KeyUp { code: 47 },
        ] {
            queue_input(&mut held, &mut paste, true, kind).unwrap();
        }
        let remaining: Vec<_> = held
            .into_iter()
            .filter(|input| !input.paste)
            .map(|input| input.kind)
            .collect();
        assert_eq!(remaining.len(), 3);
        assert!(matches!(remaining[0], InputKind::KeyUp { code: 29 }));
        assert!(matches!(remaining[1], InputKind::KeyDown { code: 47 }));
        assert!(matches!(remaining[2], InputKind::KeyUp { code: 47 }));
    }
    #[test]
    fn native_wait_coalesces_only_adjacent_motion_and_refuses_overflow() {
        let mut held = VecDeque::new();
        let mut paste = PasteKey::Waiting;
        for x in 0..10_000 {
            queue_input(
                &mut held,
                &mut paste,
                false,
                InputKind::PointerMove {
                    x: f64::from(x),
                    y: 0.,
                },
            )
            .unwrap();
        }
        assert_eq!(held.len(), 1);
        queue_input(
            &mut held,
            &mut paste,
            false,
            InputKind::KeyDown { code: 30 },
        )
        .unwrap();
        queue_input(
            &mut held,
            &mut paste,
            false,
            InputKind::PointerMove { x: 5., y: 2. },
        )
        .unwrap();
        assert_eq!(held.len(), 3);
        assert!(matches!(held[1].kind, InputKind::KeyDown { code: 30 }));
        while held.len() < INPUT_QUEUE_CAPACITY {
            queue_input(
                &mut held,
                &mut paste,
                false,
                InputKind::KeyDown { code: 30 },
            )
            .unwrap();
        }
        assert!(queue_input(&mut held, &mut paste, false, InputKind::KeyUp { code: 30 }).is_err());
        assert_eq!(held.len(), INPUT_QUEUE_CAPACITY);
    }
}
