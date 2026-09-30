// wordcount.mjs — a JS extension module for the extension-host sidecar.
// The host calls this default export once at startup with an `api`.
// Return values of event handlers become the ext/event reply effects:
// {extra_context:[..]}, {block:"reason"}, {updatedInput:{..}},
// {permissionDecision:"deny"|"ask"|"allow"}.

export default function (api) {
  api.registerTool({
    name: "wordcount",
    description: "Count the words in the `text` argument",
    input_schema: {
      type: "object",
      properties: { text: { type: "string" } },
      required: ["text"],
    },
    handler: async (args) => ({
      content: String(
        String(args?.text ?? "")
          .split(/\s+/)
          .filter(Boolean).length,
      ),
    }),
  });

  api.on("SessionStart", (payload) => {
    // payload is the same dialect hooks get: session_id,
    // transcript_path, cwd, hook_event_name, source, …
    return { extra_context: [`js-extension loaded (cwd: ${payload.cwd ?? "?"})`] };
  });

  api.log("anything"); // stderr — stdout is protocol-only
}

// optional — runs when the kernel sends ext/shutdown
export function dispose() {
  process.stderr.write("[wordcount] dispose\n");
}
