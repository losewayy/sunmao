//! Preset resolution — `--preset <name>` names a directory that looks exactly
//! like an installed plugin bundle (`plugin.json`, `hooks/`, `commands/`,
//! `skills/`, `agents/`, `mcp.json`). Where plugins under `plugins/` are
//! always on, a preset contributes only while named; `--preset a --preset b`
//! layers b after a (later wins where merging applies).
//!
//! Lookup order per name: `<cwd>/.sunmao/presets/<name>/` first, then
//! `~/.sunmao/presets/<name>/` — project beats user-global, same rule every
//! other config layer follows. A leading `+` in the name is the SPEC's
//! layering notation (`+strict` == `strict`), not part of the dirname.

use std::path::{Path, PathBuf};

/// The dirs a preset name is looked up in, in precedence order. Returned so
/// the unknown-name error can list what was actually searched.
fn search_dirs(cwd: &Path) -> Vec<PathBuf> {
    let mut dirs = vec![cwd.join(".sunmao").join("presets")];
    if let Some(home) = std::env::var_os("USERPROFILE").or_else(|| std::env::var_os("HOME")) {
        dirs.push(Path::new(&home).join(".sunmao").join("presets"));
    }
    dirs
}

/// Normalize a `--preset` argument to a directory name. `+` is the spec's
/// layering prefix; anything else that could walk out of `presets/` is
/// refused (same reasoning as plugin.rs's name sanitizer).
fn preset_name(arg: &str) -> anyhow::Result<String> {
    let name = arg.trim().strip_prefix('+').unwrap_or(arg.trim());
    if name.is_empty() || name == "." || name.contains(['/', '\\']) || name.contains("..") {
        anyhow::bail!("invalid preset name {arg:?}");
    }
    Ok(name.to_string())
}

/// Resolve `--preset` names to plugin-root dirs, keeping CLI order. Unknown
/// names are a hard error that lists the searched dirs — a silently-missing
/// preset would run a looser session than the user asked for.
pub fn resolve(cwd: &Path, args: &[String]) -> anyhow::Result<Vec<PathBuf>> {
    let search = search_dirs(cwd);
    let mut roots = Vec::new();
    for arg in args {
        let name = preset_name(arg)?;
        match search.iter().map(|d| d.join(&name)).find(|p| p.is_dir()) {
            Some(root) => roots.push(root),
            None => anyhow::bail!(
                "unknown preset `{name}` — searched: {}",
                search
                    .iter()
                    .map(|d| d.display().to_string().replace("\\\\?\\", ""))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        }
    }
    Ok(roots)
}

/// `--doctor` prints `presets` — every `<name>/` dir present under the
/// search roots, as names only (no resolution needed).
pub fn list_names(cwd: &Path) -> Vec<String> {
    let mut names = Vec::new();
    for dir in search_dirs(cwd) {
        if let Ok(entries) = std::fs::read_dir(&dir) {
            for e in entries.flatten() {
                if e.path().is_dir() {
                    names.push(e.file_name().to_string_lossy().to_string());
                }
            }
        }
    }
    names.sort();
    names.dedup();
    names
}

/// Look up one preset by name — used by frontends (ACP) that resolve at
/// session time because their cwd arrives in a request, not on the CLI.
pub fn resolve_one(cwd: &Path, arg: &str) -> anyhow::Result<PathBuf> {
    resolve(cwd, &[arg.to_string()]).map(|mut v| v.remove(0))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sandbox(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "sunmao-preset-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn resolves_project_preset_and_strips_plus() {
        let dir = sandbox("resolve");
        let preset = dir.join(".sunmao/presets/strict");
        std::fs::create_dir_all(&preset).unwrap();

        let roots = resolve(&dir, &["strict".to_string()]).unwrap();
        assert_eq!(roots, vec![preset.clone()]);
        // `+name` is the SPEC's layering notation — same dir
        let roots = resolve(&dir, &["+strict".to_string()]).unwrap();
        assert_eq!(roots, vec![preset]);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn cli_order_is_preserved() {
        let dir = sandbox("order");
        for name in ["a", "b"] {
            std::fs::create_dir_all(dir.join(format!(".sunmao/presets/{name}"))).unwrap();
        }
        let roots = resolve(&dir, &["a".into(), "b".into()]).unwrap();
        assert!(
            roots[0].ends_with("a") && roots[1].ends_with("b"),
            "layering order must follow the CLI arg order"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn unknown_preset_lists_searched_dirs() {
        let dir = sandbox("unknown");
        let err = resolve(&dir, &["nope".to_string()]).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("unknown preset `nope`"), "msg: {msg}");
        assert!(
            msg.contains(".sunmao"),
            "error must name the preset dirs searched: {msg}"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn bad_names_rejected() {
        let dir = sandbox("badnames");
        for bad in ["../evil", "a/b", "a\\b", "..", "", "  ", "+", "+../x"] {
            assert!(resolve(&dir, &[bad.to_string()]).is_err(), "name {bad:?}");
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn list_names_finds_project_presets() {
        let dir = sandbox("list");
        std::fs::create_dir_all(dir.join(".sunmao/presets/one")).unwrap();
        std::fs::create_dir_all(dir.join(".sunmao/presets/two")).unwrap();
        // a stray file is not a preset
        std::fs::write(dir.join(".sunmao/presets/notadir"), "x").unwrap();
        let names = list_names(&dir);
        assert!(names.contains(&"one".to_string()) && names.contains(&"two".to_string()));
        assert!(!names.contains(&"notadir".to_string()));
        std::fs::remove_dir_all(&dir).ok();
    }
}
