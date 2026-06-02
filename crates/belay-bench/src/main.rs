//! The `belay-bench` binary; see the `belay_bench` crate for what it
//! measures and how.

use std::process::ExitCode;

fn main() -> ExitCode {
    // Non-UTF-8 arguments cannot name a scenario or option; converting
    // lossily turns them into a usage error instead of a panic.
    let args = std::env::args_os()
        .skip(1)
        .map(|a| a.to_string_lossy().into_owned());
    let (mut out, mut err) = (std::io::stdout(), std::io::stderr());
    ExitCode::from(belay_bench::run(args, &mut out, &mut err))
}
