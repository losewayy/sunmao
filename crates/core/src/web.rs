//! Minimal web fetch → readable text. Naive tag-strip is deliberate:
//! the model needs content, not a browser.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, ToSocketAddrs};
use std::sync::OnceLock;
use std::time::Duration;

use futures_util::StreamExt;

/// Whole-request budget, body included. Sits under WebFetch's `60` row in
/// `tool-timeouts.txt`, so a stalled server makes the tool report its own
/// error instead of being abandoned by the turn loop's watchdog.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(20);

/// Connect budget. Without it a SYN black hole — a blocked IP, a TUN route
/// that never answers — burns the whole request budget before anything is
/// reported; measured connects are sub-second, so 5s fails fast without
/// cutting a slow handshake short.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// Cap on bytes pulled off the wire. The strip below is an in-memory scan
/// and the tool only ever shows 24KB of text, so a page past this is a
/// reject, not a truncate. Raise it only alongside a streaming extractor.
const MAX_BODY_BYTES: usize = 2 * 1024 * 1024;

/// Redirect chain cap, matching reqwest's own default — a custom policy
/// replaces it, so the limit has to be restated.
const MAX_REDIRECT_HOPS: usize = 10;

/// Set to `1` to let WebFetch reach loopback and private-network hosts. The
/// default is refusal — a URL the model chose is not consent to probe the
/// machine's own network — but a local dev server is a real use, so the
/// block is a default rather than a cage.
const ALLOW_PRIVATE_ENV: &str = "SUNMAO_WEBFETCH_ALLOW_PRIVATE";

/// One pool per policy, kept for the process, so the pool outlives a single
/// fetch: a second request to a host already visited reuses the open socket
/// instead of paying a fresh handshake. The opted-in pool is separate so a
/// caller that allowed private addresses cannot widen what the guarded pool
/// reaches. Idle sockets are reaped by reqwest's own idle timeout; a build
/// failure is cached too, since the config is static and a retry would fail
/// identically.
static GUARDED_CLIENT: OnceLock<Result<reqwest::Client, String>> = OnceLock::new();
static OPEN_CLIENT: OnceLock<Result<reqwest::Client, String>> = OnceLock::new();

fn client(allow_private: bool) -> anyhow::Result<&'static reqwest::Client> {
    let cell = if allow_private {
        &OPEN_CLIENT
    } else {
        &GUARDED_CLIENT
    };
    cell.get_or_init(|| build_client(allow_private))
        .as_ref()
        .map_err(|e| anyhow::anyhow!("http client init: {e}"))
}

fn build_client(allow_private: bool) -> Result<reqwest::Client, String> {
    let mut builder = reqwest::Client::builder()
        .user_agent("sunmao/0.1")
        .connect_timeout(CONNECT_TIMEOUT)
        .timeout(REQUEST_TIMEOUT)
        .redirect(reqwest::redirect::Policy::custom(move |attempt| {
            if attempt.previous().len() > MAX_REDIRECT_HOPS {
                return attempt.error("too many redirects");
            }
            // checked per hop: a public URL that 302s to a metadata address
            // is still a private fetch
            if !allow_private && let Some(reason) = private_target(attempt.url().as_str()) {
                return attempt.error(reason);
            }
            attempt.follow()
        }));
    if !allow_private {
        builder = builder.dns_resolver(GuardedResolver);
    }
    builder.build().map_err(|e| e.to_string())
}

/// DNS for the guarded client: check every address the name resolves to,
/// then hand back exactly those addresses. The connector connects to the
/// addresses this lookup returned, so a name cannot resolve to a public
/// address for the check and a private one for the connect.
struct GuardedResolver;

impl reqwest::dns::Resolve for GuardedResolver {
    fn resolve(&self, name: reqwest::dns::Name) -> reqwest::dns::Resolving {
        Box::pin(async move {
            let host = name.as_str().to_owned();
            let looked_up = {
                let looked_up_for = host.clone();
                tokio::task::spawn_blocking(move || {
                    (looked_up_for.as_str(), 0u16)
                        .to_socket_addrs()
                        .map(|addrs| addrs.collect::<Vec<SocketAddr>>())
                })
                .await
            };
            let addrs = match looked_up {
                Ok(Ok(addrs)) => addrs,
                Ok(Err(e)) => return Err(format!("cannot resolve {host}: {e}").into()),
                Err(e) => return Err(format!("lookup for {host} failed: {e}").into()),
            };
            checked_addrs(&host, addrs)
        })
    }
}

/// Every address has to pass: a name answering with one public and one
/// private address is still a private fetch.
fn checked_addrs(
    host: &str,
    addrs: Vec<SocketAddr>,
) -> Result<reqwest::dns::Addrs, Box<dyn std::error::Error + Send + Sync>> {
    if addrs.is_empty() {
        return Err(format!("{host} resolved to no addresses").into());
    }
    if let Some(blocked) = addrs.iter().find(|addr| is_private(addr.ip())) {
        return Err(private_reason(&blocked.ip().to_string()).into());
    }
    Ok(Box::new(addrs.into_iter()))
}

/// The reason `url` is off limits, or `None` when its host is fine. Literal
/// addresses and `localhost` are settled here; a real name is left to
/// [`GuardedResolver`], which resolves it once for the connection itself.
fn private_target(url: &str) -> Option<String> {
    let parsed = reqwest::Url::parse(url).ok()?;
    let host = parsed.host_str()?;
    let bare = host
        .strip_prefix('[')
        .and_then(|h| h.strip_suffix(']'))
        .unwrap_or(host);
    if let Ok(ip) = bare.parse::<IpAddr>() {
        return is_private(ip).then(|| private_reason(&ip.to_string()));
    }
    let name = bare.trim_end_matches('.').to_ascii_lowercase();
    (name == "localhost" || name.ends_with(".localhost")).then(|| private_reason(&name))
}

fn private_reason(target: &str) -> String {
    format!(
        "refusing private target {target}: WebFetch does not reach loopback, \
         link-local or private-network addresses unless {ALLOW_PRIVATE_ENV}=1"
    )
}

/// The reference implementation's blocklist (`local-fetch-url.ts:207-221`):
/// unspecified, loopback, link-local, RFC 1918, CGNAT, and IPv6
/// unique-local/link-local. IPv4-mapped and IPv4-compatible IPv6
/// (`::ffff:127.0.0.1`) are judged as the IPv4 address they encode, so the
/// mapping cannot smuggle loopback past the v4 table.
fn is_private(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => is_private_v4(v4),
        IpAddr::V6(v6) => v6
            .to_ipv4()
            .map_or_else(|| is_private_v6(v6), is_private_v4),
    }
}

fn is_private_v4(ip: Ipv4Addr) -> bool {
    let [a, b, ..] = ip.octets();
    matches!(
        (a, b),
        (0, _) | (10, _) | (100, 64..=127) | (127, _) | (169, 254) | (172, 16..=31) | (192, 168)
    )
}

fn is_private_v6(ip: Ipv6Addr) -> bool {
    let [a, ..] = ip.segments();
    ip.is_loopback() // ::1/128
        || ip.is_unspecified() // ::/128
        || (a & 0xfe00) == 0xfc00 // fc00::/7 unique-local
        || (a & 0xffc0) == 0xfe80 // fe80::/10 link-local
}

/// Fetch a URL and return text with tags/scripts stripped.
pub async fn fetch_text(url: &str) -> anyhow::Result<String> {
    let allow_private = std::env::var(ALLOW_PRIVATE_ENV).is_ok_and(|v| v == "1");
    fetch_text_with(url, allow_private).await
}

/// The guard takes the flag as an argument instead of reading the
/// environment, so tests can exercise both sides without mutating
/// process-wide state.
async fn fetch_text_with(url: &str, allow_private: bool) -> anyhow::Result<String> {
    if !allow_private && let Some(reason) = private_target(url) {
        anyhow::bail!("{reason}");
    }
    let resp = client(allow_private)?.get(url).send().await?;
    if !resp.status().is_success() {
        anyhow::bail!("http {}", resp.status());
    }
    if let Some(len) = resp.content_length()
        && len > MAX_BODY_BYTES as u64
    {
        anyhow::bail!("body too large: {len} bytes exceeds {MAX_BODY_BYTES}");
    }
    let content_type = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    let body = read_capped(resp).await?;
    Ok(strip_tags(&decode_body(&body, content_type.as_deref())))
}

/// Read the body, stopping at [`MAX_BODY_BYTES`] — `Response::text` would
/// buffer an unbounded stream and only the request timeout would stop it.
async fn read_capped(resp: reqwest::Response) -> anyhow::Result<Vec<u8>> {
    let mut buf: Vec<u8> = Vec::new();
    let mut stream = resp.bytes_stream();
    while let Some(chunk) = stream.next().await {
        if !push_capped(&mut buf, &chunk?, MAX_BODY_BYTES) {
            break;
        }
    }
    Ok(buf)
}

/// `charset` out of a `Content-Type` value — the only signal a GBK or
/// Shift-JIS page gives that its bytes are not UTF-8. Parameters are
/// `;`-separated after the media type, the name is case-insensitive, and the
/// value may be quoted.
fn charset_label(content_type: &str) -> Option<&str> {
    content_type
        .split(';')
        .skip(1) // the media type itself is not a parameter
        .filter_map(|param| param.split_once('='))
        .find(|(name, _)| name.trim().eq_ignore_ascii_case("charset"))
        .map(|(_, value)| value.trim().trim_matches(['"', '\'']))
        .filter(|label| !label.is_empty())
}

/// Decode captured bytes using the `charset` of the response's
/// `Content-Type`, with UTF-8 when the header is absent or names something
/// unknown. Byte-exact capture plus a label-driven decode is what keeps the
/// 2 MiB cap: `Response::text` would decode too, but only by buffering the
/// whole body first.
fn decode_body(bytes: &[u8], content_type: Option<&str>) -> String {
    let encoding = content_type
        .and_then(charset_label)
        .and_then(|label| encoding_rs::Encoding::for_label(label.as_bytes()))
        .unwrap_or(encoding_rs::UTF_8);
    // `decode` sniffs a BOM first and replaces malformed sequences, so a
    // truncated or mislabelled page still yields text instead of an error.
    encoding.decode(bytes).0.into_owned()
}

/// Append as much of `chunk` as fits under `cap`; false once the cap is hit.
fn push_capped(buf: &mut Vec<u8>, chunk: &[u8], cap: usize) -> bool {
    let room = cap.saturating_sub(buf.len());
    if chunk.len() >= room {
        buf.extend_from_slice(&chunk[..room]);
        return false;
    }
    buf.extend_from_slice(chunk);
    true
}

fn starts_with_ci(hay: &[u8], needle: &[u8]) -> bool {
    hay.len() >= needle.len() && hay[..needle.len()].eq_ignore_ascii_case(needle)
}

/// Case-insensitive `needle` search over `hay`.
fn find_ci(hay: &[u8], needle: &[u8]) -> Option<usize> {
    if hay.len() < needle.len() {
        return None;
    }
    (0..=hay.len() - needle.len()).find(|&i| starts_with_ci(&hay[i..], needle))
}

/// `rest` opens the element `name`, not merely a tag whose name starts with
/// it: `<scripting>` is an ordinary tag, not a skip region.
fn opens_tag(rest: &[u8], name: &[u8]) -> bool {
    if !starts_with_ci(rest, name) {
        return false;
    }
    rest.get(name.len())
        .is_none_or(|b| b.is_ascii_whitespace() || *b == b'>' || *b == b'/')
}

/// Where the scan is. `Script`/`Style` are states of their own — folding them
/// into one `in_tag` flag is what let a bare `<` inside script content
/// (`if (a<b)`) read as a tag open, so the matching `</script` was never
/// recognised again and the whole rest of the page was dropped in silence.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Scan {
    Text,
    Tag,
    Script,
    Style,
}

/// Byte scan for tag boundaries, char-aware when emitting text.
///
/// Text state advances by whole chars (`<` and `>` are ASCII, so neither
/// can appear inside a UTF-8 sequence, and tag state never slices the
/// string) — pushing a whole char while advancing one byte is what made a
/// page with any non-ASCII character, an em dash or a curly quote,
/// panic the calling task instead of returning a result.
fn strip_tags(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let bytes = html.as_bytes();
    let mut state = Scan::Text;
    let mut i = 0;
    while i < bytes.len() {
        let rest = &bytes[i..];
        match state {
            // Skip the body wholesale: nothing inside it is a boundary, so
            // no `<` there can end the skip early.
            Scan::Script | Scan::Style => {
                let close: &[u8] = if state == Scan::Script {
                    b"</script"
                } else {
                    b"</style"
                };
                let Some(off) = find_ci(rest, close) else {
                    break; // unclosed: the rest is script/style data
                };
                i += off + close.len();
                state = Scan::Tag; // swallow the close tag's own `>`
            }
            Scan::Tag => {
                if bytes[i] == b'>' {
                    state = Scan::Text;
                    // a separator, so `a</b>b` reads as `a b`, not `ab`
                    out.push(' ');
                }
                i += 1;
            }
            Scan::Text => {
                if opens_tag(rest, b"<script") {
                    state = Scan::Script;
                    i += b"<script".len();
                } else if opens_tag(rest, b"<style") {
                    state = Scan::Style;
                    i += b"<style".len();
                } else if bytes[i] == b'<' {
                    state = Scan::Tag;
                    i += 1;
                } else if starts_with_ci(rest, b"&nbsp;") {
                    out.push(' ');
                    i += 6;
                } else {
                    // `i` sits on a char boundary: every arm above consumes
                    // whole ASCII units, so this cannot slice mid-character.
                    let Some(c) = html[i..].chars().next() else {
                        break;
                    };
                    out.push(c);
                    i += c.len_utf8();
                }
            }
        }
    }
    // collapse blank runs
    let mut collapsed = String::with_capacity(out.len());
    let mut blank = 0;
    for line in out.lines() {
        let t = line.trim();
        if t.is_empty() {
            blank += 1;
            if blank <= 1 {
                collapsed.push('\n');
            }
        } else {
            blank = 0;
            collapsed.push_str(t);
            collapsed.push('\n');
        }
    }
    collapsed
}

#[cfg(test)]
mod tests;
