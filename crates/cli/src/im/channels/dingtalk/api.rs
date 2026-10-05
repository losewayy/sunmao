//! DingTalk's REST contract: the token exchange and the response envelope.
//! The adapter owns the transport; everything that shapes a request or reads
//! a response lives here, so the URL/body/error mapping is one place and
//! offline-testable.

use anyhow::{Context as _, Result};

use super::protocol::{DM_SEND_PATH, STREAM_OPEN_PATH};

/// `expires_in` fallback when the platform omits it.
const DEFAULT_TTL_SECS: u64 = 7200;
/// The reference caps a token TTL at a day; a longer one is a broken answer.
const MAX_TTL_SECS: u64 = 86_400;
/// A TTL below this is not worth caching (and would spin the token endpoint).
const MIN_TTL_SECS: u64 = 60;

/// `POST {base}/oauth2/{corpId}/token` with the corp-scoped app credentials.
/// The corp id rides the path, so it is percent-encoded rather than trusted.
pub fn token_request(
    api_base: &str,
    corp_id: &str,
    client_id: &str,
    client_secret: &str,
) -> (String, serde_json::Value) {
    let url = format!(
        "{}/oauth2/{}/token",
        base(api_base),
        encode_segment(corp_id)
    );
    let body = serde_json::json!({
        "client_id": client_id,
        "client_secret": client_secret,
        "grant_type": "client_credentials",
    });
    (url, body)
}

/// `POST {base}/gateway/connections/open` — the Stream registration.
pub fn open_url(api_base: &str) -> String {
    format!("{}{STREAM_OPEN_PATH}", base(api_base))
}

/// `POST {base}/robot/oToMessages/batchSend` — one-to-one robot messages.
pub fn dm_send_url(api_base: &str) -> String {
    format!("{}{DM_SEND_PATH}", base(api_base))
}

/// The registration answer: the one-shot endpoint and its ticket.
pub fn stream_registration(v: &serde_json::Value) -> Result<(String, String)> {
    let endpoint = v["endpoint"]
        .as_str()
        .filter(|s| !s.is_empty())
        .context("dingtalk stream open: no endpoint")?;
    let ticket = v["ticket"]
        .as_str()
        .filter(|s| !s.is_empty())
        .context("dingtalk stream open: no ticket")?;
    Ok((endpoint.to_string(), ticket.to_string()))
}

/// The WS URL: the endpoint plus its ticket. The endpoint comes back with a
/// query string of its own, so the ticket is appended to it, not glued on.
pub fn append_ticket(endpoint: &str, ticket: &str) -> Result<String> {
    let mut url = reqwest::Url::parse(endpoint).context("dingtalk stream endpoint")?;
    url.query_pairs_mut().append_pair("ticket", ticket);
    Ok(url.to_string())
}

/// The token answer: `(token, ttl_secs)`.
pub fn access_token(v: &serde_json::Value) -> Result<(String, u64)> {
    let token = v["access_token"]
        .as_str()
        .filter(|s| !s.is_empty())
        .with_context(|| {
            format!(
                "dingtalk token: code={} msg={}",
                code_of(v).unwrap_or_else(|| "-".to_string()),
                message_of(v)
            )
        })?
        .to_string();
    let ttl = v["expires_in"]
        .as_u64()
        .or_else(|| v["expires_in"].as_str().and_then(|s| s.parse().ok()))
        .unwrap_or(DEFAULT_TTL_SECS)
        .clamp(MIN_TTL_SECS, MAX_TTL_SECS);
    Ok((token, ttl))
}

/// The business code of a response, when it carries one. DingTalk spells it
/// `code`/`errcode`/`errorCode`, as a number or a string (the v1.0 API uses
/// names like `invalidParameter`), and a *present* non-zero code is a
/// failure even when the HTTP status says 200.
pub fn code_of(v: &serde_json::Value) -> Option<String> {
    ["code", "errcode", "errorCode"]
        .into_iter()
        .find_map(|k| match &v[k] {
            serde_json::Value::Number(n) => Some(n.to_string()),
            serde_json::Value::String(s) if !s.is_empty() => Some(s.clone()),
            _ => None,
        })
}

/// A response with no code, or with the code `0`, succeeded. Every other
/// spelling failed.
fn code_is_success(code: Option<&str>) -> bool {
    matches!(code, None | Some("0"))
}

/// The human half of a failed response — the platform spells it five ways.
pub fn message_of(v: &serde_json::Value) -> &str {
    ["message", "errmsg", "msg", "errorMessage", "error"]
        .into_iter()
        .find_map(|k| v[k].as_str())
        .unwrap_or("-")
}

/// One response envelope: the HTTP status *and* the business code. Both are
/// error surfaces, and an absent code is a success (nothing disagreed).
pub fn check_response(
    stage: &str,
    status: reqwest::StatusCode,
    v: &serde_json::Value,
) -> Result<()> {
    let code = code_of(v);
    if status.is_success() && code_is_success(code.as_deref()) {
        return Ok(());
    }
    anyhow::bail!(
        "dingtalk {stage}: http={status} code={} msg={}",
        code.as_deref().unwrap_or("-"),
        message_of(v)
    )
}

/// Percent-encode one path segment, unreserved characters kept verbatim.
fn encode_segment(s: &str) -> String {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(b as char);
            }
            _ => {
                out.push('%');
                out.push(HEX[(b >> 4) as usize] as char);
                out.push(HEX[(b & 0xf) as usize] as char);
            }
        }
    }
    out
}

/// The API base without a trailing slash — a config that writes one must not
/// produce `//gateway/...`.
fn base(api_base: &str) -> &str {
    api_base.trim_end_matches('/')
}
