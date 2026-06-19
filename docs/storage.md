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
