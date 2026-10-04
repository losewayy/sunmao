use super::skills::*;
use super::*;

fn scratch() -> PathBuf {
    let d = crate::fresh_test_dir("prompt");
    std::fs::create_dir_all(&d).unwrap();
    d
}

#[test]
fn builtin_sections_all_present_in_order() {
    let dir = scratch();
    // dialect section varies by backend — pin posix so the assertion
    // doesn't depend on whether this box has pwsh on PATH
    std::fs::create_dir_all(dir.join(".sunmao")).unwrap();
    std::fs::write(dir.join(".sunmao/shell.txt"), "posix").unwrap();
    let s = PromptAssembler::new(&dir).assemble(None);
    let id = s.find("You are sunmao").unwrap();
    let tg = s.find("Prefer dedicated tools").unwrap();
    let sd = s.find("deno_task_shell").unwrap();
    assert!(id < tg && tg < sd);
}

#[test]
fn complete_overrides_everything() {
    let s = PromptAssembler::new(scratch()).assemble(Some("OVERRIDE"));
    assert_eq!(s, "OVERRIDE");
}

#[test]
fn project_prompt_file_appends() {
    let dir = scratch();
    let sd = dir.join(".sunmao");
    std::fs::create_dir_all(&sd).unwrap();
    std::fs::write(sd.join("prompt.md"), "PROJECT RULE: be nice").unwrap();
    std::fs::write(sd.join("shell.txt"), "posix").unwrap();
    let s = PromptAssembler::new(&dir).assemble(None);
    assert!(s.contains("deno_task_shell"));
    assert!(s.contains("PROJECT RULE: be nice"));
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn prompt_d_replaces_named_section() {
    let dir = scratch();
    let pd = dir.join(".sunmao/prompt.d");
    std::fs::create_dir_all(&pd).unwrap();
    std::fs::write(pd.join("identity.md"), "CUSTOM IDENTITY").unwrap();
    let s = PromptAssembler::new(&dir).assemble(None);
    assert!(!s.contains("You are sunmao"));
    assert!(s.contains("CUSTOM IDENTITY"));
    // replacement keeps the original slot — identity still precedes guidance
    assert!(s.find("CUSTOM IDENTITY").unwrap() < s.find("Prefer dedicated").unwrap());
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn agents_md_lands_in_project_context() {
    let dir = scratch();
    std::fs::write(dir.join("AGENTS.md"), "work rules here").unwrap();
    let s = PromptAssembler::new(&dir).assemble(None);
    assert!(s.contains("## AGENTS.md"));
    assert!(s.contains("work rules here"));
    std::fs::remove_dir_all(&dir).ok();
}

/// project-context is a reserved stem — a prompt.d file must REPLACE the
/// dynamic section (AGENTS.md + skills index), not append before it.
#[test]
fn prompt_d_replaces_project_context() {
    let dir = scratch();
    std::fs::write(dir.join("AGENTS.md"), "work rules here").unwrap();
    let pd = dir.join(".sunmao/prompt.d");
    std::fs::create_dir_all(&pd).unwrap();
    std::fs::write(pd.join("project-context.md"), "CUSTOM CONTEXT").unwrap();
    let s = PromptAssembler::new(&dir).assemble(None);
    assert!(!s.contains("work rules here"), "{s}");
    assert!(s.contains("CUSTOM CONTEXT"));
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn subagent_default_and_replacement() {
    let a = PromptAssembler::new(scratch());
    assert!(a.assemble_subagent(None).contains("sunmao sub-agent"));

    let dir = scratch();
    let pd = dir.join(".sunmao/prompt.d");
    std::fs::create_dir_all(&pd).unwrap();
    std::fs::write(pd.join("subagent-default.md"), "CUSTOM SUBAGENT").unwrap();
    let s = PromptAssembler::new(&dir).assemble_subagent(None);
    assert_eq!(s, "CUSTOM SUBAGENT");
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn no_files_still_assembles() {
    let s = PromptAssembler::new(scratch()).assemble(None);
    assert!(s.contains("You are sunmao"));
}

#[test]
fn ptc_driver_swaps_tool_guidance_for_codemode_contract() {
    let dir = scratch();
    std::fs::create_dir_all(dir.join(".sunmao")).unwrap();
    std::fs::write(dir.join(".sunmao/shell.txt"), "posix").unwrap();
    let s = PromptAssembler::new(&dir)
        .with_driver(crate::agent::LoopDriver::Ptc)
        .assemble(None);
    assert!(s.contains("PTC (codemode)"), "{s}");
    // the default "prefer dedicated tools" prose must be gone — under ptc
    // the model can't emit those calls at all
    assert!(!s.contains("Prefer dedicated tools"), "{s}");
    // the RunCode API reference still lands
    assert!(s.contains("tools.<Name>(args)"), "{s}");
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn md_skill_indexes_from_frontmatter() {
    let dir = scratch();
    let sd = dir.join(".sunmao/skills/greeter");
    std::fs::create_dir_all(&sd).unwrap();
    std::fs::write(
        sd.join("SKILL.md"),
        "---\nname: greeter\ndescription: says hi\n---\nbody",
    )
    .unwrap();
    let s = PromptAssembler::new(&dir).assemble(None);
    assert!(s.contains("- greeter — says hi"));
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn html_skill_indexes_meta_and_resources() {
    let dir = scratch();
    let sd = dir.join(".sunmao/skills/report-card");
    std::fs::create_dir_all(&sd).unwrap();
    std::fs::write(
        sd.join("SKILL.html"),
        "<html><head><title>Report &amp; Card</title>\
         <meta name=\"description\" content=\"builds report pages\"></head>\
         <body></body></html>",
    )
    .unwrap();
    std::fs::write(sd.join("template.html"), "<html></html>").unwrap();
    std::fs::write(sd.join("notes.txt"), "not a resource").unwrap();
    let s = PromptAssembler::new(&dir).assemble(None);
    // <title> wins the name slot; &amp; unescapes; bundled .html counted
    assert!(s.contains("- Report & Card — builds report pages"));
    assert!(s.contains("SKILL.html +1 .html)"));
    assert!(!s.contains("notes.txt"));
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn skill_md_wins_over_html_in_same_dir() {
    let dir = scratch();
    let sd = dir.join(".sunmao/skills/bilingual");
    std::fs::create_dir_all(&sd).unwrap();
    std::fs::write(sd.join("SKILL.md"), "name: md-skill\ndescription: md wins").unwrap();
    std::fs::write(sd.join("SKILL.html"), "<title>html-loses</title>").unwrap();
    let s = PromptAssembler::new(&dir).assemble(None);
    assert!(s.contains("- md-skill — md wins"));
    assert!(!s.contains("html-loses"));
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn html_meta_attr_order_independent() {
    // content before name — real-world pages vary attribute order
    let (name, desc) = html_skill_meta(
        "<html><head><meta content=\"ordered differently\" name=\"description\">\
         <title>T</title></head></html>",
    );
    assert_eq!((name.as_str(), desc.as_str()), ("T", "ordered differently"));
}

#[test]
fn skills_index_is_path_sorted() {
    // prompt caching invariant: index order must not follow filesystem
    // enumeration — create z first so an unsorted scan would list it first
    let dir = scratch();
    for name in ["zed", "alpha"] {
        let sd = dir.join(format!(".sunmao/skills/{name}"));
        std::fs::create_dir_all(&sd).unwrap();
        std::fs::write(sd.join("SKILL.md"), format!("name: {name}\ndescription: d")).unwrap();
    }
    let s = PromptAssembler::new(&dir).assemble(None);
    let a = s.find("- alpha").unwrap();
    let z = s.find("- zed").unwrap();
    assert!(a < z, "skills index must sort by path");
    std::fs::remove_dir_all(&dir).ok();
}
