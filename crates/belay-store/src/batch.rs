//! Atomic groups of writes and their log encoding (specified in
//! `crate::format`, under "Write batch payload").

use std::ops::Bound;

use belay_common::codec::{DecodeError, Decoder, Encoder};

use crate::error::{Result, StoreError};
use crate::keys::prefix_end;

/// Largest accepted key, in bytes.
pub const MAX_KEY_LEN: usize = 64 * 1024;

/// Largest accepted value, in bytes.
pub const MAX_VALUE_LEN: usize = 32 * 1024 * 1024;

/// Largest accepted encoded batch, in bytes. This also bounds the length
/// of a log record, which lets recovery reject absurd lengths read from a
/// damaged header without trying to allocate them.
pub const MAX_BATCH_LEN: usize = 128 * 1024 * 1024;

const TAG_PUT: u8 = 1;

const TAG_DELETE: u8 = 2;

const TAG_DELETE_RANGE: u8 = 3;
