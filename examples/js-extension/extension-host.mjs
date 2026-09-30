// extension-host.mjs — the JS extension-host sidecar for sunmao.
//
// Lets a plugin bundle ship JS extension *modules* instead of a bespoke
// binary: plugin.json spawns this host, and this host fronts the
// `sunmao` extension protocol (docs/PROTOCOLS.md "Extension protocol")
// for every module in a directory:
//
//   {"command": "node",
//    "args": ["<bundle>/extension-host.mjs", "<bundle>/ext"]}
//
// The file is dependency-free on purpose — it is meant to be COPIED into
// a plugin bundle so ${CLAUDE_PLUGIN_ROOT} resolves inside the bundle.
// The canonical copy lives at tools/extension-host.mjs.
//
// Wire: one JSON-RPC 2.0 object per line on stdin/stdout. Replies carry
// the request's `id`; ext/shutdown is a notification (no id, no reply).
// stdout is protocol-only — log to stderr (api.log does).
//
// Module contract — each *.mjs / *.js file in the scanned dir exports a
// default function the host calls once at startup:
//
//   export default function (api) {
//     api.registerTool({name, description, input_schema, handler});
//     api.on("SessionStart", (payload) => ({extra_context: ["…"]}));
//     api.log("anything");
//   }
//   export function dispose() {}   // optional — runs on ext/shutdown
//
// Tool handlers: `async (args) => ({content})` or `{content, is_error:
// true}` — a throw becomes `{content: <err>, is_error: true}`. Event
// handlers return effect objects; replies from multiple modules merge
// the way hook aggregation does: `extra_context` concatenates, while
// `block` / `updatedInput` / `permissionDecision` are last-non-null-wins.

import fs from "node:fs";
import path from "node:path";
import readline from "node:readline";
import { pathToFileURL } from "node:url";

const warn = (msg) => process.stderr.write(`[extension-host] ${msg}\n`);

// --- registration state ---------------------------------------------------
// One pool across all modules: tools keyed by name (a later registration
// overwrites — same "last wins" rule as merged config), event subscribers
// kept in load order so reply merging is deterministic.

const tools = new Map(); // name -> {description, input_schema, handler}
const subscribers = new Map(); // event -> [handler, ...] in load order
const modules = []; // {file, ns} — kept for dispose()

function makeApi(modName) {
  return {
    registerTool(spec) {
      if (!spec || typeof spec.name !== "string" || typeof spec.handler !== "function") {
        warn(`${modName}: registerTool needs {name, handler} — skipped`);
        return;
      }
      tools.set(spec.name, spec);
    },
    on(event, handler) {
      if (typeof event !== "string" || typeof handler !== "function") {
        warn(`${modName}: api.on needs (eventName, handlerFn) — skipped`);
        return;
      }
      if (!subscribers.has(event)) subscribers.set(event, []);
      subscribers.get(event).push(handler);
    },
    log(...args) {
      warn(
        `${modName}: ` +
          args.map((a) => (typeof a === "string" ? a : JSON.stringify(a))).join(" "),
      );
    },
  };
}

// --- module loading -------------------------------------------------------
// Runs BEFORE answering ext/initialize: capabilities.tools / .events are
// derived from these registrations, so the module graph must be settled
// first. One bad module never takes the bridge down — same degrade rule
// as the Rust host: warn, skip, keep serving the rest.

async function loadModules(dir) {
  let entries;
  try {
    entries = fs.readdirSync(dir, { withFileTypes: true });
  } catch (e) {
    warn(`cannot scan ${dir}: ${e.message} — starting empty`);
    return;
  }
  // sorted = deterministic load order = deterministic merge order
  entries.sort((a, b) => a.name.localeCompare(b.name));
  for (const ent of entries) {
    if (!ent.isFile()) continue;
    if (ent.name.endsWith(".ts")) {
      // Node's type-stripping covers only erasable syntax — a .ts that
      // needs real transforms would half-load. Warn-and-skip is the
      // honest failure; ship compiled .mjs/.js.
      warn(`${ent.name}: .ts files are skipped (no toolchain — ship .mjs/.js)`);
      continue;
    }
    if (!/\.(mjs|js)$/.test(ent.name)) continue;
    const file = path.join(dir, ent.name);
    let ns;
    try {
      // pathToFileURL — a Windows path is not a valid import specifier
      ns = await import(pathToFileURL(file).href);
    } catch (e) {
      warn(`${ent.name}: import failed: ${e.message} — skipped`);
      continue;
    }
    if (typeof ns.default !== "function") {
      warn(`${ent.name}: no default export function — skipped`);
      continue;
    }
    try {
      await ns.default(makeApi(ent.name));
    } catch (e) {
      // registrations made before the throw still count — a partially
      // initialized module degrades, it doesn't veto the bridge.
      warn(`${ent.name}: default(api) threw: ${e.message}`);
    }
    modules.push({ file: ent.name, ns });
  }
}

// --- protocol --------------------------------------------------------------

const rl = readline.createInterface({ input: process.stdin });
const send = (frame) => process.stdout.write(JSON.stringify(frame) + "\n");
const reply = (id, result) => {
  if (id !== undefined) send({ jsonrpc: "2.0", id, result });
};
const replyError = (id, message) => {
  if (id !== undefined) send({ jsonrpc: "2.0", id, error: { code: -32000, message } });
};

// Fold every subscriber's reply into one — the merge semantics mirror
// hooks aggregation (apply_ext_reply on the Rust side): extra_context
// arrays concat, the effect scalars are last-non-null-wins.
async function fireEvent(event, payload) {
  const merged = {};
  for (const handler of subscribers.get(event) ?? []) {
    let out;
    try {
      out = await handler(payload);
    } catch (e) {
      warn(`ext/event ${event}: a handler threw: ${e.message}`);
      continue;
    }
    if (!out || typeof out !== "object") continue;
    const extra = out.extra_context;
    if (Array.isArray(extra)) {
      merged.extra_context = (merged.extra_context ?? []).concat(extra);
    } else if (typeof extra === "string") {
      merged.extra_context = (merged.extra_context ?? []).concat(extra);
    }
    for (const key of ["block", "updatedInput", "permissionDecision"]) {
      if (out[key] !== undefined && out[key] !== null) merged[key] = out[key];
    }
  }
  return merged;
}

async function handleLine(line) {
  let msg;
  try {
    msg = JSON.parse(line);
  } catch {
    return; // malformed frames are dropped, not answered
  }
  if (!msg || typeof msg !== "object") return;

  switch (msg.method) {
    case "ext/initialize":
      // capabilities.events is the subscription list — the kernel only
      // sends ext/event for names declared here.
      reply(msg.id, {
        name: "js-host",
        version: "0.1.0",
        capabilities: {
          tools: tools.size > 0,
          events: [...subscribers.keys()],
        },
      });
      break;

    case "ext/tools/list":
      // input_schema passes through verbatim — a malformed schema means
      // the tool doesn't exist to the LLM (same rule as native tools).
      reply(msg.id, {
        tools: [...tools.entries()].map(([name, t]) => ({
          name,
          description: t.description ?? "",
          input_schema: t.input_schema ?? { type: "object" },
        })),
      });
      break;

    case "ext/tools/call": {
      // the wire name is the extension-side name — the kernel strips
      // its ext__{plugin}__ namespace before dispatching.
      const name = msg.params?.name;
      const tool = tools.get(name);
      if (!tool) {
        replyError(msg.id, `unknown tool: ${name}`);
        break;
      }
      try {
        const out = await tool.handler(msg.params?.arguments ?? {});
        reply(
          msg.id,
          out && typeof out === "object"
            ? { content: out.content ?? "", ...(out.is_error ? { is_error: true } : {}) }
            : { content: out == null ? "" : String(out) },
        );
      } catch (e) {
        // a throwing handler is a failed ToolResult, never a dead bridge
        reply(msg.id, { content: String(e?.message ?? e), is_error: true });
      }
      break;
    }

    case "ext/event": {
      const { event, payload } = msg.params ?? {};
      reply(msg.id, await fireEvent(event, payload));
      break;
    }

    case "ext/shutdown":
      // notification — no reply. The serialized queue means every
      // earlier request already got its reply by the time we run.
      // Dispose, then exit well inside the kernel's 2s grace.
      dying = true;
      rl.close();
      for (const m of modules) {
        try {
          await m.ns.dispose?.();
        } catch (e) {
          warn(`${m.file}: dispose threw: ${e.message}`);
        }
      }
      process.exit(0);
      break;

    default:
      // unknown notifications are ignorable; unknown requests deserve an error
      if (msg.id !== undefined) replyError(msg.id, "method not found");
  }
}

// --- startup ----------------------------------------------------------------
// The kernel sends ext/initialize FIRST — modules must be fully loaded
// before we can answer it, so early frames queue behind module load.
// All frames then run through ONE serialized chain: handlers are async
// but replies must not race — ext/shutdown arriving behind an in-flight
// tool call would otherwise exit before that reply is written.

const extDir = process.argv[2];
if (!extDir) {
  warn("usage: node extension-host.mjs <extensions-dir>");
  process.exit(1);
}

const queue = [];
let loaded = false;
let draining = false;
let dying = false;

function drain() {
  if (draining) return;
  draining = true;
  (async () => {
    while (queue.length) {
      await handleLine(queue.shift()).catch((e) =>
        warn(`frame handling failed: ${e.message}`),
      );
    }
    draining = false;
  })();
}

rl.on("line", (line) => {
  queue.push(line);
  if (loaded) drain();
});
// stdin EOF without ext/shutdown = the kernel tore down abruptly — a
// sidecar with no peer is useless, so exit rather than linger. (rl.close
// is also called by our own shutdown path; `dying` lets that one finish.)
rl.on("close", () => {
  if (!dying) process.exit(0);
});

await loadModules(extDir);
loaded = true;
drain();
