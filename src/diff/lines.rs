//! Line-level diff between two documents, represented compactly as a list of
//! segments. Every segment maps to a run of aligned display rows; changed
//! segments are padded with filler rows on the shorter side.

use std::collections::HashMap;
use std::hash::{BuildHasherDefault, Hasher};

use imara_diff::{Algorithm, Diff, IndentHeuristic, IndentLevel, Token};
use serde::{Deserialize, Serialize};

use super::align;
use crate::text::Document;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum WhitespaceMode {
    #[default]
    Compare,
    IgnoreChanges,
    IgnoreAll,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct DiffOptions {
    pub whitespace: WhitespaceMode,
    pub ignore_case: bool,
    pub ignore_blank_lines: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SegKind {
    Equal,
    Change,
    /// A difference hidden by the options (e.g. only blank lines).
    Ignored,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Segment {
    pub kind: SegKind,
    pub left: usize,
    pub left_len: usize,
    pub right: usize,
    pub right_len: usize,
    /// Index into `DiffModel::aligns`, or `NO_ALIGN` for top-to-bottom pairing.
    pub align: u32,
}

pub const NO_ALIGN: u32 = u32::MAX;

impl Segment {
    fn new(kind: SegKind, left: usize, left_len: usize, right: usize, right_len: usize) -> Self {
        Segment { kind, left, left_len, right, right_len, align: NO_ALIGN }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RowInfo {
    pub seg: usize,
    pub kind: SegKind,
    pub left: Option<usize>,
    pub right: Option<usize>,
}

#[derive(Clone, Default)]
pub struct DiffModel {
    pub segments: Vec<Segment>,
    /// `row_start[i]` = first display row of segment `i`; last = total rows.
    row_start: Vec<usize>,
    /// Indices of `Change` segments, in order.
    pub hunks: Vec<usize>,
    /// Row layouts of changed blocks whose similar lines were aligned.
    aligns: Vec<Vec<(u32, u32)>>,
}

/// Hashes are already uniformly distributed; skip rehashing them.
#[derive(Default)]
struct IdentityHasher(u64);
impl Hasher for IdentityHasher {
    fn finish(&self) -> u64 {
        self.0
    }
    fn write(&mut self, _: &[u8]) {
        unreachable!()
    }
    fn write_u64(&mut self, n: u64) {
        self.0 = n;
    }
}

pub fn normalize_line<'a>(s: &'a str, opts: &DiffOptions, buf: &'a mut String) -> &'a str {
    if opts.whitespace == WhitespaceMode::Compare && !opts.ignore_case {
        return s;
    }
    buf.clear();
    match opts.whitespace {
        WhitespaceMode::Compare => buf.push_str(s),
        WhitespaceMode::IgnoreAll => buf.extend(s.chars().filter(|c| !c.is_whitespace())),
        WhitespaceMode::IgnoreChanges => {
            let mut pending_space = false;
            for c in s.trim().chars() {
                if c.is_whitespace() {
                    pending_space = true;
                } else {
                    if pending_space {
                        buf.push(' ');
                        pending_space = false;
                    }
                    buf.push(c);
                }
            }
        }
    }
    if opts.ignore_case {
        if buf.is_ascii() {
            buf.make_ascii_lowercase();
        } else {
            *buf = buf.to_lowercase();
        }
    }
    buf
}

/// Per-line hash plus the data the slider heuristic and blank-line filter need.
struct LineKeys {
    hashes: Vec<u64>,
    /// Indent level (low 8 bits) and "blank" flag (bit 8).
    meta: Vec<u16>,
}

fn line_keys(doc: &Document, opts: &DiffOptions) -> LineKeys {
    let n = doc.line_count();
    let mut keys = LineKeys { hashes: Vec::with_capacity(n), meta: Vec::with_capacity(n) };
    let mut buf = String::new();
    doc.for_each_line(0..n, |_, text| {
        let norm = normalize_line(text, opts, &mut buf);
        keys.hashes.push(xxhash_rust::xxh3::xxh3_64(norm.as_bytes()));
        let mut indent = 0u16;
        let mut blank = true;
        for b in text.bytes() {
            match b {
                b' ' => indent += 1,
                b'\t' => indent += 4 - indent % 4,
                b'\r' | b'\n' => {}
                _ => {
                    blank = false;
                    break;
                }
            }
        }
        keys.meta.push(indent.min(200) | ((blank as u16) << 8));
    });
    keys
}

impl DiffModel {
    pub fn compute(left: &Document, right: &Document, opts: &DiffOptions) -> DiffModel {
        // Hash both sides in parallel, then intern sequentially.
        let (lk, rk) = rayon::join(|| line_keys(left, opts), || line_keys(right, opts));
        let mut map: HashMap<u64, u32, BuildHasherDefault<IdentityHasher>> = HashMap::default();
        map.reserve(lk.hashes.len() + rk.hashes.len());
        let mut indents: Vec<IndentLevel> = Vec::new();
        let mut blank: Vec<bool> = Vec::new();
        let mut intern = |keys: &LineKeys| -> Vec<Token> {
            keys.hashes
                .iter()
                .zip(&keys.meta)
                .map(|(&h, &m)| {
                    let next = map.len() as u32;
                    let id = *map.entry(h).or_insert(next);
                    if id == next {
                        let is_blank = m >> 8 == 1;
                        // Rebuild the indent from its width; a blank line has no terminator char.
                        let width = if is_blank { 0 } else { (m & 0xff) as usize };
                        let line = std::iter::repeat_n(b' ', width).chain((!is_blank).then_some(b'x'));
                        indents.push(IndentLevel::for_ascii_line(line, 4));
                        blank.push(is_blank);
                    }
                    Token(id)
                })
                .collect()
        };
        let before = intern(&lk);
        let after = intern(&rk);
        let num_tokens = map.len() as u32;
        let mut diff = Diff::default();
        diff.compute_with(Algorithm::Histogram, &before, &after, num_tokens);
        let indents = &indents;
        diff.postprocess_with(&before, &after, IndentHeuristic::new(|t: Token| indents[t.0 as usize]));

        let mut segments = Vec::new();
        let (mut l, mut r) = (0usize, 0usize);
        for h in diff.hunks() {
            let (hs, he) = (h.before.start as usize, h.before.end as usize);
            let (as_, ae) = (h.after.start as usize, h.after.end as usize);
            if hs > l {
                segments.push(Segment::new(SegKind::Equal, l, hs - l, r, as_ - r));
            }
            let all_blank = opts.ignore_blank_lines
                && before[hs..he].iter().chain(&after[as_..ae]).all(|t| blank[t.0 as usize]);
            let kind = if all_blank { SegKind::Ignored } else { SegKind::Change };
            segments.push(Segment::new(kind, hs, he - hs, as_, ae - as_));
            l = he;
            r = ae;
        }
        if l < before.len() {
            segments.push(Segment::new(SegKind::Equal, l, before.len() - l, r, after.len() - r));
        }
        let mut aligns = Vec::new();
        for s in segments.iter_mut().filter(|s| s.kind == SegKind::Change) {
            if s.left_len == 0 || s.right_len == 0 || s.left_len * s.right_len > align::MAX_CELLS {
                continue;
            }
            let lines = |d: &Document, start: usize, len: usize| {
                let mut v = Vec::with_capacity(len);
                d.for_each_line(start..start + len, |_, t| {
                    let mut buf = String::new();
                    v.push(normalize_line(t, opts, &mut buf).to_string());
                });
                v
            };
            if let Some(rows) = align::align(&lines(left, s.left, s.left_len), &lines(right, s.right, s.right_len)) {
                s.align = aligns.len() as u32;
                aligns.push(rows);
            }
        }
        let mut m = DiffModel { segments, row_start: Vec::new(), hunks: Vec::new(), aligns };
        m.reindex();
        m
    }

    fn seg_len(&self, s: &Segment) -> usize {
        if s.align != NO_ALIGN { self.aligns[s.align as usize].len() } else { s.left_len.max(s.right_len) }
    }

    fn reindex(&mut self) {
        self.row_start.clear();
        self.hunks.clear();
        let mut acc = 0;
        for (i, s) in self.segments.iter().enumerate() {
            self.row_start.push(acc);
            acc += self.seg_len(s);
            if s.kind == SegKind::Change {
                self.hunks.push(i);
            }
        }
        self.row_start.push(acc);
    }

    pub fn total_rows(&self) -> usize {
        self.row_start.last().copied().unwrap_or(0)
    }

    pub fn seg_rows(&self, seg: usize) -> std::ops::Range<usize> {
        if seg >= self.segments.len() {
            let t = self.total_rows();
            return t..t;
        }
        self.row_start[seg]..self.row_start[seg + 1]
    }

    pub fn seg_of_row(&self, row: usize) -> usize {
        self.row_start.partition_point(|&s| s <= row).saturating_sub(1).min(self.segments.len().saturating_sub(1))
    }

    pub fn row_info(&self, row: usize) -> RowInfo {
        let seg = self.seg_of_row(row);
        let s = &self.segments[seg];
        let off = row - self.row_start[seg];
        if s.align != NO_ALIGN {
            let (l, r) = self.aligns[s.align as usize][off];
            return RowInfo {
                seg,
                kind: s.kind,
                left: (l != align::NONE).then(|| s.left + l as usize),
                right: (r != align::NONE).then(|| s.right + r as usize),
            };
        }
        RowInfo {
            seg,
            kind: s.kind,
            left: (off < s.left_len).then_some(s.left + off),
            right: (off < s.right_len).then_some(s.right + off),
        }
    }

    /// Display row of a line on the given side.
    pub fn row_of_line(&self, line: usize, right_side: bool) -> usize {
        let key = |s: &Segment| if right_side { (s.right, s.right_len) } else { (s.left, s.left_len) };
        let idx = self.segments.partition_point(|s| key(s).0 + key(s).1 <= line);
        match self.segments.get(idx) {
            Some(s) if s.align != NO_ALIGN => {
                let k = line.saturating_sub(key(s).0) as u32;
                let rows = &self.aligns[s.align as usize];
                let off = rows.iter().position(|&(l, r)| (if right_side { r } else { l }) == k).unwrap_or(0);
                self.row_start[idx] + off
            }
            Some(s) => self.row_start[idx] + line.saturating_sub(key(s).0),
            None => self.total_rows(),
        }
    }

    /// Index into `hunks` of the hunk at or after `row`.
    pub fn hunk_at_or_after(&self, row: usize) -> Option<usize> {
        let i = self.hunks.partition_point(|&seg| self.row_start[seg + 1] <= row);
        (i < self.hunks.len()).then_some(i)
    }

    /// Index into `hunks` of the hunk containing `row`.
    pub fn hunk_containing(&self, row: usize) -> Option<usize> {
        let i = self.hunk_at_or_after(row)?;
        self.seg_rows(self.hunks[i]).contains(&row).then_some(i)
    }

    /// Update the model after copying the content of segment `seg` from one side to
    /// the other. The segment becomes equal and following line numbers shift.
    pub fn apply_copy(&mut self, seg: usize, to_right: bool) {
        let s = self.segments[seg];
        let (new_len, delta): (usize, isize) = if to_right {
            (s.left_len, s.left_len as isize - s.right_len as isize)
        } else {
            (s.right_len, s.right_len as isize - s.left_len as isize)
        };
        self.segments[seg] = Segment::new(SegKind::Equal, s.left, new_len, s.right, new_len);
        for t in &mut self.segments[seg + 1..] {
            if to_right {
                t.right = (t.right as isize + delta) as usize;
            } else {
                t.left = (t.left as isize + delta) as usize;
            }
        }
        // Drop empty segments and merge adjacent equal runs.
        let mut merged: Vec<Segment> = Vec::with_capacity(self.segments.len());
        for t in self.segments.drain(..).filter(|t| t.left_len + t.right_len > 0) {
            if let Some(last) = merged.last_mut()
                && last.kind == SegKind::Equal
                && t.kind == SegKind::Equal
            {
                last.left_len += t.left_len;
                last.right_len += t.right_len;
                continue;
            }
            merged.push(t);
        }
        self.segments = merged;
        self.reindex();
    }

    #[cfg(test)]
    pub fn is_identical(&self) -> bool {
        self.hunks.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::text::TextSource;
    use std::sync::Arc;

    fn doc(s: &str) -> Document {
        Document::new(Arc::new(TextSource::from_string(s)))
    }

    #[test]
    fn basic_diff_and_rows() {
        let a = doc("a\nb\nc\nd\n");
        let b = doc("a\nB\nB2\nc\n");
        let m = DiffModel::compute(&a, &b, &DiffOptions::default());
        assert_eq!(m.hunks.len(), 2);
        assert_eq!(m.total_rows(), 5);
        let r1 = m.row_info(1);
        assert_eq!((r1.left, r1.right, r1.kind), (Some(1), Some(1), SegKind::Change));
        let r2 = m.row_info(2);
        assert_eq!((r2.left, r2.right), (None, Some(2)));
        let r4 = m.row_info(4);
        assert_eq!((r4.left, r4.right), (Some(3), None));
        assert_eq!(m.row_of_line(2, false), 3);
        assert_eq!(m.row_of_line(3, true), 3);
    }

    #[test]
    fn whitespace_and_case() {
        let a = doc("int  x = 1;\nFoo\n");
        let b = doc("int x = 1; \nfoo\n");
        let mut o = DiffOptions::default();
        assert_eq!(DiffModel::compute(&a, &b, &o).hunks.len(), 1);
        o.whitespace = WhitespaceMode::IgnoreChanges;
        o.ignore_case = true;
        assert!(DiffModel::compute(&a, &b, &o).is_identical());
    }

    #[test]
    fn aligned_rows() {
        let a = doc("start\nlet total = compute(a, b);\nend\n");
        let b = doc("start\n// a brand new comment\nlet total = compute(a, b, c);\nend\n");
        let m = DiffModel::compute(&a, &b, &DiffOptions::default());
        assert_eq!(m.total_rows(), 4);
        let r = m.row_info(1);
        assert_eq!((r.left, r.right), (None, Some(1)));
        let r = m.row_info(2);
        assert_eq!((r.left, r.right), (Some(1), Some(2)));
        assert_eq!(m.row_of_line(1, false), 2);
    }

    #[test]
    fn copy_updates_model() {
        let mut a = doc("1\n2\n3\n4\n5\n");
        let mut b = doc("1\nX\nY\n3\n4\nZ\n");
        let o = DiffOptions::default();
        let mut m = DiffModel::compute(&a, &b, &o);
        assert_eq!(m.hunks.len(), 2);
        let seg = m.hunks[0];
        let s = m.segments[seg];
        b.splice(s.right..s.right + s.right_len, a.slice(s.left..s.left + s.left_len));
        m.apply_copy(seg, true);
        assert_eq!(m.hunks.len(), 1);
        let fresh = DiffModel::compute(&a, &b, &o);
        assert_eq!(m.segments, fresh.segments);
        let seg = m.hunks[0];
        let s = m.segments[seg];
        a.splice(s.left..s.left + s.left_len, b.slice(s.right..s.right + s.right_len));
        m.apply_copy(seg, false);
        assert!(m.is_identical());
        assert_eq!(m.segments.len(), 1);
    }
}
