//! Folder comparison view: a virtualized tree table of both folders.

use std::collections::{BTreeSet, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use crossbeam_channel::Receiver;
use egui::{Align, Align2, Color32, FontId, Frame, Id, Key, Layout, Margin, Rect, RichText, Sense, Stroke, Ui, pos2, vec2};

use super::{AppAction, ViewCtx, chip, fmt_count, fmt_size, fmt_time, tool_button};
use crate::dircmp::{self, CompareJob, CompareMode, DirTree, Msg, Status};
use crate::theme::Palette;

static NEXT_ID: AtomicU64 = AtomicU64::new(1);
const ROW_H: f32 = 24.0;

enum Pending {
    Copy { items: Vec<u32>, to_right: bool },
    Delete { items: Vec<u32>, left: bool },
}

struct OpResult {
    copy_to_right: Option<bool>,
    delete_left: Option<bool>,
    ok: Vec<u32>,
    errors: Vec<String>,
}

pub struct DirView {
    uid: u64,
    pub left: PathBuf,
    pub right: PathBuf,
    job: Option<CompareJob>,
    tree: Option<DirTree>,
    scanned: usize,
    total_pairs: usize,
    done_pairs: usize,
    elapsed: Option<Duration>,
    error: Option<String>,
    expanded: Vec<bool>,
    restore_expanded: Option<HashSet<PathBuf>>,
    rows: Vec<u32>,
    rows_dirty: bool,
    sel: BTreeSet<u32>,
    cursor: Option<usize>,
    anchor: Option<usize>,
    filter: String,
    confirm: Option<Pending>,
    op: Option<Receiver<OpResult>>,
    toast: Option<(String, Instant)>,
    scroll_to: Option<usize>,
    filters_key: (bool, bool, bool, bool),
    /// Node to select and reveal once rows are rebuilt.
    select_node: Option<u32>,
}

impl DirView {
    pub fn new(left: PathBuf, right: PathBuf, ctx: &egui::Context, cx: &ViewCtx) -> Self {
        let mut v = DirView {
            uid: NEXT_ID.fetch_add(1, Ordering::Relaxed),
            left,
            right,
            job: None,
            tree: None,
            scanned: 0,
            total_pairs: 0,
            done_pairs: 0,
            elapsed: None,
            error: None,
            expanded: Vec::new(),
            restore_expanded: None,
            rows: Vec::new(),
            rows_dirty: true,
            sel: BTreeSet::new(),
            cursor: None,
            anchor: None,
            filter: String::new(),
            confirm: None,
            op: None,
            toast: None,
            scroll_to: None,
            filters_key: (true, true, true, true),
            select_node: None,
        };
        v.start(ctx, cx);
        v
    }

    fn id(&self) -> Id {
        Id::new(("dir-view", self.uid))
    }

    pub fn title(&self) -> String {
        let n = |p: &PathBuf| super::file_name(p);
        let (a, b) = (n(&self.left), n(&self.right));
        if a == b { format!("📁 {a}") } else { format!("📁 {a} ⬌ {b}") }
    }

    pub fn tooltip(&self) -> String {
        format!("{}\n{}", self.left.display(), self.right.display())
    }

    fn start(&mut self, ctx: &egui::Context, cx: &ViewCtx) {
        // A running copy/delete refers to node indices of the current tree.
        if self.op.is_some() {
            self.toast = Some(("Wait for the current operation to finish".into(), Instant::now()));
            return;
        }
        if let Some(t) = &self.tree {
            let set: HashSet<PathBuf> =
                (0..t.nodes.len()).filter(|&i| self.expanded.get(i).copied().unwrap_or(false)).map(|i| t.rel_path(i as u32)).collect();
            self.restore_expanded = Some(set);
        }
        let ctx2 = ctx.clone();
        self.job = Some(dircmp::start(self.left.clone(), self.right.clone(), cx.settings.dir.clone(), move || {
            ctx2.request_repaint()
        }));
        self.scanned = 0;
        self.done_pairs = 0;
        self.total_pairs = 0;
        self.elapsed = None;
        self.error = None;
    }

    fn poll(&mut self) {
        let Some(job) = &self.job else { return };
        let mut finished = false;
        let mut got_results = false;
        while let Ok(msg) = job.rx.try_recv() {
            match msg {
                Msg::Scanning { entries } => self.scanned = entries,
                Msg::Tree(tree, pairs) => {
                    self.total_pairs = pairs;
                    self.expanded = vec![false; tree.nodes.len()];
                    if let Some(set) = self.restore_expanded.take() {
                        for i in 0..tree.nodes.len() {
                            if tree.nodes[i].is_dir && set.contains(&tree.rel_path(i as u32)) {
                                self.expanded[i] = true;
                            }
                        }
                    } else {
                        // Expand the first level by default.
                        for &c in &tree.nodes[0].children {
                            self.expanded[c as usize] = tree.nodes.len() < 3000 && tree.nodes[c as usize].is_dir;
                        }
                    }
                    self.expanded[0] = true;
                    self.sel.clear();
                    self.cursor = None;
                    self.anchor = None;
                    self.tree = Some(tree);
                    self.rows_dirty = true;
                }
                Msg::Results(rs) => {
                    if let Some(t) = &mut self.tree {
                        self.done_pairs += rs.len();
                        for (i, st, err) in rs {
                            let n = &mut t.nodes[i as usize];
                            n.status = st;
                            n.error = err;
                        }
                        got_results = true;
                    }
                }
                Msg::Done(d) => {
                    self.elapsed = Some(d);
                    finished = true;
                }
                Msg::Failed(e) => {
                    self.error = Some(e);
                    finished = true;
                }
            }
        }
        if got_results || finished {
            if let Some(t) = &mut self.tree {
                t.aggregate();
            }
            self.rows_dirty = true;
        }
        if finished {
            self.job = None;
        }
    }

    fn poll_op(&mut self) {
        let Some(rx) = &self.op else { return };
        let Ok(res) = rx.try_recv() else { return };
        self.op = None;
        if let Some(t) = &mut self.tree {
            for &i in &res.ok {
                if let Some(to_right) = res.copy_to_right {
                    t.mark_copied(i, to_right);
                }
                if let Some(left) = res.delete_left {
                    t.mark_deleted(i, left);
                }
            }
            t.aggregate();
        }
        self.rows_dirty = true;
        self.sel.clear();
        let msg = if res.errors.is_empty() {
            format!("{} item(s) done", res.ok.len())
        } else {
            format!("{} done, {} failed: {}", res.ok.len(), res.errors.len(), res.errors[0])
        };
        self.toast = Some((msg, Instant::now()));
    }

    fn rebuild_rows(&mut self, cx: &ViewCtx) {
        let Some(t) = &self.tree else {
            self.rows.clear();
            return;
        };
        let s = &cx.settings;
        let q = self.filter.trim().to_lowercase();
        let n = t.nodes.len();
        let mut pass = vec![false; n];
        for i in (0..n).rev() {
            let node = &t.nodes[i];
            let status_ok = match node.status {
                Status::Identical => s.show_identical,
                Status::Different | Status::Error => s.show_different,
                Status::LeftOnly => s.show_left_only,
                Status::RightOnly => s.show_right_only,
                Status::Pending => true,
            };
            let name_ok = q.is_empty() || node.name.to_lowercase().contains(&q);
            pass[i] = if node.is_dir {
                let child_pass = node.children.iter().any(|&c| pass[c as usize]);
                let one_sided = matches!(node.status, Status::LeftOnly | Status::RightOnly);
                child_pass
                    || (status_ok && if q.is_empty() { node.children.is_empty() || one_sided } else { name_ok })
            } else {
                status_ok && name_ok
            };
        }
        let force_open = !q.is_empty();
        let mut rows = Vec::new();
        let mut stack: Vec<u32> = t.nodes[0].children.iter().rev().copied().collect();
        while let Some(i) = stack.pop() {
            if !pass[i as usize] {
                continue;
            }
            rows.push(i);
            let node = &t.nodes[i as usize];
            if node.is_dir && (force_open || self.expanded[i as usize]) {
                stack.extend(node.children.iter().rev().copied());
            }
        }
        self.rows = rows;
        self.rows_dirty = false;
        if let Some(c) = self.cursor
            && c >= self.rows.len()
        {
            self.cursor = self.rows.len().checked_sub(1);
        }
        if self.anchor.is_some_and(|a| a >= self.rows.len()) {
            self.anchor = self.cursor;
        }
    }

    pub fn ui(&mut self, ui: &mut Ui, cx: &mut ViewCtx) {
        self.poll();
        self.poll_op();
        let key = (cx.settings.show_identical, cx.settings.show_different, cx.settings.show_left_only, cx.settings.show_right_only);
        if key != self.filters_key {
            self.filters_key = key;
            self.rows_dirty = true;
        }
        if self.rows_dirty {
            self.rebuild_rows(cx);
        }
        if let Some(t) = self.select_node.take()
            && let Some(r) = self.rows.iter().position(|&x| x == t)
        {
            self.cursor = Some(r);
            self.anchor = Some(r);
            self.scroll_to = Some(r);
        }
        let pal = cx.pal;
        let ctx = ui.ctx().clone();

        egui::Panel::top(self.id().with("toolbar"))
            .frame(Frame::new().fill(pal.panel_bg).inner_margin(Margin::symmetric(8, 5)))
            .show(ui, |ui| self.toolbar(ui, cx, &ctx));
        egui::Panel::bottom(self.id().with("status"))
            .frame(Frame::new().fill(pal.panel_bg).inner_margin(Margin::symmetric(10, 3)))
            .show(ui, |ui| self.status_bar(ui, &pal));
        egui::CentralPanel::no_frame().show(ui, |ui| {
            if let Some(e) = &self.error {
                ui.vertical_centered(|ui| {
                    ui.add_space(80.0);
                    ui.label(RichText::new(format!("⚠ {e}")).color(pal.error));
                });
                return;
            }
            if self.tree.is_none() {
                ui.vertical_centered(|ui| {
                    ui.add_space(ui.available_height() * 0.38);
                    ui.add(egui::Spinner::new().size(28.0));
                    ui.label(RichText::new(format!("Scanning folders… {} entries", fmt_count(self.scanned))).color(pal.text_dim));
                });
                ctx.request_repaint_after(Duration::from_millis(100));
                return;
            }
            self.table(ui, cx);
        });
        self.keys(ui, cx);
        self.confirm_dialog(&ctx, cx);
        if self.toast.as_ref().is_some_and(|(_, t)| t.elapsed() > Duration::from_secs(5)) {
            self.toast = None;
        }
    }

    fn toolbar(&mut self, ui: &mut Ui, cx: &mut ViewCtx, ctx: &egui::Context) {
        let pal = cx.pal;
        let stats = self.tree.as_ref().map(|t| t.stats()).unwrap_or_default();
        // File operations are only allowed on a settled tree.
        let has_sel = !self.sel.is_empty() && self.job.is_none();
        let mut restart = false;
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 2.0;
            if tool_button(ui, "🔄", "", "Refresh (F5)", true).clicked() {
                restart = true;
            }
            ui.separator();
            if tool_button(ui, "⊞", "", "Expand all", self.tree.is_some()).clicked() {
                self.expanded.iter_mut().for_each(|e| *e = true);
                self.rows_dirty = true;
            }
            if tool_button(ui, "⊟", "", "Collapse all", self.tree.is_some()).clicked() {
                self.expanded.iter_mut().skip(1).for_each(|e| *e = false);
                self.rows_dirty = true;
            }
            if tool_button(ui, "⏶", "", "Previous difference (Alt+⏶)", self.tree.is_some()).clicked() {
                self.jump_diff(false);
            }
            if tool_button(ui, "⏷", "", "Next difference (Alt+⏷)", self.tree.is_some()).clicked() {
                self.jump_diff(true);
            }
            ui.separator();
            if tool_button(ui, "➡", "Copy right", "Copy selected items to the right folder", has_sel && self.op.is_none()).clicked() {
                self.ask_copy(true);
            }
            if tool_button(ui, "⬅", "Copy left", "Copy selected items to the left folder", has_sel && self.op.is_none()).clicked() {
                self.ask_copy(false);
            }
            ui.add_enabled_ui(has_sel && self.op.is_none(), |ui| {
                ui.menu_button("🗑", |ui| {
                    if ui.button("Move left items to Trash").clicked() {
                        self.ask_delete(true);
                    }
                    if ui.button("Move right items to Trash").clicked() {
                        self.ask_delete(false);
                    }
                })
                .response
                .on_hover_text("Delete selected items");
            });
            ui.separator();
            let s = &mut *cx.settings;
            chip(ui, &mut s.show_identical, &format!("= {}", fmt_count(stats.identical)), pal.identical, &pal).on_hover_text("Identical");
            chip(ui, &mut s.show_different, &format!("≠ {}", fmt_count(stats.different + stats.errors)), pal.different, &pal)
                .on_hover_text("Different");
            chip(ui, &mut s.show_left_only, &format!("⏴ {}", fmt_count(stats.left_only)), pal.left_only, &pal)
                .on_hover_text("Only in left folder");
            chip(ui, &mut s.show_right_only, &format!("⏵ {}", fmt_count(stats.right_only)), pal.right_only, &pal)
                .on_hover_text("Only in right folder");

            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                ui.menu_button("⚙ Compare", |ui| {
                    ui.set_min_width(300.0);
                    ui.label(RichText::new("Compare files by").strong());
                    for m in CompareMode::ALL {
                        if ui.radio_value(&mut s.dir.mode, m, m.label()).changed() {
                            restart = true;
                        }
                    }
                    ui.separator();
                    ui.label(RichText::new("Exclude (comma-separated globs)").strong());
                    let r = ui.add(egui::TextEdit::singleline(&mut s.dir.exclude).desired_width(280.0));
                    if r.lost_focus() {
                        restart = true;
                    }
                });
                let r = ui.add(egui::TextEdit::singleline(&mut self.filter).hint_text("🔍 Filter names").desired_width(180.0));
                if r.changed() {
                    self.rows_dirty = true;
                }
            });
        });
        if restart {
            self.start(ctx, cx);
        }
        // Root paths.
        ui.add_space(2.0);
        ui.horizontal(|ui| {
            let w = (ui.available_width() - 16.0) / 2.0;
            for (p, col, icon) in [(&self.left, pal.left_only, "■"), (&self.right, pal.right_only, "■")] {
                ui.allocate_ui(vec2(w, 20.0), |ui| {
                    ui.horizontal(|ui| {
                        ui.label(RichText::new(icon).color(col));
                        ui.add(egui::Label::new(RichText::new(p.display().to_string()).color(pal.text_dim)).truncate());
                    });
                });
            }
        });
    }

    fn status_bar(&mut self, ui: &mut Ui, pal: &Palette) {
        ui.horizontal(|ui| {
            if self.job.is_some() {
                ui.add(egui::Spinner::new().size(12.0));
                if self.tree.is_none() {
                    ui.label(RichText::new(format!("Scanning… {} entries", fmt_count(self.scanned))).color(pal.text_dim));
                } else {
                    let frac = if self.total_pairs > 0 { self.done_pairs as f32 / self.total_pairs as f32 } else { 1.0 };
                    ui.add(egui::ProgressBar::new(frac).desired_width(160.0).desired_height(10.0));
                    ui.label(
                        RichText::new(format!("Comparing {} / {} files", fmt_count(self.done_pairs), fmt_count(self.total_pairs)))
                            .color(pal.text_dim),
                    );
                }
            } else if let Some(t) = &self.tree {
                let s = t.stats();
                let differs = s.different + s.left_only + s.right_only + s.errors;
                if differs == 0 {
                    ui.label(RichText::new("✔ Folders are identical").color(pal.ins_mark));
                } else {
                    ui.label(RichText::new(format!("{} differences", fmt_count(differs))).color(pal.different));
                }
                if let Some(e) = self.elapsed {
                    ui.label(
                        RichText::new(format!("·  {} files in {:.2}s", fmt_count(t.nodes.iter().filter(|n| !n.is_dir).count()), e.as_secs_f64()))
                            .color(pal.text_faint),
                    );
                }
            }
            if self.op.is_some() {
                ui.add(egui::Spinner::new().size(12.0));
                ui.label(RichText::new("Working…").color(pal.text_dim));
            }
            if let Some((m, _)) = &self.toast {
                ui.label(RichText::new(format!("·  {m}")).color(pal.accent));
            }
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if !self.sel.is_empty() {
                    ui.label(RichText::new(format!("{} selected", self.sel.len())).color(pal.text_dim));
                }
            });
        });
    }

    fn table(&mut self, ui: &mut Ui, cx: &mut ViewCtx) {
        let pal = cx.pal;
        let full = ui.available_rect_before_wrap();
        ui.painter().rect_filled(full, 0.0, pal.editor_bg);
        let w = full.width();
        let show_dates = w > 900.0;
        let date_w = if show_dates { 128.0 } else { 0.0 };
        let size_w = 78.0;
        let status_w = 150.0;
        let name_w = (w - status_w - 2.0 * (size_w + date_w) - 24.0).max(160.0);
        let cols = Cols { name: name_w, status: status_w, size: size_w, date: date_w };

        // Header.
        let header = Rect::from_min_size(full.min, vec2(w, 26.0));
        let hp = ui.painter_at(header);
        hp.rect_filled(header, 0.0, pal.panel_bg);
        hp.line_segment([header.left_bottom(), header.right_bottom()], Stroke::new(1.0, pal.border));
        let hf = FontId::proportional(12.0);
        let mut x = header.left() + 10.0;
        let titles: [(&str, f32, Color32); 6] = [
            ("Name", cols.name, pal.text_dim),
            ("Comparison", cols.status, pal.text_dim),
            ("Left size", cols.size, pal.left_only),
            ("Left modified", cols.date, pal.left_only),
            ("Right size", cols.size, pal.right_only),
            ("Right modified", cols.date, pal.right_only),
        ];
        for (t, cw, col) in titles {
            if cw > 0.0 {
                hp.text(pos2(x, header.center().y), Align2::LEFT_CENTER, t, hf.clone(), col);
            }
            x += cw;
        }

        let body = Rect::from_min_max(pos2(full.left(), header.bottom()), full.max);
        let mut ui_body = ui.new_child(egui::UiBuilder::new().max_rect(body));
        let mut sa = egui::ScrollArea::vertical().auto_shrink(false).id_salt(self.id().with("scroll"));
        if let Some(r) = self.scroll_to.take() {
            let off = ui_body.ctx().data(|d| d.get_temp::<f32>(self.id().with("off"))).unwrap_or(0.0);
            let top = r as f32 * ROW_H;
            let vh = body.height();
            if top < off || top + ROW_H > off + vh {
                sa = sa.vertical_scroll_offset((top - vh / 2.0).max(0.0));
            }
        }
        if self.rows.is_empty() {
            ui_body.vertical_centered(|ui| {
                ui.add_space(60.0);
                let msg = if self.job.is_some() { "Comparing…" } else { "Nothing to show with the current filters" };
                ui.label(RichText::new(msg).color(pal.text_faint));
            });
            return;
        }
        let out = sa.show_rows(&mut ui_body, ROW_H, self.rows.len(), |ui, range| {
            ui.spacing_mut().item_spacing.y = 0.0;
            for ri in range {
                self.row_ui(ui, ri, &cols, cx);
            }
        });
        ui.ctx().data_mut(|d| d.insert_temp(self.id().with("off"), out.state.offset.y));
    }

    fn row_ui(&mut self, ui: &mut Ui, ri: usize, cols: &Cols, cx: &mut ViewCtx) {
        let pal = cx.pal;
        let idx = self.rows[ri];
        let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), ROW_H), Sense::click());
        let t = self.tree.as_ref().unwrap();
        let n = &t.nodes[idx as usize];
        let p = ui.painter_at(rect);
        let selected = self.sel.contains(&idx);
        let (status_text, status_col, tint) = status_style(n, &pal);
        if selected {
            p.rect_filled(rect, 0.0, pal.selection);
        } else if resp.hovered() {
            p.rect_filled(rect, 0.0, pal.hover);
        } else if let Some(tint) = tint {
            p.rect_filled(rect, 0.0, tint);
        }
        if self.cursor == Some(ri) {
            p.rect_stroke(rect.shrink(0.5), 0.0, Stroke::new(1.0, pal.accent.gamma_multiply(0.6)), egui::StrokeKind::Inside);
        }
        let font = FontId::proportional(13.0);
        let cy = rect.center().y;
        let mut x = rect.left() + 10.0 + (n.depth.saturating_sub(1)) as f32 * 16.0;
        // Indent guides.
        for d in 1..n.depth {
            let gx = rect.left() + 10.0 + (d - 1) as f32 * 16.0 + 6.0;
            p.line_segment([pos2(gx, rect.top()), pos2(gx, rect.bottom())], Stroke::new(1.0, pal.border.gamma_multiply(0.6)));
        }
        let mut toggle = false;
        if n.is_dir {
            let open = self.expanded[idx as usize] || !self.filter.trim().is_empty();
            let ar = Rect::from_center_size(pos2(x + 6.0, cy), vec2(14.0, 14.0));
            let aresp = ui.interact(ar, self.id().with(("exp", idx)), Sense::click());
            p.text(ar.center(), Align2::CENTER_CENTER, if open { "⏷" } else { "⏵" }, FontId::proportional(11.0), if aresp.hovered() { pal.text } else { pal.text_dim });
            toggle = aresp.clicked();
        }
        x += 16.0;
        let icon_col = if n.is_dir { pal.accent } else { pal.text_dim };
        p.text(pos2(x, cy), Align2::LEFT_CENTER, if n.is_dir { "📁" } else { "📄" }, FontId::proportional(13.0), icon_col);
        x += 20.0;
        let name_col = match n.status {
            Status::LeftOnly => pal.left_only,
            Status::RightOnly => pal.right_only,
            Status::Different | Status::Error => pal.text,
            _ => if n.is_dir { pal.text } else { pal.text.gamma_multiply(0.85) },
        };
        let name_clip = Rect::from_x_y_ranges(rect.left()..=rect.left() + cols.name - 6.0, rect.y_range());
        p.with_clip_rect(name_clip).text(pos2(x, cy), Align2::LEFT_CENTER, &n.name, font.clone(), name_col);

        let mut cx_ = rect.left() + 10.0 + cols.name;
        p.text(pos2(cx_, cy), Align2::LEFT_CENTER, status_text, FontId::proportional(12.5), status_col);
        cx_ += cols.status;
        let small = FontId::proportional(12.0);
        for side in [n.left, n.right] {
            if let Some(m) = side {
                if !m.is_dir {
                    p.text(pos2(cx_ + cols.size - 12.0, cy), Align2::RIGHT_CENTER, fmt_size(m.size), small.clone(), pal.text_dim);
                }
                if cols.date > 0.0 {
                    p.text(pos2(cx_ + cols.size, cy), Align2::LEFT_CENTER, fmt_time(m.mtime), small.clone(), pal.text_faint);
                }
            } else {
                p.text(pos2(cx_ + cols.size - 12.0, cy), Align2::RIGHT_CENTER, "—", small.clone(), pal.text_faint);
            }
            cx_ += cols.size + cols.date;
        }
        let resp = if let Some(e) = &n.error { resp.on_hover_text(e.clone()) } else { resp };

        if toggle {
            self.expanded[idx as usize] = !self.expanded[idx as usize];
            self.rows_dirty = true;
            return;
        }
        if resp.clicked() {
            self.click_row(ri, ui.input(|i| i.modifiers));
        }
        if resp.double_clicked() {
            self.activate(ri, cx);
        }
        if resp.secondary_clicked() && !self.sel.contains(&idx) {
            self.click_row(ri, egui::Modifiers::NONE);
        }
        resp.context_menu(|ui| {
            ui.set_min_width(220.0);
            if ui.button("Compare").clicked() {
                self.activate(ri, cx);
                ui.close();
            }
            ui.separator();
            if ui.button("Copy to right  ➡").clicked() {
                self.ask_copy(true);
                ui.close();
            }
            if ui.button("Copy to left  ⬅").clicked() {
                self.ask_copy(false);
                ui.close();
            }
            ui.separator();
            if ui.button("Move left item to Trash").clicked() {
                self.ask_delete(true);
                ui.close();
            }
            if ui.button("Move right item to Trash").clicked() {
                self.ask_delete(false);
                ui.close();
            }
            ui.separator();
            if ui.button("Copy left path").clicked() {
                let p = self.tree.as_ref().unwrap().left_path(idx);
                ui.ctx().copy_text(p.display().to_string());
                ui.close();
            }
            if ui.button("Copy right path").clicked() {
                let p = self.tree.as_ref().unwrap().right_path(idx);
                ui.ctx().copy_text(p.display().to_string());
                ui.close();
            }
        });
    }

    /// Re-compare a single file after it was saved from a file view.
    pub fn refresh_path(&mut self, path: &std::path::Path, mode: CompareMode) {
        let Some(t) = &mut self.tree else { return };
        let rel = path.strip_prefix(&t.left_root).or_else(|_| path.strip_prefix(&t.right_root));
        let Some(idx) = rel.ok().and_then(|r| t.find(r)) else { return };
        let (lp, rp) = (t.left_path(idx), t.right_path(idx));
        let (lm, rm) = (dircmp::meta_of(&lp).ok(), dircmp::meta_of(&rp).ok());
        let n = &mut t.nodes[idx as usize];
        if n.is_dir {
            return;
        }
        n.left = lm;
        n.right = rm;
        n.error = None;
        n.status = match (&lm, &rm) {
            (Some(a), Some(b)) => match dircmp::compare_files(&lp, &rp, a, b, mode, &std::sync::atomic::AtomicBool::new(false)) {
                Ok(true) => Status::Identical,
                Ok(false) => Status::Different,
                Err(e) => {
                    n.error = Some(e.to_string());
                    Status::Error
                }
            },
            (Some(_), None) => Status::LeftOnly,
            (None, Some(_)) => Status::RightOnly,
            (None, None) => return,
        };
        t.aggregate();
        self.rows_dirty = true;
    }

    fn click_row(&mut self, ri: usize, m: egui::Modifiers) {
        let idx = self.rows[ri];
        if m.shift
            && let Some(a) = self.anchor
        {
            self.sel.clear();
            let (lo, hi) = (a.min(ri), a.max(ri));
            self.sel.extend(self.rows[lo..=hi].iter().copied());
        } else if m.command {
            if !self.sel.remove(&idx) {
                self.sel.insert(idx);
            }
            self.anchor = Some(ri);
        } else {
            self.sel.clear();
            self.sel.insert(idx);
            self.anchor = Some(ri);
        }
        self.cursor = Some(ri);
    }

    fn activate(&mut self, ri: usize, cx: &mut ViewCtx) {
        let Some(t) = &self.tree else { return };
        let idx = self.rows[ri];
        let n = &t.nodes[idx as usize];
        if n.is_dir {
            self.expanded[idx as usize] = !self.expanded[idx as usize];
            self.rows_dirty = true;
            return;
        }
        if n.left.is_some_and(|m| m.is_dir) || n.right.is_some_and(|m| m.is_dir) {
            return;
        }
        cx.actions.push(AppAction::OpenFiles {
            left: n.left.map(|_| t.left_path(idx)),
            right: n.right.map(|_| t.right_path(idx)),
        });
    }

    fn jump_diff(&mut self, forward: bool) {
        let Some(t) = &self.tree else { return };
        if self.rows.is_empty() {
            return;
        }
        let is_diff = |i: u32| {
            let n = &t.nodes[i as usize];
            !n.is_dir && !matches!(n.status, Status::Identical | Status::Pending)
        };
        // Search the full tree order (not just visible rows) so collapsed folders open.
        let mut order = Vec::new();
        let mut stack: Vec<u32> = t.nodes[0].children.iter().rev().copied().collect();
        while let Some(i) = stack.pop() {
            order.push(i);
            stack.extend(t.nodes[i as usize].children.iter().rev().copied());
        }
        let cur = self.cursor.map(|c| self.rows[c]);
        let pos = cur.and_then(|c| order.iter().position(|&x| x == c));
        let found = if forward {
            let start = pos.map(|p| p + 1).unwrap_or(0);
            order[start..].iter().copied().find(|&i| is_diff(i))
        } else {
            let end = pos.unwrap_or(order.len());
            order[..end].iter().rev().copied().find(|&i| is_diff(i))
        };
        let Some(target) = found else {
            self.toast = Some(("No more differences".into(), Instant::now()));
            return;
        };
        let mut p = t.nodes[target as usize].parent;
        while let Some(pi) = p {
            self.expanded[pi as usize] = true;
            p = t.nodes[pi as usize].parent;
        }
        self.rows_dirty = true;
        self.pending_select(target);
    }

    fn pending_select(&mut self, target: u32) {
        self.rows_dirty = true;
        self.sel.clear();
        self.sel.insert(target);
        self.select_node = Some(target);
    }
}

struct Cols {
    name: f32,
    status: f32,
    size: f32,
    date: f32,
}

fn status_style(n: &dircmp::Node, pal: &Palette) -> (String, Color32, Option<Color32>) {
    let tint = |c: Color32| Some(c.gamma_multiply(if pal.dark { 0.07 } else { 0.06 }));
    match n.status {
        Status::Pending => ("Comparing…".into(), pal.text_faint, None),
        Status::Identical => ("Identical".into(), pal.identical, None),
        Status::Different if n.is_dir => (format!("{} different", n.diff_count), pal.different, tint(pal.different)),
        Status::Different => (if n.error.is_some() { "Type differs".into() } else { "Different".into() }, pal.different, tint(pal.different)),
        Status::LeftOnly => (if n.is_dir { "Left only (folder)".into() } else { "Left only".into() }, pal.left_only, tint(pal.left_only)),
        Status::RightOnly => (if n.is_dir { "Right only (folder)".into() } else { "Right only".into() }, pal.right_only, tint(pal.right_only)),
        Status::Error => ("Error".into(), pal.error, tint(pal.error)),
    }
}

impl DirView {
    fn keys(&mut self, ui: &mut Ui, cx: &mut ViewCtx) {
        if ui.ctx().egui_wants_keyboard_input() || self.confirm.is_some() {
            return;
        }
        let n = self.rows.len();
        let (alt_down, alt_up, down, up, left, right, enter, f5, shift, pgdn, pgup) = ui.input_mut(|i| {
            // Alt variants first: consume_key(NONE, ..) also matches Alt+key.
            (
                i.consume_key(egui::Modifiers::ALT, Key::ArrowDown),
                i.consume_key(egui::Modifiers::ALT, Key::ArrowUp),
                i.consume_key(egui::Modifiers::NONE, Key::ArrowDown) || i.consume_key(egui::Modifiers::SHIFT, Key::ArrowDown),
                i.consume_key(egui::Modifiers::NONE, Key::ArrowUp) || i.consume_key(egui::Modifiers::SHIFT, Key::ArrowUp),
                i.consume_key(egui::Modifiers::NONE, Key::ArrowLeft),
                i.consume_key(egui::Modifiers::NONE, Key::ArrowRight),
                i.consume_key(egui::Modifiers::NONE, Key::Enter),
                i.consume_key(egui::Modifiers::NONE, Key::F5),
                i.modifiers.shift,
                i.consume_key(egui::Modifiers::NONE, Key::PageDown),
                i.consume_key(egui::Modifiers::NONE, Key::PageUp),
            )
        });
        if f5 {
            let ctx = ui.ctx().clone();
            self.start(&ctx, cx);
            return;
        }
        if alt_down {
            self.jump_diff(true);
        }
        if alt_up {
            self.jump_diff(false);
        }
        if n == 0 {
            return;
        }
        let page = 20;
        let mv = |c: Option<usize>, d: isize| -> usize {
            match c {
                None => 0,
                Some(c) => (c as isize + d).clamp(0, n as isize - 1) as usize,
            }
        };
        let mut new_cursor = None;
        if down {
            new_cursor = Some(mv(self.cursor, 1));
        }
        if up {
            new_cursor = Some(mv(self.cursor, -1));
        }
        if pgdn {
            new_cursor = Some(mv(self.cursor, page));
        }
        if pgup {
            new_cursor = Some(mv(self.cursor, -page));
        }
        if let Some(c) = new_cursor {
            let m = if shift { egui::Modifiers::SHIFT } else { egui::Modifiers::NONE };
            self.click_row(c, m);
            self.scroll_to = Some(c);
        }
        if let Some(c) = self.cursor {
            let idx = self.rows[c] as usize;
            let t = self.tree.as_ref().unwrap();
            if right && t.nodes[idx].is_dir && !self.expanded[idx] {
                self.expanded[idx] = true;
                self.rows_dirty = true;
            }
            if left {
                if t.nodes[idx].is_dir && self.expanded[idx] {
                    self.expanded[idx] = false;
                    self.rows_dirty = true;
                } else if let Some(p) = t.nodes[idx].parent.filter(|&p| p != 0)
                    && let Some(pr) = self.rows.iter().position(|&r| r == p) {
                        self.click_row(pr, egui::Modifiers::NONE);
                        self.scroll_to = Some(pr);
                    }
            }
            if enter {
                self.activate(c, cx);
            }
        }
    }

    fn selected_items(&self) -> Vec<u32> {
        // Skip items whose ancestor is selected too (the ancestor covers them).
        let t = self.tree.as_ref().unwrap();
        self.sel
            .iter()
            .copied()
            .filter(|&i| {
                let mut p = t.nodes[i as usize].parent;
                while let Some(pi) = p {
                    if self.sel.contains(&pi) {
                        return false;
                    }
                    p = t.nodes[pi as usize].parent;
                }
                true
            })
            .collect()
    }

    fn ask_copy(&mut self, to_right: bool) {
        if self.job.is_some() || self.op.is_some() {
            return;
        }
        let t = self.tree.as_ref().unwrap();
        let items: Vec<u32> = self
            .selected_items()
            .into_iter()
            .filter(|&i| {
                let n = &t.nodes[i as usize];
                if to_right { n.left.is_some() } else { n.right.is_some() }
            })
            .collect();
        if !items.is_empty() {
            self.confirm = Some(Pending::Copy { items, to_right });
        }
    }

    fn ask_delete(&mut self, left: bool) {
        if self.job.is_some() || self.op.is_some() {
            return;
        }
        let t = self.tree.as_ref().unwrap();
        let items: Vec<u32> = self
            .selected_items()
            .into_iter()
            .filter(|&i| {
                let n = &t.nodes[i as usize];
                if left { n.left.is_some() } else { n.right.is_some() }
            })
            .collect();
        if !items.is_empty() {
            self.confirm = Some(Pending::Delete { items, left });
        }
    }

    fn confirm_dialog(&mut self, ctx: &egui::Context, cx: &mut ViewCtx) {
        let Some(p) = &self.confirm else { return };
        let pal = cx.pal;
        let t = self.tree.as_ref().unwrap();
        let (title, body, items) = match p {
            Pending::Copy { items, to_right } => (
                format!("Copy {} item(s) to the {}?", items.len(), if *to_right { "right" } else { "left" }),
                "Existing files on the destination side will be overwritten.".to_string(),
                items.clone(),
            ),
            Pending::Delete { items, left } => (
                format!("Move {} {} item(s) to the Trash?", items.len(), if *left { "left" } else { "right" }),
                "You can restore them from the system Trash / Recycle Bin.".to_string(),
                items.clone(),
            ),
        };
        let mut decision = None;
        egui::Modal::new(self.id().with("confirm")).show(ctx, |ui| {
            ui.set_width(440.0);
            ui.heading(title);
            ui.add_space(4.0);
            ui.label(RichText::new(body).color(pal.text_dim));
            ui.add_space(6.0);
            egui::ScrollArea::vertical().max_height(160.0).show(ui, |ui| {
                for &i in items.iter().take(50) {
                    ui.label(RichText::new(t.rel_path(i).display().to_string()).monospace().size(12.0));
                }
                if items.len() > 50 {
                    ui.label(format!("… and {} more", items.len() - 50));
                }
            });
            ui.add_space(10.0);
            ui.horizontal(|ui| {
                if ui.add(egui::Button::new(RichText::new("  OK  ").color(Color32::WHITE)).fill(pal.accent)).clicked()
                    || ui.input(|i| i.key_pressed(Key::Enter))
                {
                    decision = Some(true);
                }
                if ui.button("Cancel").clicked() || ui.input(|i| i.key_pressed(Key::Escape)) {
                    decision = Some(false);
                }
            });
        });
        match decision {
            Some(true) => {
                let p = self.confirm.take().unwrap();
                self.run_op(p, ctx);
            }
            Some(false) => self.confirm = None,
            None => {}
        }
    }

    fn run_op(&mut self, p: Pending, ctx: &egui::Context) {
        let t = self.tree.as_ref().unwrap();
        let (tx, rx) = crossbeam_channel::bounded(1);
        let ctx = ctx.clone();
        match p {
            Pending::Copy { items, to_right } => {
                let jobs: Vec<(u32, PathBuf, PathBuf)> = items
                    .iter()
                    .map(|&i| {
                        let (l, r) = (t.left_path(i), t.right_path(i));
                        if to_right { (i, l, r) } else { (i, r, l) }
                    })
                    .collect();
                std::thread::spawn(move || {
                    let mut res = OpResult { copy_to_right: Some(to_right), delete_left: None, ok: vec![], errors: vec![] };
                    for (i, src, dst) in jobs {
                        match dircmp::copy_item(&src, &dst) {
                            Ok(()) => res.ok.push(i),
                            Err(e) => res.errors.push(format!("{}: {e}", src.display())),
                        }
                    }
                    tx.send(res).ok();
                    ctx.request_repaint();
                });
            }
            Pending::Delete { items, left } => {
                let jobs: Vec<(u32, PathBuf)> =
                    items.iter().map(|&i| (i, if left { t.left_path(i) } else { t.right_path(i) })).collect();
                std::thread::spawn(move || {
                    let mut res = OpResult { copy_to_right: None, delete_left: Some(left), ok: vec![], errors: vec![] };
                    for (i, path) in jobs {
                        match trash::delete(&path) {
                            Ok(()) => res.ok.push(i),
                            Err(e) => res.errors.push(format!("{}: {e}", path.display())),
                        }
                    }
                    tx.send(res).ok();
                    ctx.request_repaint();
                });
            }
        }
        self.op = Some(rx);
    }
}
