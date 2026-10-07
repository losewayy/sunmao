//! The JS surface `RunCode` installs into the sandbox: `tools.<Name>(args)`
//! per callable tool plus the `store`/`load`/`describe` builtins — all thin
//! wrappers over `__ptc`, the single host channel. `RunCode` itself is
//! withheld: a sandboxed script must not spawn nested sandboxes.

use std::sync::Arc;

use serde_json::Value;

use super::PtcMsg;
use crate::context::Context;

pub(super) fn install<'js>(
    jctx: &rquickjs::Ctx<'js>,
    ctx: &Arc<Context>,
    tx: tokio::sync::mpsc::UnboundedSender<PtcMsg>,
) -> rquickjs::Result<()> {
    let globals = jctx.globals();
    globals.set(
        "__ptc",
        rquickjs::Function::new(
            jctx.clone(),
            rquickjs::prelude::Async(move |op: String, args: String| {
                let tx = tx.clone();
                async move {
                    let (rtx, rrx) = tokio::sync::oneshot::channel();
                    let args_v: Value = serde_json::from_str(&args).unwrap_or(Value::Null);
                    if tx.send((op, args_v, rtx)).is_err() {
                        return Err(rquickjs::Error::new_into_js_message(
                            "host",
                            "js",
                            "tool bridge is gone",
                        ));
                    }
                    match rrx.await {
                        Ok(Ok(out)) => Ok(out),
                        Ok(Err(e)) => Err(rquickjs::Error::new_into_js_message("host", "js", e)),
                        Err(_) => Err(rquickjs::Error::new_into_js_message(
                            "host",
                            "js",
                            "tool bridge dropped the reply",
                        )),
                    }
                }
            }),
        ),
    )?;
    let names: Vec<String> = super::callable_catalog(ctx)
        .into_iter()
        .map(|t| t.function.name)
        .collect();
    // Bracket-access assignments tolerate any tool name (mcp__x__y is a
    // valid identifier anyway, but `tools["..."]` needs no validation).
    let list = serde_json::to_string(&names).unwrap_or_else(|_| "[]".into());
    jctx.eval::<(), _>(format!(
        r#"
        // invocation ledger — `__ptc` is async (its send polls later), so
        // counting calls at invocation time is the only "did the script
        // reach a tool" signal the eval fallback can trust
        globalThis.__calls = 0;
        const __ptc_host = __ptc;
        globalThis.__ptc = (op, args) => {{ __calls++; return __ptc_host(op, args); }};
        globalThis.tools = {{}};
        for (const n of {list}) {{
            tools[n] = (args) => __ptc("tool", JSON.stringify({{name: n, args: args ?? {{}}}}))
                .then(JSON.parse);
        }}
        // store: JSON.stringify silently mangles values — Set/Map/Error/RegExp
        // become empty objects, NaN/Infinity → null, BigInt → throw, and
        // undefined fields vanish.
        // Surface every one of those as a `warn` on the reply instead of
        // letting the script trust a hollow object. BigInt gets a "42n"
        // string rather than a crash; cycles still throw (legible already).
        globalThis.store = (key, value) => {{
            const warn = [];
            if (value === undefined) warn.push("undefined isn't storable — the key stays unset");
            const payload = JSON.stringify({{key, value}}, (k, v) => {{
                const tag = Object.prototype.toString.call(v);
                if (tag === "[object Set]" || tag === "[object Map]" || tag === "[object Error]" || tag === "[object RegExp]")
                    warn.push(`${{k || "(root)"}}: ${{tag.slice(8, -1)}} serializes to {{}} — inner data is lost`);
                else if (typeof v === "number" && !Number.isFinite(v))
                    warn.push(`${{k || "(root)"}}: ${{v}} → null`);
                else if (typeof v === "bigint")
                    {{ warn.push(`${{k || "(root)"}}: BigInt → "${{v}}n" string`); return v.toString() + "n"; }}
                return v;
            }});
            return __ptc("store", payload).then(r => {{
                const o = JSON.parse(r);
                if (warn.length) o.warn = warn.join("; ");
                return o;
            }});
        }};
        // load: a key that was never set resolves to `undefined`, a stored
        // null resolves to `null` — the two cases used to collapse together
        globalThis.load = (key) => __ptc("load", JSON.stringify({{key}}))
            .then((r) => {{ const o = JSON.parse(r); return o.found ? o.value : undefined; }});
        globalThis.describe = (name) => __ptc("describe", JSON.stringify({{name: name ?? null}}))
            .then(JSON.parse);
        // console.log — pure in-sandbox collection: an extra host round
        // trip per line would pay the ~900ms bridge tax on every debug
        // print. __logs rides back with the script result; 200 lines cap
        // so a log-loop can't eat the memory limit by itself.
        globalThis.__logs = [];
        const __fmt = (v) => typeof v === "string" ? v : (v === undefined ? "undefined" : (() => {{ try {{ return JSON.stringify(v) }} catch {{ return String(v) }} }})());
        globalThis.console = {{}};
        for (const [m, tag] of [["log", ""], ["warn", "[warn] "], ["error", "[error] "]])
            console[m] = (...a) => {{ if (__logs.length < 200) __logs.push(tag + a.map(__fmt).join(" ")); }};
        "#,
    ))
}
