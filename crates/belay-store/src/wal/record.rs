//! Framing of individual log records; the layout is specified in
//! `crate::format`.

use belay_common::hash::{crc32c, crc32c_mask, crc32c_unmask};

use crate::batch::MAX_BATCH_LEN;

/// Bytes before a record's payload.
pub(crate) const RECORD_HEADER_LEN: usize = 25;

/// The only record kind so far: a write batch.
pub(crate) const KIND_BATCH: u8 = 1;
