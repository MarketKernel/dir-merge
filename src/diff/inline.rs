//! Word-level diff inside a pair of changed lines, refined to characters at the
//! edges of each changed region. Computed lazily for visible rows only.

use std::collections::HashMap;
use std::ops::Range;

use imara_diff::{Algorithm, Diff, Token};

use super::lines::{DiffOptions, WhitespaceMode};

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct InlineDiff {
    /// Changed byte ranges in the left line.
    pub left: Vec<Range<usize>>,
    /// Changed byte ranges in the right line.
    pub right: Vec<Range<usize>>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Class {
    Word,
    Space,
    Other,
}

fn class(c: char) -> Class {
    if c.is_alphanumeric() || c == '_' {
        Class::Word
    } else if c.is_whitespace() {
        Class::Space
    } else {
        Class::Other
    }
}

/// Split into word runs, whitespace runs and single punctuation characters.
fn tokenize(s: &str) -> Vec<Range<usize>> {
    let mut out = Vec::new();
    let mut iter = s.char_indices().peekable();
    while let Some((start, c)) = iter.next() {
        let cl = class(c);
        let mut end = start + c.len_utf8();
        if cl != Class::Other {
            while let Some(&(i, n)) = iter.peek() {
                if class(n) != cl {
                    break;
                }
                end = i + n.len_utf8();
                iter.next();
            }
        }
        out.push(start..end);
    }
    out
}

/// Lines longer than this are not diffed inline (too costly and rarely useful).
const MAX_INLINE_LEN: usize = 20_000;

/// Returns `None` if the lines are too different for inline highlighting to help.
pub fn inline_diff(a: &str, b: &str, opts: &DiffOptions) -> Option<InlineDiff> {
    if a.len() > MAX_INLINE_LEN || b.len() > MAX_INLINE_LEN {
        return None;
    }
    let skip_ws = opts.whitespace == WhitespaceMode::IgnoreAll;
    let ta: Vec<Range<usize>> = tokenize(a).into_iter().filter(|r| !skip_ws || !a[r.clone()].trim().is_empty()).collect();
    let tb: Vec<Range<usize>> = tokenize(b).into_iter().filter(|r| !skip_ws || !b[r.clone()].trim().is_empty()).collect();

    let mut map: HashMap<String, u32> = HashMap::new();
    let mut intern = |s: &str| -> Token {
        let key = if opts.whitespace == WhitespaceMode::IgnoreChanges && s.trim().is_empty() {
            " ".to_string()
        } else if opts.ignore_case {
            s.to_lowercase()
        } else {
            s.to_string()
        };
        let n = map.len() as u32;
        Token(*map.entry(key).or_insert(n))
    };
    let xa: Vec<Token> = ta.iter().map(|r| intern(&a[r.clone()])).collect();
    let xb: Vec<Token> = tb.iter().map(|r| intern(&b[r.clone()])).collect();
    let mut diff = Diff::default();
    diff.compute_with(Algorithm::Myers, &xa, &xb, map.len() as u32);

    let mut out = InlineDiff::default();
    let mut changed_bytes = 0usize;
    for h in diff.hunks() {
        let ra = span(&ta, h.before.start as usize..h.before.end as usize);
        let rb = span(&tb, h.after.start as usize..h.after.end as usize);
        let (ra, rb) = refine(a, b, ra, rb, opts.ignore_case);
        // In "ignore changes" mode a pure whitespace difference is not a change.
        if opts.whitespace != WhitespaceMode::Compare
            && a[ra.clone()].trim().is_empty()
            && b[rb.clone()].trim().is_empty()
        {
            continue;
        }
        changed_bytes += ra.len() + rb.len();
        if !ra.is_empty() {
            out.left.push(ra);
        }
        if !rb.is_empty() {
            out.right.push(rb);
        }
    }
    let total = a.trim().len() + b.trim().len();
    if total > 0 && changed_bytes * 100 > total * 70 {
        return None;
    }
    Some(out)
}

fn span(tokens: &[Range<usize>], idx: Range<usize>) -> Range<usize> {
    if idx.is_empty() {
        // Zero-width marker at the insertion point.
        let pos = tokens.get(idx.start).map(|r| r.start).or_else(|| tokens.last().map(|r| r.end)).unwrap_or(0);
        return pos..pos;
    }
    tokens[idx.start].start..tokens[idx.end - 1].end
}

/// Shrink a pair of changed regions by their common character prefix and suffix.
fn refine(a: &str, b: &str, mut ra: Range<usize>, mut rb: Range<usize>, ignore_case: bool) -> (Range<usize>, Range<usize>) {
    if ra.is_empty() || rb.is_empty() {
        return (ra, rb);
    }
    let eq = |x: char, y: char| if ignore_case { x.to_lowercase().eq(y.to_lowercase()) } else { x == y };
    let (sa, sb) = (&a[ra.clone()], &b[rb.clone()]);
    let prefix: usize = sa.chars().zip(sb.chars()).take_while(|&(x, y)| eq(x, y)).map(|(x, _)| x.len_utf8()).sum();
    let prefix_b: usize = sa[..prefix].chars().count();
    let prefix_b: usize = sb.chars().take(prefix_b).map(char::len_utf8).sum();
    let (ta, tb) = (&sa[prefix..], &sb[prefix_b..]);
    let suffix_chars = ta.chars().rev().zip(tb.chars().rev()).take_while(|&(x, y)| eq(x, y)).count();
    let suffix_a: usize = ta.chars().rev().take(suffix_chars).map(char::len_utf8).sum();
    let suffix_b: usize = tb.chars().rev().take(suffix_chars).map(char::len_utf8).sum();
    ra = ra.start + prefix..ra.end - suffix_a;
    rb = rb.start + prefix_b..rb.end - suffix_b;
    (ra, rb)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn word_changes() {
        let d = inline_diff("let foo = bar(1);", "let foo = baz(1, 2);", &DiffOptions::default()).unwrap();
        assert_eq!(d.left, vec![12..13]);
        assert_eq!(&"let foo = baz(1, 2);"[d.right[0].clone()], "z");
        assert_eq!(&"let foo = baz(1, 2);"[d.right[1].clone()], ", 2");
    }

    #[test]
    fn too_different() {
        assert!(inline_diff("completely", "unrelated text here", &DiffOptions::default()).is_none());
    }

    #[test]
    fn unicode_refine() {
        let d = inline_diff("привет мир", "привет мира", &DiffOptions::default()).unwrap();
        assert_eq!(d.left, Vec::<Range<usize>>::new());
        assert_eq!(&"привет мира"[d.right[0].clone()], "а");
    }
}
