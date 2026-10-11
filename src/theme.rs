//! Color palettes and egui style setup.

use std::sync::Arc;

use egui::{Color32, CornerRadius, FontData, FontDefinitions, FontFamily, FontId, Stroke, TextStyle, Visuals};

#[derive(Clone, Copy)]
pub struct Palette {
    pub dark: bool,
    pub app_bg: Color32,
    pub panel_bg: Color32,
    pub editor_bg: Color32,
    pub gutter_bg: Color32,
    pub border: Color32,
    pub text: Color32,
    pub text_dim: Color32,
    pub text_faint: Color32,
    pub accent: Color32,
    pub accent_soft: Color32,
    pub selection: Color32,
    pub hover: Color32,

    /// Line present on both sides but different.
    pub mod_bg: Color32,
    pub mod_inline: Color32,
    pub mod_mark: Color32,
    /// Line present on this side only.
    pub ins_bg: Color32,
    pub ins_inline: Color32,
    pub ins_mark: Color32,
    /// Filler where the other side has lines.
    pub filler_bg: Color32,
    pub filler_hatch: Color32,
    /// Difference hidden by options (e.g. blank lines).
    pub ignored_bg: Color32,

    pub left_only: Color32,
    pub right_only: Color32,
    pub different: Color32,
    /// Row backgrounds in the folder list.
    pub row_left_only_bg: Color32,
    pub row_right_only_bg: Color32,
    pub row_different_bg: Color32,
    pub error: Color32,
    pub whitespace: Color32,
}

impl Palette {
    pub fn get(dark: bool) -> Palette {
        if dark { Self::dark() } else { Self::light() }
    }

    fn dark() -> Palette {
        Palette {
            dark: true,
            app_bg: hex(0x1b1e24),
            panel_bg: hex(0x21252b),
            editor_bg: hex(0x23272e),
            gutter_bg: hex(0x1f2329),
            border: hex(0x343a44),
            text: hex(0xdfe3ea),
            text_dim: hex(0xaab2bf),
            text_faint: hex(0x828b9a),
            accent: hex(0x5aa9f7),
            accent_soft: Color32::from_rgba_unmultiplied(90, 169, 247, 40),
            selection: Color32::from_rgba_unmultiplied(90, 169, 247, 55),
            hover: Color32::from_rgba_unmultiplied(255, 255, 255, 10),

            mod_bg: hex(0x4a4426),
            mod_inline: hex(0x7a3a4c),
            mod_mark: hex(0xc9a94a),
            ins_bg: hex(0x4a4426),
            ins_inline: hex(0x7a3a4c),
            ins_mark: hex(0x4fb071),
            filler_bg: hex(0x1d2026),
            filler_hatch: hex(0x2a2f37),
            ignored_bg: hex(0x272b33),

            left_only: hex(0xc792ea),
            right_only: hex(0x56c2b8),
            different: hex(0xe5c07b),
            row_left_only_bg: hex(0x4a2c2c),
            row_right_only_bg: hex(0x2c4a2e),
            row_different_bg: hex(0x4a4426),
            error: hex(0xe06c75),
            whitespace: hex(0x4b5361),
        }
    }

    fn light() -> Palette {
        Palette {
            dark: false,
            app_bg: hex(0xeceef1),
            panel_bg: hex(0xf6f7f9),
            editor_bg: hex(0xffffff),
            gutter_bg: hex(0xf4f5f7),
            border: hex(0xc9ced6),
            text: hex(0x14171c),
            text_dim: hex(0x454c57),
            text_faint: hex(0x6a717c),
            accent: hex(0x2f7de1),
            accent_soft: Color32::from_rgba_unmultiplied(47, 125, 225, 30),
            selection: Color32::from_rgba_unmultiplied(47, 125, 225, 45),
            hover: Color32::from_rgba_unmultiplied(0, 0, 0, 10),

            mod_bg: hex(0xf0de7d),
            mod_inline: hex(0xf5a0b4),
            mod_mark: hex(0xc9a227),
            ins_bg: hex(0xf0de7d),
            ins_inline: hex(0xf5a0b4),
            ins_mark: hex(0x4caf6c),
            filler_bg: hex(0xf3f4f6),
            filler_hatch: hex(0xe3e6ea),
            ignored_bg: hex(0xf2f3f5),

            left_only: hex(0x8e44ad),
            right_only: hex(0x0a7a70),
            different: hex(0x9a5f0c),
            row_left_only_bg: hex(0xf0957d),
            row_right_only_bg: hex(0xa6e68f),
            row_different_bg: hex(0xf0de7d),
            error: hex(0xd03b47),
            whitespace: hex(0xc4c9d1),
        }
    }
}

const fn hex(v: u32) -> Color32 {
    Color32::from_rgb((v >> 16) as u8, (v >> 8) as u8, v as u8)
}

pub fn mono(size: f32) -> FontId {
    FontId::new(size, FontFamily::Monospace)
}

/// Interface font `delta` points off the user's base size.
pub fn ui_font(ui: &egui::Ui, delta: f32) -> FontId {
    FontId::proportional(ui.style().text_styles[&TextStyle::Body].size + delta)
}

/// Regular-weight system UI fonts, tried in order. egui's bundled default
/// (Ubuntu Light) is thin and looks washed out, so it is only a fallback.
#[cfg(target_os = "macos")]
const SYSTEM_FONTS: &[&str] = &["/System/Library/Fonts/SFNS.ttf", "/System/Library/Fonts/HelveticaNeue.ttc"];
#[cfg(windows)]
const SYSTEM_FONTS: &[&str] = &["C:\\Windows\\Fonts\\segoeui.ttf"];
#[cfg(not(any(target_os = "macos", windows)))]
const SYSTEM_FONTS: &[&str] = &[
    "/usr/share/fonts/truetype/noto/NotoSans-Regular.ttf",
    "/usr/share/fonts/noto/NotoSans-Regular.ttf",
    "/usr/share/fonts/google-noto/NotoSans-Regular.ttf",
    "/usr/share/fonts/truetype/ubuntu/Ubuntu-R.ttf",
    "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf",
    "/usr/share/fonts/TTF/DejaVuSans.ttf",
];

pub fn install_fonts(ctx: &egui::Context) {
    let Some(data) = SYSTEM_FONTS.iter().find_map(|p| std::fs::read(p).ok()) else { return };
    let mut fonts = FontDefinitions::default();
    fonts.font_data.insert("system-ui".into(), Arc::new(FontData::from_owned(data)));
    fonts.families.entry(FontFamily::Proportional).or_default().insert(0, "system-ui".into());
    ctx.set_fonts(fonts);
}

pub fn apply(ctx: &egui::Context, p: &Palette, ui_scale_font: f32) {
    let mut v = if p.dark { Visuals::dark() } else { Visuals::light() };
    v.panel_fill = p.panel_bg;
    v.window_fill = p.panel_bg;
    v.extreme_bg_color = p.editor_bg;
    v.faint_bg_color = p.gutter_bg;
    v.window_stroke = Stroke::new(1.0, p.border);
    v.selection.bg_fill = p.accent.gamma_multiply(if p.dark { 0.45 } else { 0.3 });
    v.selection.stroke = Stroke::new(1.0, p.accent);
    v.hyperlink_color = p.accent;
    v.window_corner_radius = CornerRadius::same(10);
    v.menu_corner_radius = CornerRadius::same(8);
    for w in [
        &mut v.widgets.noninteractive,
        &mut v.widgets.inactive,
        &mut v.widgets.hovered,
        &mut v.widgets.active,
        &mut v.widgets.open,
    ] {
        w.corner_radius = CornerRadius::same(6);
    }
    v.widgets.noninteractive.bg_stroke = Stroke::new(1.0, p.border);
    v.widgets.noninteractive.fg_stroke = Stroke::new(1.0, p.text);
    v.widgets.inactive.fg_stroke = Stroke::new(1.0, p.text);
    v.weak_text_color = Some(p.text_dim);
    v.widgets.inactive.weak_bg_fill = if p.dark { Color32::from_gray(48) } else { Color32::from_gray(228) };
    v.widgets.inactive.bg_fill = v.widgets.inactive.weak_bg_fill;
    ctx.set_theme(if p.dark { egui::Theme::Dark } else { egui::Theme::Light });
    ctx.set_visuals(v);

    ctx.all_styles_mut(|s| {
        s.text_styles.insert(TextStyle::Body, FontId::proportional(ui_scale_font));
        s.text_styles.insert(TextStyle::Button, FontId::proportional(ui_scale_font));
        s.text_styles.insert(TextStyle::Small, FontId::proportional(ui_scale_font - 1.5));
        s.text_styles.insert(TextStyle::Heading, FontId::proportional(ui_scale_font + 7.0));
        s.text_styles.insert(TextStyle::Monospace, FontId::monospace(ui_scale_font));
        s.spacing.item_spacing = egui::vec2(8.0, 6.0);
        s.spacing.button_padding = egui::vec2(8.0, 4.0);
        s.spacing.interact_size.y = 24.0;
    });
}
