//! Colours and style.

use egui::{Color32, CornerRadius, Stroke, Visuals};
use fluxcapture_settings::Theme;

pub const ACCENT: Color32 = Color32::from_rgb(0, 168, 150);
pub const REC: Color32 = Color32::from_rgb(230, 57, 70);
pub const WARN: Color32 = Color32::from_rgb(244, 162, 97);
pub const OK: Color32 = Color32::from_rgb(76, 175, 80);
pub const MUTED: Color32 = Color32::from_rgb(140, 140, 150);

pub fn apply(ctx: &egui::Context, theme: Theme) {
    let dark = match theme {
        Theme::Dark => true,
        Theme::Light => false,
        Theme::System => ctx.system_theme().is_none_or(|t| t == egui::Theme::Dark),
    };
    let mut v = if dark { Visuals::dark() } else { Visuals::light() };
    v.selection.bg_fill = ACCENT.linear_multiply(if dark { 0.6 } else { 0.4 });
    v.selection.stroke = Stroke::new(1.0, ACCENT);
    v.hyperlink_color = ACCENT;
    v.window_corner_radius = CornerRadius::same(8);
    v.widgets.noninteractive.corner_radius = CornerRadius::same(4);
    v.widgets.inactive.corner_radius = CornerRadius::same(4);
    v.widgets.hovered.corner_radius = CornerRadius::same(4);
    v.widgets.active.corner_radius = CornerRadius::same(4);
    if dark {
        v.panel_fill = Color32::from_rgb(24, 25, 30);
        v.window_fill = Color32::from_rgb(30, 31, 37);
        v.extreme_bg_color = Color32::from_rgb(14, 15, 18);
        v.faint_bg_color = Color32::from_rgb(34, 35, 42);
    }
    ctx.set_visuals(v);
    ctx.all_styles_mut(|s| {
        s.spacing.item_spacing = egui::vec2(8.0, 6.0);
        s.spacing.button_padding = egui::vec2(10.0, 4.0);
    });
}

/// Colour for a severity.
pub fn severity(s: fluxcapture_telemetry::Severity) -> Color32 {
    match s {
        fluxcapture_telemetry::Severity::Info => ACCENT,
        fluxcapture_telemetry::Severity::Warning => WARN,
        fluxcapture_telemetry::Severity::Critical => REC,
    }
}
