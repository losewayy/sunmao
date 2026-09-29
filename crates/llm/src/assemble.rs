//! Reassembly of streamed `tool_calls`.
//!
//! Providers emit tool calls as fragments: `index` identifies the call slot,
//! `id`/`function.name` arrive on early fragments, `function.arguments` is a
//! JSON string streamed in arbitrary slices. This module concatenates the
//! arguments buffer per index and validates the result.

use std::collections::BTreeMap;

use anyhow::{bail, Context};

use crate::types::{FunctionCall, ToolCall};
use crate::ToolCallFragment;

#[derive(Default)]
struct PartialCall {
    id: String,
    name: String,
    args: String,
}

#[derive(Default)]
pub struct ToolCallAssembler {
    calls: BTreeMap<u32, PartialCall>,
}

impl ToolCallAssembler {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push(&mut self, frag: &ToolCallFragment) {
        let slot = self.calls.entry(frag.index).or_default();
        if let Some(id) = &frag.id {
            slot.id.push_str(id);
        }
        if let Some(name) = &frag.name {
            slot.name.push_str(name);
        }
        if let Some(args) = &frag.arguments {
            slot.args.push_str(args);
        }
    }

    /// Consume all fragments into finalized [`ToolCall`]s.
    ///
    /// The accumulated arguments buffer must be valid JSON — providers are not
    /// guaranteed to produce parseable arguments, and downstream executes
    /// whatever we emit, so this is a hard check.
    pub fn finish(self) -> anyhow::Result<Vec<ToolCall>> {
        let mut out = Vec::with_capacity(self.calls.len());
        for (index, call) in self.calls {
            if call.name.is_empty() {
                bail!("tool call {index}: missing function name");
            }
            // tolerate an empty arguments buffer as `{}` — providers omit it
            // for zero-arg functions
            let args = if call.args.trim().is_empty() {
                "{}".to_string()
            } else {
                serde_json::from_str::<serde_json::Value>(&call.args)
                    .with_context(|| {
                        format!("tool call {index} ({}) arguments not valid JSON", call.name)
                    })?;
                call.args
            };
            out.push(ToolCall {
                id: call.id,
                kind: "function".into(),
                function: FunctionCall {
                    name: call.name,
                    arguments: args,
                },
            });
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frag(index: u32, id: Option<&str>, name: Option<&str>, args: Option<&str>) -> ToolCallFragment {
        ToolCallFragment {
            index,
            id: id.map(String::from),
            name: name.map(String::from),
            arguments: args.map(String::from),
        }
    }

    #[test]
    fn reassembles_split_arguments() {
        let mut a = ToolCallAssembler::new();
        a.push(&frag(0, Some("call_1"), Some("Edit"), None));
        a.push(&frag(0, None, None, Some("{\"path\":\"x\"")));
        a.push(&frag(0, None, None, Some(",\"old\":\"a\",\"new\":\"b\"}")));
        a.push(&frag(1, Some("call_2"), Some("Bash"), Some("{\"cmd\":\"ls\"}")));
        let calls = a.finish().unwrap();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].id, "call_1");
        assert_eq!(calls[0].function.name, "Edit");
        let v: serde_json::Value = serde_json::from_str(&calls[0].function.arguments).unwrap();
        assert_eq!(v["new"], "b");
        assert_eq!(calls[1].function.name, "Bash");
    }

    #[test]
    fn rejects_invalid_json_arguments() {
        let mut a = ToolCallAssembler::new();
        a.push(&frag(0, Some("c"), Some("Bash"), Some("{\"cmd\":")));
        assert!(a.finish().is_err());
    }

    #[test]
    fn empty_arguments_become_empty_object() {
        let mut a = ToolCallAssembler::new();
        a.push(&frag(0, Some("c"), Some("TodoWrite"), None));
        let calls = a.finish().unwrap();
        assert_eq!(calls[0].function.arguments, "{}");
    }
}
