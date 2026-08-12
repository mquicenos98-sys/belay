//! "Did you mean" suggestions for mistyped commands and flags.
//!
//! Distance is the optimal-string-alignment variant of Damerau-Levenshtein:
//! insertions, deletions, substitutions and swaps of adjacent characters
//! each cost one, which is how people mistype (`lsit`, `--prioirty`).

/// Edits needed to turn `a` into `b`, counting an adjacent transposition as
/// one edit.
///
/// ```
/// use belay_cli::args::edit_distance;
///
/// assert_eq!(edit_distance("list", "lsit"), 1);
/// assert_eq!(edit_distance("queue", "queues"), 1);
/// assert_eq!(edit_distance("kitten", "sitting"), 3);
/// ```
#[must_use]
pub fn edit_distance(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    // Three rows suffice: the transposition rule looks two rows back.
    let mut before: Vec<usize> = vec![0; b.len() + 1];
    let mut previous: Vec<usize> = (0..=b.len()).collect();
    let mut current: Vec<usize> = vec![0; b.len() + 1];
    for i in 1..=a.len() {
        current[0] = i;
        for j in 1..=b.len() {
            let cost = usize::from(a[i - 1] != b[j - 1]);
            let mut best = (previous[j] + 1)
                .min(current[j - 1] + 1)
                .min(previous[j - 1] + cost);
            if i > 1 && j > 1 && a[i - 1] == b[j - 2] && a[i - 2] == b[j - 1] {
                best = best.min(before[j - 2] + 1);
            }
            current[j] = best;
        }
        std::mem::swap(&mut before, &mut previous);
        std::mem::swap(&mut previous, &mut current);
    }
    previous[b.len()]
}

/// The candidate closest to `input`, if any is close enough to be a
/// plausible typo: within a third of the input's length in edits (at least
/// one), or a candidate that `input` is an unambiguous prefix of.
///
/// ```
/// use belay_cli::args::suggest;
///
/// let commands = ["enqueue", "job", "queue", "schedule"];
/// assert_eq!(suggest("sched", commands), Some("schedule"));
/// assert_eq!(suggest("qeue", commands), Some("queue"));
/// assert_eq!(suggest("deploy", commands), None);
/// ```
#[must_use]
pub fn suggest<'a, I>(input: &str, candidates: I) -> Option<&'a str>
where
    I: IntoIterator<Item = &'a str>,
{
    let candidates: Vec<&'a str> = candidates.into_iter().collect();
    let limit = (input.chars().count() / 3).max(1);
    let best = candidates
        .iter()
        .map(|c| (edit_distance(input, c), *c))
        .filter(|(d, _)| *d <= limit)
        .min_by_key(|(d, _)| *d)
        .map(|(_, c)| c);
    if best.is_some() {
        return best;
    }
    // Abbreviations: `sched` for `schedule`, but only when unambiguous and
    // long enough to mean something.
    if input.chars().count() >= 3 {
        let mut prefixed = candidates.iter().filter(|c| c.starts_with(input));
        if let (Some(only), None) = (prefixed.next(), prefixed.next()) {
            return Some(only);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn distance_counts_each_kind_of_edit_once() {
        assert_eq!(edit_distance("", ""), 0);
        assert_eq!(edit_distance("abc", ""), 3);
        assert_eq!(edit_distance("", "abc"), 3);
        assert_eq!(edit_distance("abc", "abc"), 0);
        assert_eq!(edit_distance("abc", "abd"), 1);
        assert_eq!(edit_distance("abc", "acb"), 1);
        assert_eq!(edit_distance("priority", "prioirty"), 1);
        assert_eq!(edit_distance("ca", "abc"), 3);
    }

    #[test]
    fn distance_works_on_characters_not_bytes() {
        assert_eq!(edit_distance("ü", "u"), 1);
        assert_eq!(edit_distance("größe", "grösse"), 2);
    }

    #[test]
    fn suggestions_prefer_the_closest_candidate() {
        let flags = ["priority", "payload", "tag", "tenant"];
        assert_eq!(suggest("prority", flags), Some("priority"));
        assert_eq!(suggest("paylod", flags), Some("payload"));
        assert_eq!(suggest("tga", flags), Some("tag"));
        assert_eq!(suggest("x", flags), None);
    }

    #[test]
    fn short_inputs_allow_one_edit_only() {
        assert_eq!(suggest("lss", ["ls", "list"]), Some("ls"));
        assert_eq!(suggest("gt", ["get"]), Some("get"));
        assert_eq!(suggest("xy", ["get"]), None);
    }

    #[test]
    fn ambiguous_prefixes_are_not_suggested() {
        assert_eq!(suggest("rest", ["restart", "restore"]), None);
        assert_eq!(suggest("resto", ["restart", "restore"]), Some("restore"));
        assert_eq!(suggest("re", ["resume"]), None);
    }
}
