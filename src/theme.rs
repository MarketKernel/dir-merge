//! Color palettes and egui style setup.

use egui::{Color32, CornerRadius, FontFamily, FontId, Stroke, TextStyle, Visuals};

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
    pub identical: Color32,
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
            text: hex(0xc8ccd4),
            text_dim: hex(0x8a93a3),
            text_faint: hex(0x5a6270),
            accent: hex(0x5aa9f7),
            accent_soft: Color32::from_rgba_unmultiplied(90, 169, 247, 40),
            selection: Color32::from_rgba_unmultiplied(90, 169, 247, 55),
            hover: Color32::from_rgba_unmultiplied(255, 255, 255, 10),

            mod_bg: hex(0x2b3546),
            mod_inline: hex(0x3f5a86),
            mod_mark: hex(0x5b8bd6),
            ins_bg: hex(0x253a2f),
            ins_inline: hex(0x2f6b45),
            ins_mark: hex(0x4fb071),
            filler_bg: hex(0x1d2026),
            filler_hatch: hex(0x2a2f37),
            ignored_bg: hex(0x272b33),

            left_only: hex(0xc792ea),
            right_only: hex(0x56c2b8),
            different: hex(0xe5c07b),
            identical: hex(0x8a93a3),
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
            border: hex(0xd5d9e0),
            text: hex(0x2b2f36),
            text_dim: hex(0x6b7280),
            text_faint: hex(0xa3a9b3),
            accent: hex(0x2f7de1),
            accent_soft: Color32::from_rgba_unmultiplied(47, 125, 225, 30),
            selection: Color32::from_rgba_unmultiplied(47, 125, 225, 45),
            hover: Color32::from_rgba_unmultiplied(0, 0, 0, 10),

            mod_bg: hex(0xe3ecfa),
            mod_inline: hex(0xb3cdf3),
            mod_mark: hex(0x6b9ee6),
            ins_bg: hex(0xdff3e4),
            ins_inline: hex(0xa6dfb4),
            ins_mark: hex(0x4caf6c),
            filler_bg: hex(0xf3f4f6),
            filler_hatch: hex(0xe3e6ea),
            ignored_bg: hex(0xf2f3f5),

            left_only: hex(0x8e44ad),
            right_only: hex(0x0f8b80),
            different: hex(0xb7791f),
            identical: hex(0x6b7280),
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
    v.widgets.inactive.weak_bg_fill = if p.dark { Color32::from_gray(48) } else { Color32::from_gray(228) };
    v.widgets.inactive.bg_fill = v.widgets.inactive.weak_bg_fill;
    ctx.set_theme(if p.dark { egui::Theme::Dark } else { egui::Theme::Light });
    ctx.set_visuals(v);

    ctx.all_styles_mut(|s| {
        s.text_styles.insert(TextStyle::Body, FontId::proportional(ui_scale_font));
        s.text_styles.insert(TextStyle::Button, FontId::proportional(ui_scale_font));
        s.text_styles.insert(TextStyle::Small, FontId::proportional(ui_scale_font - 2.0));
        s.text_styles.insert(TextStyle::Heading, FontId::proportional(ui_scale_font + 7.0));
        s.text_styles.insert(TextStyle::Monospace, FontId::monospace(ui_scale_font));
        s.spacing.item_spacing = egui::vec2(8.0, 6.0);
        s.spacing.button_padding = egui::vec2(8.0, 4.0);
        s.spacing.interact_size.y = 24.0;
    });
}
