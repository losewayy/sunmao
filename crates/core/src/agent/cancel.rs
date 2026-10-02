//! Cooperative cancellation — `AgentLoop::cancel` lives here because the
//! interrupt fan-out (flag → notify → sub-agents → approval drain →
//! detached Interrupt hook) is its own responsibility, separate from the
//! mod.rs orchestration surface.

use super::AgentLoop;
use crate::context::MutexRecover;

impl AgentLoop {
    /// Signal cooperative cancellation for the in-flight turn. Two hops:
    /// the flag (read at iteration boundaries) and `cancel_notify`
    /// (interrupts an in-flight stream delta or tool call via `select!`).
    /// Then cascade to live sub-agents — a running `Task` has its own
    /// Context; its Bash call would keep going after this turn died.
    /// Finished entries just notify into a dead loop — harmless.
    pub fn cancel(&self) {
        self.ctx
            .cancelled
            .store(true, std::sync::atomic::Ordering::Relaxed);
        self.ctx.cancel_notify.notify_waiters();
        for entry in self.ctx.live_tasks.lock_or_recover().iter() {
            if let Some(c) = &entry.cancel {
                c.cancel();
            }
        }
        // a parked approval card is a suspended ask() — without this drain
        // the dispatcher hangs past the turn-end reset on the pending rx
        self.ctx.approval.cancel_pending();
        // Interrupt hooks fire DETACHED — the cancel path is sync and the
        // user must never wait on a hook process to get their prompt back;
        // the outcome is observability-only by design. Gated on turn_lock:
        // cancelling an idle session is a no-op, firing there would spam
        // audit logs with interrupts that interrupted nothing.
        if self.ctx.turn_lock.try_lock().is_err() {
            crate::hooks::HookEngine::fire_detached(
                &self.ctx.hooks,
                crate::hooks::HookEvent::Interrupt,
                &self.ctx.cwd,
                &crate::hooks::HookInput::default(),
            );
        }
    }
}
