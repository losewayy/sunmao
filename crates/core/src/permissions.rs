//! Declarative permission rules — `.sunmao/permissions.json` or the
//! `permissions` block in `.claude/settings*.json` (same contract).
//!
//! Rule syntax: `"Tool"` or `"Tool(specifier)"` — e.g. `"Bash"`,
//! `"Bash(npm *)"` (glob over the command), `"Read(./src/**)"` (glob path).
//! Specifier extensions: `!` negates the entry (`Read(!**/.env)`), `re:`
//! compiles the rest as a regex (`Bash(re:^git (push|clone)\b)`); an
//! invalid regex degrades to a literal-string glob, same convention as
//! the hooks matcher. A bucket matches when some positive entry hits AND
//! no `!` entry hits — so `deny: ["Read(**/.env)", "Read(!**/.env.example")]`
//! refuses dotenv files but not the committed templates.
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

/// One specifier inside a rule — the compiled form of whatever the user
/// wrote after `Tool(`.
#[derive(Clone)]
enum Spec {
    /// Glob (`npm *`, `./src/**`) — the default shape.
    Glob(glob::Pattern),
    /// `re:<pattern>` — regex searched against the specifier. An invalid
    /// regex degrades to an exact-literal glob rather than silently
    /// dropping out of the table (same convention as the hooks matcher).
    Regex(regex::Regex),
    /// `!`-prefixed entry — vetoes a match of its bucket.
    Neg(Box<Spec>),
}

impl Spec {
    fn parse(spec: &str) -> Self {
        match spec.strip_prefix('!') {
            Some(rest) => Spec::Neg(Box::new(Self::positive(rest))),
            None => Self::positive(spec),
        }
    }

    fn positive(spec: &str) -> Self {
        if let Some(rest) = spec.strip_prefix("re:") {
            return match regex::Regex::new(rest) {
                Ok(re) => Spec::Regex(re),
                Err(e) => {
                    tracing::warn!(
                        "permission rule: bad regex {rest:?} ({e}) — matching literally"
                    );
                    Self::literal(rest)
                }
            };
        }
        match glob::Pattern::new(spec) {
            Ok(p) => Spec::Glob(p),
            Err(e) => {
                // an unparseable glob degrades to a literal too — a mangled
                // rule must not silently drop out of the deny table
                tracing::warn!("permission rule: bad glob {spec:?} ({e}) — matching literally");
                Self::literal(spec)
            }
        }
    }

    fn literal(spec: &str) -> Self {
        Spec::Glob(
            glob::Pattern::new(&glob::Pattern::escape(spec)).unwrap_or_else(|_| {
                // escape() output always parses; belt-and-suspenders fallback
                glob::Pattern::new("*").expect("' *' is a valid glob")
            }),
        )
    }

    fn hits(&self, specifier: &str) -> bool {
        match self {
            Spec::Glob(p) => p.matches(specifier),
            Spec::Regex(re) => re.is_match(specifier),
            Spec::Neg(inner) => inner.hits(specifier),
        }
    }
}

/// A rule bucket (one of allow/ask/deny): hits when a positive entry
/// matches AND no `!` entry vetoes the specifier.
fn bucket_match(specs: &[Spec], specifier: &str) -> bool {
    let vetoed = specs
        .iter()
        .any(|s| matches!(s, Spec::Neg(_)) && s.hits(specifier));
    !vetoed
        && specs
            .iter()
            .any(|s| !matches!(s, Spec::Neg(_)) && s.hits(specifier))
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

type RuleSet = (Vec<Spec>, Vec<Spec>, Vec<Spec>);

#[derive(Default)]
pub struct Permissions {
    rules: HashMap<String, RuleSet>,
}

/// Permission-rule source files in load order — shared by `load` (which
/// gates project-layer `allow` on the trust ledger) and the `/hooks`
/// roster (which lists them so `trust <n>` can pin one).
/// (path, layer) — project/plugin/preset layers need pins, the user layer
/// (~/.claude) is implicitly trusted like user-level hook files.
fn rule_files(cwd: &Path, extra_roots: &[PathBuf]) -> Vec<(PathBuf, crate::hooks::trust::Layer)> {
    use crate::hooks::trust::Layer;
    let mut out = vec![
        (cwd.join(".sunmao").join("permissions.json"), Layer::Project),
        (cwd.join(".claude").join("settings.json"), Layer::Project),
        (
            cwd.join(".claude").join("settings.local.json"),
            Layer::Project,
        ),
    ];
    if let Some(home) = std::env::var_os("USERPROFILE").or_else(|| std::env::var_os("HOME")) {
        out.push((
            Path::new(&home).join(".claude").join("settings.json"),
            Layer::User,
        ));
    }
    // preset dirs merge last — a `permissions.json` in an enabled preset
    // adds rules like any other layer (deny>ask>allow applies on check,
    // not on load order). Presets are a project-layer trust surface.
    for root in extra_roots {
        out.push((root.join("permissions.json"), Layer::Project));
    }
    out
}

/// Project-layer `allow` rules as `/hooks` rows — an `allow` short-circuits
/// the approval gate, so like hook commands it executes only once pinned.
/// `deny`/`ask` never widen a session and aren't listed or gated.
pub(crate) fn permission_rows(
    cwd: &Path,
    extra_roots: &[PathBuf],
) -> Vec<crate::hooks::trust::HookRow> {
    use crate::hooks::trust::{HookRow, Layer, RowKind, digest, is_trusted};
    let mut rows = Vec::new();
    for (path, layer) in rule_files(cwd, extra_roots) {
        if layer != Layer::Project {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        let Ok(f) = serde_json::from_str::<PermsFile>(&text) else {
            continue;
        };
        for rule in &f.permissions.allow {
            let status = if is_trusted(cwd, layer, &path, rule) {
                "pinned"
            } else {
                "untrusted"
            };
            rows.push(HookRow {
                kind: RowKind::Perm,
                event: "permissions.allow".into(),
                matcher: path
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| "permissions.json".into()),
                digest: digest(&path, rule),
                command: rule.clone(),
                pin_text: rule.clone(),
                source: path.clone(),
                status,
            });
        }
    }
    rows
}

impl Permissions {
    pub fn load(cwd: &Path, extra_roots: &[PathBuf]) -> Self {
        let mut merged = Perms::default();
        for (p, layer) in rule_files(cwd, extra_roots) {
            if let Ok(text) = std::fs::read_to_string(&p)
                && let Ok(f) = serde_json::from_str::<PermsFile>(&text)
            {
                // `allow` short-circuits the approval gate — a checked-out
                // repo could carry one, so project-layer allows apply only
                // once pinned in the trust ledger (same gate hook commands
                // pass). deny/ask always merge: they only refuse more.
                if layer == crate::hooks::trust::Layer::User {
                    merged.allow.extend(f.permissions.allow);
                } else {
                    for rule in f.permissions.allow {
                        if crate::hooks::trust::is_trusted(cwd, layer, &p, &rule) {
                            merged.allow.push(rule);
                        } else {
                            tracing::warn!(
                                "permissions: unpinned allow rule {rule:?} in {} skipped — \
                                 /hooks trust to enable",
                                p.display()
                            );
                        }
                    }
                }
                merged.ask.extend(f.permissions.ask);
                merged.deny.extend(f.permissions.deny);
            }
        }
        Self::from_rules(merged)
    }

    fn from_rules(p: Perms) -> Self {
        // group rules per tool: "Tool(spec)" → tool=Tool, pattern=spec
        fn parse(rules: Vec<String>) -> HashMap<String, Vec<Spec>> {
            let mut m: HashMap<String, Vec<Spec>> = HashMap::new();
            for r in rules {
                if let Some((tool, spec)) = r.split_once('(') {
                    let spec = spec.strip_suffix(')').unwrap_or(spec);
                    m.entry(tool.trim().to_string())
                        .or_default()
                        .push(Spec::parse(spec));
                } else {
                    // bare tool name → match anything
                    m.entry(r.trim().to_string())
                        .or_default()
                        .push(Spec::parse("*"));
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

    /// Split `permissions:` frontmatter entries into overlay buckets —
    /// `deny:`/`ask:` rules apply, `allow:` and unprefixed entries are
    /// ignored (counted for audit): a sub-agent's def can only narrow its
    /// own surface, never widen it.
    pub fn classify_overlay(entries: &[String]) -> (Vec<String>, Vec<String>, usize) {
        let mut ask = Vec::new();
        let mut deny = Vec::new();
        let mut ignored = 0;
        for e in entries {
            let e = e.trim();
            if let Some(r) = e.strip_prefix("deny:") {
                deny.push(r.to_string());
            } else if let Some(r) = e.strip_prefix("ask:") {
                ask.push(r.to_string());
            } else {
                ignored += 1;
            }
        }
        (ask, deny, ignored)
    }

    /// Overlay deny/ask rules onto a loaded set — used by agent defs
    /// (`permissions:` frontmatter, classified by `classify_overlay`).
    /// deny>ask>allow ordering makes the result strictly ≤ the base:
    /// nothing in the overlay can grant what the parent's table refused.
    pub fn with_deny_ask_overlay(&self, ask: &[String], deny: &[String]) -> Self {
        let extra = Self::from_rules(Perms {
            allow: Vec::new(),
            ask: ask.to_vec(),
            deny: deny.to_vec(),
        });
        let mut rules = self.rules.clone();
        for (tool, (_, ask_e, deny_e)) in extra.rules {
            let entry = rules.entry(tool).or_default();
            entry.1.extend(ask_e);
            entry.2.extend(deny_e);
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
        if bucket_match(deny, specifier) {
            Verdict::Deny
        } else if bucket_match(ask, specifier) {
            Verdict::Ask
        } else if bucket_match(allow, specifier) {
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

    #[test]
    fn negation_carves_exceptions_inside_a_bucket() {
        let p = Permissions::from_rules(Perms {
            allow: vec![],
            ask: vec![],
            deny: vec!["Read(**/.env)".into(), "Read(!**/.env.example)".into()],
        });
        assert_eq!(p.check("Read", "secrets/.env"), Verdict::Deny);
        assert_eq!(p.check("Read", ".env.example"), Verdict::Default);
        // a negation alone never matches — `!` can't conjure a bucket hit
        let only_neg = Permissions::from_rules(Perms {
            allow: vec![],
            ask: vec![],
            deny: vec!["Read(!**/keep/**)".into()],
        });
        assert_eq!(only_neg.check("Read", "any/path"), Verdict::Default);
    }

    #[test]
    fn negation_in_allow_still_loses_to_deny() {
        // deny>ask>allow is decided on buckets, not entries: a `deny` hit
        // outranks an `allow` hit even when the allow is vetoed by its own
        // `!` — and an unvetoed deny can never be talked down by allow.
        let p = Permissions::from_rules(Perms {
            allow: vec!["Bash(git *)".into(), "Bash(!git push*)".into()],
            ask: vec![],
            deny: vec!["Bash(git push*)".into()],
        });
        assert_eq!(p.check("Bash", "git push origin"), Verdict::Deny);
        assert_eq!(p.check("Bash", "git status"), Verdict::PreApproved);
    }

    #[test]
    fn regex_specifier() {
        let p = Permissions::from_rules(Perms {
            allow: vec![],
            ask: vec![],
            deny: vec![r"Bash(re:^git (push|clean)\b)".into()],
        });
        assert_eq!(p.check("Bash", "git push origin"), Verdict::Deny);
        assert_eq!(p.check("Bash", "git clean -fd"), Verdict::Deny);
        assert_eq!(p.check("Bash", "git pushback"), Verdict::Default);
        assert_eq!(p.check("Bash", "git status"), Verdict::Default);
        // negated regex — the exception form combine
        let q = Permissions::from_rules(Perms {
            allow: vec![],
            ask: vec![],
            deny: vec!["Bash(git *)".into(), "Bash(!re:^git (status|log)$)".into()],
        });
        assert_eq!(q.check("Bash", "git status"), Verdict::Default);
        assert_eq!(q.check("Bash", "git status --porcelain"), Verdict::Deny);
        assert_eq!(q.check("Bash", "git push"), Verdict::Deny);
    }

    #[test]
    fn invalid_regex_degrades_to_literal() {
        let p = Permissions::from_rules(Perms {
            allow: vec![],
            ask: vec![],
            deny: vec!["Bash(re:([)".into()],
        });
        // not silently dropped: the mangled rule matches only literally
        assert_eq!(p.check("Bash", "(["), Verdict::Deny);
        assert_eq!(p.check("Bash", "anything else"), Verdict::Default);
    }
}

#[cfg(test)]
mod overlay_tests {
    use super::*;

    #[test]
    fn overlay_denies_what_parent_allowed() {
        let parent = Permissions::from_rules(Perms {
            allow: vec!["Bash(rm *)".into(), "Write".into()],
            ask: vec![],
            deny: vec![],
        });
        assert_eq!(parent.check("Bash", "rm -rf x"), Verdict::PreApproved);
        let (ask, deny, ignored) = Permissions::classify_overlay(&[
            "deny:Bash(rm *)".into(),
            "deny:Write".into(),
            "ask:Edit".into(),
            // widening attempts — must not apply
            "allow:Bash(curl *)".into(),
            "Bash(echo *)".into(),
        ]);
        assert_eq!(ignored, 2);
        let child = parent.with_deny_ask_overlay(&ask, &deny);
        assert_eq!(child.check("Bash", "rm -rf x"), Verdict::Deny);
        assert_eq!(child.check("Write", "a.rs"), Verdict::Deny);
        assert_eq!(child.check("Edit", "a.rs"), Verdict::Ask);
        // ignored allow: parent's table didn't have it, overlay can't add
        assert_eq!(child.check("Bash", "curl example.com"), Verdict::Default);
    }

    #[test]
    fn overlay_cannot_undeny() {
        // parent denies; child rules can't reopen it — overlay only adds
        // deny/ask entries and deny already wins
        let parent = Permissions::from_rules(Perms {
            allow: vec![],
            ask: vec![],
            deny: vec!["Bash(rm *)".into()],
        });
        let child = parent.with_deny_ask_overlay(&[], &[]);
        assert_eq!(child.check("Bash", "rm -rf x"), Verdict::Deny);
        // even an ask on top of deny stays denied (deny > ask)
        let child2 = parent.with_deny_ask_overlay(&["Bash(rm *)".into()], &[]);
        assert_eq!(child2.check("Bash", "rm -rf x"), Verdict::Deny);
    }

    #[test]
    fn overlay_supports_new_specifiers() {
        let parent = Permissions::from_rules(Perms {
            allow: vec!["Bash".into()],
            ask: vec![],
            deny: vec![],
        });
        let (ask, deny, _) = Permissions::classify_overlay(&[
            "deny:Bash(git *)".into(),
            "deny:Bash(!git status*)".into(),
            r"ask:Bash(re:^cargo (publish|install)\b)".into(),
        ]);
        let child = parent.with_deny_ask_overlay(&ask, &deny);
        assert_eq!(child.check("Bash", "git push"), Verdict::Deny);
        assert_eq!(child.check("Bash", "git status"), Verdict::PreApproved);
        assert_eq!(child.check("Bash", "cargo publish"), Verdict::Ask);
        assert_eq!(child.check("Bash", "cargo test"), Verdict::PreApproved);
    }
}

#[cfg(test)]
mod trust_gate_tests {
    use super::*;

    /// An `allow` rule short-circuits the approval gate — a checked-out
    /// repo could carry one, so project-layer allows apply only once
    /// pinned in the trust ledger. deny/ask always merge: refusing more
    /// never widens the surface.
    #[test]
    fn project_allow_rules_need_a_trust_pin() {
        use crate::hooks::trust::{Layer, is_trusted, set_pin};
        let dir = crate::fresh_test_dir("perm-trust");
        let pdir = dir.join(".sunmao");
        std::fs::create_dir_all(&pdir).unwrap();
        let file = pdir.join("permissions.json");
        std::fs::write(
            &file,
            r#"{"permissions":{"allow":["Bash(rm *)"],"deny":["Bash(curl *)"],"ask":["Write"]}}"#,
        )
        .unwrap();
        // unpinned: the allow is skipped (the call stays gated), while
        // deny/ask still apply — they only narrow the surface
        let p = Permissions::load(&dir, &[]);
        assert_eq!(p.check("Bash", "rm -rf x"), Verdict::Default);
        assert_eq!(p.check("Bash", "curl x"), Verdict::Deny);
        assert_eq!(p.check("Write", "a.rs"), Verdict::Ask);
        // the roster carries the rule so /hooks trust can pin it
        let rows = permission_rows(&dir, &[]);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].status, "untrusted");
        // pinned: the digest covers (canonical source, rule text)
        set_pin(&dir, &file, "Bash(rm *)", true).unwrap();
        assert!(is_trusted(&dir, Layer::Project, &file, "Bash(rm *)"));
        let p = Permissions::load(&dir, &[]);
        assert_eq!(p.check("Bash", "rm -rf x"), Verdict::PreApproved);
        // a neighbouring rule isn't covered by this pin
        std::fs::write(
            &file,
            r#"{"permissions":{"allow":["Bash(rm *)","Bash(del *)"]}}"#,
        )
        .unwrap();
        let p = Permissions::load(&dir, &[]);
        assert_eq!(p.check("Bash", "rm -rf x"), Verdict::PreApproved);
        assert_eq!(p.check("Bash", "del x"), Verdict::Default);
        std::fs::remove_dir_all(&dir).ok();
    }
}
