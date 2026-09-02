//! `StdoutSink`: the host's `ConsoleSink` for the CLI runner. Bytes the
//! guest writes through the SBI console (or, once wired, the 8250 THR) land
//! on this process's own stdout.
//!
//! This is exactly the `no_std`/`std` seam the design intends: `rv64`'s
//! `ConsoleSink` trait lets the core crate hand a byte to *something*
//! without ever touching a file descriptor itself. This is that something,
//! and it lives here, on the `std` side of the boundary.

use rv64::uart::ConsoleSink;
use std::io::Write;

/// Writes guest console bytes straight to stdout, flushing after every
/// byte.
///
/// `std::io::Stdout` is already line-buffered, so flushing only on `\n`
/// (the first cut of this) adds nothing over the default — the one case it
/// doesn't cover is a shell prompt, which is exactly `# ` with *no* trailing
/// newline. A guest that reaches a prompt under a line-buffered flush would
/// have written to the buffer and stopped there, leaving the screen blank:
/// indistinguishable from a hang, which is the one failure mode this
/// project cannot afford to produce silently. Flushing every byte costs
/// nothing next to the emulator's per-instruction work and removes that
/// failure mode entirely.
#[derive(Default)]
pub struct StdoutSink;

impl ConsoleSink for StdoutSink {
    fn put(&mut self, byte: u8) {
        write_and_flush(&mut std::io::stdout().lock(), byte);
    }
}

/// The actual write-then-flush, factored out so it can be exercised against
/// an in-memory `Write` in tests — `StdoutSink` itself talks to the
/// process's real stdout, which a unit test cannot observe.
fn write_and_flush<W: Write>(out: &mut W, byte: u8) {
    let _ = out.write_all(&[byte]);
    let _ = out.flush();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct RecordingWriter {
        bytes: Vec<u8>,
        /// Buffer length at the moment of each `flush()` call.
        flushed_after: Vec<usize>,
    }

    impl Write for RecordingWriter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.bytes.extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            self.flushed_after.push(self.bytes.len());
            Ok(())
        }
    }

    /// The regression this guards: a byte with no trailing newline — a
    /// shell prompt, above all — must be observable immediately, not sit in
    /// a buffer until the next newline or process exit.
    #[test]
    fn a_byte_with_no_trailing_newline_is_flushed_immediately() {
        let mut w = RecordingWriter::default();
        write_and_flush(&mut w, b'#');
        assert_eq!(
            w.flushed_after,
            vec![1],
            "a non-newline byte must still be flushed right after it's written"
        );
    }

    #[test]
    fn every_byte_in_a_sequence_is_flushed_on_its_own() {
        let mut w = RecordingWriter::default();
        for b in b"hi" {
            write_and_flush(&mut w, *b);
        }
        assert_eq!(w.bytes, b"hi");
        assert_eq!(w.flushed_after, vec![1, 2], "each byte must be flushed as it's written");
    }

    /// Not much else to unit-test about a thin stdout wrapper without
    /// capturing the process's real stdout — this just confirms
    /// `StdoutSink` itself satisfies the trait and does not panic.
    #[test]
    fn put_does_not_panic_on_ordinary_bytes() {
        let mut sink = StdoutSink;
        for b in b"hello\n" {
            sink.put(*b);
        }
    }
}
