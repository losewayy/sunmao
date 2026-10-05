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

#[cfg(test)]
mod tests {
    use super::effective_timeout;

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
}
