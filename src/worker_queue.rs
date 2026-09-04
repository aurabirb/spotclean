//! A single background thread that serially drains a channel, used to collapse "spawn a thread
//! per request" patterns into one long-lived worker.

use std::sync::mpsc;
use std::thread;

use log::error;

/// Spawn a thread named `name` that serially applies `handle` to each item taken off `queue`.
/// Used to turn "do this potentially-slow thing for every request" into a single background
/// worker instead of a thread per request.
///
/// A panic while handling one item is caught and logged rather than killing the worker: without
/// this, one bad item (e.g. an API response that doesn't parse the way we expect) would silently
/// stop the queue forever, since nothing restarts these threads.
pub(crate) fn spawn_queue_worker<T: Send + 'static>(
    name: &'static str,
    queue: mpsc::Receiver<T>,
    mut handle: impl FnMut(T) + Send + 'static,
) {
    thread::Builder::new()
        .name(name.into())
        .spawn(move || {
            for item in queue {
                if let Err(panic) =
                    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| handle(item)))
                {
                    let message = panic
                        .downcast_ref::<&str>()
                        .map(|s| s.to_string())
                        .or_else(|| panic.downcast_ref::<String>().cloned())
                        .unwrap_or_else(|| "unknown panic payload".to_string());
                    error!("{name} panicked while processing an item, continuing: {message}");
                }
            }
        })
        .unwrap_or_else(|e| panic!("failed to spawn {name} thread: {e}"));
}
