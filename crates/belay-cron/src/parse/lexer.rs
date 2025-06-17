//! Splits one cron field into tokens.
//!
//! Tokens are deliberately coarse: a run of digits is a number and a run of
//! letters is a name, so `15W`, `5L`, `FRIL` and `LW` come out as
//! `[15, "W"]`, `[5, "L"]`, `["FRIL"]` and `["LW"]`. Deciding what a name
//! means (month, weekday, `L`, `W`) is the field parser's job, because it
//! depends on which field is being read.

use crate::error::{CronError, Field, FieldProblem, Result};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum TokenKind {
    Number(u32),
    /// Letters, uppercased.
    Name(String),
    Star,
    Question,
    Comma,
    Dash,
    Slash,
    Hash,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Token {
    pub kind: TokenKind,
    /// 1-based column of the token's first character in the whole expression.
    pub column: usize,
}
