//! Asynchronous syntax highlighting. Each document gets a worker thread that
//! highlights only the lines currently visible. Parser state is checkpointed every
//! `CHECKPOINT` lines so that scrolling back and forth stays cheap; a far jump into
//! a huge file starts from a fresh state a few hundred lines earlier (accurate for
//! virtually all code) instead of parsing everything before it.

use std::collections::{HashMap, HashSet};
use std::ops::Range;
use std::path::Path;
use std::sync::{Arc, OnceLock};

use crossbeam_channel::{Receiver, Sender, TryRecvError};
use egui::Color32;
use parking_lot::Mutex;
use syntect::highlighting::{FontStyle, HighlightState, Highlighter as SynHighlighter, RangedHighlightIterator, Theme};
use syntect::parsing::{ParseState, ScopeStack, SyntaxReference, SyntaxSet};
use two_face::theme::{EmbeddedLazyThemeSet, EmbeddedThemeName};

use crate::text::Document;

const CHECKPOINT: usize = 256;
/// How far beyond the last checkpoint we are willing to parse sequentially.
const MAX_CATCHUP: usize = 40_000;
/// Context parsed before the target when jumping far ahead.
const JUMP_CONTEXT: usize = 400;
/// Longer lines are left unhighlighted (regex engines get slow on them).
const MAX_LINE: usize = 4_000;
const MAX_CACHED: usize = 60_000;

pub struct Assets {
    pub syntaxes: SyntaxSet,
    pub themes: EmbeddedLazyThemeSet,
}

static ASSETS: OnceLock<Assets> = OnceLock::new();

pub fn assets() -> &'static Assets {
    ASSETS.get_or_init(|| Assets { syntaxes: two_face::syntax::extra_newlines(), themes: two_face::theme::extra() })
}

/// Load grammars in the background so the first comparison opens instantly.
pub fn warm_up() {
    std::thread::spawn(|| {
        let a = assets();
        for name in [EmbeddedThemeName::OneHalfDark, EmbeddedThemeName::OneHalfLight] {
            a.themes.get(name);
        }
    });
}

pub fn theme_names() -> impl Iterator<Item = &'static str> {
    EmbeddedLazyThemeSet::theme_names().iter().map(|t| t.as_name())
}

pub fn theme_by_name(name: &str) -> EmbeddedThemeName {
    EmbeddedLazyThemeSet::theme_names()
        .iter()
        .copied()
        .find(|t| t.as_name() == name)
        .unwrap_or(EmbeddedThemeName::OneHalfDark)
}

/// Pick a syntax by file name, extension or first line (shebang, modelines...).
pub fn detect_syntax(path: Option<&Path>, first_line: &str) -> Option<String> {
    let ss = &assets().syntaxes;
    let by_path = path.and_then(|p| {
        let name = p.file_name()?.to_str()?;
        ss.find_syntax_by_extension(name)
            .or_else(|| ss.find_syntax_by_extension(p.extension()?.to_str()?))
            .or_else(|| {
                let lower = p.extension()?.to_str()?.to_lowercase();
                ss.find_syntax_by_extension(&lower)
            })
    });
    by_path
        .or_else(|| ss.find_syntax_by_first_line(first_line))
        .filter(|s| s.name != "Plain Text")
        .map(|s| s.name.clone())
}

pub fn syntax_names() -> Vec<String> {
    let mut v: Vec<String> = assets()
        .syntaxes
        .syntaxes()
        .iter()
        .filter(|s| !s.hidden && s.name != "Plain Text")
        .map(|s| s.name.clone())
        .collect();
    v.sort_by_key(|s| s.to_lowercase());
    v.dedup();
    v
}

#[derive(Clone, Debug)]
pub struct StyledSpan {
    pub range: Range<usize>,
    pub color: Color32,
    pub italic: bool,
    pub underline: bool,
}

pub type LineSpans = Arc<[StyledSpan]>;

#[derive(Default)]
struct Shared {
    lines: HashMap<usize, LineSpans>,
}

enum Req {
    Doc { doc: Document, changed_from: usize },
    Config { syntax: Option<String>, theme: EmbeddedThemeName },
    Want(Range<usize>),
}

pub struct Highlighter {
    tx: Sender<Req>,
    shared: Arc<Mutex<Shared>>,
    last_want: Range<usize>,
    syntax: Option<String>,
    theme: Option<EmbeddedThemeName>,
}

impl Highlighter {
    pub fn new(doc: &Document, wake: impl Fn() + Send + 'static) -> Self {
        let (tx, rx) = crossbeam_channel::unbounded();
        let shared = Arc::new(Mutex::new(Shared::default()));
        let s2 = shared.clone();
        tx.send(Req::Doc { doc: doc.clone(), changed_from: 0 }).ok();
        std::thread::Builder::new()
            .name("highlighter".into())
            .spawn(move || Worker::new(rx, s2, Box::new(wake)).run())
            .expect("spawn highlighter");
        Highlighter { tx, shared, last_want: 0..0, syntax: None, theme: None }
    }

    pub fn set_doc(&mut self, doc: &Document, changed_from: usize) {
        self.tx.send(Req::Doc { doc: doc.clone(), changed_from }).ok();
        self.last_want = 0..0;
    }

    pub fn set_config(&mut self, syntax: Option<String>, theme: EmbeddedThemeName) {
        if self.syntax != syntax || self.theme != Some(theme) {
            self.syntax = syntax.clone();
            self.theme = Some(theme);
            self.tx.send(Req::Config { syntax, theme }).ok();
            self.last_want = 0..0;
        }
    }

    /// Ask for a range of lines, then return what is available for it now.
    pub fn fetch(&mut self, range: Range<usize>) -> Vec<Option<LineSpans>> {
        if self.syntax.is_none() {
            return vec![None; range.len()];
        }
        if range != self.last_want {
            self.last_want = range.clone();
            self.tx.send(Req::Want(range.clone())).ok();
        }
        let g = self.shared.lock();
        range.map(|l| g.lines.get(&l).cloned()).collect()
    }
}

struct Worker {
    rx: Receiver<Req>,
    shared: Arc<Mutex<Shared>>,
    wake: Box<dyn Fn() + Send>,
    doc: Option<Document>,
    syntax: Option<&'static SyntaxReference>,
    theme: Option<&'static Theme>,
    checkpoints: Vec<(ParseState, HighlightState)>,
    want: Range<usize>,
    done: HashSet<usize>,
    /// Set when the cache belongs to an older document/config and must be replaced.
    stale: bool,
}

impl Worker {
    fn new(rx: Receiver<Req>, shared: Arc<Mutex<Shared>>, wake: Box<dyn Fn() + Send>) -> Self {
        Worker {
            rx,
            shared,
            wake,
            doc: None,
            syntax: None,
            theme: None,
            checkpoints: Vec::new(),
            want: 0..0,
            done: HashSet::new(),
            stale: false,
        }
    }

    fn apply(&mut self, req: Req) {
        match req {
            Req::Doc { doc, changed_from } => {
                self.doc = Some(doc);
                self.checkpoints.truncate(changed_from / CHECKPOINT + 1);
                self.done.clear();
                self.stale = true;
            }
            Req::Config { syntax, theme } => {
                let a = assets();
                self.syntax = syntax.and_then(|s| a.syntaxes.find_syntax_by_name(&s));
                self.theme = Some(a.themes.get(theme));
                self.checkpoints.clear();
                self.done.clear();
                self.stale = true;
            }
            Req::Want(r) => self.want = r,
        }
    }

    fn run(mut self) {
        loop {
            let Ok(req) = self.rx.recv() else { return };
            self.apply(req);
            loop {
                match self.rx.try_recv() {
                    Ok(r) => self.apply(r),
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => return,
                }
            }
            if self.work() {
                (self.wake)();
            }
        }
    }

    /// Highlight the wanted range. Returns true if anything new was produced.
    fn work(&mut self) -> bool {
        let (Some(doc), Some(syntax), Some(theme)) = (self.doc.clone(), self.syntax, self.theme) else {
            return false;
        };
        let end = self.want.end.min(doc.line_count());
        let Some(first_missing) = (self.want.start..end).find(|l| !self.done.contains(l)) else {
            return false;
        };
        let ss = &assets().syntaxes;
        let hl = SynHighlighter::new(theme);
        let fresh = || (ParseState::new(syntax), HighlightState::new(&hl, ScopeStack::new()));

        let k = first_missing / CHECKPOINT;
        let (start, mut state, record) = if k < self.checkpoints.len() {
            (k * CHECKPOINT, self.checkpoints[k].clone(), true)
        } else {
            let last = self.checkpoints.len().saturating_sub(1);
            if first_missing - last * CHECKPOINT <= MAX_CATCHUP {
                match self.checkpoints.last() {
                    Some(cp) => (last * CHECKPOINT, cp.clone(), true),
                    None => (0, fresh(), true),
                }
            } else {
                (first_missing.saturating_sub(JUMP_CONTEXT), fresh(), false)
            }
        };

        let mut produced: Vec<(usize, LineSpans)> = Vec::with_capacity(end.saturating_sub(start));
        let mut line_buf = String::new();
        let mut line = start;
        const CHUNK: usize = 512;
        while line < end {
            let chunk_end = (line + CHUNK).min(end);
            doc.for_each_line(line..chunk_end, |i, text| {
                if record && i % CHECKPOINT == 0 && i / CHECKPOINT == self.checkpoints.len() {
                    self.checkpoints.push(state.clone());
                }
                let spans = if text.len() > MAX_LINE {
                    Arc::from(Vec::new())
                } else {
                    line_buf.clear();
                    line_buf.push_str(text);
                    line_buf.push('\n');
                    highlight_line(&mut state, &line_buf, text.len(), ss, &hl, &fresh)
                };
                if i >= first_missing {
                    produced.push((i, spans));
                }
            });
            line = chunk_end;
            // Stay responsive: if the view moved on, publish what we have and restart.
            if !self.rx.is_empty() {
                break;
            }
        }
        if produced.is_empty() {
            return false;
        }
        let mut g = self.shared.lock();
        if self.stale || g.lines.len() > MAX_CACHED {
            // Only lines produced in this pass survive the reset.
            g.lines.clear();
            self.done.clear();
            self.stale = false;
        }
        for (i, s) in produced {
            self.done.insert(i);
            g.lines.insert(i, s);
        }
        true
    }
}

fn highlight_line(
    state: &mut (ParseState, HighlightState),
    line: &str,
    text_len: usize,
    ss: &SyntaxSet,
    hl: &SynHighlighter,
    fresh: &dyn Fn() -> (ParseState, HighlightState),
) -> LineSpans {
    let ops = match state.0.parse_line(line, ss) {
        Ok(ops) => ops,
        Err(_) => {
            *state = fresh();
            return Arc::from(Vec::new());
        }
    };
    let mut out = Vec::new();
    for (style, _, range) in RangedHighlightIterator::new(&mut state.1, &ops, line, hl) {
        let range = range.start.min(text_len)..range.end.min(text_len);
        if range.is_empty() {
            continue;
        }
        let c = style.foreground;
        out.push(StyledSpan {
            range,
            color: Color32::from_rgba_unmultiplied(c.r, c.g, c.b, c.a),
            italic: style.font_style.contains(FontStyle::ITALIC),
            underline: style.font_style.contains(FontStyle::UNDERLINE),
        });
    }
    Arc::from(out)
}

#[cfg(test)]
mod tests {
    #[test]
    fn detects_common_languages() {
        use std::path::Path;
        let n = super::syntax_names().len();
        let t = super::theme_names().count();
        eprintln!("syntaxes={n} themes={t}");
        for (file, lang) in [("a.rs", "Rust"), ("a.py", "Python"), ("a.ts", "TypeScript"), ("a.go", "Go"), ("a.cpp", "C++"), ("A.java", "Java"), ("a.cs", "C#"), ("a.toml", "TOML"), ("Dockerfile", "Dockerfile"), ("a.kt", "Kotlin"), ("a.swift", "Swift")] {
            assert_eq!(super::detect_syntax(Some(Path::new(file)), "").as_deref(), Some(lang), "{file}");
        }
        assert_eq!(super::detect_syntax(None, "#!/usr/bin/env python3").as_deref(), Some("Python"));
    }
}
