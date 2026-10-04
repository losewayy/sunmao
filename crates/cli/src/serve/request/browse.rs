//! 浏览器面板代理 — `GET /browse?url=…` 从宿主侧拉取 HTML 文档，再从
//! 本源回给页面：dock 浏览器标签页的 iframe 因此与面板同源，批注层能
//! `elementFromPoint` 拾取真实元素（跨源直挂的 iframe 拿不到 DOM）。
//! 注入 `<base href>` 让相对资源仍从原站加载；上游的 CSP/X-Frame-
//! Options 不转发（响应是我们构造的）。iframe 侧 sandbox 不挂
//! allow-scripts——页面 JS 永远不会以本源身份执行，代理不放大 API 面。

use std::sync::Arc;

use super::super::host::Shared;
use super::HostResponse;

/// `GET /browse?url=…` — one HTML document through the host's network,
/// rebased onto ours. HTML only (415 otherwise): the pane is a page
/// viewer, not a general content proxy.
pub(super) async fn page(_s: &Arc<Shared>, url: Option<String>) -> HostResponse {
    let Some(url) = url.filter(|u| !u.trim().is_empty()) else {
        return HostResponse::err(400, "url required".into());
    };
    let client = match reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(20))
        .redirect(reqwest::redirect::Policy::limited(5))
        .build()
    {
        Ok(c) => c,
        Err(e) => return HostResponse::err(500, format!("http client: {e}")),
    };
    let resp = match client.get(&url).send().await {
        Ok(r) => r,
        Err(e) => return HostResponse::err(502, format!("fetch failed: {e}")),
    };
    let final_url = resp.url().to_string();
    let ctype = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_lowercase();
    if !ctype.contains("text/html") {
        return HostResponse::err(415, format!("not an HTML document ({ctype})"));
    }
    let html = match resp.text().await {
        Ok(t) => t,
        Err(e) => return HostResponse::err(502, format!("read failed: {e}")),
    };
    HostResponse::bytes(
        200,
        vec![("content-type".into(), "text/html; charset=utf-8".into())],
        rebase(&html, &final_url).into_bytes(),
    )
}

/// Drop `<base>` tags and CSP metas, then inject our `<base href>` right
/// after `<head>` — tag-targeted scans, not a full parse: a `<meta` /
/// `<base` shaped literal inside a script string at worst drops a string,
/// it can't corrupt structure.
fn rebase(html: &str, base: &str) -> String {
    let lower = html.to_lowercase();
    let mut out = String::with_capacity(html.len() + base.len() + 48);
    let mut i = 0;
    loop {
        // earliest of <base> / a CSP <meta> — a plain find+or_else would
        // skip a CSP meta sitting before the first <base
        let mut t = lower[i..].find("<base").map(|o| i + o);
        if let Some(m) = lower[i..].find("<meta") {
            let m = i + m;
            let csp = lower[m..]
                .find('>')
                .map(|e| lower[m..m + e].contains("content-security-policy"))
                .unwrap_or(false);
            if csp && t.map(|t| m < t).unwrap_or(true) {
                t = Some(m);
            }
        }
        let Some(t) = t else { break };
        let Some(e) = lower[t..].find('>') else { break };
        out.push_str(&html[i..t]);
        i = t + e + 1;
    }
    out.push_str(&html[i..]);
    let inject = format!("<base href=\"{}\">", base);
    let lower_out = out.to_lowercase();
    if let Some(h) = lower_out.find("<head")
        && let Some(e) = lower_out[h..].find('>')
    {
        out.insert_str(h + e + 1, &inject);
        return out;
    }
    format!("{inject}{out}")
}
