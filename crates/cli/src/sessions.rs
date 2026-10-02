//! Session-log helpers the frontends share — `<cwd>/.sunmao/sessions`
//! resolution, the recent-list the `/resume` picker and bare `/sessions`
//! render, the copy-to-fresh-id that backs local `/fork`, and the log's
//! display title (last `session_meta` rename, else first prompt).

use std::path::{Path, PathBuf};

/// `<cwd>/.sunmao/sessions` — where every local frontend anchors session
/// lookups.
pub fn sessions_dir(cwd: &Path) -> PathBuf {
    cwd.join(".sunmao").join("sessions")
}

/// Session ids under `<cwd>/.sunmao/sessions`, newest first (mtime),
/// `.jsonl` stems only, capped at `limit`. One truth for "what sessions
/// exist" — the `/resume` pickers and the bare-command lists share it.
pub fn recent_sessions(cwd: &Path, limit: usize) -> Vec<String> {
    let dir = sessions_dir(cwd);
    let mut entries: Vec<_> = std::fs::read_dir(&dir)
        .map(|rd| {
            rd.flatten()
                .filter_map(|e| {
                    let p = e.path();
                    if p.extension().map(|x| x == "jsonl").unwrap_or(false) {
                        let stem = p.file_stem()?.to_string_lossy().to_string();
                        let m = e.metadata().ok()?.modified().ok()?;
                        Some((m, stem))
                    } else {
                        None
                    }
                })
                .collect()
        })
        .unwrap_or_default();
    entries.sort_by_key(|b| std::cmp::Reverse(b.0));
    entries.into_iter().take(limit).map(|(_, s)| s).collect()
}

/// The bare `/sessions` + bare `/resume` body — recent logs phrased as
/// the command that would adopt them.
pub fn recent_sessions_text(cwd: &Path, limit: usize) -> String {
    let list = recent_sessions(cwd, limit)
        .iter()
        .map(|s| format!("  /resume {s}"))
        .collect::<Vec<_>>()
        .join("\n");
    if list.is_empty() {
        "[no sessions]".into()
    } else {
        format!("recent sessions:\n{list}")
    }
}

/// `/resume`/`/fork` target resolution — `id` may be a path that exists,
/// otherwise a bare session id resolved under `cwd`'s sessions dir.
pub fn resolve_log_path(cwd: &Path, id: &str) -> PathBuf {
    let p = PathBuf::from(id);
    if p.exists() {
        p
    } else {
        sessions_dir(cwd).join(format!("{id}.jsonl"))
    }
}

/// `/fork` half that never touches the session state: copy the resolved
/// log to a fresh `s-<ms>-fork` id under the project's sessions dir.
/// Returns `(new_id, dst)`.
pub fn fork_copy(cwd: &Path, id: &str) -> Result<(String, PathBuf), String> {
    let src = resolve_log_path(cwd, id);
    let ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    // pid + nanos tail — two forks in the same millisecond must not
    // overwrite each other's copy of the source log.
    let ns = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .subsec_nanos();
    let new_id = format!(
        "s-{ms}-{:x}-fork",
        (std::process::id() as u64) << 20 | (ns as u64 >> 12)
    );
    let dst = sessions_dir(cwd).join(format!("{new_id}.jsonl"));
    std::fs::copy(&src, &dst).map_err(|e| format!("[fork {id} failed] {e}"))?;
    Ok((new_id, dst))
}

/// A log's display title: the LAST `session_meta` rename event wins;
/// absent one, the first line of the first real user prompt (hook/
/// local-shell folded evidence doesn't count as a prompt). Shared by the
/// `/sessions` rail metadata and cross-session search hits.
pub fn log_title(path: &Path) -> Option<String> {
    use std::io::BufRead;
    let f = std::fs::File::open(path).ok()?;
    let mut renamed: Option<String> = None;
    let mut prompt: Option<String> = None;
    for line in std::io::BufReader::new(f).lines().map_while(Result::ok) {
        // cheap pre-filter: only rename/message lines can carry a title
        if !line.contains("session_meta") && !line.contains(r#""role":"user""#) {
            continue;
        }
        let Ok(v) = serde_json::from_str::<serde_json::Value>(&line) else {
            continue;
        };
        if v["type"].as_str() == Some("session_meta") {
            if let Some(t) = v["title"].as_str().map(str::trim).filter(|t| !t.is_empty()) {
                renamed = Some(t.chars().take(80).collect());
            }
            continue;
        }
        if prompt.is_some() || v.pointer("/message/role").and_then(|r| r.as_str()) != Some("user") {
            continue;
        }
        // content is a string in old logs, a block array in new ones —
        // deserialize through Message so both land on the same text view.
        let Some(c) = serde_json::from_value::<sunmao_llm::types::Message>(v["message"].clone())
            .ok()
            .and_then(|m| m.content_text())
        else {
            continue;
        };
        if c.starts_with("[hook context]") || c.starts_with("<local-shell>") {
            continue;
        }
        if let Some(first) = c.lines().map(str::trim).find(|l| !l.is_empty()) {
            prompt = Some(first.chars().take(80).collect());
        }
    }
    renamed.or(prompt)
}

/// One session that matched a `/search`/`GET /sessions?q=` query — the log
/// id, its display title, and up to 3 context snippets around hits in
/// user/assistant message content.
pub struct SearchHit {
    pub id: String,
    pub title: Option<String>,
    pub hits: Vec<String>,
}

/// How far around the match a search snippet reaches (chars each side).
const SNIP_CTX: usize = 60;
/// Result budget — beyond this many sessions a broader query is the
/// intended next step, not a longer list.
const SEARCH_MAX: usize = 20;

/// Extract a ≤160-char window centered on the `ql`-length match found at
/// `pos` (byte offsets into the lowercased content — safe: we only slice at
/// boundaries derived from char_indices).
fn snippet(content: &str, lower: &str, pos: usize, qlen: usize) -> String {
    // char index of the match inside `lower` (lower == content length-wise
    // for anything but exotic case mappings; the window is display-only)
    let ci = lower[..pos].chars().count();
    let qn = lower[pos..pos + qlen].chars().count();
    let chars: Vec<char> = content.chars().collect();
    let start = ci.saturating_sub(SNIP_CTX).min(chars.len());
    let end = (ci + qn + SNIP_CTX).min(chars.len());
    let mid: String = chars[start..end].iter().collect();
    let mut out = String::with_capacity(mid.len() + 8);
    if start > 0 {
        out.push('…');
    }
    out.push_str(
        mid.split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
            .as_str(),
    );
    if end < chars.len() {
        out.push('…');
    }
    out.chars().take(160).collect()
}

/// Grep every `*.jsonl` under `dirs` for `q` inside user/assistant message
/// content (case-insensitive substring). Synthetic folded lines
/// (`[hook context]`, `<local-shell>`) are skipped — they're evidence, not
/// something a human typed or the model authored. Newest logs first,
/// ≤ `SEARCH_MAX` sessions, ≤ 3 snippets each.
pub fn search_sessions(dirs: &[PathBuf], q: &str) -> Vec<SearchHit> {
    let ql = q.to_lowercase();
    if ql.is_empty() {
        return Vec::new();
    }
    let mut logs: Vec<(std::time::SystemTime, PathBuf)> = Vec::new();
    for dir in dirs {
        if let Ok(rd) = std::fs::read_dir(dir) {
            for e in rd.flatten() {
                let p = e.path();
                if p.extension().map(|x| x == "jsonl").unwrap_or(false) {
                    let m = e
                        .metadata()
                        .and_then(|m| m.modified())
                        .unwrap_or(std::time::UNIX_EPOCH);
                    logs.push((m, p));
                }
            }
        }
    }
    logs.sort_by_key(|l| std::cmp::Reverse(l.0));

    use std::io::BufRead;
    let mut out = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for (_, p) in logs {
        if out.len() >= SEARCH_MAX {
            break;
        }
        if !seen.insert(
            p.file_stem()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned(),
        ) {
            continue; // same id under another known project dir
        }
        let Ok(f) = std::fs::File::open(&p) else {
            continue;
        };
        let mut hits: Vec<String> = Vec::new();
        for line in std::io::BufReader::new(f).lines().map_while(Result::ok) {
            if hits.len() >= 3 {
                break;
            }
            if !line.contains("\"role\"") {
                continue;
            }
            let Ok(v) = serde_json::from_str::<serde_json::Value>(&line) else {
                continue;
            };
            if v["type"].as_str() != Some("message") {
                continue;
            }
            let role = v.pointer("/message/role").and_then(|r| r.as_str());
            if !matches!(role, Some("user") | Some("assistant")) {
                continue;
            }
            let Some(c) =
                serde_json::from_value::<sunmao_llm::types::Message>(v["message"].clone())
                    .ok()
                    .and_then(|m| m.content_text())
            else {
                continue;
            };
            if c.starts_with("[hook context]") || c.starts_with("<local-shell>") {
                continue;
            }
            let lower = c.to_lowercase();
            let Some(pos) = lower.find(&ql) else { continue };
            hits.push(snippet(&c, &lower, pos, ql.len()));
        }
        if hits.is_empty() {
            continue;
        }
        out.push(SearchHit {
            id: p
                .file_stem()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned(),
            title: log_title(&p),
            hits,
        });
    }
    out
}

/// `/search <q>` output for the local frontends — one block per session,
/// phrased as the `/resume` that adopts it.
pub fn search_text(dirs: &[PathBuf], q: &str) -> String {
    let hits = search_sessions(dirs, q);
    if hits.is_empty() {
        return format!("[no sessions match '{q}']");
    }
    let mut out = format!("sessions matching '{q}' ({}):", hits.len());
    for h in &hits {
        out.push_str(&format!(
            "\n  /resume {}  — {}",
            h.id,
            h.title.as_deref().unwrap_or("(untitled)")
        ));
        for s in &h.hits {
            out.push_str(&format!("\n      {s}"));
        }
    }
    out
}
