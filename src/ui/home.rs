//! Start page: pick two files or folders to compare.

use std::path::PathBuf;

use egui::{Align, Button, Color32, Frame, Layout, Margin, RichText, Stroke, Ui, vec2};

use super::{AppAction, ViewCtx};

#[derive(Default)]
pub struct HomeView {
    pub left: String,
    pub right: String,
    error: Option<String>,
}

impl HomeView {
    pub fn title(&self) -> String {
        "New comparison".into()
    }

    /// Fill path fields from dropped files. Returns true when both are set.
    pub fn accept_drop(&mut self, paths: &[PathBuf]) -> bool {
        for p in paths {
            let s = p.display().to_string();
            if self.left.trim().is_empty() {
                self.left = s;
            } else if self.right.trim().is_empty() || paths.len() == 1 {
                self.right = s;
            }
        }
        !self.left.trim().is_empty() && !self.right.trim().is_empty()
    }

    pub fn ui(&mut self, ui: &mut Ui, cx: &mut ViewCtx) {
        let pal = cx.pal;
        egui::CentralPanel::no_frame().show(ui, |ui| {
            ui.painter().rect_filled(ui.max_rect(), 0.0, pal.app_bg);
            egui::ScrollArea::vertical().auto_shrink(false).show(ui, |ui| {
                let w = ui.available_width().min(760.0);
                ui.add_space((ui.available_height() * 0.08).max(24.0));
                ui.vertical_centered(|ui| {
                    ui.set_max_width(w);
                    ui.label(RichText::new("DirMerge").size(32.0).strong().color(pal.text));
                    ui.label(
                        RichText::new("Compare and merge files and folders").size(17.0).color(pal.text_dim),
                    );
                    ui.add_space(22.0);

                    Frame::new()
                        .fill(pal.panel_bg)
                        .stroke(Stroke::new(1.0, pal.border))
                        .corner_radius(12)
                        .inner_margin(Margin::same(20))
                        .show(ui, |ui| {
                            ui.set_width(w - 40.0);
                            self.path_row(ui, cx, true);
                            ui.add_space(6.0);
                            ui.horizontal(|ui| {
                                ui.add_space(ui.available_width() / 2.0 - 40.0);
                                if ui
                                    .add(Button::new(RichText::new("⬌  Swap")).frame_when_inactive(false))
                                    .on_hover_text("Swap")
                                    .clicked()
                                {
                                    std::mem::swap(&mut self.left, &mut self.right);
                                }
                            });
                            ui.add_space(6.0);
                            self.path_row(ui, cx, false);
                            ui.add_space(16.0);
                            ui.horizontal(|ui| {
                                if let Some(e) = &self.error {
                                    ui.label(RichText::new(format!("⚠ {e}")).color(pal.error));
                                } else {
                                    ui.label(
                                        RichText::new("Tip: drop two files or folders onto this window")
                                            .color(pal.text_faint),
                                    );
                                }
                                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                    let ready = !self.left.trim().is_empty() && !self.right.trim().is_empty();
                                    let btn = Button::new(RichText::new("Compare  ⏵").size(16.0).color(Color32::WHITE))
                                        .fill(if ready { pal.accent } else { pal.text_faint })
                                        .min_size(vec2(130.0, 34.0))
                                        .corner_radius(8);
                                    if ui.add_enabled(ready, btn).clicked() {
                                        self.submit(cx);
                                    }
                                });
                            });
                        });

                    if !cx.settings.recent.is_empty() {
                        ui.add_space(26.0);
                        ui.allocate_ui_with_layout(vec2(w, 20.0), Layout::left_to_right(Align::Center), |ui| {
                            ui.label(RichText::new("RECENT COMPARISONS").small().strong().color(pal.text_faint));
                        });
                        ui.add_space(4.0);
                        let mut remove = None;
                        let recent = cx.settings.recent.clone();
                        for (i, r) in recent.iter().enumerate() {
                            let resp = Frame::new()
                                .fill(pal.panel_bg)
                                .corner_radius(8)
                                .inner_margin(Margin::symmetric(12, 7))
                                .show(ui, |ui| {
                                    ui.set_width(w - 24.0);
                                    ui.horizontal(|ui| {
                                        let is_dir = std::path::Path::new(&r.left).is_dir();
                                        ui.label(RichText::new(if is_dir { "📁" } else { "📄" }).color(pal.accent));
                                        ui.vertical(|ui| {
                                            // Leave room for the remove button when the path truncates.
                                            ui.set_max_width(ui.available_width() - 32.0);
                                            ui.spacing_mut().item_spacing.y = 1.0;
                                            let (lp, rp) = (std::path::Path::new(&r.left), std::path::Path::new(&r.right));
                                            let (ln, rn) = (super::file_name(lp), super::file_name(rp));
                                            let title = if ln == rn { ln } else { format!("{ln}  ⬌  {rn}") };
                                            ui.label(RichText::new(title).strong().color(pal.text));
                                            let dirs = |p: &std::path::Path| p.parent().map(|d| d.display().to_string()).unwrap_or_default();
                                            let sub = if dirs(lp) == dirs(rp) { dirs(lp) } else { format!("{}  ·  {}", dirs(lp), dirs(rp)) };
                                            ui.add(egui::Label::new(RichText::new(sub).small().color(pal.text_dim)).truncate());
                                        });
                                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                            if ui.add(Button::new("✖").frame_when_inactive(false).small()).on_hover_text("Remove").clicked() {
                                                remove = Some(i);
                                            }
                                        });
                                    });
                                })
                                .response;
                            let resp = ui.interact(resp.rect, ui.id().with(("recent", i)), egui::Sense::click());
                            if resp.hovered() {
                                ui.painter().rect_stroke(resp.rect, 8.0, Stroke::new(1.0, pal.accent), egui::StrokeKind::Inside);
                            }
                            if resp.clicked() && remove.is_none() {
                                self.left = r.left.clone();
                                self.right = r.right.clone();
                                self.submit(cx);
                            }
                            ui.add_space(4.0);
                        }
                        if let Some(i) = remove {
                            cx.settings.recent.remove(i);
                        }
                    }
                    ui.add_space(24.0);
                    ui.label(RichText::new(SHORTCUTS).small().color(pal.text_faint));
                });
            });
        });
    }

    fn path_row(&mut self, ui: &mut Ui, cx: &ViewCtx, left: bool) {
        let pal = cx.pal;
        let (val, label, col) = if left {
            (&mut self.left, "Left", pal.left_only)
        } else {
            (&mut self.right, "Right", pal.right_only)
        };
        ui.horizontal(|ui| {
            ui.add_sized(vec2(44.0, 24.0), egui::Label::new(RichText::new(label).strong().color(col)));
            // Reserve exactly what the two buttons need so both rows line up.
            let btn_w = |text: &str| {
                let font = egui::TextStyle::Button.resolve(ui.style());
                ui.painter().layout_no_wrap(text.into(), font, Color32::PLACEHOLDER).size().x + 2.0 * ui.spacing().button_padding.x
            };
            let buttons_w = btn_w(FILE_BTN) + btn_w(FOLDER_BTN) + 2.0 * ui.spacing().item_spacing.x;
            let edit_w = ui.available_width() - buttons_w;
            let r = ui.add(
                egui::TextEdit::singleline(val)
                    .hint_text("Path to a file or folder…")
                    .desired_width(edit_w)
                    .margin(Margin::symmetric(8, 6)),
            );
            if r.changed() {
                self.error = None;
            }
            if ui.button(FILE_BTN).clicked()
                && let Some(p) = rfd::FileDialog::new().pick_file()
            {
                *val = p.display().to_string();
            }
            if ui.button(FOLDER_BTN).clicked()
                && let Some(p) = rfd::FileDialog::new().pick_folder()
            {
                *val = p.display().to_string();
            }
        });
    }

    fn submit(&mut self, cx: &mut ViewCtx) {
        let l = PathBuf::from(self.left.trim());
        let r = PathBuf::from(self.right.trim());
        for p in [&l, &r] {
            if !p.exists() {
                self.error = Some(format!("Not found: {}", p.display()));
                return;
            }
        }
        if l.is_dir() != r.is_dir() {
            self.error = Some("Both paths must be files, or both must be folders".into());
            return;
        }
        self.error = None;
        cx.actions.push(AppAction::Compare { left: l, right: r });
    }
}

const FILE_BTN: &str = "📄 File…";
const FOLDER_BTN: &str = "📁 Folder…";

const SHORTCUTS: &str = "Alt+⏶/⏷ previous/next difference  ·  Alt+⏴/⏵ copy difference  ·  Ctrl/⌘+S save  ·  Ctrl/⌘+F find  ·  Ctrl/⌘+L go to line  ·  F5 refresh";
