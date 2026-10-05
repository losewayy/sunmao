//! Cooperative cancellation — `AgentLoop::cancel` lives here because the
//! interrupt fan-out (flag → notify → sub-agents → approval drain →
//! detached Interrupt hook) is its own responsibility, separate from the
//! mod.rs orchestration surface.

use super::AgentLoop;
use crate::context::MutexRecover;

/// How long a cooperative cancel may take before the round force-ends
/// itself (`turn::chain` races the driver against this deadline). It sits
/// above the slowest *legitimate* cooperative exit — a SIGKILLed tool still
/// reaps its pipes, bounded at `tool::PIPE_DRAIN_TIMEOUT` (5s) — so the
/// backstop never preempts a stop that is already working, and far below
/// the 120s default shell budget, so a tool that ignores the signal cannot
/// park the turn for its whole allowance.
pub(crate) const HARD_STOP_GRACE: std::time::Duration = std::time::Duration::from_secs(10);

impl AgentLoop {
    /// Signal cooperative cancellation for the in-flight turn. Two hops:
    /// the flag (read at iteration boundaries) and `cancel_notify`
    /// (interrupts an in-flight stream delta or tool call via `select!`).
    /// Then cascade to live sub-agents — a running `Task` has its own
    /// Context; its Bash call would keep going after this turn died.
    /// Finished entries just notify into a dead loop — harmless.
    pub fn cancel(&self) {
        for entry in self.ctx.live_tasks.lock_or_recover().iter() {
            if let Some(c) = &entry.cancel {
                c.cancel();
            }
        }
        self.cancel_main();
    }

    /// `cancel()` minus the sub-agent cascade — the IM frontend's `/stop`.
    /// Stopping the main agent is the only thing a channel can express;
    /// sub-agents keep running (their `TaskDone` write-backs land in the
    /// log as always). The pointed version for one child is `cancel_sub`.
    /// Idempotent by construction: `CancelSignal::cancel` only sets a flag
    /// and wakes waiters, so a second click is a no-op, not an error.
    pub fn cancel_main(&self) {
        self.ctx.cancel_signal().cancel();
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
