# Storage

belay keeps jobs, queues, schedules, calendars and workflow runs in its own
key-value store, the `belay-store` crate. It is a sorted, byte-oriented
store with atomic batch writes, made durable by a write-ahead log of
checksummed records and periodic snapshots. This document describes the
data model, the on-disk format, the durability guarantees of each sync
policy, what recovery does after a crash, and the tools for looking at a
data directory.

The format reference lives in the crate itself (`belay_store::format`);
everything below agrees with it.

## Data model

Keys and values are byte strings. Keys sort bytewise. Every write is a
`WriteBatch`, applied atomically under the next sequence number, starting
at 1: after a crash either all of a batch is visible or none of it, and
readers never observe half a batch.

The durable store, `LogKv`, holds the whole dataset in a `BTreeMap` in
memory. That is a deliberate choice: belay stores job metadata, queue
configuration and schedules, which are small, so reads never touch the
disk. Durability comes from the log and the snapshots described below.
`MemKv` is the same table without any files, used by tests and by
`belay dev` unless it is given `--data`.

The rest of belay keeps its records under a few prefixes. The queue engine
builds keys with the order-preserving encoding of `belay_store::keys`;
the scheduler uses readable text prefixes. Values are compact JSON.

| prefix | key | value | written by |
|---|---|---|---|
| `job` | job id (16-byte ULID, so keys sort by creation) | the job document | queue engine |
| `queue` | queue name | `{"config": {...}, "paused": false}` | queue engine |
| `uniq` | queue name, uniqueness key | `{"job": ..., "until": ..., "holder_state": ...}` | queue engine |
| `qmeta` | `schema` | record format version (currently 1) | queue engine |
| `sched/` | schedule id | the schedule with its history | scheduler |
| `cal/` | calendar name | the calendar spec | scheduler |
| `wf/` | run id | the run with its step failure messages | workflow engine |
| `wfr/` | run id `/` step | a step result that a placeholder reads | workflow engine |

The in-memory indexes of the queue (ready sets, timers, lease expiries,
counts) are not stored: they are rebuilt from the job documents when the
engine opens, so there is no secondary index that could drift from the
data.

Composite keys use the encoding below, whose bytewise order equals the
element-wise order of the tuple:

| element | encoding |
|---|---|
| `u8`, `u16`, `u32`, `u64` | big-endian, fixed width |
| `i32`, `i64` | big-endian with the sign bit flipped |
| `u64_desc` | big-endian of `!v` (reversed order) |
| `uvarint` | one length byte `n` (0-8), then `n` significant bytes big-endian |
| `bool` | `0x00` or `0x01` |
| `bytes`, `str` | `0x00` escaped as `0x00 0xFF`, terminated by `0x00 0x01` |
| `timestamp` | as `i64` milliseconds |
| `ulid` | 16 bytes big-endian |

The two-byte terminator makes the encoding prefix-free, so `"job"` and
`"jobs"` never overlap. This is why a queue record shows up in a log dump
as `queue\x00\x01emails\x00\x01`.

## The Kv trait

Everything above the store programs against one trait, shared between
threads as `Arc<dyn Kv>`:

```rust
pub trait Kv: Send + Sync + fmt::Debug {
    fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>>;
    fn scan(&self, range: KeyRange) -> Result<Vec<(Vec<u8>, Vec<u8>)>>;
    fn write(&self, batch: WriteBatch) -> Result<u64>;
    fn flush(&self) -> Result<()>;
    fn stats(&self) -> KvStats;
    fn checkpoint(&self) -> Result<CheckpointInfo>;

    // Provided: scan_keys, count, contains, put, delete, last_seq.
}
```

- `write` applies a batch atomically and returns its sequence number. An
  empty batch writes nothing and returns the latest sequence number. A
  write is visible to every thread once `write` returns.
- `scan` returns the entries of a `KeyRange` in key order, or descending
  when the range is reversed, as a consistent snapshot.
- `flush` makes every write that has returned durable, whatever the sync
  policy.
- `checkpoint` writes a snapshot and discards the log it covers; it is a
  no-op for `MemKv`.

A `WriteBatch` holds operations applied in order, a later operation on
the same key winning:

```rust
let mut batch = WriteBatch::new();
batch.put("job/1", "ready")
    .delete("job/0")
    .delete_range("tmp/a", "tmp/m")   // every key in [start, end)
    .delete_from("zz")                // every key >= start
    .delete_prefix("cache/");         // every key starting with the prefix
let seq = kv.write(batch)?;
```

A `KeyRange` is built with `KeyRange::all()`, `KeyRange::prefix(p)`,
`KeyRange::new(a..b)` or `KeyRange::from_bounds(start, end)`, and refined
with `.limit(n)`, `.reverse()` and `.after(last_key)`. Scans return whole
vectors, so large ranges are read a page at a time by passing the last key
of one page to `after` for the next.

Size limits are checked before anything is written:

| limit | value |
|---|---|
| `MAX_KEY_LEN` | 64 KiB |
| `MAX_VALUE_LEN` | 32 MiB |
| `MAX_BATCH_LEN` (encoded batch, and the longest log record a reader accepts) | 128 MiB |

The job layer has much tighter limits of its own (a 1 MiB payload, for
instance); see [queue.md](queue.md).

## The write-ahead log

The log is a series of segment files named `wal-<first seq>.log`. Integers
are little-endian. A *varint* is unsigned LEB128 in its minimal encoding
(non-minimal encodings are rejected), and `bytes` means a varint length
followed by that many bytes.

Every checksum is CRC-32C (Castagnoli, reflected polynomial `0x82F63B78`)
stored *masked*, as LevelDB does, so that a CRC over data containing CRCs
does not degenerate:

```text
masked = ((crc >> 15) | (crc << 17)) + 0xA282EAD8   (32-bit wrapping)
```

A segment is a 24-byte header followed by records.

| offset | size | field | contents |
|---|---|---|---|
| 0 | 8 | `magic` | `BELAYWAL` |
| 8 | 2 | `version` | 1 |
| 10 | 2 | `flags` | 0 (reserved) |
| 12 | 8 | `first_seq` | sequence number of the first record; equals the number in the file name |
| 20 | 4 | `header_crc` | masked CRC-32C of bytes 0..20 |

Each record is a 25-byte header followed by its payload.

| offset | size | field | contents |
|---|---|---|---|
| 0 | 4 | `crc` | masked CRC-32C of bytes 4..25+len |
| 4 | 4 | `len` | payload length, at most `MAX_BATCH_LEN` |
| 8 | 8 | `seq` | sequence number of the batch |
| 16 | 8 | `synced_to` | offset in this segment up to which the file was known to be synced when the record was written |
| 24 | 1 | `kind` | 1 = write batch |
| 25 | len | payload | the encoded batch |

The payload of a write batch:

```text
batch := count:varint op{count}
op    := 0x01 key:bytes value:bytes     put
       | 0x02 key:bytes                 delete
       | 0x03 start:bytes end:bytes     delete every key in [start, end)
       | 0x04 start:bytes               delete every key >= start
```

An empty or inverted range deletes nothing.

The writer keeps four invariants that recovery relies on:

- records carry consecutive sequence numbers starting at `first_seq`, and
  each segment starts where the previous one ended;
- a segment is created with its header synced and its directory entry
  synced before any record in it is acknowledged;
- a segment is synced completely ("sealed") before the next one is
  created, under every sync policy;
- `synced_to` never exceeds the record's own offset.

The active segment is sealed and a new one started once it holds at least
`storage.segment_size` bytes (32 MiB by default). A single large batch may
overshoot that size.

## Group commit and sync policies

Writers encode their batch outside any lock, take a sequence number and
join a queue. A writer that finds no leader active becomes the leader: it
takes the whole queue, frames every batch into one buffer, appends it with
a single write, syncs once if the policy says so, applies the batches to
the in-memory table and wakes everyone. Batches arriving meanwhile queue up
for the next leader, so under load one sync covers many writes. Only one
leader runs at a time, which keeps records in sequence order. The
`commit_groups` statistic counts these log writes; `batches /
commit_groups` is the average group size.

A batch becomes visible to readers only once it has been written (and,
under `always`, synced), so a reader never sees data a crash could take
back.

`storage.sync` chooses when the log reaches stable storage:

| policy | `write` returns after | lost on power loss |
|---|---|---|
| `always` (default) | the batch is synced | nothing acknowledged |
| `interval:<d>`, e.g. `interval:10ms` | the batch is handed to the OS | at most the batches acknowledged in the last `d` plus one sync |
| `never` | the batch is handed to the OS | anything since the last segment roll, checkpoint, `flush` or close |

Under `interval:<d>` a background thread (`belay-store`) syncs the active
segment every `d`. Under `never` the log is synced only when a segment is
sealed, on checkpoints, on `flush` and on close.

Two guarantees hold under every policy:

- recovery restores the state after some *prefix* of the acknowledged
  batches, never a state with a batch missing in the middle;
- a process crash that leaves the operating system running loses nothing
  that was acknowledged, since the data is already in the OS cache.

On disk, file syncs use `sync_data` (fdatasync), which also persists the
file length. Directory syncs are real on Unix and a no-op on Windows,
where NTFS journals metadata changes itself.

If a log write or sync fails, the store stops accepting writes and every
later write fails with "store stopped accepting writes after a failure
(...); reopen it to recover". Retrying after a failed fsync could report
success for data the kernel has already dropped, so belay does not try.
Reopening recovers from whatever reached the disk; a batch whose write
failed may or may not survive that reopen. The queue engine reports these
errors to clients as `503 unavailable`.

## Snapshots and checkpoints

A checkpoint writes a snapshot of the whole table and deletes the log
segments it covers, which bounds both the size of the log and the time
recovery takes. It runs in three phases:

1. **Freeze** (the log lock is held, writers wait): seal the active
   segment and start `wal-<seq+1>.log`, then copy the in-memory table.
2. **Write** (no store locks held, writers proceed): stream the copy to
   `snap-<seq>.db.tmp`, sync it, rename it to `snap-<seq>.db`, sync the
   directory.
3. **Trim**: delete the segments sealed in phase 1 and the previous
   snapshot.

Writers therefore stall for one sync, one file creation and a copy of the
table, not for the snapshot write. The copy doubles the memory held for
the duration of the checkpoint.

A snapshot file has this layout:

```text
header, 28 bytes
  0   8  magic     "BELAYSNP"
  8   2  version   1
 10   2  flags     0 (reserved)
 12   8  seq       sequence number of the last batch reflected
 20   8  count     number of entries
entries, count times, keys strictly ascending
         key:bytes value:bytes
trailer, 20 bytes
  0   8  body_len  length of header + entries
  8   4  crc       masked CRC-32C of header + entries
 12   8  magic     "BELAYEND"
```

A snapshot under its final name is complete, because it only gets that
name by an atomic rename after a sync; the trailer checksum guards against
damage at rest. Reading one validates the trailer, the checksum, the header
and the key order before anything is used.

Checkpoints are triggered three ways:

| trigger | setting | default |
|---|---|---|
| log size: the segments on disk add up to this many bytes (on the store's background thread) | `storage.checkpoint_bytes` | 128 MiB; 0 disables |
| time: at least this often while anything is written (checked by the engine's housekeeping thread every `metrics.refresh`) | `storage.checkpoint_interval` | 1h; 0 disables |
| by hand | `belay admin checkpoint`, `POST /v1/admin/checkpoint` | |

A checkpoint with nothing written since the previous one does nothing.
Keep `checkpoint_bytes` well above `segment_size`: the log is only ever
discarded a whole segment at a time.

A crash at any point leaves a recoverable directory. Before the rename,
the previous snapshot plus the full log still describe the state (the
leftover `.tmp` file is deleted on open); after it, the new snapshot does,
and segments it covers are recognised by their sequence numbers and
deleted on open.

## Recovery

Opening a store recovers its state:

1. Delete `*.tmp` snapshots.
2. Load the newest snapshot that validates. Invalid ones are skipped and,
   once recovery succeeds, renamed to `*.corrupt`.
3. Skip the segments wholly covered by the snapshot (every segment before
   the last one whose `first_seq <= snapshot_seq + 1`) and delete them.
4. Replay the rest in order. Records at or below `snapshot_seq` are
   skipped; every other record must carry the next sequence number.
5. Classify each region that fails to parse (bad checksum, truncated
   record, bad header).
6. Sync the last segment, which becomes sealed; the new active segment is
   `wal-<last_seq+1>.log`.

Step 5 is where a crash is told apart from damage:

| where the region is | classification | what happens |
|---|---|---|
| in a sealed segment (any but the last) | corruption | error in strict mode |
| in the last segment, and a later valid record has `synced_to` beyond the damaged offset | corruption: the damaged bytes had been synced before that record was written | error in strict mode |
| in the last segment otherwise | torn tail | the segment is truncated at the damage (removed if the damage is in its header), a warning is logged and the event is counted in `RecoveryStats` |

A torn tail is the normal result of a crash in the middle of a write, and
is always truncated. The `synced_to` field is what lets a torn tail hold
intact records after a damaged one, as happens when the operating system
writes dirty pages back out of order, without mistaking it for corruption.
For example, suppose one group wrote records at offsets 24 and 100 and
synced the file up to offset 180, and the next group wrote records at 180
and 260, both carrying `synced_to = 180`:

```text
offset  24        100       180       260
        [ rec 7 ][ rec 8 ][ rec 9 ][ rec 10 ]
        |---- synced ----|   synced_to = 180 in rec 9 and rec 10
```

Damage at offset 120 is corruption, because rec 9 proves the file had been
synced past it. Damage at offset 200 is a torn tail: nothing proves it had
ever reached the disk, so the segment is truncated at 200 and the store
recovers through rec 8.

The limit of the rule: damage confined to the final record of the log
cannot be told apart from a torn write, so it is truncated (with the
warning) even if that record had been synced.

Corruption, a sequence gap, a record that fails to decode although its
checksum matches, and a bad snapshot whose sequence number is beyond what
the log restores are errors under `storage.recovery = "strict"`, the
default. The server then refuses to start, and apart from truncating a torn
tail the directory is left unchanged. With `storage.recovery =
"skip-corrupt"` (also spelled `skip_corrupt`) those problems are skipped
with a warning, everything else is recovered, and the store immediately
writes a checkpoint so the damage is not met again.

What recovery did is reported in `RecoveryStats`, visible under
`recovery` in `belay admin store`:

| field | meaning |
|---|---|
| `snapshot_seq`, `snapshot_keys` | the snapshot recovery started from (0 if none) |
| `bad_snapshots` | snapshots passed over because they failed validation |
| `segments_replayed`, `batches_replayed` | log read and applied |
| `torn_tails`, `torn_bytes` | torn tails truncated (0 or 1) and the bytes removed |
| `skipped_regions`, `skipped_bytes` | damaged regions skipped under `skip-corrupt` |
| `removed_files` | leftover temporary snapshots, superseded snapshots and covered segments deleted |
| `duration` | wall-clock time recovery took |
| `lost_data` | whether anything was discarded or skipped |

## Directory layout and the LOCK file

```text
LOCK                               exists while a store has the directory open;
                                   holds the owner's process id
wal-00000000000000000001.log       log segment; the number is the sequence
                                   number of its first record
snap-00000000000000000742.db       snapshot of the state after batch 742
snap-00000000000000000742.db.tmp   snapshot being written; deleted on open
snap-00000000000000000742.db.corrupt
                                   snapshot that failed validation, set aside
```

Numbers are zero-padded to 20 digits so that names sort numerically. Other
files are ignored.

The `LOCK` file keeps a second process, or a second handle in the same
process, from opening the directory. It is created exclusively and holds
the owner's process id; closing the store removes it. A lock left behind
by a crashed process is detected as stale only on Linux, where a lock whose
process id has no `/proc` entry is taken over. Elsewhere the error names
the holder, and the file must be deleted by hand once that process is
known to be gone:

```text
store is locked by process 4127 (/var/lib/belay/LOCK); if that process is gone, delete the file
```

## Configuration

The `[storage]` section of the configuration file:

```toml
[storage]
dir = "belay-data"          # relative to the working directory
in_memory = false           # keep everything in memory; lost on exit
sync = "always"             # "always", "interval:10ms" or "never"
checkpoint_bytes = "128MiB" # 0 disables size-triggered checkpoints
checkpoint_interval = "1h"  # 0 disables time-triggered checkpoints
segment_size = "32MiB"      # between 128 bytes and 1 GiB
recovery = "strict"         # or "skip-corrupt"
```

Sizes are written as integers or strings such as `"64MiB"` or `"1GB"`;
durations as `"30s"` or `"1h30m"`. Every key can be overridden from the
environment, for example `BELAY_STORAGE__SYNC=interval:10ms` or
`BELAY_STORAGE__DIR=/var/lib/belay`.

`belay dev` keeps its data in memory unless given `--data DIR`.

From Rust, `LogKv::open` takes an `Options` builder with the same knobs:

```rust
use belay_store::{LogKv, Options, RecoveryMode, SyncPolicy};
use std::time::Duration;

let kv = LogKv::open(
    "/var/lib/belay",
    Options::new()
        .sync(SyncPolicy::Interval(Duration::from_millis(10)))
        .segment_size(8 << 20)
        .checkpoint_every(64 << 20)   // or .manual_checkpoints()
        .recovery(RecoveryMode::Strict)
        .create_if_missing(true),
)?;
```

## Testing crashes with MemFs

All file access goes through the `Vfs` trait, which covers the dozen
operations the store performs. `OsFs` is the real file system; `MemFs`
keeps files in memory and models what survives a crash. Each file keeps
its contents and the length known to be synced, and directory entries are
tracked both as visible and as of the last `sync_dir` of their directory.

A simulated crash is one of:

| `Crash` | what survives |
|---|---|
| `PowerLoss` | synced file data and synced directory entries only |
| `ProcessExit` | everything written, synced or not (the OS keeps running) |
| `TornWrite { path, keep }` | like `PowerLoss`, plus the first `keep` unsynced bytes of `path` |

A file created or deleted without a directory sync therefore disappears or
comes back after a power loss, as it can on a real disk.

```rust
use belay_store::vfs::{Crash, MemFs, Vfs};
use std::io::Write;
use std::path::Path;

let fs = MemFs::new();
fs.create_dir_all(Path::new("db")).unwrap();
let path = Path::new("db").join("log");
let mut f = fs.create(&path).unwrap();
f.write_all(b"durable").unwrap();
f.sync().unwrap();
f.write_all(b" and not").unwrap();
fs.sync_dir(Path::new("db")).unwrap();

let after = fs.crash_copy(&Crash::PowerLoss);
assert_eq!(after.read(&path).unwrap(), b"durable");
let torn = fs.crash_copy(&Crash::TornWrite { path: path.clone(), keep: 4 });
assert_eq!(torn.read(&path).unwrap(), b"durable and");
```

`crash` applies a crash in place: clones of the `MemFs` taken before it
(the ones handed to a store) belong to the "dead process", and their file
handles, locks and writes fail from then on. `crash_copy` returns an
independent copy of what a crash would leave, which lets a test try many
crash points against one history. Faults are injected with `fail_syncs`
(every sync fails) and `fail_after_ops(Some(n))` (every mutating operation
after the next `n` fails), and `overwrite` flips bytes in place, synced
region included, to model bit rot.

The store's crash tests use these to check, among other things, that every
torn offset of an unsynced tail recovers an exact prefix of the batches,
that a crash after any single I/O operation keeps every acknowledged
write, that any damaged byte in a sealed segment is reported, and that a
batch whose sync failed is all or nothing.

## Inspecting a store

`belay admin store-inspect <DIR>` reads a data directory directly, without
taking the lock or changing a file, so it also works on a store that is
open elsewhere (whose last segment may then show a torn tail that is just
a write in progress). It checks every snapshot and record checksum and the
continuity of sequence numbers:

```console
$ belay admin store-inspect /var/lib/belay
FILE                          SEQS     ENTRIES     SIZE  NOTE
snap-00000000000000000034.db  ..=34         18  7.8 KiB
wal-00000000000000000035.log  35..=35        1    589 B
snapshot seq 34, recoverable through seq 35
no problems found
```

`--records` also lists every log record with its operations, and
`--json` prints the report as JSON (`dir`, `ok`, `locked`,
`snapshot_seq`, `last_seq`, `files`, `problems`):

```console
$ belay admin store-inspect /var/lib/belay --records
...
wal-00000000000000000035.log:
  seq 35 at 24 (565 bytes, synced to 24)
    put "queue\x00\x01emails\x00\x01" (172 bytes)
    put "job\x00\x01\x01\xa0\xefJ\xa5k_R\x00Eg=\xc0\xed\xa7[" (323 bytes)
```

Problems come in four kinds: `torn tail`, `corrupt`, `gap` and `bad
snapshot`. A torn tail alone is harmless, since opening the store truncates
it, and the command exits with status 0:

```console
$ belay admin store-inspect /var/lib/belay
FILE                          SEQS   ENTRIES     SIZE  NOTE
snap-00000000000000000034.db  ..=34       18  7.8 KiB
wal-00000000000000000035.log               0    400 B
snapshot seq 34, recoverable through seq 34
torn tail: /var/lib/belay/wal-00000000000000000035.log at offset 24: record of 540 bytes extends 189 bytes past the end of the file (376 bytes)
```

Anything else means a strict open would refuse the directory, and the
command exits with status 1:

```console
$ belay admin store-inspect /var/lib/belay
FILE                          SEQS     ENTRIES     SIZE  NOTE
snap-00000000000000000034.db  ..=34             7.8 KiB  invalid
wal-00000000000000000035.log  35..=35        1    589 B
snapshot seq 0, recoverable through seq 35
bad snapshot: /var/lib/belay/snap-00000000000000000034.db at offset 0: corrupt /var/lib/belay/snap-00000000000000000034.db at offset 8005: checksum mismatch (stored 14815b13, computed c7499d97)
gap: /var/lib/belay/wal-00000000000000000035.log at offset 0: batches 1 to 34 are missing
error: /var/lib/belay has 2 problems that opening it would not survive
hint: storage.recovery = "skip-corrupt" opens it anyway, dropping the damaged records
```

Against a running server, `belay admin store` shows the store's
statistics (`GET /v1/admin/store`): keys, live bytes, the last and the
durable sequence number, batches, commit groups, log syncs and bytes
written since open, log segments and their size, checkpoints, the newest
snapshot, and the recovery statistics. `belay admin checkpoint` writes a
checkpoint now. Both need a token with the `admin` scope when
authentication is on. (`belay admin verify` is a different check: it
compares the queue's in-memory indexes with the stored job documents.)

The same inspection is available from Rust in `belay_store::inspect`:
`list_files`, `verify` (returning a `VerifyReport`), `read_segment` and
`read_snapshot`.

## Limitations

- The whole dataset lives in memory, twice over while a checkpoint copies
  the table. Job payloads are limited to 1 MiB each for this reason, and
  retention matters: finished jobs stay in memory until they are deleted.
- One process per directory. A lock left by a crashed process is detected
  as stale only on Linux; elsewhere the `LOCK` file must be deleted by
  hand.
- Scans return materialized vectors; large ranges have to be paged with
  `limit` and `after`.
- Damage confined to the very last record of the log looks exactly like a
  torn write and is truncated with a warning rather than reported.
- After a failed log write or sync the store refuses further writes until
  it is reopened, which for a server means a restart.
- Opening replays the log since the last snapshot and the queue engine
  then scans every job, so start-up time grows with the log size and the
  number of retained jobs.
- No compression, no encryption, no replication. A node's data exists only
  in its own data directory; see [cluster.md](cluster.md).
