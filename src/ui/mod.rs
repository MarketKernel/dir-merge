pub mod dir_view;
pub mod file_view;
pub mod home;

use std::path::PathBuf;
use std::time::SystemTime;

use egui::{Button, Color32, Response, RichText, Ui};

use crate::settings::Settings;
use crate::theme::Palette;

pub enum AppAction {
    OpenFiles { left: Option<PathBuf>, right: Option<PathBuf> },
    OpenDirs { left: PathBuf, right: PathBuf },
    /// Open whatever the two paths are (files or folders).
    Compare { left: PathBuf, right: PathBuf },
    Toast(String),
    /// A file was written; folder views showing it should refresh that entry.
    FileSaved(PathBuf),
}

pub struct ViewCtx<'a> {
    pub settings: &'a mut Settings,
    pub pal: Palette,
    pub actions: &'a mut Vec<AppAction>,
}

/// Compact toolbar button with an icon and an optional label.
pub fn tool_button(ui: &mut Ui, icon: &str, label: &str, tip: &str, enabled: bool) -> Response {
    let text = if label.is_empty() { RichText::new(icon).size(15.0) } else { RichText::new(format!("{icon}  {label}")) };
    let r = ui.add_enabled(enabled, Button::new(text).frame_when_inactive(false).min_size(egui::vec2(28.0, 26.0)));
    if tip.is_empty() { r } else { r.on_hover_text(tip) }
}

/// A rounded "chip" toggle used for filters.
pub fn chip(ui: &mut Ui, on: &mut bool, text: &str, color: Color32, pal: &Palette) -> Response {
    let fill = if *on { color.gamma_multiply(if pal.dark { 0.25 } else { 0.18 }) } else { Color32::TRANSPARENT };
    let stroke = if *on { color.gamma_multiply(0.8) } else { pal.border };
    let txt = RichText::new(text).color(if *on { color } else { pal.text_dim });
    let r = ui.add(Button::new(txt).fill(fill).stroke(egui::Stroke::new(1.0, stroke)).corner_radius(12));
    if r.clicked() {
        *on = !*on;
    }
    r
}

pub fn fmt_size(n: u64) -> String {
    const U: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    if n < 1024 {
        return format!("{n} B");
    }
    let mut v = n as f64;
    let mut i = 0;
    while v >= 1024.0 && i < U.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    format!("{v:.1} {}", U[i])
}

pub fn fmt_time(t: Option<SystemTime>) -> String {
    match t {
        Some(t) => {
            let dt: chrono::DateTime<chrono::Local> = t.into();
            dt.format("%Y-%m-%d %H:%M").to_string()
        }
        None => String::new(),
    }
}

pub fn fmt_count(n: usize) -> String {
    let s = n.to_string();
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            out.push('\u{202F}');
        }
        out.push(c);
    }
    out
}

pub fn file_name(p: &std::path::Path) -> String {
    p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| p.display().to_string())
}
