//! [`Grid`]: a belay-common [`Table`] that can also color its header and
//! chosen columns.
//!
//! `Table` replaces control characters in cells (they would break the
//! layout), so escape sequences cannot go into cells. Instead the table is
//! rendered plain and colors are spliced in afterwards at the column
//! offsets, which are recomputed here with the same rules `Table` uses:
//! widths in characters, two spaces between columns, cells padded on the
//! right unless the column is right-aligned.

use belay_common::text::{truncate_middle, Align, Table};

use super::style::Style;

/// Chooses a style for a cell from its text.
pub(crate) type Painter = fn(&str) -> Option<Style>;

/// Rows under headers, with per-column alignment, width limits and
/// coloring.
#[derive(Clone, Debug)]
pub(crate) struct Grid {
    headers: Vec<&'static str>,
    rows: Vec<Vec<String>>,
    align: Vec<Align>,
    max_width: Vec<Option<usize>>,
    painters: Vec<Option<Painter>>,
}
