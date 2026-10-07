//! Plain-text tables whose columns are as wide as their widest value.
//!
//! Fixed `{:<N}` widths pushed a row out of line as soon as a value outgrew
//! its column (an RFC 3339 timestamp with nanoseconds is 35 characters), so
//! every table goes through here instead.

use std::io::{self, Write};

/// Columns are separated by this much space.
const GAP: &str = "  ";

/// Writes `headers` and then `rows`, one line each. Every column but the last
/// is padded to the widest value in it, header included; the last column is
/// written as it is, so no line ends in spaces. Width is counted in
/// characters, as `format!("{:<N}")` does, not in bytes.
///
/// A row shorter than the header leaves its remaining cells empty, and cells
/// beyond the header's columns are ignored.
pub(crate) fn write_table(
    out: &mut dyn Write,
    headers: &[&str],
    rows: &[Vec<String>],
) -> io::Result<()> {
    let widths: Vec<usize> = headers
        .iter()
        .enumerate()
        .map(|(i, h)| {
            rows.iter()
                .filter_map(|r| r.get(i))
                .map(|c| c.chars().count())
                .fold(h.chars().count(), usize::max)
        })
        .collect();

    write_line(out, &widths, headers.iter().copied())?;
    for row in rows {
        write_line(out, &widths, (0..widths.len()).map(|i| cell(row, i)))?;
    }
    Ok(())
}

fn cell(row: &[String], i: usize) -> &str {
    row.get(i).map_or("", String::as_str)
}

fn write_line<'a>(
    out: &mut dyn Write,
    widths: &[usize],
    cells: impl Iterator<Item = &'a str>,
) -> io::Result<()> {
    let last = widths.len().saturating_sub(1);
    let mut line = String::new();
    for (i, (text, width)) in cells.zip(widths).enumerate() {
        if i > 0 {
            line.push_str(GAP);
        }
        line.push_str(text);
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
        let rows: Vec<Vec<String>> = rows
            .iter()
            .map(|r| r.iter().map(ToString::to_string).collect())
            .collect();
        let mut out = Vec::new();
        write_table(&mut out, headers, &rows).unwrap();
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
}
