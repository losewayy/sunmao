//! Declarative permission rules — `.sunmao/permissions.json` or the
//! `permissions` block in `.claude/settings*.json` (same contract).
//!
//! Rule syntax: `"Tool"` or `"Tool(specifier)"` — e.g. `"Bash"`,
//! `"Bash(npm *)"` (glob over the command), `"Read(./src/**)"` (glob path).
//! Semantics: deny wins over ask wins over allow; unmatched →
//! the runtime's default (approval gate for risky, allow otherwise).

use std::collections::HashMap;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Verdict {
    /// No rule matched — fall through to the runtime default (risk classifier).
    Default,
    /// An `allow` rule matched — skip the approval gate entirely.
    PreApproved,
    /// An `ask` rule matched — force interactive approval.
    Ask,
    /// A `deny` rule matched — hard refuse.
    Deny,
}

#[derive(Debug, serde::Deserialize, Default)]
struct PermsFile {
    #[serde(default)]
    permissions: Perms,
}

#[derive(Debug, serde::Deserialize, Default)]
struct Perms {
    #[serde(default)]
    allow: Vec<String>,
    #[serde(default)]
    ask: Vec<String>,
    #[serde(default)]
    deny: Vec<String>,
}

type RuleSet = (Vec<glob::Pattern>, Vec<glob::Pattern>, Vec<glob::Pattern>);

#[derive(Default)]
pub struct Permissions {
    rules: HashMap<String, RuleSet>,
}

impl Permissions {
    pub fn load(cwd: &Path, extra_roots: &[PathBuf]) -> Self {
        let mut paths = vec![
            cwd.join(".sunmao").join("permissions.json"),
            cwd.join(".claude").join("settings.json"),
            cwd.join(".claude").join("settings.local.json"),
        ];
        if let Some(home) = std::env::var_os("USERPROFILE").or_else(|| std::env::var_os("HOME")) {
            paths.push(Path::new(&home).join(".claude").join("settings.json"));
        }
        // preset dirs merge last — a `permissions.json` in an enabled preset
        // adds rules like any other layer (deny>ask>allow applies on check,
        // not on load order).
        for root in extra_roots {
            paths.push(root.join("permissions.json"));
        }
        let mut merged = Perms::default();
        for p in paths {
            if let Ok(text) = std::fs::read_to_string(&p)
                && let Ok(f) = serde_json::from_str::<PermsFile>(&text) {
                    merged.allow.extend(f.permissions.allow);
                    merged.ask.extend(f.permissions.ask);
                    merged.deny.extend(f.permissions.deny);
                }
        }
        Self::from_rules(merged)
    }

    fn from_rules(p: Perms) -> Self {
        // group rules per tool: "Tool(spec)" → tool=Tool, pattern=spec
        fn parse(rules: Vec<String>) -> HashMap<String, Vec<glob::Pattern>> {
            let mut m: HashMap<String, Vec<glob::Pattern>> = HashMap::new();
            for r in rules {
                if let Some((tool, spec)) = r.split_once('(') {
                    let spec = spec.trim_end_matches(')');
                    if let Ok(pat) = glob::Pattern::new(spec) {
                        m.entry(tool.trim().to_string()).or_default().push(pat);
                    }
                } else {
                    // bare tool name → match anything
                    if let Ok(pat) = glob::Pattern::new("*") {
                        m.entry(r.trim().to_string()).or_default().push(pat);
                    }
                }
            }
            m
        }
        let allow = parse(p.allow);
        let ask = parse(p.ask);
        let deny = parse(p.deny);
        let mut rules: HashMap<String, RuleSet> = HashMap::new();
        for tool in allow.keys().chain(ask.keys()).chain(deny.keys()) {
            rules.insert(
                tool.clone(),
                (
                    allow.get(tool).cloned().unwrap_or_default(),
                    ask.get(tool).cloned().unwrap_or_default(),
                    deny.get(tool).cloned().unwrap_or_default(),
                ),
            );
        }
        Self { rules }
    }

    /// Check a tool call: `specifier` is the command for Bash, path for file
    /// tools — whatever the rule glob should match against.
    /// deny > ask > allow; no matching rule → Allow (default flow decides).
    pub fn check(&self, tool: &str, specifier: &str) -> Verdict {
        let Some((allow, ask, deny)) = self.rules.get(tool) else {
            return Verdict::Default;
        };
        if deny.iter().any(|p| p.matches(specifier)) {
            Verdict::Deny
        } else if ask.iter().any(|p| p.matches(specifier)) {
            Verdict::Ask
        } else if allow.iter().any(|p| p.matches(specifier)) {
            Verdict::PreApproved
        } else {
            Verdict::Default
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn perms() -> Permissions {
        Permissions::from_rules(Perms {
            allow: vec!["Bash(cargo *)".into(), "Read".into()],
            ask: vec!["Bash(git push*)".into()],
            deny: vec!["Bash(rm -rf *)".into(), "Write(./src/**)".into()],
        })
    }

    #[test]
    fn deny_beats_ask_beats_allow() {
        let p = perms();
        assert_eq!(p.check("Bash", "rm -rf /"), Verdict::Deny);
        assert_eq!(p.check("Bash", "git push origin main"), Verdict::Ask);
        assert_eq!(p.check("Bash", "cargo test"), Verdict::PreApproved);
        // no rule matched → default flow
        assert_eq!(p.check("Bash", "echo hi"), Verdict::Default);
        // tool with no rules at all
        assert_eq!(p.check("Glob", "**/*"), Verdict::Default);
        // bare tool name matches everything
        assert_eq!(p.check("Read", "any/path"), Verdict::PreApproved);
        // path-scoped deny
        assert_eq!(p.check("Write", "./src/main.rs"), Verdict::Deny);
        assert_eq!(p.check("Write", "./other/x.rs"), Verdict::Default);
    }
}
