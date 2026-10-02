use crate::tool::*;
use anyhow::bail;
use serde::Deserialize;
use serde_json::{Value, json};

// ---------- WebFetch ----------

pub struct WebFetchTool;

#[async_trait::async_trait]
impl ToolImpl for WebFetchTool {
    fn name(&self) -> &'static str {
        "WebFetch"
    }

    fn decl(&self) -> Tool {
        Tool::function(
            "WebFetch",
            "Fetch a URL and return readable text (tags stripped, ~24KB cap). \
             For docs, error lookups, changelog checks.",
            json!({
                "type": "object",
                "properties": {
                    "url": {"type": "string", "description": "http(s) URL"}
                },
                "required": ["url"]
            }),
        )
    }

    async fn call(
        &self,
        args: Value,
        _ctx: &std::sync::Arc<crate::context::Context>,
    ) -> anyhow::Result<ToolResult> {
        #[derive(Deserialize)]
        struct Args {
            url: String,
        }
        let a: Args = serde_json::from_value(args)?;
        if !a.url.starts_with("http://") && !a.url.starts_with("https://") {
            bail!("url must be http(s)");
        }
        let resp = crate::web::fetch_text(&a.url).await?;
        const CAP: usize = 24 * 1024;
        let truncated = resp.chars().take(CAP).collect::<String>();
        Ok(ToolResult {
            output: truncated,
            ok: true,
        })
    }
}
