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
