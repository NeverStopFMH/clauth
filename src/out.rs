//! Stdout that survives the reader leaving.
//!
//! Rust ignores `SIGPIPE`, so a write into a pipe whose reader is gone comes
//! back `EPIPE` and `println!` panics on it — `clauth sessions | head -3`
//! printed a Rust panic and exited 101 where a CLI should just stop. Restoring
//! the default disposition once at startup is the usual fix and the wrong one
//! for this binary: `std` sets no `MSG_NOSIGNAL` on Linux socket writes, so it
//! would also kill the daemon the first time an upstream hung up mid-request.
//! The handling lives at the emitter instead.
//!
//! [`outln!`] and [`out!`] are what the crate prints with; a bare `println!`
//! under `src/` is the bug this module exists to stop. Stderr keeps the `std`
//! macros on purpose — a `logline!` raised from a background thread costs that
//! thread alone today, and routing it through an exiting emitter would let a
//! closed stderr take the whole process down instead.

use std::fmt::Arguments;
use std::io::{ErrorKind, Write};

/// Whether a chunk reached its reader.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Wrote {
    /// The bytes landed.
    Yes,
    /// The reader closed the pipe. Nothing written after this can land either.
    ReaderGone,
}

/// Write one chunk, flushing when it carries no newline of its own.
///
/// Split out from [`emit`] so the `EPIPE` arm can be driven against a closed
/// writer without the exit taking the test process with it. A write error that
/// is not `EPIPE` — a full disk behind a redirect — still panics exactly as
/// `println!` did, because that one IS this run failing.
pub(crate) fn write_chunk<W: Write>(w: &mut W, args: Arguments<'_>, newline: bool) -> Wrote {
    let written = if newline {
        writeln!(w, "{args}")
    } else {
        // No newline means a prompt, and line buffering would hold it back
        // until the answer had already been typed blind.
        write!(w, "{args}").and_then(|()| w.flush())
    };
    match written {
        Ok(()) => Wrote::Yes,
        Err(e) if e.kind() == ErrorKind::BrokenPipe => Wrote::ReaderGone,
        Err(e) => panic!("failed printing to stdout: {e}"),
    }
}

/// [`outln!`] / [`out!`] backend — call the macros, not this.
///
/// A reader that left ends the run at exit 0: it is not this run failing, so a
/// pipeline reports whatever the reader returned. The exit is immediate and
/// runs no destructor, which every stdout writer in the crate is already clear
/// of — the session guards live inside `start::run`, which prints nothing.
pub(crate) fn emit(args: Arguments<'_>, newline: bool) {
    if write_chunk(&mut std::io::stdout().lock(), args, newline) == Wrote::ReaderGone {
        std::process::exit(0);
    }
}

/// One line on stdout: `println!` with a reader that is allowed to leave.
macro_rules! outln {
    ($($arg:tt)*) => {
        $crate::out::emit(::std::format_args!($($arg)*), true)
    };
}
pub(crate) use outln;

/// Stdout with no trailing newline, flushed — the prompt form of [`outln!`].
macro_rules! out {
    ($($arg:tt)*) => {
        $crate::out::emit(::std::format_args!($($arg)*), false)
    };
}
pub(crate) use out;

#[cfg(test)]
#[path = "../tests/inline/out.rs"]
mod tests;
