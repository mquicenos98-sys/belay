# HTTP API reference

belay serves a JSON API over HTTP/1.1, version 1, under `/v1`. The
`belay` command, the Rust client (`belay-client`) and the worker
runtime all speak it; anything that can send HTTP requests can too.

The examples use curl against a server whose address and token are in
the environment:

```sh
export BELAY_URL=http://127.0.0.1:7700
export BELAY_TOKEN=...        # leave out the Authorization header without tokens
```

The server speaks plain `http://` only. Put a TLS-terminating proxy in
front of it when clients cross an untrusted network.

## Conventions

**Bodies** are JSON objects, sent and received compact. Request bodies
are parsed as JSON whatever `Content-Type` says, so `curl -d '{...}'`
works; an empty body where one is required is `400` with the message
`request body is empty; expected a JSON object`. Responses carry
`Content-Type: application/json`, except the probes and `/metrics`
(plain text) and the event stream (`text/event-stream`).

**Field names** are `snake_case`. Optional fields that are unset, and
empty lists and maps, are left out of responses; plain scalars
(`priority`, `paused`) are always written. In requests, a missing field
and `null` both mean "not set", except for handler results, where `null`
is a value.

**Timestamps** are RFC 3339 strings. The server writes UTC with
milliseconds (`"2026-09-29T22:21:13.931Z"`) and accepts any offset
(`"2026-09-30T02:30:00-05:00"`), with 0 to 9 fraction digits.

**Durations** are strings in belay's compact form: `"30s"`, `"1h30m"`,
`"250ms"`, `"1.5h"`; units `ns`, `us`, `ms`, `s`, `m`, `h`, `d`, `w`,
largest first; a unitless string is seconds (`"90"`). JSON numbers are
refused as durations, because nobody can tell whether `30000` means
seconds or milliseconds.

**Enumerations** are lowercase strings: `"ready"`, `"fire_once"`,
`"until_finished"`. Variants with a parameter are written `name:param`:
`"for:1h"`, `"fire_all:10"`. **Backoff policies** use their text form:
`"exp:1s*2..10m~full"`, `"const:5s"`, `"linear:1s+2s..1m"`.

**Identifiers** are strings. Jobs and workflow runs have ULIDs (26
characters of Crockford base32, case-insensitive, time-ordered). Queue
names are `[a-z0-9][a-z0-9_.:-]{0,63}`; schedule ids and calendar names
are `[a-z0-9][a-z0-9_.-]{0,127}`; lease tokens are 16 hexadecimal digits.

**Strictness.** Request bodies reject unknown fields, naming the closest
known one. Every endpoint rejects query parameters it does not know
(`unknown query parameter "limti", did you mean "limit"?`), except the
probes and `/metrics`, which ignore them. Responses may gain fields in
later versions; clients should ignore fields they do not know.

**Request ids.** Every response carries `X-Request-Id`. A well-formed id
sent by the client (1 to 128 characters of `A-Z a-z 0-9 - _ . : / + = @`)
is kept, otherwise the server makes one up; it also appears in the
access log line of the request.

**HTTP details.** `HEAD` is answered for every `GET` route and `OPTIONS`
for every path, with `Allow`. A path that differs from a route only by a
trailing slash is answered `308` to the right one. Requests are refused
before they reach the API, with a plain-text answer, when they are
malformed (`400`, including a request that carries both
`Content-Length` and `Transfer-Encoding`), too slow (`408`), have a
request line over 8 KiB (`414`) or a head over 32 KiB or 100 fields
(`431`), or declare a body over twice `server.max_body` (`413`).

## Errors

Every failure answered by the API has an error body and the HTTP status
its code maps to:

```json
{"error": {"code": "not_found", "message": "job 01M3QM3YTBDKTE6E25JG340DVZ not found", "details": {"resource": "job", "id": "01M3QM3YTBDKTE6E25JG340DVZ"}}}
```

`code` is what clients branch on; `message` is for people and may change;
`details` is optional structured context.

| code | status | meaning |
|---|---|---|
| `invalid_argument` | 400 | The request is malformed or breaks a rule. Do not retry it unchanged. |
| `unauthenticated` | 401 | No token, an unknown token, or not a `Bearer` token. |
| `permission_denied` | 403 | The token lacks the scope the route needs. |
| `not_found` | 404 | The job, queue, schedule, calendar, workflow run or route does not exist. |
| `already_exists` | 409 | Creating something that exists: a schedule id that is taken, or a uniqueness key held under `on_conflict: reject`. |
| `conflict` | 409 | A concurrent change got in the way. |
| `lease_lost` | 409 | The lease token is stale: the lease expired or the job went to another worker. The worker must abandon the job. |
| `failed_precondition` | 409 | The resource is not in a state that allows the operation, such as deleting a running job or a calendar in use. |
| `resource_exhausted` | 429 | The token is over its request rate. |
| `internal` | 500 | A bug or an unexpected failure; the server log has the details (look for the request id). |
| `unavailable` | 503 | The server cannot serve this now: it is shutting down, or its store refuses writes. Retry later. |

`429` and `503` answers carry `Retry-After` (in whole seconds, at least
1). The router's own refusals get error bodies too: an unknown path is
`404 not_found` (`no route matches GET /v1/nope`), a known path with the
wrong method is `405` with `Allow` and an `invalid_argument` body, and a
body over `server.max_body` is `413` with `invalid_argument` and
`details: {"limit": ..., "size": ...}`.

Invalid input lists every problem found, each with a JSON path:

```json
{"error": {"code": "invalid_argument",
  "message": "$.jobs[0].priority: must be between -1000 and 1000, got 1001",
  "details": {"violations": [{"field": "$.jobs[0].priority", "message": "must be between -1000 and 1000, got 1001"}]}}}
```

Other `details` shapes: `{"resource", "id"}` with `not_found` and
`already_exists`; `{"job"}` with `lease_lost`; `{"job", "state"}` or
`{"state", "requested"}` with `failed_precondition` on a job;
`{"retry_after"}` with `resource_exhausted`; `{"line", "column"}` for
JSON that does not parse.

## Pagination

Every listing returns one page:

```json
{"items": [...], "next_cursor": "05tg387f8b47bdd8vgt6yp73yv9tkdr"}
```

`next_cursor` is absent on the last page. Pass it back as `cursor` to get
the next page; `limit` sets the page size (default 100, 1 to 1000, other
values are `400`). Cursors are opaque, carry a checksum, and are only
valid for the listing that issued them; a truncated or edited one is
`400 invalid_argument`. Jobs are listed by id, newest first unless
`order=asc`; since ids start with their creation time, a cursor is a
stable position in creation order. Queues, schedules, calendars, workers
and cluster members are listed by name; workflow runs by id, newest
first.

```sh
curl -s -H "Authorization: Bearer $BELAY_TOKEN" "$BELAY_URL/v1/jobs?queue=emails&state=dead&limit=50"
curl -s -H "Authorization: Bearer $BELAY_TOKEN" "$BELAY_URL/v1/jobs?queue=emails&state=dead&limit=50&cursor=05tg387f8b47bdd8vgt6yp73yv9tkdr"
```

## Authentication and scopes

Authentication is enforced once `[[auth.tokens]]` are configured (and
`auth.disabled` is not set). Send the token as
`Authorization: Bearer <token>`. Tokens and their configuration are
described in [configuration.md](configuration.md#auth).

A missing or unknown token is `401 unauthenticated`, a token without the
route's scope `403 permission_denied`. Both carry an RFC 6750 challenge
that tells the cases apart:

```text
WWW-Authenticate: Bearer realm="belay"                                          (no token)
WWW-Authenticate: Bearer realm="belay", error="invalid_token"                   (unknown token)
WWW-Authenticate: Bearer realm="belay", error="insufficient_scope", scope="admin"
```

Every route and the scope it needs; `admin` grants every scope:

| route | scope |
|---|---|
| `GET /healthz`, `GET /readyz` | none |
| `GET /metrics`, `GET /v1/info` | `read` |
| `POST /v1/jobs` | `enqueue` |
| `GET /v1/jobs`, `GET /v1/jobs/:id` | `read` |
| `POST /v1/jobs/:id/cancel` | `enqueue` |
| `POST /v1/jobs/:id/retry`, `POST /v1/jobs/:id/promote`, `DELETE /v1/jobs/:id` | `admin` |
| `POST /v1/lease`, `POST /v1/jobs/:id/heartbeat`, `/complete`, `/fail` | `work` |
| `GET /v1/queues`, `GET /v1/queues/:name` | `read` |
| `PUT /v1/queues/:name/config`, `POST /v1/queues/:name/pause`, `/resume`, `/purge`, `DELETE /v1/queues/:name` | `admin` |
| `GET /v1/schedules`, `GET /v1/schedules/:id`, `GET /v1/schedules/:id/preview` | `read` |
| `POST /v1/schedules`, `PUT` and `DELETE /v1/schedules/:id`, `POST /v1/schedules/:id/pause`, `/resume`, `/trigger` | `admin` |
| `GET /v1/calendars`, `GET /v1/calendars/:name` | `read` |
| `PUT /v1/calendars/:name`, `DELETE /v1/calendars/:name` | `admin` |
| `POST /v1/workflows`, `POST /v1/workflows/:id/cancel` | `enqueue` |
| `GET /v1/workflows`, `GET /v1/workflows/:id` | `read` |
| `GET /v1/workers`, `GET /v1/events`, `GET /v1/cluster/members` | `read` |
| `POST /v1/admin/checkpoint`, `GET /v1/admin/store`, `GET /v1/admin/verify`, `POST /v1/admin/shutdown` | `admin` |

Producers may withdraw what they submitted (cancelling a job or a
workflow run) but not operate the system: retrying and promoting change
what runs when, and deleting erases history. Before 0.6.0, cancelling a
job needed `admin`.

Without tokens every request is served as the principal `anonymous` with
every scope.

## Rate limiting

While authentication is enforced, each token has a token bucket of 2000
requests refilled at 1000 per second (programs embedding `belay-server`
can change both, per token). A request over it is answered `429
resource_exhausted` with `Retry-After`, and counted in
`belay_http_rate_limited_total{token="..."}`. The limit is a guard
against runaway clients rather than a quota: one token is often shared
by a whole worker fleet, and every job costs at least two requests. An
open server (no tokens) is not rate limited.

## Clusters and redirects

In a cluster ([cluster.md](cluster.md)) every queue and every schedule
is owned by one live node. A request about a queue or schedule owned by
another node is answered `307 Temporary Redirect` with an absolute
`Location` on the owner, keeping the path and query:

```text
HTTP/1.1 307 Temporary Redirect
Location: http://10.0.0.11:7700/v1/jobs?queue=emails&state=dead

{"redirect":{"node":"belay-a","location":"http://10.0.0.11:7700/v1/jobs?queue=emails&state=dead"}}
```

A 307 keeps the method and body, so a client replays the request on the
owner with its own credentials; every node accepts the same tokens.
Nothing is proxied. Redirected are: `POST /v1/jobs`, `GET /v1/jobs` with
`queue=`, `POST /v1/lease`, every `/v1/queues/:name...` route,
`POST /v1/schedules` and every `/v1/schedules/:id...` route.

Everything else is served by the node asked: listings show what that
node holds, the event stream carries that node's events, and a job or
workflow run can only be reached on the node that stores it (a node that
does not have it says so in its `not_found` message). An enqueue batch
naming queues owned by different nodes is refused with `400`, listing the
owners in `details.owners`. A lease is served from the requested queues
this node owns; the others are named in the `Belay-Remote-Queues`
response header, and only when it owns none is the lease redirected, to
the owner of the first queue listed.

## Health, readiness and metrics

### GET /healthz

No authentication. `200` with the text `ok` as long as the process can
answer. Restart on this.

### GET /readyz

No authentication. `200` with `ready`, or `503 unavailable` while the
node should not get traffic: from the moment a shutdown begins
(`"shutting down"`) and, in a cluster, until the node has joined or its
`join_grace` has passed (`"not ready: waiting to join the cluster"`).
Route traffic on this.

### GET /metrics

Scope `read`. The Prometheus text format (version 0.0.4,
`Content-Type: text/plain; version=0.0.4; charset=utf-8`). The families
are listed in [metrics.md](metrics.md).

```sh
curl -s -H "Authorization: Bearer $BELAY_TOKEN" "$BELAY_URL/metrics" | grep ^belay_queue_jobs
```

### GET /v1/info

Scope `read`. The server's identity.

```json
{"version":"0.6.0","node":"belay-1","started_at":"2026-09-29T22:21:09.012Z","uptime":"4s876ms",
 "time":"2026-09-29T22:21:13.888Z","storage":"disk","cluster_size":1}
```

| field | meaning |
|---|---|
| `version` | belay version |
| `node` | node id (`cluster.node_id`) |
| `started_at`, `uptime` | when the engine started, and since how long |
| `time` | the server's clock; compare with yours to spot skew, which matters for `run_at` and lease deadlines |
| `storage` | `disk`, `memory` or `custom` (an embedded engine given its own store) |
| `cluster_size` | live members; 1 without clustering |

## Jobs

A job is submitted as a **job spec** and stored as a **job**.

Job spec fields (the body of `POST /v1/jobs`, a schedule's `template`, a
workflow step's `job`):

| field | type | default | meaning |
|---|---|---|---|
| `queue` | string | required | Queue to enqueue into; created on first use. |
| `kind` | string | required | Handler name workers dispatch on: 1 to 128 characters, a letter or digit then letters, digits, `_ . : / -`. |
| `payload` | any JSON | `null` | Handler input; at most 1 MiB as compact JSON. |
| `priority` | integer | `0` | `-1000` to `1000`; higher runs first. |
| `run_at` | timestamp | now | Do not run before this time. Exclusive with `delay`. |
| `delay` | duration | none | Do not run until this long after enqueue; at most `365d`. |
| `max_attempts` | integer | queue default | Total attempts including the first, 1 to 1000. |
| `retry` | backoff policy | queue default | Delay curve between attempts. |
| `timeout` | duration | queue default | Execution budget of one attempt, `1s` to `7d`. Leases never extend past it. |
| `unique` | object or string | none | Deduplication: `{"key": "user:42", "scope": "until_finished", "on_conflict": "return_existing"}`, or just the key as a string for those defaults. See below. |
| `tags` | array of strings | none | Up to 16 distinct labels of at most 64 characters, for filtering. |
| `tenant` | string | none | Owner for fair sharing within the queue. |
| `meta` | object of strings | none | Up to 32 annotations; keys up to 64 characters, values up to 1024 bytes. Keys starting with `belay.` are set by belay. |

`unique.scope` is how long the key stays claimed: `until_started`,
`until_finished` (default) or `for:<duration>` (`1s` to `30d` after
enqueue). `unique.on_conflict` is what an enqueue does when the key is
held in the same queue: `return_existing` (default; succeed without a new
job, returning the holder with status `duplicate`), `reject` (`409
already_exists`) or `replace` (cancel a holder that has not started and
enqueue the new job, status `replaced`; a running holder is kept, as with
`return_existing`).

A job adds its state and history:

| field | meaning |
|---|---|
| `id` | the job's ULID |
| `state` | `scheduled` (waiting for its run time), `ready`, `running`, `retrying` (waiting out a backoff delay), `succeeded`, `dead` (attempts exhausted or a non-retryable failure) or `cancelled` |
| `attempt` | attempts started so far: 0 until first leased |
| `spec` | the spec, with the queue's `max_attempts`, `retry` and `timeout` filled in |
| `created_at`, `updated_at` | when it was enqueued, and last changed |
| `run_at` | when it becomes (or became) eligible: the spec's run time, then the end of each backoff delay |
| `started_at`, `finished_at` | when the latest attempt started; when it reached a terminal state |
| `lease` | while running: `worker`, `acquired_at`, `expires_at`, `heartbeat_at` (the token only in lease responses) |
| `cancel_requested` | `true` while a running job has been asked to stop |
| `errors` | failed attempts, oldest first, at most 10: `at`, `attempt`, `message`, `kind`, `retryable`, `worker` |
| `result` | what the handler returned, once succeeded |
| `progress` | the running attempt's latest report: `percent` (0 to 100), `message` |

### POST /v1/jobs

Scope `enqueue`. Enqueues one job (the body is a spec) or several
(`{"jobs": [spec, ...]}`, 1 to 1000, all checked before any is
enqueued). The response has one result per spec, in order:

```sh
curl -s -i -H "Authorization: Bearer $BELAY_TOKEN" "$BELAY_URL/v1/jobs" \
  -d '{"queue": "emails", "kind": "send_welcome", "payload": {"user": 42}, "unique": "welcome:42"}'
```

```text
HTTP/1.1 201 Created
Location: /v1/jobs/01M3QM3YTBDKTE6E25JG340DV3

{"results":[{"id":"01M3QM3YTBDKTE6E25JG340DV3","status":"created","state":"ready"}]}
```

`status` is `created`, `duplicate` (the uniqueness key was held; `id` is
the holder) or `replaced`; `state` is the job's state after the call.
The answer is `201 Created` when at least one job was created (with
`Location` for a single spec) and `200 OK` when every spec matched an
existing job, so a retried enqueue with a uniqueness key is safe.

Other answers: `400` for invalid specs, `409 already_exists` under
`on_conflict: reject`, and in a cluster `307` to the queue's owner or
`400` for a batch spanning owners.

### GET /v1/jobs

Scope `read`. One page of jobs, newest first. Query parameters, all
optional and combined with AND:

| parameter | meaning |
|---|---|
| `queue` | only this queue (redirected to its owner in a cluster) |
| `state` | only these states, comma-separated or repeated: `state=dead,retrying` |
| `kind`, `tag`, `tenant` | only jobs with this kind, carrying this tag, of this tenant |
| `created_after`, `created_before` | RFC 3339 times, exclusive |
| `order` | `desc` (default) or `asc` |
| `cursor`, `limit` | see [Pagination](#pagination) |

```sh
curl -s -H "Authorization: Bearer $BELAY_TOKEN" "$BELAY_URL/v1/jobs?queue=emails&state=dead&limit=20"
```

### GET /v1/jobs/:id

Scope `read`. The job, or `404`. A malformed id is `400`.

```json
{"id":"01M3QM49XATSFSEX1EEWGPTZN3","state":"retrying","attempt":1,
 "spec":{"queue":"manual","kind":"resize","payload":{"img":1},"priority":0,"max_attempts":25,"retry":"exp:1s*2..1h~10%","timeout":"30m"},
 "created_at":"2026-09-29T22:21:25.290Z","updated_at":"2026-09-29T22:21:32.250Z","run_at":"2026-09-29T22:21:34.250Z",
 "started_at":"2026-09-29T22:21:25.376Z",
 "errors":[{"at":"2026-09-29T22:21:32.250Z","attempt":1,"message":"upstream returned 503","kind":"http_503","retryable":true,"worker":"w1"}]}
```

### POST /v1/jobs/:id/cancel

Scope `enqueue`. Cancels a job and answers `200` with it. Scheduled,
ready and retrying jobs are cancelled at once. A running job gets
`cancel_requested: true`; its worker learns this from its next heartbeat
and should stop and report with `fail`, which cancels it (as does its
lease running out). Cancelling a cancelled job changes nothing. A
succeeded or dead job is `409 failed_precondition`.

```sh
curl -s -X POST -H "Authorization: Bearer $BELAY_TOKEN" "$BELAY_URL/v1/jobs/01M3QM49XATSFSEX1EEWGPTZN4/cancel"
```

### POST /v1/jobs/:id/retry

Scope `admin`. Sends a `dead` or `cancelled` job back to `ready` with a
fresh budget of attempts (`attempt` restarts at 0, the error history is
kept); its uniqueness key is not claimed again. `200` with the job;
other states are `409 failed_precondition`.

### POST /v1/jobs/:id/promote

Scope `admin`. Makes a `scheduled` or `retrying` job ready now. `200`
with the job; promoting a ready job changes nothing; other states are
`409 failed_precondition`.

### DELETE /v1/jobs/:id

Scope `admin`. Deletes a finished (`succeeded`, `dead` or `cancelled`)
job ahead of its retention. `204 No Content`; a pending job is `409
failed_precondition` (cancel it first).

## The worker protocol

A worker leases jobs, keeps each lease alive with heartbeats while the
handler runs, and reports the outcome. Every call after the lease
presents the lease token; a stale one is `409 lease_lost`, which means
the job is no longer this worker's to report on: another worker may be
running it. The worker runtime (`belay-worker`, `belay worker`) does all
of this; see [workers.md](workers.md).

### POST /v1/lease

Scope `work`. Asks for up to `max_jobs` jobs from `queues`.

| field | type | default | meaning |
|---|---|---|---|
| `worker` | string | required | Worker id, 1 to 128 characters: a letter or digit, then letters, digits, `_ . : @ / -`. |
| `queues` | array | required | 1 to 32 distinct queue names. The server takes from them in turn (`engine.queue_order = "round_robin"`, the default) or strictly in the order listed (`"in_order"`). |
| `max_jobs` | integer | `1` | 1 to 100. |
| `lease_for` | duration | `"30s"` | Lease length, `1s` to `1h`, capped at each job's attempt deadline. |
| `wait` | duration | none | Long-poll for up to this long (at most `60s`) when nothing is ready. |

```sh
curl -s -H "Authorization: Bearer $BELAY_TOKEN" "$BELAY_URL/v1/lease" \
  -d '{"worker": "w1", "queues": ["manual"], "max_jobs": 1, "lease_for": "30s", "wait": "20s"}'
```

```json
{"jobs":[{"job":{"id":"01M3QM49XATSFSEX1EEWGPTZN3","state":"running","attempt":1,
  "spec":{"queue":"manual","kind":"resize","payload":{"img":1},"priority":0,"max_attempts":25,"retry":"exp:1s*2..1h~10%","timeout":"30m"},
  "created_at":"2026-09-29T22:21:25.290Z","updated_at":"2026-09-29T22:21:25.376Z","run_at":"2026-09-29T22:21:25.290Z",
  "started_at":"2026-09-29T22:21:25.376Z",
  "lease":{"worker":"w1","token":"8a2a6ef0e3cb9895","acquired_at":"2026-09-29T22:21:25.376Z","expires_at":"2026-09-29T22:21:55.376Z","heartbeat_at":"2026-09-29T22:21:25.376Z"}},
  "token":"8a2a6ef0e3cb9895"}]}
```

Jobs come highest priority first, then by `run_at`, from the queues that
are not paused and have room under their `max_concurrency` and
`rate_limit`, sharing between tenants. With `wait`, the request is held
until a job is ready or the wait runs out, and then answered with
whatever there is, possibly `{"jobs": []}`; a job enqueued meanwhile is
handed out at once. A server shutdown ends a wait early with no jobs.
The response is `200` either way. Each leased job starts an attempt:
`state` becomes `running` and `attempt` goes up by one.

The lease is also how the server knows about workers: `GET /v1/workers`
lists the ids it has heard from.

### POST /v1/jobs/:id/heartbeat

Scope `work`. Renews the lease and optionally records progress.

| field | type | meaning |
|---|---|---|
| `token` | string | the lease token (required) |
| `extend_by` | duration | new lease length from now, `1s` to `1h`; default the original `lease_for` |
| `progress` | object | `{"percent": 40, "message": "resizing"}`; percent 0 to 100, message at most 1024 bytes |

```sh
curl -s -H "Authorization: Bearer $BELAY_TOKEN" "$BELAY_URL/v1/jobs/01M3QM49XATSFSEX1EEWGPTZN3/heartbeat" \
  -d '{"token": "8a2a6ef0e3cb9895", "extend_by": "1m", "progress": {"percent": 40, "message": "resizing"}}'
```

```json
{"expires_at":"2026-09-29T22:22:32.076Z","cancel_requested":false}
```

A heartbeat never shortens a lease and never extends it past the
attempt's deadline (`started_at` plus the job's `timeout`). When
`cancel_requested` is `true`, stop the handler and report with `fail`.
Heartbeat well within the lease: the worker runtime sends one every third
of `lease_for`, which survives one lost heartbeat.

### POST /v1/jobs/:id/complete

Scope `work`. The attempt succeeded. Body `{"token": "...", "result":
...}`; `result` is any JSON up to 1 MiB and is optional (an explicit
`null` is stored as `null`). `204 No Content`; the job is `succeeded`.

```sh
curl -s -H "Authorization: Bearer $BELAY_TOKEN" "$BELAY_URL/v1/jobs/01M3QM49XATSFSEX1EEWGPTZN3/complete" \
  -d '{"token": "8a2a6ef0e3cb9895", "result": {"thumbnail": "s3://media/1-small.png"}}'
```

### POST /v1/jobs/:id/fail

Scope `work`. The attempt failed.

| field | type | default | meaning |
|---|---|---|---|
| `token` | string | required | the lease token |
| `error` | string | required | what went wrong; never refused for its length, truncated to 4096 bytes when recorded |
| `kind` | string | none | a machine-readable class such as `timeout` or `http_503`, at most 64 characters |
| `retryable` | boolean | `true` | whether another attempt may help |
| `retry_in` | duration | none | retry after exactly this long (at most `365d`) instead of the backoff policy's delay, for example from a `Retry-After` |

`204 No Content`. The job goes to `retrying` (with `run_at` at the end of
the delay) if the error is retryable and attempts remain, to `cancelled`
if cancellation was requested, and to `dead` otherwise.

```sh
curl -s -H "Authorization: Bearer $BELAY_TOKEN" "$BELAY_URL/v1/jobs/01M3QM49XATSFSEX1EEWGPTZN3/fail" \
  -d '{"token": "8a2a6ef0e3cb9895", "error": "upstream returned 503", "kind": "http_503", "retry_in": "2s"}'
```

A lease that runs out counts as a failed, retryable attempt: its error
is `lease expired` (kind `lease_expired`), or `timed out after <timeout>`
(kind `timeout`) when the attempt's deadline is what ended it. A
`job.expired` event is followed by `job.failed` or `job.dead`. Jobs are
therefore run at least once, and handlers should be idempotent.

## Queues

Queues exist from their first enqueue, configuration, pause or resume;
there is no separate "create". A queue's status:

```json
{"name":"emails","paused":false,
 "config":{"max_concurrency":16,"rate_limit":{"per_second":20.0,"burst":40},"default_timeout":"2m",
           "default_retry":"exp:30s*2..2h~full","default_max_attempts":15,
           "retention":{"succeeded":"1d","dead":"14d","cancelled":"1d"}},
 "counts":{"scheduled":0,"ready":3,"running":2,"retrying":1,"succeeded":120,"dead":0,"cancelled":0},
 "oldest_ready_age":"8s470ms",
 "throughput":{"window":"1m","enqueued":30,"succeeded":28,"failed":1,"dead":0}}
```

`oldest_ready_age` (absent when nothing is ready) is how long the oldest
ready job has been waiting: the best single signal that workers are
falling behind. `throughput` counts the last minute. The configuration
fields are those of `[queues.<name>]` in
[configuration.md](configuration.md#queuesname).

### GET /v1/queues

Scope `read`. One page of queue statuses, by name.

### GET /v1/queues/:name

Scope `read`. The queue's status, or `404` for a queue that does not
exist.

### PUT /v1/queues/:name/config

Scope `admin`. Replaces the queue's configuration (fields left out take
their defaults; `{}` is the default configuration) and answers `200` with
the queue's status. Creates the queue if needed. Jobs already enqueued
keep the defaults they were given.

```sh
curl -s -X PUT -H "Authorization: Bearer $BELAY_TOKEN" "$BELAY_URL/v1/queues/emails/config" \
  -d '{"max_concurrency": 8, "rate_limit": {"per_second": 10}, "retention": {"dead": "30d"}}'
```

### POST /v1/queues/:name/pause, /resume

Scope `admin`. Stops (or resumes) handing out jobs from the queue;
enqueueing continues while paused. `200` with the status.

### POST /v1/queues/:name/purge

Scope `admin`. Deletes the queue's jobs, only those in `?state=` if
given; running jobs are never purged. `200` with `{"purged": 12}`.

```sh
curl -s -X POST -H "Authorization: Bearer $BELAY_TOKEN" "$BELAY_URL/v1/queues/emails/purge?state=dead"
```

### DELETE /v1/queues/:name

Scope `admin`. Deletes the queue, its configuration and its jobs. A
queue with unfinished jobs is refused with `409 failed_precondition`
unless `?force=true` (also `?force` or `?force=1`). `200` with
`{"purged": n}`, the number of jobs deleted.

## Schedules

A schedule is a cron expression, a time zone and a job template. The
spec (the body of `POST` and `PUT`) has the fields of `[[schedules]]` in
[configuration.md](configuration.md#schedules); the semantics are in
[cron.md](cron.md) and [scheduling.md](scheduling.md). A stored schedule
adds its bookkeeping:

```json
{"spec":{"id":"nightly-report","cron":"30 2 * * *","timezone":"America/Bogota",
         "template":{"queue":"reports","kind":"build","payload":{"format":"pdf"},"priority":0},
         "misfire":"fire_once","overlap":"skip","jitter":"30s","paused":false,"description":"Daily sales report"},
 "next_fire_at":"2026-09-30T07:30:00.000Z","last_fire_at":"2026-09-29T07:30:00.000Z",
 "last_job":"01M3QM5C658EQHTT18FRYQK19S","fire_count":1,
 "created_at":"2026-09-28T22:21:53.829Z","updated_at":"2026-09-29T07:30:00.012Z"}
```

`next_fire_at` already accounts for the schedule's bounds and calendar
(but not for jitter), and is absent when the schedule will never fire
again. Jobs it creates carry `meta` entries `belay.schedule` (the id)
and `belay.fire_time` (the nominal fire time).

### GET /v1/schedules

Scope `read`. One page of schedules, by id.

### POST /v1/schedules

Scope `admin`. Creates a schedule: `201 Created` with the schedule and
`Location: /v1/schedules/<id>`. A taken id is `409 already_exists`; an
invalid expression or zone is `400` naming the field.

```sh
curl -s -H "Authorization: Bearer $BELAY_TOKEN" "$BELAY_URL/v1/schedules" -d '{
  "id": "nightly-report", "cron": "30 2 * * *", "timezone": "America/Bogota",
  "template": {"queue": "reports", "kind": "build", "payload": {"format": "pdf"}},
  "misfire": "fire_once", "overlap": "skip", "jitter": "30s"}'
```

### GET /v1/schedules/:id

Scope `read`. The schedule, or `404`.

### PUT /v1/schedules/:id

Scope `admin`. Creates or replaces the schedule: `201` (with `Location`)
when it was created, `200` when it replaced one. The body may leave out
`id`; if it names one, it must match the path.

### DELETE /v1/schedules/:id

Scope `admin`. Deletes the schedule and answers `200` with its last
state. Jobs it created stay.

### POST /v1/schedules/:id/pause, /resume

Scope `admin`. `200` with the schedule. Fires that fall while a schedule
is paused are not caught up when it resumes.

### POST /v1/schedules/:id/trigger

Scope `admin`. Fires the schedule now, whatever its cron expression and
paused state say. `200` with `{"job": "<id of the enqueued job>"}`.

### GET /v1/schedules/:id/preview

Scope `read`. The next `?n=` fire times (default 10, 1 to 1000), starting
with the stored `next_fire_at`. Calendar exclusions and the schedule's
bounds are applied; jitter is not. A paused schedule is previewed as if
it were running.

```sh
curl -s -H "Authorization: Bearer $BELAY_TOKEN" "$BELAY_URL/v1/schedules/nightly-report/preview?n=3"
```

```json
{"times":["2026-09-30T07:30:00.000Z","2026-10-01T07:30:00.000Z","2026-10-02T07:30:00.000Z"]}
```

## Calendars

A calendar restricts when schedules may fire. Its fields are those of
`[[calendars]]` in [configuration.md](configuration.md#calendars):

```json
{"name":"business-hours","timezone":"America/Bogota","exclude_dates":["2026-12-25"],
 "windows":[{"days":["mon","tue","wed","thu","fri"],"start":"09:00","end":"17:00"}]}
```

Calendars are not owned by a node: each node stores its own and checks a
schedule against the calendars of the node that fires it.

### GET /v1/calendars

Scope `read`. One page of calendars, by name.

### GET /v1/calendars/:name

Scope `read`. The calendar, or `404`.

### PUT /v1/calendars/:name

Scope `admin`. Creates or replaces the calendar: `201` (with `Location`)
or `200`, with the calendar. The body may leave out `name`. Schedules
using the calendar get their next fire time recomputed.

```sh
curl -s -X PUT -H "Authorization: Bearer $BELAY_TOKEN" "$BELAY_URL/v1/calendars/business-hours" \
  -d @examples/calendar-business-hours.json
```

### DELETE /v1/calendars/:name

Scope `admin`. `200` with the deleted calendar. A calendar that a
schedule uses is refused with `409 failed_precondition`, naming the
schedules in `details.schedules`.

## Workflows

A workflow is a DAG of steps, each running one job once its dependencies
have finished; see [workflows.md](workflows.md) for the rules.

Spec (the body of `POST /v1/workflows`):

| field | type | meaning |
|---|---|---|
| `name` | string | a label for people and filtering |
| `steps` | array | 1 to 256 steps: `name` (`[a-z0-9][a-z0-9_-]{0,63}`, unique), `job` (a job spec without `workflow`), `depends_on` (names of other steps), `on_failure` (`fail_workflow` (default), `continue` or `skip_dependents`), `when` (`all_succeeded` (default), `any_succeeded`, `any_failed` or `always`) |
| `meta` | object of strings | annotations, as for jobs |

Step payloads may contain `{{steps.<name>.result<path>}}` placeholders,
resolved from an ancestor step's result when the step is dispatched.
[`examples/workflow-etl.json`](../examples/workflow-etl.json) is a
complete example.

A run:

```json
{"id":"01M3QM5J3K82RX3ZXEG4P682BE","state":"running","spec":{...},
 "steps":{"fetch":{"state":"succeeded","job":"01M3QM5J3M942A9EWY0CGZVF4Q","started_at":"2026-09-29T22:22:06.451Z","finished_at":"2026-09-29T22:22:06.453Z"},
          "load":{"state":"running","job":"01M3QM5J3NPPMDRD3FB3HZDMX9","started_at":"2026-09-29T22:22:06.453Z"}},
 "created_at":"2026-09-29T22:22:06.451Z"}
```

Run states are `running`, `succeeded`, `failed` and `cancelled`; step
states `pending`, `running`, `succeeded`, `failed`, `skipped` and
`cancelled`. A run's steps are enqueued on the node that accepted it,
which alone serves it.

### POST /v1/workflows

Scope `enqueue`. Validates the graph (unique names, known dependencies,
no cycles, placeholders naming ancestors), enqueues the steps without
dependencies, and answers `201 Created` with the run and
`Location: /v1/workflows/<id>`. A cycle is reported with its path:
`$.steps: dependency cycle: a -> b -> a (each step depends on the next)`.

```sh
curl -s -H "Authorization: Bearer $BELAY_TOKEN" "$BELAY_URL/v1/workflows" -d @examples/workflow-etl.json
```

### GET /v1/workflows

Scope `read`. One page of runs, newest first. `?state=` and `?name=`
filter, `?order=asc` lists oldest first, `cursor` and `limit` page.

### GET /v1/workflows/:id

Scope `read`. The run, or `404`. With `?details=true` the answer is
`{"workflow": {...}, "errors": {"<step>": "<failure message>"}}`, with the
last error of each failed step.

### POST /v1/workflows/:id/cancel

Scope `enqueue`. Cancels the run and its unfinished steps' jobs, and
answers `200` with the run. Cancelling a finished run changes nothing.

## Workers

### GET /v1/workers

Scope `read`. One page of the workers this node has heard from, by id,
built from their lease and heartbeat traffic:

```json
{"items":[{"id":"worker-7","queues":["default","emails"],
  "first_seen":"2026-09-29T22:21:09.018Z","last_seen":"2026-09-29T22:22:11.498Z",
  "active_jobs":2,"leased":130,"succeeded":127,"failed":1}]}
```

`queues` are the ones named in the worker's latest lease request;
`failed` counts failed attempts, expired leases included. The format has
room for an `addr` field, which this version does not fill in. A worker
silent for `engine.worker_timeout` is reported lost (`worker.lost`) and
listed for `engine.worker_retention` more. A worker polling several
nodes of a cluster appears on each.

## Events

### GET /v1/events

Scope `read`. The node's events as a stream of server-sent events.

| parameter | meaning |
|---|---|
| `from` | first sequence number to deliver (inclusive) |
| `types` | comma-separated patterns: `*`, a category (`job.*`) or a type (`schedule.fired`); an unknown one is `400` with a suggestion |
| `queue` | only events about this queue (events about no queue, such as schedule and workflow events, are then left out) |

A client resuming after a disconnect sends the last id it received as
`Last-Event-ID`, which takes precedence over `from` (the stream starts
after it). Without either, the stream starts with the oldest event the
server still buffers (`events.buffer`, 10000 by default). The answer is
`200` with `Content-Type: text/event-stream`, `Cache-Control: no-cache`,
`X-Accel-Buffering: no` (so nginx does not buffer it) and a chunked body:

```sh
curl -s -N -H "Authorization: Bearer $BELAY_TOKEN" "$BELAY_URL/v1/events?types=job.*&queue=emails"
```

```text
retry: 2000

id: 1877706506502145
event: job.enqueued
data: {"seq":1877706506502145,"at":"2026-09-29T22:21:13.932Z","type":"job.enqueued","job":"01M3QM3YTBDKTE6E25JG340DV3","queue":"emails","kind":"echo","run_at":"2026-09-29T22:21:13.931Z"}

id: 1877706506502146
event: job.started
data: {"seq":1877706506502146,"at":"2026-09-29T22:21:13.932Z","type":"job.started","job":"01M3QM3YTBDKTE6E25JG340DV3","queue":"emails","worker":"dev-worker","attempt":1}

:

```

The stream starts with `retry:` (how long to wait before reconnecting, in
milliseconds). Each event is one frame: `id:` its sequence number,
`event:` its type, `data:` its JSON, which repeats both as `seq` and
`type` and adds `at` and the fields of the type. A comment line (`:`) is
sent every 15 seconds when there is nothing else, which keeps proxies
from closing the connection.

Sequence numbers increase by one per event and keep increasing across
restarts: a node numbers its events from its start time in seconds times
2^20, so they are large, stay below 2^53, and an id from before a restart
is never ahead of the stream. Events themselves live in memory only;
those from before a restart cannot be replayed.

**Lag frames.** A client that resumes from a point the buffer no longer
holds, or reads too slowly to keep up (`events.subscriber_queue`), gets
one frame without an id:

```text
event: lagged
data: {"type":"lagged","missed":3}
```

`missed` is how many sequence numbers were skipped; the stream then
continues with the oldest buffered event. Having no id, the frame does
not move the client's resume point.

The stream never answers `204`. It ends cleanly when the server shuts
down or the client goes away; a request arriving while the engine stops
gets `503` with `Retry-After`. A bad `Last-Event-ID` (not a number) is
`400`.

Event types, with the fields each adds:

| type | fields |
|---|---|
| `job.enqueued` | `job`, `queue`, `kind`, `run_at` |
| `job.started` | `job`, `queue`, `worker`, `attempt` |
| `job.progress` | `job`, `queue`, `progress` |
| `job.succeeded` | `job`, `queue`, `attempt`, `runtime` |
| `job.failed` | `job`, `queue`, `attempt`, `error`, `retry_at` (an attempt failed; the job will retry) |
| `job.dead` | `job`, `queue`, `attempt`, `error` |
| `job.cancelled` | `job`, `queue` |
| `job.expired` | `job`, `queue`, `worker`, `attempt` (a lease ran out; `job.failed` or `job.dead` follows) |
| `job.retried` | `job`, `queue` (a dead or cancelled job sent back to ready) |
| `job.promoted` | `job`, `queue` |
| `job.deleted` | `job`, `queue` |
| `schedule.created`, `schedule.updated`, `schedule.deleted`, `schedule.paused`, `schedule.resumed` | `schedule` |
| `schedule.fired` | `schedule`, `job`, `fire_time` |
| `schedule.misfired` | `schedule`, `fire_time` (the first missed), `missed`, `policy` |
| `schedule.skipped` | `schedule`, `fire_time`, `reason` (`overlap` or `calendar`) |
| `workflow.started` | `workflow`, `name` |
| `workflow.step` | `workflow`, `step`, `state`, `job` (once dispatched) |
| `workflow.finished` | `workflow`, `name`, `state` |
| `queue.paused`, `queue.resumed`, `queue.configured`, `queue.deleted` | `queue` |
| `queue.purged` | `queue`, `state` (when the purge named one), `count` |
| `worker.seen` | `worker`, `queues` (first lease, or first after being lost) |
| `worker.lost` | `worker`, `last_seen` |

New fields may be added to a type; clients should ignore fields they do
not know and skip types they do not know.

## Cluster

### GET /v1/cluster/members

Scope `read`. One page of the members as this node sees them, by node
id. A node that is not clustered lists itself.

```json
{"items":[{"id":"belay-a","addr":"http://10.0.0.11:7700","state":"alive","leader":true},
          {"id":"belay-b","addr":"http://10.0.0.12:7700","state":"alive","leader":false},
          {"id":"belay-c","addr":"http://10.0.0.13:7700","state":"suspect","leader":false}]}
```

`state` is `alive`, `suspect`, `dead` or `left`; `leader` marks the live
member with the lowest id, which is reported only (nothing depends on it
yet). Views converge through gossip but are not
agreed on at any instant, so two nodes may briefly disagree.

## Administration

### POST /v1/admin/checkpoint

Scope `admin`. Writes a snapshot of the store now and deletes the log it
covers. `200` with `{"seq": 34, "took": "24us800ns"}`, the store
sequence number the snapshot covers and how long it took.

### GET /v1/admin/store

Scope `admin`. What the store is and its statistics: `storage`
(`disk`, `memory` or `custom`), `dir` for disk stores, key and byte
counts, sequence numbers (`last_seq`, `durable_seq`), write-ahead log
figures (`wal_segments`, `wal_bytes`, `wal_syncs`), checkpoint figures,
and `recovery`, what opening the store found (snapshot loaded, batches
replayed, torn tails truncated, regions skipped, `lost_data`).

### GET /v1/admin/verify

Scope `admin`. Rebuilds the queue engine's indexes from the store and
compares them with the live ones:
`{"consistent": true, "jobs": 4, "queues": 3, "claims": 3, "problems": [], "more_problems": 0}`.
It holds the queue lock for a full scan, which is why it needs `admin`
although it changes nothing: a diagnostic, not something to poll.

### POST /v1/admin/shutdown

Scope `admin`. Starts a graceful shutdown and answers `202 Accepted`
before the connection closes:

```json
{"shutting_down":true,"already":false,"grace":"30s"}
```

The server stops accepting connections, `/readyz` turns to `503`,
requests in flight finish (long polls answer with no jobs, event streams
end), and after at most `server.shutdown_grace` the engine shuts down.
This is the way to stop a server cleanly: the standard library cannot
catch signals, so Ctrl-C and `SIGTERM` end the process at once. That is
safe too (the store is crash-consistent), but running jobs then only
return to their queues when their leases expire.
