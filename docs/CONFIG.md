# CONFIG.md — every file sunmao reads

All optional. Missing file = feature off. Invalid JSON = warning, feature off.
Project-level beats user-level; later files in each list override earlier ones
per-key where merging applies (hooks/permissions/mcp).

## sunmao-native (`<cwd>/.sunmao/`)

| File | Shape | Effect |
|---|---|---|
| `hooks.json` | `{"hooks": {"PreToolUse": [{"matcher": "Bash", "command": "..."}]}}` | Claude-contract hook procs; stdin=JSON event, exit 0 allow / 2 veto |
| `mcp.json` | `{"mcpServers": {"name": {"command","args","env"} \| {"url"}}}` | MCP servers — stdio spawn or streamable-HTTP |
| `permissions.json` | `{"permissions": {"allow":[..],"ask":[..],"deny":[..]}}` | `Tool(glob)` rules; deny>ask>allow>default |
| `plugin.json` | `{"name", "hooks":{...}, "mcpServers":{...}}` | bundle manifest — folds hooks + MCP into the same paths |
| `prompt.md` | markdown | system-prompt section appended to the assembly |
| `prompt.d/<name>.md` | markdown | prompt section; a name matching a built-in section (`identity`, `tool-guidance`, `shell-dialect`, `subagent-default`) **replaces** that section — the cold-plug mechanism |
| `commands/*.md` | markdown | `/name` injects file body as prompt |
| `skills/*/SKILL.md` | frontmatter `name`/`description` + body | indexed; body read on demand |
| `agents/*.md` | frontmatter `name`/`description` + body | `Task` tool `subagent_type` picks; body = sub-agent system prompt |
| `plugin/` | same tree as a plugin root | "this project is a plugin" convention |
| `plugins/<name>/` | plugin dir | contributes `commands/`, `skills/`, `agents/` — its `plugin.json` is NOT currently merged (only the two top-level manifests are) |
| `sessions/*.jsonl` | runtime state (gitignored) | session logs — `--resume`/`--fork`/`--dataflow` read these |
| `jobs/{id}/` | runtime state | background `Bash` jobs — `output.log` + `output.idx` + `meta.json` |
| `artifacts/` | runtime state | `HtmlArtifact` outputs |

## Claude-compatible (`<cwd>/.claude/`, `~/.claude/`)

| Path | Read for |
|---|---|
| `.claude/settings.json` | `hooks` + `permissions` blocks (merged) |
| `.claude/settings.local.json` | same, local overrides |
| `~/.claude/settings.json` | user-level same blocks |
| `~/.sunmao/prompt.md` | user-level prompt section (applied before the project layer) |
| `~/.sunmao/prompt.d/<name>.md` | user-level section replacement, same naming rule |
| `.claude/commands/*.md` | slash commands |
| `.claude/agents/*.md` | sub-agent defs |
| `.claude/skills/*/SKILL.md` | skill index |
| `.claude/plugins/<name>/` | `commands/`+`skills/`+`agents/` dirs only — manifest fields not merged |
| `.claude-plugin/plugin.json` | plugin manifest at repo root |
| `~/.agents/skills/*/SKILL.md` | ecosystem skills |

## Provider config (env or flags)

```bash
SUNMAO_BASE_URL   default http://127.0.0.1:7863/v1
SUNMAO_API_KEY    default your-api-key-here
SUNMAO_MODEL      default global:deepseek-v4.1-flash
SUNMAO_PROVIDER   openai (default) | anthropic
```

## System prompt assembly (PromptAssembler)

```text
order  source
10–30  built-in sections — crates/core/assets/prompt/{identity,
       tool-guidance, shell-dialect}.md (baked via include_str!)
40     ~/.sunmao/prompt.md + ~/.sunmao/prompt.d/*.md   (sorted)
50     .sunmao/prompt.md + .sunmao/prompt.d/*.md       (sorted)
60     project context — AGENTS.md / CLAUDE.md + skills index (dynamic)
——     --system flag: complete replacement, outranks everything
```

Same stem = same section: a later file named `identity.md` replaces the
identity section in place. `--doctor` prints the assembled byte count and
first line. `Task` resolves `subagent_type` against `agents/*.md`, else the
`subagent-default` section (replaceable the same way).

## Event vocabulary (what lands in `sessions/*.jsonl`)

```jsonc
{"type":"started","model":"…","cwd":"…"}
{"type":"message","message":{role,content?,tool_calls?,tool_call_id?}}
{"type":"tool_call","call":{id,function{name,arguments}}}
{"type":"tool_result","call_id","name","ok","output"}
{"type":"compacted","summary"}        // clears transcript on fold
{"type":"artifact","name","path","bytes"}
{"type":"usage","usage":{prompt_tokens,completion_tokens,total_tokens}}
```

`--dataflow <file>` folds these into a JSON report (files read/written,
shell commands, tool calls/failures, compactions, token totals).
