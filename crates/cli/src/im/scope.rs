//! Per-credential namespacing for adapter cursors. A cursor is only
//! meaningful for the credential that was issued it: Telegram's `getUpdates`
//! offset and WeChat's `get_updates_buf` are positions in one bot's update
//! stream, and a WeChat reply window belongs to the token that opened it.
//! Replaying one under a different credential either drops messages the new
//! bot never saw or hands the platform a cursor it rejects, so every cursor
//! key carries a tag derived from its credential.
//!
//! The tag is a `DefaultHasher` digest, deliberately: this is namespacing,
//! not authentication. `DefaultHasher` is not a cryptographic hash (it is
//! trivially invertible for a guessable token), and std does not promise its
//! algorithm across releases, so a Rust upgrade may orphan a stored cursor —
//! the cost of that is one replayed or skipped window, never a wrong reply.
//! The credential itself is what stays out of the store.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash as _, Hasher as _};

/// A short, stable tag for a credential — eight hex digits, the reference's
/// `hash8` shape. `DefaultHasher::new()` uses fixed keys (unlike
/// `RandomState`), so the tag survives a restart.
pub(crate) fn credential_tag(secret: &str) -> String {
    let mut hasher = DefaultHasher::new();
    secret.hash(&mut hasher);
    format!("{:08x}", hasher.finish() as u32)
}

/// A store key namespaced to one credential: `{prefix}:{tag}`.
pub(crate) fn scoped(prefix: &str, secret: &str) -> String {
    format!("{prefix}:{}", credential_tag(secret))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_tag_separates_credentials_and_is_stable() {
        assert_eq!(credential_tag("tok-a"), credential_tag("tok-a"));
        assert_ne!(credential_tag("tok-a"), credential_tag("tok-b"));
        assert_eq!(credential_tag("tok-a").len(), 8);
        assert!(
            credential_tag("tok-a")
                .chars()
                .all(|c| c.is_ascii_hexdigit())
        );
        assert_eq!(
            scoped("tg:offset", "tok-a"),
            "tg:offset:".to_string() + &credential_tag("tok-a")
        );
        // the secret is never part of the key
        assert!(!scoped("wx:cursor", "tok-a").contains("tok-a"));
    }
}
