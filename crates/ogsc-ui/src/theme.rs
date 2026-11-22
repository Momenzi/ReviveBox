//! Retro-modern dark visual theme, badges, and deterministic offline cover-art renderer.

use egui::{
    Align2, Color32, CornerRadius, FontId, Margin, Pos2, Rect, RichText, Stroke, StrokeKind, Ui,
    Vec2,
};
use ogsc_recipes::{HealthStatus, IssueSeverity};
use ogsc_scanner::GameStatus;

pub const BG_CANVAS: Color32 = Color32::from_rgb(15, 18, 24);
pub const BG_SURFACE: Color32 = Color32::from_rgb(23, 28, 38);
pub const BG_ELEVATED: Color32 = Color32::from_rgb(31, 38, 52);
pub const BG_CONSOLE: Color32 = Color32::from_rgb(11, 14, 19);
pub const BORDER_SUBTLE: Color32 = Color32::from_rgb(44, 54, 74);
pub const BORDER_ACTIVE: Color32 = Color32::from_rgb(245, 166, 35);
pub const ACCENT_AMBER: Color32 = Color32::from_rgb(245, 166, 35);
pub const ACCENT_AMBER_TEXT: Color32 = Color32::from_rgb(20, 16, 10);
pub const ACCENT_CYAN: Color32 = Color32::from_rgb(56, 189, 248);
pub const STATUS_PATCHED: Color32 = Color32::from_rgb(52, 211, 153);
pub const STATUS_NEEDS_FIXES: Color32 = Color32::from_rgb(251, 146, 60);
pub const STATUS_BLOCKER: Color32 = Color32::from_rgb(248, 113, 113);
pub const STATUS_VANILLA: Color32 = Color32::from_rgb(148, 163, 184);
pub const TEXT_PRIMARY: Color32 = Color32::from_rgb(230, 237, 247);
pub const TEXT_MUTED: Color32 = Color32::from_rgb(147, 161, 184);

/// Applies the dark retro-modern visual style globally to an [`egui::Context`].
pub fn apply_retro_dark_theme(ctx: &egui::Context) {
    let mut style = (*ctx.style()).clone();
    let mut visuals = egui::Visuals::dark();

    visuals.window_fill = BG_SURFACE;
    visuals.panel_fill = BG_CANVAS;
    visuals.faint_bg_color = BG_ELEVATED;
    visuals.extreme_bg_color = BG_CONSOLE;
    visuals.window_stroke = Stroke::new(1.0_f32, BORDER_SUBTLE);
    visuals.window_corner_radius = CornerRadius::same(8);
    visuals.menu_corner_radius = CornerRadius::same(6);
    visuals.selection.bg_fill = Color32::from_rgb(66, 52, 26);
    visuals.selection.stroke = Stroke::new(1.0_f32, ACCENT_AMBER);
    visuals.override_text_color = Some(TEXT_PRIMARY);

    visuals.widgets.noninteractive.bg_fill = BG_SURFACE;
    visuals.widgets.noninteractive.bg_stroke = Stroke::new(1.0_f32, BORDER_SUBTLE);
    visuals.widgets.noninteractive.corner_radius = CornerRadius::same(6);
    visuals.widgets.noninteractive.fg_stroke = Stroke::new(1.0_f32, TEXT_PRIMARY);

    visuals.widgets.inactive.bg_fill = BG_ELEVATED;
    visuals.widgets.inactive.weak_bg_fill = BG_ELEVATED;
    visuals.widgets.inactive.bg_stroke = Stroke::new(1.0_f32, BORDER_SUBTLE);
    visuals.widgets.inactive.corner_radius = CornerRadius::same(6);
    visuals.widgets.inactive.fg_stroke = Stroke::new(1.0_f32, TEXT_PRIMARY);

    visuals.widgets.hovered.bg_fill = Color32::from_rgb(41, 50, 68);
    visuals.widgets.hovered.weak_bg_fill = Color32::from_rgb(41, 50, 68);
    visuals.widgets.hovered.bg_stroke = Stroke::new(1.0_f32, ACCENT_AMBER);
    visuals.widgets.hovered.corner_radius = CornerRadius::same(6);

    visuals.widgets.active.bg_fill = Color32::from_rgb(52, 62, 82);
    visuals.widgets.active.bg_stroke = Stroke::new(1.0_f32, ACCENT_AMBER);
    visuals.widgets.active.corner_radius = CornerRadius::same(6);

    style.visuals = visuals;
    style.spacing.item_spacing = Vec2::new(8.0, 6.0);
    style.spacing.button_padding = Vec2::new(10.0, 5.0);
    ctx.set_style(style);
}

/// Maps library [`GameStatus`] to display label and badge accent color.
pub const fn status_badge_style(status: GameStatus) -> (&'static str, Color32) {
    match status {
        GameStatus::NeedsFixes => ("Needs Fixes", STATUS_NEEDS_FIXES),
        GameStatus::Patched => ("Patched", STATUS_PATCHED),
        GameStatus::DeclinedVanilla => ("Vanilla", STATUS_VANILLA),
    }
}

/// Maps recipe [`HealthStatus`] to display label and color.
pub const fn health_badge_style(health: HealthStatus) -> (&'static str, Color32) {
    match health {
        HealthStatus::BrokenOnWin11 => ("Broken on Win11", STATUS_BLOCKER),
        HealthStatus::NeedsWrapper => ("Needs Wrapper", STATUS_NEEDS_FIXES),
        HealthStatus::PartiallyPlayable => ("Partially Playable", ACCENT_AMBER),
        HealthStatus::ReadyToPlay => ("Ready to Play", STATUS_PATCHED),
    }
}

/// Maps [`IssueSeverity`] to display label and color.
pub const fn severity_badge_style(severity: IssueSeverity) -> (&'static str, Color32) {
    match severity {
        IssueSeverity::Blocker => ("BLOCKER", STATUS_BLOCKER),
        IssueSeverity::Major => ("MAJOR", STATUS_NEEDS_FIXES),
        IssueSeverity::Minor => ("MINOR", ACCENT_AMBER),
        IssueSeverity::Enhancement => ("ENHANCEMENT", ACCENT_CYAN),
    }
}

/// Renders a compact pill badge with subtle translucent fill and colored border/text.
pub fn paint_pill(ui: &mut Ui, label: &str, color: Color32) {
    let fill = Color32::from_rgba_unmultiplied(color.r(), color.g(), color.b(), 28);
    let frame = egui::Frame::new()
        .fill(fill)
        .stroke(Stroke::new(1.0_f32, color))
        .corner_radius(CornerRadius::same(12))
        .inner_margin(Margin::symmetric(8, 2));
    frame.show(ui, |ui| {
        ui.label(RichText::new(label).size(11.0).strong().color(color));
    });
}

/// Draws a primary high-contrast amber CTA button.
pub fn primary_button(ui: &mut Ui, text: &str) -> egui::Response {
    ui.add(
        egui::Button::new(RichText::new(text).strong().color(ACCENT_AMBER_TEXT))
            .fill(ACCENT_AMBER)
            .corner_radius(CornerRadius::same(6)),
    )
}

/// Renders deterministic retro box-art typography onto `rect` when cover art is unavailable or loading.
pub fn paint_fallback_cover(
    ui: &Ui,
    rect: Rect,
    game_id: &str,
    title: &str,
    release_year: Option<u16>,
    status: GameStatus,
) {
    let painter = ui.painter_at(rect);
    let (top_color, bottom_color, stripe_color) = palette_for_game(game_id);
    let (_, status_color) = status_badge_style(status);
    let scale = (rect.width() / 176.0).clamp(0.52, 1.25);
    let pad = (10.0 * scale).max(6.0);

    painter.rect_filled(rect, CornerRadius::same(6), bottom_color);

    let header_rect =
        Rect::from_min_max(rect.min, Pos2::new(rect.max.x, rect.min.y + rect.height() * 0.40));
    painter.rect_filled(header_rect, CornerRadius::same(6), top_color);

    // Subtle CRT / retro horizontal scanlines in the upper banner
    let mut y = rect.min.y + 6.0;
    let line_stroke = Stroke::new(1.0_f32, Color32::from_white_alpha(12));
    while y < header_rect.max.y {
        painter.line_segment(
            [Pos2::new(rect.min.x + 8.0, y), Pos2::new(rect.max.x - 8.0, y)],
            line_stroke,
        );
        y += 6.0;
    }

    // Top-right release year pill
    if let Some(year) = release_year {
        let yr_w = (44.0 * scale).max(30.0);
        let yr_h = (20.0 * scale).max(14.0);
        let yr_rect = Rect::from_min_size(
            Pos2::new(rect.max.x - yr_w - pad * 0.8, rect.min.y + pad * 0.8),
            Vec2::new(yr_w, yr_h),
        );
        painter.rect_filled(yr_rect, CornerRadius::same(4), Color32::from_black_alpha(140));
        painter.rect_stroke(
            yr_rect,
            CornerRadius::same(4),
            Stroke::new(1.0_f32, stripe_color),
            StrokeKind::Inside,
        );
        painter.text(
            yr_rect.center(),
            Align2::CENTER_CENTER,
            year.to_string(),
            FontId::monospace((11.0 * scale).max(8.5)),
            TEXT_PRIMARY,
        );
    }

    // Accent divider stripe
    let stripe_y = header_rect.max.y;
    painter.line_segment(
        [Pos2::new(rect.min.x, stripe_y), Pos2::new(rect.max.x, stripe_y)],
        Stroke::new(2.0_f32, stripe_color),
    );

    // Wrapped typographic title kept strictly inside the lower panel above the footer
    let footer_h = (22.0 * scale).max(16.0);
    let title_min_y = stripe_y + (8.0 * scale).max(5.0);
    let title_max_y = (rect.max.y - footer_h).max(title_min_y + 10.0);
    let galley = painter.layout(
        title.to_owned(),
        FontId::proportional((15.5 * scale).clamp(9.5, 18.0)),
        TEXT_PRIMARY,
        (rect.width() - pad * 2.0).max(24.0),
    );
    let title_clip = Rect::from_min_max(
        Pos2::new(rect.min.x + pad, title_min_y),
        Pos2::new(rect.max.x - pad, title_max_y),
    );
    painter.with_clip_rect(title_clip).galley(
        Pos2::new(rect.min.x + pad, title_min_y),
        galley,
        TEXT_PRIMARY,
    );

    // Subtle bottom archive footer
    let (footer_label, footer_font) = if rect.width() >= 135.0 {
        ("REVIVEBOX • SECOND CHANCE", (9.5 * scale).max(8.0))
    } else {
        ("REVIVEBOX", (8.5 * scale).max(7.5))
    };
    painter.text(
        Pos2::new(rect.min.x + pad, rect.max.y - (12.0 * scale).max(9.0)),
        Align2::LEFT_CENTER,
        footer_label,
        FontId::monospace(footer_font),
        Color32::from_rgba_unmultiplied(status_color.r(), status_color.g(), status_color.b(), 190),
    );
}

const fn palette_for_game(game_id: &str) -> (Color32, Color32, Color32) {
    let bytes = game_id.as_bytes();
    let mut hash: u32 = 2_166_136_261;
    let mut idx = 0;
    while idx < bytes.len() {
        hash ^= bytes[idx] as u32;
        hash = hash.wrapping_mul(16_777_619);
        idx += 1;
    }

    match hash % 5 {
        0 => (Color32::from_rgb(38, 46, 68), Color32::from_rgb(20, 24, 34), ACCENT_AMBER),
        1 => (Color32::from_rgb(28, 56, 68), Color32::from_rgb(17, 26, 33), ACCENT_CYAN),
        2 => (
            Color32::from_rgb(56, 38, 60),
            Color32::from_rgb(24, 18, 29),
            Color32::from_rgb(244, 114, 182),
        ),
        3 => (Color32::from_rgb(33, 58, 48), Color32::from_rgb(18, 27, 24), STATUS_PATCHED),
        _ => (Color32::from_rgb(62, 42, 32), Color32::from_rgb(27, 20, 18), STATUS_NEEDS_FIXES),
    }
}
