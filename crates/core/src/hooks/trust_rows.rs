//! Roster rows that aren't hook commands — the two other trust surfaces
//! `/hooks` must show and the gate must pin: privilege-bearing config
//! files (`loop:` manifests, risk/readonly tables) and spawn specs
//! (extensions, MCP transports). Same ledger, same digest rule; split
//! from `trust.rs` only by scan surface.

use std::path::{Path, PathBuf};

use super::trust::{HookRow, Layer, RowKind, digest, is_trusted, spec_display, spec_text};

/// Privilege-bearing config files, as roster rows. Three surfaces can
/// *widen* the session from a plain file — a manifest's `loop:` pick
/// (choosing `bare` removes the entire gate), the project
/// `risky-patterns.txt` (wholesale-replaces the builtin ask table), and
/// any `readonly-verbs.txt` (widens what read-only runs). Each needs the
/// same pin an `allow` rule or hook command does; the pin_text is the
/// verbatim claim — `loop:<name>` or the file's bytes — so editing it
/// invalidates trust exactly like editing a command does.
pub(crate) fn config_rows(cwd: &Path, extra_roots: &[PathBuf]) -> Vec<HookRow> {
    let mut rows = Vec::new();
    let row =
        |event: &str, matcher: String, source: PathBuf, command: String, pin: String| HookRow {
            kind: RowKind::Perm,
            event: event.into(),
            matcher,
            command,
            status: if is_trusted(cwd, Layer::Project, &source, &pin) {
                "pinned"
            } else {
                "untrusted"
            },
            digest: digest(&source, &pin),
            pin_text: pin,
            source,
        };
    // the same scan LoopDriver::resolve does — project-layer manifests only
    // (a preset root outside the project is user-invoked, implicit trust)
    let ccwd = cwd.canonicalize().unwrap_or_else(|_| cwd.to_path_buf());
    let mut manifests = vec![
        cwd.join(".sunmao").join("plugin.json"),
        cwd.join(".claude-plugin").join("plugin.json"),
    ];
    for base in [
        cwd.join(".sunmao").join("plugins"),
        cwd.join(".claude").join("plugins"),
    ] {
        for e in crate::sorted_entries(&base) {
            if e.path().is_dir() {
                manifests.push(e.path().join("plugin.json"));
            }
        }
    }
    manifests.extend(extra_roots.iter().filter_map(|r| {
        let p = r.join("plugin.json");
        p.canonicalize()
            .unwrap_or_else(|_| p.clone())
            .starts_with(&ccwd)
            .then_some(p)
    }));
    for m in manifests {
        let Ok(text) = std::fs::read_to_string(&m) else {
            continue;
        };
        let Some(name) = serde_json::from_str::<serde_json::Value>(&text)
            .ok()
            .and_then(|f| f.get("loop").and_then(|l| l.as_str()).map(str::to_string))
        else {
            continue;
        };
        rows.push(row(
            "loop",
            m.display().to_string(),
            m,
            format!("\"loop\": \"{name}\""),
            format!("loop:{name}"),
        ));
    }
    let rp = cwd.join(".sunmao/risky-patterns.txt");
    if let Ok(text) = std::fs::read_to_string(&rp) {
        rows.push(row(
            "risky-patterns",
            rp.display().to_string(),
            rp,
            "replaces the builtin ask table".into(),
            text,
        ));
    }
    for f in [
        cwd.join(".sunmao/readonly-verbs.txt"),
        cwd.join(".sunmao/plugin/readonly-verbs.txt"),
    ]
    .into_iter()
    .chain(
        crate::sorted_entries(&cwd.join(".sunmao").join("plugins"))
            .into_iter()
            .chain(crate::sorted_entries(&cwd.join(".claude").join("plugins")))
            .map(|e| e.path().join("readonly-verbs.txt")),
    ) {
        if let Ok(text) = std::fs::read_to_string(&f) {
            rows.push(row(
                "readonly-verbs",
                f.display().to_string(),
                f,
                "widens the read-only verb set".into(),
                text,
            ));
        }
    }
    rows
}

/// The spawn half of the `/hooks` roster: every ext spec and every MCP
/// `command:` spec the session *would* launch, as rows alongside the hook
/// commands. Built live from the same scans `connect_all` runs, so the
/// listing can never drift from what the gate sees.
pub(crate) fn spawn_rows(cwd: &Path, extra_roots: &[PathBuf]) -> Vec<HookRow> {
    let row = |kind: RowKind,
               event: &str,
               name: String,
               source: PathBuf,
               text: String,
               display: String| {
        let status = if is_trusted(cwd, Layer::Project, &source, &text) {
            "pinned"
        } else {
            "untrusted"
        };
        HookRow {
            kind,
            event: event.to_string(),
            matcher: name,
            digest: digest(&source, &text),
            command: display,
            pin_text: text,
            source,
            status,
        }
    };
    let mut rows = Vec::new();
    for (manifest, spec, plugin) in crate::ext::resolve_specs(cwd, extra_roots) {
        rows.push(row(
            RowKind::Ext,
            "ext:spawn",
            plugin,
            manifest,
            spec_text(&spec.command, &spec.args, &spec.env),
            spec_display(&spec.command, &spec.args, &spec.env),
        ));
    }
    let mut servers: Vec<_> = crate::mcp::resolve_servers(cwd, extra_roots)
        .into_iter()
        .collect();
    servers.sort_by(|a, b| a.0.cmp(&b.0));
    for (name, (spec, source)) in servers {
        // every transport is pinnable — a url spec hands tool outputs and
        // call arguments to a remote, the same trust surface a spawned
        // child is. The pin covers both halves (spec_text_mcp), so a
        // command pin can't drift into an unpinned remote.
        if spec.command.is_none() && spec.url.is_none() {
            continue;
        }
        rows.push(row(
            RowKind::Mcp,
            "mcp:connect",
            name,
            source,
            crate::mcp::spec_text_mcp(&spec),
            crate::mcp::spec_display_mcp(&spec),
        ));
    }
    rows
}
