# CONFIG.md — every file sunmao reads

All optional. Missing file = feature off. Invalid JSON = warning, feature off. Project-level beats user-level; later files in each list override earlier ones per-key where merging applies (hooks/permissions/mcp).

## sunmao-native (`<cwd>/.sunmao/`)

| File | Shape | Effect |
|---|---|---|
| `hooks.json` | `{"hooks": {"PreToolUse": [{"matcher": "Bash", "hooks": [{"type": "command", "command": "..."}]}]}}` | Claude-contract hook procs; stdin=JSON event, exit 0 allow / 2 veto. Project/plugin/preset commands run only once pinned — see `trusted-hooks.json` |
| `trusted-hooks.json` | `{"trusted": {"<sha256(canonical-source + "\\n" + command)>": {"source","command"}}}` | trust ledger — written by `/hooks trust <n>`, removed when empty. It covers every "a config file names a command we would exec" surface: hook commands, plugin `extensions` spawn specs and MCP `command:` stdio servers (`url` transports don't spawn and aren't listed). For spawn specs the digest's `command` is the expanded `{"command","args","env"}` JSON — editing any leg invalidates the pin; for a `perm` row it's the rule text verbatim. Untrusted rows are skipped and audit-logged (`hook.untrusted` / `ext.untrusted` / `mcp.untrusted`); the `/hooks` roster lists all three kinds under one numbering. User-level hook files (`~/.claude`, `~/.codex`, `~/.cursor`) are implicitly trusted and never need a row |
| `mcp.json` | `{"mcpServers": {"name": {"command","args","env"} \| {"url","headers"?,"auth_env"?,"token_file"?,"timeout_secs"?}}}` | MCP servers — stdio spawn or streamable-HTTP; `auth_env`/`token_file` supply a static bearer token, `headers` merges literal request headers. Server prompts surface as `/srv:name` slash commands. `command:` specs spawn only once pinned (`trusted-hooks.json`); `url` specs connect ungated |
| `permissions.json` | `{"permissions": {"allow":[..],"ask":[..],"deny":[..]}}` | `Tool(glob)` rules; deny>ask>allow>default. Project-layer `allow` rules apply only once pinned in `trusted-hooks.json` — an allow short-circuits the approval gate, so it runs the same trust check as a hook command (`/hooks` lists them as `perm` rows; deny/ask always merge — they only narrow the surface). Specifier extensions: a `!` prefix negates the entry (a bucket matches iff some positive entry hits and no `!` entry vetoes — `["Read(**/.env)", "Read(!**/.env.example)"]`), `re:` compiles the specifier as a regex (invalid regexes degrade to literal match, like hook matchers) |
| `risky-patterns.txt` | `pattern | reason` per line | **replaces** the shipped approval-gate table outright (cold-plug); preset dirs' same-named file merges additively |
| `plugin.json` | `{"name", "hooks":{...}, "mcpServers":{...}, "extensions":[{...}], "loop": "full"\|"bare"}` | bundle manifest — folds hooks + MCP + extension children into the same paths; `extensions` entries are `{command, args, env}` spawn specs (`${CLAUDE_PLUGIN_ROOT}` → the plugin dir) — any executable speaking the `ext/*` JSON-RPC protocol qualifies (PROTOCOLS.md "Extension protocol"); tools surface as `ext__{name}__{tool}`; hooks, `mcpServers` command specs and `extensions` spawn only once pinned (`trusted-hooks.json`); `loop` picks the turn driver — `full` (contract loop: hooks+gate+compaction), `bare` (none of those), or `ptc` (full loop but only `RunCode`+`SearchTools` are advertised; everything else rides the script's `tools.*` bridge) |
| `prompt.md` | markdown | system-prompt section appended to the assembly |
| `prompt.d/<name>.md` | markdown | prompt section; a name matching a built-in section (`identity`, `tool-guidance`, `shell-dialect`, `subagent-default`) **replaces** that section — the cold-plug mechanism |
| `commands/*.md` | markdown | `/name` injects file body as prompt; `/name args` substitutes `$ARGUMENTS` / positional `$1`..`$9` where the body placed them — bodies with no placeholder get args appended |
| `skills/*/SKILL.md` | frontmatter `name`/`description` + body | indexed; body read on demand. `SKILL.html` is the alternate skill body (`<title>`/`<meta name="description">` supply the index fields; `SKILL.md` wins when both exist). Bundled `*.html` files count as resources and surface in the index line |
| `agents/*.md` | frontmatter `name`/`description`/`model`/`tools`/`spawns`/`permissions` + body | `Task` tool `subagent_type` picks; body = sub-agent system prompt; `model` routes the spawn (see below); `tools` (CSV/list) trims the child's tool registry; `spawns` (CSV/list, `*`=all) whitelists what it may itself spawn — a restricted parent's omitted `subagent_type` defaults to the first entry, self-recursion is refused; `permissions` (CSV/list of `deny:`/`ask:`-prefixed `Tool(spec)` rules, same specifier syntax as permissions.json) overlays deny/ask onto the child's permission table — `allow:`/unprefixed entries are ignored, so a def can only narrow the child's surface, never widen it (an applied overlay logs an `agent.perms` audit row) |

**Sub-agent message channels** — `Task{steer:"sub-…-lN", message:"…"}` injects a mid-run user message into a running child (folded at its next request boundary; the steer queues, it doesn't interrupt). `SendMessage` is the child-side uplink: pushes a `<sub-agent-message id=… lane=…>`-tagged user message onto the parent's steer queue, same fold semantics in reverse — a background child can ask mid-run instead of waiting for `task_done`. Foreground `Task` returns `[task:sub-…-lN]` so the next call has a handle. The roster's cancel control (`task_cancel` ws frame → `cancel_sub`) is the surgical version of `agent.cancel()`: it trips that ONE child's flag+notify and its own finish path records `done=false` — a cancelled child is a failure, not a clean exit (`TurnOutcome::Cancelled` discriminates it).
| `models.json` | `{"providers": {"p": {"base_url","api_key_env","dialect"}}, "routes": {"r": "sel" \| ["sel",...]}, "default_model": "sel"}` | model routing — `model:` selectors resolve `provider/model`, bare `model` (session provider), or `@route` chains; unresolvable → inherit parent. `default_model` (optional) is the selector a **new** session starts on — `--model`/`SUNMAO_MODEL` still wins, and a session already under way keeps the model in its own log |
| `plugin/` | same tree as a plugin root | "this project is a plugin" convention |
| `plugins/<name>/` | plugin dir | contributes `commands/`, `skills/`, `agents/` **and** merges its `plugin.json` (`hooks` + `mcpServers`, `${CLAUDE_PLUGIN_ROOT}` → the plugin dir); `sunmao plugin install|list|remove` manages this dir — install takes a local dir, a git URL, or `owner/repo` (clones via `git`, depth 1) |
| `plugins/<name>.js` | ES module | **frontend** plugin — a different species from the `<name>/` bundle dirs above (a file, not a dir; registers UI slots, no backend effects). Runs only while pinned in `plugins.json` at its current sha256; a changed file stops serving until re-approved, and disabling/tampering tears the plugin's surfaces down live (panes, pages, chips, palette rows, menu items, transforms all unregister). `spec.id` must equal the filename minus `.js`, and `sunmao.register({id, slots:{...}})` must run **during module evaluation** — deferred `setTimeout(register)` is rejected (the import window is how the file↔plugin link is verified). Slot ids match `[\w-]+`. Slots: `dock` (pane `{id,title,icon,mount}`), `settings` (page `{id,title,icon,render}`), `composer` (cmp-bar chip `{id,icon,label,tip,onClick|popover(el,host)}`), `palette` (command row `{g,t,i,k,sub,run(host)}`), `sessionMenu` (right-click item `{v,t,i,warn,run(id,host)}`). `host` facade: `api()`, `sess()`, `session()` ({id,cwd,model,driver,busy,goal}), `on('live'|'session')`, `onSend(fn)` draft transform (return string to rewrite, `false` to veto), `send()`/`steer()` (prompt frames), `pop()`/`menuPop()`/`closePop()` (chrome popovers), `store(k,v)`/`load(k)` (namespaced KV via ui.json), `icon(name,svg)` (register a custom `<symbol>`), `t`/`esc`/`ic`/`toast` — the facade is convenenience-scoped, not a boundary: an enabled plugin is trusted page code (same-origin fetch exists); the pin ledger is the trust control and iframe isolation is the v2 path. Manage via the settings 插件 page; `GET /plugins?sess=` lists, `GET /plugins/{name}?sess=` serves only enabled+unmodified bytes, `PUT /plugins {name, enabled}` writes the ledger. Examples: `examples/plugins/todos.js` (dock), `examples/plugins/snippets.js` (composer+palette+sessionMenu+store) |
| `plugins.json` | `{"enabled": {"<name>.js": "<sha256-hex>"}}` | frontend-plugin pin ledger — same shape as `trusted-hooks.json`, written by the settings page |
| `presets/<name>/` | plugin dir | same bundle shape as `plugins/<name>/` (plus `permissions.json`), but only active while named via `--preset <name>` — see "Presets" below |
| `sessions/*.jsonl` | runtime state (gitignored) | session logs — `--resume`/`--fork`/`--dataflow` read these |
| `shell.txt` | one word: `pwsh` / `powershell` / `posix` / `bash` / `deno` / `auto` | shell backend pin for `Bash` — see "Shell backend" below (the GUI's 终端 settings page writes it through `PUT /shell`) |
| `ui.json` | `{"mode","accent","background","foreground","wallpaper","dim","panelOpacity","blur","translucentSidebar","contrast","fonts"{ui,code},"zoom",...}` | GUI appearance preferences — the browser settings page persists via `GET|PUT /ui`; browser localStorage is only a first-frame cache. `zoom` is the Tauri shell's webview zoom factor (Ctrl/Cmd+=/-/0), restored on startup |
| `wallpapers/` | runtime state | uploaded custom wallpaper (`custom.{jpg,png,webp}`, one file — `PUT /wallpaper` rotates it; `GET /wallpaper` serves it) |
| `checkpoints/{session_id}/` | runtime state | snapshot-before-write ledger — `{seq}-{hash}.bak` blobs + `manifest.jsonl`; `/rewind` restores files from the earliest entry at/after the target turn |
| `jobs/{id}/` | runtime state | background `Bash` jobs — `output.log` + `exit.json` (on finish); `job.json`'s `foreground` marker keeps an inline command out of the panel until a timeout moves it to the background, and a command that ends inside its budget retires its own dir |
| `artifacts/` | runtime state | `HtmlArtifact` outputs — `{name}.html` plus `{name}.state.json` human-annotation sidecars |

## Claude-compatible (`<cwd>/.claude/`, `~/.claude/`)

| Path | Read for |
|---|---|
| `.claude/settings.json` | `hooks` + `permissions` blocks (merged) |
| `.claude/settings.local.json` | same, local overrides |
| `.codex/hooks.json` | same `{"hooks": {...}}` shape — Codex bundles (e.g. rtk `init --codex`) load verbatim |
| `.cursor/hooks.json` | Cursor flat entries `{command, matcher, timeout}` under camelCase events — normalized by `hooks/cursor.rs` |
| `~/.claude/settings.json` | user-level same blocks |
| `~/.codex/hooks.json` | user-level Codex hooks |
| `~/.cursor/hooks.json` | user-level Cursor hooks |
| `~/.sunmao/prompt.md` | user-level prompt section (applied before the project layer) |
| `~/.sunmao/shell.txt` | user-level shell backend pin — the machine-wide default a project can override |
| `~/.sunmao/prompt.d/<name>.md` | user-level section replacement, same naming rule |
| `~/.sunmao/presets/<name>/` | user-level preset dir — searched when the project has no match |
| `~/.sunmao/channels.json` | IM gateway config (`dm_policy`/`dm_scope`/`allowlist`/`owner`/`channels[]`) — read by `sunmao im` at startup; `token_env`/`token_file` supply credentials, never inline. GUI 设置→IM 渠道 edits it via `GET|PUT /channels`; full contract in `docs/IM.md` |
| `~/.sunmao/im/` | IM gateway runtime state — `state.db` (routing index, pairing codes, allowlist, delivery ledger, adapter cursors), `status.json` heartbeat, `workspace/` (the fixed IM cwd whose `.sunmao/sessions/im-main.jsonl` is the shared DM session) |
| `.claude/commands/*.md` | slash commands |
| `.claude/agents/*.md` | sub-agent defs |
| `.claude/skills/*/SKILL.md` | skill index |
| `.claude/plugins/<name>/` | `commands/`+`skills/`+`agents/` dirs + `plugin.json` manifest (same merge as `.sunmao/plugins/`) |
| `.claude-plugin/plugin.json` | plugin manifest at repo root |
| `~/.agents/skills/*/SKILL.md` | ecosystem skills |

## Presets (`--preset <name>`)

A preset is a directory that looks exactly like an installed plugin bundle — `plugin.json`, `hooks/hooks.json`, `commands/`, `skills/`, `agents/`, `mcp.json`, `permissions.json`. Unlike `plugins/<name>/` (always active), a preset contributes **only while named on the command line**:

```bash
sunmao --preset strict-audit --preset +verbose   # layers in order; + is decorative
```

Resolution: `<name>` is looked up in `<cwd>/.sunmao/presets/` first, then `~/.sunmao/presets/`; an unknown name is a startup error listing the dirs searched. Each resolved dir is an extra plugin root appended **after** the always-on sources — preset hooks run last, a preset `mcpServers` key overrides a same-named one, and `permissions.json` rules merge into the same deny>ask>allow table. For first-match surfaces (slash command names, agent defs) a preset fills gaps rather than shadowing project files. Sub-agents inherit the parent's presets. `sunmao --doctor` lists active and available presets.

## Shell backend (`Bash` tool)

`Bash` runs either on the embedded POSIX interpreter (`deno_task_shell` — identical syntax on every platform) or on a real PowerShell 7 via `pwsh -EncodedCommand`. Resolution order, first hit wins:

```text
1. SUNMAO_SHELL              (env var — the machine-wide default)
2. <cwd>/.sunmao/shell.txt   (project pin)
3. ~/.sunmao/shell.txt       (user pin)
4. auto-detect               (no config at all)
```

Values: `pwsh`/`powershell` pick PowerShell, `posix`/`bash`/`deno` pin the embedded interpreter, `auto` forces detection past a lower layer's pin. `pwsh` resolves only when the `pwsh` binary is on PATH — `powershell.exe` is Windows PowerShell 5 and never counts. Auto-detect picks `pwsh` on Windows when it's on PATH, Posix everywhere else; a `pwsh` request on a box without the binary silently falls back rather than failing every `Bash` call (`sunmao doctor` flags that case). `sunmao doctor` reports the effective backend, which layer chose it, and `pwsh --version`.

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

Same stem = same section: a later file named `identity.md` replaces the identity section in place. `--doctor` prints the assembled byte count and first line. `Task` resolves `subagent_type` against `agents/*.md`, else the `subagent-default` section (replaceable the same way). The reserved builtin stems are `identity`, `tool-guidance`, `shell-dialect`, `compact`, `subagent-default`, `project-context` — a `prompt.d/` file under one of these names silently *replaces* the builtin rather than adding a section, so give custom sections distinct names.

## Event vocabulary (what lands in `sessions/*.jsonl`)

```jsonc
{"type":"started","model":"…","cwd":"…"}
{"type":"message","message":{role,content?,tool_calls?,tool_call_id?}}
{"type":"tool_call","call":{id,function{name,arguments}},"depth":0}
{"type":"tool_result","call_id","name","ok","output","depth":0}
{"type":"compacted","summary"}        // clears transcript on fold
{"type":"artifact","name","path","bytes"}
{"type":"usage","usage":{prompt_tokens,completion_tokens,total_tokens}}
{"type":"hook","event":"PreToolUse.updatedInput","detail":"…"}  // audit-only, skipped by the message fold
// trust pinning writes two more Hook events: "hook.untrusted" (a
// project/plugin command skipped at plan time — detail names event,
// command, source file) and "hook.trust"/"hook.untrust" (the pin
// decision itself, so who approved what is durable too); the same
// ledger gates spawn surfaces — "ext.untrusted" / "mcp.untrusted" are
// the connect-time skips for plugin extensions and stdio MCP servers
{"type":"task_done","id":"sub-…-l2","ok":true,"output":"…"}  // background Task finished — folds into the message stream as a <task-result> user message; full transcript at sessions/<id>.jsonl
// a Task{steer} mid-run injection lands as a plain
//   {"type":"message","message":{"role":"user",…}} (folded at the child's
//   next request boundary) PLUS an audit-only {"type":"hook","event":"steer"}
//   row — the folded message is byte-identical to typed input, the hook row
//   is the attribution trail.
// a child's SendMessage uplink lands on the PARENT side the same way — a
//   tagged user message <sub-agent-message id=… lane=…> folded at the
//   parent's next boundary, again paired with the steer hook row.
{"type":"mode_change","mode":"accept_edits"}  // approval stance changed — audit-only; a resumed session reseeds Context.approval_mode from the last one
{"type":"session_meta","title":"…"}  // serve rename — audit-only; readers take the LAST one as the rail/title, overriding first-prompt derivation
{"type":"checkpoint","turn":N,"files":["a.txt",...]}  // pre-write bytes snapshotted into checkpoints/{session_id}/ — audit-only, skipped by the fold; /rewind folds the manifest back
```

`--dataflow <file>` folds these into a JSON report (files read/written, shell commands, tool calls/failures, compactions, token totals, `sub_agent_uplinks`/`sub_agent_downlinks` — the SendMessage/steer channel traffic listed under `data_flow`).

## Model routing (`models.json`)

Sub-agents resolve a `model:` selector against named providers. Example — a cheap/fast model for scout-style agents, session model otherwise:

```jsonc
{
  "providers": {
    "big": { "base_url": "https://api.example.com/v1", "api_key_env": "BIG_KEY", "dialect": "anthropic" }
  },
  "routes": { "smol": ["big/claude-haiku", "qwen-flash"] }
}
```

- `big/claude-haiku` — explicit provider + model
- `qwen-flash` — bare id on the session's provider (env/flags)
- `@smol` — route name; a list is an ordered fallback chain
- `agents/*.md` `model:` pins any of these; absent or unresolvable → the sub-agent inherits the parent's adapter. Keys come from `api_key_env` (an env var name), never the file itself.
- `dialect`: `"openai"` (chat completions, default), `"openai-responses"` (OpenAI `/responses` — required for o-series/GPT-5 reasoning; the adapter chains `previous_response_id` + `prompt_cache_key` so repeat requests send only new items while provider prefix caching stays warm), `"anthropic"` (`/messages`).
- `/model [selector]` in the TUI switches the *session's* active adapter mid-run (next request onward); bare `/model` lists routes + providers.

### Catalog entries: capability flags

A catalog entry can carry optional capability flags beyond `id`:

```jsonc
{ "id": "qwen3-max", "vision": true, "context_length": 262144,
  "thinking": ["low", "medium", "high"], "reasoning": true }
```

- `vision`, `context_length` — surfaced as picker badges and available to the model-choice UI.
- `thinking` — the model's selectable thinking/effort levels. `/effort` and the GUI chip list them verbatim; the vocabulary is free-form (providers don't agree on one).
- `reasoning` — the provider signals reasoning support without naming levels (e.g. OpenRouter `supported_parameters` containing `"reasoning"`); frontends then offer the canonical `low`/`medium`/`high` trio.
- Both fill automatically from `/models` fetch when the endpoint declares them (`supported_parameters`, `capabilities`, a truthy `reasoning` field); hand-edited entries pass through verbatim.

`/effort [level]` sets a session-scoped reasoning-effort override — the value lands in the request's dialect spelling (chat completions `reasoning_effort`, responses `reasoning.effort`, anthropic `output_config.effort`). Bare `/effort` lists the current override + the active model's levels; `/effort default` clears. The setting rides the same surfaces as `/model`: a durable `effort.change` audit fact (resume reseeds it), a GUI composer chip, and the ACP `ThoughtLevel` config option. Any string passes through — a catalog that never learned the provider's vocabulary is a UI hint, not a gate.

## Eval cases (`sunmao eval <file>`)

Case-driven regression runner: each case sends `prompt` through the real agent loop in a fresh session, then asserts against the recorded `ToolCall` events and the final assistant message. A case file is one JSON object, a JSON array of them, or JSONL (one object per line, blank lines and `#`/`//` comments skipped):

```jsonc
{
  "name": "uses-read-before-write",
  "prompt": "fix the typo in note.txt then tell me DONE",
  "cwd": "fixtures/case1",              // optional; relative to the eval
                                        // file's dir, default = --cwd
  "expect": {
    "final_contains": "DONE",           // last assistant text must contain
    "tool_called": ["Read", "Write"],   // each must appear as a ToolCall
    "tool_not_called": ["Bash"],        // must NOT appear
    "max_tool_calls": 10,               // total ToolCall events ≤ N
    "turns": 1                          // v1 runs exactly one turn
  }
}
```

Each case prints `PASS`/`FAIL name — <failures>`; a summary line and a nonzero exit on any failure. `--report <path>` writes the case results as a JSON array. Every case is a real session — its log lands in `--session-dir` as `s-<secs>-c<idx>.jsonl`, so hooks fire and `transcript_path` is a real file.

Multi-step cases — `steps: [{prompt, expect}]` instead of the flat `prompt`/`expect` (mixing both is a parse error): each step is its own `run_turn` on the SAME session, so step 2 sees step 1's transcript. Per-step assertions scope to that turn's `ToolCall` events and reply; failures read `step N: <failure>`.

