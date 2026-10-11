//! Aligns similar lines inside a changed block so that modified lines sit next
//! to their counterparts instead of simply being paired top to bottom.

/// Marker for "no line on this side" in an alignment row.
pub const NONE: u32 = u32::MAX;

/// Blocks larger than this (left × right lines) are left top-aligned.
pub const MAX_CELLS: usize = 40_000;
/// Minimum similarity for two lines to be paired.
const MIN_SIM: f32 = 0.45;

/// Sorted character bigrams of a whitespace-trimmed line.
fn bigrams(s: &str) -> Vec<u64> {
    let chars: Vec<char> = s.split_whitespace().flat_map(|w| w.chars().chain([' '])).collect();
    let mut v: Vec<u64> = chars.windows(2).map(|w| ((w[0] as u64) << 32) | w[1] as u64).collect();
    v.sort_unstable();
    v
}

/// Dice coefficient of two sorted bigram multisets.
fn dice(a: &[u64], b: &[u64]) -> f32 {
    if a.is_empty() && b.is_empty() {
        return 1.0;
    }
    let (mut i, mut j, mut common) = (0, 0, 0usize);
    while i < a.len() && j < b.len() {
        match a[i].cmp(&b[j]) {
            std::cmp::Ordering::Less => i += 1,
            std::cmp::Ordering::Greater => j += 1,
            std::cmp::Ordering::Equal => {
                common += 1;
                i += 1;
                j += 1;
            }
        }
    }
    2.0 * common as f32 / (a.len() + b.len()) as f32
}

/// Returns display rows as `(left offset, right offset)` pairs (`NONE` = filler),
/// or `None` when plain top alignment is already the best choice.
pub fn align(left: &[String], right: &[String]) -> Option<Vec<(u32, u32)>> {
    let (n, m) = (left.len(), right.len());
    if n == 0 || m == 0 || n * m > MAX_CELLS {
        return None;
    }
    let lb: Vec<Vec<u64>> = left.iter().map(|s| bigrams(s)).collect();
    let rb: Vec<Vec<u64>> = right.iter().map(|s| bigrams(s)).collect();
    let sim = |i: usize, j: usize| {
        let s = dice(&lb[i], &rb[j]);
        if s >= MIN_SIM { s } else { 0.0 }
    };
    // Weighted LCS: dp[i][j] = best score using left[i..] and right[j..].
    let w = m + 1;
    let mut dp = vec![0f32; (n + 1) * w];
    let mut take = vec![false; (n + 1) * w];
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            let s = sim(i, j);
            let diag = if s > 0.0 { s + dp[(i + 1) * w + j + 1] } else { 0.0 };
            let skip = dp[(i + 1) * w + j].max(dp[i * w + j + 1]);
            if diag > skip {
                dp[i * w + j] = diag;
                take[i * w + j] = true;
            } else {
                dp[i * w + j] = skip;
            }
        }
    }
    // Collect matched pairs.
    let mut pairs = Vec::new();
    let (mut i, mut j) = (0, 0);
    while i < n && j < m {
        if take[i * w + j] {
            pairs.push((i, j));
            i += 1;
            j += 1;
        } else if dp[(i + 1) * w + j] >= dp[i * w + j + 1] {
            i += 1;
        } else {
            j += 1;
        }
    }
    // Emit rows: unmatched gaps between matches are paired top to bottom.
    let mut rows = Vec::with_capacity(n.max(m));
    let (mut li, mut ri) = (0usize, 0usize);
    let gap = |rows: &mut Vec<(u32, u32)>, l0: usize, l1: usize, r0: usize, r1: usize| {
        for k in 0..(l1 - l0).max(r1 - r0) {
            let l = if l0 + k < l1 { (l0 + k) as u32 } else { NONE };
            let r = if r0 + k < r1 { (r0 + k) as u32 } else { NONE };
            rows.push((l, r));
        }
    };
    for (pi, pj) in pairs {
        gap(&mut rows, li, pi, ri, pj);
        rows.push((pi as u32, pj as u32));
        li = pi + 1;
        ri = pj + 1;
    }
    gap(&mut rows, li, n, ri, m);
    let top_aligned = rows.len() == n.max(m) && rows.iter().enumerate().all(|(k, &(l, r))| {
        (l == NONE || l as usize == k) && (r == NONE || r as usize == k)
    });
    if top_aligned { None } else { Some(rows) }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| x.to_string()).collect()
    }

    #[test]
    fn inserted_line_before_modified() {
        let l = s(&["let total = compute(a, b);", "println!(\"{total}\");"]);
        let r = s(&["// new comment here", "let total = compute(a, b, c);", "println!(\"{total:?}\");"]);
        let rows = align(&l, &r).unwrap();
        assert_eq!(rows, vec![(NONE, 0), (0, 1), (1, 2)]);
    }

    #[test]
    fn already_top_aligned() {
        let l = s(&["foo(1)", "bar(2)"]);
        let r = s(&["foo(10)", "bar(20)", "baz()"]);
        assert!(align(&l, &r).is_none());
    }
}
