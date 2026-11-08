//! Local workspace widgets; they never send remote input or perform network I/O.
mod pointer;

use super::{Appearance, TabId, TabProfile, TabSpec, VideoSize, WorkspaceModel};
use crate::render::design;

#[derive(Clone)]
pub struct DeviceView {
    pub key: String,
    pub label: String,
    pub displays: Vec<rds_core::DisplayInfo>,
}

impl DeviceView {
    pub fn validate(&self) -> Result<(), super::WorkspaceError> {
        TabSpec {
            device: self.key.clone(),
            label: self.label.clone(),
            display: 0,
            profile: TabProfile::default(),
        }
        .validate()?;
        if self.displays.len() > 32 {
            return Err(super::WorkspaceError::Capacity);
        }
        let mut ids = std::collections::HashSet::new();
        if self
            .displays
            .iter()
            .any(|display| !ids.insert(display.index))
        {
            return Err(super::WorkspaceError::InvalidDevice);
        }
        Ok(())
    }
}

pub(crate) enum Action {
    Select(TabId),
    Close(TabId),
    Move(TabId, usize),
    Open(TabSpec),
    OpenAll(Vec<TabSpec>),
    Inspect { target: String, grant_file: String },
    Configure(TabId, TabProfile),
    Reconnect(TabId),
    Save,
    Appearance(Appearance),
}

pub(crate) struct UiFrame {
    pub primitives: Vec<egui::ClippedPrimitive>,
    pub textures: egui::TexturesDelta,
    pub pixels_per_point: f32,
    /// Physical-pixel desktop content area, shared with pointer mapping.
    pub content: egui::Rect,
    /// Physical-pixel local overlay; it does not shrink the video viewport.
    pub restore: egui::Rect,
}

impl UiFrame {
    fn empty(pixels_per_point: f32) -> Self {
        Self {
            primitives: vec![],
            textures: Default::default(),
            pixels_per_point,
            content: egui::Rect::NOTHING,
            restore: egui::Rect::NOTHING,
        }
    }
}

impl Drop for UiFrame {
    fn drop(&mut self) {
        // The window/GPU owner is ending. Pending CPU texture commands have
        // no future painter; explicitly discard them rather than requiring a
        // final presentation from an occluded or already destroyed surface.
        self.textures.clear();
    }
}

pub(crate) struct Chrome {
    appearance: Appearance,
    context: egui::Context,
    input: egui_winit::State,
    pub frame: UiFrame,
    pub actions: Vec<Action>,
    pub message: String,
    connect_open: bool,
    target: String,
    grant_file: String,
    settings: Option<(TabId, TabProfile)>,
    about: bool,
    collapsed: bool,
    pointer: pointer::LocalPointer,
    repaint_at: Option<std::time::Instant>,
}

impl Chrome {
    pub fn toggle_panels(&mut self, window: &winit::window::Window) {
        self.collapsed = !self.collapsed;
        self.context.memory_mut(|memory| {
            if let Some(id) = memory.focused() {
                memory.surrender_focus(id);
            }
        });
        window.request_redraw();
    }

    pub fn connected(&mut self) {
        self.connect_open = false;
    }
    pub fn new(window: &winit::window::Window, appearance: Appearance) -> Self {
        let context = egui::Context::default();
        design::fonts(&context);
        design::apply(&context, appearance);
        let input = egui_winit::State::new(
            context.clone(),
            egui::ViewportId::ROOT,
            window,
            Some(window.scale_factor() as f32),
            window.theme(),
            Some(4096),
        );
        Self {
            appearance,
            context,
            input,
            frame: UiFrame::empty(window.scale_factor() as f32),
            actions: vec![],
            message: String::new(),
            connect_open: false,
            target: String::new(),
            grant_file: String::new(),
            settings: None,
            about: false,
            collapsed: false,
            pointer: Default::default(),
            repaint_at: None,
        }
    }

    pub fn appearance(&mut self, appearance: Appearance) {
        if self.appearance != appearance {
            design::apply(&self.context, appearance);
            self.appearance = appearance;
        }
    }

    pub fn event(
        &mut self,
        window: &winit::window::Window,
        event: &winit::event::WindowEvent,
    ) -> bool {
        let local_pointer = self.pointer.event(event, &self.frame);
        let keyboard = matches!(event, winit::event::WindowEvent::KeyboardInput { .. });
        let local_keyboard = self.editing();
        // egui-winit consumes Tab unconditionally and reads native Paste before
        // reporting consumption. Remote key presses must bypass those effects.
        if !local_keyboard
            && let winit::event::WindowEvent::KeyboardInput { event, .. } = event
            && event.state == winit::event::ElementState::Pressed
        {
            return false;
        }
        // Releases still clear egui's previous held-key state after a dialog
        // closes, but cannot consume the remote tab's matching key release.
        let response = self.input.on_window_event(window, event);
        // RedrawRequested already causes this paint. egui-winit marks it as
        // needing repaint; feeding that back into winit creates an idle loop.
        if response.repaint && !matches!(event, winit::event::WindowEvent::RedrawRequested) {
            window.request_redraw();
        }
        local_pointer || (response.consumed && (!keyboard || local_keyboard))
    }

    /// Consume an elapsed UI deadline once; the next paint supplies a new one.
    pub fn repaint_deadline(
        &mut self,
        window: &winit::window::Window,
    ) -> Option<std::time::Instant> {
        if self
            .repaint_at
            .is_some_and(|at| at <= std::time::Instant::now())
        {
            self.repaint_at = None;
            window.request_redraw();
        }
        self.repaint_at
    }

    pub fn editing(&self) -> bool {
        self.connect_open
            || self.settings.is_some()
            || self.about
            || self.context.egui_wants_keyboard_input()
    }

    pub fn prepare(
        &mut self,
        window: &winit::window::Window,
        model: &WorkspaceModel,
        devices: &[DeviceView],
        status: &str,
    ) {
        let context = self.context.clone();
        // Keep every pass of this frame on one layout. Apply a click only after
        // tessellation, then repaint independently of incoming remote frames.
        let collapsed = self.collapsed && !model.tabs().is_empty();
        let mut toggle = false;
        self.frame.restore = egui::Rect::NOTHING;
        let output = context.run_ui(self.input.take_egui_input(window), |root| {
            if !collapsed {
            egui::Panel::top("workspace-tabs").resizable(false).show(root, |ui| {
                ui.horizontal(|ui| {
                    if ui.add_enabled_ui(!model.tabs().is_empty(), |ui| panel_arrow(ui, false))
                        .inner.clicked() { toggle = true; }
                    ui.label(egui::RichText::new("RDS").strong().size(20.));
                    ui.separator();
                    egui::ScrollArea::horizontal().id_salt("tab-scroll").show(ui, |ui| {
                        ui.horizontal(|ui| {
                            for (index, tab) in model.tabs().iter().enumerate() {
                                let label = format!("{} · Display {}", tab.spec.label, tab.spec.display);
                                let response = ui.selectable_label(model.active() == Some(tab.id), label);
                                if response.clicked() { self.actions.push(Action::Select(tab.id)); }
                                response.context_menu(|ui| {
                                    if ui.button("Move left").clicked() {
                                        self.actions.push(Action::Move(tab.id, index.saturating_sub(1)));
                                        ui.close();
                                    }
                                    if ui.button("Move right").clicked() {
                                        self.actions.push(Action::Move(tab.id, index + 1)); ui.close();
                                    }
                                    if ui.button("Close display").clicked() {
                                        self.actions.push(Action::Close(tab.id)); ui.close();
                                    }
                                });
                                if ui.small_button("×").on_hover_text("Close this display").clicked() {
                                    self.actions.push(Action::Close(tab.id));
                                }
                            }
                        });
                    });
                    if ui.button("+ Connect").clicked() { self.connect_open = true; }
                });
                ui.horizontal(|ui| {
                    if let Some(tab) = model.active().and_then(|id| model.tab(id)) {
                        if ui.button("Settings").clicked() {
                            self.settings = Some((tab.id, tab.spec.profile.clone()));
                        }
                        if ui.button("Reconnect").clicked() { self.actions.push(Action::Reconnect(tab.id)); }
                        ui.label(format!("{} · {} FPS{}", video_label(tab.spec.profile.video_size),
                            tab.spec.profile.max_fps, if tab.spec.profile.interactive { "" } else { " · View only" }));
                    }
                    if ui.button("Save workspace").clicked() { self.actions.push(Action::Save); }
                    ui.menu_button("Appearance", |ui| {
                        for appearance in Appearance::all() {
                            let label = appearance.id().replace('-', " ");
                            if ui.selectable_label(self.appearance == appearance, label).clicked() {
                                self.actions.push(Action::Appearance(appearance));
                                ui.close();
                            }
                        }
                    });
                    if ui.button("About").clicked() { self.about = true; }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.weak("Ctrl+Tab to switch");
                    });
                });
            });
            egui::Panel::bottom("workspace-status").resizable(false).show(root, |ui| {
                ui.horizontal(|ui| {
                    ui.label(status);
                    if !self.message.is_empty() { ui.separator(); ui.label(&self.message); }
                });
            });
            } else {
                let response = restore_button(&context);
                self.frame.restore = response.rect * context.pixels_per_point();
                toggle |= response.clicked();
            }
            let content = root.available_rect_before_wrap();
            self.frame.content = egui::Rect::from_min_max(
                content.min * context.pixels_per_point(), content.max * context.pixels_per_point());
            if model.tabs().is_empty() {
                egui::CentralPanel::default().show(root, |ui| {
                    ui.vertical_centered(|ui| {
                        ui.add_space((ui.available_height() * 0.3).max(design::metrics().space_6));
                        ui.heading("Your remote workspace");
                        ui.label("Open displays from one or several computers. Each display stays connected in its own tab.");
                        ui.add_space(design::metrics().space_4);
                        if ui.button("Connect a computer").clicked() { self.connect_open = true; }
                    });
                });
            }
            if self.connect_open {
                let mut open = true;
                egui::Window::new("Connect a computer").open(&mut open)
                    .default_width(design::metrics().size_dock_max).collapsible(false).show(&context, |ui| {
                        ui.label("Device name, endpoint ID or connection ticket");
                        ui.add(egui::TextEdit::singleline(&mut self.target).char_limit(8192).desired_width(f32::INFINITY));
                        ui.collapsing("Connection authorization", |ui| {
                            ui.label("Optional grant file on this computer");
                            ui.add(egui::TextEdit::singleline(&mut self.grant_file).char_limit(4096).desired_width(f32::INFINITY));
                        });
                        if ui.add_enabled(!self.target.trim().is_empty(), egui::Button::new("Find displays")).clicked() {
                            self.actions.push(Action::Inspect { target: self.target.trim().into(), grant_file: self.grant_file.trim().into() });
                        }
                        if !self.message.is_empty() { ui.label(&self.message); }
                        ui.separator();
                        egui::ScrollArea::vertical().max_height(design::metrics().size_featured_card_min).show(ui, |ui| {
                            for device in devices {
                                ui.group(|ui| {
                                    ui.strong(&device.label);
                                    if ui.button("Load displays").clicked() {
                                        self.actions.push(Action::Inspect { target: device.key.clone(), grant_file: String::new() });
                                    }
                                    if device.displays.is_empty() { ui.weak("Inspect the computer to load its displays"); }
                                    for display in &device.displays {
                                        ui.horizontal(|ui| {
                                            ui.label(format!("Display {} · {} × {}{}", display.index, display.width, display.height,
                                                if display.primary { " · Primary" } else { "" }));
                                            if ui.button("Open").clicked() {
                                                self.actions.push(Action::Open(TabSpec { device: device.key.clone(), label: device.label.clone(),
                                                    display: display.index, profile: TabProfile::default() }));
                                            }
                                        });
                                    }
                                    if device.displays.len() > 1 && ui.button("Open all displays").clicked() {
                                        self.actions.push(Action::OpenAll(device.displays.iter().map(|display|
                                            TabSpec { device: device.key.clone(), label: device.label.clone(),
                                                display: display.index, profile: TabProfile::default() }).collect()));
                                    }
                                });
                            }
                        });
                    });
                self.connect_open = open;
            }
            if let Some((id, mut profile)) = self.settings.clone() {
                let mut open = model.tab(id).is_some();
                let mut apply = false;
                egui::Window::new("Display settings").open(&mut open)
                    .default_width(design::metrics().size_featured_card_min).collapsible(false).show(&context, |ui| {
                        if let Some(tab) = model.tab(id) {
                            ui.strong(format!("{} · Display {}", tab.spec.label, tab.spec.display));
                        }
                        ui.add_space(design::metrics().space_2);
                        egui::ComboBox::from_label("Resolution").selected_text(video_label(profile.video_size))
                            .show_ui(ui, |ui| {
                                for size in [VideoSize::Hd, VideoSize::FullHd, VideoSize::Native] {
                                    ui.selectable_value(&mut profile.video_size, size, video_label(size));
                                }
                            });
                        ui.horizontal(|ui| {
                            ui.label("Frame rate");
                            ui.add(egui::DragValue::new(&mut profile.max_fps).range(1..=240).suffix(" FPS"));
                        });
                        ui.checkbox(&mut profile.interactive, "Enable keyboard and mouse");
                        ui.checkbox(&mut profile.clipboard, "Share text clipboard when focused");
                        ui.checkbox(&mut profile.payload_receipts, "Confirm complete frame delivery");
                        ui.weak("Applying restarts this display only. Other tabs stay connected.");
                        apply = ui.button("Apply to this display").clicked();
                    });
                self.settings = if open && !apply { Some((id, profile.clone())) } else { None };
                if apply { self.actions.push(Action::Configure(id, profile)); }
            }
            if self.about {
                egui::Window::new("About NDDev Remote").open(&mut self.about).default_width(design::metrics().size_dock_max).show(&context, |ui| {
                    ui.heading("NDDev Remote");
                    ui.label("Your computers and displays in one workspace.");
                    ui.label(concat!("Version ", env!("CARGO_PKG_VERSION")));
                    ui.separator();
                    ui.label("Embedded font copyright and license notices");
                    egui::ScrollArea::vertical().max_height(design::metrics().size_featured_card_min).show(ui, |ui| {
                        for (name, text) in FONT_NOTICES {
                            ui.collapsing(*name, |ui| { ui.label(*text); });
                        }
                    });
                });
            }
        });
        self.repaint_at = output
            .viewport_output
            .get(&egui::ViewportId::ROOT)
            .and_then(|viewport| std::time::Instant::now().checked_add(viewport.repaint_delay));
        self.input
            .handle_platform_output(window, output.platform_output);
        self.frame.pixels_per_point = output.pixels_per_point;
        self.frame.textures.append(output.textures_delta);
        self.frame.primitives = context.tessellate(output.shapes, output.pixels_per_point);
        if toggle {
            self.toggle_panels(window);
        }
    }
}

fn restore_button(context: &egui::Context) -> egui::Response {
    egui::Area::new(egui::Id::new("restore-workspace-panels"))
        .order(egui::Order::Foreground)
        .anchor(egui::Align2::CENTER_TOP, egui::Vec2::ZERO)
        .movable(false)
        .default_size([
            design::metrics().size_control_md,
            design::metrics().size_control_sm,
        ])
        .fade_in(false)
        .show(context, |ui| panel_arrow(ui, true))
        .inner
}

fn panel_arrow(ui: &mut egui::Ui, restore: bool) -> egui::Response {
    let (rect, response) = ui.allocate_exact_size(
        egui::vec2(
            design::metrics().size_control_md,
            design::metrics().size_control_sm,
        ),
        egui::Sense::click(),
    );
    let label = if restore {
        "Show panels"
    } else {
        "Hide panels"
    };
    response.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, true, label));
    let visuals = ui.style().interact(&response);
    ui.painter().rect(
        rect,
        visuals.corner_radius,
        visuals.bg_fill,
        visuals.bg_stroke,
        egui::StrokeKind::Inside,
    );
    let center = rect.center();
    let direction = if restore { 1. } else { -1. };
    ui.painter().add(egui::Shape::line(
        vec![
            center
                + egui::vec2(
                    -design::metrics().mark_dot,
                    -design::metrics().mark_dot / 2. * direction,
                ),
            center + egui::vec2(0., design::metrics().mark_dot / 2. * direction),
            center
                + egui::vec2(
                    design::metrics().mark_dot,
                    -design::metrics().mark_dot / 2. * direction,
                ),
        ],
        egui::Stroke::new(design::metrics().mark_stroke, visuals.fg_stroke.color),
    ));
    response.on_hover_text(format!("{label} (Ctrl+Shift+H)"))
}

const FONT_NOTICES: &[(&str, &str)] = &[
    (
        "IBM Plex Sans and Arabic",
        include_str!(
            "../../../../../vendor/nddev-opennetwork-design-tokens/fonts/OFL-IBM-Plex.txt"
        ),
    ),
    (
        "OpenNetwork design system",
        include_str!("../../../../../vendor/nddev-opennetwork-design-tokens/NOTICE"),
    ),
    (
        "Ubuntu Light",
        include_str!("../../../assets/font-licenses/UFL.txt"),
    ),
    (
        "Noto Emoji",
        include_str!("../../../assets/font-licenses/OFL.txt"),
    ),
    (
        "Hack",
        include_str!("../../../assets/font-licenses/Hack-Regular.txt"),
    ),
    (
        "Emoji Icon",
        include_str!("../../../assets/font-licenses/emoji-icon-font-mit-license.txt"),
    ),
];

fn video_label(size: VideoSize) -> &'static str {
    match size {
        VideoSize::Hd => "HD",
        VideoSize::FullHd => "Full HD",
        VideoSize::Native => "Native resolution",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn restore_overlay_never_reserves_desktop_space_after_resize_or_dpi_change() {
        let context = egui::Context::default();
        for (width, height, scale) in [(1000., 700., 1.), (1440., 900., 2.), (640., 480., 1.5)] {
            let bounds = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(width, height));
            // An Area's first frame measures it. Subsequent passes must use the
            // same small rectangle, centered in the resized logical window.
            for _ in 0..3 {
                let mut input = egui::RawInput {
                    screen_rect: Some(bounds),
                    ..Default::default()
                };
                input
                    .viewports
                    .get_mut(&egui::ViewportId::ROOT)
                    .unwrap()
                    .native_pixels_per_point = Some(scale);
                let mut output = context.run_ui(input, |root| {
                    let response = restore_button(&context);
                    assert_eq!(root.available_rect_before_wrap(), bounds);
                    assert_eq!(
                        response.rect.size(),
                        egui::vec2(
                            design::metrics().size_control_md,
                            design::metrics().size_control_sm
                        )
                    );
                    assert!((response.rect.center().x - bounds.center().x).abs() <= 1.);
                    assert_eq!(response.rect.top(), bounds.top());
                    let content = bounds * scale;
                    let viewport = crate::render::input::Viewport::content(
                        (width * scale) as u32,
                        (height * scale) as u32,
                        width as u32,
                        height as u32,
                        Some([
                            0.,
                            0.,
                            f64::from(content.width()),
                            f64::from(content.height()),
                        ]),
                    );
                    assert_eq!(viewport.x, 0.);
                    assert_eq!(viewport.y, 0.);
                    assert_eq!(viewport.width, f64::from(content.width()));
                    assert_eq!(viewport.height, f64::from(content.height()));
                });
                output.textures_delta.clear();
            }
        }
    }

    #[test]
    fn closing_without_another_paint_discards_pending_texture_commands() {
        let mut textures = egui::TexturesDelta::default();
        textures.free(egui::TextureId::Managed(7));
        let frame = UiFrame {
            primitives: vec![],
            textures,
            pixels_per_point: 1.,
            content: egui::Rect::NOTHING,
            restore: egui::Rect::NOTHING,
        };
        // A minimized/closing window can own one final unpainted UI update.
        // Its GPU is being destroyed, so it must not require another paint.
        drop(frame);
    }
}
