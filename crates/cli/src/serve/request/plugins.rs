//! `/plugins` — the frontend-plugin surface. Plugins are `.sunmao/plugins/*.js`
//! in the VIEWED session's project; the pin ledger `.sunmao/plugins.json`
//! (`{"enabled": {"name.js": "<sha256-hex>"}}`) decides what actually loads —
//! a file appearing in the dir is inert until pinned at its content hash, so
//! an agent-written file can't self-enable and a pinned file that's later
//! modified stops serving until re-approved. Serving re-hashes at request
//! time: `GET /plugins/{name}` 404s an enabled-but-tampered file rather than
//! running mutated code the user never signed off on.

use std::sync::Arc;

use sha2::Digest as _;

use sunmao_core::context::MutexRecover as _;

use super::super::host::{Shared, display_path};
use super::{HostResponse, ops::project_for};

const LEDGER: &str = "plugins.json";

fn plugins_dir(project: &std::path::Path) -> std::path::PathBuf {
    project.join(".sunmao").join("plugins")
}

fn ledger_path(project: &std::path::Path) -> std::path::PathBuf {
    project.join(".sunmao").join(LEDGER)
}

/// Pinned hashes by plugin filename. A missing/invalid ledger just means
/// nothing is enabled — an empty set is the honest default, never "load
/// everything".
fn pinned(project: &std::path::Path) -> std::collections::HashMap<String, String> {
    std::fs::read_to_string(ledger_path(project))
        .ok()
        .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok())
        .and_then(|v| {
            v["enabled"].as_object().map(|o| {
                o.iter()
                    .filter_map(|(k, v)| v.as_str().map(|h| (k.clone(), h.to_string())))
                    .collect()
            })
        })
        .unwrap_or_default()
}

fn sha256_hex(bytes: &[u8]) -> String {
    let mut h = sha2::Sha256::new();
    h.update(bytes);
    format!("{:x}", h.finalize())
}

/// One entry per `.js` file in the dir — name, size, enabled flag, and
/// `tampered` for an enabled file whose bytes no longer match its pin.
fn catalog(project: &std::path::Path) -> Vec<serde_json::Value> {
    let pins = pinned(project);
    let mut out: Vec<serde_json::Value> = Vec::new();
    if let Ok(rd) = std::fs::read_dir(plugins_dir(project)) {
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            // regular files only — a `x.js` DIRECTORY is backend-bundle
            // territory (.sunmao/plugins/<name>/) or noise, not a plugin
            if !name.ends_with(".js") || !e.file_type().is_ok_and(|t| t.is_file()) {
                continue;
            }
            let bytes = std::fs::read(e.path()).unwrap_or_default();
            let want = pins.get(&name);
            out.push(serde_json::json!({
                "name": name,
                "bytes": bytes.len(),
                "enabled": want.is_some(),
                // an enabled file whose hash drifted ISN'T served — the UI
                // flags it for re-approval instead of silently running it
                "tampered": want.is_some_and(|w| *w != sha256_hex(&bytes)),
            }));
        }
    }
    out.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));
    out
}

/// A plugin filename is exactly that — no separators, no traversal, a
/// `.js` tail, a non-empty stem in a filesystem-boring charset. `:` is
/// refused because Windows reads `name:stream` as an alternate data
/// stream, and the DOS device stems (`con`, `nul`, …) would make
/// `std::fs::read` block the handler on a device — a one-request local
/// DoS. The charset bound doubles as the frontend's attribute-safety
/// contract (`data-pane="plg:<id>:<slot>"` interpolates the stem raw).
fn valid_name(name: &str) -> bool {
    const DEVICES: &[&str] = &[
        "con", "prn", "aux", "nul", "com1", "com2", "com3", "com4", "com5", "com6", "com7", "com8",
        "com9", "lpt1", "lpt2", "lpt3", "lpt4", "lpt5", "lpt6", "lpt7", "lpt8", "lpt9",
    ];
    let Some(stem) = name.strip_suffix(".js") else {
        return false;
    };
    !stem.is_empty()
        && stem
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
        && !stem.contains("..")
        && !DEVICES.contains(&stem.to_ascii_lowercase().as_str())
}

/// `GET /plugins?sess=` — the management page's inventory: every `.js` in
/// the project's plugins dir with its pin status.
pub(super) fn plugins_list(s: &Arc<Shared>, sess: Option<String>) -> HostResponse {
    let project = project_for(s, sess);
    HostResponse::json(serde_json::json!({
        "dir": display_path(&plugins_dir(&project)),
        "plugins": catalog(&project),
    }))
}

/// `GET /plugins/{name}?sess=` — the loader's fetch: the file's bytes only
/// while it is enabled AND still hash-identical to what was pinned.
pub(super) fn plugin_get(s: &Arc<Shared>, name: &str, sess: Option<String>) -> HostResponse {
    if !valid_name(name) {
        return HostResponse::err(404, "no such plugin".into());
    }
    let project = project_for(s, sess);
    let Some(want) = pinned(&project).get(name).cloned() else {
        return HostResponse::err(404, "plugin not enabled".into());
    };
    let Ok(bytes) = std::fs::read(plugins_dir(&project).join(name)) else {
        return HostResponse::err(404, "plugin file gone".into());
    };
    if sha256_hex(&bytes) != want {
        return HostResponse::err(409, "plugin changed since approval".into());
    }
    HostResponse {
        status: 200,
        headers: vec![(
            "content-type".into(),
            "text/javascript; charset=utf-8".into(),
        )],
        body: bytes,
    }
}

/// `PUT /plugins {sess, name, enabled}` — the pin/unpin write. Enabling
/// stamps the file's CURRENT hash into the ledger (pin what you approved);
/// disabling drops the entry. Either way the fresh catalog goes back.
pub(super) fn plugins_put(s: &Arc<Shared>, sess: Option<String>, body: &[u8]) -> HostResponse {
    let v: serde_json::Value = match serde_json::from_slice(body) {
        Ok(v) => v,
        Err(e) => return HostResponse::err(400, format!("bad json: {e}")),
    };
    let Some(name) = v["name"].as_str().map(str::to_string) else {
        return HostResponse::err(400, "missing name".into());
    };
    if !valid_name(&name) {
        return HostResponse::err(400, "bad name".into());
    }
    let project = project_for(s, sess);
    let file = plugins_dir(&project).join(&name);
    // the read-modify-write serializes on one lock — two racing PUTs
    // can't silently resurrect each other's pin
    static LEDGER_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _write = LEDGER_LOCK.lock_or_recover();
    let mut pins = pinned(&project);
    if v["enabled"].as_bool().unwrap_or(false) {
        let Ok(bytes) = std::fs::read(&file) else {
            return HostResponse::err(404, "no such plugin".into());
        };
        pins.insert(name, sha256_hex(&bytes));
    } else {
        pins.remove(&name);
    }
    let json = serde_json::json!({ "enabled": pins });
    // tempfile + rename — a crash mid-write leaves the last good ledger,
    // not a half-JSON that pins-nothing-but-also-loses-history
    let led = ledger_path(&project);
    let tmp = led.with_extension("json.tmp");
    if std::fs::write(
        &tmp,
        serde_json::to_string_pretty(&json).unwrap_or_default(),
    )
    .is_err()
        || std::fs::rename(&tmp, &led).is_err()
    {
        return HostResponse::err(500, "ledger write failed".into());
    }
    drop(_write);
    HostResponse::json(serde_json::json!({
        "dir": display_path(&plugins_dir(&project)),
        "plugins": catalog(&project),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_plugin_loads_only_while_enabled_and_untampered() {
        let dir = std::env::temp_dir().join(format!("sm-plg-{}", std::process::id()));
        let plug = dir.join(".sunmao").join("plugins");
        std::fs::create_dir_all(&plug).unwrap();
        std::fs::write(plug.join("a.js"), b"// v1").unwrap();
        // not pinned → absent from the load set, catalog says disabled
        assert!(pinned(&dir).is_empty());
        assert_eq!(catalog(&dir)[0]["enabled"], false);
        // pin it → enabled, matching
        let hash = sha256_hex(b"// v1");
        std::fs::write(
            ledger_path(&dir),
            format!(r#"{{"enabled":{{"a.js":"{hash}"}}}}"#),
        )
        .unwrap();
        assert_eq!(catalog(&dir)[0]["tampered"], false);
        // mutate after approval → tampered, loader refuses it
        std::fs::write(plug.join("a.js"), b"// v2").unwrap();
        assert_eq!(catalog(&dir)[0]["tampered"], true);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn plugin_names_cannot_traverse() {
        for bad in ["../x.js", "a/b.js", "a\\b.js", "..\\x.js", "x.txt", "."] {
            assert!(!valid_name(bad), "{bad}");
        }
        assert!(valid_name("pane.js"));
    }

    #[test]
    fn plugin_names_reject_streams_devices_and_empty_stems() {
        // ':' is an NTFS alternate-data-stream suffix; device stems hang a
        // blocking read; bare ".js" has no id; '"' would break the pane
        // attribute the frontend interpolates the stem into
        for bad in [
            "x:y.js", "con.js", "NUL.js", "aux.js", "com3.js", ".js", "my\"x.js", "my x.js",
        ] {
            assert!(!valid_name(bad), "{bad}");
        }
        assert!(valid_name("my-pane_2.js"));
        assert!(valid_name("a.b.js"));
    }
}
