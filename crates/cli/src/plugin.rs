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
    /// Install a plugin dir (containing plugin.json) into .sunmao/plugins/.
    Install {
        /// Path to the plugin source directory.
        path: PathBuf,
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

/// `cwd` is the raw `--cwd` flag — canonicalized the same way `main` does
/// for the session path.
pub fn run(args: &PluginArgs, cwd: &std::path::Path) -> anyhow::Result<()> {
    let cwd = cwd.canonicalize().context("bad --cwd")?;
    match &args.op {
        PluginOp::Install { path, force } => {
            let name = sunmao_core::plugin::install(path, &cwd, *force)?;
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
