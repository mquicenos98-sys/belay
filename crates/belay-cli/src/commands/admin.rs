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

/// Storage statistics with byte counts made readable.
fn store_fields(report: &Value) -> Fields {
    let mut f = Fields::new();
    let Some(map) = report.as_object() else {
        f.add("store", belay_json::to_string_pretty(report));
        return f;
    };
    for (key, value) in map.iter() {
        let label = key.replace('_', " ");
        let text = match value {
            Value::Number(n) if key.ends_with("bytes") => {
                n.as_u64().map_or_else(|| n.to_string(), humanize_bytes)
            }
            Value::String(s) => s.clone(),
            Value::Object(inner) => inner
                .iter()
                .map(|(k, v)| format!("{k}={}", v))
                .collect::<Vec<_>>()
                .join("\n"),
            other => other.to_string(),
        };
        f.add(&label, text);
    }
    f
}

fn store_inspect(ctx: &mut Ctx<'_>, m: &Matches) -> Result<()> {
    let dir: PathBuf = m.require("dir")?;
    if !dir.is_dir() {
        return Err(Error::failed(format!(
            "{} is not a directory",
            dir.display()
        )));
    }
    let fs = OsFs::new();
    let report = inspect::verify(&fs, &dir)?;
    if ctx.out.json {
        ctx.out.json_value(&report_json(&report))?;
    } else {
        ctx.out.text(&report.to_string())?;
        let files = &report.files;
        if files.locked {
            ctx.out
                .line("LOCK present: the store is open, or was not closed cleanly")?;
        }
        for name in &files.temp_files {
            ctx.out.line(format!("leftover temporary file: {name}"))?;
        }
        for name in &files.other_files {
            ctx.out.line(format!("not a store file: {name}"))?;
        }
        if m.flag("records") {
            dump_records(ctx, &fs, &report)?;
        }
    }
    if report.is_ok() {
        return Ok(());
    }
    Err(Error::Failed {
        message: format!(
            "{} has {} that opening it would not survive",
            dir.display(),
            belay_common::text::plural(
                report
                    .problems
                    .iter()
                    .filter(|p| p.kind != inspect::ProblemKind::TornTail)
                    .count() as u64,
                "problem"
            )
        ),
        hint: Some(
            "storage.recovery = \"skip-corrupt\" opens it anyway, dropping the damaged records"
                .to_owned(),
        ),
    })
}

fn dump_records(ctx: &mut Ctx<'_>, fs: &OsFs, report: &VerifyReport) -> Result<()> {
    let count = report.segments.len();
    for (i, segment) in report.segments.iter().enumerate() {
        ctx.out.line(format!("\n{}:", segment.file.name))?;
        let dump = inspect::read_segment(fs, &segment.file.path, i + 1 == count)?;
        for record in &dump.records {
            ctx.out
                .line(format!("  {}", record.to_string().replace('\n', "\n  ")))?;
        }
    }
    Ok(())
}

fn report_json(report: &VerifyReport) -> Value {
    let files: Vec<Value> = report
        .snapshots
        .iter()
        .map(|s| {
            json!({
                "name": s.file.name.as_str(),
                "kind": "snapshot",
                "seq": s.file.seq,
                "size": s.file.size,
                "keys": s.keys,
                "valid": s.keys.is_some(),
            })
        })
        .chain(report.segments.iter().map(|s| {
            json!({
                "name": s.file.name.as_str(),
                "kind": "log",
                "seq": s.file.seq,
                "size": s.file.size,
                "records": s.records,
                "covered": s.covered,
            })
        }))
        .collect();
    let problems: Vec<Value> = report
        .problems
        .iter()
        .map(|p| {
            json!({
                "kind": p.kind.to_string(),
                "file": p.file.display().to_string(),
                "offset": p.offset,
                "detail": p.detail.as_str(),
            })
        })
        .collect();
    json!({
        "dir": report.files.dir.display().to_string(),
        "ok": report.is_ok(),
        "locked": report.files.locked,
        "snapshot_seq": report.snapshot_seq,
        "last_seq": report.last_seq,
        "files": files,
        "problems": problems,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn storage_statistics_humanize_byte_counts() {
        let report = json!({
            "storage": "disk",
            "keys": 12,
            "wal_bytes": 3_145_728,
            "recovery": {"torn_tails": 0, "duration": "1ms"}
        });
        let text = store_fields(&report).render(false);
        assert!(text.contains("storage    disk\n"), "{text}");
        assert!(text.contains("wal bytes  3.0 MiB\n"), "{text}");
        assert!(
            text.contains("recovery   torn_tails=0\n           duration=\"1ms\"\n"),
            "{text}"
        );
    }
}
