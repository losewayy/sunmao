//! ServerSpec — one `mcpServers` entry, plus the HTTP transport builder
//! that honors its `headers`/`auth_env`/`token_file`/`timeout` knobs.
//! Credentials resolve here and only ever reach the transport config —
//! nothing logs or echoes token values.

use std::collections::HashMap;

use anyhow::Context as _;
use serde::Deserialize;

/// One `mcp.json` server entry — Claude's shape plus the auth surface the
/// ecosystem uses for remote servers (a literal headers map, an env var
/// holding the bearer token, or a file containing it).
#[derive(Debug, Default, Deserialize)]
pub(crate) struct ServerSpec {
    /// stdio transport: spawn this command.
    #[serde(default)]
    pub(crate) command: Option<String>,
    /// streamable-HTTP transport: connect to this URL instead.
    #[serde(default)]
    pub(crate) url: Option<String>,
    #[serde(default)]
    pub(crate) args: Vec<String>,
    #[serde(default)]
    pub(crate) env: HashMap<String, String>,
    /// Extra HTTP headers for the `url` transport — `{name: value}`;
    /// `${VAR}`/`$VAR` inside a value expands from the process env at
    /// connect time (unset vars collapse to empty, so the header goes out
    /// malformed rather than secretly absent — pair sensitive values with
    /// `auth_env`/`token_file` instead).
    #[serde(default)]
    pub(crate) headers: HashMap<String, String>,
    /// Env var name holding a bearer token for the `url` transport —
    /// `{"auth_env": "FOO_TOKEN"}` sends `Authorization: Bearer $FOO_TOKEN`.
    /// A missing/empty var fails the server (warn-and-skip), the request
    /// never goes out unauthenticated.
    #[serde(default)]
    pub(crate) auth_env: Option<String>,
    /// Path to a file containing the bearer token — read once at connect,
    /// whitespace-trimmed. `${CLAUDE_PLUGIN_ROOT}` expands like command/env.
    #[serde(default)]
    pub(crate) token_file: Option<String>,
    /// Per-request timeout override, seconds (default: reqwest's own).
    #[serde(default)]
    pub(crate) timeout_secs: Option<u64>,
}

/// `${VAR}`/`$VAR` expansion in header values — the mcp.json convention
/// for env-sourced request headers. Unset vars expand to empty.
fn expand_env(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    let mut rest = value;
    while let Some(i) = rest.find('$') {
        out.push_str(&rest[..i]);
        rest = &rest[i + 1..];
        let (name, braced) = if let Some(r) = rest.strip_prefix('{') {
            let end = r.find('}').unwrap_or(r.len());
            (&r[..end], end + 1)
        } else {
            let end = rest
                .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
                .unwrap_or(rest.len());
            (&rest[..end], end)
        };
        if name.is_empty() {
            out.push('$');
            rest = &rest[if braced > 0 {
                braced.min(rest.len())
            } else {
                0
            }..];
            continue;
        }
        out.push_str(&std::env::var(name).unwrap_or_default());
        rest = &rest[braced..];
    }
    out.push_str(rest);
    out
}

impl ServerSpec {
    /// Expand `${CLAUDE_PLUGIN_ROOT}` in command/args/env/headers/token_file
    /// — plugin manifests use it to address files inside their own bundle.
    pub(crate) fn expand_plugin_root(&mut self, root: &str) {
        if let Some(c) = &mut self.command {
            *c = super::expand_plugin_root(c, root);
        }
        for a in &mut self.args {
            *a = super::expand_plugin_root(a, root);
        }
        for v in self.env.values_mut() {
            *v = super::expand_plugin_root(v, root);
        }
        for v in self.headers.values_mut() {
            *v = super::expand_plugin_root(v, root);
        }
        if let Some(t) = &mut self.token_file {
            *t = super::expand_plugin_root(t, root);
        }
    }

    /// Resolve the bearer token: `auth_env` first, then `token_file`.
    /// Returns the header value (`"Bearer …"`); `Err` is warn-and-skip —
    /// the message names the mechanism, never the value.
    fn bearer(&self) -> anyhow::Result<Option<String>> {
        if let Some(var) = &self.auth_env {
            match std::env::var(var) {
                Ok(v) if !v.trim().is_empty() => {
                    return Ok(Some(format!("Bearer {}", v.trim())));
                }
                _ => anyhow::bail!("env var {var} unset or empty"),
            }
        }
        if let Some(path) = &self.token_file {
            let text = std::fs::read_to_string(path)
                .map_err(|e| anyhow::anyhow!("token_file {}: {e}", path))?;
            let tok = text.trim();
            if tok.is_empty() {
                anyhow::bail!("token_file {path} is empty");
            }
            return Ok(Some(format!("Bearer {tok}")));
        }
        Ok(None)
    }

    /// The streamable-HTTP transport for this spec — custom headers and the
    /// resolved bearer token ride `StreamableHttpClientTransportConfig`; a
    /// bad header name/value or missing credential fails the server, not
    /// the session.
    pub(crate) fn http_transport(
        &self,
        url: &str,
    ) -> anyhow::Result<rmcp::transport::StreamableHttpClientTransport<reqwest::Client>> {
        let mut custom_headers = HashMap::new();
        for (k, v) in &self.headers {
            let name = reqwest::header::HeaderName::try_from(k.as_str())
                .map_err(|e| anyhow::anyhow!("bad header name {k:?}: {e}"))?;
            let value = reqwest::header::HeaderValue::from_str(&expand_env(v))
                .map_err(|e| anyhow::anyhow!("bad value for header {k}: {e}"))?;
            custom_headers.insert(name, value);
        }
        let auth_header = self.bearer().map_err(|e| anyhow::anyhow!("{e:#}"))?;
        let mut builder = reqwest::Client::builder()
            .pool_max_idle_per_host(0)
            .redirect(reqwest::redirect::Policy::none());
        if let Some(secs) = self.timeout_secs {
            builder = builder.timeout(std::time::Duration::from_secs(secs));
        }
        let client = builder.build().context("build http client")?;
        let mut config =
            rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig::default();
        config.uri = url.into();
        config.auth_header = auth_header;
        config.custom_headers = custom_headers;
        Ok(rmcp::transport::StreamableHttpClientTransport::with_client(
            client, config,
        ))
    }
}
