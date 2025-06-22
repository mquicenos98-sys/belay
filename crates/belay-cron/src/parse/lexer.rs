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

/// Numbers longer than this cannot be valid in any field (years have four
/// digits); rejecting them early keeps the arithmetic in `u32`.
const MAX_DIGITS: usize = 6;

/// Tokenizes `text`, the field's source, which starts at `column`.
pub(crate) fn tokenize(text: &str, field: Field, column: usize) -> Result<Vec<Token>> {
    let chars: Vec<char> = text.chars().collect();
    let mut tokens = Vec::with_capacity(chars.len());
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        let col = column + i;
        let single = match c {
            '*' => Some(TokenKind::Star),
            '?' => Some(TokenKind::Question),
            ',' => Some(TokenKind::Comma),
            '-' => Some(TokenKind::Dash),
            '/' => Some(TokenKind::Slash),
            '#' => Some(TokenKind::Hash),
            _ => None,
        };
        if let Some(kind) = single {
            tokens.push(Token { kind, column: col });
            i += 1;
            continue;
        }
        if c.is_ascii_digit() {
            let start = i;
            while i < chars.len() && chars[i].is_ascii_digit() {
                i += 1;
            }
            if i - start > MAX_DIGITS {
                return Err(CronError::field(field, col, FieldProblem::NumberTooLarge));
            }
            let value = chars[start..i]
                .iter()
                .fold(0u32, |acc, d| acc * 10 + d.to_digit(10).unwrap_or(0));
            tokens.push(Token {
                kind: TokenKind::Number(value),
                column: col,
            });
            continue;
        }
        if c.is_ascii_alphabetic() {
            let start = i;
            while i < chars.len() && chars[i].is_ascii_alphabetic() {
                i += 1;
            }
            let name: String = chars[start..i]
                .iter()
                .map(|c| c.to_ascii_uppercase())
                .collect();
            tokens.push(Token {
                kind: TokenKind::Name(name),
                column: col,
            });
            continue;
        }
        return Err(CronError::field(
            field,
            col,
            FieldProblem::UnexpectedChar(c),
        ));
    }
    Ok(tokens)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(text: &str) -> Vec<TokenKind> {
        tokenize(text, Field::DayOfWeek, 1)
            .unwrap()
            .into_iter()
            .map(|t| t.kind)
            .collect()
    }

    #[test]
    fn splits_numbers_names_and_punctuation() {
        use TokenKind::*;
        assert_eq!(
            kinds("*/15,1-5"),
            vec![Star, Slash, Number(15), Comma, Number(1), Dash, Number(5)]
        );
        assert_eq!(kinds("mon#2"), vec![Name("MON".into()), Hash, Number(2)]);
        assert_eq!(kinds("5L"), vec![Number(5), Name("L".into())]);
        assert_eq!(kinds("FriL"), vec![Name("FRIL".into())]);
        assert_eq!(kinds("L-3"), vec![Name("L".into()), Dash, Number(3)]);
        assert_eq!(kinds("?"), vec![Question]);
    }

    #[test]
    fn columns_are_offset_by_the_field_start() {
        let tokens = tokenize("10-20", Field::Minute, 7).unwrap();
        let cols: Vec<usize> = tokens.iter().map(|t| t.column).collect();
        assert_eq!(cols, vec![7, 9, 10]);
    }

    #[test]
    fn rejects_unknown_characters_with_their_column() {
        let err = tokenize("1,2%", Field::Hour, 5).unwrap_err();
        assert_eq!(
            err,
            CronError::field(Field::Hour, 8, FieldProblem::UnexpectedChar('%'))
        );
        let err = tokenize("5é", Field::Hour, 1).unwrap_err();
        assert_eq!(err.column(), Some(2));
    }

    #[test]
    fn rejects_absurdly_long_numbers() {
        let err = tokenize("12345678901", Field::Minute, 1).unwrap_err();
        assert_eq!(
            err.to_string(),
            "number too large in minute field at column 1"
        );
        assert!(tokenize("002025", Field::Year, 1).is_ok());
    }
}
