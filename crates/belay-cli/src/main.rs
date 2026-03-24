//! The `belay` binary; see the `belay_cli` crate for everything it does.

use std::process::ExitCode;

fn main() -> ExitCode {
    // Unlocked handles: server and worker threads log to stderr while the
    // command runs, so the command must not hold the stream's lock.
    let (mut out, mut err) = (std::io::stdout(), std::io::stderr());
    belay_cli::run(std::env::args_os().skip(1), &mut out, &mut err).into()
}
