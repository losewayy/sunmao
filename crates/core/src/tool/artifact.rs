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

    async fn call(&self, args: Value, ctx: &crate::context::Context) -> anyhow::Result<ToolResult> {
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
        let bytes = a.html.len();
        tokio::fs::write(&path, &a.html).await?;
        ctx.sessions
            .lock()
            .await
            .append(&crate::session::SessionEvent::Artifact {
                name: a.name.clone(),
                path: path.display().to_string(),
                bytes,
            })
            .await?;
        // live event for frontends that can render/link artifacts — the
        // session event above is the durable fact; this is the UI seam
        if let Some(sink) = ctx.live_sink.get() {
            sink.on_event(&crate::agent::LiveEvent::Artifact {
                name: a.name.clone(),
                path: path.display().to_string(),
                bytes,
            });
        }
        Ok(ToolResult {
            output: format!(
                "artifact '{}' → {} ({} bytes)",
                a.name,
                path.display(),
                bytes
            ),
            ok: true,
        })
    }
}
