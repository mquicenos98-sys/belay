//! Help text generated from the command tree: usage line, arguments,
//! options with their defaults and environment variables, subcommands and
//! examples.

use std::fmt::Write as _;

use super::{Command, Flag};

/// The `Usage:` line of the command at `path` (names below `root`), e.g.
/// `belay enqueue [OPTIONS] <QUEUE> <KIND>`.
#[must_use]
pub fn usage_line(root: &Command, path: &[&str]) -> String {
    let chain = root.walk(path);
    let command = chain[chain.len() - 1];
    let mut line: Vec<String> = chain.iter().map(|c| c.name.to_owned()).collect();
    if command.is_group() {
        if !root.globals.is_empty() && chain.len() == 1 {
            line.push("[OPTIONS]".to_owned());
        }
        line.push("<COMMAND>".to_owned());
    } else {
        if !command.flags.is_empty() || !root.globals.is_empty() {
            line.push("[OPTIONS]".to_owned());
        }
        line.extend(command.args.iter().map(|a| a.display_name()));
    }
    line.join(" ")
}

/// The full help of the command at `path`.
#[must_use]
pub fn render_help(root: &Command, path: &[&str]) -> String {
    let chain = root.walk(path);
    let command = chain[chain.len() - 1];
    let is_root = chain.len() == 1;
    let full_name: Vec<&str> = chain.iter().map(|c| c.name).collect();
    let full_name = full_name.join(" ");

    let mut out = String::new();
    let _ = writeln!(out, "{}", capitalize(command.about));
    if let Some(details) = command.details {
        let _ = writeln!(out, "\n{}", details.trim_end());
    }
    let _ = writeln!(out, "\nUsage: {}", usage_line(root, path));

    if command.is_group() {
        let rows: Vec<(String, String)> = command
            .subcommands
            .iter()
            .map(|c| {
                let about = match c.aliases.as_slice() {
                    [] => capitalize(c.about),
                    aliases => format!("{} (alias: {})", capitalize(c.about), aliases.join(", ")),
                };
                (c.name.to_owned(), about)
            })
            .collect();
        section(&mut out, "Commands", &rows);
    }

    if !command.args.is_empty() {
        let rows: Vec<(String, String)> = command
            .args
            .iter()
            .map(|a| (a.display_name(), a.help.to_owned()))
            .collect();
        section(&mut out, "Arguments", &rows);
    }

    let mut own: Vec<(String, String)> = command.flags.iter().map(flag_row).collect();
    let help_row = (
        "-h, --help".to_owned(),
        "Print help (see more with 'belay help <COMMAND>')".to_owned(),
    );
    if is_root {
        own.extend(root.globals.iter().map(flag_row));
        own.push(help_row);
        own.push(("-V, --version".to_owned(), "Print version".to_owned()));
        section(&mut out, "Options", &own);
    } else {
        own.push(help_row);
        section(&mut out, "Options", &own);
        let globals: Vec<(String, String)> = root.globals.iter().map(flag_row).collect();
        section(&mut out, "Global options", &globals);
    }

    if !command.examples.is_empty() {
        out.push_str("\nExamples:\n");
        for example in &command.examples {
            let _ = writeln!(out, "  {example}");
        }
    }
    if command.is_group() {
        let _ = writeln!(
            out,
            "\nRun '{full_name} <COMMAND> --help' for more on a command."
        );
    }
    out
}

fn capitalize(text: &str) -> String {
    let mut chars = text.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => String::new(),
    }
}

/// `-p, --priority <N>` and its description with default and
/// environment variable.
fn flag_row(flag: &Flag) -> (String, String) {
    let mut name = match flag.short {
        Some(short) => format!("-{short}, --{}", flag.long),
        None => format!("    --{}", flag.long),
    };
    if let Some(value) = flag.value {
        let _ = write!(name, " <{value}>");
    }
    let mut help = flag.help.to_owned();
    if flag.repeated && flag.takes_value() {
        help.push_str(" (repeatable)");
    }
    if let Some(var) = flag.env {
        let _ = write!(help, " [env: {var}]");
    }
    if let Some(default) = flag.default {
        let _ = write!(help, " [default: {default}]");
    }
    (name, help)
}

/// A titled two-column section, the first column padded to its widest
/// entry.
fn section(out: &mut String, title: &str, rows: &[(String, String)]) {
    if rows.is_empty() {
        return;
    }
    let width = rows
        .iter()
        .map(|(n, _)| n.chars().count())
        .max()
        .unwrap_or(0);
    let _ = writeln!(out, "\n{title}:");
    for (name, help) in rows {
        let pad = width - name.chars().count();
        let _ = writeln!(out, "  {name}{}  {help}", " ".repeat(pad));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::args::Arg;

    fn tree() -> Command {
        Command::new("belay", "durable job queue")
            .global(Flag::switch("json", "Print JSON"))
            .global(Flag::option("url", "URL", "Server URL").env("BELAY_URL"))
            .subcommand(
                Command::new("enqueue", "enqueue a job")
                    .details("Adds one job to a queue.")
                    .arg(Arg::required("queue", "Queue name"))
                    .arg(Arg::optional("kind", "Handler name"))
                    .flag(
                        Flag::option("priority", "N", "Priority")
                            .short('p')
                            .default("0"),
                    )
                    .flag(Flag::option("tag", "TAG", "Tag").repeated())
                    .flag(Flag::switch("wait", "Wait for the result"))
                    .example("belay enqueue emails send --wait"),
            )
            .subcommand(
                Command::new("job", "manage jobs")
                    .subcommand(Command::new("list", "List jobs").alias("ls"))
                    .subcommand(Command::new("get", "Show a job").arg(Arg::required("id", "Job"))),
            )
    }

    #[test]
    fn usage_lines_follow_the_command_shape() {
        let root = tree();
        assert_eq!(usage_line(&root, &[]), "belay [OPTIONS] <COMMAND>");
        assert_eq!(usage_line(&root, &["job"]), "belay job <COMMAND>");
        assert_eq!(
            usage_line(&root, &["enqueue"]),
            "belay enqueue [OPTIONS] <QUEUE> [KIND]"
        );
        assert_eq!(
            usage_line(&root, &["job", "get"]),
            "belay job get [OPTIONS] <ID>"
        );
    }

    #[test]
    fn leaf_help_snapshot() {
        let text = render_help(&tree(), &["enqueue"]);
        assert_eq!(
            text,
            "Enqueue a job

Adds one job to a queue.

Usage: belay enqueue [OPTIONS] <QUEUE> [KIND]

Arguments:
  <QUEUE>  Queue name
  [KIND]   Handler name

Options:
  -p, --priority <N>  Priority [default: 0]
      --tag <TAG>     Tag (repeatable)
      --wait          Wait for the result
  -h, --help          Print help (see more with 'belay help <COMMAND>')

Global options:
      --json       Print JSON
      --url <URL>  Server URL [env: BELAY_URL]

Examples:
  belay enqueue emails send --wait
"
        );
    }

    #[test]
    fn group_help_lists_commands_with_aliases() {
        let text = render_help(&tree(), &["job"]);
        assert!(
            text.starts_with("Manage jobs\n\nUsage: belay job <COMMAND>\n"),
            "{text}"
        );
        assert!(text.contains("\nCommands:\n  list  List jobs (alias: ls)\n  get   Show a job\n"));
        assert!(text.ends_with("Run 'belay job <COMMAND> --help' for more on a command.\n"));
    }

    #[test]
    fn root_help_merges_globals_into_options() {
        let text = render_help(&tree(), &[]);
        assert!(text.contains("\nOptions:\n      --json"), "{text}");
        assert!(text.contains("-V, --version"));
        assert!(!text.contains("Global options"));
    }

    #[test]
    fn unknown_paths_fall_back_to_the_deepest_known_command() {
        let text = render_help(&tree(), &["job", "nope"]);
        assert!(text.starts_with("Manage jobs"));
    }
}
