//! Plain-text tables whose columns are as wide as their widest value.
//!
//! Fixed `{:<N}` widths pushed a row out of line as soon as a value outgrew
//! its column (an RFC 3339 timestamp with nanoseconds is 35 characters), so
//! every table goes through here instead.
//!
//! Cells can be styled. Widths and padding are worked out from the text alone
//! and the style wraps just the text, so escape sequences never move a column.

use std::io::{self, Write};

use anstyle::Style;

/// Columns are separated by this much space.
const GAP: &str = "  ";

/// One value in a table.
pub(crate) struct Cell {
    text: String,
    style: Style,
}

impl Cell {
    pub(crate) fn plain(text: impl Into<String>) -> Self {
        Self::styled(text, Style::new())
    }

    pub(crate) fn styled(text: impl Into<String>, style: Style) -> Self {
        Self {
            text: text.into(),
            style,
        }
    }
}

/// Writes `headers` (in `header_style`) and then `rows`, one line each. Every
/// column but the last is padded to the widest value in it, header included;
/// the last column is written as it is, so no line ends in spaces. Width is
/// counted in characters, as `format!("{:<N}")` does, not in bytes.
///
/// A row shorter than the header leaves its remaining cells empty, and cells
/// beyond the header's columns are ignored.
pub(crate) fn write_table(
    out: &mut dyn Write,
    headers: &[&str],
    header_style: Style,
    rows: &[Vec<Cell>],
) -> io::Result<()> {
    let widths: Vec<usize> = headers
        .iter()
        .enumerate()
        .map(|(i, h)| {
            rows.iter()
                .filter_map(|r| r.get(i))
                .map(|c| c.text.chars().count())
                .fold(h.chars().count(), usize::max)
        })
        .collect();

    let header: Vec<Cell> = headers
        .iter()
        .map(|h| Cell::styled(*h, header_style))
        .collect();
    write_line(out, &widths, &header)?;
    for row in rows {
        write_line(out, &widths, row)?;
    }
    Ok(())
}

fn write_line(out: &mut dyn Write, widths: &[usize], cells: &[Cell]) -> io::Result<()> {
    let last = widths.len().saturating_sub(1);
    let mut line = String::new();
    for (i, width) in widths.iter().enumerate() {
        if i > 0 {
            line.push_str(GAP);
        }
        let Some(cell) = cells.get(i).filter(|c| !c.text.is_empty()) else {
            // Nothing to show; padding for the next column is still needed.
            if i < last {
                line.extend(std::iter::repeat_n(' ', *width));
            }
            continue;
        };
        let Cell { text, style } = cell;
        line.push_str(&format!("{style}{text}{style:#}"));
        if i < last {
            let pad = width.saturating_sub(text.chars().count());
            line.extend(std::iter::repeat_n(' ', pad));
        }
    }
    // An empty last cell would leave the padding of the one before it.
    writeln!(out, "{}", line.trim_end())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn table(headers: &[&str], rows: &[&[&str]]) -> String {
        let rows: Vec<Vec<Cell>> = rows
            .iter()
            .map(|r| r.iter().map(|c| Cell::plain(*c)).collect())
            .collect();
        let mut out = Vec::new();
        write_table(&mut out, headers, Style::new(), &rows).unwrap();
        String::from_utf8(out).unwrap()
    }

    #[test]
    fn a_value_wider_than_its_header_widens_the_column() {
        let out = table(
            &["AT", "ID"],
            &[&["2026-10-05T18:44:18+00:00", "a"], &["x", "b"]],
        );
        assert_eq!(
            out,
            "AT                         ID\n\
             2026-10-05T18:44:18+00:00  a\n\
             x                          b\n"
        );
    }

    #[test]
    fn a_header_wider_than_every_value_sets_the_width() {
        let out = table(&["DEVICE_FAMILY", "V"], &[&["UAP", "1"]]);
        assert_eq!(out, "DEVICE_FAMILY  V\nUAP            1\n");
    }

    #[test]
    fn the_last_column_is_not_padded() {
        let out = table(
            &["A", "B"],
            &[&["1", "short"], &["2", "a much longer value"]],
        );
        assert!(out.lines().all(|l| !l.ends_with(' ')), "{out:?}");
    }

    #[test]
    fn an_empty_last_cell_leaves_no_trailing_spaces() {
        let out = table(&["A", "B"], &[&["1", ""]]);
        assert_eq!(out, "A  B\n1\n");
    }

    #[test]
    fn width_counts_characters_not_bytes() {
        let out = table(&["NAME", "N"], &[&["café", "1"], &["ab", "2"]]);
        assert_eq!(out, "NAME  N\ncafé  1\nab    2\n");
    }

    #[test]
    fn short_rows_are_padded_and_extra_cells_are_ignored() {
        let out = table(&["A", "B", "C"], &[&["1"], &["2", "x", "y", "z"]]);
        assert_eq!(out, "A  B  C\n1\n2  x  y\n");
    }

    #[test]
    fn no_rows_still_prints_the_header() {
        assert_eq!(table(&["A", "B"], &[]), "A  B\n");
    }

    #[test]
    fn styles_wrap_the_text_and_leave_the_layout_alone() {
        let bold = Style::new().bold();
        let rows = vec![
            vec![Cell::styled("a", bold), Cell::plain("1")],
            vec![Cell::plain("longer"), Cell::styled("2", bold)],
        ];
        let mut out = Vec::new();
        write_table(&mut out, &["NAME", "N"], bold, &rows).unwrap();
        let styled = String::from_utf8(out).unwrap();
        assert!(styled.contains('\x1b'), "{styled:?}");

        // Without the escapes it is the table a plain run prints.
        let stripped = anstream::adapter::strip_str(&styled).to_string();
        assert_eq!(
            stripped,
            table(&["NAME", "N"], &[&["a", "1"], &["longer", "2"]])
        );
    }

    #[test]
    fn an_empty_styled_cell_writes_no_escapes_and_no_trailing_space() {
        let rows = vec![vec![
            Cell::plain("1"),
            Cell::styled("", Style::new().bold()),
        ]];
        let mut out = Vec::new();
        write_table(&mut out, &["A", "B"], Style::new(), &rows).unwrap();
        assert_eq!(String::from_utf8(out).unwrap(), "A  B\n1\n");
    }
}
