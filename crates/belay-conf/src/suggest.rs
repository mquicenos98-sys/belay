//! "Did you mean" support: finds the requested key closest to a key nobody
//! read, if it is close enough to be a plausible typo.

/// Levenshtein distance over chars, with two rolling rows.
fn distance(a: &[char], b: &[char]) -> usize {
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    let mut cur = vec![0; b.len() + 1];
    for i in 1..=a.len() {
        cur[0] = i;
        for j in 1..=b.len() {
            let cost = usize::from(a[i - 1] != b[j - 1]);
            cur[j] = (prev[j] + 1).min(cur[j - 1] + 1).min(prev[j - 1] + cost);
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    prev[b.len()]
}

/// Case and `-`/`_` differences are the cheapest mistakes, so they are
/// ignored when measuring distance (`Max-Body` matches `max_body` exactly).
fn normalize(s: &str) -> Vec<char> {
    s.chars()
        .map(|c| {
            if c == '-' {
                '_'
            } else {
                c.to_ascii_lowercase()
            }
        })
        .collect()
}
