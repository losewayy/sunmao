//! Minimal web fetch → readable text. Naive tag-strip is deliberate:
//! the model needs content, not a browser.

/// Fetch a URL and return text with tags/scripts stripped.
pub async fn fetch_text(url: &str) -> anyhow::Result<String> {
    let resp = reqwest::Client::builder()
        .user_agent("sunmao/0.1")
        .timeout(std::time::Duration::from_secs(20))
        .build()?
        .get(url)
        .send()
        .await?;
    if !resp.status().is_success() {
        anyhow::bail!("http {}", resp.status());
    }
    let body = resp.text().await?;
    Ok(strip_tags(&body))
}

fn strip_tags(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let mut in_tag = false;
    let mut skip_depth = 0i32; // inside <script>/<style>
    let bytes = html.as_bytes();
    let lower = html.to_lowercase();
    let lb = lower.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if !in_tag {
            if lb[i..].starts_with(b"<script") || lb[i..].starts_with(b"<style") {
                skip_depth += 1;
                in_tag = true;
            } else if lb[i..].starts_with(b"</script") || lb[i..].starts_with(b"</style") {
                if skip_depth > 0 {
                    skip_depth -= 1;
                }
                in_tag = true;
            } else if bytes[i] == b'<' {
                in_tag = true;
            } else if skip_depth == 0 {
                if lb[i..].starts_with(b"&nbsp;") {
                    out.push(' ');
                    i += 6;
                    continue;
                }
                out.push(html[i..].chars().next().unwrap());
            }
        } else if bytes[i] == b'>' {
            in_tag = false;
            if skip_depth == 0 {
                out.push(' ');
            }
        }
        i += 1;
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
mod tests {
    use super::strip_tags;

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
}
