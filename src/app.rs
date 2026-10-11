//! Top-level application: tabs, theme, settings and about windows, global shortcuts.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use egui::{Align, Align2, Color32, Frame, Key, KeyboardShortcut, Layout, Margin, Modifiers, RichText, Sense, Stroke, Ui, vec2};

use crate::highlight;
use crate::settings::{Settings, ThemePref};
use crate::theme::{self, Palette};
use crate::ui::dir_view::DirView;
use crate::ui::file_view::FileView;
use crate::ui::home::HomeView;
use crate::ui::{AppAction, ViewCtx};
use crate::diff::WhitespaceMode;
use crate::dircmp::CompareMode;

enum Tab {
    Home(HomeView),
    Dir(Box<DirView>),
    File(Box<FileView>),
}

impl Tab {
    fn title(&self) -> String {
        match self {
            Tab::Home(h) => h.title(),
            Tab::Dir(d) => d.title(),
            Tab::File(f) => f.title(),
        }
    }

    fn tooltip(&self) -> String {
        match self {
            Tab::Home(_) => String::new(),
            Tab::Dir(d) => d.tooltip(),
            Tab::File(f) => f.tooltip(),
        }
    }

    fn dirty(&self) -> bool {
        matches!(self, Tab::File(f) if f.is_dirty())
    }
}

#[derive(Clone, Copy)]
enum CloseReq {
    Tab(usize),
    App,
}

pub struct DirMergeApp {
    settings: Settings,
    tabs: Vec<Tab>,
    active: usize,
    applied: Option<(bool, u32)>,
    toast: Option<(String, Instant)>,
    close_req: Option<CloseReq>,
    allow_close: bool,
    show_settings: bool,
    show_about: bool,
    pending: Vec<AppAction>,
    #[cfg(feature = "autotest")]
    driver: Option<crate::autotest::Driver>,
}

impl DirMergeApp {
    pub fn new(cc: &eframe::CreationContext<'_>, args: Vec<PathBuf>) -> Self {
        highlight::warm_up();
        theme::install_fonts(&cc.egui_ctx);
        let mut settings: Settings = cc.storage.and_then(|s| eframe::get_value(s, Settings::KEY)).unwrap_or_default();
        settings.migrate();
        let mut app = DirMergeApp {
            settings,
            tabs: vec![Tab::Home(HomeView::default())],
            active: 0,
            applied: None,
            toast: None,
            close_req: None,
            allow_close: false,
            show_settings: false,
            show_about: false,
            pending: Vec::new(),
            #[cfg(feature = "autotest")]
            driver: crate::autotest::Driver::from_env(),
        };
        match args.as_slice() {
            [l, r, ..] => app.pending.push(AppAction::Compare { left: l.clone(), right: r.clone() }),
            [one] => {
                if let Tab::Home(h) = &mut app.tabs[0] {
                    h.left = one.display().to_string();
                }
            }
            [] => {}
        }
        app
    }

    fn is_dark(&self, ctx: &egui::Context) -> bool {
        match self.settings.theme {
            ThemePref::Dark => true,
            ThemePref::Light => false,
            ThemePref::System => ctx.system_theme().map(|t| t == egui::Theme::Dark).unwrap_or(true),
        }
    }

    fn handle(&mut self, action: AppAction, ctx: &egui::Context) {
        match action {
            AppAction::Compare { left, right } => {
                self.settings.add_recent(&left.display().to_string(), &right.display().to_string());
                if left.is_dir() && right.is_dir() {
                    self.handle(AppAction::OpenDirs { left, right }, ctx);
                } else {
                    self.handle(AppAction::OpenFiles { left: Some(left), right: Some(right) }, ctx);
                }
            }
            AppAction::OpenFiles { left, right } => {
                // Re-focus an existing tab with the same pair.
                if let Some(i) = self.tabs.iter().position(|t| matches!(t, Tab::File(f) if f.paths == [left.clone(), right.clone()])) {
                    self.active = i;
                    return;
                }
                let v = FileView::new(left, right, ctx, &self.settings.diff);
                self.open_tab(Tab::File(Box::new(v)));
            }
            AppAction::OpenDirs { left, right } => {
                let pal = Palette::get(self.is_dark(ctx));
                let mut sink = Vec::new();
                let cx = ViewCtx { settings: &mut self.settings, pal, actions: &mut sink };
                let v = DirView::new(left, right, ctx, &cx);
                self.open_tab(Tab::Dir(Box::new(v)));
            }
            AppAction::Toast(m) => self.toast = Some((m, Instant::now())),
            AppAction::FileSaved(path) => {
                let mode = self.settings.dir.mode;
                for t in &mut self.tabs {
                    if let Tab::Dir(d) = t {
                        d.refresh_path(&path, mode);
                    }
                }
            }
        }
    }

    /// Replace the active start page, or add a new tab.
    fn open_tab(&mut self, tab: Tab) {
        if let Some(Tab::Home(_)) = self.tabs.get(self.active) {
            self.tabs[self.active] = tab;
        } else {
            self.tabs.push(tab);
            self.active = self.tabs.len() - 1;
        }
    }

    fn close_tab(&mut self, i: usize, force: bool) {
        if i >= self.tabs.len() {
            return;
        }
        if !force && self.tabs[i].dirty() {
            self.close_req = Some(CloseReq::Tab(i));
            return;
        }
        self.tabs.remove(i);
        if self.tabs.is_empty() {
            self.tabs.push(Tab::Home(HomeView::default()));
        }
        if self.active >= i && self.active > 0 {
            self.active -= 1;
        }
        self.active = self.active.min(self.tabs.len() - 1);
    }

    fn tab_bar(&mut self, ui: &mut Ui, pal: &Palette, ctx: &egui::Context) {
        let mut close = None;
        let mut new_tab = false;
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 2.0;
            for i in 0..self.tabs.len() {
                let title = self.tabs[i].title();
                let active = i == self.active;
                let font = theme::ui_font(ui, 0.0);
                let galley = ui.painter().layout_no_wrap(title.clone(), font.clone(), pal.text);
                let w = (galley.size().x + 44.0).clamp(90.0, 280.0);
                let (rect, resp) = ui.allocate_exact_size(vec2(w, 32.0), Sense::click());
                let hovered = resp.hovered();
                let p = ui.painter();
                if active {
                    p.rect_filled(rect.shrink2(vec2(0.0, 2.0)).translate(vec2(0.0, 2.0)), egui::CornerRadius { nw: 8, ne: 8, sw: 0, se: 0 }, pal.editor_bg);
                    p.line_segment(
                        [rect.left_bottom() + vec2(8.0, -1.0), rect.right_bottom() + vec2(-8.0, -1.0)],
                        Stroke::new(2.0, pal.accent),
                    );
                } else if hovered {
                    p.rect_filled(rect.shrink(3.0), 6.0, pal.hover);
                }
                let tc = if active { pal.text } else { pal.text_dim };
                let text_rect = egui::Rect::from_min_max(rect.min + vec2(12.0, 0.0), rect.max - vec2(26.0, 0.0));
                p.with_clip_rect(text_rect).text(
                    egui::pos2(text_rect.left(), rect.center().y + 1.0),
                    Align2::LEFT_CENTER,
                    &title,
                    font,
                    tc,
                );
                // Close button.
                let cr = egui::Rect::from_center_size(egui::pos2(rect.right() - 14.0, rect.center().y + 1.0), vec2(18.0, 18.0));
                let cresp = ui.interact(cr, ui.id().with(("tab-close", i)), Sense::click());
                if active || hovered || cresp.hovered() {
                    if cresp.hovered() {
                        ui.painter().rect_filled(cr, 4.0, pal.hover);
                    }
                    ui.painter().text(cr.center(), Align2::CENTER_CENTER, "✖", theme::ui_font(ui, -3.5), pal.text_dim);
                }
                if cresp.clicked() || resp.middle_clicked() {
                    close = Some(i);
                } else if resp.clicked() {
                    self.active = i;
                }
                let tip = self.tabs[i].tooltip();
                if !tip.is_empty() {
                    resp.on_hover_text(tip);
                }
            }
            if ui.add(egui::Button::new(RichText::new("+").size(18.0)).frame_when_inactive(false)).on_hover_text("New comparison (Ctrl/⌘+T)").clicked() {
                new_tab = true;
            }
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                ui.add_space(6.0);
                if ui.add(egui::Button::new(RichText::new("ℹ").size(15.0)).frame_when_inactive(false)).on_hover_text("About DirMerge").clicked() {
                    self.show_about = !self.show_about;
                }
                if ui.add(egui::Button::new(RichText::new("⚙").size(15.0)).frame_when_inactive(false)).on_hover_text("Settings").clicked() {
                    self.show_settings = !self.show_settings;
                }
                let dark = pal.dark;
                let icon = if dark { "☀" } else { "🌙" };
                if ui.add(egui::Button::new(RichText::new(icon).size(15.0)).frame_when_inactive(false)).on_hover_text("Toggle light/dark").clicked() {
                    self.settings.theme = if dark { ThemePref::Light } else { ThemePref::Dark };
                }
            });
        });
        if new_tab {
            self.tabs.push(Tab::Home(HomeView::default()));
            self.active = self.tabs.len() - 1;
        }
        if let Some(i) = close {
            self.close_tab(i, false);
        }
        let _ = ctx;
    }

    fn settings_window(&mut self, ctx: &egui::Context) {
        let mut open = self.show_settings;
        egui::Window::new("Settings")
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .default_width(380.0)
            .anchor(Align2::CENTER_CENTER, vec2(0.0, 0.0))
            .show(ctx, |ui| {
                let s = &mut self.settings;
                egui::Grid::new("settings-grid").num_columns(2).spacing(vec2(16.0, 10.0)).show(ui, |ui| {
                    ui.label("Theme");
                    ui.horizontal(|ui| {
                        ui.selectable_value(&mut s.theme, ThemePref::System, "System");
                        ui.selectable_value(&mut s.theme, ThemePref::Dark, "Dark");
                        ui.selectable_value(&mut s.theme, ThemePref::Light, "Light");
                    });
                    ui.end_row();
                    ui.label("Interface font size");
                    ui.add(egui::Slider::new(&mut s.ui_font_size, 11.0..=20.0).step_by(0.5));
                    ui.end_row();
                    ui.label("Code font size");
                    ui.add(egui::Slider::new(&mut s.code_font_size, 9.0..=24.0).step_by(0.5));
                    ui.end_row();
                    ui.label("Tab width");
                    ui.add(egui::Slider::new(&mut s.tab_width, 1..=12));
                    ui.end_row();
                    ui.label("Syntax highlighting");
                    ui.checkbox(&mut s.syntax_highlighting, "");
                    ui.end_row();
                    ui.label("Dark color scheme");
                    theme_combo(ui, "dark-theme", &mut s.dark_syntax_theme);
                    ui.end_row();
                    ui.label("Light color scheme");
                    theme_combo(ui, "light-theme", &mut s.light_syntax_theme);
                    ui.end_row();
                    ui.label("Show whitespace");
                    ui.checkbox(&mut s.show_whitespace, "");
                    ui.end_row();
                    ui.label("Whitespace");
                    egui::ComboBox::from_id_salt("ws")
                        .selected_text(match s.diff.whitespace {
                            WhitespaceMode::Compare => "Compare",
                            WhitespaceMode::IgnoreChanges => "Ignore changes",
                            WhitespaceMode::IgnoreAll => "Ignore all",
                        })
                        .show_ui(ui, |ui| {
                            ui.selectable_value(&mut s.diff.whitespace, WhitespaceMode::Compare, "Compare");
                            ui.selectable_value(&mut s.diff.whitespace, WhitespaceMode::IgnoreChanges, "Ignore changes");
                            ui.selectable_value(&mut s.diff.whitespace, WhitespaceMode::IgnoreAll, "Ignore all");
                        });
                    ui.end_row();
                    ui.label("Ignore case");
                    ui.checkbox(&mut s.diff.ignore_case, "");
                    ui.end_row();
                    ui.label("Ignore blank lines");
                    ui.checkbox(&mut s.diff.ignore_blank_lines, "");
                    ui.end_row();
                    ui.label("Folder compare");
                    egui::ComboBox::from_id_salt("mode").selected_text(s.dir.mode.label()).show_ui(ui, |ui| {
                        for m in CompareMode::ALL {
                            ui.selectable_value(&mut s.dir.mode, m, m.label());
                        }
                    });
                    ui.end_row();
                    ui.label("Exclude");
                    ui.add(egui::TextEdit::singleline(&mut s.dir.exclude).desired_width(300.0));
                    ui.end_row();
                });
                ui.add_space(8.0);
                if ui.button("Reset to defaults").clicked() {
                    let recent = std::mem::take(&mut self.settings.recent);
                    self.settings = Settings { recent, ..Settings::default() };
                }
            });
        self.show_settings = open;
    }

    fn about_window(&mut self, ctx: &egui::Context, pal: &Palette) {
        let mut open = self.show_about;
        egui::Window::new("About")
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .anchor(Align2::CENTER_CENTER, vec2(0.0, 0.0))
            .show(ctx, |ui| {
                ui.set_width(320.0);
                ui.vertical_centered(|ui| {
                    ui.add_space(4.0);
                    ui.label(RichText::new("DirMerge").size(26.0).strong().color(pal.text));
                    ui.label(RichText::new(format!("Version {}", env!("CARGO_PKG_VERSION"))).color(pal.text_dim));
                    ui.add_space(8.0);
                    ui.label(RichText::new(env!("CARGO_PKG_DESCRIPTION")).color(pal.text));
                    ui.add_space(8.0);
                    ui.hyperlink(env!("CARGO_PKG_REPOSITORY"));
                    ui.add_space(4.0);
                });
            });
        if ctx.input(|i| i.key_pressed(Key::Escape)) {
            open = false;
        }
        self.show_about = open;
    }

    fn close_dialog(&mut self, ctx: &egui::Context, pal: &Palette) {
        let Some(req) = self.close_req else { return };
        let mut choice = None;
        egui::Modal::new(egui::Id::new("close-confirm")).show(ctx, |ui| {
            ui.set_width(420.0);
            ui.heading("Save changes?");
            ui.add_space(4.0);
            let what = match req {
                CloseReq::Tab(i) => format!("“{}” has unsaved changes.", self.tabs.get(i).map(|t| t.title()).unwrap_or_default()),
                CloseReq::App => "Some comparisons have unsaved changes.".into(),
            };
            ui.label(RichText::new(what).color(pal.text_dim));
            ui.add_space(12.0);
            ui.horizontal(|ui| {
                if ui.add(egui::Button::new(RichText::new("Save").color(Color32::WHITE)).fill(pal.accent)).clicked() {
                    choice = Some(0);
                }
                if ui.button("Don't save").clicked() {
                    choice = Some(1);
                }
                if ui.button("Cancel").clicked() || ui.input(|i| i.key_pressed(Key::Escape)) {
                    choice = Some(2);
                }
            });
        });
        let Some(c) = choice else { return };
        self.close_req = None;
        if c == 2 {
            return;
        }
        let targets: Vec<usize> = match req {
            CloseReq::Tab(i) => vec![i],
            CloseReq::App => (0..self.tabs.len()).collect(),
        };
        if c == 0 {
            let mut sink = Vec::new();
            for &i in &targets {
                if let Some(Tab::File(f)) = self.tabs.get_mut(i) {
                    let mut cx = ViewCtx { settings: &mut self.settings, pal: *pal, actions: &mut sink };
                    f.save_all(&mut cx);
                }
            }
            for a in sink {
                self.handle(a, ctx);
            }
            if targets.iter().any(|&i| self.tabs.get(i).is_some_and(|t| t.dirty())) {
                return; // A save failed or was cancelled.
            }
        }
        match req {
            CloseReq::Tab(i) => self.close_tab(i, true),
            CloseReq::App => {
                self.allow_close = true;
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            }
        }
    }
}

fn theme_combo(ui: &mut Ui, id: &str, value: &mut String) {
    egui::ComboBox::from_id_salt(id).selected_text(value.clone()).height(360.0).show_ui(ui, |ui| {
        for name in highlight::theme_names() {
            ui.selectable_value(value, name.to_string(), name);
        }
    });
}

impl eframe::App for DirMergeApp {
    fn ui(&mut self, ui: &mut Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        let dark = self.is_dark(&ctx);
        let pal = Palette::get(dark);
        let key = (dark, (self.settings.ui_font_size * 10.0) as u32);
        if self.applied != Some(key) {
            theme::apply(&ctx, &pal, self.settings.ui_font_size);
            self.applied = Some(key);
        }

        for a in std::mem::take(&mut self.pending) {
            self.handle(a, &ctx);
        }

        // Drag & drop.
        let dropped: Vec<PathBuf> = ctx.input(|i| i.raw.dropped_files.iter().filter_map(|f| f.path.clone()).collect());
        if !dropped.is_empty() {
            if dropped.len() >= 2 {
                self.handle(AppAction::Compare { left: dropped[0].clone(), right: dropped[1].clone() }, &ctx);
            } else if let Some(Tab::Home(h)) = self.tabs.get_mut(self.active) {
                h.accept_drop(&dropped);
            } else {
                let mut h = HomeView::default();
                h.accept_drop(&dropped);
                self.tabs.push(Tab::Home(h));
                self.active = self.tabs.len() - 1;
            }
        }

        // Global shortcuts.
        let (new_tab, close_tab, next_tab, prev_tab) = ctx.input_mut(|i| {
            (
                i.consume_shortcut(&KeyboardShortcut::new(Modifiers::COMMAND, Key::T)),
                i.consume_shortcut(&KeyboardShortcut::new(Modifiers::COMMAND, Key::W)),
                i.consume_shortcut(&KeyboardShortcut::new(Modifiers::CTRL, Key::Tab)),
                i.consume_shortcut(&KeyboardShortcut::new(Modifiers::CTRL | Modifiers::SHIFT, Key::Tab)),
            )
        });
        if new_tab {
            self.tabs.push(Tab::Home(HomeView::default()));
            self.active = self.tabs.len() - 1;
        }
        if close_tab {
            self.close_tab(self.active, false);
        }
        if next_tab {
            self.active = (self.active + 1) % self.tabs.len();
        }
        if prev_tab {
            self.active = (self.active + self.tabs.len() - 1) % self.tabs.len();
        }

        // Unsaved changes on window close.
        if ctx.input(|i| i.viewport().close_requested()) && !self.allow_close && self.tabs.iter().any(|t| t.dirty()) {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            self.close_req = Some(CloseReq::App);
        }

        egui::Panel::top("tabs")
            .frame(Frame::new().fill(pal.app_bg).inner_margin(Margin { left: 6, right: 6, top: 4, bottom: 0 }))
            .show(ui, |ui| self.tab_bar(ui, &pal, &ctx));

        if let Some((msg, t)) = &self.toast {
            if t.elapsed() > Duration::from_secs(6) {
                self.toast = None;
            } else {
                let msg = msg.clone();
                egui::Panel::bottom("toast")
                    .frame(Frame::new().fill(pal.error.gamma_multiply(0.2)).inner_margin(Margin::symmetric(10, 4)))
                    .show(ui, |ui| {
                        ui.label(RichText::new(msg).color(pal.text));
                    });
                ctx.request_repaint_after(Duration::from_millis(500));
            }
        }

        let mut actions = Vec::new();
        egui::CentralPanel::no_frame().show(ui, |ui| {
            let mut cx = ViewCtx { settings: &mut self.settings, pal, actions: &mut actions };
            match &mut self.tabs[self.active] {
                Tab::Home(h) => h.ui(ui, &mut cx),
                Tab::Dir(d) => d.ui(ui, &mut cx),
                Tab::File(f) => f.ui(ui, &mut cx),
            }
        });
        for a in actions {
            self.handle(a, &ctx);
        }

        self.settings_window(&ctx);
        self.about_window(&ctx, &pal);
        self.close_dialog(&ctx, &pal);
    }

    #[cfg(feature = "autotest")]
    fn raw_input_hook(&mut self, ctx: &egui::Context, raw_input: &mut egui::RawInput) {
        if let Some(d) = &mut self.driver {
            d.hook(ctx, raw_input);
        }
    }

    fn save(&mut self, storage: &mut dyn eframe::Storage) {
        eframe::set_value(storage, Settings::KEY, &self.settings);
    }

    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        let c = Palette::get(true).app_bg;
        egui::Rgba::from(c).to_array()
    }
}
