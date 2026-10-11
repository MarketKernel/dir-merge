//! Side-by-side file comparison view.
//!
//! Rendering is fully virtualized: only the rows inside the viewport are
//! materialized each frame, and the scroll position is kept as an `f64` pixel
//! offset so files with tens of millions of lines scroll precisely.

use std::collections::HashMap;
use std::fs;
use std::io::{BufWriter, Write};
use std::ops::Range;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, TryRecvError};
use egui::text::{LayoutJob, TextFormat};
use egui::{
    Align, Align2, Color32, CursorIcon, FontId, Frame, Id, Key, KeyboardShortcut, Layout, Margin,
    Modifiers, Pos2, Rect, RichText, Sense, Stroke, StrokeKind, Ui, UiBuilder, pos2, vec2,
};

use super::{ViewCtx, file_name, fmt_count, fmt_size, tool_button};
use crate::diff::{DiffModel, DiffOptions, InlineDiff, RowInfo, SegKind, WhitespaceMode, inline_diff};
use crate::highlight::{self, Highlighter, LineSpans};
use crate::text::{Document, TextSource};
use crate::theme::mono;

const OV_W: f32 = 18.0;
const CENTER_W: f32 = 46.0;
const SB_W: f32 = 12.0;
const HB_H: f32 = 10.0;
const TEXT_PAD: f32 = 8.0;
/// Characters of a single line that are rendered at most.
const MAX_DISPLAY: usize = 10_000;
/// Re-diff synchronously below this many total lines, in the background above.
const SYNC_DIFF_LIMIT: usize = 400_000;
const MAX_UNDO: usize = 200;

static NEXT_ID: AtomicU64 = AtomicU64::new(1);

fn waker(ctx: &egui::Context) -> impl Fn() + Send + Sync + Clone + 'static {
    let ctx = ctx.clone();
    move || ctx.request_repaint()
}

/// A location-bar mark: (top, bottom, side, color).
type OverviewMark = (f32, f32, usize, Color32);

struct Loaded {
    sources: [Arc<TextSource>; 2],
    model: DiffModel,
    syntax: Option<String>,
    bytes_identical: bool,
    elapsed: Duration,
}

struct Side {
    doc: Document,
    hl: Highlighter,
    dirty: bool,
}

#[derive(Clone)]
struct Snapshot {
    docs: [Document; 2],
    dirty: [bool; 2],
    model: DiffModel,
}

struct Data {
    sides: [Side; 2],
    model: DiffModel,
    model_gen: u64,
    opts: DiffOptions,
    inline: HashMap<usize, Option<InlineDiff>>,
    undo: Vec<Snapshot>,
    redo: Vec<Snapshot>,
    rediff_rx: Option<Receiver<(u64, DiffModel)>>,
    rediff_token: u64,
    syntax: Option<String>,
    binary: bool,
    bytes_identical: bool,
    elapsed: Duration,
}

#[derive(Clone, Copy, Debug)]
struct Sel {
    side: usize,
    anchor: usize,
    cursor: usize,
}

impl Sel {
    fn rows(&self) -> Range<usize> {
        self.anchor.min(self.cursor)..self.anchor.max(self.cursor) + 1
    }
}

struct Edit {
    side: usize,
    line: usize,
    text: String,
    original: String,
    focus: bool,
}

#[derive(Default)]
struct Find {
    open: bool,
    query: String,
    case: bool,
    focus: bool,
    msg: String,
}

enum Act {
    CopyHunk(usize, bool),
    CopyAll(bool),
    Nav(Nav),
    Undo,
    Redo,
    SaveAll,
    Save(usize),
    Reload,
    Swap,
    CopySelection,
    StartEdit(usize, usize),
    InsertLine(usize, usize),
    DeleteSelection,
    FindNext(bool),
    ForceReload,
}

#[derive(Clone, Copy)]
enum Nav {
    First,
    Prev,
    Next,
    Last,
}

pub struct FileView {
    uid: u64,
    pub paths: [Option<PathBuf>; 2],
    loading: Option<Receiver<Result<Loaded, String>>>,
    error: Option<String>,
    data: Option<Data>,
    force_text: bool,

    scroll_y: f64,
    scroll_target: Option<f64>,
    scroll_x: f32,
    current: Option<usize>,
    sel: Option<Sel>,
    edit: Option<Edit>,
    ctx_row: Option<(usize, usize)>,
    find: Find,
    goto: Option<String>,
    goto_focus: bool,
    toast: Option<(String, Instant)>,
    view_h: f32,
    row_h: f32,
    /// Cached location-bar marks keyed by (model generation, bar height).
    overview: Option<(u64, u32, Vec<OverviewMark>)>,
    pending_first_nav: bool,
    syntax_override: Option<Option<String>>,
    /// Last time the files were checked for external modification.
    last_disk_check: Instant,
    /// Sides whose file changed on disk since it was loaded.
    changed_on_disk: [bool; 2],
    /// Modification times of the files as we last knew them.
    disk_mtime: [Option<std::time::SystemTime>; 2],
}

impl FileView {
    pub fn new(left: Option<PathBuf>, right: Option<PathBuf>, ctx: &egui::Context, opts: &DiffOptions) -> Self {
        let mut v = FileView {
            uid: NEXT_ID.fetch_add(1, Ordering::Relaxed),
            paths: [left, right],
            loading: None,
            error: None,
            data: None,
            force_text: false,
            scroll_y: 0.0,
            scroll_target: None,
            scroll_x: 0.0,
            current: None,
            sel: None,
            edit: None,
            ctx_row: None,
            find: Find::default(),
            goto: None,
            goto_focus: false,
            toast: None,
            view_h: 600.0,
            row_h: 18.0,
            overview: None,
            pending_first_nav: true,
            syntax_override: None,
            last_disk_check: Instant::now(),
            changed_on_disk: [false; 2],
            disk_mtime: [None; 2],
        };
        v.start_load(ctx, opts);
        v
    }

    fn id(&self) -> Id {
        Id::new(("file-view", self.uid))
    }

    pub fn title(&self) -> String {
        let names: Vec<String> =
            self.paths.iter().map(|p| p.as_deref().map(file_name).unwrap_or_else(|| "Untitled".into())).collect();
        let base = if names[0] == names[1] { names[0].clone() } else { format!("{} ⬌ {}", names[0], names[1]) };
        if self.is_dirty() { format!("• {base}") } else { base }
    }

    pub fn tooltip(&self) -> String {
        let p = |i: usize| self.paths[i].as_ref().map(|p| p.display().to_string()).unwrap_or_else(|| "(none)".into());
        format!("{}\n{}", p(0), p(1))
    }

    pub fn is_dirty(&self) -> bool {
        self.data.as_ref().is_some_and(|d| d.sides.iter().any(|s| s.dirty))
    }

    fn start_load(&mut self, ctx: &egui::Context, opts: &DiffOptions) {
        let (tx, rx) = crossbeam_channel::bounded(1);
        let paths = self.paths.clone();
        let opts = opts.clone();
        let wake = waker(ctx);
        std::thread::Builder::new()
            .name("load-files".into())
            .spawn(move || {
                let t0 = Instant::now();
                let load = |p: &Option<PathBuf>| -> Result<Arc<TextSource>, String> {
                    match p {
                        Some(p) => TextSource::load(p).map(Arc::new).map_err(|e| format!("{}: {e}", p.display())),
                        None => Ok(Arc::new(TextSource::empty())),
                    }
                };
                let res = (|| {
                    let (l, r) = std::thread::scope(|sc| {
                        let h = sc.spawn(|| load(&paths[1]));
                        (load(&paths[0]), h.join().unwrap_or_else(|_| Err("loader panicked".into())))
                    });
                    let (l, r) = (l?, r?);
                    let bytes_identical = l.with_bytes(|a| r.with_bytes(|b| a == b));
                    let (ld, rd) = (Document::new(l.clone()), Document::new(r.clone()));
                    let model = DiffModel::compute(&ld, &rd, &opts);
                    let first = |d: &Document| if d.line_count() > 0 { d.line_text(0) } else { String::new() };
                    let syntax = highlight::detect_syntax(paths[0].as_deref(), &first(&ld))
                        .or_else(|| highlight::detect_syntax(paths[1].as_deref(), &first(&rd)));
                    Ok(Loaded { sources: [l, r], model, syntax, bytes_identical, elapsed: t0.elapsed() })
                })();
                tx.send(res).ok();
                wake();
            })
            .expect("spawn loader");
        self.loading = Some(rx);
        self.error = None;
        self.changed_on_disk = [false; 2];
        self.edit = None;
        self.sel = None;
    }

    fn install(&mut self, ld: Loaded, ctx: &egui::Context, opts: &DiffOptions) {
        let wake = waker(ctx);
        let mk = |src: &Arc<TextSource>| {
            let doc = Document::new(src.clone());
            let hl = Highlighter::new(&doc, wake.clone());
            Side { doc, hl, dirty: false }
        };
        let binary = ld.sources.iter().any(|s| s.binary);
        self.disk_mtime = [ld.sources[0].mtime, ld.sources[1].mtime];
        let sides = [mk(&ld.sources[0]), mk(&ld.sources[1])];
        self.data = Some(Data {
            sides,
            model: ld.model,
            model_gen: 0,
            opts: opts.clone(),
            inline: HashMap::new(),
            undo: Vec::new(),
            redo: Vec::new(),
            rediff_rx: None,
            rediff_token: 0,
            syntax: ld.syntax,
            binary,
            bytes_identical: ld.bytes_identical,
            elapsed: ld.elapsed,
        });
        self.overview = None;
        let total = self.data.as_ref().unwrap().model.total_rows();
        if self.scroll_y > total as f64 * self.row_h as f64 {
            self.scroll_y = 0.0;
        }
    }

    fn poll(&mut self, ctx: &egui::Context, cx: &ViewCtx) {
        if let Some(rx) = &self.loading {
            match rx.try_recv() {
                Ok(Ok(ld)) => {
                    self.loading = None;
                    self.install(ld, ctx, &cx.settings.diff);
                }
                Ok(Err(e)) => {
                    self.loading = None;
                    self.error = Some(e);
                }
                Err(TryRecvError::Empty) => {}
                Err(TryRecvError::Disconnected) => {
                    self.loading = None;
                    self.error = Some("Loading failed unexpectedly".into());
                }
            }
        }
        let Some(d) = &mut self.data else { return };
        if self.last_disk_check.elapsed() > Duration::from_secs(2) {
            self.last_disk_check = Instant::now();
            for side in 0..2 {
                if let (Some(p), Some(t)) = (&self.paths[side], self.disk_mtime[side]) {
                    let now = fs::metadata(p).and_then(|m| m.modified()).ok();
                    self.changed_on_disk[side] = now.is_some_and(|n| n != t);
                }
            }
        }
        ctx.request_repaint_after(Duration::from_secs(2));
        if let Some(rx) = &d.rediff_rx
            && let Ok((tok, m)) = rx.try_recv()
        {
            if tok == d.rediff_token {
                d.set_model(m);
                self.overview = None;
            }
            d.rediff_rx = None;
        }
        if cx.settings.diff != d.opts {
            d.opts = cx.settings.diff.clone();
            d.rediff(ctx);
            self.overview = None;
        }
        let syntax = match &self.syntax_override {
            Some(s) => s.clone(),
            None => d.syntax.clone(),
        };
        let syntax = if cx.settings.syntax_highlighting { syntax } else { None };
        let theme_name = if cx.pal.dark { &cx.settings.dark_syntax_theme } else { &cx.settings.light_syntax_theme };
        let theme = highlight::theme_by_name(theme_name);
        for s in &mut d.sides {
            s.hl.set_config(syntax.clone(), theme);
        }
        if let Some(c) = self.current
            && c >= d.model.hunks.len()
        {
            self.current = d.model.hunks.len().checked_sub(1);
        }
    }

    // ------------------------------------------------------------------ UI

    pub fn ui(&mut self, ui: &mut Ui, cx: &mut ViewCtx) {
        let ctx = ui.ctx().clone();
        self.poll(&ctx, cx);
        let pal = cx.pal;

        if self.loading.is_some() {
            ui.centered_and_justified(|ui| {
                ui.vertical_centered(|ui| {
                    ui.add_space(ui.available_height() * 0.4);
                    ui.add(egui::Spinner::new().size(28.0));
                    ui.label(RichText::new("Loading and comparing…").color(pal.text_dim));
                });
            });
            return;
        }
        if let Some(err) = self.error.clone() {
            ui.vertical_centered(|ui| {
                ui.add_space(ui.available_height() * 0.35);
                ui.label(RichText::new("⚠").size(36.0).color(pal.error));
                ui.label(RichText::new(err).color(pal.text));
                ui.add_space(8.0);
                if ui.button("Retry").clicked() {
                    self.start_load(&ctx, &cx.settings.diff);
                }
            });
            return;
        }
        if self.data.is_none() {
            return;
        }
        if self.data.as_ref().unwrap().binary && !self.force_text {
            self.binary_ui(ui, cx);
            return;
        }

        let mut acts: Vec<Act> = Vec::new();
        egui::Panel::top(self.id().with("toolbar"))
            .frame(Frame::new().fill(pal.panel_bg).inner_margin(Margin::symmetric(8, 5)))
            .show(ui, |ui| self.toolbar(ui, cx, &mut acts));
        if self.changed_on_disk.iter().any(|&c| c) {
            egui::Panel::top(self.id().with("disk-banner"))
                .frame(Frame::new().fill(pal.different.gamma_multiply(0.18)).inner_margin(Margin::symmetric(12, 5)))
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        let which = match self.changed_on_disk {
                            [true, true] => "Both files have",
                            [true, false] => "The left file has",
                            _ => "The right file has",
                        };
                        ui.label(RichText::new(format!("⚠  {which} been modified by another program.")).color(pal.text));
                        if ui.button("Reload").clicked() {
                            acts.push(Act::ForceReload);
                        }
                        if ui.button("Ignore").clicked() {
                            self.ignore_disk_change();
                        }
                    });
                });
        }
        egui::Panel::bottom(self.id().with("status"))
            .frame(Frame::new().fill(pal.panel_bg).inner_margin(Margin::symmetric(10, 3)))
            .show(ui, |ui| self.status_bar(ui, cx));
        egui::CentralPanel::no_frame().show(ui, |ui| {
            let mut area = ui.available_rect_before_wrap();
            let font = mono(cx.settings.code_font_size);
            let (row_h, char_w) = ctx.fonts_mut(|f| (f.row_height(&font).ceil() + 4.0, f.glyph_width(&font, '0')));
            self.row_h = row_h;
            let d = self.data.as_ref().unwrap();
            let max_lines = d.sides[0].doc.line_count().max(d.sides[1].doc.line_count()).max(1);
            let digits = (max_lines as f64).log10().floor() as usize + 1;
            let gutter_w = digits.max(3) as f32 * char_w + 20.0;

            // Headers aligned with the panes.
            let header = Rect::from_min_size(area.min, vec2(area.width(), 32.0));
            area.min.y = header.max.y;
            let lay = Lay::new(area, gutter_w, false);
            self.headers(ui, header, &lay, cx, &mut acts);

            if self.find.open || self.goto.is_some() {
                let bar = Rect::from_min_size(area.min, vec2(area.width(), 34.0));
                area.min.y = bar.max.y;
                self.find_bar(ui, bar, cx, &mut acts);
            }
            self.diff_area(ui, area, gutter_w, row_h, char_w, &font, cx, &mut acts);
        });
        self.handle_keys(ui, &mut acts);
        for a in acts {
            self.apply(a, &ctx, cx);
        }
        if let Some((_, t)) = &self.toast
            && t.elapsed() > Duration::from_secs(4)
        {
            self.toast = None;
        }
    }

    fn binary_ui(&mut self, ui: &mut Ui, cx: &mut ViewCtx) {
        let pal = cx.pal;
        let d = self.data.as_ref().unwrap();
        let same = d.bytes_identical;
        ui.vertical_centered(|ui| {
            ui.add_space(ui.available_height() * 0.3);
            ui.label(RichText::new("🗋").size(40.0).color(pal.text_faint));
            let (msg, col) = if same {
                ("Binary files are identical", pal.text)
            } else {
                ("Binary files differ", pal.text)
            };
            ui.label(RichText::new(msg).size(20.0).color(col));
            ui.add_space(6.0);
            for (i, s) in d.sides.iter().enumerate() {
                let p = self.paths[i].as_ref().map(|p| p.display().to_string()).unwrap_or_default();
                ui.label(RichText::new(format!("{p}  —  {}", fmt_size(s.doc.base.file_len))).color(pal.text_dim));
            }
            ui.add_space(12.0);
            if ui.button("Compare as text anyway").clicked() {
                self.force_text = true;
            }
        });
    }

    fn toolbar(&mut self, ui: &mut Ui, cx: &mut ViewCtx, acts: &mut Vec<Act>) {
        let d = self.data.as_ref().unwrap();
        let has_hunks = !d.model.hunks.is_empty();
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 2.0;
            if tool_button(ui, "⏮", "", "First difference", has_hunks).clicked() {
                acts.push(Act::Nav(Nav::First));
            }
            if tool_button(ui, "⏶", "", "Previous difference (Alt+⏶)", has_hunks).clicked() {
                acts.push(Act::Nav(Nav::Prev));
            }
            if tool_button(ui, "⏷", "", "Next difference (Alt+⏷)", has_hunks).clicked() {
                acts.push(Act::Nav(Nav::Next));
            }
            if tool_button(ui, "⏭", "", "Last difference", has_hunks).clicked() {
                acts.push(Act::Nav(Nav::Last));
            }
            ui.separator();
            let cur = self.current.is_some();
            if tool_button(ui, "➡", "Copy right", "Copy current difference to the right (Alt+⏵)", cur).clicked()
                && let Some(c) = self.current {
                    acts.push(Act::CopyHunk(d.model.hunks[c], true));
                }
            if tool_button(ui, "⬅", "Copy left", "Copy current difference to the left (Alt+⏴)", cur).clicked()
                && let Some(c) = self.current {
                    acts.push(Act::CopyHunk(d.model.hunks[c], false));
                }
            ui.menu_button(RichText::new("⏩ All"), |ui| {
                if ui.button("Copy all differences to the right  ➡").clicked() {
                    acts.push(Act::CopyAll(true));
                }
                if ui.button("Copy all differences to the left  ⬅").clicked() {
                    acts.push(Act::CopyAll(false));
                }
            });
            ui.separator();
            if tool_button(ui, "⟲", "", "Undo (Ctrl/Cmd+Z)", !d.undo.is_empty()).clicked() {
                acts.push(Act::Undo);
            }
            if tool_button(ui, "⟳", "", "Redo (Ctrl/Cmd+Shift+Z)", !d.redo.is_empty()).clicked() {
                acts.push(Act::Redo);
            }
            let dirty = d.sides.iter().any(|s| s.dirty);
            if tool_button(ui, "💾", "", "Save all (Ctrl/Cmd+S)", dirty).clicked() {
                acts.push(Act::SaveAll);
            }
            ui.separator();
            if tool_button(ui, "🔍", "", "Find (Ctrl/Cmd+F)", true).clicked() {
                self.find.open = true;
                self.find.focus = true;
            }
            if tool_button(ui, "⬌", "", "Swap sides", true).clicked() {
                acts.push(Act::Swap);
            }
            if tool_button(ui, "🔄", "", "Reload & recompare (F5)", true).clicked() {
                acts.push(Act::Reload);
            }

            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                let s = &mut *cx.settings;
                ui.menu_button("⚙ View", |ui| {
                    ui.set_min_width(240.0);
                    ui.label(RichText::new("Whitespace").strong());
                    ui.radio_value(&mut s.diff.whitespace, WhitespaceMode::Compare, "Compare whitespace");
                    ui.radio_value(&mut s.diff.whitespace, WhitespaceMode::IgnoreChanges, "Ignore changes in amount");
                    ui.radio_value(&mut s.diff.whitespace, WhitespaceMode::IgnoreAll, "Ignore all whitespace");
                    ui.separator();
                    ui.checkbox(&mut s.diff.ignore_case, "Ignore case");
                    ui.checkbox(&mut s.diff.ignore_blank_lines, "Ignore blank lines");
                    ui.separator();
                    ui.checkbox(&mut s.syntax_highlighting, "Syntax highlighting");
                    ui.checkbox(&mut s.show_whitespace, "Show whitespace");
                    ui.horizontal(|ui| {
                        ui.label("Font size");
                        ui.add(egui::DragValue::new(&mut s.code_font_size).range(8.0..=32.0).speed(0.1));
                    });
                    ui.horizontal(|ui| {
                        ui.label("Tab width");
                        ui.add(egui::DragValue::new(&mut s.tab_width).range(1..=16));
                    });
                });
            });
        });
    }

    fn status_bar(&mut self, ui: &mut Ui, cx: &mut ViewCtx) {
        let pal = cx.pal;
        let d = self.data.as_ref().unwrap();
        let mut new_syntax = None;
        ui.horizontal(|ui| {
            let n = d.model.hunks.len();
            if n == 0 {
                let msg = if d.bytes_identical || d.sides.iter().any(|s| s.dirty) {
                    "No differences"
                } else {
                    "No differences (with current options)"
                };
                ui.label(RichText::new(format!("✔ {msg}")).color(pal.text));
            } else {
                ui.label(
                    RichText::new(format!("{} difference{}", fmt_count(n), if n == 1 { "" } else { "s" }))
                        .color(pal.text),
                );
                if let Some(c) = self.current {
                    ui.label(RichText::new(format!("·  {} of {}", c + 1, fmt_count(n))).color(pal.text_dim));
                }
            }
            if d.rediff_rx.is_some() {
                ui.add(egui::Spinner::new().size(12.0));
                ui.label(RichText::new("Comparing…").color(pal.text_dim));
            }
            if let Some((msg, _)) = &self.toast {
                ui.label(RichText::new(format!("·  {msg}")).color(pal.accent));
            }
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                ui.label(RichText::new(format!("{:.0} ms", d.elapsed.as_secs_f64() * 1000.0)).color(pal.text_faint))
                    .on_hover_text("Load + compare time");
                let current = match &self.syntax_override {
                    Some(s) => s.clone(),
                    None => d.syntax.clone(),
                };
                let mut chosen = current.clone();
                egui::ComboBox::from_id_salt(self.id().with("syntax"))
                    .selected_text(chosen.clone().unwrap_or_else(|| "Plain text".into()))
                    .height(400.0)
                    .show_ui(ui, |ui| {
                        ui.selectable_value(&mut chosen, None, "Plain text");
                        for name in highlight::syntax_names() {
                            ui.selectable_value(&mut chosen, Some(name.clone()), name);
                        }
                    });
                if chosen != current {
                    new_syntax = Some(chosen);
                }
            });
        });
        if let Some(s) = new_syntax {
            self.syntax_override = Some(s);
        }
    }

    fn headers(&mut self, ui: &mut Ui, header: Rect, lay: &Lay, cx: &mut ViewCtx, acts: &mut Vec<Act>) {
        let pal = cx.pal;
        ui.painter().rect_filled(header, 0.0, pal.panel_bg);
        ui.painter().line_segment(
            [header.left_bottom(), header.right_bottom()],
            Stroke::new(1.0, pal.border),
        );
        let d = self.data.as_ref().unwrap();
        for side in 0..2 {
            let r = Rect::from_x_y_ranges(lay.gutter[side].left()..=lay.text[side].right(), header.y_range()).shrink2(vec2(6.0, 3.0));
            let src = &d.sides[side].doc.base;
            let dirty = d.sides[side].dirty;
            ui.scope_builder(UiBuilder::new().max_rect(r).layout(Layout::left_to_right(Align::Center)), |ui| {
                ui.spacing_mut().item_spacing.x = 6.0;
                let accent = if side == 0 { pal.row_left_only_bg } else { pal.row_right_only_bg };
                ui.label(RichText::new("■").color(accent));
                let path = self.paths[side].as_ref().map(|p| p.display().to_string()).unwrap_or_else(|| "Untitled".into());
                let fname = self.paths[side].as_deref().map(file_name).unwrap_or_else(|| "Untitled".into());
                let dir = self.paths[side].as_deref().and_then(|p| p.parent()).map(|p| p.display().to_string()).unwrap_or_default();
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if dirty && ui.add(egui::Button::new("💾 Save").small()).on_hover_text("Save this side").clicked() {
                        acts.push(Act::Save(side));
                    }
                    let info = format!(
                        "{} · {} · {} lines",
                        src.encoding.label(),
                        src.eol.label(),
                        fmt_count(d.sides[side].doc.line_count())
                    );
                    ui.label(RichText::new(info).small().color(pal.text_faint)).on_hover_text(format!(
                        "{}{}",
                        fmt_size(src.file_len),
                        if src.mapped { " · memory-mapped" } else { "" }
                    ));
                    ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
                        let name = if dirty { format!("{fname} •") } else { fname.clone() };
                        ui.label(RichText::new(name).strong().color(pal.text))
                            .on_hover_text(&path);
                        ui.add(egui::Label::new(RichText::new(dir).small().color(pal.text_dim)).truncate()).on_hover_text(&path);
                    });
                });
            });
        }
    }

    fn find_bar(&mut self, ui: &mut Ui, bar: Rect, cx: &mut ViewCtx, acts: &mut Vec<Act>) {
        let pal = cx.pal;
        ui.painter().rect_filled(bar, 0.0, pal.panel_bg);
        ui.painter().line_segment([bar.left_bottom(), bar.right_bottom()], Stroke::new(1.0, pal.border));
        ui.scope_builder(
            UiBuilder::new().max_rect(bar.shrink2(vec2(10.0, 4.0))).layout(Layout::left_to_right(Align::Center)),
            |ui| {
                if self.find.open {
                    let r = ui.add(
                        egui::TextEdit::singleline(&mut self.find.query).hint_text("Find in both files").desired_width(260.0),
                    );
                    if self.find.focus {
                        r.request_focus();
                        self.find.focus = false;
                    }
                    if r.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter)) {
                        let shift = ui.input(|i| i.modifiers.shift);
                        acts.push(Act::FindNext(!shift));
                        r.request_focus();
                    }
                    if r.changed() {
                        self.find.msg.clear();
                    }
                    ui.toggle_value(&mut self.find.case, "Aa").on_hover_text("Match case");
                    if tool_button(ui, "⏶", "", "Previous match (Shift+Enter)", true).clicked() {
                        acts.push(Act::FindNext(false));
                    }
                    if tool_button(ui, "⏷", "", "Next match (Enter / F3)", true).clicked() {
                        acts.push(Act::FindNext(true));
                    }
                    ui.label(RichText::new(&self.find.msg).color(pal.text_dim));
                    if tool_button(ui, "✖", "", "Close (Esc)", true).clicked() {
                        self.find.open = false;
                    }
                }
                if let Some(g) = &mut self.goto {
                    ui.separator();
                    ui.label("Go to line");
                    let r = ui.add(egui::TextEdit::singleline(g).desired_width(90.0).hint_text("line"));
                    if self.goto_focus {
                        r.request_focus();
                        self.goto_focus = false;
                    }
                    let mut close = tool_button(ui, "✖", "", "Close", true).clicked();
                    if r.lost_focus() {
                        if ui.input(|i| i.key_pressed(Key::Enter))
                            && let Ok(n) = g.trim().parse::<usize>()
                        {
                            let d = self.data.as_ref().unwrap();
                            let side = if d.sides[0].doc.line_count() > 0 { 0 } else { 1 };
                            let line = n.saturating_sub(1).min(d.sides[side].doc.line_count().saturating_sub(1));
                            let row = d.model.row_of_line(line, side == 1);
                            self.sel = Some(Sel { side, anchor: row, cursor: row });
                            self.scroll_to_row(row, true);
                        }
                        close = true;
                    }
                    if close {
                        self.goto = None;
                    }
                }
            },
        );
    }

    #[allow(clippy::too_many_arguments)]
    fn diff_area(
        &mut self,
        ui: &mut Ui,
        area: Rect,
        gutter_w: f32,
        row_h: f32,
        char_w: f32,
        font: &FontId,
        cx: &mut ViewCtx,
        acts: &mut Vec<Act>,
    ) {
        let pal = cx.pal;
        let id = self.id();
        let d = self.data.as_mut().unwrap();
        let total = d.model.total_rows();

        // Horizontal extent: longest line of both sources (bounded).
        let max_chars = d.sides.iter().map(|s| s.doc.base.max_line_len).max().unwrap_or(0).min(MAX_DISPLAY) + 2;
        let content_w = max_chars as f32 * char_w + TEXT_PAD * 2.0;
        let probe = Lay::new(area, gutter_w, false);
        let need_h = content_w > probe.text[0].width();
        let lay = Lay::new(area, gutter_w, need_h);
        let view_h = lay.text[0].height();
        self.view_h = view_h;

        // ---- scrolling input
        let max_scroll = ((total as f64 + 2.0) * row_h as f64 - view_h as f64).max(0.0);
        let max_scroll_x = (content_w - lay.text[0].width()).max(0.0);
        if ui.rect_contains_pointer(area) {
            let (delta, shift) = ui.input(|i| (i.smooth_scroll_delta, i.modifiers.shift));
            let (dx, dy) = if shift && delta.x == 0.0 { (delta.y, 0.0) } else { (delta.x, delta.y) };
            if dx != 0.0 || dy != 0.0 {
                self.scroll_target = None;
                self.scroll_y -= dy as f64;
                self.scroll_x -= dx;
            }
        }
        if let Some(t) = self.scroll_target {
            let t = t.clamp(0.0, max_scroll);
            let dt = ui.input(|i| i.stable_dt).min(0.05) as f64;
            let k = (dt * 16.0).min(1.0);
            self.scroll_y += (t - self.scroll_y) * k;
            if (t - self.scroll_y).abs() < 0.5 {
                self.scroll_y = t;
                self.scroll_target = None;
            } else {
                ui.ctx().request_repaint();
            }
        }
        self.scroll_y = self.scroll_y.clamp(0.0, max_scroll);
        self.scroll_x = self.scroll_x.clamp(0.0, max_scroll_x);
        let scroll_y = self.scroll_y;

        let first = (scroll_y / row_h as f64).floor() as usize;
        let y0 = lay.text[0].top() - (scroll_y - first as f64 * row_h as f64) as f32;
        let last = (first + (view_h / row_h).ceil() as usize + 1).min(total);
        let rows: Vec<RowInfo> = (first..last).map(|r| d.model.row_info(r)).collect();

        // ---- materialize visible text
        let mut texts: [Vec<Option<String>>; 2] = [Vec::new(), Vec::new()];
        let mut spans: [(usize, Vec<Option<LineSpans>>); 2] = [(0, Vec::new()), (0, Vec::new())];
        for side in 0..2 {
            let doc = &d.sides[side].doc;
            let n = doc.line_count();
            texts[side] = rows.iter().map(|r| line_of(r, side).filter(|&l| l < n).map(|l| doc.line_text(l))).collect();
            let lines: Vec<usize> = rows.iter().filter_map(|r| line_of(r, side)).filter(|&l| l < n).collect();
            if let (Some(&lo), Some(&hi)) = (lines.iter().min(), lines.iter().max()) {
                spans[side] = (lo, d.sides[side].hl.fetch(lo..hi + 1));
            }
        }
        for (i, r) in rows.iter().enumerate() {
            if r.kind == SegKind::Change
                && let (Some(a), Some(b)) = (&texts[0][i], &texts[1][i])
            {
                let row = first + i;
                let opts = &d.opts;
                d.inline.entry(row).or_insert_with(|| inline_diff(a, b, opts));
            }
        }

        // ---- paint
        let painter = ui.painter_at(area);
        painter.rect_filled(area, 0.0, pal.editor_bg);
        for side in 0..2 {
            painter.rect_filled(lay.gutter[side], 0.0, pal.gutter_bg);
        }
        painter.rect_filled(lay.center, 0.0, pal.gutter_bg);

        let find_q = if self.find.open && !self.find.query.is_empty() { Some(self.find.query.as_str()) } else { None };
        let font_h = ui.ctx().fonts_mut(|f| f.row_height(font));
        let tab_w = cx.settings.tab_width.max(1);
        let show_ws = cx.settings.show_whitespace;
        for (i, r) in rows.iter().enumerate() {
            let row = first + i;
            let y = y0 + i as f32 * row_h;
            for side in 0..2 {
                let has = line_of(r, side).is_some();
                let other = line_of(r, 1 - side).is_some();
                let full = Rect::from_min_max(pos2(lay.gutter[side].left(), y), pos2(lay.text[side].right(), y + row_h));
                let bg = match r.kind {
                    SegKind::Equal => None,
                    SegKind::Ignored if has => Some(pal.ignored_bg),
                    SegKind::Change if has && other => Some(pal.mod_bg),
                    SegKind::Change if has => Some(pal.ins_bg),
                    _ => None,
                };
                if let Some(bg) = bg {
                    painter.rect_filled(full, 0.0, bg);
                }
                if !has && r.kind != SegKind::Equal {
                    let fr = full.intersect(lay.text[side].union(lay.gutter[side]));
                    painter.rect_filled(fr, 0.0, pal.filler_bg);
                    hatch(&painter.with_clip_rect(fr.intersect(area)), fr, (scroll_y % 9.0) as f32, pal.filler_hatch);
                }
                if let Some(sel) = self.sel
                    && sel.side == side
                    && sel.rows().contains(&row)
                {
                    painter.rect_filled(full, 0.0, pal.selection);
                }
                // Gutter: change marker + line number.
                if r.kind == SegKind::Change && has {
                    let m = Rect::from_min_size(pos2(lay.gutter[side].left(), y), vec2(3.0, row_h));
                    painter.rect_filled(m, 0.0, if other { pal.mod_mark } else { pal.ins_mark });
                }
                if let Some(l) = line_of(r, side) {
                    let col = if r.kind == SegKind::Equal { pal.text_faint } else { pal.text_dim };
                    painter.text(
                        pos2(lay.gutter[side].right() - 8.0, y + row_h / 2.0),
                        Align2::RIGHT_CENTER,
                        (l + 1).to_string(),
                        font.clone(),
                        col,
                    );
                }
                // Text.
                if let Some(text) = &texts[side][i] {
                    let line = line_of(r, side).unwrap();
                    let (lo, sp) = &spans[side];
                    let line_spans = sp.get(line.wrapping_sub(*lo)).and_then(|s| s.as_ref());
                    let inline: &[Range<usize>] = match d.inline.get(&row) {
                        Some(Some(id)) => {
                            if side == 0 { &id.left } else { &id.right }
                        }
                        _ => &[],
                    };
                    let whole_line_changed = r.kind == SegKind::Change && other && matches!(d.inline.get(&row), Some(None));
                    let marks = find_q.map(|q| find_all(text, q, self.find.case)).unwrap_or_default();
                    let st = JobStyle {
                        font: font.clone(),
                        fg: pal.text,
                        inline_bg: if other { pal.mod_inline } else { pal.ins_inline },
                        mark_bg: pal.accent.gamma_multiply(0.45),
                        ws: pal.whitespace,
                        tab_w,
                        show_ws,
                    };
                    let job = build_job(text, line_spans.map(|s| &s[..]), inline, &marks, &st);
                    let galley = painter.layout_job(job);
                    let tp = lay.text[side];
                    let pos = pos2(tp.left() + TEXT_PAD - self.scroll_x, y + (row_h - font_h) / 2.0);
                    if whole_line_changed {
                        let w = galley.size().x;
                        let hl = Rect::from_min_size(pos2(pos.x - 2.0, y + 1.0), vec2(w + 4.0, row_h - 2.0));
                        painter.with_clip_rect(tp).rect_filled(hl, 3.0, pal.mod_inline);
                    }
                    painter.with_clip_rect(tp.intersect(area)).galley(pos, galley, pal.text);
                }
            }
        }

        // Current difference outline.
        if let Some(c) = self.current
            && let Some(&seg) = d.model.hunks.get(c)
        {
            let rr = d.model.seg_rows(seg);
            if rr.end > first && rr.start < last + 1 {
                let top = y0 + (rr.start as f32 - first as f32) * row_h;
                let bot = y0 + (rr.end as f32 - first as f32) * row_h;
                let x0 = lay.gutter[0].left();
                let x1 = lay.text[1].right();
                let st = Stroke::new(1.5, pal.accent);
                let clip = painter.with_clip_rect(Rect::from_x_y_ranges(x0..=x1, lay.text[0].y_range()));
                clip.line_segment([pos2(x0, top), pos2(x1, top)], st);
                clip.line_segment([pos2(x0, bot), pos2(x1, bot)], st);
            }
        }

        // ---- center strip with merge buttons
        if !rows.is_empty() {
            let s0 = rows.first().unwrap().seg;
            let s1 = rows.last().unwrap().seg;
            let center_painter = painter.with_clip_rect(lay.center.intersect(lay.text[0].union(lay.center)));
            for seg in s0..=s1 {
                let s = d.model.segments[seg];
                if s.kind == SegKind::Equal {
                    continue;
                }
                let rr = d.model.seg_rows(seg);
                let top = y0 + (rr.start as f32 - first as f32) * row_h;
                let bot = y0 + (rr.end as f32 - first as f32) * row_h;
                let band = Rect::from_x_y_ranges(lay.center.x_range(), top..=bot);
                let col = match s.kind {
                    SegKind::Ignored => pal.ignored_bg,
                    _ if s.left_len > 0 && s.right_len > 0 => pal.mod_bg,
                    _ => pal.ins_bg,
                };
                center_painter.rect_filled(band, 0.0, col);
                if s.kind != SegKind::Change {
                    continue;
                }
                // Keep the buttons visible while a large hunk scrolls past.
                let by = top.max(lay.text[0].top()).min(bot - row_h);
                let bh = row_h.min(22.0);
                let lb = Rect::from_min_size(pos2(lay.center.left() + 3.0, by + (row_h - bh) / 2.0), vec2(CENTER_W / 2.0 - 4.0, bh));
                let rb = Rect::from_min_size(pos2(lay.center.center().x + 1.0, lb.top()), lb.size());
                for (rect, to_right, glyph, tip) in
                    [(lb, true, "⏵", "Copy to right (Alt+⏵)"), (rb, false, "⏴", "Copy to left (Alt+⏴)")]
                {
                    if rect.top() < lay.text[0].top() - 1.0 || rect.bottom() > lay.text[0].bottom() + 1.0 {
                        continue;
                    }
                    let resp = ui.interact(rect, id.with(("merge", seg, to_right)), Sense::click()).on_hover_text(tip);
                    let hov = resp.hovered();
                    let fill = if hov { pal.accent } else { pal.panel_bg.gamma_multiply(0.85) };
                    painter.rect(rect, 4.0, fill, Stroke::new(1.0, if hov { pal.accent } else { pal.border }), StrokeKind::Inside);
                    painter.text(
                        rect.center(),
                        Align2::CENTER_CENTER,
                        glyph,
                        FontId::proportional(13.0),
                        if hov { Color32::WHITE } else { pal.text_dim },
                    );
                    if resp.clicked() {
                        acts.push(Act::CopyHunk(seg, to_right));
                    }
                }
            }
        }
        // Pane separators.
        for x in [lay.center.left(), lay.center.right(), lay.gutter[0].left(), lay.vscroll.left()] {
            painter.line_segment([pos2(x, area.top()), pos2(x, area.bottom())], Stroke::new(1.0, pal.border));
        }

        // ---- pane interaction (selection, editing, context menu)
        let pointer = ui.input(|i| i.pointer.interact_pos());
        let row_at = |p: Pos2| -> usize {
            let r = first as f64 + ((p.y - y0) / row_h).floor() as f64;
            (r.max(0.0) as usize).min(total.saturating_sub(1))
        };
        for side in 0..2 {
            let pane = lay.gutter[side].union(lay.text[side]);
            let resp = ui.interact(pane, id.with(("pane", side)), Sense::click_and_drag());
            let resp = resp.on_hover_cursor(CursorIcon::Default);
            if total == 0 {
                continue;
            }
            if let Some(p) = pointer {
                let row = row_at(p);
                if resp.drag_started() || resp.clicked() {
                    let shift = ui.input(|i| i.modifiers.shift);
                    match self.sel {
                        Some(ref mut s) if shift && s.side == side => s.cursor = row,
                        _ => self.sel = Some(Sel { side, anchor: row, cursor: row }),
                    }
                    self.current = d.model.hunk_containing(row);
                }
                if resp.dragged()
                    && let Some(s) = &mut self.sel
                {
                    s.cursor = row;
                    // Auto-scroll while dragging beyond the edges.
                    if p.y < lay.text[0].top() {
                        self.scroll_y -= (lay.text[0].top() - p.y) as f64 * 0.5;
                        ui.ctx().request_repaint();
                    } else if p.y > lay.text[0].bottom() {
                        self.scroll_y += (p.y - lay.text[0].bottom()) as f64 * 0.5;
                        ui.ctx().request_repaint();
                    }
                }
                if resp.double_clicked() {
                    acts.push(Act::StartEdit(side, row));
                }
                if resp.secondary_clicked() {
                    self.ctx_row = Some((side, row));
                    if !self.sel.is_some_and(|s| s.side == side && s.rows().contains(&row)) {
                        self.sel = Some(Sel { side, anchor: row, cursor: row });
                    }
                }
            }
            let ctx_row = self.ctx_row;
            let current = ctx_row.and_then(|(_, r)| d.model.hunk_containing(r)).map(|h| d.model.hunks[h]);
            resp.context_menu(|ui| {
                ui.set_min_width(220.0);
                if ui.button("Copy text").clicked() {
                    acts.push(Act::CopySelection);
                    ui.close();
                }
                if let Some((s, r)) = ctx_row {
                    if ui.button("Edit line").clicked() {
                        acts.push(Act::StartEdit(s, r));
                        ui.close();
                    }
                    if ui.button("Insert line below").clicked() {
                        acts.push(Act::InsertLine(s, r));
                        ui.close();
                    }
                }
                if ui.button("Delete selected lines").clicked() {
                    acts.push(Act::DeleteSelection);
                    ui.close();
                }
                if let Some(seg) = current {
                    ui.separator();
                    if ui.button("Copy this difference to the right  ➡").clicked() {
                        acts.push(Act::CopyHunk(seg, true));
                        ui.close();
                    }
                    if ui.button("Copy this difference to the left  ⬅").clicked() {
                        acts.push(Act::CopyHunk(seg, false));
                        ui.close();
                    }
                }
            });
        }

        // ---- inline line editor
        let mut commit: Option<bool> = None;
        if let Some(e) = &mut self.edit {
            let row = d.model.row_of_line(e.line, e.side == 1);
            if row >= first && row < last {
                let y = y0 + (row - first) as f32 * row_h;
                let tp = lay.text[e.side];
                let rect = Rect::from_min_max(pos2(tp.left(), y), pos2(tp.right(), y + row_h));
                painter.rect_filled(rect, 0.0, pal.editor_bg);
                painter.rect_stroke(rect, 2.0, Stroke::new(1.0, pal.accent), StrokeKind::Inside);
                let r = ui.put(
                    rect,
                    egui::TextEdit::singleline(&mut e.text)
                        .font(font.clone())
                        .frame(Frame::NONE)
                        .margin(Margin::symmetric(TEXT_PAD as i8, 1))
                        .desired_width(rect.width()),
                );
                if e.focus {
                    r.request_focus();
                    e.focus = false;
                } else if r.lost_focus() {
                    commit = Some(!ui.input(|i| i.key_pressed(Key::Escape)));
                }
            } else {
                // The edited line scrolled out of view: commit the edit.
                commit = Some(true);
            }
        }
        if let Some(ok) = commit {
            let e = self.edit.take().unwrap();
            if ok && e.text != e.original {
                d.push_undo();
                d.sides[e.side].doc.replace_lines(e.line..e.line + 1, vec![e.text]);
                d.after_edit(e.side, e.line, ui.ctx());
            }
        }

        // ---- overview (location) bar
        self.overview_bar(ui, &lay, total, row_h, view_h, cx);
        // ---- scrollbars
        self.scrollbars(ui, &lay, total, row_h, view_h, max_scroll, content_w, cx);
    }

    fn overview_bar(&mut self, ui: &mut Ui, lay: &Lay, total: usize, row_h: f32, view_h: f32, cx: &ViewCtx) {
        let pal = cx.pal;
        let d = self.data.as_ref().unwrap();
        let ov = lay.overview;
        let painter = ui.painter_at(ov);
        painter.rect_filled(ov, 0.0, pal.panel_bg);
        if total == 0 {
            return;
        }
        let h = ov.height();
        let key = (d.model_gen, h as u32);
        if self.overview.as_ref().is_none_or(|(g, hh, _)| (*g, *hh) != key) {
            let mut marks: Vec<OverviewMark> = Vec::new();
            for &seg in &d.model.hunks {
                let s = d.model.segments[seg];
                let rr = d.model.seg_rows(seg);
                let a = rr.start as f32 / total as f32 * h;
                let b = (rr.end as f32 / total as f32 * h).max(a + 2.0);
                for side in 0..2 {
                    let (mine, other) = if side == 0 { (s.left_len, s.right_len) } else { (s.right_len, s.left_len) };
                    let col = match (mine > 0, other > 0) {
                        (true, true) => pal.mod_mark,
                        (true, false) => pal.ins_mark,
                        _ => pal.text_faint,
                    };
                    if let Some(last) = marks.iter_mut().rev().find(|m| m.2 == side)
                        && last.3 == col
                        && a <= last.1 + 0.5
                    {
                        last.1 = last.1.max(b);
                        continue;
                    }
                    marks.push((a, b, side, col));
                }
            }
            self.overview = Some((key.0, key.1, marks));
        }
        let (_, _, marks) = self.overview.as_ref().unwrap();
        for &(a, b, side, col) in marks {
            let x = ov.left() + 3.0 + side as f32 * 6.5;
            painter.rect_filled(Rect::from_min_max(pos2(x, ov.top() + a), pos2(x + 5.0, ov.top() + b)), 1.0, col);
        }
        let content_h = total as f64 * row_h as f64;
        let vy = (self.scroll_y / content_h) as f32 * h;
        let vh = ((view_h as f64 / content_h) as f32 * h).clamp(6.0, h);
        let vr = Rect::from_min_size(pos2(ov.left() + 1.0, ov.top() + vy), vec2(ov.width() - 2.0, vh));
        painter.rect(vr, 2.0, pal.accent_soft, Stroke::new(1.0, pal.accent.gamma_multiply(0.6)), StrokeKind::Inside);

        let resp = ui.interact(ov, self.id().with("overview"), Sense::click_and_drag()).on_hover_cursor(CursorIcon::PointingHand);
        if (resp.clicked() || resp.dragged())
            && let Some(p) = resp.interact_pointer_pos()
        {
            let frac = ((p.y - ov.top()) / h).clamp(0.0, 1.0) as f64;
            self.scroll_target = None;
            self.scroll_y = frac * content_h - view_h as f64 / 2.0;
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn scrollbars(
        &mut self,
        ui: &mut Ui,
        lay: &Lay,
        total: usize,
        row_h: f32,
        view_h: f32,
        max_scroll: f64,
        content_w: f32,
        cx: &ViewCtx,
    ) {
        let pal = cx.pal;
        let track = lay.vscroll;
        let painter = ui.painter_at(track);
        painter.rect_filled(track, 0.0, pal.panel_bg);
        let content_h = (total as f64 + 2.0) * row_h as f64;
        if content_h > view_h as f64 {
            let th = ((view_h as f64 / content_h) as f32 * track.height()).max(28.0);
            let ty = if max_scroll > 0.0 { (self.scroll_y / max_scroll) as f32 * (track.height() - th) } else { 0.0 };
            let thumb = Rect::from_min_size(pos2(track.left() + 2.0, track.top() + ty), vec2(track.width() - 4.0, th));
            let resp = ui.interact(track, self.id().with("vscroll"), Sense::click_and_drag());
            let active = resp.hovered() || resp.dragged();
            painter.rect_filled(thumb, 4.0, if active { pal.text_dim } else { pal.text_faint.gamma_multiply(0.7) });
            if resp.dragged() {
                let k = max_scroll / (track.height() - th).max(1.0) as f64;
                self.scroll_y += resp.drag_delta().y as f64 * k;
                self.scroll_target = None;
            } else if resp.clicked()
                && let Some(p) = resp.interact_pointer_pos()
            {
                let page = view_h as f64 * 0.9;
                self.scroll_y += if p.y < thumb.top() { -page } else if p.y > thumb.bottom() { page } else { 0.0 };
            }
        }
        if let Some(hb) = lay.hscroll {
            for side in 0..2 {
                let tr = Rect::from_x_y_ranges(lay.text[side].x_range(), hb.y_range());
                let p = ui.painter_at(tr);
                p.rect_filled(tr, 0.0, pal.panel_bg);
                let vis = lay.text[side].width();
                let tw = (vis / content_w * tr.width()).max(30.0);
                let maxx = (content_w - vis).max(1.0);
                let tx = self.scroll_x / maxx * (tr.width() - tw);
                let thumb = Rect::from_min_size(pos2(tr.left() + tx, tr.top() + 2.0), vec2(tw, tr.height() - 4.0));
                let resp = ui.interact(tr, self.id().with(("hscroll", side)), Sense::click_and_drag());
                let active = resp.hovered() || resp.dragged();
                p.rect_filled(thumb, 4.0, if active { pal.text_dim } else { pal.text_faint.gamma_multiply(0.7) });
                if resp.dragged() {
                    self.scroll_x += resp.drag_delta().x * maxx / (tr.width() - tw).max(1.0);
                }
            }
        }
    }

    fn handle_keys(&mut self, ui: &mut Ui, acts: &mut Vec<Act>) {
        let typing = ui.ctx().egui_wants_keyboard_input();
        let cmd = |k: Key| KeyboardShortcut::new(Modifiers::COMMAND, k);
        let cmd_shift = |k: Key| KeyboardShortcut::new(Modifiers::COMMAND | Modifiers::SHIFT, k);
        ui.input_mut(|i| {
            if i.consume_shortcut(&cmd(Key::S)) {
                acts.push(Act::SaveAll);
            }
            if i.consume_shortcut(&cmd(Key::F)) {
                self.find.open = true;
                self.find.focus = true;
            }
            if i.consume_shortcut(&cmd(Key::L)) {
                self.goto = Some(String::new());
                self.goto_focus = true;
            }
            if i.consume_key(Modifiers::NONE, Key::F5) {
                acts.push(Act::Reload);
            }
            if i.consume_key(Modifiers::SHIFT, Key::F3) {
                acts.push(Act::FindNext(false));
            } else if i.consume_key(Modifiers::NONE, Key::F3) {
                acts.push(Act::FindNext(true));
            }
            if typing {
                return;
            }
            if i.consume_shortcut(&cmd_shift(Key::Z)) || i.consume_shortcut(&cmd(Key::Y)) {
                acts.push(Act::Redo);
            } else if i.consume_shortcut(&cmd(Key::Z)) {
                acts.push(Act::Undo);
            }
            if i.consume_shortcut(&cmd(Key::C)) || i.events.iter().any(|e| matches!(e, egui::Event::Copy)) {
                acts.push(Act::CopySelection);
            }
            if i.consume_key(Modifiers::ALT, Key::ArrowDown) {
                acts.push(Act::Nav(Nav::Next));
            }
            if i.consume_key(Modifiers::ALT, Key::ArrowUp) {
                acts.push(Act::Nav(Nav::Prev));
            }
            if i.consume_key(Modifiers::ALT, Key::Home) {
                acts.push(Act::Nav(Nav::First));
            }
            if i.consume_key(Modifiers::ALT, Key::End) {
                acts.push(Act::Nav(Nav::Last));
            }
            if let Some(c) = self.current
                && let Some(d) = &self.data
                && let Some(&seg) = d.model.hunks.get(c)
            {
                if i.consume_key(Modifiers::ALT, Key::ArrowRight) {
                    acts.push(Act::CopyHunk(seg, true));
                }
                if i.consume_key(Modifiers::ALT, Key::ArrowLeft) {
                    acts.push(Act::CopyHunk(seg, false));
                }
            }
            let row_h = self.row_h as f64;
            let page = self.view_h as f64 * 0.9;
            if i.consume_key(Modifiers::NONE, Key::ArrowDown) {
                self.scroll_y += row_h * 3.0;
            }
            if i.consume_key(Modifiers::NONE, Key::ArrowUp) {
                self.scroll_y -= row_h * 3.0;
            }
            if i.consume_key(Modifiers::NONE, Key::PageDown) || i.consume_key(Modifiers::NONE, Key::Space) {
                self.scroll_y += page;
            }
            if i.consume_key(Modifiers::NONE, Key::PageUp) {
                self.scroll_y -= page;
            }
            if i.consume_shortcut(&cmd(Key::Home)) || i.consume_key(Modifiers::NONE, Key::Home) {
                self.scroll_target = Some(0.0);
            }
            if i.consume_shortcut(&cmd(Key::End)) || i.consume_key(Modifiers::NONE, Key::End) {
                self.scroll_target = None;
                self.scroll_y = f64::MAX / 4.0;
            }
            if i.consume_key(Modifiers::NONE, Key::Escape) {
                if self.find.open {
                    self.find.open = false;
                } else {
                    self.sel = None;
                }
            }
        });
        if self.pending_first_nav
            && let Some(d) = &self.data
        {
            self.pending_first_nav = false;
            if !d.model.hunks.is_empty() {
                acts.push(Act::Nav(Nav::First));
            }
        }
    }

    fn scroll_to_row(&mut self, row: usize, animate: bool) {
        let target = row as f64 * self.row_h as f64 - self.view_h as f64 / 3.0;
        let target = target.max(0.0);
        if animate && (target - self.scroll_y).abs() < self.view_h as f64 * 6.0 {
            self.scroll_target = Some(target);
        } else {
            self.scroll_target = None;
            self.scroll_y = target;
        }
    }

    // ------------------------------------------------------------------ actions

    fn apply(&mut self, act: Act, ctx: &egui::Context, cx: &mut ViewCtx) {
        let Some(d) = self.data.as_mut() else { return };
        // Line numbers in the model are stale until a background re-diff lands.
        let needs_model = matches!(
            act,
            Act::CopyHunk(..) | Act::CopySelection | Act::StartEdit(..) | Act::InsertLine(..) | Act::DeleteSelection
        );
        if needs_model && d.rediff_rx.is_some() {
            self.toast = Some(("Still comparing — try again in a moment".into(), Instant::now()));
            return;
        }
        match act {
            Act::Nav(nav) => {
                let n = d.model.hunks.len();
                if n == 0 {
                    return;
                }
                let first_vis = (self.scroll_y / self.row_h as f64) as usize;
                let idx = match nav {
                    Nav::First => 0,
                    Nav::Last => n - 1,
                    Nav::Next => match self.current {
                        Some(c) => (c + 1).min(n - 1),
                        None => d.model.hunk_at_or_after(first_vis).unwrap_or(n - 1),
                    },
                    Nav::Prev => match self.current {
                        Some(c) => c.saturating_sub(1),
                        None => d.model.hunk_at_or_after(first_vis).unwrap_or(n).saturating_sub(1),
                    },
                };
                self.current = Some(idx);
                let row = d.model.seg_rows(d.model.hunks[idx]).start;
                self.sel = None;
                self.scroll_to_row(row, true);
            }
            Act::CopyHunk(seg, to_right) => {
                d.push_undo();
                d.copy_segment(seg, to_right, ctx);
                self.overview = None;
                // Advance to the next difference, like WinMerge does.
                if d.model.segments.is_empty() {
                    self.current = None;
                    return;
                }
                let row = d.model.seg_rows(seg.min(d.model.segments.len() - 1)).start;
                self.current = d.model.hunk_at_or_after(row);
                if let Some(c) = self.current {
                    let r = d.model.seg_rows(d.model.hunks[c]).start;
                    let first_vis = (self.scroll_y / self.row_h as f64) as usize;
                    let vis_rows = (self.view_h / self.row_h) as usize;
                    if r < first_vis || r > first_vis + vis_rows.saturating_sub(2) {
                        self.scroll_to_row(r, true);
                    }
                }
            }
            Act::CopyAll(to_right) => {
                d.push_undo();
                let (src, dst) = if to_right { (0, 1) } else { (1, 0) };
                let n = d.sides[src].doc.line_count();
                let pieces = d.sides[src].doc.slice(0..n);
                let m = d.sides[dst].doc.line_count();
                d.sides[dst].doc.splice(0..m, pieces);
                d.sides[dst].dirty = true;
                d.notify_hl(dst, 0);
                d.rediff(ctx);
                self.overview = None;
                self.current = None;
            }
            Act::Undo => {
                if d.undo_redo(true) {
                    self.overview = None;
                    self.edit = None;
                }
            }
            Act::Redo => {
                if d.undo_redo(false) {
                    self.overview = None;
                    self.edit = None;
                }
            }
            Act::SaveAll => {
                for side in 0..2 {
                    if self.data.as_ref().unwrap().sides[side].dirty {
                        self.save(side, cx);
                    }
                }
            }
            Act::Save(side) => self.save(side, cx),
            Act::Reload => {
                if self.is_dirty() {
                    self.data.as_mut().unwrap().rediff(ctx);
                    self.toast = Some(("Recompared (unsaved changes kept)".into(), Instant::now()));
                } else {
                    self.start_load(ctx, &cx.settings.diff);
                }
            }
            Act::Swap => {
                self.paths.swap(0, 1);
                self.disk_mtime.swap(0, 1);
                self.changed_on_disk.swap(0, 1);
                self.syntax_override = None;
                d.sides.swap(0, 1);
                d.undo.clear();
                d.redo.clear();
                d.rediff(ctx);
                self.sel = None;
                self.overview = None;
            }
            Act::CopySelection => {
                if let Some(sel) = self.sel {
                    let mut out = String::new();
                    for row in sel.rows() {
                        if row >= d.model.total_rows() {
                            break;
                        }
                        if let Some(l) = line_of(&d.model.row_info(row), sel.side) {
                            out.push_str(&d.sides[sel.side].doc.line_text(l));
                            out.push('\n');
                        }
                    }
                    if !out.is_empty() {
                        ctx.copy_text(out);
                    }
                }
            }
            Act::StartEdit(side, row) => {
                if row < d.model.total_rows()
                    && let Some(line) = line_of(&d.model.row_info(row), side)
                {
                    let text = d.sides[side].doc.line_text(line);
                    self.edit = Some(Edit { side, line, original: text.clone(), text, focus: true });
                }
            }
            Act::InsertLine(side, row) => {
                let info = d.model.row_info(row.min(d.model.total_rows().saturating_sub(1)));
                let at = match line_of(&info, side) {
                    Some(l) => l + 1,
                    None => line_at_or_after(&d.model, row, side),
                };
                d.push_undo();
                d.sides[side].doc.replace_lines(at..at, vec![String::new()]);
                d.after_edit(side, at, ctx);
                if d.rediff_rx.is_none() {
                    self.edit = Some(Edit { side, line: at, text: String::new(), original: String::new(), focus: true });
                }
            }
            Act::DeleteSelection => {
                if let Some(sel) = self.sel {
                    let lines: Vec<usize> = sel
                        .rows()
                        .filter(|&r| r < d.model.total_rows())
                        .filter_map(|r| line_of(&d.model.row_info(r), sel.side))
                        .collect();
                    if let (Some(&a), Some(&b)) = (lines.first(), lines.last()) {
                        d.push_undo();
                        d.sides[sel.side].doc.splice(a..b + 1, Vec::new());
                        d.after_edit(sel.side, a, ctx);
                        self.sel = None;
                    }
                }
            }
            Act::FindNext(forward) => self.find_next(forward),
            Act::ForceReload => self.start_load(ctx, &cx.settings.diff),
        }
    }

    fn find_next(&mut self, forward: bool) {
        let Some(d) = &self.data else { return };
        if self.find.query.is_empty() {
            return;
        }
        let total = d.model.total_rows();
        if total == 0 {
            return;
        }
        let from_row = self.sel.map(|s| s.cursor).unwrap_or((self.scroll_y / self.row_h as f64) as usize).min(total - 1);
        let q = if self.find.case { self.find.query.clone() } else { self.find.query.to_lowercase() };
        let case = self.find.case;
        let matches = |t: &str| if case { t.contains(&q) } else { t.to_lowercase().contains(&q) };
        let mut best: Option<(usize, usize)> = None; // (row, side)
        for side in 0..2 {
            let doc = &d.sides[side].doc;
            let start = line_at_or_after(&d.model, from_row, side);
            let found = if forward {
                let begin = match line_of(&d.model.row_info(from_row), side) {
                    Some(l) => l + 1,
                    None => start,
                };
                find_line(doc, begin..doc.line_count(), true, &matches)
            } else {
                find_line(doc, 0..start.min(doc.line_count()), false, &matches)
            };
            if let Some(l) = found {
                let row = d.model.row_of_line(l, side == 1);
                let better = match best {
                    None => true,
                    Some((b, _)) => if forward { row < b } else { row > b },
                };
                if (forward && row > from_row || !forward && row < from_row || row != from_row) && better {
                    best = Some((row, side));
                }
            }
        }
        match best {
            Some((row, side)) => {
                self.find.msg.clear();
                self.sel = Some(Sel { side, anchor: row, cursor: row });
                self.current = d.model.hunk_containing(row);
                self.scroll_to_row(row, true);
            }
            None => self.find.msg = "No more matches".into(),
        }
    }

    fn save(&mut self, side: usize, cx: &mut ViewCtx) {
        let path = match &self.paths[side] {
            Some(p) => p.clone(),
            None => match rfd::FileDialog::new().set_title("Save as").save_file() {
                Some(p) => p,
                None => return,
            },
        };
        let d = self.data.as_mut().unwrap();
        match save_document(&d.sides[side].doc, &path, &[&d.sides[0].doc.base, &d.sides[1].doc.base]) {
            Ok(()) => {
                // Reload so the document is backed by the new file again.
                if let Ok(src) = TextSource::load(&path) {
                    let doc = Document::new(Arc::new(src));
                    if doc.line_count() == d.sides[side].doc.line_count() {
                        d.sides[side].doc = doc;
                        d.notify_hl(side, usize::MAX);
                    }
                }
                d.sides[side].dirty = false;
                // Every other state in the history now differs from what is on disk.
                for snap in d.undo.iter_mut().chain(d.redo.iter_mut()) {
                    snap.dirty[side] = true;
                }
                self.disk_mtime[side] = fs::metadata(&path).and_then(|m| m.modified()).ok();
                self.changed_on_disk[side] = false;
                self.paths[side] = Some(path.clone());
                self.toast = Some((format!("Saved {}", file_name(&path)), Instant::now()));
                cx.actions.push(super::AppAction::FileSaved(path));
            }
            Err(e) => cx.actions.push(super::AppAction::Toast(format!("Save failed: {e}"))),
        }
    }

    /// Stop warning about the current on-disk versions.
    fn ignore_disk_change(&mut self) {
        for side in 0..2 {
            self.disk_mtime[side] = self.paths[side].as_ref().and_then(|p| fs::metadata(p).and_then(|m| m.modified()).ok());
        }
        self.changed_on_disk = [false; 2];
    }

    /// Save every modified side (used by the app before closing).
    pub fn save_all(&mut self, cx: &mut ViewCtx) {
        for side in 0..2 {
            if self.data.as_ref().is_some_and(|d| d.sides[side].dirty) {
                self.save(side, cx);
            }
        }
    }
}

impl Data {
    fn snapshot(&self) -> Snapshot {
        Snapshot {
            docs: [self.sides[0].doc.clone(), self.sides[1].doc.clone()],
            dirty: [self.sides[0].dirty, self.sides[1].dirty],
            model: self.model.clone(),
        }
    }

    fn push_undo(&mut self) {
        let s = self.snapshot();
        self.undo.push(s);
        if self.undo.len() > MAX_UNDO {
            self.undo.remove(0);
        }
        self.redo.clear();
    }

    fn undo_redo(&mut self, undo: bool) -> bool {
        let snap = if undo { self.undo.pop() } else { self.redo.pop() };
        let Some(snap) = snap else { return false };
        let cur = self.snapshot();
        if undo { self.redo.push(cur) } else { self.undo.push(cur) }
        let [l, r] = snap.docs;
        self.sides[0].doc = l;
        self.sides[1].doc = r;
        self.sides[0].dirty = snap.dirty[0];
        self.sides[1].dirty = snap.dirty[1];
        self.set_model(snap.model);
        self.rediff_rx = None;
        self.notify_hl(0, 0);
        self.notify_hl(1, 0);
        true
    }

    fn set_model(&mut self, m: DiffModel) {
        self.model = m;
        self.model_gen += 1;
        self.inline.clear();
    }

    fn notify_hl(&mut self, side: usize, from_line: usize) {
        let s = &mut self.sides[side];
        s.hl.set_doc(&s.doc, from_line);
    }

    fn after_edit(&mut self, side: usize, line: usize, ctx: &egui::Context) {
        self.sides[side].dirty = true;
        self.inline.clear();
        self.notify_hl(side, line);
        self.rediff(ctx);
    }

    fn copy_segment(&mut self, seg: usize, to_right: bool, ctx: &egui::Context) {
        let Some(s) = self.model.segments.get(seg).copied() else { return };
        let (src, dst) = if to_right { (0, 1) } else { (1, 0) };
        let (sa, sl, da, dl) =
            if to_right { (s.left, s.left_len, s.right, s.right_len) } else { (s.right, s.right_len, s.left, s.left_len) };
        let pieces = self.sides[src].doc.slice(sa..sa + sl);
        self.sides[dst].doc.splice(da..da + dl, pieces);
        self.sides[dst].dirty = true;
        if self.rediff_rx.is_some() {
            // A background diff is outdated now; recompute from scratch.
            self.rediff(ctx);
        } else {
            self.model.apply_copy(seg, to_right);
            self.model_gen += 1;
            self.inline.clear();
        }
        self.notify_hl(dst, da);
    }

    fn rediff(&mut self, ctx: &egui::Context) {
        let total = self.sides[0].doc.line_count() + self.sides[1].doc.line_count();
        self.rediff_token += 1;
        if total <= SYNC_DIFF_LIMIT {
            let m = DiffModel::compute(&self.sides[0].doc, &self.sides[1].doc, &self.opts);
            self.set_model(m);
            self.rediff_rx = None;
            return;
        }
        let (tx, rx) = crossbeam_channel::bounded(1);
        let (l, r, o, tok) = (self.sides[0].doc.clone(), self.sides[1].doc.clone(), self.opts.clone(), self.rediff_token);
        let wake = waker(ctx);
        std::thread::spawn(move || {
            let m = DiffModel::compute(&l, &r, &o);
            tx.send((tok, m)).ok();
            wake();
        });
        self.rediff_rx = Some(rx);
    }
}

fn save_document(doc: &Document, path: &std::path::Path, sources: &[&Arc<TextSource>]) -> std::io::Result<()> {
    // Write through symlinks instead of replacing the link with a regular file.
    let resolved = fs::canonicalize(path).ok();
    let path = resolved.as_deref().unwrap_or(path);
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "file".into());
    let tmp = path.with_file_name(format!(".{name}.dirmerge-tmp"));
    let res = (|| {
        let f = fs::File::create(&tmp)?;
        let mut w = BufWriter::with_capacity(1 << 20, f);
        doc.write_to(&mut w)?;
        w.flush()?;
        w.get_ref().sync_all()?;
        drop(w);
        if let Ok(m) = fs::metadata(path) {
            fs::set_permissions(&tmp, m.permissions()).ok();
        }
        // On Windows a memory-mapped file can't be replaced: take a private copy first.
        if cfg!(windows) {
            for s in sources {
                if s.path.as_ref().and_then(|p| fs::canonicalize(p).ok()).as_deref() == Some(path) {
                    s.detach();
                }
            }
        }
        fs::rename(&tmp, path)
    })();
    if res.is_err() {
        fs::remove_file(&tmp).ok();
    }
    res
}

fn line_of(r: &RowInfo, side: usize) -> Option<usize> {
    if side == 0 { r.left } else { r.right }
}

/// First line on `side` at or after display row `row`.
fn line_at_or_after(m: &DiffModel, row: usize, side: usize) -> usize {
    if m.segments.is_empty() {
        return 0;
    }
    let seg = m.seg_of_row(row);
    let s = m.segments[seg];
    let off = row.saturating_sub(m.seg_rows(seg).start);
    if side == 0 { s.left + off.min(s.left_len) } else { s.right + off.min(s.right_len) }
}

fn find_line(doc: &Document, range: Range<usize>, forward: bool, pred: &dyn Fn(&str) -> bool) -> Option<usize> {
    const CHUNK: usize = 8192;
    if forward {
        let mut a = range.start;
        while a < range.end {
            let b = (a + CHUNK).min(range.end);
            let mut hit = None;
            doc.for_each_line(a..b, |i, t| {
                if hit.is_none() && pred(t) {
                    hit = Some(i);
                }
            });
            if hit.is_some() {
                return hit;
            }
            a = b;
        }
    } else {
        let mut b = range.end;
        while b > range.start {
            let a = b.saturating_sub(CHUNK).max(range.start);
            let mut hit = None;
            doc.for_each_line(a..b, |i, t| {
                if pred(t) {
                    hit = Some(i);
                }
            });
            if hit.is_some() {
                return hit;
            }
            b = a;
        }
    }
    None
}

fn find_all(text: &str, q: &str, case: bool) -> Vec<Range<usize>> {
    let mut out = Vec::new();
    if q.is_empty() {
        return out;
    }
    if case || !q.is_ascii() || !text.is_ascii() {
        if case {
            out.extend(text.match_indices(q).map(|(i, m)| i..i + m.len()));
        } else {
            // Case-insensitive for non-ASCII: compare per char window.
            let ql = q.to_lowercase();
            let idx: Vec<usize> = text.char_indices().map(|(i, _)| i).chain([text.len()]).collect();
            let qn = ql.chars().count();
            let mut i = 0;
            while i + qn < idx.len() {
                let s = &text[idx[i]..idx[i + qn]];
                if s.to_lowercase() == ql {
                    out.push(idx[i]..idx[i + qn]);
                    i += qn;
                } else {
                    i += 1;
                }
            }
        }
    } else {
        let lt = text.to_ascii_lowercase();
        let lq = q.to_ascii_lowercase();
        out.extend(lt.match_indices(&lq).map(|(i, m)| i..i + m.len()));
    }
    out
}

/// Pane geometry, left to right:
/// overview | gutter L | text L | center | gutter R | text R | vscroll
struct Lay {
    overview: Rect,
    gutter: [Rect; 2],
    text: [Rect; 2],
    center: Rect,
    vscroll: Rect,
    hscroll: Option<Rect>,
}

impl Lay {
    fn new(area: Rect, gutter_w: f32, hscroll: bool) -> Lay {
        let bottom = if hscroll { area.bottom() - HB_H } else { area.bottom() };
        let pane_w = ((area.width() - OV_W - CENTER_W - SB_W - 2.0 * gutter_w) / 2.0).max(40.0);
        let mut x = area.left();
        let mut take = |w: f32| {
            let r = Rect::from_min_max(pos2(x, area.top()), pos2(x + w, bottom));
            x += w;
            r
        };
        let overview = take(OV_W);
        let g0 = take(gutter_w);
        let t0 = take(pane_w);
        let center = take(CENTER_W);
        let g1 = take(gutter_w);
        let t1 = take(pane_w);
        let vscroll = Rect::from_min_max(pos2(area.right() - SB_W, area.top()), pos2(area.right(), area.bottom()));
        Lay {
            overview: Rect::from_min_max(overview.min, pos2(overview.right(), area.bottom())),
            gutter: [g0, g1],
            text: [t0, Rect::from_min_max(t1.min, pos2(vscroll.left(), bottom))],
            center,
            vscroll,
            hscroll: hscroll.then(|| Rect::from_min_max(pos2(area.left(), bottom), pos2(area.right(), area.bottom()))),
        }
    }
}

fn hatch(painter: &egui::Painter, rect: Rect, phase: f32, color: Color32) {
    const STEP: f32 = 9.0;
    let st = Stroke::new(1.0, color);
    let t = rect.top() + phase;
    let start = ((rect.left() + t) / STEP).floor() as i64;
    let end = ((rect.right() + rect.bottom() + phase) / STEP).ceil() as i64;
    for k in start..=end {
        let c = k as f32 * STEP;
        // Line x + y' = c, where y' = y + phase.
        let p1 = pos2(c - (rect.top() + phase), rect.top());
        let p2 = pos2(c - (rect.bottom() + phase), rect.bottom());
        painter.line_segment([p1, p2], st);
    }
}

struct JobStyle {
    font: FontId,
    fg: Color32,
    inline_bg: Color32,
    mark_bg: Color32,
    ws: Color32,
    tab_w: usize,
    show_ws: bool,
}

/// Build a layout job: expands tabs, applies syntax colors, inline-diff and
/// search-match backgrounds, and optionally makes whitespace visible.
fn build_job(
    text: &str,
    spans: Option<&[highlight::StyledSpan]>,
    inline: &[Range<usize>],
    marks: &[Range<usize>],
    st: &JobStyle,
) -> LayoutJob {
    let mut job = LayoutJob::default();
    let mut end = text.len().min(MAX_DISPLAY);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    let text_cut = &text[..end];
    let spans = spans.unwrap_or(&[]);
    let (mut si, mut ii, mut mi) = (0usize, 0usize, 0usize);
    let mut run = String::new();
    let mut run_fmt: Option<TextFormat> = None;
    let mut col = 0usize;
    let fmt_at = |pos: usize, si: &mut usize, ii: &mut usize, mi: &mut usize, ws: bool| -> TextFormat {
        while *si < spans.len() && spans[*si].range.end <= pos {
            *si += 1;
        }
        while *ii < inline.len() && inline[*ii].end <= pos {
            *ii += 1;
        }
        while *mi < marks.len() && marks[*mi].end <= pos {
            *mi += 1;
        }
        let span = spans.get(*si).filter(|s| s.range.start <= pos);
        let in_inline = inline.get(*ii).is_some_and(|r| r.start <= pos);
        let in_mark = marks.get(*mi).is_some_and(|r| r.start <= pos);
        let mut f = TextFormat::simple(st.font.clone(), span.map(|s| s.color).unwrap_or(st.fg));
        if ws && st.show_ws {
            f.color = st.ws;
        }
        if let Some(s) = span {
            f.italics = s.italic;
            if s.underline {
                f.underline = Stroke::new(1.0, f.color);
            }
        }
        if in_mark {
            f.background = st.mark_bg;
        } else if in_inline {
            f.background = st.inline_bg;
        }
        f
    };
    for (pos, ch) in text_cut.char_indices() {
        let is_ws = ch == ' ' || ch == '\t';
        let f = fmt_at(pos, &mut si, &mut ii, &mut mi, is_ws);
        if run_fmt.as_ref() != Some(&f) {
            if let Some(rf) = run_fmt.take() {
                job.append(&run, 0.0, rf);
                run.clear();
            }
            run_fmt = Some(f);
        }
        match ch {
            '\t' => {
                let n = st.tab_w - col % st.tab_w;
                if st.show_ws {
                    run.push('»');
                    run.extend(std::iter::repeat_n(' ', n - 1));
                } else {
                    run.extend(std::iter::repeat_n(' ', n));
                }
                col += n;
            }
            ' ' if st.show_ws => {
                run.push('·');
                col += 1;
            }
            c if c.is_control() => {
                run.push(char::from_u32(0x2400 + c as u32).filter(|_| (c as u32) < 32).unwrap_or('�'));
                col += 1;
            }
            c => {
                run.push(c);
                col += 1;
            }
        }
    }
    if let Some(rf) = run_fmt.take() {
        job.append(&run, 0.0, rf);
    }
    if end < text.len() {
        job.append(" …", 0.0, TextFormat::simple(st.font.clone(), st.ws));
    }
    job
}
