//! Rebuilding the state from the newest valid snapshot and the log,
//! following the procedure in `crate::format` ("Recovery").
//!
//! Nothing is modified until the whole log has been read, except for the
//! truncation of a torn tail, which is always the last step of reading.
//! A strict recovery that fails leaves the directory unchanged apart from
//! that truncation, for `belay admin store-inspect` or a retry with
//! `RecoveryMode::SkipCorrupt`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Instant;

use belay_common::{info, warn};

use super::options::RecoveryMode;
use crate::batch::WriteBatch;
use crate::error::{io_err, Result, StoreError};
use crate::files;
use crate::kv::RecoveryStats;
use crate::snapshot;
use crate::table::Table;
use crate::vfs::Vfs;
use crate::wal::{parse_segment_header, Event, Record, RecordReader, SEGMENT_HEADER_LEN};

/// The outcome of a successful recovery.
#[derive(Debug)]
pub(super) struct Recovered {
    pub(super) table: Table,
    /// Sequence number of the last batch restored.
    pub(super) last_seq: u64,
    /// Sequence number and size of the snapshot recovery started from.
    pub(super) snapshot_seq: u64,
    pub(super) snapshot_bytes: u64,
    /// Log segments left on disk, by first sequence number, with sizes.
    pub(super) segments: BTreeMap<u64, u64>,
    pub(super) stats: RecoveryStats,
}

struct Recovery<'a> {
    fs: &'a dyn Vfs,
    mode: RecoveryMode,
    stats: RecoveryStats,
}

impl Recovery<'_> {
    /// Reports corruption: an error in strict mode, a warning (and a
    /// skipped region of `skipped` bytes) otherwise.
    fn corruption(&mut self, file: &Path, offset: u64, reason: String, skipped: u64) -> Result<()> {
        match self.mode {
            RecoveryMode::Strict => Err(StoreError::corrupt(file, offset, reason)),
            RecoveryMode::SkipCorrupt => {
                warn!(
                    "skipping corrupt data in {}", file.display();
                    offset, reason = %reason, bytes = skipped
                );
                self.stats.skipped_regions += 1;
                self.stats.skipped_bytes += skipped;
                Ok(())
            }
        }
    }

    fn truncate_torn_tail(
        &mut self,
        path: &Path,
        offset: usize,
        len: usize,
        reason: &str,
    ) -> Result<()> {
        warn!(
            "truncating torn log tail in {}", path.display();
            offset, bytes = len - offset, reason = %reason
        );
        self.stats.torn_tails += 1;
        self.stats.torn_bytes += (len - offset) as u64;
        if offset == 0 {
            // Not even the header made it: the crash hit while the segment
            // was being created, before any record in it was acknowledged.
            self.fs.remove(path).map_err(io_err("removing", path))
        } else {
            self.fs
                .truncate(path, offset as u64)
                .map_err(io_err("truncating", path))
        }
    }

    /// Applies one valid record, checking that sequence numbers continue
    /// where the previous record left off.
    fn apply(
        &mut self,
        path: &Path,
        rec: Record<'_>,
        snapshot_seq: u64,
        expected: &mut u64,
        table: &mut Table,
    ) -> Result<()> {
        let len = (rec.end() - rec.offset) as u64;
        if rec.seq <= snapshot_seq {
            return Ok(());
        }
        if rec.seq < *expected {
            return self.corruption(
                path,
                rec.offset as u64,
                format!(
                    "batch {} appears again after batch {}",
                    rec.seq,
                    *expected - 1
                ),
                len,
            );
        }
        if rec.seq > *expected {
            self.corruption(
                path,
                rec.offset as u64,
                format!("batches {} to {} are missing", *expected, rec.seq - 1),
                0,
            )?;
            *expected = rec.seq;
        }
        match WriteBatch::decode(rec.payload) {
            Ok(batch) => {
                table.apply(batch);
                self.stats.batches_replayed += 1;
            }
            // The checksum matched, so this is not damage at rest but bad
            // data that was written that way.
            Err(e) => self.corruption(
                path,
                rec.offset as u64,
                format!("batch {} does not decode: {e}", rec.seq),
                len,
            )?,
        }
        *expected = rec.seq + 1;
        Ok(())
    }

    /// Replays one segment. Returns its size afterwards, or `None` if it
    /// was removed.
    fn replay_segment(
        &mut self,
        path: &Path,
        first: u64,
        is_last: bool,
        snapshot_seq: u64,
        expected: &mut u64,
        table: &mut Table,
    ) -> Result<Option<u64>> {
        let buf = self.fs.read(path).map_err(io_err("reading", path))?;
        self.stats.segments_replayed += 1;
        let before_first = first.saturating_sub(1);
        let mut start = SEGMENT_HEADER_LEN;
        match parse_segment_header(&buf) {
            Ok(seq) if seq == first => {}
            Ok(seq) => {
                self.corruption(
                    path,
                    12,
                    format!("segment header says first seq {seq}, file name says {first}"),
                    buf.len() as u64,
                )?;
                return Ok(Some(buf.len() as u64));
            }
            Err(reason) => {
                let damage = RecordReader::new(&buf, 0, before_first).damage_at(0, reason);
                if is_last && !damage.proven {
                    self.truncate_torn_tail(path, 0, buf.len(), &damage.reason)?;
                    return Ok(None);
                }
                self.corruption(
                    path,
                    0,
                    damage.reason.clone(),
                    damage.len_in(buf.len()) as u64,
                )?;
                start = damage.resume.unwrap_or(buf.len());
            }
        }

        let mut end = buf.len();
        for event in RecordReader::new(&buf, start, before_first) {
            match event {
                Event::Record(rec) => self.apply(path, rec, snapshot_seq, expected, table)?,
                Event::Damage(d) if is_last && !d.proven => {
                    self.truncate_torn_tail(path, d.offset, buf.len(), &d.reason)?;
                    end = d.offset;
                    break;
                }
                Event::Damage(d) => {
                    let why = if d.proven {
                        "a later record shows it had been synced"
                    } else {
                        "the segment had been sealed"
                    };
                    let skipped = d.len_in(buf.len()) as u64;
                    self.corruption(
                        path,
                        d.offset as u64,
                        format!("{} ({why})", d.reason),
                        skipped,
                    )?;
                }
            }
        }
        if is_last && end == buf.len() {
            // What was read may only exist in the OS cache (after a process
            // crash); make it durable before the segment counts as sealed.
            let mut file = self.fs.open_append(path).map_err(io_err("opening", path))?;
            file.sync().map_err(io_err("syncing", path))?;
        }
        Ok(Some(end as u64))
    }

    fn remove_leftover(&mut self, path: &Path, what: &str) {
        match self.fs.remove(path) {
            Ok(()) => self.stats.removed_files += 1,
            Err(e) => warn!("cannot remove {what} {}: {e}", path.display()),
        }
    }
}

/// Recovers the store in `dir`, which the caller has locked.
pub(super) fn recover(fs: &dyn Vfs, dir: &Path, mode: RecoveryMode) -> Result<Recovered> {
    let started = Instant::now();
    let listing = files::list(fs, dir)?;
    let mut r = Recovery {
        fs,
        mode,
        stats: RecoveryStats::default(),
    };

    // 1. The newest snapshot that validates.
    let mut bad_snapshots: Vec<(u64, PathBuf)> = Vec::new();
    let mut first_bad: Option<StoreError> = None;
    let mut base = None;
    for (seq, name) in listing.snapshots.iter().rev() {
        let path = dir.join(name);
        let loaded = snapshot::read(fs, &path).and_then(|snap| {
            if snap.seq == *seq {
                Ok(snap)
            } else {
                Err(StoreError::corrupt(
                    &path,
                    12,
                    format!(
                        "snapshot header says seq {}, file name says {seq}",
                        snap.seq
                    ),
                ))
            }
        });
        match loaded {
            Ok(snap) => {
                let bytes = fs.file_len(&path).map_err(io_err("reading", &path))?;
                base = Some((snap, bytes));
                break;
            }
            Err(e) if e.is_corruption() => {
                warn!("snapshot failed validation, trying an older one"; error = %e);
                r.stats.bad_snapshots += 1;
                bad_snapshots.push((*seq, path));
                first_bad.get_or_insert(e);
            }
            Err(e) => return Err(e),
        }
    }
    let (mut table, snapshot_seq, snapshot_bytes) = match base {
        Some((snap, bytes)) => (Table::from_map(snap.map), snap.seq, bytes),
        None => (Table::new(), 0, 0),
    };
    r.stats.snapshot_seq = snapshot_seq;
    r.stats.snapshot_keys = table.len() as u64;

    // 2. The log, from the segment holding `snapshot_seq + 1` on.
    let segs = &listing.segments;
    let start = segs
        .iter()
        .rposition(|(first, _)| *first <= snapshot_seq + 1)
        .unwrap_or(0);
    let mut expected = snapshot_seq + 1;
    let mut segments = BTreeMap::new();
    for (i, (first, name)) in segs.iter().enumerate().skip(start) {
        let path = dir.join(name);
        if *first > expected {
            let reason = format!(
                "log gap: batches {expected} to {} are missing before this segment",
                first - 1
            );
            if mode == RecoveryMode::Strict {
                // A bad snapshot is the likelier root cause of a gap.
                return Err(first_bad.unwrap_or_else(|| StoreError::corrupt(&path, 0, reason)));
            }
            r.corruption(&path, 0, reason, 0)?;
            expected = *first;
        }
        let is_last = i + 1 == segs.len();
        if let Some(size) = r.replay_segment(
            &path,
            *first,
            is_last,
            snapshot_seq,
            &mut expected,
            &mut table,
        )? {
            segments.insert(*first, size);
        }
    }
    let last_seq = expected - 1;

    // 3. A snapshot that failed validation proves the store once reached
    //    its sequence number.
    if let Some(&(bad_seq, ref bad_path)) = bad_snapshots.iter().max_by_key(|(seq, _)| *seq) {
        if bad_seq > last_seq {
            if mode == RecoveryMode::Strict {
                return Err(first_bad.expect("recorded with the bad snapshot"));
            }
            let reason = format!(
                "state recovered only up to batch {last_seq}; this snapshot covered up to {bad_seq}"
            );
            r.corruption(bad_path, 0, reason, 0)?;
        }
    }

    // 4. Clean up.
    let mut changed = false;
    for name in &listing.temps {
        r.remove_leftover(&dir.join(name), "temporary snapshot");
        changed = true;
    }
    for (_, name) in &segs[..start] {
        r.remove_leftover(&dir.join(name), "covered log segment");
        changed = true;
    }
    for (seq, name) in &listing.snapshots {
        let is_bad = bad_snapshots.iter().any(|(bad, _)| bad == seq);
        if *seq < snapshot_seq && !is_bad {
            r.remove_leftover(&dir.join(name), "superseded snapshot");
            changed = true;
        }
    }
    for (_, path) in &bad_snapshots {
        // Kept for inspection, under a name recovery ignores.
        let mut aside = path.clone().into_os_string();
        aside.push(".corrupt");
        let aside = PathBuf::from(aside);
        match fs.rename(path, &aside) {
            Ok(()) => changed = true,
            Err(e) => warn!("cannot move corrupt snapshot {} aside: {e}", path.display()),
        }
    }
    if changed {
        fs.sync_dir(dir).map_err(io_err("syncing directory", dir))?;
    }

    r.stats.duration = started.elapsed();
    info!(
        "recovered store in {}", dir.display();
        seq = last_seq,
        keys = table.len(),
        snapshot_seq,
        replayed = r.stats.batches_replayed,
        elapsed = r.stats.duration
    );
    Ok(Recovered {
        table,
        last_seq,
        snapshot_seq,
        snapshot_bytes,
        segments,
        stats: r.stats,
    })
}
