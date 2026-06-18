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
