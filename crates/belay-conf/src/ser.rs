//! TOML output, as used by `belay config print`.
//!
//! Layout is deterministic: within each table, plain keys come first, then
//! sub-tables and arrays of tables as `[section]`/`[[section]]` blocks, each
//! group in insertion order. Parsing the output yields an equal table.

use crate::path::{write_basic_string, write_key};
use crate::value::{Table, Value};

/// Renders `table` as a TOML document.
///
/// ```
/// use belay_conf::Document;
///
/// let doc = Document::parse("[server]\nlisten = '0.0.0.0:7700'\n[[schedules]]\nid = 'nightly'\n")?;
/// assert_eq!(
///     belay_conf::to_string(doc.root()),
///     "[server]\nlisten = \"0.0.0.0:7700\"\n\n[[schedules]]\nid = \"nightly\"\n"
/// );
/// # Ok::<(), belay_conf::Error>(())
/// ```
pub fn to_string(table: &Table) -> String {
    let mut out = String::new();
    write_table(&mut out, &mut Vec::new(), table);
    out
}

/// A non-empty array whose elements are all tables is written as `[[x]]`
/// sections; anything else stays inline.
fn is_array_of_tables(value: &Value) -> bool {
    match value {
        Value::Array(items) => {
            !items.is_empty() && items.iter().all(|v| matches!(v, Value::Table(_)))
        }
        _ => false,
    }
}

fn is_section(value: &Value) -> bool {
    matches!(value, Value::Table(_)) || is_array_of_tables(value)
}

fn write_table<'t>(out: &mut String, path: &mut Vec<&'t str>, table: &'t Table) {
    for (key, value) in table.iter().filter(|(_, v)| !is_section(v)) {
        write_key(out, key);
        out.push_str(" = ");
        write_inline(out, value);
        out.push('\n');
    }
    for (key, value) in table.iter() {
        path.push(key);
        match value {
            Value::Table(t) => {
                // A table holding only sections gets its header implicitly
                // from theirs; empty tables need one to exist at all.
                if t.is_empty() || t.iter().any(|(_, v)| !is_section(v)) {
                    write_header(out, path, false);
                }
                write_table(out, path, t);
            }
            Value::Array(items) if is_array_of_tables(value) => {
                for item in items.iter().filter_map(Value::as_table) {
                    write_header(out, path, true);
                    write_table(out, path, item);
                }
            }
            _ => {}
        }
        path.pop();
    }
}

fn write_header(out: &mut String, path: &[&str], array: bool) {
    if !out.is_empty() {
        out.push('\n');
    }
    out.push_str(if array { "[[" } else { "[" });
    for (i, key) in path.iter().enumerate() {
        if i > 0 {
            out.push('.');
        }
        write_key(out, key);
    }
    out.push_str(if array { "]]\n" } else { "]\n" });
}
