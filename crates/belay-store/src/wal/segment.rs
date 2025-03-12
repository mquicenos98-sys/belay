//! Segment headers, the segment writer, and a reader that classifies
//! damage, following the rules in `crate::format`.

use std::io::Write;
use std::path::{Path, PathBuf};

use belay_common::hash::{crc32c, crc32c_mask, crc32c_unmask};

use super::record::{encode_record, find_next_record, parse_record, Record};
use crate::error::{io_err, Result};
use crate::files::segment_name;
use crate::vfs::{Vfs, VfsFile};

/// Size of the segment header.
pub(crate) const SEGMENT_HEADER_LEN: usize = 24;

const MAGIC: &[u8; 8] = b"BELAYWAL";

const VERSION: u16 = 1;
