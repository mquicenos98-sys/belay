# Contributing to belay

Thanks for looking. belay is a side project with one maintainer, so
issues with a clear reproduction and small, focused pull requests are
what gets merged fastest. For anything larger than a fix, open an issue
first and say what you want to change and why.

## Building and testing

You need a Rust toolchain, 1.80 or newer. belay depends on nothing
outside the standard library, so there is nothing else to install and
no registry access needed.

```sh
cargo build --workspace                  # every crate and binary
cargo test --workspace                   # unit, integration, doc and end-to-end tests
cargo build --workspace --examples       # the crate examples
cargo fmt --all                          # rustfmt, default style
cargo doc --workspace --no-deps          # the crate docs; keep them warning-free
cargo build --release -p belay-cli       # target/release/belay
```

CI runs the build and the tests on Linux, macOS and Windows, checks
formatting with `cargo fmt --all --check`, and builds the docs. A pull
request should pass the same locally, with no compiler warnings,
tests included. There is no clippy step, but its advice is usually
worth taking.

To work on one crate, test just that one; the whole workspace takes a
while:

```sh
cargo test -p belay-queue
cargo test -p belay-store --test crash
cargo test -p belay-cron dst_gap
```

The whole suite runs in a few minutes on a laptop. Tests are
deterministic: engines are driven with a manual clock, randomness comes
from seeded generators, and anything that has to wait for another
thread waits for a condition with a timeout rather than sleeping for a
fixed time. A test that fails once in a hundred runs is a bug; please
report it with the seed or the output.

## The end-to-end suite

`crates/belay-testkit` holds harnesses for testing belay in-process, and
its `tests/` directory is the end-to-end suite: a real engine behind a
real HTTP server on `127.0.0.1:0`, real workers leasing over HTTP, and
clusters gossiping over an in-memory network, all driven through the
public API.

```sh
cargo test -p belay-testkit                          # the whole suite
cargo test -p belay-testkit --test durability        # one file
BELAY_TEST_LOG=debug cargo test -p belay-testkit --test cluster -- --nocapture
```

Logs are captured and only printed when `BELAY_TEST_LOG` names a level.
The files are grouped by feature (`jobs`, `retries`, `queues`,
`schedules`, `workflows`, `events`, `auth`, `durability`, `cluster`,
`operations`, and a short bounded `soak`); a new user-visible feature
should come with a scenario in the matching file. The crate docs of
`belay-testkit` show how the pieces combine: `TestServer` (with
`tick`, `restart` and `crash`), `TestWorker`, `TestCluster`, the
recording handlers and the `eventually` family of waits.

## Benchmarks

`crates/belay-bench` is a benchmark harness with its own binary. Always
run it in release mode; a debug build says so in its header, and its
numbers mean nothing.

```sh
cargo run --release -p belay-bench -- --list                  # the scenarios
cargo run --release -p belay-bench -- all --quick             # a reduced run, a minute or two
cargo run --release -p belay-bench -- store-write --filter sync=always
cargo run --release -p belay-bench -- queue-http --json > after.json
```

Scenarios cover store writes by sync policy, store recovery, the queue
embedded and over HTTP, workers, cron iteration and JSON. Each prints
throughput and latency percentiles per case. `--quick` shrinks the
matrix, `--smoke` only checks that every case runs, `--filter
key=value` selects cases, `--seed` reproduces a run's inputs, `--dir`
puts store files on a disk of your choice, and `--json` writes one
document to compare runs with. There is no built-in comparison: run
twice before trusting a small difference. A change that claims to make
something faster should include before and after numbers from the
scenario that measures it.

## Crate layout

The workspace is `crates/*`, one crate per concern, and a crate only
depends on crates below it:

| layer | crates |
|---|---|
| foundations (no dependencies at all) | `belay-common`, `belay-json`, `belay-conf`, `belay-cron`, `belay-metrics`, `belay-http` |
| model and storage | `belay-proto`, `belay-store`, `belay-cluster` |
| engines and clients | `belay-queue`, `belay-scheduler`, `belay-worker`, `belay-client` |
| composition | `belay-core` (the `Engine` and `Config`) |
| serving | `belay-server` (HTTP API), `belay-cli` (the `belay` binary) |
| testing | `belay-testkit` (harnesses and the end-to-end suite), `belay-bench` |

[docs/architecture.md](docs/architecture.md) describes what each crate
does and how a request travels through them. Inside a crate:

- `src/lib.rs` starts with the crate documentation: what the crate is
  for, an example, and its limitations. Keep it current; it is the first
  thing people read.
- Modules stay between roughly 100 and 900 lines; split a module that
  grows past that into submodules.
- Unit tests go in a `#[cfg(test)] mod tests` at the bottom of the file
  they test; tests that use only the public API go in `tests/*.rs`.
- Runnable examples go in `examples/` and must build with
  `cargo build --workspace --examples`.
- User-facing documentation goes in `docs/`, and every behaviour change
  in `CHANGELOG.md` under the next version.

## Code conventions

**Standard library only.** `[dependencies]` and `[dev-dependencies]`
list other `belay-*` crates by path and nothing else. If you need
something small (a hash, a parser), write it, test it and put it in the
crate it belongs to; a crate from crates.io will not be merged. Manifests
inherit `version`, `edition`, `rust-version`, `license`, `authors` and
`repository` from the workspace.

**No `unsafe`.** Every crate has `#![forbid(unsafe_code)]`.

**Stay on Rust 1.80.** `rust-version` is a promise: no language or
library features stabilised later (no `let` chains, no
`Option::is_none_or`). When unsure, write it the older way.

**Portable.** Code builds and runs on Linux, macOS and Windows: use
`std::path`, no hard-coded separators, no platform-only APIs without a
fallback. Files use LF line endings.

**Errors.** Each crate defines its own `Error` enum by hand, with
`Display`, `std::error::Error` (with `source()` where it wraps another
error), `From` conversions for what it wraps, and
`pub type Result<T, E = Error> = std::result::Result<T, E>`. Messages are
lowercase, short and specific: `queue name must be 1-64 characters, got
71`, not `invalid input`. Errors that reach API clients become an
`ApiError` with the right code (see
[docs/http-api.md](docs/http-api.md#errors)).

**No panics on fallible paths.** No `unwrap()` in library code; `expect`
only for real invariants, with a message saying why it holds. Poisoned
mutexes are recovered, not propagated (`belay_common::sync::lock`).

**Public items are documented**, concisely, with examples where they
help; examples in doc comments are compiled and run as tests. Comments
explain why (trade-offs, invariants, edge cases, references to RFCs),
not what the next line does. Avoid filler and marketing words.

**Idiomatic, small types.** `Debug` on public types, `Display` for
anything shown to people, builders where many options have defaults,
`#[must_use]` where ignoring a value is a mistake, `&str` and slices in
signatures.

**Logging** uses the `belay_common` macros with structured fields:
`info!("lease expired"; job = %id, worker)`. Messages are lowercase and
say what happened; fields carry the values.

## Tests

- **Names state the behaviour**, not the function under test:
  `rejects_queue_names_with_uppercase`,
  `lease_expiry_returns_job_to_ready`,
  `dst_gap_fires_at_first_valid_instant`.
- **Edge cases and error paths**, not just the happy path: empty, zero,
  maximum, one past the limit, time zone transitions, crashes at every
  point, malformed input.
- **Deterministic and fast.** Use `ManualClock` and `tick(now)` instead
  of real time; sleep at most a few milliseconds when a thread really has
  to run; seed every random generator. A crate's whole suite should run
  in well under half a minute.
- **Regression tests** for bugs, with a one-line comment saying what
  went wrong: `// Regression: a purge skipped one job at every batch
  boundary.`
- Storage changes get crash tests: `belay_store::vfs::MemFs` can drop
  unsynced data or tear the last write at any byte offset.

## Commit messages

Conventional commits, with the area of the change as the scope and an
imperative, lowercase summary of at most about 60 characters:

```text
feat(queue): let a lease request wait for work
fix(store): fsync the directory after renaming a snapshot
test(cron): cover the spring-forward gap in every embedded zone
docs(server): list the scopes of every route
perf(json): skip the escape scan for plain ASCII keys
refactor(core): split the engine builder out of engine/mod.rs
```

The scope is the crate without its `belay-` prefix (`queue`, `store`,
`cli`), or a narrower area of a big crate where that says more:
`workflow` and `scheduler` in belay-scheduler, `config` in belay-core,
`api` in belay-proto, `sse` and `http` in belay-http, `log`, `time` and
`codec` in belay-common. Use `docs` for the `docs/` directory, `ci` and
`build` for the workflow and manifests. A body, wrapped at 72 columns,
says what was wrong or missing and why the change is right; for a fix,
how it showed up.

One logical change per commit, and every commit builds and passes its
crate's tests.

## License

By contributing you agree that your contributions are licensed under
MIT OR Apache-2.0, like the rest of belay.
