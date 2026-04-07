//! Server-wide read-outs: `info`, `version`, `members` and `workers`.

use std::time::Duration;

use belay_json::{json, ToJson, Value};

use super::Ctx;
use crate::args::Command;
use crate::error::Result;
use crate::output::human::{approx, relative, when};
use crate::output::{state_style, Fields, Grid, Style};

/// Clock differences below this are not worth mentioning: they are within
/// the time the request itself took.
const SKEW_TOLERANCE: Duration = Duration::from_secs(2);

pub(crate) fn info_spec() -> Command {
    Command::new(
        "info",
        "show the server's version, node, storage and uptime",
    )
}

pub(crate) fn version_spec() -> Command {
    Command::new("version", "print the version of this belay command")
}

pub(crate) fn members_spec() -> Command {
    Command::new(
        "members",
        "list the cluster's members as the server sees them",
    )
}

pub(crate) fn workers_spec() -> Command {
    Command::new(
        "workers",
        "list the workers the server has heard from recently",
    )
}

pub(crate) fn version(ctx: &mut Ctx<'_>) -> Result<()> {
    let version = env!("CARGO_PKG_VERSION");
    if ctx.out.json {
        ctx.out.json_value(&json!({"version": version}))?;
    } else if ctx.out.quiet {
        ctx.out.line(version)?;
    } else {
        ctx.out.line(format!("belay {version}"))?;
    }
    Ok(())
}
