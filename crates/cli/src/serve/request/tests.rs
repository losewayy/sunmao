use super::session_meta;
use crate::serve::host::display_path;

#[test]
fn title_is_first_typed_prompt() {
    let dir = std::env::temp_dir().join(format!("sunmao-meta-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let p = dir.join("s-1.jsonl");
    let log = [
        r#"{"type":"started","model":"m","cwd":"x"}"#,
        r#"{"type":"message","message":{"role":"system","content":"identity"}}"#,
        r#"{"type":"message","message":{"role":"user","content":"[hook context] injected"}}"#,
        r#"{"type":"message","message":{"role":"user","content":"\n  fix the drag bug  \nsecond line"}}"#,
        r#"{"type":"message","message":{"role":"user","content":"later prompt"}}"#,
    ];
    std::fs::write(&p, log.join("\n")).unwrap();
    let m = session_meta(&p);
    assert_eq!(m["title"], "fix the drag bug");
    assert!(m["mtime"].as_u64().is_some());

    std::fs::write(&p, log[..3].join("\n")).unwrap();
    assert!(session_meta(&p)["title"].is_null());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn display_path_strips_verbatim_prefix() {
    let p = |s: &str| display_path(std::path::Path::new(s));
    assert_eq!(p(r"\\?\C:\work\x"), r"C:\work\x");
    assert_eq!(p(r"\\?\UNC\srv\share\x"), r"\\srv\share\x");
    assert_eq!(p("/home/u/x"), "/home/u/x");
}
