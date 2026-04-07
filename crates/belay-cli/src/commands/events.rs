//! `belay events tail`: follows the server's event stream.

use belay_json::{ToJson, Value};
use belay_proto::event::{Event, EventFilter, EventPattern};
use belay_proto::QueueName;

use super::Ctx;
use crate::args::{Command, Flag, Matches};
use crate::error::Result;
use crate::output::state_style;

pub(crate) fn spec() -> Command {
    Command::new("events", "follow the event stream").subcommand(
        Command::new("tail", "print events as they happen")
            .details(
                "Starts with the oldest event the server still holds (or --from), then\n\
                 follows new ones, reconnecting and resuming after interruptions. With --json\n\
                 each event is one line of JSON.",
            )
            .flag(
                Flag::option(
                    "types",
                    "PATTERNS",
                    "Only these types, e.g. job.*,schedule.fired",
                )
                .repeated(),
            )
            .flag(Flag::option(
                "queue",
                "QUEUE",
                "Only events about this queue",
            ))
            .flag(Flag::option("from", "SEQ", "Start at this sequence number"))
            .flag(Flag::option("limit", "N", "Exit after this many events").short('n'))
            .example("belay events tail --types job.dead,job.failed --queue emails")
            .example("belay events tail --json | jq .type"),
    )
}

pub(crate) fn tail(ctx: &mut Ctx<'_>, m: &Matches) -> Result<()> {
    let mut filter = EventFilter::new();
    filter.types = m.get_list::<EventPattern>("types")?;
    filter.queue = m.get::<QueueName>("queue")?;
    filter.from = m.get::<u64>("from")?;
    let limit = m.get::<u64>("limit")?;
    if limit == Some(0) {
        return Ok(());
    }
    let subscription = ctx.client()?.events(&filter)?;
    let mut seen = 0;
    for event in subscription {
        let event = event?;
        if ctx.out.json {
            ctx.out.json_line(&event.to_json())?;
        } else {
            let line = render(&event, ctx.out.color);
            ctx.out.line(line)?;
            ctx.out.flush()?;
        }
        seen += 1;
        if limit.is_some_and(|n| seen >= n) {
            break;
        }
    }
    Ok(())
}

/// `seq  time  type  key=value ...`, the type colored by its outcome.
fn render(event: &Event, color: bool) -> String {
    let type_name = event.type_name();
    let outcome = type_name.rsplit('.').next().unwrap_or(type_name);
    let shown_type = match state_style(outcome).filter(|_| color) {
        Some(style) => style.paint(&format!("{type_name:<18}")),
        None => format!("{type_name:<18}"),
    };
    let mut line = format!("{}  {}  {shown_type}", event.seq, event.at.to_rfc3339());
    if let Value::Object(fields) = event.to_json() {
        for (key, value) in fields.iter() {
            if matches!(key, "seq" | "at" | "type") {
                continue;
            }
            line.push_str(&format!(" {key}={}", scalar(value)));
        }
    }
    line.trim_end().to_owned()
}
