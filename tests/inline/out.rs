//! Stdout emitter tests. The `EPIPE` arm is driven through [`write_chunk`]
//! against writers that fail on demand — [`emit`] itself ends the process, so
//! nothing here can call it.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use super::*;
use std::io::{Error, ErrorKind};

/// A writer that fails every write with `kind`, the way a pipe whose reader has
/// exited fails a real one.
struct Failing(ErrorKind);

impl Write for Failing {
    fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
        Err(Error::from(self.0))
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Err(Error::from(self.0))
    }
}

/// A writer that takes the bytes but fails the flush, which is where a prompt's
/// `EPIPE` surfaces: the chunk carries no newline, so nothing forces it out
/// until the explicit flush.
struct FlushFails(ErrorKind);

impl Write for FlushFails {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Err(Error::from(self.0))
    }
}

/// The regression: `clauth sessions | head -3` exited 101 with a Rust panic on
/// stderr because `println!` panics on `EPIPE`. The reader leaving has to come
/// back as an outcome the caller can act on.
#[test]
fn a_closed_reader_is_an_outcome_not_a_panic() {
    let mut w = Failing(ErrorKind::BrokenPipe);
    assert_eq!(
        write_chunk(&mut w, format_args!("a line"), true),
        Wrote::ReaderGone
    );
    assert_eq!(
        write_chunk(&mut w, format_args!("prompt: "), false),
        Wrote::ReaderGone,
        "the prompt form reports it too"
    );
    assert_eq!(
        write_chunk(
            &mut FlushFails(ErrorKind::BrokenPipe),
            format_args!("p: "),
            false
        ),
        Wrote::ReaderGone,
        "including when only the flush is what hits the closed pipe"
    );
}

/// Only `EPIPE` is the reader leaving. Every other write failure is this run
/// failing, and swallowing one would turn a full disk behind a redirect into a
/// silent success.
#[test]
#[should_panic(expected = "failed printing to stdout")]
fn any_other_write_failure_still_panics() {
    write_chunk(
        &mut Failing(ErrorKind::StorageFull),
        format_args!("a line"),
        true,
    );
}

#[test]
fn a_reachable_reader_gets_the_bytes() {
    let mut buf: Vec<u8> = Vec::new();
    assert_eq!(
        write_chunk(&mut buf, format_args!("hi {}", 1), true),
        Wrote::Yes
    );
    assert_eq!(
        write_chunk(&mut buf, format_args!("p: "), false),
        Wrote::Yes
    );
    assert_eq!(String::from_utf8(buf).unwrap(), "hi 1\np: ");
}
