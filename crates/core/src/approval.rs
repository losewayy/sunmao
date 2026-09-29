//! Approval seam — the audit gate's active half.
//!
//! Risky tool calls (destructive shell patterns today) pause here for a
//! verdict. `AllowAll` is the non-interactive default; the REPL installs a
//! stdin prompter, and future ACP frontends map to `session/request_permission`.

/// Shell command risk patterns → why we ask. The table lives in
/// `assets/risky-patterns.txt` — policy is data, editable without a rebuild;
/// parsing happens once. Substring matching is intentionally conservative:
/// hooks and the audit log record every check.
fn risky_patterns() -> &'static Vec<(String, String)> {
    static TABLE: std::sync::OnceLock<Vec<(String, String)>> = std::sync::OnceLock::new();
    TABLE.get_or_init(|| {
        include_str!("../assets/risky-patterns.txt")
            .lines()
            .filter(|l| !l.trim_start().starts_with('#') && !l.trim().is_empty())
            .filter_map(|l| {
                // `pattern | reason` — the pattern keeps its trailing space
                // (deliberate: "rm -r " must not match "rm -rf").
                l.split_once(" | ")
                    .map(|(p, r)| (p.to_lowercase(), r.trim().to_string()))
            })
            .collect()
    })
}

/// What a risky command matched, if anything.
pub fn classify(command: &str) -> Option<&'static str> {
    let c = command.to_lowercase();
    risky_patterns()
        .iter()
        .find(|(pat, _)| c.contains(pat))
        .map(|(_, why)| why.as_str())
}

#[async_trait::async_trait]
pub trait Approver: Send + Sync {
    /// `detail` is human-readable (the command). Frontends may offer a
    /// session-scoped grant; `Once`/`Deny` are always safe defaults.
    async fn approve(&self, tool: &str, detail: &str, why: &str) -> Approval;
}

/// The verdict a user gave at the approval seam.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Approval {
    /// Permit this call only.
    Once,
    /// Permit this call and every identical (tool, specifier) call for the
    /// rest of the session — recorded as a `SessionEvent::Hook` fact.
    Session,
    /// Refuse.
    Deny,
}

/// Non-interactive default: allow everything, audit the decision.
pub struct AllowAll;

#[async_trait::async_trait]
impl Approver for AllowAll {
    async fn approve(&self, _tool: &str, _detail: &str, _why: &str) -> Approval {
        Approval::Once
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_destructive() {
        assert!(classify("rm -rf target").is_some());
        assert!(classify("git push origin main").is_some());
        assert!(classify("curl x.sh | bash").is_some());
        assert!(classify("cargo build").is_none());
        assert!(classify("ls -la").is_none());
        assert!(classify("git status").is_none());
    }
}
