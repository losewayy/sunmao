//! Tests for the fetch-to-text path: the tag/script scanner, charset
//! decode, connection reuse, and the private-address guard.
//!
//! Separate file because `web.rs` shares the arch gate 600-line budget
//! (`cargo xtask arch`, rule 1).

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

use super::{
    GuardedResolver, charset_label, checked_addrs, decode_body, fetch_text, fetch_text_with,
    is_private, private_target, push_capped, strip_tags,
};

/// Minimal HTTP/1.1 stub: counts TCP connections and answers `/ok` and
/// `/hop`. A shared connection pool shows up as one connection serving
/// two fetches.
async fn spawn_stub() -> (std::net::SocketAddr, Arc<AtomicUsize>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind stub");
    let addr = listener.local_addr().expect("stub addr");
    let conns = Arc::new(AtomicUsize::new(0));
    let counted = Arc::clone(&conns);
    tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = listener.accept().await else {
                return;
            };
            counted.fetch_add(1, Ordering::SeqCst);
            tokio::spawn(async move {
                let mut buf: Vec<u8> = Vec::new();
                loop {
                    let mut chunk = [0u8; 1024];
                    let Ok(n) = sock.read(&mut chunk).await else {
                        return;
                    };
                    if n == 0 {
                        return;
                    }
                    buf.extend_from_slice(&chunk[..n]);
                    let Some(end) = buf.windows(4).position(|w| w == b"\r\n\r\n") else {
                        continue;
                    };
                    let head = String::from_utf8_lossy(&buf[..end]).to_string();
                    buf.drain(..end + 4);
                    let path = head.split_whitespace().nth(1).unwrap_or("/").to_string();
                    let resp: &[u8] = match path.as_str() {
                            "/ok" => {
                                b"HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: 7\r\n\r\nok page"
                            }
                            "/hop" => {
                                b"HTTP/1.1 302 Found\r\nLocation: /ok\r\nContent-Length: 0\r\n\r\n"
                            }
                            _ => b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n",
                        };
                    if sock.write_all(resp).await.is_err() {
                        return;
                    }
                }
            });
        }
    });
    (addr, conns)
}

/// The model naming a loopback target is not consent to probe the host's
/// own network: refuse it before any socket is opened.
#[tokio::test]
async fn private_targets_are_refused_by_default() {
    let (addr, conns) = spawn_stub().await;
    let err = fetch_text(&format!("http://{addr}/ok"))
        .await
        .expect_err("loopback must be refused");
    assert!(err.to_string().contains("private"), "{err}");
    assert_eq!(
        conns.load(Ordering::SeqCst),
        0,
        "a refused target must not be dialled"
    );
}

#[tokio::test]
async fn metadata_and_private_ranges_are_refused_by_default() {
    for url in [
        "http://169.254.169.254/latest/meta-data/",
        "http://[::ffff:127.0.0.1]:1/",
        "http://[::1]:1/",
        "http://10.0.0.1/",
        "http://172.16.0.1/",
        "http://192.168.1.1/",
        "http://100.64.0.1/",
        "http://0.0.0.0/",
        "http://localhost:1/",
        "http://[fe80::1]:1/",
        "http://[fd00::1]:1/",
    ] {
        let err = fetch_text(url).await.expect_err("must be refused");
        assert!(err.to_string().contains("private"), "{url}: {err}");
    }
}

/// The escape hatch: a local dev server is a real use, so the guard is a
/// default. Opted in, loopback loads and its redirects still follow.
#[tokio::test]
async fn the_escape_hatch_allows_a_local_target() {
    let (addr, _) = spawn_stub().await;
    let direct = fetch_text_with(&format!("http://{addr}/ok"), true)
        .await
        .expect("opted-in loopback fetch");
    assert!(direct.contains("ok page"), "{direct}");
    let hopped = fetch_text_with(&format!("http://{addr}/hop"), true)
        .await
        .expect("opted-in redirect");
    assert!(hopped.contains("ok page"), "{hopped}");
}

/// The blocklist, entry by entry, plus the IPv4-mapped forms that would
/// otherwise smuggle loopback past the v4 table.
#[test]
fn the_blocklist_covers_reference_entries_and_mapped_forms() {
    for ip in [
        "0.0.0.0",
        "0.1.2.3",
        "10.0.0.1",
        "100.64.0.1",
        "100.127.255.255",
        "127.0.0.1",
        "169.254.169.254",
        "172.16.0.1",
        "172.31.255.255",
        "192.168.1.1",
        "::",
        "::1",
        "::ffff:127.0.0.1",
        "::ffff:10.0.0.1",
        "::127.0.0.1",
        "fc00::1",
        "fd00::1",
        "fe80::1",
        "febf::1",
    ] {
        assert!(
            is_private(ip.parse().expect("ip literal")),
            "{ip} must be private"
        );
    }
    for ip in [
        "8.8.8.8",
        "1.1.1.1",
        "93.184.216.34",
        "100.63.255.255",
        "100.128.0.0",
        "172.15.0.1",
        "172.32.0.1",
        "::ffff:8.8.8.8",
        "2606:4700::1111",
    ] {
        assert!(
            !is_private(ip.parse().expect("ip literal")),
            "{ip} must stay reachable"
        );
    }
}

#[test]
fn the_guard_reads_literal_hosts_without_dns() {
    for url in [
        "http://127.0.0.1:8080/x",
        "http://localhost:8080/",
        "http://LOCALHOST./",
        "http://api.localhost/",
        "http://169.254.169.254/latest/meta-data/",
        "http://[::1]:80/",
        "http://[::ffff:127.0.0.1]/",
    ] {
        assert!(private_target(url).is_some(), "{url} must be refused");
    }
    for url in ["http://93.184.216.34/", "https://example.com/"] {
        assert!(private_target(url).is_none(), "{url} must be allowed");
    }
}

#[test]
fn a_name_with_one_private_address_is_refused_wholesale() {
    let mixed: Vec<SocketAddr> = vec![
        "93.184.216.34:443".parse().expect("public addr"),
        "127.0.0.1:443".parse().expect("private addr"),
    ];
    assert!(checked_addrs("example.com", mixed).is_err());
    assert!(checked_addrs("example.com", Vec::new()).is_err());
    let public: Vec<SocketAddr> = vec!["93.184.216.34:443".parse().expect("public addr")];
    assert_eq!(
        checked_addrs("example.com", public)
            .expect("allowed")
            .count(),
        1
    );
}

/// The resolver, not just the URL guard: `localhost` resolves to
/// loopback, and that answer is refused before any connect.
#[tokio::test]
async fn the_resolver_refuses_a_name_that_resolves_private() {
    use reqwest::dns::Resolve;

    let name: reqwest::dns::Name = "localhost".parse().expect("valid name");
    // `Addrs` is a boxed iterator, so the Ok arm cannot be unwrapped with
    // `expect_err` (it needs Debug on both sides)
    let err = match GuardedResolver.resolve(name).await {
        Ok(_) => panic!("localhost must be refused"),
        Err(e) => e,
    };
    assert!(err.to_string().contains("private"), "{err}");
}

/// One client per process, so a second fetch to a host already visited
/// rides the pooled socket instead of paying a fresh TCP handshake.
#[tokio::test]
async fn second_fetch_reuses_the_pooled_connection() {
    let (addr, conns) = spawn_stub().await;
    let url = format!("http://{addr}/ok");
    let first = fetch_text_with(&url, true).await.expect("first fetch");
    let second = fetch_text_with(&url, true).await.expect("second fetch");
    assert!(first.contains("ok page"), "{first}");
    assert!(second.contains("ok page"), "{second}");
    assert_eq!(
        conns.load(Ordering::SeqCst),
        1,
        "each fetch opened its own connection"
    );
}

#[test]
fn keeps_body_text_drops_script_style() {
    let html = "<html><head><title>T</title><style>body{color:red}</style></head>\
            <body><p>Hello world</p><script>evil()</script><p>Bye</p></body></html>";
    let out = strip_tags(html);
    assert!(out.contains("Hello world"), "{out}");
    assert!(out.contains("Bye"), "{out}");
    assert!(!out.contains("evil()"), "{out}");
    assert!(!out.contains("color:red"), "{out}");
}

#[test]
fn example_dot_com_shape() {
    let html = "<!doctype html><html><head><title>Example Domain</title>\
            <style>x{y:z}</style></head><body><p>This domain is for use in examples.</p>\
            <a href=https://iana.org>Learn more</a></body></html>";
    let out = strip_tags(html);
    assert!(out.contains("Example Domain"), "{out}");
    assert!(out.contains("This domain is for use in examples."), "{out}");
    assert!(out.contains("Learn more"), "{out}");
}

/// Regression: a multi-byte character in text state used to advance one
/// byte past the char, so the next iteration sliced mid-character and
/// panicked — any real page (em dashes, curly quotes, CJK) killed the
/// turn instead of returning text.
#[test]
fn multibyte_text_does_not_panic() {
    let html =
        "<html><body><p>asyncio \u{2014} the docs \u{2019}quoted\u{2019} 中文</p></body></html>";
    let out = strip_tags(html);
    assert!(out.contains("asyncio \u{2014} the docs"), "{out}");
    assert!(out.contains("\u{2019}quoted\u{2019}"), "{out}");
    assert!(out.contains("中文"), "{out}");
}

#[test]
fn multibyte_attribute_value_does_not_panic() {
    let html = "<html><body><img alt=\"\u{65e5}\u{672c}\" src=x><p>text</p></body></html>";
    let out = strip_tags(html);
    assert!(out.contains("text"), "{out}");
}

#[test]
fn nbsp_entity_becomes_one_space() {
    let out = strip_tags("<p>a&nbsp;b&NBSP;c</p>");
    assert!(out.contains("a b c"), "{out}");
    assert!(!out.contains("nbsp"), "{out}");
}

#[test]
fn close_script_tag_ends_the_skip() {
    // the `</script` arm fires only while `in_tag` is false, so content
    // after a normally closed script block must survive
    let out = strip_tags("<script>var a = 1;</script><p>tail</p>");
    assert!(!out.contains("var a = 1;"), "{out}");
    assert!(out.contains("tail"), "{out}");
}

/// Regression: a bare `<` in script content used to flip the one flag
/// that meant both "inside a tag" and "skipping", so `</script` never
/// matched again and every byte after it was dropped — one `if (a<b)`
/// cost the rest of the page.
#[test]
fn bare_lt_inside_script_keeps_the_rest() {
    let out = strip_tags("<script>if (a<b) {}</script><p>tail</p>");
    assert!(!out.contains("a<b"), "{out}");
    assert!(out.contains("tail"), "{out}");
}

#[test]
fn script_body_never_leaks_through_gt_or_entities() {
    let out = strip_tags("<script>if (a>b) { x = \"&nbsp;\"; }</script><p>tail</p>");
    assert!(out.contains("tail"), "{out}");
    assert!(!out.contains("x ="), "{out}");
    assert!(!out.contains('\u{a0}'), "{out}");
}

#[test]
fn bare_lt_inside_style_keeps_the_rest() {
    let out = strip_tags("<style>a::before{content:\"<\"}</style><p>tail</p>");
    assert!(out.contains("tail"), "{out}");
    assert!(!out.contains("content"), "{out}");
}

#[test]
fn unterminated_script_swallows_the_remainder() {
    // an unclosed <script> is script data to the end of the document
    let out = strip_tags("<p>head</p><script>if (a<b) { forever()");
    assert!(out.contains("head"), "{out}");
    assert!(!out.contains("forever"), "{out}");
}

#[test]
fn script_prefixed_tag_names_are_not_script() {
    let out = strip_tags("<scripting>keep</scripting><stylesheet>alsokeep</stylesheet>");
    assert!(out.contains("keep"), "{out}");
    assert!(out.contains("alsokeep"), "{out}");
}

/// Regression: a GBK page (the audit's case) came back as replacement
/// characters because the body was decoded as UTF-8 lossy no matter what
/// the server declared.
#[test]
fn gbk_body_decodes_via_content_type_charset() {
    let gbk = [0xD6u8, 0xD0, 0xCE, 0xC4]; // 中文
    let out = decode_body(&gbk, Some("text/html; charset=gbk"));
    assert_eq!(out, "中文", "{out}");
}

#[test]
fn shift_jis_body_decodes_via_content_type_charset() {
    let sjis = [0x93u8, 0xFA, 0x96, 0x7B, 0x8C, 0xEA]; // 日本語
    let out = decode_body(&sjis, Some("text/html; charset=Shift_JIS"));
    assert_eq!(out, "日本語", "{out}");
}

#[test]
fn quoted_and_upper_case_charset_labels_are_honoured() {
    let gbk = [0xD6u8, 0xD0, 0xCE, 0xC4];
    let out = decode_body(&gbk, Some("text/html; charset=\"GB2312\""));
    assert_eq!(out, "中文", "{out}");
}

#[test]
fn utf8_without_a_charset_header_stays_utf8() {
    let out = decode_body("中文".as_bytes(), Some("text/html"));
    assert_eq!(out, "中文", "{out}");
}

#[test]
fn unknown_charset_label_falls_back_to_utf8() {
    let out = decode_body("ok".as_bytes(), Some("text/html; charset=nonesuch"));
    assert_eq!(out, "ok", "{out}");
}

#[test]
fn utf8_bom_is_stripped() {
    let mut bytes = vec![0xEF, 0xBB, 0xBF];
    bytes.extend_from_slice("hi".as_bytes());
    assert_eq!(decode_body(&bytes, Some("text/html")), "hi");
}

#[test]
fn charset_label_reads_only_the_charset_parameter() {
    assert_eq!(charset_label("text/html; charset=gbk"), Some("gbk"));
    assert_eq!(charset_label("text/html; charset=\"GBK\""), Some("GBK"));
    assert_eq!(
        charset_label("text/html;charset=shift_jis"),
        Some("shift_jis")
    );
    assert_eq!(charset_label("text/html"), None);
    // a `charset` inside another parameter's value is not a charset
    assert_eq!(
        charset_label("multipart/form-data; boundary=--charset=x"),
        None
    );
    assert_eq!(charset_label("text/html; charset="), None);
}

#[test]
fn push_capped_stops_at_the_cap() {
    let mut buf = Vec::new();
    assert!(push_capped(&mut buf, b"abc", 5));
    assert_eq!(buf, b"abc");
    // exact fit: consumed, and reports the cap as reached
    assert!(!push_capped(&mut buf, b"de", 5));
    assert_eq!(buf, b"abcde");
    // already full: nothing more is appended
    assert!(!push_capped(&mut buf, b"fg", 5));
    assert_eq!(buf, b"abcde");
}
