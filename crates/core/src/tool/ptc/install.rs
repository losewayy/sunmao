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
        globalThis.tools = {{}};
        for (const n of {list}) {{
            tools[n] = (args) => __ptc("tool", JSON.stringify({{name: n, args: args ?? {{}}}}))
                .then(JSON.parse);
        }}
        globalThis.store = (key, value) => __ptc("store", JSON.stringify({{key, value}}))
            .then(JSON.parse);
        globalThis.load = (key) => __ptc("load", JSON.stringify({{key}}))
            .then((r) => JSON.parse(r).value);
        globalThis.describe = (name) => __ptc("describe", JSON.stringify({{name: name ?? null}}))
            .then(JSON.parse);
        "#,
    ))
}
