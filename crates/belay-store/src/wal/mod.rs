//! The write-ahead log: segment files of checksummed batch records.
//!
//! The byte layout, the writer's invariants and the rules that tell a torn
//! tail from corruption are specified in `crate::format`; this module
//! implements them. `record` frames and parses single records, `segment`
//! writes segments and reads them back, classifying damage.

mod record;
mod segment;

pub(crate) use record::{Record, RECORD_HEADER_LEN};
pub(crate) use segment::{
    parse_segment_header, Event, RecordReader, SegmentWriter, SEGMENT_HEADER_LEN,
};
