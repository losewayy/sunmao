//! Prompt assembly — the system prompt is data, not a constant.
//!
//! One assembler, one layering order, every frontend (REPL / TUI / ACP) and
//! the Task sub-agent shares it — the prompt stops drifting per frontend.
//!
//! Sections are named files, concatenated in a fixed order:
//!
//! | order | section | source |
//! |---|---|---|
//! | 10..30 | `identity`, `tool-guidance`, `shell-dialect` | `assets/prompt/*.md` baked into the binary |
//! | 40 | `prompt.md` + `prompt.d/*.md` | `~/.sunmao/` |
//! | 50 | `prompt.md` + `prompt.d/*.md` | `<cwd>/.sunmao/` |
//! | 60 | `project-context` | `AGENTS.md` / `CLAUDE.md` + the skills index (dynamic) |
//!
//! A later file whose name matches an earlier section **replaces** it in
//! place — `~/.sunmao/prompt.d/identity.md` swaps the identity section
//! without a rebuild. That is the cold-plug rule: every prompt segment can
//! be swapped at the file layer; nothing is hot-reloaded mid-session.
//!
//! `--system` is a *complete* section (dsh's `complete` analog): when set it
//! replaces the whole assembly — the one thing that outranks files.

use std::path::{Path, PathBuf};

/// One named prompt section. `order` decides concatenation order; `name` is
/// the replacement key for same-named files in later layers.
struct Section {
    name: String,
    order: u32,
    text: String,
}

/// Section names users can target from `prompt.d/`.
pub mod names {
    pub const IDENTITY: &str = "identity";
    pub const TOOL_GUIDANCE: &str = "tool-guidance";
    pub const SHELL_DIALECT: &str = "shell-dialect";
    pub const SUBAGENT_DEFAULT: &str = "subagent-default";
    pub const COMPACT: &str = "compact";
    /// The goal-chain continuation prompt — one section, layered like
    /// `compact` (user `prompt.d/goal-continue.md` replaces it).
    pub const GOAL_CONTINUE: &str = "goal-continue";
    /// `RunCode` usage semantics — the tool's own decl covers the wire
    /// contract; this section carries the "when/why" for the model.
    pub const PTC: &str = "ptc";
    pub const PROJECT_CONTEXT: &str = "project-context";
}

/// Assembles prompts for one working directory.
pub struct PromptAssembler {
    cwd: PathBuf,
    /// Preset plugin roots — their `skills/` subdirs join the skills index
    /// and their `agents/` defs are resolvable by `assemble_subagent`.
    extra_roots: Vec<PathBuf>,
    /// The shell dialect the `shell-dialect` section should describe —
    /// resolved the same way `Context::new` does, so sub-agent prompts
    /// (built without a Context) still match the session's backend.
    shell: crate::tool::ShellBackend,
    /// The session's loop driver — under `ptc` the tool-guidance slot
    /// carries the codemode contract (`RunCode` is the only callable tool)
    /// instead of the default call-per-tool guidance.
    driver: crate::agent::LoopDriver,
}

impl PromptAssembler {
    pub fn new(cwd: impl Into<PathBuf>) -> Self {
        let cwd = cwd.into();
        Self {
            shell: crate::tool::ShellBackend::resolve(&cwd),
            cwd,
            extra_roots: Vec::new(),
            driver: crate::agent::LoopDriver::default(),
        }
    }

    /// Enabled preset dirs — scanned last by every consumer this assembler
    /// delegates to (skills index, agent defs).
    pub fn with_extra_roots(mut self, roots: &[PathBuf]) -> Self {
        self.extra_roots = roots.to_vec();
        self
    }

    /// The loop driver this prompt is assembled for — pass the Context's
    /// resolved driver so the guidance matches the advertised surface.
    pub fn with_driver(mut self, driver: crate::agent::LoopDriver) -> Self {
        self.driver = driver;
        self
    }

    /// The session system prompt. `complete` (`--system`) wins outright.
    pub fn assemble(&self, complete: Option<&str>) -> String {
        if let Some(full) = complete {
            return full.to_string();
        }
        let mut sections = builtin_sections(self.shell, self.driver);
        apply_layer(&mut sections, &user_layer_dir(), 40);
        apply_layer(&mut sections, &self.cwd.join(".sunmao"), 50);
        let ctx = project_context(&self.cwd, &self.extra_roots);
        if !ctx.trim().is_empty() {
            sections.push(Section {
                name: names::PROJECT_CONTEXT.into(),
                order: 60,
                text: ctx,
            });
        }
        render(sections)
    }

    /// The Task sub-agent's prompt: a named `agents/*.md` def wins; otherwise
    /// the `subagent-default` section — itself replaceable from `prompt.d/`.
    pub fn assemble_subagent(&self, subagent_type: Option<&str>) -> String {
        if let Some(def) = subagent_type.and_then(|t| {
            crate::agents::load_all(&self.cwd, &self.extra_roots)
                .into_iter()
                .find(|d| d.name == t)
        }) {
            return def.system_prompt;
        }
        self.section_or(names::SUBAGENT_DEFAULT, || {
            include_str!("../assets/prompt/subagent-default.md")
                .trim()
                .to_string()
        })
    }

    /// The summarization instruction fed to the model during compaction —
    /// an asset file (`assets/prompt/compact.md`), replaceable via
    /// `prompt.d/compact.md` like every other section.
    pub fn assemble_compact(&self) -> String {
        self.section_or(names::COMPACT, || {
            include_str!("../assets/prompt/compact.md")
                .trim()
                .to_string()
        })
    }

    /// The goal-chain continuation prompt's template — an asset file
    /// (`assets/prompt/goal-continue.md`) replaceable via
    /// `prompt.d/goal-continue.md`. Placeholders `{objective}`, `{round}`
    /// and `{max}` are filled by `goal_continue_prompt`.
    pub fn assemble_goal_continue(&self) -> String {
        self.section_or(names::GOAL_CONTINUE, || {
            include_str!("../assets/prompt/goal-continue.md")
                .trim()
                .to_string()
        })
    }

    /// `prompt.md`/`prompt.d` layering applied to a single-section list —
    /// user layer then project layer; a same-named file replaces the baked
    /// text in place.
    fn section_or(&self, name: &str, default: impl FnOnce() -> String) -> String {
        let mut sections = vec![Section {
            name: name.into(),
            order: 0,
            text: default(),
        }];
        apply_layer(&mut sections, &user_layer_dir(), 40);
        apply_layer(&mut sections, &self.cwd.join(".sunmao"), 50);
        sections
            .into_iter()
            .find(|s| s.name == name)
            .map(|s| s.text)
            .unwrap_or_default()
    }
}

/// Kernel-owned prompt text lives in `assets/prompt/` — files, not string
/// literals. Baked in via `include_str!` so the binary stays self-contained.
/// `shell` picks the dialect section — the model gets told which grammar
/// `Bash` actually speaks, not whichever dialect the build defaults to.
/// `driver` picks the tool-guidance section: `ptc` advertises RunCode as
/// the whole surface, so the default "prefer dedicated tools" guidance
/// would describe a call shape the model can't emit.
fn builtin_sections(
    shell: crate::tool::ShellBackend,
    driver: crate::agent::LoopDriver,
) -> Vec<Section> {
    let mk = |name: &str, order: u32, text: &str| Section {
        name: name.into(),
        order,
        text: text.trim().to_string(),
    };
    let dialect = match shell {
        crate::tool::ShellBackend::Posix => {
            include_str!("../assets/prompt/shell-dialect.md")
        }
        crate::tool::ShellBackend::Pwsh => {
            include_str!("../assets/prompt/shell-dialect-pwsh.md")
        }
    };
    // same section slot either way: a prompt.d/tool-guidance.md override
    // replaces the driver's default too — cold-plug beats the driver pick.
    let guidance = match driver {
        crate::agent::LoopDriver::Ptc => include_str!("../assets/prompt/ptc-driver.md"),
        _ => include_str!("../assets/prompt/tool-guidance.md"),
    };
    vec![
        mk(
            names::IDENTITY,
            10,
            include_str!("../assets/prompt/identity.md"),
        ),
        mk(names::TOOL_GUIDANCE, 20, guidance),
        mk(names::SHELL_DIALECT, 30, dialect),
        mk(names::PTC, 35, include_str!("../assets/prompt/ptc.md")),
    ]
}

/// `prompt.d` stems that shadow kernel-owned sections. A same-stem file
/// *replaces* the baked text in place (the designed override) — the trap is
/// silence: a file the user dropped under an innocent name turns into a
/// builtin replacement the moment a new section claims that stem. `doctor`
/// surfaces the collision; CONFIG.md names the reserved set.
pub const RESERVED_STEMS: &[&str] = &[
    names::IDENTITY,
    names::TOOL_GUIDANCE,
    names::SHELL_DIALECT,
    names::SUBAGENT_DEFAULT,
    names::COMPACT,
    names::GOAL_CONTINUE,
    names::PTC,
    names::PROJECT_CONTEXT,
];

/// `prompt.d/*.md` files in `dir` whose stem shadows a kernel section —
/// `(stem, path)` pairs for `doctor` to report. Empty dir / unreadable dir
/// reports nothing (same posture as `apply_layer`).
pub fn shadowed_builtins(dir: &Path) -> Vec<(String, PathBuf)> {
    let mut out = Vec::new();
    if let Ok(entries) = std::fs::read_dir(dir.join("prompt.d")) {
        for p in entries.flatten().map(|e| e.path()) {
            if !p.extension().map(|x| x == "md").unwrap_or(false) {
                continue;
            }
            let stem = p
                .file_stem()
                .unwrap_or_default()
                .to_string_lossy()
                .to_string();
            if RESERVED_STEMS.contains(&stem.as_str()) {
                out.push((stem, p));
            }
        }
    }
    out
}

/// `~/.sunmao/` — the user-level layer.
pub fn user_layer_dir() -> PathBuf {
    std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(PathBuf::from)
        .unwrap_or_default()
        .join(".sunmao")
}

/// Fold one directory layer into the section list: `prompt.md` (named
/// `prompt`) plus every `prompt.d/*.md` in filename order. A file whose stem
/// matches an existing section replaces that section's text in place;
/// anything else appends at this layer's order.
fn apply_layer(sections: &mut Vec<Section>, dir: &Path, order: u32) {
    if dir.as_os_str().is_empty() {
        return;
    }
    let mut files: Vec<PathBuf> = Vec::new();
    let prompt_md = dir.join("prompt.md");
    if prompt_md.is_file() {
        files.push(prompt_md);
    }
    if let Ok(entries) = std::fs::read_dir(dir.join("prompt.d")) {
        let mut extra: Vec<PathBuf> = entries
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().map(|x| x == "md").unwrap_or(false))
            .collect();
        extra.sort();
        files.extend(extra);
    }
    for path in files {
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        let text = text.trim().to_string();
        if text.is_empty() {
            continue;
        }
        let name = path
            .file_stem()
            .unwrap_or_default()
            .to_string_lossy()
            .to_string();
        match sections.iter_mut().find(|s| s.name == name) {
            Some(slot) => slot.text = text,
            None => sections.push(Section { name, order, text }),
        }
    }
}

fn render(mut sections: Vec<Section>) -> String {
    sections.retain(|s| !s.text.trim().is_empty());
    sections.sort_by_key(|s| s.order);
    sections
        .into_iter()
        .map(|s| s.text)
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// Project-level context: `AGENTS.md` / `CLAUDE.md` bodies plus the skills
/// index (name + description from SKILL.md frontmatter — bodies are Read on
/// demand). Moved here from the REPL so every frontend gets it.
fn project_context(cwd: &Path, extra_roots: &[PathBuf]) -> String {
    let mut out = String::new();
    for name in ["AGENTS.md", "CLAUDE.md"] {
        let p = cwd.join(name);
        if let Ok(text) = std::fs::read_to_string(&p) {
            let text: String = text.chars().take(8_000).collect();
            out.push_str(&format!("## {name}\n{text}\n\n"));
        }
    }
    let mut skills_dirs = vec![
        cwd.join(".sunmao").join("skills"),
        cwd.join(".claude").join("skills"),
        cwd.join(".sunmao").join("plugin").join("skills"),
    ];
    // plugin bundles: .sunmao/plugins/<name>/skills/, .claude/plugins/<name>/skills/
    for base in [
        cwd.join(".sunmao").join("plugins"),
        cwd.join(".claude").join("plugins"),
    ] {
        for p in crate::sorted_entries(&base) {
            skills_dirs.push(p.path().join("skills"));
        }
    }
    // ecosystem scan — skills authored for other harnesses load unmodified
    if let Some(home) = std::env::var_os("USERPROFILE").or_else(|| std::env::var_os("HOME")) {
        let h = std::path::Path::new(&home);
        skills_dirs.push(h.join(".claude").join("skills"));
        skills_dirs.push(h.join(".agents").join("skills"));
    }
    // preset bundles contribute skills the same way, layered last
    for root in extra_roots {
        skills_dirs.push(root.join("skills"));
    }
    let mut lines = Vec::new();
    for skills_dir in skills_dirs {
        for e in crate::sorted_entries(&skills_dir) {
            // a skill dir may speak either doc language — SKILL.md (the
            // ecosystem contract) or SKILL.html (the first-class payload:
            // a page for the human, structured text for the agent)
            let mut skill = e.path().join("SKILL.md");
            let is_html_skill = if skill.exists() {
                false
            } else {
                let html = e.path().join("SKILL.html");
                if html.exists() {
                    skill = html;
                    true
                } else {
                    continue;
                }
            };
            if let Ok(text) = std::fs::read_to_string(&skill) {
                let (name, desc) = if is_html_skill {
                    html_skill_meta(&text)
                } else {
                    md_skill_meta(&text, &e.file_name().to_string_lossy())
                };
                // bundled .html resources surface in the index — a template
                // the agent can copy/Read is discoverable, not invisible
                let resources: Vec<String> = crate::sorted_entries(&e.path())
                    .into_iter()
                    .filter(|f| {
                        let s = f.file_name().to_string_lossy().to_string();
                        s.ends_with(".html") && s != "SKILL.html"
                    })
                    .map(|f| f.file_name().to_string_lossy().to_string())
                    .collect();
                let res_note = if resources.is_empty() {
                    String::new()
                } else {
                    format!(" +{} .html", resources.len())
                };
                lines.push(format!(
                    "- {} — {} ({}{})",
                    name,
                    desc,
                    skill.display().to_string().replace("\\\\?\\", ""),
                    res_note
                ));
            }
        }
    }
    if !lines.is_empty() {
        out.push_str("## Available skills (Read the SKILL file path to load)\n");
        for l in &lines {
            out.push_str(l);
            out.push('\n');
        }
    }
    out
}

/// `SKILL.md` frontmatter parse: first 20 lines for `name:`/`description:`,
/// dir name as the fallback.
fn md_skill_meta(text: &str, fallback: &str) -> (String, String) {
    let mut name = fallback.to_string();
    let mut desc = String::new();
    for line in text.lines().take(20) {
        if let Some(v) = line.strip_prefix("name:") {
            name = v.trim().to_string();
        }
        if let Some(v) = line.strip_prefix("description:") {
            desc = v.trim().to_string();
        }
    }
    (name, desc)
}

/// `SKILL.html` meta — the page already carries its identity: `<title>`
/// is the name, `<meta name="description">` the description. Falls back
/// to the first <h1> for the name; description may be empty.
fn html_skill_meta(text: &str) -> (String, String) {
    let tag_text = |open: &str, close: &str| -> Option<String> {
        let start = text.find(open)? + open.len();
        let end = text[start..].find(close)? + start;
        Some(html_unescape(text[start..end].trim()))
    };
    let name = tag_text("<title>", "</title>")
        .or_else(|| tag_text("<h1>", "</h1>"))
        .unwrap_or_default();
    let desc = html_meta_description(text).unwrap_or_default();
    (name, desc)
}

/// Scan `<meta>` tags for `name="description"` and read its `content`
/// attribute — attribute order varies in the wild.
fn html_meta_description(text: &str) -> Option<String> {
    let mut rest = text;
    while let Some(i) = rest.find("<meta") {
        let tail = &rest[i..];
        let end = tail.find('>')? + 1;
        let tag = &tail[..end];
        if tag.contains("name=\"description\"") || tag.contains("name='description'") {
            return attr_value(tag, "content").map(|v| html_unescape(v.trim()));
        }
        rest = &tail[end..];
    }
    None
}

/// `attr="..."` or `attr='...'` inside a tag — returns the quoted body.
fn attr_value<'a>(tag: &'a str, attr: &str) -> Option<&'a str> {
    for pat in [format!("{attr}=\""), format!("{attr}='")] {
        if let Some(i) = tag.find(&pat) {
            let s = i + pat.len();
            let q = tag.as_bytes()[s - 1] as char;
            return tag[s..].find(q).map(|e| &tag[s..s + e]);
        }
    }
    None
}

/// The tiny escape set HTML skill titles realistically use — entities we
/// can fix without a parser; unknown entities pass through untouched.
fn html_unescape(s: &str) -> String {
    s.replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
}

/// One goal-chain round's kick prompt — the layered `goal-continue`
/// template with `{objective}` / `{round}` / `{max}` filled in. The text
/// lands as a real user-role message (it IS a turn), so substitutions are
/// plain replaces — a `{round}` left over in a custom template is the
/// author's literal text, not an error.
pub fn goal_continue_prompt(cwd: &Path, objective: &str, round: u32, max: u32) -> String {
    PromptAssembler::new(cwd)
        .assemble_goal_continue()
        .replace("{objective}", objective)
        .replace("{round}", &round.to_string())
        .replace("{max}", &max.to_string())
}

#[cfg(test)]
mod tests;
