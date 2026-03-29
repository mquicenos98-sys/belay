//! Terminal colors: when to use them and which state gets which color.
//!
//! Colors are ANSI SGR sequences, used only when stdout is a terminal and
//! neither `--no-color` nor a non-empty `NO_COLOR` (<https://no-color.org>)
//! says otherwise; `TERM=dumb` disables them too.

/// A text style.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Style {
    Bold,
    Dim,
    Red,
    Green,
    Yellow,
    Blue,
    Cyan,
}

impl Style {
    fn code(self) -> &'static str {
        match self {
            Style::Bold => "1",
            Style::Dim => "2",
            Style::Red => "31",
            Style::Green => "32",
            Style::Yellow => "33",
            Style::Blue => "34",
            Style::Cyan => "36",
        }
    }

    /// `text` wrapped in this style's escape sequence.
    pub(crate) fn paint(self, text: &str) -> String {
        format!("\x1b[{}m{text}\x1b[0m", self.code())
    }
}

/// Whether to color output, from the `--no-color` flag, the environment
/// and whether stdout is a terminal.
pub(crate) fn color_enabled(
    no_color_flag: bool,
    var: &dyn Fn(&str) -> Option<String>,
    stdout_is_terminal: bool,
) -> bool {
    if no_color_flag || !stdout_is_terminal {
        return false;
    }
    if var("NO_COLOR").is_some_and(|v| !v.is_empty()) {
        return false;
    }
    var("TERM").map_or(true, |term| term != "dumb")
}

/// The color for a state name, shared by jobs, steps, workflows, queues and
/// cluster members so that the same word always looks the same.
pub(crate) fn state_style(state: &str) -> Option<Style> {
    match state {
        "succeeded" | "alive" | "active" => Some(Style::Green),
        "running" => Some(Style::Cyan),
        "ready" => Some(Style::Blue),
        "scheduled" | "retrying" | "paused" | "suspect" | "pending" => Some(Style::Yellow),
        "dead" | "failed" => Some(Style::Red),
        "cancelled" | "skipped" | "left" => Some(Style::Dim),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(pairs: &'static [(&'static str, &'static str)]) -> impl Fn(&str) -> Option<String> {
        move |name| {
            pairs
                .iter()
                .find(|(k, _)| *k == name)
                .map(|(_, v)| (*v).to_owned())
        }
    }

    #[test]
    fn colors_need_a_terminal_and_no_opt_out() {
        assert!(color_enabled(false, &env(&[]), true));
        assert!(!color_enabled(false, &env(&[]), false));
        assert!(!color_enabled(true, &env(&[]), true));
        assert!(!color_enabled(false, &env(&[("NO_COLOR", "1")]), true));
        assert!(!color_enabled(false, &env(&[("TERM", "dumb")]), true));
        // An empty NO_COLOR does not count, per the convention.
        assert!(color_enabled(false, &env(&[("NO_COLOR", "")]), true));
    }

    #[test]
    fn states_map_to_consistent_colors() {
        assert_eq!(state_style("succeeded"), Some(Style::Green));
        assert_eq!(state_style("dead"), state_style("failed"));
        assert_eq!(state_style("whatever"), None);
        assert_eq!(Style::Red.paint("x"), "\x1b[31mx\x1b[0m");
    }
}
