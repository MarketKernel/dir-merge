//! Line-based piece table. A document is a sequence of pieces, each referencing a
//! run of lines in an immutable source: the original file, the file on the other
//! side (after a merge), or a small buffer of edited lines. Copying a block of
//! lines between documents therefore never copies text.

use std::io::{self, Write};
use std::ops::Range;
use std::sync::Arc;

use super::source::{TextSource, bom_bytes, encode_into};

#[derive(Clone)]
pub enum PieceSrc {
    File(Arc<TextSource>),
    Added(Arc<Vec<String>>),
}

impl PieceSrc {
    fn same(&self, other: &PieceSrc) -> bool {
        match (self, other) {
            (PieceSrc::File(a), PieceSrc::File(b)) => Arc::ptr_eq(a, b),
            (PieceSrc::Added(a), PieceSrc::Added(b)) => Arc::ptr_eq(a, b),
            _ => false,
        }
    }
}

#[derive(Clone)]
pub struct Piece {
    pub src: PieceSrc,
    pub start: usize,
    pub len: usize,
}

#[derive(Clone)]
pub struct Document {
    /// The file this document was loaded from (defines path, encoding and EOL on save).
    pub base: Arc<TextSource>,
    pieces: Vec<Piece>,
    /// `prefix[i]` = document line where piece `i` starts; last element = line count.
    prefix: Vec<usize>,
    /// Incremented on every modification.
    pub revision: u64,
}

impl Document {
    pub fn new(base: Arc<TextSource>) -> Self {
        let n = base.line_count();
        let pieces = if n > 0 {
            vec![Piece { src: PieceSrc::File(base.clone()), start: 0, len: n }]
        } else {
            Vec::new()
        };
        let mut doc = Document { base, pieces, prefix: Vec::new(), revision: 0 };
        doc.rebuild_prefix();
        doc
    }

    fn rebuild_prefix(&mut self) {
        self.prefix.clear();
        let mut acc = 0;
        for p in &self.pieces {
            self.prefix.push(acc);
            acc += p.len;
        }
        self.prefix.push(acc);
    }

    pub fn line_count(&self) -> usize {
        *self.prefix.last().unwrap()
    }

    fn locate(&self, line: usize) -> (usize, usize) {
        let idx = self.prefix.partition_point(|&s| s <= line) - 1;
        (idx, line - self.prefix[idx])
    }

    pub fn line_text(&self, line: usize) -> String {
        let (pi, off) = self.locate(line);
        let p = &self.pieces[pi];
        match &p.src {
            PieceSrc::File(s) => s.line_text(p.start + off),
            PieceSrc::Added(v) => v[p.start + off].clone(),
        }
    }

    /// Visit the text of lines in `range` efficiently (one lock per piece).
    pub fn for_each_line(&self, range: Range<usize>, mut f: impl FnMut(usize, &str)) {
        if range.is_empty() {
            return;
        }
        let (mut pi, mut off) = self.locate(range.start);
        let mut line = range.start;
        while line < range.end {
            let p = &self.pieces[pi];
            let take = (p.len - off).min(range.end - line);
            match &p.src {
                PieceSrc::File(s) => {
                    let first = line;
                    s.for_each_line(p.start + off..p.start + off + take, |l, text, _| {
                        f(first + l - (p.start + off), text)
                    });
                }
                PieceSrc::Added(v) => {
                    for (k, t) in v[p.start + off..p.start + off + take].iter().enumerate() {
                        f(line + k, t);
                    }
                }
            }
            line += take;
            pi += 1;
            off = 0;
        }
    }

    /// Pieces describing lines `range` of this document.
    pub fn slice(&self, range: Range<usize>) -> Vec<Piece> {
        let mut out = Vec::new();
        if range.is_empty() {
            return out;
        }
        let (mut pi, mut off) = self.locate(range.start);
        let mut remaining = range.len();
        while remaining > 0 {
            let p = &self.pieces[pi];
            let take = (p.len - off).min(remaining);
            out.push(Piece { src: p.src.clone(), start: p.start + off, len: take });
            remaining -= take;
            pi += 1;
            off = 0;
        }
        out
    }

    /// Replace lines `range` by `new` pieces.
    pub fn splice(&mut self, range: Range<usize>, new: Vec<Piece>) {
        assert!(range.end <= self.line_count());
        let mut out: Vec<Piece> = Vec::with_capacity(self.pieces.len() + new.len() + 2);
        let total = self.line_count();
        out.extend(self.slice(0..range.start));
        out.extend(new);
        out.extend(self.slice(range.end..total));
        // Coalesce adjacent pieces that continue each other.
        let mut merged: Vec<Piece> = Vec::with_capacity(out.len());
        for p in out.into_iter().filter(|p| p.len > 0) {
            if let Some(last) = merged.last_mut()
                && last.src.same(&p.src)
                && last.start + last.len == p.start
            {
                last.len += p.len;
                continue;
            }
            merged.push(p);
        }
        self.pieces = merged;
        self.rebuild_prefix();
        self.revision += 1;
    }

    pub fn replace_lines(&mut self, range: Range<usize>, lines: Vec<String>) {
        let len = lines.len();
        let piece = Piece { src: PieceSrc::Added(Arc::new(lines)), start: 0, len };
        self.splice(range, vec![piece]);
    }

    /// Serialize using the base file's encoding. Lines that come from the base file
    /// keep their original terminators; lines from elsewhere get the default EOL.
    pub fn write_to(&self, w: &mut impl Write) -> io::Result<()> {
        let enc = self.base.encoding;
        let eol = self.base.default_eol;
        w.write_all(bom_bytes(enc))?;
        let total = self.line_count();
        let mut bad: Option<char> = None;
        let put = |t: &str, buf: &mut Vec<u8>, bad: &mut Option<char>| {
            if let Err(c) = encode_into(enc, t, buf) {
                bad.get_or_insert(c);
            }
        };
        let mut buf: Vec<u8> = Vec::with_capacity(64 * 1024);
        let mut line_no = 0usize;
        for p in &self.pieces {
            let last_piece_line = line_no + p.len;
            match &p.src {
                PieceSrc::File(s) if Arc::ptr_eq(s, &self.base) && s.can_copy_raw() => {
                    let range = s.raw_lines_range(p.start..p.start + p.len);
                    w.write_all(&buf)?;
                    buf.clear();
                    let ends_with_nl = s.with_bytes(|b| {
                        let raw = &b[range.clone()];
                        // Write in bounded chunks so huge mapped files stream to disk.
                        for chunk in raw.chunks(4 * 1024 * 1024) {
                            w.write_all(chunk)?;
                        }
                        Ok::<_, io::Error>(raw.last() == Some(&b'\n'))
                    })?;
                    if !ends_with_nl && last_piece_line < total {
                        put(eol, &mut buf, &mut bad);
                    }
                }
                PieceSrc::File(s) => {
                    let mut k = line_no;
                    s.for_each_line(p.start..p.start + p.len, |_, text, line_eol| {
                        k += 1;
                        put(text, &mut buf, &mut bad);
                        if k < total || !line_eol.is_empty() {
                            put(eol, &mut buf, &mut bad);
                        }
                    });
                }
                PieceSrc::Added(v) => {
                    for (k, text) in v[p.start..p.start + p.len].iter().enumerate() {
                        put(text, &mut buf, &mut bad);
                        if line_no + k + 1 < total || self.base_ends_with_newline() {
                            put(eol, &mut buf, &mut bad);
                        }
                    }
                }
            }
            line_no = last_piece_line;
            if let Some(c) = bad {
                return Err(io::Error::other(format!(
                    "the character '{c}' cannot be saved in {} encoding",
                    enc.label()
                )));
            }
            if buf.len() > 1024 * 1024 {
                w.write_all(&buf)?;
                buf.clear();
            }
        }
        w.write_all(&buf)?;
        Ok(())
    }

    fn base_ends_with_newline(&self) -> bool {
        let n = self.base.line_count();
        n == 0 || self.base.with_bytes(|b| b[self.base.line_range(n - 1)].last() == Some(&b'\n'))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn doc(s: &str) -> Document {
        Document::new(Arc::new(TextSource::from_string(s)))
    }

    fn all(d: &Document) -> Vec<String> {
        (0..d.line_count()).map(|i| d.line_text(i)).collect()
    }

    fn save(d: &Document) -> String {
        let mut v = Vec::new();
        d.write_to(&mut v).unwrap();
        String::from_utf8(v).unwrap()
    }

    #[test]
    fn splice_between_documents() {
        let mut a = doc("1\n2\n3\n4\n");
        let b = doc("x\ny\n");
        a.splice(1..3, b.slice(0..2));
        assert_eq!(all(&a), ["1", "x", "y", "4"]);
        a.splice(0..0, b.slice(1..2));
        assert_eq!(all(&a), ["y", "1", "x", "y", "4"]);
        assert_eq!(save(&a), "y\n1\nx\ny\n4\n");
        a.replace_lines(2..3, vec!["edited".into()]);
        assert_eq!(all(&a), ["y", "1", "edited", "y", "4"]);
        let mut seen = Vec::new();
        a.for_each_line(1..4, |i, t| seen.push((i, t.to_string())));
        assert_eq!(seen, [(1, "1".into()), (2, "edited".into()), (3, "y".into())]);
    }

    #[test]
    fn unmappable_characters_fail_the_save() {
        let (bytes, _, _) = encoding_rs::WINDOWS_1251.encode("Привет, мир! Как дела? Всё хорошо.\nВторая строка\n");
        let path = std::env::temp_dir().join(format!("dm-enc-{}.txt", std::process::id()));
        std::fs::write(&path, &bytes).unwrap();
        let mut a = Document::new(Arc::new(TextSource::load(&path).unwrap()));
        std::fs::remove_file(&path).ok();
        a.replace_lines(1..2, vec!["Третья".into()]);
        let mut out = Vec::new();
        a.write_to(&mut out).unwrap();
        assert_eq!(encoding_rs::WINDOWS_1251.decode(&out).0, "Привет, мир! Как дела? Всё хорошо.\nТретья\n");
        a.replace_lines(1..2, vec!["日本".into()]);
        assert!(a.write_to(&mut Vec::new()).is_err());
    }

    #[test]
    fn save_preserves_eols() {
        let mut a = doc("a\r\nb\r\nc");
        let b = doc("x\ny");
        assert_eq!(save(&a), "a\r\nb\r\nc");
        a.splice(3..3, b.slice(0..1));
        assert_eq!(save(&a), "a\r\nb\r\nc\r\nx\r\n");
        let mut a = doc("a\nb\n");
        a.splice(0..1, b.slice(1..2));
        assert_eq!(save(&a), "y\nb\n");
    }
}
