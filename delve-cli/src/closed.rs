//! Telling a closed stdout from any other broken pipe.
//!
//! `delve catalog | head -1` closes stdout after the first line, which is not
//! a failure. A broken pipe from a webhook or an SMTP connection is, and has
//! the same `io::ErrorKind`, so the kind alone can't tell them apart. Output
//! goes through [`Stdout`], which tags the errors of the stdout writes
//! themselves; [`is_closed_stdout`] looks for the tag.

use std::fmt;
use std::io::{self, ErrorKind, Write};

/// The payload of an `io::Error` for a write that found stdout closed.
#[derive(Debug)]
struct StdoutClosed;

impl fmt::Display for StdoutClosed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("stdout was closed")
    }
}

impl std::error::Error for StdoutClosed {}

/// A writer on standard output, whose `BrokenPipe` errors carry the
/// [`StdoutClosed`] tag.
pub(crate) struct Stdout<W>(W);

impl<W: Write> Stdout<W> {
    pub(crate) fn new(inner: W) -> Self {
        Self(inner)
    }
}

fn tag(err: io::Error) -> io::Error {
    if err.kind() == ErrorKind::BrokenPipe {
        io::Error::new(ErrorKind::BrokenPipe, StdoutClosed)
    } else {
        err
    }
}

impl<W: Write> Write for Stdout<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.write(buf).map_err(tag)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.0.flush().map_err(tag)
    }
}

/// Whether `err`, or anything it wraps, is a write that found stdout closed.
pub(crate) fn is_closed_stdout(err: &anyhow::Error) -> bool {
    err.chain().any(|cause| {
        cause
            .downcast_ref::<io::Error>()
            .and_then(io::Error::get_ref)
            .is_some_and(|inner| inner.is::<StdoutClosed>())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn broken_pipe() -> io::Error {
        io::Error::from(ErrorKind::BrokenPipe)
    }

    #[test]
    fn a_tagged_broken_pipe_is_a_closed_stdout() {
        let err = anyhow::Error::new(tag(broken_pipe())).context("writing the table");
        assert!(is_closed_stdout(&err));
    }

    #[test]
    fn a_broken_pipe_from_elsewhere_is_not() {
        let err = anyhow::Error::new(broken_pipe()).context("posting the webhook");
        assert!(!is_closed_stdout(&err));
    }

    #[test]
    fn other_stdout_errors_are_not() {
        let err = anyhow::Error::new(tag(io::Error::from(ErrorKind::PermissionDenied)));
        assert!(!is_closed_stdout(&err));
    }
}
