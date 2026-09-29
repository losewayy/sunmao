//! Approval seam — the audit gate's active half.
//!
//! Risky tool calls (destructive shell patterns today) pause here for a
//! verdict. `AllowAll` is the non-interactive default; the REPL installs a
//! stdin prompter, and future ACP frontends map to `session/request_permission`.

/// Shell command risk patterns → why we ask. Substring matching is
/// intentionally conservative: hooks and the audit log record every check.
pub const RISKY_PATTERNS: &[(&str, &str)] = &[
    ("rm -rf", "recursive force delete"),
    ("rm -r ", "recursive delete"),
    ("git push", "publishes history"),
    ("git reset --hard", "discards work"),
    ("git clean", "deletes untracked files"),
    ("git checkout --", "discards changes"),
    ("git restore .", "discards changes"),
    ("del /s", "recursive delete"),
    ("del /f /s", "forced recursive delete"),
    ("rmdir /s", "recursive delete"),
    ("Remove-Item -Recurse", "recursive delete"),
    ("| sh", "piped-to-shell remote exec"),
    ("| bash", "piped-to-shell remote exec"),
    ("| pwsh", "piped-to-shell remote exec"),
    ("| iex", "piped-to-shell remote exec"),
    ("Invoke-Expression", "dynamic eval"),
    ("curl", "network egress — inspect before allowing"),
    ("wget ", "network egress"),
    ("shutdown", "system power"),
    ("reg delete", "registry mutation"),
    ("reg add", "registry mutation"),
    ("format ", "disk format"),
    ("mkfs", "disk format"),
    ("dd if=", "raw disk write"),
    ("chmod 777", "world-writable perms"),
    ("> /dev/", "raw device write"),
    ("taskkill /f", "force process kill"),
];

/// What a risky command matched, if anything.
pub fn classify(command: &str) -> Option<&'static str> {
    let c = command.to_lowercase();
    RISKY_PATTERNS
        .iter()
        .find(|(pat, _)| c.contains(&pat.to_lowercase()))
        .map(|(_, why)| *why)
}

#[async_trait::async_trait]
pub trait Approver: Send + Sync {
    /// Return true to permit. `detail` is human-readable (the command).
    async fn approve(&self, tool: &str, detail: &str, why: &str) -> bool;
}

/// Non-interactive default: allow everything, audit the decision.
pub struct AllowAll;

#[async_trait::async_trait]
impl Approver for AllowAll {
    async fn approve(&self, _tool: &str, _detail: &str, _why: &str) -> bool {
        true
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
