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

    // 3. session dir writable
    print!("session dir {} ... ", cli.session_dir.display());
    match std::fs::create_dir_all(&cli.session_dir) {
        Ok(()) => println!("OK"),
        Err(e) => {
            ok = false;
            println!("FAIL ({e})");
        }
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

    // 5. MCP servers: list without connecting (connection smoke is live-tested)
    let mcp_path = cli.cwd.join(".sunmao/mcp.json");
    if let Ok(text) = std::fs::read_to_string(&mcp_path) {
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) {
            if let Some(servers) = v["mcpServers"].as_object() {
                for name in servers.keys() {
                    println!("mcp server configured: {name}");
                }
            }
        }
    }

    // 6. model routing: models.json parses, agents dir counted
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
    if let Ok(entries) = std::fs::read_dir(cli.cwd.join(".sunmao/agents")) {
        let n = entries
            .flatten()
            .filter(|e| e.path().extension().map(|x| x == "md").unwrap_or(false))
            .count();
        if n > 0 {
            println!("agents: {n} sub-agent definition(s)");
        }
    }

    // 7. extensions: plugin manifests must parse; node presence matters only
    //    when a manifest references the JS sidecar (binary extensions don't)
    let mut ext_specs = 0usize;
    let mut uses_js_host = false;
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
                    for e in exts {
                        let cmd = e["command"].as_str().unwrap_or("");
                        let args: Vec<String> = e["args"]
                            .as_array()
                            .map(|a| {
                                a.iter()
                                    .filter_map(|x| x.as_str().map(String::from))
                                    .collect()
                            })
                            .unwrap_or_default();
                        if cmd == "node" || args.iter().any(|a| a.contains("extension-host.mjs")) {
                            uses_js_host = true;
                        }
                    }
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
    if uses_js_host {
        print!("node (JS extension host) ... ");
        match which_node() {
            Some(p) => println!("OK ({p})"),
            None => {
                ok = false;
                println!("FAIL — a manifest spawns extension-host.mjs but node isn't on PATH");
            }
        }
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
        if let Ok(p) = std::process::Command::new(cand).arg("--version").output() {
            if p.status.success() {
                let v = String::from_utf8_lossy(&p.stdout);
                return Some(v.lines().next().unwrap_or("?").trim().to_string());
            }
        }
    }
    None
}

fn which_node() -> Option<String> {
    for cand in ["node", "node.exe"] {
        if let Ok(p) = std::process::Command::new(cand).arg("--version").output() {
            if p.status.success() {
                let v = String::from_utf8_lossy(&p.stdout);
                return Some(v.lines().next().unwrap_or("?").trim().to_string());
            }
        }
    }
    None
}
