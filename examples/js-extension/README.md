# js-extension — the JS extension-host sidecar, bundled

A plugin bundle that ships **JS extension modules** instead of a bespoke
binary. `plugin.json` spawns `extension-host.mjs`, which fronts the
`sunmao` extension protocol for every `.mjs`/`.js` file under `ext/`.

## Layout

```text
plugin.json          — "extensions": node extension-host.mjs ext/
extension-host.mjs   — a copy of tools/extension-host.mjs from the
                       sunmao repo. It is a REAL copy on purpose (no
                       symlinks, no runtime fetch): a plugin bundle is
                       self-contained — ${CLAUDE_PLUGIN_ROOT} only
                       resolves inside the bundle dir. Re-copy it when
                       the upstream sidecar changes.
ext/wordcount.mjs    — one JS extension module (a tool + a SessionStart
                       subscription)
```

## Install / try

```bash
# as an always-on plugin
cp -r examples/js-extension .sunmao/plugins/js-extension

# or as a preset (active only while named)
cp -r examples/js-extension .sunmao/presets/jsext
sunmao --preset jsext
```

Requires `node` on PATH (any recent version — the sidecar is
`node:`-builtins-only). The tool surfaces as
`ext__js-extension__wordcount`.

## Module contract

Each `*.mjs` / `*.js` file under `ext/` exports a default function:

```js
export default function (api) {
  api.registerTool({ name, description, input_schema, handler });
  //   handler: async (args) => ({content} | {content, is_error:true})

  api.on("<HookEvent>", (payload) => ({
    extra_context: ["…"],      // concatenated across modules
    block: "reason",           // vetoes — last non-null wins
    updatedInput: { … },       // PreToolUse arg rewrite — last wins
    permissionDecision: "deny" // "deny" | "ask" | "allow" — last wins
  }));

  api.log("…");                // stderr; stdout is protocol-only
}

export function dispose() {}   // optional — runs on ext/shutdown
```

`*.ts` files are skipped with a warning (no toolchain — ship compiled
`.mjs`/`.js`). One bad module degrades to a stderr warning; the bridge
keeps serving the rest.
