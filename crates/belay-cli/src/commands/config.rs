//! `belay config ...`: checking, printing and generating server
//! configuration files, offline.
//!
//! Files are loaded exactly as `belay server` loads them, `BELAY_...`
//! environment overrides included, so `config check` answers "would the
//! server start with this?" and `config print` shows what it would run
//! with.

use belay_core::config::Config;
use belay_json::{json, Value};

use super::Ctx;
use crate::args::{Arg, Command, Flag, Matches};
use crate::error::{Error, Result};
use crate::output::Fields;

/// Environment variable naming the configuration file, as for `server`.
pub(crate) const ENV_CONFIG: &str = "BELAY_CONFIG";

pub(crate) fn spec() -> Command {
    let path = || {
        Arg::optional(
            "path",
            "Configuration file (default: BELAY_CONFIG, else none)",
        )
    };
    Command::new("config", "check, print and generate configuration files")
        .subcommand(
            Command::new(
                "check",
                "validate a configuration file as the server would load it",
            )
            .details(
                "Reports errors with the offending line and warns about unknown keys, which\n\
                     are usually typos. BELAY_<SECTION>__<KEY> environment variables are applied\n\
                     on top of the file, as the server does.",
            )
            .arg(path())
            .flag(Flag::switch("strict", "Treat warnings as errors")),
        )
        .subcommand(
            Command::new(
                "print",
                "print the effective configuration, defaults included",
            )
            .arg(path()),
        )
        .subcommand(Command::new(
            "default",
            "print a commented example configuration",
        ))
}

pub(crate) fn run(ctx: &mut Ctx<'_>, m: &Matches, action: &str) -> Result<()> {
    match action {
        "default" => {
            if ctx.out.json {
                return Err(m
                    .constraint("config default prints TOML; drop --json")
                    .into());
            }
            ctx.out.text(Config::example_toml())?;
            Ok(())
        }
        "check" => check(ctx, m),
        "print" => {
            let (_, config) = load(ctx, m.value("path"))?;
            if ctx.out.json {
                return Err(m.constraint("config print prints TOML; drop --json").into());
            }
            for warning in config.warnings() {
                ctx.out.err_text(&format!("{warning}\n"))?;
            }
            ctx.out.text(&config.to_toml())?;
            Ok(())
        }
        other => unreachable!("config {other} is not declared"),
    }
}

/// Loads the file at `path` (or `BELAY_CONFIG`, or nothing) with the
/// environment overlay. Returns the name used in messages.
pub(crate) fn load(ctx: &Ctx<'_>, path: Option<&str>) -> Result<(String, Config)> {
    let path = path
        .map(str::to_owned)
        .or_else(|| ctx.runner.var(ENV_CONFIG).filter(|p| !p.trim().is_empty()));
    let vars = ctx.runner.vars();
    let (name, source) = match &path {
        Some(path) => {
            let text = std::fs::read_to_string(path)
                .map_err(|e| Error::io(format!("reading {path}"), e))?;
            (path.clone(), text)
        }
        None => ("(defaults)".to_owned(), String::new()),
    };
    let config = Config::from_source_with_env(&name, &source, vars)?;
    Ok((name, config))
}

fn check(ctx: &mut Ctx<'_>, m: &Matches) -> Result<()> {
    let (name, config) = load(ctx, m.value("path"))?;
    let warnings: Vec<String> = config.warnings().iter().map(ToString::to_string).collect();
    if ctx.out.json {
        let listed: Vec<Value> = warnings.iter().map(|w| Value::String(w.clone())).collect();
        ctx.out.json_value(&json!({
            "file": name.as_str(),
            "ok": warnings.is_empty() || !m.flag("strict"),
            "warnings": listed,
        }))?;
    } else {
        for warning in &warnings {
            ctx.out.err_text(&format!("{warning}\n"))?;
        }
        if !ctx.out.quiet {
            ctx.out.line(format!("{name}: ok"))?;
            ctx.out.fields(&summary(&config))?;
        }
    }
    if m.flag("strict") && !warnings.is_empty() {
        return Err(Error::failed(format!(
            "{name} has {} (--strict)",
            belay_common::text::plural(warnings.len() as u64, "warning")
        )));
    }
    Ok(())
}

/// The settings that matter most at a glance.
fn summary(config: &Config) -> Fields {
    let mut f = Fields::new();
    let storage = &config.storage;
    f.add("listen", &config.server.listen).add(
        "storage",
        if storage.in_memory {
            "in memory (lost on exit)".to_owned()
        } else {
            format!("{} (sync {})", storage.dir.display(), storage.sync)
        },
    );
    let auth = &config.auth;
    f.add(
        "auth",
        if auth.disabled {
            "disabled".to_owned()
        } else if auth.tokens.is_empty() {
            "none (loopback only)".to_owned()
        } else {
            belay_common::text::plural(auth.tokens.len() as u64, "token")
        },
    );
    if !config.queues.is_empty() {
        let names: Vec<&str> = config.queues.keys().map(|q| q.as_str()).collect();
        f.add("queues", names.join(", "));
    }
    if !config.schedules.is_empty() {
        let ids: Vec<&str> = config.schedules.iter().map(|s| s.id.as_str()).collect();
        f.add("schedules", ids.join(", "));
    }
    if !config.calendars.is_empty() {
        let names: Vec<&str> = config.calendars.iter().map(|c| c.name.as_str()).collect();
        f.add("calendars", names.join(", "));
    }
    let cluster = &config.cluster;
    f.add(
        "cluster",
        if cluster.enabled {
            format!(
                "node {}, {}",
                cluster.node_id,
                belay_common::text::plural(cluster.seeds.len() as u64, "seed")
            )
        } else {
            "off".to_owned()
        },
    );
    f
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn summaries_show_the_essentials() {
        let config = Config::from_str(
            "[server]\nlisten = \"127.0.0.1:9000\"\n[storage]\nin_memory = true\n\
             [queues.emails]\nmax_concurrency = 2\n",
        )
        .unwrap();
        let text = summary(&config).render(false);
        assert!(text.contains("listen   127.0.0.1:9000\n"), "{text}");
        assert!(text.contains("storage  in memory"), "{text}");
        assert!(text.contains("auth     none (loopback only)"), "{text}");
        assert!(text.contains("queues   emails"), "{text}");
        assert!(text.contains("cluster  off"), "{text}");
    }
}
