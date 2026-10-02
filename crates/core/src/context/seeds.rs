//! Session-seed helpers — read-back scans over a persisted log and the
//! per-tool watchdog table. Split from `mod.rs` so the context struct
//! and its constructor stay the file's single story. These are all
//! cold reads: they run once per Context build, never per turn.

/// Recover the task list a persisted log ended on: scan for `Todos`
/// event lines (prefiltered by the serializer's literal prefix) and take
/// the last one. Ephemeral logs and missing files seed empty — a fresh
/// session simply has no list yet.
pub(super) fn seed_todos(path: &std::path::Path) -> Vec<crate::tool::TodoItem> {
    if path.as_os_str().is_empty() {
        return Vec::new();
    }
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    for line in text.lines().rev() {
        if line.starts_with(crate::tool::TODOS_LINE_PREFIX)
            && let Ok(crate::session::SessionEvent::Todos { items }) =
                serde_json::from_str::<crate::session::SessionEvent>(line)
        {
            return items;
        }
    }
    Vec::new()
}

/// Per-tool watchdog seconds — `assets/tool-timeouts.txt` merged with
/// `.sunmao/tool-timeouts.txt` and plugin dirs, project rows overriding
/// builtins by name (risky-patterns stacks additively; timeouts are a
/// value, not a set, so merge means "last wins" not "append").
pub(crate) fn tool_timeout_table(cwd: &std::path::Path) -> std::collections::HashMap<String, u64> {
    let parse = |text: &str| {
        text.lines()
            .filter(|l| !l.trim_start().starts_with('#') && !l.trim().is_empty())
            .filter_map(|l| {
                let (name, secs) = l.split_once(" | ")?;
                secs.trim()
                    .parse::<u64>()
                    .ok()
                    .map(|s| (name.trim().to_string(), s))
            })
            .collect::<Vec<_>>()
    };
    let mut table: std::collections::HashMap<String, u64> =
        parse(include_str!("../../assets/tool-timeouts.txt"))
            .into_iter()
            .collect();
    let mut layers = vec![
        cwd.join(".sunmao/tool-timeouts.txt"),
        cwd.join(".sunmao/plugin/tool-timeouts.txt"),
    ];
    layers.extend(
        crate::sorted_entries(&cwd.join(".sunmao").join("plugins"))
            .into_iter()
            .chain(crate::sorted_entries(&cwd.join(".claude").join("plugins")))
            .map(|e| e.path().join("tool-timeouts.txt")),
    );
    for f in layers {
        if let Ok(text) = std::fs::read_to_string(&f) {
            table.extend(parse(&text));
        }
    }
    table
}

/// The approval stance a reopened log left behind — last `ModeChange` wins.
/// Same line-scan trick as `seed_todos`: cheap suffix read, no full fold.
pub(super) fn seed_mode(path: &std::path::Path) -> crate::agent::ApprovalMode {
    if path.as_os_str().is_empty() {
        return Default::default();
    }
    let Ok(text) = std::fs::read_to_string(path) else {
        return Default::default();
    };
    for line in text.lines().rev() {
        if line.contains("\"mode_change\"")
            && let Ok(crate::session::SessionEvent::ModeChange { mode }) =
                serde_json::from_str::<crate::session::SessionEvent>(line)
        {
            return mode;
        }
    }
    Default::default()
}
