//! How long a foreground command may run.
//!
//! Its own file because it is policy, not plumbing: `shell.rs` is at the
//! repository's file budget, and this is the one place that decides the
//! default and the ceiling.

/// A caller may raise the default, but never past the ceiling: a model that
/// asks for an hour (or a year) must not park a shell forever — a hung
/// process holds the turn, the cursor and the user's only slot. Work that
/// genuinely runs longer belongs in the background path.
pub(crate) fn effective_timeout(requested: Option<u64>) -> u64 {
    const DEFAULT_TIMEOUT_SECS: u64 = 120;
    const MAX_TIMEOUT_SECS: u64 = 600;
    requested
        .unwrap_or(DEFAULT_TIMEOUT_SECS)
        .clamp(1, MAX_TIMEOUT_SECS)
}

/// A foreground command that reaches its budget is *moved to the background*
/// instead of being killed: a long build is legitimate work, it just must not
/// park the conversation on the user's only slot. The one trigger for that
/// move, and the only switch — `false` restores the old kill-on-timeout.
pub(crate) const AUTO_BACKGROUND_ON_TIMEOUT: bool = true;

/// The second tier a command gets once it has been moved to the background:
/// still a clock, but a longer one, so a runaway process can't sit there for
/// a day. `None` disables it. Deliberately NOT applied to an explicit
/// `background: true` spawn — that was asked for as a job, and jobs are
/// meant to outlive the turn with no clock of their own.
pub(crate) const BACKGROUND_TIMEOUT_SECS: Option<u64> = Some(600);

#[cfg(test)]
mod tests {
    use super::{AUTO_BACKGROUND_ON_TIMEOUT, BACKGROUND_TIMEOUT_SECS, effective_timeout};

    #[test]
    fn a_requested_timeout_is_clamped_to_the_ceiling() {
        assert_eq!(effective_timeout(None), 120);
        assert_eq!(effective_timeout(Some(30)), 30);
        assert_eq!(effective_timeout(Some(600)), 600);
        assert_eq!(
            effective_timeout(Some(86_400)),
            600,
            "a day must not park the shell"
        );
        assert_eq!(
            effective_timeout(Some(0)),
            1,
            "zero would kill before the spawn"
        );
    }

    /// The background tier is armed when the run is handed off — a FRESH
    /// clock, not the remainder of the foreground one — so it only owes a
    /// usable long-run window, longer than the foreground default it
    /// replaces. `None` is the documented "no clock at all".
    #[test]
    fn the_background_tier_is_a_longer_second_budget() {
        let auto = AUTO_BACKGROUND_ON_TIMEOUT;
        assert!(auto, "auto-background is the shipped default");
        if let Some(bg) = BACKGROUND_TIMEOUT_SECS {
            assert!(
                bg > effective_timeout(None),
                "a detached job must not get a shorter budget than a foreground default"
            );
        }
    }
}
