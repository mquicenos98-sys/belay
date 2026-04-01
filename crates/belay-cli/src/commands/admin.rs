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

/// A call the typed client does not cover, with the same URL, token and
/// error decoding.
fn call(ctx: &mut Ctx<'_>, method: Method, path: &str) -> Result<Value> {
    let base = ctx.client()?.base_url().to_owned();
    let http = belay_http::Client::new(belay_http::ClientConfig {
        connect_timeout: Duration::from_secs(5),
        read_timeout: Some(Duration::from_secs(120)),
        user_agent: concat!("belay-cli/", env!("CARGO_PKG_VERSION")).to_owned(),
        ..belay_http::ClientConfig::default()
    });
    let mut request = Request::new(method, format!("{base}{path}").as_str());
    request.headers.insert(ACCEPT, "application/json");
    if let Some(token) = &ctx.token {
        request
            .headers
            .insert(AUTHORIZATION, format!("Bearer {token}").as_str());
    }
    let response = http
        .request(request)
        .map_err(|e| Error::Client(belay_client::Error::Transport(e)))?;
    if !response.status.is_success() {
        let api = decode_error(response.status.as_u16(), &response.body);
        return Err(Error::Client(belay_client::Error::Api(api)));
    }
    if response.body.iter().all(u8::is_ascii_whitespace) {
        return Ok(json!({}));
    }
    belay_json::parse_bytes(&response.body)
        .map_err(|e| Error::failed(format!("unexpected response to {path}: {e}")))
}

fn verify(ctx: &mut Ctx<'_>) -> Result<()> {
    let outcome = call(ctx, Method::Get, "/v1/admin/verify")?;
    let count = |key: &str| outcome.get(key).and_then(Value::as_u64).unwrap_or(0);
    let problems: Vec<String> = outcome
        .get("problems")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|p| p.as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default();
    let consistent = outcome.get("consistent").and_then(Value::as_bool) == Some(true);
    if ctx.out.json {
        ctx.out.json_value(&outcome)?;
    } else {
        let checked = format!(
            "{}, {}, {}",
            belay_common::text::plural(count("jobs"), "job"),
            belay_common::text::plural(count("queues"), "queue"),
            belay_common::text::plural(count("claims"), "uniqueness claim")
        );
        if consistent {
            ctx.out.line(format!("consistent: checked {checked}"))?;
        } else {
            ctx.out.line(format!("inconsistent: checked {checked}"))?;
            for problem in &problems {
                ctx.out.line(format!("  {problem}"))?;
            }
            if count("more_problems") > 0 {
                ctx.out
                    .line(format!("  ... and {} more", count("more_problems")))?;
            }
        }
    }
    if consistent {
        Ok(())
    } else {
        Err(Error::Failed {
            message: format!(
                "the server's indexes disagree with its store ({})",
                belay_common::text::plural(
                    problems.len() as u64 + count("more_problems"),
                    "problem"
                )
            ),
            hint: Some("restarting the server rebuilds the indexes from the store".to_owned()),
        })
    }
}
