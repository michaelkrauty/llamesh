//! Helpers shared by unit tests.

use std::sync::{Arc, Mutex};
use tracing::subscriber::DefaultGuard;
use tracing::Dispatch;

/// Log output captured on the current thread; see [`capture_logs`].
pub struct LogCapture {
    output: Arc<Mutex<Vec<u8>>>,
    _default: DefaultGuard,
    _second_dispatcher: Dispatch,
}

impl LogCapture {
    /// The JSON log lines captured so far.
    pub fn contents(&self) -> String {
        String::from_utf8(self.output.lock().unwrap().clone()).unwrap()
    }
}

#[derive(Clone)]
struct SharedBuffer(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for SharedBuffer {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Capture events at `max_level` and above, as JSON lines, from the current
/// thread until the returned [`LogCapture`] is dropped. Use it from a
/// current-thread runtime (a plain `#[tokio::test]`), so that the code under
/// test logs on this thread.
///
/// tracing caches whether any subscriber is interested in a callsite when the
/// callsite is first hit. While only one dispatcher exists, tracing-core
/// assumes it is the global default and asks the dispatcher of the thread that
/// hits the callsite; a test running in parallel on a thread without a
/// subscriber then caches the callsite as uninteresting, and this thread's
/// subscriber never sees its events. A second, silent dispatcher, kept alive as
/// long as the capture, makes tracing-core ask every live dispatcher instead.
pub fn capture_logs(max_level: tracing::Level) -> LogCapture {
    let second_dispatcher = Dispatch::new(tracing::subscriber::NoSubscriber::default());
    let output = Arc::new(Mutex::new(Vec::new()));
    let writer = SharedBuffer(output.clone());
    let subscriber = tracing_subscriber::fmt()
        .json()
        .with_max_level(max_level)
        .with_writer(move || writer.clone())
        .finish();
    LogCapture {
        output,
        _default: tracing::subscriber::set_default(subscriber),
        _second_dispatcher: second_dispatcher,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn log_from_shared_callsite() {
        tracing::warn!("from the shared callsite");
    }

    #[test]
    fn captures_callsites_first_hit_on_another_thread() {
        let capture = capture_logs(tracing::Level::WARN);
        // The other thread has no subscriber and registers the callsite first.
        std::thread::spawn(log_from_shared_callsite).join().unwrap();
        log_from_shared_callsite();

        let logged = capture.contents();
        assert_eq!(
            logged.matches("from the shared callsite").count(),
            1,
            "expected exactly this thread's event: {logged}"
        );
    }
}
