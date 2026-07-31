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
