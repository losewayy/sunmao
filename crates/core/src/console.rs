//! Decoding a spawned process's piped output.
//!
//! Console tools on Windows emit bytes in the OEM codepage (`GetOEMCP()`) —
//! GBK (936) on a zh-CN box, not UTF-8 — so a blind `String::from_utf8_lossy`
//! turns `ping`/`ipconfig` output into mojibake and the model reads garbage.
//! `console_text` is the three-way decode every pipe-drain site shares:
//! strict UTF-8 first, then the OEM codepage on Windows, lossy last.

/// Decode stdout/stderr bytes captured from a spawned process.
///
/// - valid UTF-8 → as-is (Rust/modern tools, `chcp 65001` consoles)
/// - Windows, non-UTF-8 OEM codepage → `MultiByteToWideChar` (permits
///   malformed sequences, mapping them to the codepage's default char)
/// - anything left → `from_utf8_lossy` — never fails, never lies about
///   bytes it couldn't understand
pub fn console_text(bytes: &[u8]) -> String {
    if let Ok(s) = std::str::from_utf8(bytes) {
        return s.to_string();
    }
    #[cfg(windows)]
    {
        let cp = oem_codepage();
        if cp != 0
            && cp != windows_sys::Win32::Globalization::CP_UTF8
            && let Some(s) = decode_with_codepage(cp, bytes)
        {
            return s;
        }
    }
    String::from_utf8_lossy(bytes).into_owned()
}

/// The system OEM codepage (`GetOEMCP`) — what a console program that never
/// called `SetConsoleOutputCP` writes in. 0 means "don't know", no decode.
#[cfg(windows)]
fn oem_codepage() -> u32 {
    // SAFETY: GetOEMCP takes no arguments and cannot fail.
    unsafe { windows_sys::Win32::Globalization::GetOEMCP() }
}

/// `MultiByteToWideChar` with `flags = 0` — permissive: invalid sequences
/// map to the codepage's default char ('?'), which is the OEM-side mirror of
/// lossy decoding rather than a reason to reject the whole buffer.
#[cfg(windows)]
fn decode_with_codepage(cp: u32, bytes: &[u8]) -> Option<String> {
    use windows_sys::Win32::Globalization::MultiByteToWideChar;
    if bytes.is_empty() {
        return Some(String::new());
    }
    // SAFETY: null output buffer queries the required length; the second
    // call writes into a buffer of exactly that size.
    unsafe {
        let len = MultiByteToWideChar(
            cp,
            0,
            bytes.as_ptr(),
            bytes.len() as i32,
            std::ptr::null_mut(),
            0,
        );
        if len <= 0 {
            return None;
        }
        let mut wide = vec![0u16; len as usize];
        let wrote = MultiByteToWideChar(
            cp,
            0,
            bytes.as_ptr(),
            bytes.len() as i32,
            wide.as_mut_ptr(),
            len,
        );
        if wrote <= 0 {
            return None;
        }
        wide.truncate(wrote as usize);
        Some(String::from_utf16_lossy(&wide))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utf8_passes_through() {
        assert_eq!(console_text("hello 世界".as_bytes()), "hello 世界");
        assert_eq!(console_text(b""), "");
    }

    /// "中文" in GBK (cp936): strictly invalid UTF-8, so on a 936/GB18030
    /// OEM box the codepage arm must decode it; on UTF-8/non-Windows
    /// machines lossy is the honest floor. Either way it must not panic.
    #[test]
    fn gbk_bytes_decode_or_degrade() {
        let gbk = [0xD6u8, 0xD0, 0xCE, 0xC4];
        let s = console_text(&gbk);
        #[cfg(windows)]
        {
            let cp = oem_codepage();
            if cp == 936 || cp == 54936 {
                assert_eq!(s, "中文", "OEM cp{cp} should decode GBK");
            } else if let Some(expected) = decode_with_codepage(cp, &gbk) {
                // a single-byte OEM cp (437 on en-US runners, 850…) maps
                // every byte — the honest decode is that cp's glyphs,
                // not replacement chars
                assert_eq!(s, expected, "cp{cp} should drive the decode");
            } else {
                assert!(s.contains('\u{FFFD}'), "cp{cp}: {s:?}");
            }
        }
        #[cfg(not(windows))]
        assert!(s.contains('\u{FFFD}'), "{s:?}");
    }

    /// The OEM arm itself, exercised deterministically: cp936 bytes for
    /// "中文" decode even on a box whose OEM codepage is UTF-8 — the test
    /// targets the primitive, not the ambient machine state.
    #[cfg(windows)]
    #[test]
    fn codepage936_decodes_gbk() {
        let gbk = [0xD6u8, 0xD0, 0xCE, 0xC4];
        assert_eq!(decode_with_codepage(936, &gbk).as_deref(), Some("中文"));
    }

    /// Bytes invalid under every codepage still return *something* rather
    /// than panicking — lossy is the floor, not an error path.
    #[test]
    fn garbage_bytes_degrade_lossy() {
        let _ = console_text(&[0xFF, 0xFE, 0x00, 0xFF]);
    }
}
