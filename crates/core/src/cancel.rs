//! WHIQ: cooperative cancellation of the call in progress.
//!
//! A server thread asks with [`request`]; long-running code calls [`checkpoint`] at safe points
//! (each formula a recalculation evaluates, each row read, written or drawn, each step of a
//! workbook build). A checkpoint that sees the request unwinds with a [`Cancelled`] payload
//! (`resume_unwind`: no panic message), which the command runner catches like a failure: it
//! puts the workbook back as it was before the command and rebuilds the calculation state, so
//! the document is consistent. The runner clears the request before each call ([`clear`]).
//! Nothing asks for cancellation outside the MCP server, so the app never sees it.

use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::ThreadId;

/// Fast path: is any stop pending?
static PENDING: AtomicBool = AtomicBool::new(false);
/// The thread asked to stop (the MCP worker that runs the call).
static TARGET: Mutex<Option<ThreadId>> = Mutex::new(None);

/// The unwind payload of a cancelled call.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Cancelled;

fn target() -> std::sync::MutexGuard<'static, Option<ThreadId>> {
    TARGET.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Asks the call running on thread `thread` to stop at its next safe point (other threads are
/// not affected).
pub fn request(thread: ThreadId) {
    *target() = Some(thread);
    PENDING.store(true, Ordering::SeqCst);
}

/// Forgets a request (after each call).
pub fn clear() {
    *target() = None;
    PENDING.store(false, Ordering::SeqCst);
}

/// Has a stop been asked for on this thread?
pub fn requested() -> bool {
    PENDING.load(Ordering::Relaxed) && *target() == Some(std::thread::current().id())
}

/// A safe point: unwinds with [`Cancelled`] when a stop was asked for on this thread.
pub fn checkpoint() {
    if PENDING.load(Ordering::Relaxed) && requested() {
        std::panic::resume_unwind(Box::new(Cancelled));
    }
}

/// Is this unwind payload a cancellation?
pub fn is_cancelled(payload: &(dyn std::any::Any + Send)) -> bool {
    payload.is::<Cancelled>()
}
