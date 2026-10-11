//! Colours, spacing and widget styling shared by the whole app.

use eframe::egui;
use egui::{Color32, CornerRadius, FontFamily, FontId, Stroke, TextStyle, vec2};

pub const BG: Color32 = Color32::from_rgb(18, 20, 24);
pub const SURFACE: Color32 = Color32::from_rgb(25, 28, 33);
pub const CARD: Color32 = Color32::from_rgb(33, 37, 44);
pub const CARD_HOVER: Color32 = Color32::from_rgb(40, 45, 53);
pub const BORDER: Color32 = Color32::from_rgb(46, 51, 60);
pub const TEXT: Color32 = Color32::from_rgb(230, 233, 238);
pub const MUTED: Color32 = Color32::from_rgb(138, 146, 160);
pub const ACCENT: Color32 = Color32::from_rgb(255, 122, 69);
pub const ACCENT_DIM: Color32 = Color32::from_rgb(92, 52, 36);
pub const BLUE: Color32 = Color32::from_rgb(110, 170, 255);
pub const GOOD: Color32 = Color32::from_rgb(110, 205, 140);
pub const WARN: Color32 = Color32::from_rgb(240, 190, 90);
pub const DANGER: Color32 = Color32::from_rgb(240, 100, 100);

pub const RADIUS: u8 = 8;

/// Stripe colours for the track list.
pub const TRACK_COLORS: [Color32; 6] = [
    Color32::from_rgb(255, 122, 69),
    Color32::from_rgb(110, 170, 255),
    Color32::from_rgb(110, 205, 140),
    Color32::from_rgb(240, 190, 90),
    Color32::from_rgb(190, 130, 255),
    Color32::from_rgb(90, 210, 210),
];

pub fn apply(ctx: &egui::Context) {
    ctx.set_visuals_of(egui::Theme::Dark, visuals());
    ctx.set_theme(egui::Theme::Dark);
    ctx.all_styles_mut(|s| {
        s.spacing.item_spacing = vec2(8.0, 6.0);
        s.spacing.button_padding = vec2(12.0, 6.0);
        s.spacing.interact_size.y = 22.0;
        s.spacing.slider_rail_height = 4.0;
        s.spacing.combo_width = 140.0;
        s.text_styles = [
            (TextStyle::Small, FontId::new(11.5, FontFamily::Proportional)),
            (TextStyle::Body, FontId::new(14.0, FontFamily::Proportional)),
            (TextStyle::Button, FontId::new(14.0, FontFamily::Proportional)),
            (TextStyle::Heading, FontId::new(22.0, FontFamily::Proportional)),
            (TextStyle::Monospace, FontId::new(13.5, FontFamily::Monospace)),
        ]
        .into();
    });
}

fn visuals() -> egui::Visuals {
    let mut v = egui::Visuals::dark();
    let r = CornerRadius::same(RADIUS);
    v.panel_fill = SURFACE;
    v.window_fill = SURFACE;
    v.window_stroke = Stroke::new(1.0, BORDER);
    v.window_corner_radius = CornerRadius::same(14);
    v.menu_corner_radius = r;
    v.extreme_bg_color = BG;
    v.faint_bg_color = CARD;
    v.override_text_color = None;
    v.hyperlink_color = BLUE;
    v.warn_fg_color = WARN;
    v.error_fg_color = DANGER;
    v.selection.bg_fill = Color32::from_rgb(178, 86, 50);
    v.selection.stroke = Stroke::new(1.0, ACCENT);
    v.slider_trailing_fill = true;
    v.handle_shape = egui::style::HandleShape::Circle;
    v.striped = false;

    let w = &mut v.widgets;
    w.noninteractive.bg_fill = SURFACE;
    w.noninteractive.weak_bg_fill = SURFACE;
    w.noninteractive.bg_stroke = Stroke::new(1.0, BORDER);
    w.noninteractive.fg_stroke = Stroke::new(1.0, TEXT);
    w.noninteractive.corner_radius = r;

    w.inactive.bg_fill = CARD;
    w.inactive.weak_bg_fill = CARD;
    w.inactive.bg_stroke = Stroke::NONE;
    w.inactive.fg_stroke = Stroke::new(1.0, TEXT);
    w.inactive.corner_radius = r;

    w.hovered.bg_fill = CARD_HOVER;
    w.hovered.weak_bg_fill = CARD_HOVER;
    w.hovered.bg_stroke = Stroke::new(1.0, BORDER);
    w.hovered.fg_stroke = Stroke::new(1.5, TEXT);
    w.hovered.corner_radius = r;
    w.hovered.expansion = 0.0;

    w.active.bg_fill = ACCENT_DIM;
    w.active.weak_bg_fill = ACCENT_DIM;
    w.active.bg_stroke = Stroke::new(1.0, ACCENT);
    w.active.fg_stroke = Stroke::new(1.5, TEXT);
    w.active.corner_radius = r;
    w.active.expansion = 0.0;

    w.open = w.hovered;
    v
}

/// A rounded card container.
pub fn card() -> egui::Frame {
    egui::Frame::new().fill(CARD).corner_radius(CornerRadius::same(10)).inner_margin(egui::Margin::same(14))
}

/// Small all-caps section label.
pub fn section(ui: &mut egui::Ui, text: &str) {
    ui.label(egui::RichText::new(text.to_uppercase()).small().color(MUTED).strong());
}

/// Filled call-to-action button.
pub fn primary_button(text: impl Into<String>) -> egui::Button<'static> {
    egui::Button::new(egui::RichText::new(text.into()).color(Color32::from_rgb(24, 16, 12)).strong())
        .fill(ACCENT)
        .corner_radius(CornerRadius::same(RADIUS))
}
