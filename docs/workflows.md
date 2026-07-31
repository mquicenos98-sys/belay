# Workflows

A workflow is a directed acyclic graph of steps, each of which runs one
job. A step starts once the steps it depends on have finished; conditions
decide whether it runs at all, failure policies decide what a failed step
does to the rest, and a step's payload can use the results of the steps
before it. The workflow engine lives in `belay-scheduler` and creates its
jobs through the ordinary queue, so steps are leased, retried and reported
like any other job.

## The model

A run is submitted as a `WorkflowSpec`:

| field | meaning | limits |
|---|---|---|
| `name` | a label for people and for filtering listings | letters, digits, `_ . : / -`, at most 64 characters |
| `steps` | the steps, in declaration order | 1 to 256 |
| `meta` | string annotations | as job `meta`: at most 32 entries |

Each step is a `StepSpec`:

| field | meaning | default |
|---|---|---|
| `name` | unique within the workflow, `[a-z0-9][a-z0-9_-]{0,63}` | required |
| `job` | the job to run: a full job spec (queue, kind, payload, retries, timeout...) | required |
| `depends_on` | names of the steps that must finish first | none (a root step) |
| `on_failure` | `fail_workflow`, `continue` or `skip_dependents` | `fail_workflow` |
| `when` | `all_succeeded`, `any_succeeded`, `any_failed` or `always` | `all_succeeded` |

Step names cannot contain dots, because they appear in
`{{steps.<name>.result}}` placeholders.

A run moves through these states:

| run state | meaning |
|---|---|
| `running` | steps are still pending or running |
| `succeeded` | every step finished, and no step failed under `fail_workflow` |
| `failed` | a step failed under `fail_workflow` |
| `cancelled` | cancelled on request |

and each of its steps through these:

| step state | meaning |
|---|---|
| `pending` | waiting for its dependencies |
| `running` | its job has been dispatched |
| `succeeded` | its job succeeded |
| `failed` | its job ended `dead` or `cancelled`, its payload could not be resolved, or the queue refused its job |
| `skipped` | not run: its condition was false, or an upstream failure skipped it |
| `cancelled` | cancelled with the workflow |

A run is `succeeded` even when some of its steps failed, as long as none of
them failed under `fail_workflow`. Look at the step states (or the run's
`errors` with `?details=true`) to tell a clean run from one that continued
past failures.

## Steps and dependencies

A step becomes eligible once every step it depends on has finished,
whether it succeeded, failed, was skipped or was cancelled. Its condition
then decides between dispatching its job and marking it `skipped`. Steps
without dependencies are dispatched as soon as the run is submitted, and
eligible steps are handled in declaration order, which breaks ties.
Finishing a step can make its dependents eligible, and skipping those can
make theirs eligible, so one change is carried through the graph until
every eligible step is running or finished.

A step's job is the step's `job` spec plus two things the engine adds: a
`workflow` field (`{"id": <run>, "step": <name>}`) linking it to the run,
and, unless the step brings a `unique` rule of its own, the uniqueness key
`wf:<run id>:<step>` held for `workflows.dedupe_window` (24 hours by
default). The job may not set `workflow` itself.

Submission validates the whole graph and reports every problem with its
path:

- step names must be unique and follow the naming rule;
- every dependency must name a step of the workflow (with a "did you mean"
  suggestion), and a step may not depend on itself or list a dependency
  twice;
- the graph must be acyclic; a cycle is reported as a path:

  ```text
  $.steps: dependency cycle: a -> b -> a (each step depends on the next)
  ```

- every placeholder must refer to an ancestor of its step, directly or
  through other steps, and be well formed.

## Conditions

A condition is evaluated against the final states of the step's
dependencies. Steps without dependencies always run.

| `when` | the step runs when |
|---|---|
| `all_succeeded` (default) | every dependency succeeded |
| `any_succeeded` | at least one dependency succeeded |
| `any_failed` | at least one dependency failed or was cancelled: a cleanup or compensation step |
| `always` | whatever happened |

A skipped dependency is neither a success nor a failure. Under the default
condition, a step whose dependency was skipped is skipped too, so skipping
spreads down a branch by itself.

## Failure policies

When a step fails, its `on_failure` policy applies:

| `on_failure` | effect |
|---|---|
| `fail_workflow` (default) | every step still pending or running is cancelled (running step jobs are cancelled through the queue) and the run ends `failed` |
| `skip_dependents` | every step downstream of the failed one, directly or not, is skipped, whatever its condition |
| `continue` | nothing more; downstream conditions decide what runs |

A step fails when its job ends `dead` (its attempts ran out, or it failed
with a non-retryable error) or `cancelled`. Retries happen inside the job:
a step whose job is `retrying` is still `running`. A step also fails,
without a job ever running, when a placeholder in its payload cannot be
resolved or when the queue refuses its job. The reason is kept with the
run, for example `job 01M3... is dead: upstream answered 503`, and shown by
`GET /v1/workflows/:id?details=true`:

```json
{"workflow": {...}, "errors": {"load": "job 01M3QMM69VVA8AM441HENGDZ7Y is dead: connection refused"}}
```

To run a cleanup step after a failure without failing the run, give the
failing step `on_failure: continue` and the cleanup step `when: any_failed`.
Under `fail_workflow` the cleanup step would be cancelled along with
everything else before it could run.

## Payload placeholders

A string in a step's payload may refer to the result of an ancestor step:

```text
{{steps.<name>.result}}              the whole result
{{steps.<name>.result/items/0/url}}  an RFC 6901 JSON Pointer into it
{{steps.<name>.result.items.0.url}}  dotted keys, the same pointer spelled differently
```

Spaces just inside the braces are ignored. The rules:

- A JSON string that is **exactly one placeholder** is replaced by the
  referenced value, whatever its type: `"{{steps.count.result}}"` becomes
  the number `42`, `"{{steps.fetch.result.urls}}"` an array.
- Placeholders **inside a longer string** are replaced by text: string
  values as they are, anything else as compact JSON (`null`, `42`,
  `{"a":1}`). `"page {{steps.count.result}} of 9"` becomes `"page 42 of 9"`.
- Only string values are scanned, at any depth; object keys are not.
- `{{...}}` markers that do not start with `steps.` are left alone, so
  payloads can carry templates meant for other tools.
- Placeholders are resolved when the step is dispatched. A missing value
  fails the step with a message naming the placeholder: the referenced step
  did not succeed (`step "fetch" is failed, so it has no result`), or the
  path leads nowhere in its result (`the result of step "fetch" has no value
  at /items/9`). The step's failure policy then applies.

There is no escape syntax: a payload cannot contain the literal text
`{{steps.` followed by `}}` without it being treated as a placeholder.

Results are stored alongside the run (under `wfr/<run>/<step>`) only for
steps that some placeholder reads, so large results of other steps cost
nothing beyond the job itself.

## Submitting and following runs

| action | CLI | HTTP | scope |
|---|---|---|---|
| submit | `belay workflow submit <FILE> [--wait] [--wait-timeout D]` | `POST /v1/workflows` (`201`, with `Location`) | `enqueue` |
| show | `belay workflow get <id>` | `GET /v1/workflows/:id[?details=true]` | `read` |
| list | `belay workflow list [-n N] [--all]` | `GET /v1/workflows?state=&name=&order=&cursor=&limit=` | `read` |
| cancel | `belay workflow cancel <id>` | `POST /v1/workflows/:id/cancel` | `enqueue` |

`submit` reads JSON or TOML, from a file or from stdin with `-`. The spec
is validated in full before anything is stored; the answer is the new run
with its root steps already running. Listings are newest first by default
(`?order=asc` for oldest first), 100 per page, at most 1000.

Every change is published on the event stream: `workflow.started`,
`workflow.step` (a step changed state, with its job once dispatched) and
`workflow.finished` (with the final state). The metrics are
`belay_workflows_running` and `belay_workflows_finished_total{state}`.

## A complete example

`etl.json` extracts a file, transforms and loads it, then either reports or
raises an alarm. It uses the built-in `echo` handler, which returns its
payload as the result, so it runs as is against `belay dev --queues
default,etl`:

```json
{
  "name": "nightly-etl",
  "meta": {"team": "data"},
  "steps": [
    {
      "name": "extract",
      "job": {"queue": "etl", "kind": "echo",
              "payload": {"rows": 1200, "file": "s3://raw/2026-09-29.csv"}}
    },
    {
      "name": "transform",
      "depends_on": ["extract"],
      "job": {"queue": "etl", "kind": "echo",
              "payload": {"input": "{{steps.extract.result.file}}",
                          "expected": "{{steps.extract.result/rows}}",
                          "label": "batch of {{steps.extract.result.rows}} rows"}}
    },
    {
      "name": "load",
      "depends_on": ["transform"],
      "job": {"queue": "etl", "kind": "echo",
              "payload": {"from": "{{steps.transform.result.input}}"}}
    },
    {
      "name": "notify-failure",
      "depends_on": ["load"],
      "when": "any_failed",
      "job": {"queue": "default", "kind": "echo", "payload": {"channel": "#data"}}
    },
    {
      "name": "report",
      "depends_on": ["load"],
      "on_failure": "continue",
      "job": {"queue": "default", "kind": "echo",
              "payload": {"rows": "{{steps.extract.result.rows}}"}}
    }
  ]
}
```

```console
$ belay workflow submit etl.json --wait
id        01M3QMM68KYMYD3C1F8F8TJ4RR
name      nightly-etl
state     succeeded
created   2026-09-29T22:30:05.843Z (just now)
finished  2026-09-29T22:30:05.911Z (just now)

STEP            STATE      JOB                         AFTER      STARTED   TOOK
extract         succeeded  01M3QMM68YK12SSDARAM4JTD6Z  -          just now  26ms
transform       succeeded  01M3QMM69D7767FFBH3QJBCATJ  extract    just now  14ms
load            succeeded  01M3QMM69VVA8AM441HENGDZ7Y  transform  just now  14ms
notify-failure  skipped    -                           load       -         -
report          succeeded  01M3QMM6A9M19YR3P4MA34KRS7  load       just now  14ms
```

`notify-failure` was skipped because `load` succeeded. The `transform`
step's job shows the resolved payload and the fields the engine added:

```console
$ belay job get 01M3QMM69D7767FFBH3QJBCATJ --json
{
  "id": "01M3QMM69D7767FFBH3QJBCATJ",
  "state": "succeeded",
  "attempt": 1,
  "spec": {
    "queue": "etl",
    "kind": "echo",
    "payload": {
      "input": "s3://raw/2026-09-29.csv",
      "expected": 1200,
      "label": "batch of 1200 rows"
    },
    ...
    "unique": {
      "key": "wf:01M3QMM68KYMYD3C1F8F8TJ4RR:transform",
      "scope": "for:1d",
      "on_conflict": "return_existing"
    },
    "workflow": {
      "id": "01M3QMM68KYMYD3C1F8F8TJ4RR",
      "step": "transform"
    }
  },
  ...
}
```

`"{{steps.extract.result/rows}}"`, being the whole string, became the
number `1200`; inside `"batch of ... rows"` the same value became text.

Had `load` died, the run would have ended `failed` at once: `load` uses the
default `fail_workflow`, which cancels `notify-failure` and `report` before
either can run. With `"on_failure": "continue"` on `load`, a failure would
instead let `notify-failure` run (its condition `any_failed` holds), skip
`report` (its default condition needs `load` to succeed), and end the run
`succeeded`.

## Cancellation

`belay workflow cancel <id>` (`POST /v1/workflows/:id/cancel`) cancels a
run: the jobs of running steps are cancelled through the queue, every step
still pending or running is marked `cancelled`, and the run ends
`cancelled`. A step job that is already running only has its cancellation
requested; its worker learns of it at its next heartbeat. Cancelling a run
that is already over returns it unchanged.

Cancelling one step's job directly (`belay job cancel`) is different: the
step fails, and its failure policy applies.

## Recovery and crash safety

A run is stored before any of its jobs is dispatched, and each step's job
carries the uniqueness key `wf:<run>:<step>`. If belay stops between
dispatching a step and saving that fact, the step is dispatched again on
recovery, and the queue returns the original job instead of creating a
second one; if that job already finished, its outcome is picked up at once.

The engine learns about finished jobs from the queue as they finish. To
catch what it could not be told (completions while it was down, a job
purged or deleted, a dispatch deferred because the queue was unavailable),
it reconciles every running run at startup and every
`workflows.sweep_every` (30s): it asks the queue about each running step's
job, finishes the steps whose jobs finished, fails the steps whose jobs no
longer exist (`job ... no longer exists`), and dispatches eligible steps
that have no job yet.

A step whose completion was recovered from the job's state alone, without
the job record, has no stored result; a placeholder reading it then fails
with a message saying so.

## Retention

Finished runs are kept for `workflows.keep_for` (7 days by default) after
they finish, then deleted along with their stored step results, at most
1000 per sweep. The jobs of their steps follow their queues' own retention.

```toml
[workflows]
keep_for = "7d"         # finished runs are deleted after this long
sweep_every = "30s"     # retention and reconciliation
dedupe_window = "1d"    # how long a step's job is deduplicated; 1s to 30d
```

## Limitations

- Retrying a step's dead job by hand after the run has moved on does not
  reopen the step.
- Results are kept only for steps that some placeholder reads, and only
  placeholders in payloads are resolved; other job fields are copied as
  written.
- Deduplication of a repeated dispatch lasts for `dedupe_window`; a crash
  between dispatching and saving followed by a longer outage can dispatch a
  step twice.
- In a cluster, a run is served by the node that accepted it, and all its
  step jobs go into that node's queues, whichever node owns them. There is
  no redirect for workflow calls.
- Runs still going are held in memory in full; finished runs are read from
  the store on demand.
