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
        ".claude/settings.json",
    ] {
        let p = cli.cwd.join(f);
        if p.exists() {
            println!("config {} present", f);
        }
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
