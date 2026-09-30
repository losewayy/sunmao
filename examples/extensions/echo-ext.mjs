// echo-ext.mjs — a reference `sunmao` extension in ~50 lines.
//
// Wire: one JSON-RPC 2.0 object per line on stdin/stdout. Replies carry
// the request's `id`; notifications (ext/shutdown) have none. Write ONLY
// protocol frames to stdout — log to stderr if you need noise.
//
// Protocol summary (docs/PROTOCOLS.md "Extension protocol"):
//   ext/initialize → reply {name, version, capabilities:{tools, events}}
//   ext/tools/list → reply {tools:[{name, description, input_schema}]}
//   ext/tools/call → params {name, arguments}; reply {content, is_error?}
//   ext/event      → params {event, payload}; reply may carry
//                    {extra_context:[..], block:"reason", updatedInput:{..}}
//   ext/shutdown   → notification; exit afterwards

import readline from "node:readline";

const rl = readline.createInterface({ input: process.stdin });
const send = (frame) => process.stdout.write(JSON.stringify(frame) + "\n");
const reply = (id, result) => send({ jsonrpc: "2.0", id, result });
const replyError = (id, message) =>
  send({ jsonrpc: "2.0", id, error: { code: -32000, message } });

rl.on("line", (line) => {
  let msg;
  try {
    msg = JSON.parse(line);
  } catch {
    return; // malformed frames are dropped, not answered
  }

  switch (msg.method) {
    case "ext/initialize":
      // capabilities.events is the subscription list — the host only
      // sends ext/event for names declared here.
      reply(msg.id, {
        name: "echo",
        version: "0.1.0",
        capabilities: { tools: true, events: ["SessionStart"] },
      });
      break;

    case "ext/tools/list":
      // tool schemas pass through verbatim to the model — a malformed
      // input_schema means the tool does not exist to the LLM.
      reply(msg.id, {
        tools: [
          {
            name: "ping",
            description: "Echo the `msg` argument back",
            input_schema: {
              type: "object",
              properties: { msg: { type: "string" } },
              required: ["msg"],
            },
          },
        ],
      });
      break;

    case "ext/tools/call":
      // surfaces to the model as ext__echo__ping — the wire name here is
      // the extension-side name without the ext__ prefix.
      if (msg.params?.name === "ping") {
        const m = msg.params?.arguments?.msg ?? "";
        reply(msg.id, { content: `pong: ${m}` });
      } else {
        replyError(msg.id, `unknown tool: ${msg.params?.name}`);
      }
      break;

    case "ext/event": {
      // payload is the same dialect hooks get on stdin — session_id,
      // transcript_path, cwd, hook_event_name, source, tool_name, …
      const { event } = msg.params ?? {};
      if (event === "SessionStart") {
        reply(msg.id, { extra_context: ["echo-ext warm"] });
      } else {
        reply(msg.id, {});
      }
      break;
    }

    case "ext/shutdown":
      // notification — no reply; exit so the host's 2s grace never trips.
      process.exit(0);
      break;

    default:
      if (msg.id !== undefined) replyError(msg.id, "method not found");
  }
});
