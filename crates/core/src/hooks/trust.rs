//! Hook trust pinning — the fail-closed side of the dispatcher.
//!
//! `HookEngine::load` ingests hook commands from every convention dir a
//! project might carry (`.sunmao/`, `.claude/`, `.codex/`, `.cursor/`,
//! plugin manifests, presets). Some of those files arrive with the project
//! itself — clone a repo, run `sunmao`, and a stranger's `SessionStart`
//! command executes before the first prompt. Trust pinning closes that:
//! project- and plugin-layer commands execute only after the user pins
//! them in `<cwd>/.sunmao/trusted-hooks.json` (`/hooks trust <n>`);
//! unpinned commands are skipped at plan time and logged as
//! `hook.untrusted` audit facts. User-level files (`~/.claude`,
//! `~/.codex`, `~/.cursor`) are implicitly trusted — the user wrote them.
//!
//! The ledger keys on `sha256(canonical source path + "\n" + command)`:
//! editing the command or moving the file invalidates the pin, and two
//! sources carrying the identical command each need their own approval.
//! No managed/admin layering — single-user harness (SPEC §1).

use std::path::{Path, PathBuf};

/// Which layer a hook source belongs to — decides the default trust.
/// `Project` is the serde-skip default: an origin we forgot to tag is
/// untrusted, never accidentally implicit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum Layer {
    /// Files under the project dir (.sunmao/.claude/.codex/.cursor,
    /// plugin bundles, presets) — untrusted until pinned.
    #[default]
    Project,
    /// Files under the user home (~/.claude, ~/.codex, ~/.cursor) —
    /// implicitly trusted; the user owns that layer.
    User,
}

/// One roster row for `/hooks` — command text is shown verbatim so the
/// review is of the real bytes that would execute, not a summary.
#[derive(Debug)]
pub struct HookRow {
    pub event: String,
    pub matcher: String,
    pub command: String,
    /// The file this command was loaded from (canonicalized).
    pub source: PathBuf,
    /// `user` = implicit trust, `pinned` = ledger hit, `untrusted` = skipped.
    pub status: &'static str,
    /// Trust-pin key (sha256 hex) — the ledger's identity for this row.
    pub digest: String,
}

/// The ledger path — same `.sunmao/` bucket as every other per-project
/// runtime file. Format: `{"trusted": {"<sha256>": {"source", "command"}}}`.
/// Missing/invalid file = nothing trusted (fail-closed by default).
pub(crate) fn ledger_path(cwd: &Path) -> PathBuf {
    cwd.join(".sunmao").join("trusted-hooks.json")
}

/// Trust-pin key for a `(source file, command)` pair. The source is
/// canonicalized first so `--cwd .` and `--cwd C:\proj` pin the same row;
/// `\\?\` verbatim prefixes are stripped like `expand_plugin_root` does.
pub(crate) fn digest(source: &Path, command: &str) -> String {
    let canon = source
        .canonicalize()
        .unwrap_or_else(|_| source.to_path_buf());
    let canon = canon.display().to_string().replace("\\\\?\\", "");
    sha256(&format!("{canon}\n{command}"))
}

/// The set of pinned digests — re-read per call: the file is a few KB and
/// a write (`/hooks trust`) must take effect for every engine that shares
/// the project (serve hosts one Context per tab). Statelessness beats a
/// cache the size of the consistency bug.
fn trusted_set(cwd: &Path) -> std::collections::HashSet<String> {
    std::fs::read_to_string(ledger_path(cwd))
        .ok()
        .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok())
        .and_then(|v| v.get("trusted").and_then(|t| t.as_object().cloned()))
        .map(|m| m.keys().cloned().collect())
        .unwrap_or_default()
}

/// Is this command allowed to plan? User layer is implicit; everything
/// else must appear in the ledger.
pub(crate) fn is_trusted(cwd: &Path, layer: Layer, source: &Path, command: &str) -> bool {
    if layer == Layer::User {
        return true;
    }
    trusted_set(cwd).contains(&digest(source, command))
}

/// Write or clear a pin. `set=true` records `{source, command}` under the
/// digest (the record is review context, not a lookup path); `set=false`
/// removes the key, and an empty `trusted` map removes the file so
/// CONFIG.md's "missing file = feature off" stays literal.
pub(crate) fn set_pin(cwd: &Path, source: &Path, command: &str, set: bool) -> Result<(), String> {
    let path = ledger_path(cwd);
    let mut doc: serde_json::Value = std::fs::read_to_string(&path)
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_else(|| serde_json::json!({"trusted": {}}));
    let map = doc
        .as_object_mut()
        .and_then(|o| o.get_mut("trusted"))
        .and_then(|t| t.as_object_mut())
        .ok_or_else(|| "trusted-hooks.json: 'trusted' is not an object".to_string())?;
    let key = digest(source, command);
    if set {
        map.insert(
            key,
            serde_json::json!({
                "source": source.display().to_string(),
                "command": command,
            }),
        );
    } else {
        map.remove(&key);
    }
    let empty = doc["trusted"]
        .as_object()
        .map(|m| m.is_empty())
        .unwrap_or(true);
    if empty {
        match std::fs::remove_file(&path) {
            Ok(()) => return Ok(()),
            // absent already = the desired end state
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(format!("remove {}: {e}", path.display())),
        }
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("create {}: {e}", parent.display()))?;
    }
    let out = serde_json::to_string_pretty(&doc).map_err(|e| e.to_string())?;
    std::fs::write(&path, out).map_err(|e| format!("write {}: {e}", path.display()))
}

/// sha256 hex — hand-rolled (the dependency budget asks whether a crate
/// saves more than it costs; a hash is ~60 lines).
fn sha256(data: &str) -> String {
    const K: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4,
        0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe,
        0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f,
        0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7,
        0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc,
        0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b,
        0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116,
        0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
        0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7,
        0xc67178f2,
    ];
    let mut h: [u32; 8] = [
        0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
        0x5be0cd19,
    ];
    let mut m = data.as_bytes().to_vec();
    let bits = (m.len() as u64) * 8;
    m.push(0x80);
    while m.len() % 64 != 56 {
        m.push(0);
    }
    m.extend_from_slice(&bits.to_be_bytes());
    for chunk in m.as_chunks::<64>().0 {
        let mut w = [0u32; 64];
        for (i, b) in chunk.as_chunks::<4>().0.iter().enumerate() {
            w[i] = u32::from_be_bytes(*b);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16]
                .wrapping_add(s0)
                .wrapping_add(w[i - 7])
                .wrapping_add(s1);
        }
        let (mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut hh) =
            (h[0], h[1], h[2], h[3], h[4], h[5], h[6], h[7]);
        for i in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ (!e & g);
            let t1 = hh
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(K[i])
                .wrapping_add(w[i]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let t2 = s0.wrapping_add(maj);
            hh = g;
            g = f;
            f = e;
            e = d.wrapping_add(t1);
            d = c;
            c = b;
            b = a;
            a = t1.wrapping_add(t2);
        }
        h[0] = h[0].wrapping_add(a);
        h[1] = h[1].wrapping_add(b);
        h[2] = h[2].wrapping_add(c);
        h[3] = h[3].wrapping_add(d);
        h[4] = h[4].wrapping_add(e);
        h[5] = h[5].wrapping_add(f);
        h[6] = h[6].wrapping_add(g);
        h[7] = h[7].wrapping_add(hh);
    }
    h.iter().map(|w| format!("{w:08x}")).collect()
}

/// Roster shape `serve`'s REST read face serializes — kept next to
/// `HookRow` so the two can't drift apart.
pub fn row_json(r: &HookRow) -> serde_json::Value {
    serde_json::json!({
        "event": r.event,
        "matcher": r.matcher,
        "command": r.command,
        "source": r.source.display().to_string().replace("\\\\?\\", ""),
        "status": r.status,
        "digest": r.digest,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sha256_matches_known_vectors() {
        assert_eq!(
            sha256(""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            sha256("abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        // multi-block input exercises the length padding path
        let long = "a".repeat(200);
        assert_eq!(
            sha256(&long),
            "c2a908d98f5df987ade41b5fce213067efbcc21ef2240212a41e54b5e7c28ae5"
        );
    }

    #[test]
    fn pin_roundtrip_and_edit_invalidation() {
        let dir = crate::fresh_test_dir("trust");
        let src = dir.join(".sunmao/hooks.json");
        std::fs::create_dir_all(src.parent().unwrap()).unwrap();
        std::fs::write(&src, "{}").unwrap(); // nothing pinned → untrusted; pin → trusted
        assert!(!is_trusted(&dir, Layer::Project, &src, "echo hi"));
        set_pin(&dir, &src, "echo hi", true).unwrap();
        assert!(is_trusted(&dir, Layer::Project, &src, "echo hi"));
        // a different command at the same source is NOT covered
        assert!(!is_trusted(&dir, Layer::Project, &src, "echo bye"));
        // user layer never needs a pin
        assert!(is_trusted(&dir, Layer::User, &src, "echo bye"));
        // revoke → back to untrusted; empty ledger removes the file
        set_pin(&dir, &src, "echo hi", false).unwrap();
        assert!(!is_trusted(&dir, Layer::Project, &src, "echo hi"));
        assert!(!ledger_path(&dir).exists());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn ledger_keeps_human_readable_context() {
        let dir = crate::fresh_test_dir("trust-doc");
        std::fs::create_dir_all(&dir).unwrap();
        let src = dir.join("hooks.json");
        std::fs::write(&src, "{}").unwrap();
        set_pin(&dir, &src, "echo ok", true).unwrap();
        let v: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(ledger_path(&dir)).unwrap()).unwrap();
        let entry = v["trusted"].as_object().unwrap().values().next().unwrap();
        assert_eq!(entry["command"], "echo ok");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// The digest key IS the sha256 of canonical-source+command — external
    /// tools (and this same code on a different path spelling) must be able
    /// to recompute it.
    #[test]
    fn digest_is_recomputable() {
        let dir = crate::fresh_test_dir("trust-digest");
        std::fs::create_dir_all(&dir).unwrap();
        let src = dir.join("h.json");
        std::fs::write(&src, "{}").unwrap();
        let d = digest(&src, "x");
        assert_eq!(d.len(), 64);
        assert_eq!(d, digest(&src, "x"));
        assert_ne!(d, digest(&src, "y"));
        std::fs::remove_dir_all(&dir).ok();
    }
}
