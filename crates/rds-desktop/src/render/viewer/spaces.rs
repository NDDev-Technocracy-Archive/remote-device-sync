//! One authoritative workspace; one native fullscreen window per remote display.
//! No child viewer processes, Space-number APIs or duplicated remote streams.
use super::super::clipboard::CopyFence;
use super::*;
use crate::render::workspace::{DeviceView, TabId, TabSpec, WorkspaceModel, chrome::Action};
use std::collections::BTreeMap;
mod clipboard;

pub(super) struct Screens {
    model: WorkspaceModel,
    devices: Vec<DeviceView>,
    panes: BTreeMap<Option<TabId>, App>,
    updates: std::sync::mpsc::Receiver<WorkspaceUpdate>,
    actions: std::sync::mpsc::Receiver<Action>,
    action_sender: std::sync::mpsc::SyncSender<Action>,
    events: tokio::sync::mpsc::Sender<WorkspaceEvent>,
    proxy: EventLoopProxy<()>,
    clipboard: Arc<CopyFence>,
    pending_paste: Option<clipboard::PendingPaste>,
    error: Option<DesktopError>,
    activated: bool,
}

impl Viewer {
    /// Native macOS Borderless fullscreen creates a separate Space per window.
    /// Other platforms retain their window manager's fullscreen behavior.
    pub fn spaces(
        initial: Vec<TabSpec>,
        devices: Vec<DeviceView>,
    ) -> Result<
        (
            Self,
            WorkspaceHandle,
            tokio::sync::mpsc::Receiver<WorkspaceEvent>,
        ),
        DesktopError,
    > {
        if devices.len() > 32 {
            return Err(DesktopError::Input(
                "workspace device limit exceeded".into(),
            ));
        }
        for device in &devices {
            device
                .validate()
                .map_err(|e| DesktopError::Input(e.to_string()))?;
        }
        let mut trial = WorkspaceModel::default();
        for spec in &initial {
            trial
                .open(spec.clone())
                .map_err(|e| DesktopError::Input(e.to_string()))?;
        }
        let (mut viewer, _, _) = Self::new(0)?;
        let proxy = viewer.event_loop.create_proxy();
        let (updates, receive_updates) = std::sync::mpsc::sync_channel(64);
        let (action_sender, actions) = std::sync::mpsc::sync_channel(64);
        let (events, receiver) = tokio::sync::mpsc::channel(32);
        let handle = WorkspaceHandle::new(updates, proxy.clone());
        viewer.screens = Some(Screens {
            model: WorkspaceModel::default(),
            devices,
            panes: BTreeMap::new(),
            updates: receive_updates,
            actions,
            action_sender,
            events,
            proxy,
            clipboard: Arc::new(CopyFence::default()),
            pending_paste: None,
            error: None,
            activated: false,
        });
        for spec in initial {
            handle.update(WorkspaceUpdate::Open(spec))?;
        }
        Ok((viewer, handle, receiver))
    }
}

impl Screens {
    pub(super) fn result(self) -> Result<(), DesktopError> {
        self.error.map_or(Ok(()), Err)
    }
    fn project(&mut self) {
        for (id, app) in &mut self.panes {
            if let Some(deck) = &mut app.workspace {
                deck.model = self.model.clone();
                if let Some(id) = id {
                    let _ = deck.model.select(*id);
                }
                deck.devices.clone_from(&self.devices);
            }
            if let Some(window) = &app.window {
                window.request_redraw();
            }
        }
    }
    fn message(&mut self, message: &str) {
        for app in self.panes.values_mut() {
            if let Some(deck) = &mut app.workspace {
                deck.message = message.chars().take(512).collect();
            }
            if let Some(window) = &app.window {
                window.request_redraw();
            }
        }
    }
    fn pane(&mut self, id: Option<TabId>, event_loop: &ActiveEventLoop) {
        let tab = id.and_then(|id| self.model.tab(id)).cloned();
        let (mut session, view, input) = SessionView::new(
            tab.as_ref().map_or(0, |t| t.spec.display),
            self.proxy.clone(),
        );
        session.tab = id;
        lock(&view.state).clipboard =
            super::super::clipboard::Clipboard::with_arbiter(self.clipboard.clone());
        if let Some(tab) = &tab {
            view.label(format!(
                "RDS · {} · Display {}",
                tab.spec.label, tab.spec.display
            ));
        } else {
            view.status("Ready");
        }
        let mut model = self.model.clone();
        if let Some(id) = id {
            let _ = model.select(id);
        }
        let mut app = App {
            window: None,
            gpu: None,
            session,
            error: None,
            last_window_status: None,
            workspace: Some(workspace::Deck::for_space(
                model,
                id,
                self.devices.clone(),
                self.events.clone(),
                self.action_sender.clone(),
            )),
            activity: tab
                .as_ref()
                .map(|_| super::super::platform::remote_activity()),
            modifiers: Default::default(),
            scoped_window: true,
            window_closed: false,
            fullscreen: tab.is_some(),
            native_input: NativeInputPolicy::default(),
            shortcuts: None,
        };
        app.resumed(event_loop);
        if tab.is_some()
            && let (Some(deck), Some(window)) = (&mut app.workspace, &app.window)
            && let Some(chrome) = &mut deck.chrome
        {
            chrome.connected();
            chrome.toggle_panels(window);
        }
        if app.window_closed {
            if self.error.is_none() {
                self.error = app.error.take();
            }
            if let Some(id) = id {
                let _ = self.model.close(id);
            }
            return;
        }
        if let Some(tab) = tab
            && self
                .events
                .try_send(WorkspaceEvent::Start { tab, view, input })
                .is_err()
        {
            app.close_window(event_loop);
            if let Some(id) = id {
                let _ = self.model.close(id);
            }
            self.message("Connection queue is full; reopen this display");
            return;
        }
        self.panes.insert(id, app);
    }
    fn open(&mut self, spec: TabSpec, event_loop: &ActiveEventLoop) {
        match self.model.open(spec) {
            Ok((id, true)) => {
                self.pane(Some(id), event_loop);
                // Remove the initial settings window only after successful creation.
                if self.panes.contains_key(&Some(id)) {
                    self.remove(None, event_loop);
                }
                self.project();
            }
            Ok((id, false)) => self.focus(id),
            Err(error) => self.message(&error.to_string()),
        }
    }
    fn focus(&self, id: TabId) {
        if let Some(window) = self
            .panes
            .get(&Some(id))
            .and_then(|app| app.window.as_ref())
        {
            super::super::platform::activate_application();
            window.focus_window();
        }
    }
    fn remove(&mut self, id: Option<TabId>, event_loop: &ActiveEventLoop) {
        if let Some(mut app) = self.panes.remove(&id) {
            app.close_window(event_loop);
            if self.error.is_none() {
                self.error = app.error.take();
            }
        }
    }
    fn close(&mut self, id: TabId, event_loop: &ActiveEventLoop) {
        self.remove(Some(id), event_loop);
        if self.model.close(id).is_ok() {
            let _ = self.events.try_send(WorkspaceEvent::Stop(id));
        }
        self.project();
    }
    fn restart(&mut self, id: TabId, event_loop: &ActiveEventLoop) {
        let Some(tab) = self.model.tab(id).cloned() else {
            return;
        };
        let Some(app) = self.panes.get_mut(&Some(id)) else {
            return;
        };
        app.release(event_loop);
        lock(&app.session.handle.state).clipboard.reset();
        let (mut session, view, input) = SessionView::new(tab.spec.display, self.proxy.clone());
        session.tab = Some(id);
        lock(&view.state).clipboard =
            super::super::clipboard::Clipboard::with_arbiter(self.clipboard.clone());
        view.label(format!(
            "RDS · {} · Display {}",
            tab.spec.label, tab.spec.display
        ));
        app.session = session;
        app.last_window_status = None;
        if let Some(deck) = &mut app.workspace {
            deck.cancel_clipboard_tab(id);
        }
        if let Some(gpu) = &mut app.gpu {
            gpu.clear_picture();
        }
        if self
            .events
            .try_send(WorkspaceEvent::Start { tab, view, input })
            .is_err()
        {
            self.close(id, event_loop);
            self.message("Connection queue is full; reopen this display");
        }
        self.project();
    }
    fn process(&mut self, event_loop: &ActiveEventLoop) {
        for _ in 0..64 {
            let Ok(update) = self.updates.try_recv() else {
                break;
            };
            match update {
                WorkspaceUpdate::CloseWindow => {
                    event_loop.exit();
                    return;
                }
                WorkspaceUpdate::Open(spec) => self.open(spec, event_loop),
                WorkspaceUpdate::Message(message) => self.message(&message),
                WorkspaceUpdate::Device(device) => {
                    if let Err(error) = device.validate() {
                        self.message(&error.to_string());
                        continue;
                    }
                    if let Some(old) = self.devices.iter_mut().find(|old| old.key == device.key) {
                        *old = device;
                    } else if self.devices.len() < 32 {
                        self.devices.push(device);
                    } else {
                        self.message("At most 32 saved computers are supported");
                    }
                    self.project();
                }
            }
        }
        for _ in 0..64 {
            let Ok(action) = self.actions.try_recv() else {
                break;
            };
            match action {
                Action::Open(spec) => self.open(spec, event_loop),
                Action::OpenAll(specs) => {
                    let mut trial = self.model.clone();
                    match specs
                        .iter()
                        .try_for_each(|spec| trial.open(spec.clone()).map(|_| ()))
                    {
                        Ok(()) => {
                            for spec in specs {
                                self.open(spec, event_loop);
                            }
                        }
                        Err(error) => self.message(&error.to_string()),
                    }
                }
                Action::Close(id) => self.close(id, event_loop),
                Action::Select(id) => self.focus(id),
                Action::Configure(id, profile) => match self.model.configure(id, profile) {
                    Ok(_) => self.restart(id, event_loop),
                    Err(error) => self.message(&error.to_string()),
                },
                Action::Reconnect(id) => {
                    if let Some(tab) = self.model.tab(id) {
                        let profile = tab.spec.profile.clone();
                        if self.model.configure(id, profile).is_ok() {
                            self.restart(id, event_loop);
                        }
                    }
                }
                Action::Move(id, position) => {
                    let _ = self.model.move_tab(id, position);
                    self.project();
                }
                Action::Inspect { target, grant_file } => {
                    if self
                        .events
                        .try_send(WorkspaceEvent::Inspect { target, grant_file })
                        .is_err()
                    {
                        self.message("Connection queue is full; try again");
                    }
                }
                Action::Save => {
                    let tabs = self
                        .model
                        .tabs()
                        .iter()
                        .map(|tab| tab.spec.clone())
                        .collect();
                    if self.events.try_send(WorkspaceEvent::Save { tabs }).is_err() {
                        self.message("Save queue is full; try again");
                    }
                }
            }
        }
        let closed: Vec<_> = self
            .panes
            .iter()
            .filter_map(|(id, app)| app.window_closed.then_some(*id))
            .collect();
        for id in closed {
            if let Some(id) = id {
                self.close(id, event_loop);
            } else {
                self.remove(None, event_loop);
            }
        }
        if self.panes.is_empty() {
            event_loop.exit();
        }
    }
}

impl ApplicationHandler<()> for Screens {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.panes.is_empty() {
            self.pane(None, event_loop);
        }
        self.process(event_loop);
        if !self.activated && !event_loop.exiting() {
            self.activated = true;
            if let Some(id) = self.model.tabs().first().map(|tab| tab.id) {
                self.focus(id);
            }
        }
    }
    fn user_event(&mut self, event_loop: &ActiveEventLoop, _: ()) {
        self.process(event_loop);
        if event_loop.exiting() {
            return;
        }
        for app in self.panes.values_mut() {
            let work = {
                let state = lock(&app.session.handle.state);
                state.wake_pending || state.close || state.clipboard.needs_work()
            };
            if work {
                app.user_event(event_loop, ());
            }
        }
        self.process(event_loop);
        self.check_paste(event_loop);
    }
    fn window_event(&mut self, event_loop: &ActiveEventLoop, id: WindowId, event: WindowEvent) {
        if event_loop.exiting() {
            self.pending_paste = None;
            return;
        }
        if self.paste_event(event_loop, id, &event) {
            return;
        }
        if let Some(app) = self
            .panes
            .values_mut()
            .find(|app| app.window.as_ref().is_some_and(|window| window.id() == id))
        {
            app.window_event(event_loop, id, event);
        }
        self.process(event_loop);
    }
    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        if event_loop.exiting() {
            self.pending_paste = None;
            return;
        }
        self.check_paste(event_loop);
        let deadline = self
            .panes
            .values_mut()
            .filter_map(|app| {
                let paste = app.workspace_paste_deadline();
                let ui = app
                    .workspace
                    .as_mut()
                    .and_then(|deck| deck.chrome.as_mut())
                    .zip(app.window.as_ref())
                    .and_then(|(chrome, window)| chrome.repaint_deadline(window));
                paste.into_iter().chain(ui).min()
            })
            .chain(self.pending_paste.as_ref().map(|paste| paste.deadline))
            .min();
        event_loop.set_control_flow(deadline.map_or(
            winit::event_loop::ControlFlow::Wait,
            winit::event_loop::ControlFlow::WaitUntil,
        ));
    }
    fn exiting(&mut self, event_loop: &ActiveEventLoop) {
        for app in self.panes.values_mut() {
            app.close_window(event_loop);
        }
        self.panes.clear();
    }
}
