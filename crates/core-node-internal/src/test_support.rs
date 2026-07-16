//! Test-only scaffolding shared between this crate's unit tests and
//! downstream crates' tests (via the `test-support` feature).

use std::io::Write;
use std::sync::Arc;

use tracing_subscriber::fmt::MakeWriter;

/// Cloneable in-memory sink for `tracing_subscriber`: pass a clone to
/// `with_writer` and read back everything logged via [`LogCapture::logs`].
#[derive(Clone, Default)]
pub struct LogCapture {
    buffer: Arc<parking_lot::Mutex<Vec<u8>>>,
}

impl LogCapture {
    pub fn new() -> Self {
        Self::default()
    }

    /// Snapshot of everything captured so far.
    pub fn logs(&self) -> String {
        String::from_utf8(self.buffer.lock().clone()).expect("captured logs are valid UTF-8")
    }
}

// The capture is its own writer: clones share one buffer, so the subscriber
// can take a writer per event while readers snapshot the same log.
impl Write for LogCapture {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.buffer.lock().extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> MakeWriter<'a> for LogCapture {
    type Writer = LogCapture;

    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}
