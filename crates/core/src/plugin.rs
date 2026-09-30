//! Plugin bundle install/list/remove — the management half of the
//! `plugins/<name>/` convention. Consumption (commands/skills/agents/hooks/mcp
//! loaders) scans these dirs directly; this module only owns how a bundle
//! lands in `<cwd>/.sunmao/plugins/` and comes back out. Project-local only —
//! no user-global dir by design.

use std::path::{Path, PathBuf};

use anyhow::Context as _;

/// One installed plugin under `.sunmao/plugins/`.
#[derive(Debug)]
pub struct PluginInfo {
    /// Directory name — what `remove` takes.
    pub name: String,
    pub version: Option<String>,
    pub description: Option<String>,
    /// Present components: commands / skills / agents dirs, plus `hooks`
    /// and `mcp` when the manifest or `hooks/hooks.json` contributes them.
    pub components: Vec<String>,
    /// The plugin's root dir.
    pub path: PathBuf,
}

fn plugins_dir(cwd: &Path) -> PathBuf {
    cwd.join(".sunmao").join("plugins")
}

/// The plugin name becomes a directory name — it must not be able to walk
/// out of `plugins/`. Kept strict on purpose: no separators, no `..`.
fn sanitize_name(name: &str) -> anyhow::Result<String> {
    let name = name.trim();
    if name.is_empty() || name == "." || name.contains(['/', '\\']) || name.contains("..") {
        anyhow::bail!("invalid plugin name {name:?} — must not be empty or contain path parts");
    }
    Ok(name.to_string())
}

fn manifest_name(manifest: &Path) -> anyhow::Result<String> {
    let text = std::fs::read_to_string(manifest)
        .with_context(|| format!("read {}", manifest.display()))?;
    let v: serde_json::Value =
        serde_json::from_str(&text).with_context(|| format!("parse {}", manifest.display()))?;
    let obj = v
        .as_object()
        .with_context(|| format!("{} is not a JSON object", manifest.display()))?;
    let Some(name) = obj.get("name").and_then(|n| n.as_str()) else {
        anyhow::bail!("{}: missing \"name\" field", manifest.display());
    };
    sanitize_name(name).with_context(|| format!("{}: bad \"name\"", manifest.display()))
}

/// Copy a source dir (containing `plugin.json`) into
/// `<cwd>/.sunmao/plugins/<name>/`. Refuses to overwrite unless `force`.
/// Returns the plugin name.
pub fn install(source_dir: &Path, cwd: &Path, force: bool) -> anyhow::Result<String> {
    let name = manifest_name(&source_dir.join("plugin.json"))?;
    let target = plugins_dir(cwd).join(&name);
    // installing a dir that already lives under plugins/ would recursively
    // copy it into itself — `plugin install .sunmao/plugins/demo` isn't a
    // reinstall, it's a cycle
    let canon_src = source_dir
        .canonicalize()
        .unwrap_or_else(|_| source_dir.to_path_buf());
    let canon_plugins = cwd
        .canonicalize()
        .unwrap_or_else(|_| cwd.to_path_buf())
        .join(".sunmao")
        .join("plugins");
    if canon_src.starts_with(&canon_plugins) {
        anyhow::bail!("plugin source is already inside .sunmao/plugins/");
    }
    if target.exists() {
        if !force {
            anyhow::bail!("plugin {name} already installed — pass --force to overwrite");
        }
        std::fs::remove_dir_all(&target)
            .with_context(|| format!("remove old {}", target.display()))?;
    }
    copy_dir(source_dir, &target).with_context(|| format!("copy to {}", target.display()))?;
    Ok(name)
}

/// Everything installed under `.sunmao/plugins/` — dirs without a manifest
/// are skipped (a half-removed bundle shouldn't break listing).
pub fn list(cwd: &Path) -> Vec<PluginInfo> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(plugins_dir(cwd)) else {
        return out;
    };
    for entry in entries.flatten() {
        let root = entry.path();
        if !root.is_dir() {
            continue;
        }
        let manifest = root.join("plugin.json");
        let Ok(text) = std::fs::read_to_string(&manifest) else {
            continue;
        };
        let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) else {
            continue;
        };
        let name = entry.file_name().to_string_lossy().to_string();
        let mut components = Vec::new();
        for (dir, label) in [
            ("commands", "commands"),
            ("skills", "skills"),
            ("agents", "agents"),
        ] {
            if root.join(dir).is_dir() {
                components.push(label.to_string());
            }
        }
        // hooks/mcp can arrive via dirs or straight from the manifest keys —
        // either way the bundle contributes them at load time.
        if root.join("hooks").is_dir() || v.get("hooks").is_some() {
            components.push("hooks".into());
        }
        if v.get("mcpServers").is_some() {
            components.push("mcp".into());
        }
        out.push(PluginInfo {
            name,
            version: v.get("version").and_then(|x| x.as_str()).map(String::from),
            description: v
                .get("description")
                .and_then(|x| x.as_str())
                .map(String::from),
            components,
            path: root,
        });
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

/// Delete `<cwd>/.sunmao/plugins/<name>/`. The name is sanitized and the dir
/// must carry a manifest — `remove` never touches anything that isn't an
/// installed plugin.
pub fn remove(name: &str, cwd: &Path) -> anyhow::Result<()> {
    let name = sanitize_name(name)?;
    let target = plugins_dir(cwd).join(&name);
    if !target.join("plugin.json").is_file() {
        anyhow::bail!("plugin {name} not installed");
    }
    std::fs::remove_dir_all(&target).with_context(|| format!("remove {}", target.display()))
}

/// Recursive copy on std only — bundles are small and a dependency for this
/// would be heavier than the function.
fn copy_dir(src: &Path, dst: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let to = dst.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_dir(&entry.path(), &to)?;
        } else {
            std::fs::copy(entry.path(), &to)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sandbox(tag: &str) -> PathBuf {
        let dir = crate::fresh_test_dir(&format!("plugin-{tag}"));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn plugin_src(dir: &Path, name: &str) -> PathBuf {
        let src = dir.join("src-plugin");
        std::fs::create_dir_all(src.join("commands")).unwrap();
        std::fs::write(
            src.join("plugin.json"),
            format!("{{\"name\": \"{name}\", \"version\": \"1.0\", \"description\": \"d\"}}"),
        )
        .unwrap();
        std::fs::write(src.join("commands").join("hi.md"), "hello").unwrap();
        src
    }

    #[test]
    fn install_list_remove_roundtrip() {
        let dir = sandbox("roundtrip");
        let cwd = dir.join("cwd");
        let src = plugin_src(&dir, "demo");

        let name = install(&src, &cwd, false).unwrap();
        assert_eq!(name, "demo");
        assert!(cwd.join(".sunmao/plugins/demo/commands/hi.md").is_file());

        let plugins = list(&cwd);
        assert_eq!(plugins.len(), 1);
        assert_eq!(plugins[0].name, "demo");
        assert_eq!(plugins[0].version.as_deref(), Some("1.0"));
        assert!(plugins[0].components.contains(&"commands".to_string()));
        assert!(!plugins[0].components.contains(&"mcp".to_string()));

        remove("demo", &cwd).unwrap();
        assert!(list(&cwd).is_empty());
        assert!(remove("demo", &cwd).is_err());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn refuses_overwrite_then_force_wins() {
        let dir = sandbox("force");
        let cwd = dir.join("cwd");
        let src = plugin_src(&dir, "demo");

        install(&src, &cwd, false).unwrap();
        assert!(install(&src, &cwd, false).is_err());
        install(&src, &cwd, true).unwrap();
        assert!(cwd.join(".sunmao/plugins/demo/plugin.json").is_file());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn bad_names_and_manifests_rejected() {
        let dir = sandbox("names");
        let cwd = dir.join("cwd");

        // manifest without a name
        let noname = dir.join("noname");
        std::fs::create_dir_all(&noname).unwrap();
        std::fs::write(noname.join("plugin.json"), "{}").unwrap();
        assert!(install(&noname, &cwd, false).is_err());

        // names that could escape plugins/ are refused at both ends
        for bad in ["../evil", "a/b", "a\\b", "..", "", "  "] {
            let src = dir.join("bad-plugin");
            std::fs::create_dir_all(&src).unwrap();
            std::fs::write(
                src.join("plugin.json"),
                format!("{{\"name\": \"{}\"}}", bad.replace('\\', "\\\\")),
            )
            .unwrap();
            assert!(install(&src, &cwd, false).is_err(), "name {bad:?}");
            assert!(remove(bad, &cwd).is_err(), "remove {bad:?}");
        }

        // a manifest that isn't even an object
        let broken = dir.join("broken");
        std::fs::create_dir_all(&broken).unwrap();
        std::fs::write(broken.join("plugin.json"), "[1,2]").unwrap();
        assert!(install(&broken, &cwd, false).is_err());

        // no manifest at all
        let empty = dir.join("empty");
        std::fs::create_dir_all(&empty).unwrap();
        assert!(install(&empty, &cwd, false).is_err());

        // installing an already-installed bundle would recurse into itself
        let self_src = plugin_src(&dir, "selfy");
        install(&self_src, &cwd, false).unwrap();
        assert!(install(&cwd.join(".sunmao/plugins/selfy"), &cwd, false).is_err());

        let _ = std::fs::remove_dir_all(&dir);
    }
}
