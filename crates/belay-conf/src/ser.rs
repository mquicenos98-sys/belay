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

/// Appends `value` in inline syntax.
pub(crate) fn write_inline(out: &mut String, value: &Value) {
    match value {
        Value::String(s) => write_basic_string(out, s),
        Value::Integer(i) => out.push_str(&i.to_string()),
        Value::Float(f) => write_float(out, *f),
        Value::Boolean(b) => out.push_str(if *b { "true" } else { "false" }),
        Value::Datetime(s) => out.push_str(s),
        Value::Array(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                write_inline(out, item);
            }
            out.push(']');
        }
        Value::Table(t) if t.is_empty() => out.push_str("{}"),
        Value::Table(t) => {
            out.push_str("{ ");
            for (i, (key, item)) in t.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                write_key(out, key);
                out.push_str(" = ");
                write_inline(out, item);
            }
            out.push_str(" }");
        }
    }
}

fn write_float(out: &mut String, f: f64) {
    if f.is_nan() {
        out.push_str("nan");
    } else if f.is_infinite() {
        out.push_str(if f > 0.0 { "inf" } else { "-inf" });
    } else {
        // Debug formatting is the shortest text that round-trips and always
        // has a `.` or an exponent (`1.0`, `1e-7`, `1e300`), which TOML
        // needs to tell a float from an integer.
        out.push_str(&format!("{f:?}"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Document;

    fn round_trip(src: &str) -> String {
        let doc = Document::parse(src).unwrap();
        let text = doc.to_toml();
        let again = Document::parse(&text).unwrap_or_else(|e| panic!("{e}\n---\n{text}"));
        assert_eq!(again.root(), doc.root(), "\n{text}");
        text
    }

    #[test]
    fn plain_keys_precede_sections_and_order_is_stable() {
        let text = round_trip("[a]\nsub = { x = 1 }\nz = 2\ny = 'q'\n[b]\nk = true\n");
        assert_eq!(
            text,
            "[a]\nz = 2\ny = \"q\"\n\n[a.sub]\nx = 1\n\n[b]\nk = true\n"
        );
    }

    #[test]
    fn arrays_of_tables_with_nested_sections() {
        let src =
            "[[s]]\nid = 'a'\n[s.template]\nqueue = 'q'\n[[s.hooks]]\nurl = 'h'\n[[s]]\nid = 'b'\n";
        let text = round_trip(src);
        assert_eq!(
            text,
            "[[s]]\nid = \"a\"\n\n[s.template]\nqueue = \"q\"\n\n[[s.hooks]]\nurl = \"h\"\n\n[[s]]\nid = \"b\"\n"
        );
    }

    #[test]
    fn scalars_and_escapes_survive() {
        round_trip(concat!(
            "s = \"quote \\\" backslash \\\\ nl \\n tab \\t bell \\u0007 é\"\n",
            "f = [1.0, -0.5, 1e-7, 6.02e23, inf, -inf, 0.1]\n",
            "i = [-9223372036854775808, 0, 42]\n",
            "d = [1979-05-27T07:32:00Z, 1979-05-27, 07:32:00.5]\n",
            "\"key with spaces\" = { \"\" = 1, nested = { deep = [] } }\n",
            "mixed = [1, 'two', { three = 3 }, [4]]\n",
            "empty = {}\n",
            "[empty_section]\n",
        ));
    }

    #[test]
    fn tables_with_only_subtables_get_no_header() {
        let text = round_trip("[queues.emails]\nmax_concurrency = 4\n[queues.reports]\n");
        assert_eq!(
            text,
            "[queues.emails]\nmax_concurrency = 4\n\n[queues.reports]\n"
        );
    }

    #[test]
    fn nan_is_written_as_toml_nan() {
        let mut t = Table::new();
        t.insert("x", f64::NAN);
        assert_eq!(to_string(&t), "x = nan\n");
    }
}
