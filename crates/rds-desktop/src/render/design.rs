//! Renderer bindings for the unchanged, released OpenNetwork token package.
use egui::{Color32, CornerRadius, FontFamily, FontId, Stroke};
use nddev_opennetwork_design_tokens::{
    Color, Colors, Density, Direction, Metrics, Motion, Platform, Script, TextStyle, TypeRole,
    durations, font_files, shadows,
};

use super::workspace::Appearance;

pub(crate) fn metrics() -> &'static Metrics {
    Metrics::resolve(Platform::Desktop, Density::Comfortable)
}
fn color(value: Color) -> Color32 {
    Color32::from_rgba_unmultiplied(value.r, value.g, value.b, value.a)
}
fn widget(fill: Color, border: Color, text: Color, focus: bool) -> egui::style::WidgetVisuals {
    let m = metrics();
    egui::style::WidgetVisuals {
        bg_fill: color(fill),
        weak_bg_fill: color(fill),
        bg_stroke: Stroke::new(
            if focus {
                m.focus_width
            } else {
                m.border_width_thin
            },
            color(border),
        ),
        corner_radius: CornerRadius::same(m.radius_sm as u8),
        fg_stroke: Stroke::new(m.mark_stroke, color(text)),
        expansion: m.space_0,
    }
}
pub(crate) fn fonts(context: &egui::Context) {
    let mut fonts = egui::FontDefinitions::default();
    let mut names = Vec::new();
    for face in font_files::ALL.iter().filter(|face| face.weight == 400) {
        fonts.font_data.insert(
            face.file.into(),
            std::sync::Arc::new(egui::FontData::from_static(face.data)),
        );
        names.push(face.file.into());
    }
    // Keep the renderer's existing emoji/unknown-glyph and monospace fallback.
    // Theme changes do not reload faces or invalidate the font atlas.
    let proportional = fonts.families.entry(FontFamily::Proportional).or_default();
    names.append(proportional);
    *proportional = names;
    context.set_fonts(fonts);
}
pub(crate) fn apply(context: &egui::Context, appearance: Appearance) {
    let theme = appearance.tokens();
    let c = Colors::resolve(theme, Direction::On);
    let m = metrics();
    let mut visuals = if theme.is_dark() {
        egui::Visuals::dark()
    } else {
        egui::Visuals::light()
    };
    visuals.override_text_color = None;
    visuals.weak_text_color = Some(color(c.text_secondary));
    visuals.widgets.noninteractive = widget(c.surface_base, c.border_subtle, c.text_primary, false);
    visuals.widgets.inactive = widget(c.surface_raised, c.border_control, c.text_primary, false);
    visuals.widgets.hovered = widget(c.surface_hover, c.border_strong, c.text_primary, false);
    visuals.widgets.active = widget(c.surface_pressed, c.focus_ring, c.text_primary, true);
    visuals.widgets.open = widget(c.surface_selected, c.focus_ring, c.text_primary, true);
    visuals.selection.bg_fill = color(c.surface_selected);
    visuals.selection.stroke = Stroke::new(m.border_width_thin, color(c.text_primary));
    visuals.hyperlink_color = color(c.link_default);
    visuals.faint_bg_color = color(c.surface_sunken);
    visuals.extreme_bg_color = color(c.surface_sunken);
    visuals.text_edit_bg_color = Some(color(c.surface_raised));
    visuals.code_bg_color = color(c.surface_sunken);
    visuals.warn_fg_color = color(c.status_warning_text);
    visuals.error_fg_color = color(c.status_danger_text);
    visuals.window_fill = color(c.surface_overlay);
    visuals.panel_fill = color(c.surface_base);
    visuals.window_stroke = Stroke::new(m.border_width_thin, color(c.border_subtle));
    visuals.window_corner_radius = CornerRadius::same(m.radius_lg as u8);
    visuals.menu_corner_radius = CornerRadius::same(m.radius_md as u8);
    let shadow = shadows::elevation_2(theme)[0];
    visuals.window_shadow = egui::epaint::Shadow {
        offset: [shadow.x as i8, shadow.y as i8],
        blur: shadow.blur as u8,
        spread: shadow.spread as u8,
        color: color(shadow.color),
    };
    visuals.popup_shadow = visuals.window_shadow;
    visuals.resize_corner_size = m.size_icon;
    visuals.text_cursor.stroke = Stroke::new(m.border_width_medium, color(c.text_primary));
    visuals.ime_composition.active_underline_stroke =
        Stroke::new(m.border_width_medium, color(c.accent_text));
    visuals.ime_composition.inactive_underline_stroke =
        Stroke::new(m.border_width_thin, color(c.text_secondary));
    let mut style = egui::Style {
        visuals,
        ..Default::default()
    };
    style.animation_time = durations::duration_ui_fast(Motion::Standard).as_secs_f32();
    style.spacing.item_spacing = egui::vec2(m.space_3, m.space_2);
    style.spacing.button_padding = egui::vec2(m.space_3, m.space_2);
    style.spacing.window_margin = egui::Margin::same(m.space_4 as i8);
    style.spacing.menu_margin = egui::Margin::same(m.space_2 as i8);
    style.spacing.interact_size = egui::vec2(m.size_control_md, m.size_control_md);
    style.spacing.icon_width = m.size_icon;
    style.spacing.icon_width_inner = m.space_3;
    style.spacing.icon_spacing = m.space_2;
    style.spacing.indent = m.space_4;
    style.spacing.slider_width = m.size_direction_card_min;
    style.spacing.slider_rail_height = m.border_width_medium;
    style.spacing.combo_width = m.size_direction_card_min;
    style.spacing.text_edit_width = m.size_featured_card_min;
    style.spacing.tooltip_width = m.size_featured_card_min;
    style.spacing.menu_width = m.size_featured_card_min;
    style.spacing.menu_spacing = m.space_2;
    style.spacing.combo_height = m.size_direction_card_min;
    style.spacing.default_area_size =
        egui::vec2(m.size_featured_card_min, m.size_featured_card_min);
    style.spacing.scroll.bar_width = m.space_2;
    style.spacing.scroll.handle_min_length = m.size_control_sm;
    style.spacing.scroll.bar_inner_margin = m.space_1;
    style.spacing.scroll.bar_outer_margin = m.space_0;
    for (target, role) in [
        (egui::TextStyle::Body, TypeRole::Body),
        (egui::TextStyle::Button, TypeRole::Body),
        (egui::TextStyle::Small, TypeRole::Caption),
        (egui::TextStyle::Heading, TypeRole::H4),
        (egui::TextStyle::Monospace, TypeRole::Code),
    ] {
        let text = TextStyle::resolve(role, Platform::Desktop, Script::Latin);
        let family = if role == TypeRole::Code {
            FontFamily::Monospace
        } else {
            FontFamily::Proportional
        };
        style
            .text_styles
            .insert(target, FontId::new(text.size, family));
    }
    let native_theme = if theme.is_dark() {
        egui::Theme::Dark
    } else {
        egui::Theme::Light
    };
    context.set_style_of(native_theme, style);
    context.set_theme(native_theme);
}
