//! Time zones: UTC, fixed offsets and POSIX rules, with an embedded table of
//! IANA names.
//!
//! A [`Tz`] converts between Unix seconds and wall-clock [`LocalDateTime`]s.
//! Only the *current* rule of a zone is modelled (see the [`zones`] table),
//! which is what a scheduler firing jobs from now on needs; past rule changes
//! are not.
//!
//! Local readings around a transition fall into three cases, reported by
//! [`Tz::from_local`] as a [`LocalResult`]: a normal reading maps to one
//! instant, a reading inside a backward jump (autumn) maps to two, and a
//! reading inside a forward jump (spring) maps to none.

mod posix;

pub(crate) mod zones;

use std::borrow::Cow;
use std::fmt;
use std::str::FromStr;

use crate::civil::{LocalDateTime, MAX_NAIVE, MIN_NAIVE};
use crate::error::{CronError, Result};

pub use posix::PosixTz;
use posix::MAX_OFFSET;
