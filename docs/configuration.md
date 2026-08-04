# Configuration

`belay server` reads one TOML file. Every setting has a default, so an
empty file (or no file at all) is a valid configuration: a server on
`127.0.0.1:7700` without authentication, keeping its data in
`./belay-data`. `belay dev` ignores configuration files altogether and
runs the defaults with in-memory storage.

The file is named with `--config` (`-c`), or the `BELAY_CONFIG`
environment variable when the flag is absent. Any setting can then be
overridden by an environment variable such as
`BELAY_SERVER__LISTEN=0.0.0.0:7700` (see
[Environment overrides](#environment-overrides)).

```sh
belay config default > belay.toml   # a commented example of every section
belay config check belay.toml       # would the server start with this?
belay config print belay.toml       # every setting, defaults included
belay server --config belay.toml
```

[`examples/belay.toml`](../examples/belay.toml) is a complete
single-node production configuration, and
[`examples/cluster/`](../examples/cluster/) holds one for each node of a
three-node cluster.

## Syntax and value formats

The file is TOML 1.0: comments, bare, quoted and dotted keys, all string
forms, integers with `_` separators and `0x`/`0o`/`0b` prefixes, floats,
booleans, arrays, inline tables, `[tables]` and `[[arrays.of.tables]]`.
Duplicate keys and table redefinitions are errors, as TOML says.

Values belay interprets beyond plain TOML types:

| kind | written as | examples |
|---|---|---|
| duration | a string of `<number><unit>` parts, largest unit first; units `ns`, `us` (`µs`), `ms`, `s`, `m`, `h`, `d`, `w`; a bare number is seconds; an integer is also taken as seconds | `"30s"`, `"1h30m"`, `"250ms"`, `"1.5h"`, `"90"`, `90` |
| byte size | an integer, or a string with a unit: `k`/`kib`, `m`/`mib`, `g`/`gib`, `t`/`tib` (powers of 1024) or `kb`, `mb`, `gb`, `tb` (powers of 1000), any case | `4096`, `"64MiB"`, `"1GB"`, `"1.5 GiB"` |
| address | `host:port`; the host is an IP literal or a name made of letters, digits, `-`, `.` and `_`, checked but not resolved when the file is read | `"0.0.0.0:7700"`, `"[::1]:7700"`, `"belay-1.internal:7700"` |
| backoff policy | the compact text form (see below) | `"exp:1s*2..10m~full"` |
| time | RFC 3339 with an offset, as a TOML date-time or a string | `2026-11-01T00:00:00-05:00` |

Backoff policies describe the delay before each retry:

```text
const:5s                 5s every time
linear:1s+2s..1m         1s, 3s, 5s, ... capped at 1m
exp:1s*2..10m            1s, 2s, 4s, ... capped at 10m
exp:500ms*1.5            uncapped (the ..max part is optional)
exp:1s*2..10m~full       with jitter: ~full, ~equal, ~none or ~<p>%
```

`constant:` and `exponential:` are accepted as long spellings.

The tables for queues, schedules and calendars (`[queues.<name>]`,
`[[schedules]]`, `[[calendars]]`) take exactly the fields of the JSON
bodies the HTTP API accepts for the same objects, decoded and validated by
the same code, so a queue configured in the file and one configured with
`PUT /v1/queues/:name/config` behave the same. Like the API, they reject
unknown fields. Unknown keys anywhere else are warnings (see
[Validation](#validation-errors-and-warnings)).

## [server]

How the HTTP API is served.

| key | type | default | meaning |
|---|---|---|---|
| `listen` | address | `"127.0.0.1:7700"` | Address the API listens on. Anything but a loopback address (`127.0.0.0/8`, `::1`, `localhost`) requires at least one `[[auth.tokens]]` entry or `auth.disabled = true`. |
| `threads` | integer | `16` | Checked (at least 1, at most `max_connections`) but not used by the server, which serves each connection on its own thread; see `max_connections`. |
| `max_connections` | integer | `1024` | Most open connections, and so most connection threads. Must be at least 1 and at least `threads`. A connection beyond it gets an immediate plain-text `503` with `Retry-After: 1`; once more than seven eighths are in use, responses stop offering keep-alive. |
| `max_body` | byte size | `"4MiB"` | Largest request body. Bodies up to twice the limit (and at least 64 KiB over it) are read and refused with a JSON `413`; larger declared bodies are refused with a plain-text `413` before they are read. Must be positive. |
| `keep_alive` | duration | `"1m"` | How long an idle keep-alive connection stays open. |
| `request_timeout` | duration | `"30s"` | Budget for receiving a request's head, and the longest silence while receiving its body. Expiry answers `408`. |
| `shutdown_grace` | duration | `"30s"` | How long a graceful shutdown waits for requests in flight before stopping the engine anyway. |

Every long-polling lease and every open event stream holds a connection
(and its thread) for as long as it waits, so size `max_connections` for
the number of worker slots polling at once, plus event subscribers, plus
ordinary traffic. See [operations.md](operations.md#capacity).

## [storage]

Where and how jobs, schedules and workflows are kept. The storage engine
itself is described in [storage.md](storage.md).

| key | type | default | meaning |
|---|---|---|---|
| `dir` | string | `"belay-data"` | Data directory, relative to the working directory unless absolute. Created if missing. Must not be empty. |
| `in_memory` | boolean | `false` | Keep everything in memory; it is lost when the process exits. `dir` is then ignored. |
| `sync` | string | `"always"` | When the write-ahead log reaches the disk: `always`, `interval:<duration>` (for example `interval:10ms`; must be positive) or `never`. Case-insensitive. |
| `checkpoint_bytes` | byte size | `"128MiB"` | Log size on disk that triggers a checkpoint (a snapshot, after which the covered log is deleted). `0` disables size-triggered checkpoints. |
| `checkpoint_interval` | duration | `"1h"` | Longest time between checkpoints while anything is written. `0` disables time-triggered checkpoints. Bounds recovery time for stores that grow too slowly to reach `checkpoint_bytes`. |
| `segment_size` | byte size | `"32MiB"` | Size at which a log segment file is sealed and a new one started. Between 128 bytes and 1 GiB. |
| `recovery` | string | `"strict"` | What opening does about damage a crash cannot explain: `strict` refuses to start and names the file and offset; `skip-corrupt` (or `skip_corrupt`) recovers everything around it, logs what it skipped, and writes a fresh checkpoint. |

What each sync policy loses:

| `sync` | a write is acknowledged after | lost on power loss or kernel crash |
|---|---|---|
| `always` | the log is synced | nothing acknowledged |
| `interval:<d>` | the batch is handed to the operating system | at most the writes acknowledged in the last `<d>` |
| `never` | the batch is handed to the operating system | anything since the last segment roll, checkpoint or clean shutdown |

A process crash with the machine still running loses nothing under any
policy, and recovery always restores the state after some prefix of the
acknowledged writes, never one with a write missing in the middle.
[operations.md](operations.md#choosing-a-sync-policy) discusses the
choice.

## [auth]

Bearer tokens and the scopes they grant.

| key | type | default | meaning |
|---|---|---|---|
| `disabled` | boolean | `false` | Serve without authentication even if tokens are configured, and allow a non-loopback `server.listen` without tokens. For development. |
| `tokens` | array of tables | none | The accepted tokens, written as `[[auth.tokens]]` entries. |

Authentication is enforced as soon as one token is configured (and
`disabled` is not set). Without tokens every caller is allowed
everything, which is only accepted on a loopback address.

Each `[[auth.tokens]]` entry:

| key | type | required | meaning |
|---|---|---|---|
| `name` | string | yes | 1 to 64 characters from letters, digits, `-`, `_` and `.`; unique. Appears in the access log, in `403` messages and as the `token` label of `belay_http_rate_limited_total`. |
| `token` | string | one of the two | The secret in the clear; at least 16 characters. |
| `token_sha256` | string | one of the two | The SHA-256 of the secret as 64 hexadecimal digits, which keeps the secret out of the file. |
| `scopes` | array of strings | yes | At least one of `admin`, `enqueue`, `work`, `read`. |

Give each token either `token` or `token_sha256`, not both. Two tokens
may not share a name or a secret. Only the digest is kept in memory,
presented tokens are hashed and compared with every configured digest in
constant time, and `belay config print` writes every token as
`token_sha256`.

```toml
[[auth.tokens]]
name = "app"
token_sha256 = "88643a677da2afbd7b70760293508d708f646cb7fe22c5edd0a8be6625664a7e"  # gitleaks:allow
scopes = ["enqueue", "read"]
```

To produce a secret and its digest:

```sh
SECRET=$(openssl rand -hex 32)
printf %s "$SECRET" | sha256sum    # hash exactly the characters of the token
```

What the scopes allow (`admin` allows everything):

| scope | allows |
|---|---|
| `read` | every `GET` except `/v1/admin/*`: jobs, queues, schedules, calendars, workflows, workers, events, cluster members, `/v1/info` and `/metrics` |
| `enqueue` | `POST /v1/jobs`, `POST /v1/jobs/:id/cancel`, `POST /v1/workflows`, `POST /v1/workflows/:id/cancel` |
| `work` | the worker protocol: `POST /v1/lease` and `POST /v1/jobs/:id/heartbeat`, `/complete`, `/fail` |
| `admin` | everything else: deleting, retrying and promoting jobs, every queue, schedule and calendar change, `/v1/admin/*` |

`/healthz` and `/readyz` never need a token. The full route table is in
[http-api.md](http-api.md#authentication-and-scopes). Each token is also
rate limited (1000 requests per second, bursts of 2000) while
authentication is enforced.
