//! Parses the tokens of one field into plain values and special items.
//!
//! ```text
//! field   := item (',' item)*
//! item    := ('*' | '?') ['/' step]
//!          | value ['-' value] ['/' step]
//!          | 'L' ['-' n] | 'LW' | n 'W'              (day-of-month only)
//!          | value 'L' | NAME 'L' | value '#' n      (day-of-week only)
//! ```
//!
//! A range whose end is below its start wraps around the field (`FRI-MON`,
//! `22-2`, `NOV-FEB`), except in the year field where it is an error. `a/n`
//! without an end runs to the top of the field, as in Quartz.

use super::lexer::{Token, TokenKind};
use crate::error::{CronError, Field, FieldProblem, Result};

pub(crate) const MONTH_NAMES: [&str; 12] = [
    "JAN", "FEB", "MAR", "APR", "MAY", "JUN", "JUL", "AUG", "SEP", "OCT", "NOV", "DEC",
];
pub(crate) const DAY_NAMES: [&str; 7] = ["SUN", "MON", "TUE", "WED", "THU", "FRI", "SAT"];

/// The values a field accepts and how they map onto the canonical numbering
/// the rest of the crate uses (weekdays 0-6 from Sunday).
#[derive(Debug, Clone, Copy)]
pub(crate) struct Domain {
    pub field: Field,
    /// Smallest value that may be written.
    pub min: u32,
    /// Largest value that may be written.
    pub max: u32,
    /// Top of the range that `*`, open steps and wrapping cover. It differs
    /// from `max` only for the standard day-of-week field, where 7 is an alias
    /// of Sunday and must not be visited twice.
    pub cycle_max: u32,
    pub names: &'static [&'static str],
    /// Value of `names[0]`.
    pub name_base: u32,
}

impl Domain {
    pub(crate) const SECOND: Domain = Domain::numeric(Field::Second, 0, 59);
    pub(crate) const MINUTE: Domain = Domain::numeric(Field::Minute, 0, 59);
    pub(crate) const HOUR: Domain = Domain::numeric(Field::Hour, 0, 23);
    pub(crate) const DAY_OF_MONTH: Domain = Domain::numeric(Field::DayOfMonth, 1, 31);
    pub(crate) const YEAR: Domain = Domain::numeric(Field::Year, crate::MIN_YEAR, crate::MAX_YEAR);
    pub(crate) const MONTH: Domain = Domain {
        field: Field::Month,
        min: 1,
        max: 12,
        cycle_max: 12,
        names: &MONTH_NAMES,
        name_base: 1,
    };
    /// Vixie numbering: 0-7, both 0 and 7 are Sunday.
    pub(crate) const DAY_OF_WEEK: Domain = Domain {
        field: Field::DayOfWeek,
        min: 0,
        max: 7,
        cycle_max: 6,
        names: &DAY_NAMES,
        name_base: 0,
    };
    /// Quartz numbering: 1-7, 1 is Sunday.
    pub(crate) const DAY_OF_WEEK_QUARTZ: Domain = Domain {
        field: Field::DayOfWeek,
        min: 1,
        max: 7,
        cycle_max: 7,
        names: &DAY_NAMES,
        name_base: 1,
    };

    const fn numeric(field: Field, min: u32, max: u32) -> Domain {
        Domain {
            field,
            min,
            max,
            cycle_max: max,
            names: &[],
            name_base: 0,
        }
    }

    /// Maps a written value onto the canonical numbering.
    fn canonical(&self, v: u32) -> u32 {
        if self.field == Field::DayOfWeek {
            (v - self.min) % 7
        } else {
            v
        }
    }

    fn span(&self) -> u32 {
        self.cycle_max - self.min + 1
    }
}

/// A day-of-month or day-of-week item that is not a plain value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Special {
    /// `L`: the last day of the month.
    LastDay,
    /// `L-n`: `n` days before the last day.
    LastDayOffset(u32),
    /// `nW`: the weekday (Mon-Fri) nearest day `n`, within the month.
    NearestWeekday(u32),
    /// `LW`: the last weekday (Mon-Fri) of the month.
    LastWeekday,
    /// `dL`: the last weekday `d` (0-6) of the month.
    LastOfWeekday(u32),
    /// `d#n`: the `n`th weekday `d` (0-6) of the month.
    NthWeekday(u32, u32),
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct ParsedField {
    /// Plain values in canonical numbering, possibly repeated.
    pub values: Vec<u32>,
    pub specials: Vec<Special>,
}

/// Parses a non-empty field. `end_column` is the column just past its text.
pub(crate) fn parse_field(
    tokens: &[Token],
    domain: &Domain,
    end_column: usize,
) -> Result<ParsedField> {
    FieldParser {
        tokens,
        pos: 0,
        domain,
        end_column,
        out: ParsedField::default(),
    }
    .run()
}

struct FieldParser<'a> {
    tokens: &'a [Token],
    pos: usize,
    domain: &'a Domain,
    end_column: usize,
    out: ParsedField,
}

impl FieldParser<'_> {
    fn run(mut self) -> Result<ParsedField> {
        loop {
            self.item()?;
            match self.bump() {
                None => return Ok(self.out),
                Some(t) if t.kind == TokenKind::Comma => {
                    if self.peek().is_none() {
                        return Err(self.error(t.column, FieldProblem::EmptyItem));
                    }
                }
                Some(t) => return Err(self.unexpected(&t)),
            }
        }
    }

    fn error(&self, column: usize, problem: FieldProblem) -> CronError {
        CronError::field(self.domain.field, column, problem)
    }

    fn peek(&self) -> Option<&TokenKind> {
        self.tokens.get(self.pos).map(|t| &t.kind)
    }

    fn bump(&mut self) -> Option<Token> {
        let t = self.tokens.get(self.pos).cloned();
        if t.is_some() {
            self.pos += 1;
        }
        t
    }

    fn eat(&mut self, kind: &TokenKind) -> bool {
        if self.peek() == Some(kind) {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    fn peek_name(&self, name: &str) -> bool {
        matches!(self.peek(), Some(TokenKind::Name(n)) if n == name)
    }

    fn expect_token(&mut self) -> Result<Token> {
        self.bump()
            .ok_or_else(|| self.error(self.end_column, FieldProblem::UnexpectedEnd))
    }

    /// The error for a token that cannot appear where it did.
    fn unexpected(&self, t: &Token) -> CronError {
        let problem = match &t.kind {
            TokenKind::Name(n) if n == "L" || n == "LW" => FieldProblem::NotAllowed("'L'"),
            TokenKind::Name(n) if n == "W" => FieldProblem::NotAllowed("'W'"),
            TokenKind::Hash => FieldProblem::NotAllowed("'#'"),
            TokenKind::Question => FieldProblem::NotAllowed("'?'"),
            TokenKind::Comma => FieldProblem::EmptyItem,
            TokenKind::Number(n) => {
                FieldProblem::UnexpectedChar(n.to_string().chars().next().unwrap_or('0'))
            }
            TokenKind::Name(n) => FieldProblem::UnexpectedChar(n.chars().next().unwrap_or('?')),
            TokenKind::Star => FieldProblem::UnexpectedChar('*'),
            TokenKind::Dash => FieldProblem::UnexpectedChar('-'),
            TokenKind::Slash => FieldProblem::UnexpectedChar('/'),
        };
        self.error(t.column, problem)
    }

    fn is_day_field(&self) -> bool {
        matches!(self.domain.field, Field::DayOfMonth | Field::DayOfWeek)
    }

    fn item(&mut self) -> Result<()> {
        let tok = self.expect_token()?;
        match &tok.kind {
            TokenKind::Star | TokenKind::Question => {
                if tok.kind == TokenKind::Question && !self.is_day_field() {
                    return Err(self.unexpected(&tok));
                }
                let step = if self.eat(&TokenKind::Slash) {
                    self.step()?
                } else {
                    1
                };
                self.push_range(self.domain.min, self.domain.cycle_max, step);
                Ok(())
            }
            TokenKind::Comma => Err(self.error(tok.column, FieldProblem::EmptyItem)),
            TokenKind::Name(n) if self.domain.field == Field::DayOfMonth && n == "L" => {
                if self.eat(&TokenKind::Dash) {
                    let (k, column) = self.number()?;
                    if !(1..=30).contains(&k) {
                        return Err(self.error(
                            column,
                            FieldProblem::OutOfRange {
                                value: k,
                                min: 1,
                                max: 30,
                            },
                        ));
                    }
                    self.special(Special::LastDayOffset(k))
                } else {
                    self.special(Special::LastDay)
                }
            }
            TokenKind::Name(n) if self.domain.field == Field::DayOfMonth && n == "LW" => {
                self.special(Special::LastWeekday)
            }
            TokenKind::Name(n) if self.domain.field == Field::DayOfWeek && n == "L" => {
                let problem = FieldProblem::NotAllowed("'L' without a weekday");
                Err(self.error(tok.column, problem))
            }
            TokenKind::Name(n)
                if self.domain.field == Field::DayOfWeek
                    && n.len() == 4
                    && n.ends_with('L')
                    && DAY_NAMES.contains(&&n[..3]) =>
            {
                let day = self.name_value(&n[..3], tok.column)?;
                self.special(Special::LastOfWeekday(self.domain.canonical(day)))
            }
            TokenKind::Number(_) | TokenKind::Name(_) => self.value_item(&tok),
            _ => Err(self.unexpected(&tok)),
        }
    }

    /// An item starting with a plain value: single, range, step, or one of
    /// the value-prefixed specials (`15W`, `5L`, `MON#2`).
    fn value_item(&mut self, tok: &Token) -> Result<()> {
        let start = self.value(tok)?;
        match self.domain.field {
            Field::DayOfMonth if self.peek_name("W") => {
                self.pos += 1;
                return self.special(Special::NearestWeekday(start));
            }
            Field::DayOfWeek if self.peek_name("L") => {
                self.pos += 1;
                return self.special(Special::LastOfWeekday(self.domain.canonical(start)));
            }
            Field::DayOfWeek if self.peek() == Some(&TokenKind::Hash) => {
                self.pos += 1;
                let (n, column) = self.number()?;
                if !(1..=5).contains(&n) {
                    return Err(self.error(column, FieldProblem::InvalidOccurrence(n)));
                }
                return self.special(Special::NthWeekday(self.domain.canonical(start), n));
            }
            _ => {}
        }
        if self.eat(&TokenKind::Dash) {
            let end_tok = self.expect_token()?;
            let end = self.value(&end_tok)?;
            let step = if self.eat(&TokenKind::Slash) {
                self.step()?
            } else {
                1
            };
            self.push_span(start, end, step, tok.column)
        } else if self.eat(&TokenKind::Slash) {
            let step = self.step()?;
            self.push_range(start, self.domain.cycle_max.max(start), step);
            Ok(())
        } else {
            self.out.values.push(self.domain.canonical(start));
            Ok(())
        }
    }

    /// Records a special item; none of them accept a step.
    fn special(&mut self, special: Special) -> Result<()> {
        if let Some(TokenKind::Slash) = self.peek() {
            let column = self.tokens[self.pos].column;
            return Err(self.error(
                column,
                FieldProblem::NotAllowed("a step after 'L', 'W' or '#'"),
            ));
        }
        self.out.specials.push(special);
        Ok(())
    }

    fn number(&mut self) -> Result<(u32, usize)> {
        let tok = self.expect_token()?;
        match tok.kind {
            TokenKind::Number(n) => Ok((n, tok.column)),
            _ => Err(self.unexpected(&tok)),
        }
    }

    fn step(&mut self) -> Result<u32> {
        let (step, column) = self.number()?;
        let max = self.domain.span();
        if step == 0 || step > max {
            return Err(self.error(column, FieldProblem::InvalidStep { step, max }));
        }
        Ok(step)
    }

    /// A number or name, range-checked, in the field's written numbering.
    fn value(&self, tok: &Token) -> Result<u32> {
        match &tok.kind {
            TokenKind::Number(n) => {
                if (self.domain.min..=self.domain.max).contains(n) {
                    Ok(*n)
                } else {
                    Err(self.error(
                        tok.column,
                        FieldProblem::OutOfRange {
                            value: *n,
                            min: self.domain.min,
                            max: self.domain.max,
                        },
                    ))
                }
            }
            TokenKind::Name(n) => self.name_value(n, tok.column),
            _ => Err(self.unexpected(tok)),
        }
    }

    fn name_value(&self, name: &str, column: usize) -> Result<u32> {
        if let Some(i) = self.domain.names.iter().position(|n| *n == name) {
            return Ok(i as u32 + self.domain.name_base);
        }
        let problem = match name {
            "L" | "LW" => FieldProblem::NotAllowed("'L'"),
            "W" => FieldProblem::NotAllowed("'W'"),
            _ => FieldProblem::UnknownName(name.to_string()),
        };
        Err(self.error(column, problem))
    }

    /// `start-end/step`, wrapping around the cycle when `end < start`.
    fn push_span(&mut self, start: u32, end: u32, step: u32, column: usize) -> Result<()> {
        let d = *self.domain;
        // In the standard day-of-week field 7 is Sunday: `SAT-7` is a plain
        // range to the top, while `7-TUE` means `SUN-TUE`.
        let start = if start == 7 && end < 7 && d.cycle_max == 6 {
            0
        } else {
            start
        };
        if start <= end {
            self.push_range(start, end, step);
            return Ok(());
        }
        if d.field == Field::Year {
            return Err(self.error(column, FieldProblem::ReversedRange { start, end }));
        }
        let end = end.min(d.cycle_max);
        let len = (d.cycle_max - start + 1) + (end - d.min + 1);
        for k in (0..len).step_by(step as usize) {
            let mut v = start + k;
            if v > d.cycle_max {
                v -= d.span();
            }
            self.out.values.push(d.canonical(v));
        }
        Ok(())
    }

    fn push_range(&mut self, lo: u32, hi: u32, step: u32) {
        for v in (lo..=hi).step_by(step as usize) {
            self.out.values.push(self.domain.canonical(v));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::lexer::tokenize;
    use super::*;

    fn parse(text: &str, domain: &Domain) -> Result<ParsedField> {
        let tokens = tokenize(text, domain.field, 1)?;
        parse_field(&tokens, domain, text.chars().count() + 1)
    }

    fn values(text: &str, domain: &Domain) -> Vec<u32> {
        let mut v = parse(text, domain).unwrap().values;
        v.sort_unstable();
        v.dedup();
        v
    }

    #[test]
    fn stars_ranges_steps_and_lists() {
        assert_eq!(values("*", &Domain::HOUR), (0..24).collect::<Vec<_>>());
        assert_eq!(values("*/15", &Domain::MINUTE), vec![0, 15, 30, 45]);
        assert_eq!(values("10-50/20", &Domain::MINUTE), vec![10, 30, 50]);
        assert_eq!(values("5/20", &Domain::MINUTE), vec![5, 25, 45]);
        assert_eq!(
            values("1,3,5-7", &Domain::DAY_OF_MONTH),
            vec![1, 3, 5, 6, 7]
        );
        assert_eq!(values("*/5", &Domain::MONTH), vec![1, 6, 11]);
        assert_eq!(values("2030-2040/5", &Domain::YEAR), vec![2030, 2035, 2040]);
    }

    #[test]
    fn names_are_case_insensitive_and_mix_with_numbers() {
        assert_eq!(values("jan,Mar-MAY", &Domain::MONTH), vec![1, 3, 4, 5]);
        assert_eq!(values("mon-fri", &Domain::DAY_OF_WEEK), vec![1, 2, 3, 4, 5]);
        assert_eq!(values("MON-5", &Domain::DAY_OF_WEEK), vec![1, 2, 3, 4, 5]);
    }

    #[test]
    fn sunday_is_both_zero_and_seven() {
        assert_eq!(values("7", &Domain::DAY_OF_WEEK), vec![0]);
        assert_eq!(values("5-7", &Domain::DAY_OF_WEEK), vec![0, 5, 6]);
        assert_eq!(
            values("0-7", &Domain::DAY_OF_WEEK),
            (0..7).collect::<Vec<_>>()
        );
        assert_eq!(values("7-2", &Domain::DAY_OF_WEEK), vec![0, 1, 2]);
        assert_eq!(values("*/2", &Domain::DAY_OF_WEEK), vec![0, 2, 4, 6]);
    }

    #[test]
    fn quartz_weekdays_start_at_one_for_sunday() {
        let d = Domain::DAY_OF_WEEK_QUARTZ;
        assert_eq!(values("2-6", &d), vec![1, 2, 3, 4, 5]);
        assert_eq!(values("1", &d), vec![0]);
        assert_eq!(values("MON-FRI", &d), vec![1, 2, 3, 4, 5]);
        assert_eq!(values("1/2", &d), vec![0, 2, 4, 6]);
        assert!(parse("0", &d).is_err());
    }

    #[test]
    fn reversed_ranges_wrap_around_the_cycle() {
        assert_eq!(values("22-2", &Domain::HOUR), vec![0, 1, 2, 22, 23]);
        assert_eq!(values("NOV-FEB", &Domain::MONTH), vec![1, 2, 11, 12]);
        assert_eq!(values("FRI-MON", &Domain::DAY_OF_WEEK), vec![0, 1, 5, 6]);
        // Stepping continues across the wrap: 22, 0, 2.
        assert_eq!(values("22-3/2", &Domain::HOUR), vec![0, 2, 22]);
        // Sunday must not be visited twice when stepping over the wrap.
        assert_eq!(values("FRI-MON/2", &Domain::DAY_OF_WEEK), vec![0, 5]);
        assert_eq!(values("30-2", &Domain::DAY_OF_MONTH), vec![1, 2, 30, 31]);
    }

    #[test]
    fn year_ranges_do_not_wrap() {
        let err = parse("2030-2025", &Domain::YEAR).unwrap_err();
        assert_eq!(
            err.to_string(),
            "reversed range 2030-2025 in year field at column 1"
        );
    }

    #[test]
    fn day_of_month_specials() {
        let f = parse("1,L,L-3,15W,LW", &Domain::DAY_OF_MONTH).unwrap();
        assert_eq!(f.values, vec![1]);
        assert_eq!(
            f.specials,
            vec![
                Special::LastDay,
                Special::LastDayOffset(3),
                Special::NearestWeekday(15),
                Special::LastWeekday
            ]
        );
    }

    #[test]
    fn day_of_week_specials() {
        let f = parse("FRIL,5L,MON#2,1#5", &Domain::DAY_OF_WEEK).unwrap();
        assert!(f.values.is_empty());
        assert_eq!(
            f.specials,
            vec![
                Special::LastOfWeekday(5),
                Special::LastOfWeekday(5),
                Special::NthWeekday(1, 2),
                Special::NthWeekday(1, 5)
            ]
        );
        let f = parse("7L,SUN#1", &Domain::DAY_OF_WEEK).unwrap();
        assert_eq!(
            f.specials,
            vec![Special::LastOfWeekday(0), Special::NthWeekday(0, 1)]
        );
        let f = parse("6L", &Domain::DAY_OF_WEEK_QUARTZ).unwrap();
        assert_eq!(f.specials, vec![Special::LastOfWeekday(5)]);
    }

    #[test]
    fn errors_point_at_the_offending_token() {
        let cases: [(&str, Domain, &str); 16] = [
            (
                "61",
                Domain::MINUTE,
                "invalid value 61 in minute field (0-59) at column 1",
            ),
            (
                "1-61",
                Domain::MINUTE,
                "invalid value 61 in minute field (0-59) at column 3",
            ),
            (
                "*/0",
                Domain::MINUTE,
                "invalid step 0 in minute field (1-60) at column 3",
            ),
            (
                "*/25",
                Domain::HOUR,
                "invalid step 25 in hour field (1-24) at column 3",
            ),
            (
                "1,,2",
                Domain::HOUR,
                "empty list item in hour field at column 3",
            ),
            (
                "1,",
                Domain::HOUR,
                "empty list item in hour field at column 2",
            ),
            (
                "5-",
                Domain::HOUR,
                "unexpected end of hour field at column 3",
            ),
            (
                "L",
                Domain::HOUR,
                "'L' is not allowed in the hour field at column 1",
            ),
            (
                "5W",
                Domain::DAY_OF_WEEK,
                "'W' is not allowed in the day-of-week field at column 2",
            ),
            (
                "MON#6",
                Domain::DAY_OF_WEEK,
                "invalid occurrence #6 in day-of-week field (1-5) at column 5",
            ),
            (
                "1#2",
                Domain::HOUR,
                "'#' is not allowed in the hour field at column 2",
            ),
            (
                "?",
                Domain::MINUTE,
                "'?' is not allowed in the minute field at column 1",
            ),
            (
                "FOO",
                Domain::MONTH,
                "unknown name \"FOO\" in month field at column 1",
            ),
            (
                "L",
                Domain::DAY_OF_WEEK,
                "'L' without a weekday is not allowed in the day-of-week field at column 1",
            ),
            (
                "L/2",
                Domain::DAY_OF_MONTH,
                "a step after 'L', 'W' or '#' is not allowed in the day-of-month field at column 2",
            ),
            (
                "L-31",
                Domain::DAY_OF_MONTH,
                "invalid value 31 in day-of-month field (1-30) at column 3",
            ),
        ];
        for (text, domain, expected) in cases {
            assert_eq!(
                parse(text, &domain).unwrap_err().to_string(),
                expected,
                "{text}"
            );
        }
    }

    #[test]
    fn nearest_weekday_rejects_day_zero_and_names() {
        assert!(parse("0W", &Domain::DAY_OF_MONTH).is_err());
        assert!(parse("32W", &Domain::DAY_OF_MONTH).is_err());
        assert!(parse("JANW", &Domain::DAY_OF_MONTH).is_err());
    }
}
