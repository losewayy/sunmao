//! Path stringification — ONE convergent spell for turning a `Path` into a
//! display/pattern string. Windows `canonicalize`/`current_dir` hand back
//! `\\?\`-prefixed verbatim paths; splicing that spelling into glob patterns
//! or UI strings is how "relative patterns silently match nothing" happens.
//! Every `Path → String` at a seam (tool patterns, UI labels, logs) funnels
//! through here.

/// Display form: `\\?\C:\…` → `C:\…`, `\\?\UNC\server\…` → `\\server\…`.
/// Non-verbatim paths pass through untouched.
pub fn display_path(p: &std::path::Path) -> String {
    let s = p.display().to_string();
    if let Some(rest) = s.strip_prefix(r"\\?\UNC\") {
        format!(r"\\{rest}")
    } else if let Some(rest) = s.strip_prefix(r"\\?\") {
        rest.to_string()
    } else {
        s
    }
}

/// Forward-slash display form for pattern/query contexts (glob matchers,
/// JSON payloads) where `\` would be an escape char.
pub fn display_path_fwd(p: &std::path::Path) -> String {
    display_path(p).replace('\\', "/")
}
