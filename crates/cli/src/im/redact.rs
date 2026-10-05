//! Redaction of credential- and peer-bearing text on the transport error
//! paths. This is not a security boundary — the values never leave the
//! process any other way — it exists so an error can be logged, or written
//! into the delivery ledger, without carrying a token, a one-shot ticket, a
//! peer identifier, or the URL that holds one.

/// What a masked value is replaced with. Call sites that build their own
/// message (instead of masking a whole string) use it directly.
pub(crate) const MASK: &str = "[redacted]";

/// Replace every occurrence of every secret in `text`. An empty secret is
/// skipped: masking it would rewrite the whole string.
pub(crate) fn mask(text: &str, secrets: &[&str]) -> String {
    let mut out = text.to_string();
    for secret in secrets.iter().copied().filter(|s| !s.is_empty()) {
        if out.contains(secret) {
            out = out.replace(secret, MASK);
        }
    }
    out
}

/// An error flattened to text with `secrets` masked. Used where a failing
/// WebSocket handshake can echo the URL it dialed — DingTalk appends a
/// one-shot `ticket` to the endpoint, so the URL itself is a credential.
pub(crate) fn masked_error(err: anyhow::Error, secrets: &[&str]) -> anyhow::Error {
    anyhow::anyhow!("{}", mask(&format!("{err:#}"), secrets))
}

/// Strip the request URL off a transport error. `reqwest`'s `Display`
/// appends ` for url (…)`, and a request URL can carry a credential in its
/// path (Telegram's bot token) or a peer identifier (QQ's `openid`, Feishu's
/// `message_id`), so an error that keeps its URL hands that value to every
/// log line and every delivery-ledger row built from it.
pub(crate) fn transport(err: reqwest::Error) -> reqwest::Error {
    err.without_url()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    /// A loopback port nothing listens on: bind, read the port, drop the
    /// listener.
    fn closed_loopback_port() -> u16 {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.local_addr().unwrap().port()
    }

    #[test]
    fn masking_removes_every_occurrence_of_every_secret() {
        let masked = mask("connect wss://x/?ticket=T1 failed (ticket T1)", &["T1"]);
        assert!(!masked.contains("T1"), "{masked}");
        assert_eq!(masked.matches(MASK).count(), 2, "{masked}");
        // an empty secret would match everywhere
        assert_eq!(mask("plain", &["", "x"]), "plain");
        assert_eq!(mask("nothing to hide", &[]), "nothing to hide");
    }

    #[test]
    fn a_masked_error_keeps_the_diagnosis_and_drops_the_secrets() {
        let url = "wss://example.invalid/connect?ticket=SEC";
        let err = anyhow::anyhow!("{url}: handshake failed").context("dingtalk ws connect");
        let text = format!("{:#}", masked_error(err, &["SEC", url]));
        assert!(!text.contains("SEC"), "{text}");
        assert!(!text.contains(url), "{text}");
        assert!(text.contains("handshake failed"), "{text}");
    }

    #[tokio::test]
    async fn a_transport_error_never_carries_the_url() {
        let token = "123456:AAHtopsecret";
        let url = format!(
            "http://127.0.0.1:{}/bot{token}/sendMessage",
            closed_loopback_port()
        );
        // a short timeout so a filtered port cannot park the test
        let raw = reqwest::Client::builder()
            .timeout(Duration::from_secs(5))
            .build()
            .unwrap()
            .post(&url)
            .send()
            .await
            .unwrap_err();
        assert!(
            raw.to_string().contains(token),
            "reqwest stopped printing the URL, so this test lost its teeth: {raw}"
        );
        let text = format!("{:#}", transport(raw));
        assert!(
            !text.contains(token),
            "the token survived redaction: {text}"
        );
        assert!(!text.contains(&url), "the URL survived redaction: {text}");
    }
}
