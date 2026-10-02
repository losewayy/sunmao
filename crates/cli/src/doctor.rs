//! `sunmao doctor` — environment self-check. Validates the seams the kernel
//! depends on before a session starts: provider reachability, rg availability,
//! session dir writability, MCP server connectivity.

pub async fn run(cli: &crate::Cli) -> anyhow::Result<()> {
    let mut ok = true;

    // 1. provider reachability — one cheap non-streaming call
    print!("provider {} ({}) ... ", cli.provider, cli.base_url);
    let llm: std::sync::Arc<dyn sunmao_llm::ProviderAdapter> = match cli.provider.as_str() {
        "anthropic" => std::sync::Arc::new(sunmao_llm::AnthropicClient::new(
            &cli.base_url,
            &cli.api_key,
            &cli.model,
        )),
        _ => std::sync::Arc::new(sunmao_llm::OaiClient::new(
            &cli.base_url,
            &cli.api_key,
            &cli.model,
        )),
    };
    let probe = sunmao_llm::ChatRequest {
        messages: &[sunmao_llm::types::Message::user("ping")],
        tools: None,
        max_tokens: Some(1),
        temperature: None,
    };
    match llm.stream(probe).await {
        Ok(_) => println!("OK"),
        Err(e) => {
            ok = false;
            println!("FAIL ({e:#})");
        }
    }

    // 2. rg for Grep
    print!("rg binary ... ");
    match which_rg() {
        Some(p) => println!("OK ({})", p),
        None => {
            ok = false;
            println!("FAIL (not found — Grep tool will error at runtime)");
        }
    }

    // 2b. shell backend — SUNMAO_SHELL decides whether the Bash tool uses
    // the embedded POSIX shell or a real `pwsh`; a wrong value fails EVERY
    // Bash call, not just this check.
    print!("shell backend ... ");
    match std::env::var_os("SUNMAO_SHELL") {
        None => println!("embedded POSIX shell (deno_task_shell)"),
        Some(v) => {
            let name = v.to_string_lossy();
            if name.eq_ignore_ascii_case("pwsh") {
                match std::process::Command::new("pwsh")
                    .arg("-NoProfile")
                    .arg("-Command")
                    .arg("1")
                    .output()
                {
                    Ok(p) if p.status.success() => println!("pwsh OK"),
                    _ => {
                        ok = false;
                        println!(
                            "FAIL (SUNMAO_SHELL=pwsh but `pwsh` won't run — unset it or fix PATH)"
                        );
                    }
                }
            } else {
                ok = false;
                println!(
                    "FAIL (SUNMAO_SHELL={name} — only `pwsh` is a valid opt-in; anything else falls back but signals a typo)"
                );
            }
        }
    }

    // 3. session dir writable
    print!("session dir {} ... ", cli.session_dir.display());
    match std::fs::create_dir_all(&cli.session_dir) {
        Ok(()) => println!("OK"),
        Err(e) => {
            ok = false;
            println!("FAIL ({e})");
        }
    }

    // 3b. git — not required to run, but `git`-shaped tool calls (and half
    // the risky-pattern watch list) assume it; warn, don't fail.
    print!("git ... ");
    match std::process::Command::new("git").arg("--version").output() {
        Ok(p) if p.status.success() => {
            let v = String::from_utf8_lossy(&p.stdout);
            println!("OK ({})", v.trim());
        }
        _ => println!("not found — git commands will fail (non-fatal)"),
    }

    // 4. config files present
    for f in [
        ".sunmao/hooks.json",
        ".sunmao/mcp.json",
        ".sunmao/permissions.json",
        ".sunmao/prompt.md",
        ".claude/settings.json",
    ] {
        let p = cli.cwd.join(f);
        if p.exists() {
            println!("config {} present", f);
        }
    }
    if let Ok(entries) = std::fs::read_dir(cli.cwd.join(".sunmao/prompt.d")) {
        for e in entries.flatten() {
            println!(
                "config .sunmao/prompt.d/{} present",
                e.file_name().to_string_lossy()
            );
        }
    }

    // 5. prompt assembly preview — the same PromptAssembler every frontend
    // runs; --system shows up as the complete override. Enabled presets
    // contribute to the skills index, so the preview sees them too.
    let preset_roots = match sunmao_core::presets::resolve(&cli.cwd, &cli.preset) {
        Ok(roots) => {
            for r in &roots {
                println!("preset active: {}", r.display());
            }
            roots
        }
        Err(e) => {
            ok = false;
            println!("presets: FAIL ({e:#})");
            Vec::new()
        }
    };
    let names = sunmao_core::presets::list_names(&cli.cwd);
    if !names.is_empty() {
        println!("presets available: {}", names.join(", "));
    }
    {
        let assembled = sunmao_core::prompt::PromptAssembler::new(&cli.cwd)
            .with_extra_roots(&preset_roots)
            .assemble(cli.system.as_deref());
        let first = assembled.lines().next().unwrap_or("");
        println!(
            "system prompt: {} bytes — first line: {}",
            assembled.len(),
            first
        );
    }

    // 6. MCP servers: list without connecting (connection smoke is live-tested)
    let mcp_path = cli.cwd.join(".sunmao/mcp.json");
    if let Ok(text) = std::fs::read_to_string(&mcp_path)
        && let Ok(v) = serde_json::from_str::<serde_json::Value>(&text)
        && let Some(servers) = v["mcpServers"].as_object()
    {
        for name in servers.keys() {
            println!("mcp server configured: {name}");
        }
    }

    // 7. model routing: models.json parses, agents dir counted
    let models_path = cli.cwd.join(".sunmao/models.json");
    if models_path.exists() {
        print!("models.json ... ");
        match std::fs::read_to_string(&models_path)
            .ok()
            .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok())
        {
            Some(_) => println!("OK"),
            None => {
                ok = false;
                println!("FAIL (invalid JSON — selectors will not resolve)");
            }
        }
    }
    // 7b. prompt.d files that shadow a kernel section — the override is by
    //     design, but a same-stem file the user forgot (or one a *future*
    //     builtin claims) silently becomes a replacement. Info, not failure.
    for dir in [
        sunmao_core::prompt::user_layer_dir(),
        cli.cwd.join(".sunmao"),
    ] {
        for (stem, path) in sunmao_core::prompt::shadowed_builtins(&dir) {
            println!(
                "prompt.d: {} shadows builtin section `{}` (replaces it in place)",
                path.display(),
                stem
            );
        }
    }
    if let Ok(entries) = std::fs::read_dir(cli.cwd.join(".sunmao/agents")) {
        let n = entries
            .flatten()
            .filter(|e| e.path().extension().map(|x| x == "md").unwrap_or(false))
            .count();
        if n > 0 {
            println!("agents: {n} sub-agent definition(s)");
        }
    }

    // 8. extensions: plugin manifests must parse (binary/script children
    //    are the manifest's own responsibility — `--doctor` only counts)
    let mut ext_specs = 0usize;
    let mut manifests = vec![cli.cwd.join(".sunmao/plugin.json")];
    for base in [
        cli.cwd.join(".sunmao/plugins"),
        cli.cwd.join(".claude/plugins"),
    ] {
        let mut plugins: Vec<_> = std::fs::read_dir(&base)
            .map(|rd| rd.flatten().map(|p| p.path()).collect())
            .unwrap_or_default();
        plugins.sort();
        for p in plugins {
            if p.is_dir() {
                manifests.push(p.join("plugin.json"));
            }
        }
    }
    for manifest in &manifests {
        if !manifest.exists() {
            continue;
        }
        match std::fs::read_to_string(manifest)
            .ok()
            .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok())
        {
            Some(v) => {
                if let Some(exts) = v["extensions"].as_array() {
                    ext_specs += exts.len();
                }
            }
            None => {
                ok = false;
                println!(
                    "plugin manifest {} ... FAIL (invalid JSON)",
                    manifest.display()
                );
            }
        }
    }
    if ext_specs > 0 {
        println!("extensions: {ext_specs} spec(s) across plugin manifests");
    }

    println!();
    println!(
        "{}",
        if ok {
            "all checks passed"
        } else {
            "some checks FAILED"
        }
    );
    std::process::exit(if ok { 0 } else { 1 });
}

fn which_rg() -> Option<String> {
    for cand in ["rg", "rg.exe"] {
        if let Ok(p) = std::process::Command::new(cand).arg("--version").output()
            && p.status.success()
        {
            let v = String::from_utf8_lossy(&p.stdout);
            return Some(v.lines().next().unwrap_or("?").trim().to_string());
        }
    }
    None
}
