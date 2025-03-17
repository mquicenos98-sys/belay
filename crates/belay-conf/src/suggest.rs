//! "Did you mean" support: finds the requested key closest to a key nobody
//! read, if it is close enough to be a plausible typo.

/// Optimal string alignment distance: Levenshtein plus transposition of
/// adjacent characters, the most common typo (`lsiten` for `listen`).
fn distance(a: &[char], b: &[char]) -> usize {
    // Three rolling rows: the transposition step looks two rows back.
    let mut prev2 = vec![0; b.len() + 1];
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    let mut cur = vec![0; b.len() + 1];
    for i in 1..=a.len() {
        cur[0] = i;
        for j in 1..=b.len() {
            let cost = usize::from(a[i - 1] != b[j - 1]);
            cur[j] = (prev[j] + 1).min(cur[j - 1] + 1).min(prev[j - 1] + cost);
            if i > 1 && j > 1 && a[i - 1] == b[j - 2] && a[i - 2] == b[j - 1] {
                cur[j] = cur[j].min(prev2[j - 2] + 1);
            }
        }
        std::mem::swap(&mut prev2, &mut prev);
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

    fn dist(a: &str, b: &str) -> usize {
        distance(
            &a.chars().collect::<Vec<_>>(),
            &b.chars().collect::<Vec<_>>(),
        )
    }

    #[test]
    fn distance_counts_edits_and_transpositions() {
        assert_eq!(dist("", ""), 0);
        assert_eq!(dist("abc", ""), 3);
        assert_eq!(dist("sever", "server"), 1);
        assert_eq!(dist("lsiten", "listen"), 1);
        assert_eq!(dist("kitten", "sitting"), 3);
        assert_eq!(dist("héllo", "hello"), 1);
    }

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
