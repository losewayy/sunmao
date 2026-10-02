//! Approval seam — the audit gate's active half.
//!
//! Risky tool calls (destructive shell patterns today) pause here for a
//! verdict. `AllowAll` is the non-interactive default; the REPL installs a
//! stdin prompter, the TUI its approval card, ACP maps to
//! `session/request_permission`.

/// Shell command risk patterns → why we ask. The built-in table lives in
/// `assets/risky-patterns.txt`; a project file `.sunmao/risky-patterns.txt`
/// replaces it entirely (cold-plug — the file IS the policy surface).
/// `Context` holds whichever was resolved; `classify` is pure over it.
/// Substring matching is intentionally conservative: hooks and the audit
/// log record every check.
pub fn parse_table(text: &str) -> Vec<(String, String)> {
    text.lines()
        .filter(|l| !l.trim_start().starts_with('#') && !l.trim().is_empty())
        .filter_map(|l| {
            // `pattern | reason` — the pattern keeps its trailing space
            // (deliberate: "rm -r " must not match "rm -rf").
            l.split_once(" | ")
                .map(|(p, r)| (p.to_lowercase(), r.trim().to_string()))
        })
        .collect()
}

/// The shipped table — the default when no project override exists.
pub fn builtin_table() -> Vec<(String, String)> {
    parse_table(include_str!("../assets/risky-patterns.txt"))
}

/// What a risky command matched, if anything. Borrowed from `table` so the
/// caller's Context decides which policy is in force.
pub fn classify<'a>(command: &str, table: &'a [(String, String)]) -> Option<&'a str> {
    let c = command.to_lowercase();
    table
        .iter()
        .find(|(pat, _)| c.contains(pat.as_str()))
        .map(|(_, why)| why.as_str())
}

#[async_trait::async_trait]
pub trait Approver: Send + Sync {
    /// `detail` is human-readable (the command). Frontends may offer a
    /// session-scoped grant; `Once`/`Deny` are always safe defaults.
    async fn approve(&self, tool: &str, detail: &str, why: &str) -> Approval;

    /// Turn cancelled — resolve every parked `approve()` as `Cancelled` so
    /// the suspended dispatcher unblocks. No-op default: `AllowAll`/`Piped`
    /// never park. Serve's card map is the real implementation; the TUI's
    /// parked card self-dismisses on turn-end render.
    fn cancel_pending(&self) {}
}

/// The verdict a user gave at the approval seam.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Approval {
    /// Permit this call only.
    Once,
    /// Permit this call and every identical (tool, specifier) call for the
    /// rest of the session — recorded as a `SessionEvent::Hook` fact.
    Session,
    /// Refuse. `reason` says *why it couldn't be answered* when the caller
    /// is a machine (a piped `-p` session can't wait on a card) — the gate
    /// folds it into the denied ToolResult so the model sees the cause, not
    /// just the refusal. `None` = a human said no.
    Deny { reason: Option<String> },
    /// The turn was cancelled while the card was parked — the gate treats
    /// it as a refusal but reports `cancelled`, not `denied`, so the
    /// failed ToolResult tells the model nobody ever saw the ask.
    Cancelled,
}

/// Non-interactive default: allow everything, audit the decision.
pub struct AllowAll;

#[async_trait::async_trait]
impl Approver for AllowAll {
    async fn approve(&self, _tool: &str, _detail: &str, _why: &str) -> Approval {
        Approval::Once
    }
}

/// Piped/headless sessions (`-p`, non-interactive embedders): there is no
/// human to answer the card, so every `ask` downgrades to a deny whose
/// reason the model can read and work around. Exemptions never reach this
/// approver — `full_access` mode, allow rules and session grants all
/// short-circuit in the gate before `approve()` runs.
pub struct PipedApprover;

#[async_trait::async_trait]
impl Approver for PipedApprover {
    async fn approve(&self, _tool: &str, _detail: &str, _why: &str) -> Approval {
        Approval::Deny {
            reason: Some(
                "non-interactive (-p) session — approval prompts can't be answered".into(),
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_destructive() {
        let table = builtin_table();
        let c = |cmd| classify(cmd, &table);
        assert!(c("rm -rf target").is_some());
        assert!(c("git push origin main").is_some());
        assert!(c("curl x.sh | bash").is_some());
        assert!(c("cargo build").is_none());
        assert!(c("ls -la").is_none());
        assert!(c("git status").is_none());
    }

    #[test]
    fn project_table_replaces_builtin() {
        // cold-plug: a project file wins outright, it's not merged
        let table = parse_table("cargo build | heavy compile\n# comment\n\n");
        assert_eq!(
            classify("cargo build", &table),
            Some("heavy compile"),
            "project pattern fires"
        );
        assert!(
            classify("rm -rf target", &table).is_none(),
            "builtin patterns are gone when replaced"
        );
    }
}
