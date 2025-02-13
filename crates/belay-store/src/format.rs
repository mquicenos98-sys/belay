//! The on-disk format of a [`LogKv`](crate::LogKv) directory, and the
//! rules recovery applies to it.
//!
//! This module holds no code; it is the reference for the files the store
//! writes. Integers are little-endian. *Varints* are unsigned LEB128 in
//! their minimal encoding (non-minimal encodings are rejected), and
//! `bytes` means a varint length followed by that many bytes.
//!
//! # Checksums
//!
//! Every checksum is CRC-32C (Castagnoli, reflected polynomial
//! `0x82F63B78`, as computed by `belay_common::hash::crc32c`), stored
//! *masked* the way LevelDB does it, so that a CRC over data that itself
//! contains CRCs does not degenerate:
//!
//! ```text
//! masked = ((crc >> 15) | (crc << 17)) + 0xA282EAD8   (32-bit wrapping)
//! ```
//!
//! # Directory layout
//!
//! ```text
//! LOCK                               exists while a store has the directory open;
//!                                    holds the owner's process id
//! wal-00000000000000000001.log       log segment; the number is the sequence
//!                                    number of its first record
//! snap-00000000000000000742.db       snapshot of the state after batch 742
//! snap-00000000000000000742.db.tmp   snapshot being written; deleted on open
//! snap-00000000000000000742.db.corrupt
//!                                    snapshot that failed validation, set aside
//! ```
//!
//! Numbers are zero-padded to 20 digits so names sort numerically. Other
//! files are ignored.
//!
//! # Log segments (version 1)
//!
//! A segment is a header followed by records.
//!
//! ```text
//! segment header, 24 bytes
//!   0   8  magic       "BELAYWAL"
//!   8   2  version     1
//!  10   2  flags       0 (reserved)
//!  12   8  first_seq   sequence number of the first record; equals the
//!                      number in the file name
//!  20   4  header_crc  masked CRC-32C of bytes 0..20
//!
//! record, 25-byte header + payload
//!   0   4  crc         masked CRC-32C of bytes 4..25+len
//!   4   4  len         payload length, at most MAX_BATCH_LEN
//!   8   8  seq         sequence number of the batch
//!  16   8  synced_to   offset in this segment up to which the file was known
//!                      to be synced when this record was written
//!  24   1  kind        1 = write batch
//!  25 len  payload
//! ```
//!
//! Invariants the writer maintains:
//!
//! - Records carry consecutive sequence numbers starting at `first_seq`,
//!   and each segment starts where the previous one ended.
//! - A segment is created with its header synced and its directory entry
//!   synced before any record in it is acknowledged.
//! - A segment is synced completely ("sealed") before the next one is
//!   created, under every sync policy.
//! - `synced_to` never exceeds the record's own offset.
//!
//! # Write batch payload
//!
//! ```text
//! batch := count:varint op{count}
//! op    := 0x01 key:bytes value:bytes     put
//!        | 0x02 key:bytes                 delete
//!        | 0x03 start:bytes end:bytes     delete every key in [start, end)
//!        | 0x04 start:bytes               delete every key >= start
//! ```
//!
//! Operations apply in order; a later operation on the same key wins. An
//! empty or inverted range deletes nothing.
//!
//! # Snapshots (version 1)
//!
//! ```text
//! header, 28 bytes
//!   0   8  magic     "BELAYSNP"
//!   8   2  version   1
//!  10   2  flags     0 (reserved)
//!  12   8  seq       sequence number of the last batch reflected
//!  20   8  count     number of entries
//! entries, count times, keys strictly ascending
//!          key:bytes value:bytes
//! trailer, 20 bytes
//!   0   8  body_len  length of header + entries
//!   8   4  crc       masked CRC-32C of header + entries
//!  12   8  magic     "BELAYEND"
//! ```
//!
//! A snapshot is written as `snap-<seq>.db.tmp`, synced, renamed to
//! `snap-<seq>.db`, and the directory synced. A snapshot under its final
//! name is therefore complete; the trailer guards against damage at rest.
//!
//! # Checkpoints
//!
//! 1. With the log locked: seal the active segment and start
//!    `wal-<seq+1>.log`, then copy the in-memory table. Writers wait for
//!    this step only.
//! 2. Write `snap-<seq>.db` as above.
//! 3. Delete every segment sealed in step 1 and the previous snapshot.
//!
//! A crash before step 2's rename leaves the previous snapshot and the
//! full log (plus possibly a `.tmp` file, deleted on open); a crash after
//! it leaves the new snapshot, and segments it covers are deleted on open.
//!
//! # Recovery
//!
//! 1. Delete `*.tmp` snapshots.
//! 2. Load the newest snapshot that validates. Invalid ones are skipped
//!    and, once recovery succeeds, renamed to `*.corrupt`.
//! 3. Skip segments wholly covered by the snapshot (every segment before
//!    the last one whose `first_seq <= snapshot_seq + 1`) and delete them.
//! 4. Replay the rest in order. Records at or below `snapshot_seq` are
//!    skipped; every other record must carry the next sequence number.
//! 5. Classify each region that fails to parse (bad checksum, truncated
//!    record, bad header):
//!    - in a sealed segment (any but the last): **corruption**;
//!    - in the last segment, when some later valid record in it has
//!      `synced_to` beyond the damaged offset: **corruption**, because the
//!      damaged bytes had been synced before that record was written;
//!    - otherwise: a **torn tail**. The segment is truncated at the damage
//!      (removed, if the damage is in its header), a warning is logged and
//!      the event is counted in `RecoveryStats`.
//!
//!    Corruption, a sequence gap, a record that fails to decode, and a bad
//!    snapshot whose sequence number is beyond what the log restores are
//!    errors in `RecoveryMode::Strict`. `RecoveryMode::SkipCorrupt`
//!    skips them with a warning, and the store then writes a checkpoint so
//!    they are not met again.
//! 6. Sync the last segment, which becomes sealed: the new active segment
//!    is `wal-<last_seq+1>.log`.
//!
//! The `synced_to` rule is what allows a torn tail to hold intact records
//! after a damaged one, as happens when the operating system writes dirty
//! pages back out of order, without mistaking it for corruption. Its
//! limit: damage confined to the final record of the log cannot be told
//! apart from a torn write, so it is truncated (with the warning) even if
//! that record had been synced.
//!
//! # Limits
//!
//! Keys up to [`MAX_KEY_LEN`](crate::MAX_KEY_LEN) bytes, values up to
//! [`MAX_VALUE_LEN`](crate::MAX_VALUE_LEN), encoded batches up to
//! [`MAX_BATCH_LEN`](crate::MAX_BATCH_LEN), which also bounds the record
//! length a reader accepts.
