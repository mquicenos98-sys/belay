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
