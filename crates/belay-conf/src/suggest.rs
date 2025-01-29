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

/// The candidate closest to `name`, allowing roughly one edit per three
/// characters. Ties go to the alphabetically first candidate so the result
/// does not depend on iteration order.
pub(crate) fn closest<'c, I>(name: &str, candidates: I) -> Option<&'c str>
where
    I: IntoIterator<Item = &'c str>,
{
    let target = normalize(name);
    let limit = (target.len() / 3).max(1);
    candidates
        .into_iter()
        .filter(|c| *c != name)
        .map(|c| (distance(&target, &normalize(c)), c))
        .filter(|&(d, _)| d <= limit)
        .min()
        .map(|(_, c)| c)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn suggests_plausible_typos_only() {
        let known = ["listen", "threads", "max_body"];
        assert_eq!(closest("lsiten", known), Some("listen"));
        assert_eq!(closest("thread", known), Some("threads"));
        assert_eq!(closest("Max-Body", known), Some("max_body"));
        assert_eq!(closest("storage", known), None);
        assert_eq!(closest("ab", ["ac", "xy"]), Some("ac"));
    }

    #[test]
    fn ties_are_broken_alphabetically() {
        assert_eq!(closest("cat", ["cut", "bat", "cot"]), Some("bat"));
    }
}
