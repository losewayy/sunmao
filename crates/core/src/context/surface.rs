//! Advertised tool surface — which declarations the model sees per
//! request. This is a policy seam, not a registry query: the driver,
//! turn mode, catalog size and `promoted_tools` all reshape the answer.

use super::{Context, MutexRecover, RwLockRecover};

/// Tool-count budget before the advertised surface goes lazy: at or below
/// this every declaration ships each request (today's ~14 builtins never
/// trigger it); above it only `LAZY_HOT` + `SearchTools` + promoted tools
/// stay on the wire and the rest resolve through `SearchTools` on demand.
const LAZY_ADVERTISE_AT: usize = 20;
/// The always-advertised set once the surface goes lazy — the everyday
/// file/shell/plan tools plus `SearchTools` itself (the discovery path
/// must never be one of the deferred) and `RunCode` (deliberately part of
/// the standard surface too — catalog growth must not silently drop it,
/// which is exactly what happened to the under-threshold eager surface
/// revealing it by accident). Cold-but-promoted names rejoin per request
/// via `promoted_tools`.
const LAZY_HOT: &[&str] = &[
    "Read",
    "Write",
    "Edit",
    "Bash",
    "Glob",
    "Grep",
    "Task",
    "TodoWrite",
    "SearchTools",
    "RunCode",
];

impl Context {
    /// The declarations the next request advertises. Under `ptc` only
    /// `RunCode`+`SearchTools` are on the wire — every other registered
    /// tool stays callable through the script's `tools.*` bridge, which
    /// is the whole point of the driver: the schema budget holds two
    /// tools no matter how large the catalog is. `FusionExecute` and
    /// `SearchTools` are withheld under the other drivers — with every
    /// declaration already on the wire they're dead schema weight. That
    /// flips once the catalog outgrows [`LAZY_ADVERTISE_AT`]: the lazy
    /// surface keeps the hot set + `SearchTools` (and anything it
    /// promoted) and defers the rest, so a fat MCP roster stops costing
    /// a schema wall per request.
    pub fn advertised_tools(&self) -> Vec<sunmao_llm::types::Tool> {
        let decls = self.tools.declarations();
        if self.loop_driver == crate::agent::LoopDriver::Ptc {
            return decls
                .into_iter()
                .filter(|t| matches!(t.function.name.as_str(), "RunCode" | "SearchTools"))
                .collect();
        }
        // `SearchTools`/`FusionExecute` are withheld under the eager standard
        // surface — dead schema weight while every decl is already on the
        // wire. `RunCode` stays: it is a real capability the model can drive
        // (the ptc driver's whole loop), not a discovery crutch. The fusion
        // surface itself (armed Lead's read set, escalated Lead back to
        // standard-minus-delegate) is `fusion::lead_decl`'s call — the table
        // lives with the policy.
        if *self.turn_mode.read_or_recover() == crate::agent::TurnMode::Fusion {
            let escalated = self.fusion.lock_or_recover().escalated;
            return decls
                .into_iter()
                .filter(|t| crate::agent::fusion::lead_decl(&t.function.name, escalated))
                .collect();
        }
        if decls.len() > LAZY_ADVERTISE_AT {
            let promoted = self.promoted_tools.lock_or_recover().clone();
            return decls
                .into_iter()
                .filter(|t| {
                    (LAZY_HOT.contains(&t.function.name.as_str())
                        || promoted.contains(&t.function.name))
                        && t.function.name != "FusionExecute"
                })
                .collect();
        }
        decls
            .into_iter()
            .filter(|t| t.function.name != "SearchTools" && t.function.name != "FusionExecute")
            .collect()
    }
}
