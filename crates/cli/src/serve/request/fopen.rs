//! `POST /session/{id}/open` — the click-to-open allowlist: a path opens
//! only if THIS session's log surfaced it (tool args, Glob/Grep result
//! rows, artifacts, child-agent transcripts, the session root). The sink
//! is the OS association handler, so surfaced-but-executable extensions
//! are refused — a transcript click must never become code execution.

/// Resolve a click's raw path against the session's surfaced set.
/// `Err` = forbidden (not in the log / not a path); `Ok(None)` = surfaced
/// but gone from disk; `Ok(Some)` = safe to hand to the OS handler.
pub(super) fn validate_open_click(
    text: &str,
    fallback_cwd: &std::path::Path,
    raw: &str,
    sessions_dir: Option<&std::path::Path>,
) -> Result<Option<std::path::PathBuf>, String> {
    let (cwd, exposed) = exposed_paths_with_children(text, fallback_cwd, sessions_dir);
    let Some(want) = normalize_click(&cwd, raw) else {
        return Err("not a filesystem path".into());
    };
    if !exposed.contains(&path_key(&want)) {
        return Err("path wasn't surfaced by this session".into());
    }
    // the OS association handler is the open sink — on extensions that
    // EXECUTE rather than display, a transcript click must not become code
    // execution, even though the path was legitimately surfaced
    if let Some(ext) = want.extension().and_then(|e| e.to_str()) {
        const NO_OPEN: &[&str] = &[
            "exe", "com", "bat", "cmd", "scr", "pif", "msi", "msp", "reg", "lnk", "url", "hta",
            "rdp", "vbs", "vbe", "js", "jse", "wsf", "wsh", "ps1", "psm1", "jar", "dll", "desktop",
            "command", "app", "sh",
        ];
        if NO_OPEN.contains(&ext.to_ascii_lowercase().as_str()) {
            return Err(format!(".{ext} opens by executing, not viewing — refused"));
        }
    }
    Ok(if want.exists() { Some(want) } else { None })
}

/// The session's surfaced set: normalized keys for every path a tool call
/// or its output put in front of the user, plus the session root. Follows
/// `sub-…` transcript ids into sibling logs under `sessions_dir` — the
/// parent transcript renders a child's tool cards, so paths the child
/// surfaced must open too.
fn exposed_paths_with_children(
    text: &str,
    fallback_cwd: &std::path::Path,
    sessions_dir: Option<&std::path::Path>,
) -> (std::path::PathBuf, std::collections::HashSet<String>) {
    let mut cwd = fallback_cwd.to_path_buf();
    for line in text.lines() {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        if v["type"].as_str() == Some("started") {
            if let Some(c) = v["cwd"].as_str() {
                cwd = std::path::PathBuf::from(c);
            }
            break;
        }
    }
    let mut set = std::collections::HashSet::new();
    collect_surfaced(text, &cwd, &mut set, 0, sessions_dir);
    (cwd, set)
}

/// Scan one log's events into `set`. `depth` guards the sub-agent log
/// follow — `task_done`/Task-result `sub-…` ids are sibling files in the
/// same sessions dir, so a path a child surfaced still opens from the
/// parent's transcript (the child card renders in the viewed session).
fn collect_surfaced(
    text: &str,
    cwd: &std::path::Path,
    set: &mut std::collections::HashSet<String>,
    depth: usize,
    sessions_dir: Option<&std::path::Path>,
) {
    if depth == 0 {
        push_surfaced(set, cwd, &cwd.to_string_lossy());
    }
    for line in text.lines() {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        match v["type"].as_str().unwrap_or("") {
            "tool_call" => {
                let name = v["call"]["function"]["name"].as_str().unwrap_or("");
                if !matches!(name, "Read" | "Write" | "Edit" | "Glob" | "Grep") {
                    continue;
                }
                let args = v["call"]["function"]["arguments"].as_str().unwrap_or("{}");
                let Ok(a) = serde_json::from_str::<serde_json::Value>(args) else {
                    continue;
                };
                if let Some(p) = a["path"].as_str() {
                    push_surfaced(set, cwd, p);
                }
            }
            // PTC `tools.*` calls surface the same file paths under a
            // different event shape — same contract, different field names
            "ptc_call" => {
                let name = v["name"].as_str().unwrap_or("");
                if !matches!(name, "Read" | "Write" | "Edit" | "Glob" | "Grep") {
                    continue;
                }
                if let Some(p) =
                    serde_json::from_str::<serde_json::Value>(v["args"].as_str().unwrap_or("{}"))
                        .ok()
                        .and_then(|a| a["path"].as_str().map(str::to_string))
                {
                    push_surfaced(set, cwd, &p);
                }
                if matches!(name, "Glob" | "Grep") {
                    push_result_lines(set, cwd, name, v["output"].as_str().unwrap_or(""));
                }
            }
            "tool_result" => {
                let name = v["name"].as_str().unwrap_or("");
                // a Task's done output carries the child's transcript id —
                // follow it into the sibling sub-*.jsonl so paths the child
                // surfaced stay clickable in this transcript
                if let (true, Some(dir)) = (depth < 3, sessions_dir) {
                    follow_child_logs(v["output"].as_str().unwrap_or(""), set, depth, dir);
                }
                if name == "Glob" || name == "Grep" {
                    push_result_lines(set, cwd, name, v["output"].as_str().unwrap_or(""));
                }
            }
            "task_done" => {
                if let (Some(id), true, Some(dir)) = (v["id"].as_str(), depth < 3, sessions_dir) {
                    follow_child_logs(id, set, depth, dir);
                }
            }
            "artifact" => {
                if let Some(p) = v["path"].as_str() {
                    push_surfaced(set, cwd, p);
                }
            }
            _ => {}
        }
    }
}

fn push_surfaced(set: &mut std::collections::HashSet<String>, cwd: &std::path::Path, raw: &str) {
    if let Some(p) = normalize_click(cwd, raw) {
        set.insert(path_key(&p));
    }
}

/// Glob/Grep output rows → surfaced paths. `path:line:match` splits at the
/// FIRST `:digits:` so a drive-letter colon inside the path can't eat it.
fn push_result_lines(
    set: &mut std::collections::HashSet<String>,
    cwd: &std::path::Path,
    name: &str,
    output: &str,
) {
    for line in output.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('[') || line.starts_with('…') {
            continue;
        }
        if name == "Grep" {
            for (i, _) in line.match_indices(':') {
                let rest = &line[i + 1..];
                let Some(end) = rest.find(':') else { break };
                if end > 0 && rest[..end].chars().all(|c| c.is_ascii_digit()) {
                    push_surfaced(set, cwd, &line[..i]);
                    break;
                }
            }
        } else {
            push_surfaced(set, cwd, line);
        }
    }
}

/// A Task result mentions the child's transcript (`sub-NNN-lN`) — read that
/// sibling log and fold ITS surfaced paths into the viewed session's set.
fn follow_child_logs(
    hay: &str,
    set: &mut std::collections::HashSet<String>,
    depth: usize,
    dir: &std::path::Path,
) {
    let mut i = 0;
    while let Some(at) = hay[i..].find("sub-") {
        let tail = &hay[i + at + 4..];
        let id: String = tail
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric() || *c == '-')
            .collect();
        i += at + 4;
        // strict shape — an output line can string "sub-…-like" text into a
        // bogus name; only real transcript ids may resolve to a file
        let valid = {
            let mut parts = id.split('-');
            matches!(parts.next(), Some(d) if d.chars().all(|c| c.is_ascii_digit()) && !d.is_empty())
                && matches!(parts.next(), Some(l) if l.strip_prefix('l').is_some_and(|n| n.chars().all(|c| c.is_ascii_digit()) && !n.is_empty()))
                && parts.next().is_none()
        };
        if !valid {
            continue;
        }
        let log = dir.join(format!("sub-{id}.jsonl"));
        if let Ok(text) = std::fs::read_to_string(&log) {
            let mut cwd = std::path::PathBuf::new();
            for l in text.lines() {
                if let Ok(v) = serde_json::from_str::<serde_json::Value>(l)
                    && v["type"].as_str() == Some("started")
                {
                    if let Some(c) = v["cwd"].as_str() {
                        cwd = std::path::PathBuf::from(c);
                    }
                    break;
                }
            }
            collect_surfaced(&text, &cwd, set, depth + 1, Some(dir));
        }
    }
}

/// Lexical normalize a clicked path: `file://` stripped, other schemes
/// refused, relative joins the session cwd, `.`/`..` folded without
/// touching the filesystem (the file may already be gone).
fn normalize_click(cwd: &std::path::Path, raw: &str) -> Option<std::path::PathBuf> {
    let raw = raw.trim();
    let raw = raw.strip_prefix("file://").unwrap_or(raw);
    if raw.is_empty() || raw.contains("://") {
        return None;
    }
    let p = std::path::PathBuf::from(raw);
    let joined = if p.is_absolute() { p } else { cwd.join(&p) };
    let mut out = std::path::PathBuf::new();
    for c in joined.components() {
        match c {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    Some(out)
}

/// Set-key for a path: slash-normalized, and case-folded on Windows where
/// the filesystem can't tell `Src/Foo.rs` from `src/foo.rs`.
fn path_key(p: &std::path::Path) -> String {
    let s = p.to_string_lossy().replace('/', "\\");
    #[cfg(windows)]
    return s.to_lowercase();
    #[cfg(not(windows))]
    s
}

#[cfg(test)]
mod tests {
    use super::validate_open_click;

    fn open_fixture() -> String {
        concat!(
            r#"{"type":"started","model":"m","cwd":"/proj","driver":"full"}"#,
            "\n",
            r#"{"type":"tool_call","call":{"function":{"name":"Read","arguments":"{\"path\":\"src/foo.rs\"}"}}}"#,
            "\n",
            r#"{"type":"tool_call","call":{"function":{"name":"Bash","arguments":"{\"command\":\"cat /etc/passwd\"}"}}}"#,
            "\n",
            r#"{"type":"tool_result","name":"Glob","ok":true,"output":"src/a.rs\nsrc/dir/b.rs\n[truncated at 200]"}"#,
            "\n",
            r#"{"type":"tool_result","name":"Grep","ok":true,"output":"src/c.rs:12:match text"}"#,
            "\n",
            r#"{"type":"artifact","name":"x","path":"/proj/.sunmao/artifacts/x.html","bytes":1}"#,
            "\n",
        )
        .to_string()
    }

    #[test]
    fn open_click_accepts_surfaced_paths_only() {
        let text = open_fixture();
        let cwd = std::path::Path::new("/fallback");
        // tool-arg, glob line, grep path, artifact, and the session root
        for ok in [
            "src/foo.rs",
            "src/a.rs",
            "src/dir/b.rs",
            "src/c.rs",
            "/proj/.sunmao/artifacts/x.html",
            "/proj",
            "./src/foo.rs",
            "src/../src/foo.rs",
        ] {
            assert!(validate_open_click(&text, cwd, ok, None).is_ok(), "{ok}");
        }
        // forged: never surfaced, bash-command string, scheme, traversal
        for bad in [
            "src/secret.rs",
            "/etc/passwd",
            "https://evil/x",
            "../outside.rs",
        ] {
            assert!(validate_open_click(&text, cwd, bad, None).is_err(), "{bad}");
        }
        // surfaced but executable-by-association — the open sink refuses
        let exec_text = format!(
            "{}{}",
            r#"{"type":"started","model":"m","cwd":"/proj","driver":"full"}"#.to_string() + "\n",
            r#"{"type":"tool_call","call":{"function":{"name":"Read","arguments":"{\"path\":\"build.ps1\"}"}}}"#,
        );
        assert!(
            validate_open_click(&exec_text, cwd, "build.ps1", None)
                .unwrap_err()
                .contains("executing")
        );
    }

    #[test]
    fn open_click_reports_gone_files_distinctly() {
        let text = open_fixture();
        // surfaced + missing on disk → Ok(None), the 404 branch
        assert!(matches!(
            validate_open_click(&text, std::path::Path::new("/x"), "src/foo.rs", None),
            Ok(None)
        ));
    }

    #[test]
    fn open_click_resolves_a_real_surfaced_file() {
        let dir = std::env::temp_dir().join("sm-open-fixture");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("src")).unwrap();
        let real = dir.join("src").join("real.txt");
        std::fs::write(&real, "x").unwrap();
        let cwd_json = serde_json::to_string(&dir.to_string_lossy().to_string()).unwrap();
        let started =
            format!(r#"{{"type":"started","model":"m","cwd":{cwd_json},"driver":"full"}}"#);
        let text = format!(
            "{started}\n{}\n",
            r#"{"type":"tool_call","call":{"function":{"name":"Read","arguments":"{\"path\":\"src/real.txt\"}"}}}"#
        );
        match validate_open_click(&text, std::path::Path::new("/fb"), "src/real.txt", None) {
            Ok(Some(p)) => assert!(p.ends_with("real.txt")),
            other => panic!("expected Ok(Some), got {other:?}"),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
