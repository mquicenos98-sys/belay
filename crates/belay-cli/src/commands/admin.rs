//! `belay admin ...`: checkpoints, storage statistics, consistency checks
//! and shutdown over the API, plus `store-inspect`, which reads a data
//! directory directly.

use std::path::PathBuf;
use std::time::Duration;

use belay_common::text::humanize_bytes;
use belay_http::header::{ACCEPT, AUTHORIZATION};
use belay_http::{Method, Request};
use belay_json::{json, Value};
use belay_proto::api::decode_error;
use belay_store::inspect::{self, VerifyReport};
use belay_store::vfs::OsFs;

use super::Ctx;
use crate::args::{Arg, Command, Flag, Matches};
use crate::error::{Error, Result};
use crate::output::Fields;

pub(crate) fn spec() -> Command {
    Command::new("admin", "administer a server or a data directory")
        .subcommand(Command::new(
            "checkpoint",
            "write a storage snapshot now and drop the log it covers",
        ))
        .subcommand(Command::new(
            "store",
            "show the server's storage statistics",
        ))
        .subcommand(Command::new(
            "verify",
            "check that the server's job indexes agree with its store",
        ))
        .subcommand(Command::new(
            "shutdown",
            "stop the server gracefully (drain requests, then the engine)",
        ))
        .subcommand(
            Command::new(
                "store-inspect",
                "list and verify the files of a data directory",
            )
            .details(
                "Reads the directory without locking or changing it, so it works on a store\n\
                     that is open elsewhere (whose last log segment may then show a torn tail,\n\
                     which is just a write in progress). Checks every snapshot and log record\n\
                     checksum and the continuity of sequence numbers; fails (status 1) when\n\
                     opening the store would lose data.",
            )
            .arg(Arg::required("dir", "Data directory (storage.dir)"))
            .flag(Flag::switch("records", "Also list every log record")),
        )
}

pub(crate) fn run(ctx: &mut Ctx<'_>, m: &Matches, action: &str) -> Result<()> {
    match action {
        "checkpoint" => {
            let done = ctx.client()?.admin().checkpoint()?;
            if ctx.out.json {
                ctx.out.json(&done)?;
            } else if ctx.out.quiet {
                ctx.out.line(done.seq)?;
            } else {
                ctx.out.line(format!(
                    "checkpoint written through seq {} in {}",
                    done.seq,
                    crate::output::human::approx(done.took)
                ))?;
            }
            Ok(())
        }
        "store" => {
            let report = ctx.client()?.admin().store()?;
            if ctx.out.json {
                ctx.out.json_value(&report)?;
            } else {
                ctx.out.fields(&store_fields(&report))?;
            }
            Ok(())
        }
        "verify" => verify(ctx),
        "shutdown" => {
            let answer = call(ctx, Method::Post, "/v1/admin/shutdown")?;
            if ctx.out.json {
                ctx.out.json_value(&answer)?;
            } else if answer.get("already").and_then(Value::as_bool) == Some(true) {
                ctx.out.done("the server was already shutting down")?;
            } else {
                let grace = answer.get("grace").and_then(Value::as_str).unwrap_or("?");
                ctx.out.done(format!(
                    "server at {} is shutting down (requests get up to {grace} to finish)",
                    ctx.url
                ))?;
            }
            Ok(())
        }
        "store-inspect" => store_inspect(ctx, m),
        other => unreachable!("admin {other} is not declared"),
    }
}
