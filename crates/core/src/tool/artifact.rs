use crate::tool::*;
use anyhow::bail;
use serde::Deserialize;
use serde_json::{Value, json};

// ---------- HtmlArtifact (SPEC §4.10) ----------

pub struct HtmlArtifactTool;

#[async_trait::async_trait]
impl ToolImpl for HtmlArtifactTool {
    fn name(&self) -> &'static str {
        "HtmlArtifact"
    }

    fn decl(&self) -> Tool {
        Tool::function(
            "HtmlArtifact",
            "Produce an HTML artifact — plans, reports, dashboards, anything meant for a              human to *look at*. Written to .sunmao/artifacts/{name}.html and registered              in the session log. Use this instead of Markdown for rich deliverables.",
            json!({
                "type": "object",
                "properties": {
                    "name": {"type": "string", "description": "artifact slug, e.g. migration-plan"},
                    "html": {"type": "string", "description": "complete HTML document"}
                },
                "required": ["name", "html"]
            }),
        )
    }

    async fn call(
        &self,
        args: Value,
        ctx: &Arc<crate::context::Context>,
    ) -> anyhow::Result<ToolResult> {
        #[derive(Deserialize)]
        struct Args {
            name: String,
            html: String,
        }
        let a: Args = serde_json::from_value(args)?;
        if !a
            .name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        {
            bail!("artifact name must be [a-z0-9_-]");
        }
        let dir = ctx.cwd.join(".sunmao").join("artifacts");
        tokio::fs::create_dir_all(&dir).await?;
        let path = dir.join(format!("{}.html", a.name));
        // version chain (GUI.md §3): overwriting an artifact archives the
        // previous file as `{name}.v{rev-1}.html` — the Artifact event
        // sequence in the log stays the authoritative version list.
        let rev = archive_prev(&dir, &a.name).await?;
        let bytes = a.html.len();
        tokio::fs::write(&path, &a.html).await?;
        ctx.sessions
            .lock()
            .await
            .append(&crate::session::SessionEvent::Artifact {
                name: a.name.clone(),
                path: path.display().to_string(),
                bytes,
                rev,
            })
            .await?;
        // live event for frontends that can render/link artifacts — the
        // session event above is the durable fact; this is the UI seam
        if let Some(sink) = ctx.live_sink.get() {
            sink.on_event(&crate::agent::LiveEvent::Artifact {
                name: a.name.clone(),
                path: path.display().to_string(),
                bytes,
                rev,
            });
        }
        let rev_note = if rev > 1 {
            format!(" (rev {rev})")
        } else {
            String::new()
        };
        Ok(ToolResult {
            output: format!(
                "artifact '{}' → {} ({} bytes){rev_note}",
                a.name,
                path.display(),
                bytes
            ),
            ok: true,
        })
    }
}

/// Move the current `{name}.html` aside as `{name}.v{K}.html` and return the
/// NEW revision number (K+1; 1 when nothing existed). Indexing keeps the
/// chain dense: latest is always `{name}.html`, history is `v1..v{rev-1}`.
pub(crate) async fn archive_prev(dir: &std::path::Path, name: &str) -> anyhow::Result<usize> {
    let latest = dir.join(format!("{name}.html"));
    if !latest.exists() {
        return Ok(1);
    }
    let mut max_arch = 0usize;
    if let Ok(mut rd) = tokio::fs::read_dir(dir).await {
        while let Ok(Some(e)) = rd.next_entry().await {
            let fname = e.file_name();
            let Some(stem) = fname
                .to_str()
                .and_then(|f| f.strip_prefix(&format!("{name}.v")))
                .and_then(|r| r.strip_suffix(".html"))
            else {
                continue;
            };
            if let Ok(k) = stem.parse::<usize>() {
                max_arch = max_arch.max(k);
            }
        }
    }
    let idx = max_arch + 1;
    tokio::fs::rename(&latest, dir.join(format!("{name}.v{idx}.html"))).await?;
    Ok(idx + 1)
}

/// Newest revision number of `name` (1 = only the latest file exists,
/// 0 = no artifact at all) — used by `/artifacts/{name}/revs` and the
/// `?rev=` reader to map a version onto `{name}.v{rev-1}.html`.
pub fn artifact_rev(dir: &std::path::Path, name: &str) -> usize {
    let latest = dir.join(format!("{name}.html"));
    if !latest.exists() {
        return 0;
    }
    let mut max_arch = 0usize;
    if let Ok(rd) = std::fs::read_dir(dir) {
        for e in rd.flatten() {
            let fname = e.file_name();
            let Some(stem) = fname
                .to_str()
                .and_then(|f| f.strip_prefix(&format!("{name}.v")))
                .and_then(|r| r.strip_suffix(".html"))
            else {
                continue;
            };
            if let Ok(k) = stem.parse::<usize>() {
                max_arch = max_arch.max(k);
            }
        }
    }
    max_arch + 1
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The version chain: rewrite k archives the old file as `.v{k-1}`,
    /// latest stays at `{name}.html`, `artifact_rev` reports the count.
    /// The Archive ordering matters — a lost rename would silently drop
    /// the version the log promised.
    #[tokio::test]
    async fn rewrites_archive_dense_versions() {
        let dir = crate::fresh_test_dir("art-rev");
        let art = dir.join("artifacts");
        tokio::fs::create_dir_all(&art).await.unwrap();
        // first write → rev 1, nothing archived
        tokio::fs::write(art.join("plan.html"), b"v1")
            .await
            .unwrap();
        assert_eq!(artifact_rev(&art, "plan"), 1);
        assert_eq!(archive_prev(&art, "plan").await.unwrap(), 2);
        assert_eq!(
            std::fs::read_to_string(art.join("plan.v1.html")).unwrap(),
            "v1"
        );
        tokio::fs::write(art.join("plan.html"), b"v2")
            .await
            .unwrap();
        assert_eq!(artifact_rev(&art, "plan"), 2);
        assert_eq!(archive_prev(&art, "plan").await.unwrap(), 3);
        assert_eq!(
            std::fs::read_to_string(art.join("plan.v2.html")).unwrap(),
            "v2"
        );
        tokio::fs::write(art.join("plan.html"), b"v3")
            .await
            .unwrap();
        assert_eq!(artifact_rev(&art, "plan"), 3);
        assert_eq!(artifact_rev(&art, "missing"), 0);
        std::fs::remove_dir_all(&dir).ok();
    }
}
