//! The few styles the commands use. Commands write them unconditionally;
//! whether they reach the terminal is decided once, on stdout, by `--color`.

use anstyle::Style;

/// Column headings.
pub(crate) const HEADER: Style = Style::new().bold();
/// Identifiers, hashes and times: there to be matched or copied, not read.
pub(crate) const DIM: Style = Style::new().dimmed();
/// The value a row is about.
pub(crate) const STRONG: Style = Style::new().bold();
