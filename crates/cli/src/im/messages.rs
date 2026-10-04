//! Channel-facing reply templates — `assets/im-messages.txt` is the
//! cold-plug surface (rule 6); this is just the `key<TAB>text` lookup.

/// The paired template text for `key` — missing keys return "" rather
/// than panicking: a stale asset file must degrade the reply, not the
/// daemon.
pub(crate) fn get(key: &str) -> String {
    include_str!("../../assets/im-messages.txt")
        .lines()
        .filter(|l| !l.starts_with('#'))
        .find_map(|l| l.split_once('\t').filter(|(k, _)| *k == key))
        .map(|(_, t)| t.trim().to_string())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    #[test]
    fn every_template_present() {
        for k in [
            "pairing_offer",
            "pairing_cooldown",
            "pairing_pending",
            "pairing_row",
            "pairing_approved",
            "stopped",
            "stopped_idle",
            "redelivery_prefix",
            "status_line",
            "mode_fixed",
            "help",
        ] {
            assert!(!super::get(k).is_empty(), "missing template {k}");
        }
    }
}
