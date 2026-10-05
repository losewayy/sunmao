//! Cancellation endpoints — the durable flag plus the wake channel that
//! interrupts an in-flight await.
//!
//! Both halves have to be read together: the flag is the *memory* (it
//! survives the click, whenever it lands), the `Notify` is only the wake.
//! `Notify::notify_waiters` stores no permit, so a bare `notified()` drops
//! any cancel that arrived before the waiter registered — which is the
//! normal shape of a click, not an edge case. `CancelSignal::wait` is the
//! only correct way to await one: it reads the flag before registering and
//! again after, so a cancel that lands in either gap still returns.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use tokio::sync::Notify;

use super::Context;

/// A context's cancellation handle. Cheap to clone, safe to observe from
/// any task (the turn loop, a tool's blocking thread, a roster row).
#[derive(Clone)]
pub struct CancelSignal {
    flag: Arc<AtomicBool>,
    notify: Arc<Notify>,
}

impl CancelSignal {
    pub(crate) fn new(flag: Arc<AtomicBool>, notify: Arc<Notify>) -> Self {
        Self { flag, notify }
    }

    /// Request cancellation. The flag goes first, then the wake — the
    /// order `wait()` relies on: a waiter that missed the wake still reads
    /// the flag, and one that registers after it is caught by the recheck.
    pub fn cancel(&self) {
        self.flag.store(true, Ordering::Relaxed);
        self.notify.notify_waiters();
    }

    pub fn is_cancelled(&self) -> bool {
        self.flag.load(Ordering::Relaxed)
    }

    /// Resolve as soon as cancellation has been requested — at any moment,
    /// including before this call. Never resolves otherwise, so it is safe
    /// as a `select!` arm that must not fire.
    pub async fn wait(&self) {
        if self.is_cancelled() {
            return;
        }
        let woken = self.notify.notified();
        tokio::pin!(woken);
        // register, then read again: a cancel between the two reads wakes
        // this waiter; one before the first read is caught by it
        woken.as_mut().enable();
        if self.is_cancelled() {
            return;
        }
        woken.await;
    }
}

impl std::fmt::Debug for CancelSignal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CancelSignal")
            .field("cancelled", &self.is_cancelled())
            .finish()
    }
}

impl Context {
    /// This context's cancellation handle — one clone per consumer that
    /// must be able to abort (the turn loop's `select!`s, the shell's kill
    /// path, a sub-agent's roster row).
    pub fn cancel_signal(&self) -> CancelSignal {
        CancelSignal::new(self.cancelled.clone(), self.cancel_notify.clone())
    }
}
