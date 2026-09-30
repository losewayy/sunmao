//! `sunmao <subcommand>` — the `Cmd` enum is the dispatch point. `plugin`
//! ops live here: install/list/remove for plugin bundles under
//! `<cwd>/.sunmao/plugins/`. Pure file ops: no provider, no session.

use std::path::PathBuf;

use anyhow::Context as _;
use clap::Subcommand;

#[derive(Subcommand)]
pub enum Cmd {
    /// Manage plugin bundles under .sunmao/plugins/.
    Plugin(PluginArgs),
    /// Run eval cases against the real loop and report pass/fail.
    Eval(crate::eval::EvalArgs),
}

#[derive(clap::Args)]
pub struct PluginArgs {
    #[command(subcommand)]
    pub op: PluginOp,
}

#[derive(Subcommand)]
pub enum PluginOp {
    /// Install a plugin source into .sunmao/plugins/ — a local dir
    /// (containing plugin.json), a git URL, or `owner/repo` shorthand.
    Install {
        /// Plugin source: directory path, git URL, or owner/repo.
        source: String,
        /// Overwrite an already-installed plugin of the same name.
        #[arg(long)]
        force: bool,
    },
    /// List installed plugins.
    List,
    /// Remove an installed plugin by name.
    Remove {
        /// Plugin name (its directory under .sunmao/plugins/).
        name: String,
    },
}

/// Resolve an install source to a local dir. Git-looking sources (URL or
/// `owner/repo`) clone to a temp dir via the `git` binary — no libgit dep.
/// Returns (dir, Some(temp guard)) for clones so callers can't forget the
/// cleanup; plain paths return the path itself.
fn resolve_source(source: &str) -> anyhow::Result<(PathBuf, Option<TempDir>)> {
    let url = git_url(source);
    if url.is_none() {
        return Ok((PathBuf::from(source), None));
    }
    let url = url.unwrap();
    let tmp = TempDir::new()?;
    let status = std::process::Command::new("git")
        .args(["clone", "--depth", "1", &url])
        .arg(tmp.path())
        .status()
        .context("failed to spawn git — is it on PATH?")?;
    anyhow::ensure!(status.success(), "git clone {url} failed");
    Ok((tmp.path().to_path_buf(), Some(tmp)))
}

/// `owner/repo` or an explicit URL → cloneable URL. Anything that exists as
/// a local path wins — a dir named `a/b` is a dir, not a repo shorthand.
fn git_url(source: &str) -> Option<String> {
    if PathBuf::from(source).exists() {
        return None;
    }
    if source.starts_with("https://") || source.starts_with("git@") || source.ends_with(".git") {
        return Some(source.to_string());
    }
    let parts: Vec<&str> = source.split('/').collect();
    if parts.len() == 2
        && parts
            .iter()
            .all(|p| !p.is_empty() && !p.contains(['\\', ':']))
    {
        return Some(format!("https://github.com/{source}.git"));
    }
    None
}

/// RAII temp dir for `git clone` targets — deleted on drop.
struct TempDir(PathBuf);

/// pid alone recycles; nanos keeps each clone scratch dir unique even when
/// a stale dir survives from a killed run.
fn fresh_dir(tag: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    std::env::temp_dir().join(format!("sunmao-{tag}-{}-{nanos}", std::process::id()))
}

impl TempDir {
    fn new() -> anyhow::Result<Self> {
        let dir = fresh_dir("plugin");
        std::fs::create_dir_all(&dir)?;
        Ok(Self(dir))
    }
    fn path(&self) -> &std::path::Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// `cwd` is the raw `--cwd` flag — canonicalized the same way `main` does
/// for the session path.
pub fn run(args: &PluginArgs, cwd: &std::path::Path) -> anyhow::Result<()> {
    let cwd = cwd.canonicalize().context("bad --cwd")?;
    match &args.op {
        PluginOp::Install { source, force } => {
            let (dir, _guard) = resolve_source(source)?;
            let name = sunmao_core::plugin::install(&dir, &cwd, *force)?;
            println!("installed: {name} → .sunmao/plugins/{name}");
        }
        PluginOp::List => {
            let plugins = sunmao_core::plugin::list(&cwd);
            if plugins.is_empty() {
                println!("[no plugins installed]");
            }
            for p in plugins {
                println!(
                    "{}\t{}\t[{}]\t{}",
                    p.name,
                    p.version.as_deref().unwrap_or("-"),
                    p.components.join(" "),
                    // canonicalize() leaves the \\?\ verbatim prefix on
                    // Windows — strip it so the path is copyable
                    p.path.display().to_string().replace("\\\\?\\", "")
                );
            }
        }
        PluginOp::Remove { name } => {
            sunmao_core::plugin::remove(name, &cwd)?;
            println!("removed: {name}");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_classification() {
        // explicit URLs clone
        assert_eq!(
            git_url("https://github.com/a/b.git").as_deref(),
            Some("https://github.com/a/b.git")
        );
        assert_eq!(
            git_url("git@github.com:a/b.git").as_deref(),
            Some("git@github.com:a/b.git")
        );
        // owner/repo shorthand → github
        assert_eq!(
            git_url("acme/audit-pack").as_deref(),
            Some("https://github.com/acme/audit-pack.git")
        );
        // an existing local path always wins over repo shorthand
        let tmp = fresh_dir("plug-src");
        std::fs::create_dir_all(&tmp).unwrap();
        assert!(git_url(&tmp.to_string_lossy()).is_none());
        // bare name without slash is a path, not a repo
        assert!(git_url("not-a-repo").is_none());
        // three-segment paths aren't shorthand
        assert!(git_url("a/b/c").is_none());
        let _ = std::fs::remove_dir_all(&tmp);
    }
}
