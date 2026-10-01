//! Case-file parsing for `sunmao eval`. A case is a list of steps: the
//! flat `prompt`/`expect` shape desugars to one step; `steps: [{prompt,
//! expect}]` runs each prompt as its own turn on the SAME session —
//! per-step assertions see only that step's tool calls and its reply.

use anyhow::{Context as _, bail};
use serde_json::Value;

/// One eval case — `expect` vocabulary stays deliberately small (see
/// docs/CONFIG.md): final_contains / tool_called / tool_not_called /
/// max_tool_calls / turns.
#[derive(Debug)]
pub(super) struct Case {
    pub(super) name: String,
    pub(super) steps: Vec<Step>,
    pub(super) cwd: Option<String>,
}

#[derive(Debug)]
pub(super) struct Step {
    pub(super) prompt: String,
    pub(super) expect: Expect,
}

#[derive(Default, Debug)]
pub(super) struct Expect {
    pub(super) final_contains: Option<String>,
    pub(super) tool_called: Vec<String>,
    pub(super) tool_not_called: Vec<String>,
    pub(super) max_tool_calls: Option<u64>,
    pub(super) turns: Option<u64>,
}

/// Case file = one JSON object, a JSON array of objects, or JSONL (one
/// object per line; blank lines and `#`/`//` comments skipped). Whole-file
/// JSON wins so a single-line file still parses as one case.
pub(super) fn parse_cases(text: &str) -> anyhow::Result<Vec<Case>> {
    match serde_json::from_str::<Value>(text) {
        Ok(Value::Array(items)) => items
            .iter()
            .enumerate()
            .map(|(i, v)| case_from_value(v, &format!("case[{i}]")))
            .collect(),
        Ok(v) => Ok(vec![case_from_value(&v, "case")?]),
        Err(_) => {
            let mut out = Vec::new();
            for (i, line) in text.lines().enumerate() {
                let line = line.trim();
                if line.is_empty() || line.starts_with('#') || line.starts_with("//") {
                    continue;
                }
                let v: Value = serde_json::from_str(line)
                    .with_context(|| format!("line {}: not a JSON object", i + 1))?;
                out.push(case_from_value(&v, &format!("line {}", i + 1))?);
            }
            Ok(out)
        }
    }
}

fn case_from_value(v: &Value, at: &str) -> anyhow::Result<Case> {
    let obj = v
        .as_object()
        .with_context(|| format!("{at}: not an object"))?;
    let name = obj
        .get("name")
        .and_then(Value::as_str)
        .with_context(|| format!("{at}: missing \"name\""))?
        .to_string();
    let cwd = obj.get("cwd").and_then(Value::as_str).map(str::to_string);
    // `steps` is the multi-turn shape; the flat `prompt`+`expect` case is
    // a one-step case. Mixing them is refused — which step would the flat
    // expect pin to is a coin flip nobody should debug.
    let steps = match obj.get("steps") {
        Some(s) => {
            if obj.contains_key("prompt") || obj.contains_key("expect") {
                bail!("{at}: \"steps\" doesn't mix with flat \"prompt\"/\"expect\"");
            }
            s.as_array()
                .with_context(|| format!("{at}: \"steps\" is not an array"))?
                .iter()
                .enumerate()
                .map(|(i, s)| step_from_value(s, &format!("{at}.steps[{i}]")))
                .collect::<anyhow::Result<Vec<_>>>()?
        }
        None => vec![Step {
            prompt: obj
                .get("prompt")
                .and_then(Value::as_str)
                .with_context(|| format!("{at}: missing \"prompt\" (or \"steps\")"))?
                .to_string(),
            expect: match obj.get("expect") {
                None => Expect::default(),
                Some(e) => expect_from_value(e, at)?,
            },
        }],
    };
    if steps.is_empty() {
        bail!("{at}: \"steps\" is empty");
    }
    Ok(Case { name, steps, cwd })
}

fn step_from_value(v: &Value, at: &str) -> anyhow::Result<Step> {
    let obj = v
        .as_object()
        .with_context(|| format!("{at}: step is not an object"))?;
    Ok(Step {
        prompt: obj
            .get("prompt")
            .and_then(Value::as_str)
            .with_context(|| format!("{at}: missing \"prompt\""))?
            .to_string(),
        expect: match obj.get("expect") {
            None => Expect::default(),
            Some(e) => expect_from_value(e, at)?,
        },
    })
}

fn expect_from_value(v: &Value, at: &str) -> anyhow::Result<Expect> {
    let obj = v
        .as_object()
        .with_context(|| format!("{at}: \"expect\" is not an object"))?;
    let str_list = |key: &str| -> anyhow::Result<Vec<String>> {
        match obj.get(key) {
            None => Ok(Vec::new()),
            Some(v) => v
                .as_array()
                .with_context(|| format!("{at}: \"{key}\" is not an array"))?
                .iter()
                .map(|x| {
                    x.as_str()
                        .map(str::to_string)
                        .with_context(|| format!("{at}: \"{key}\" entry is not a string"))
                })
                .collect(),
        }
    };
    Ok(Expect {
        final_contains: obj
            .get("final_contains")
            .and_then(Value::as_str)
            .map(str::to_string),
        tool_called: str_list("tool_called")?,
        tool_not_called: str_list("tool_not_called")?,
        max_tool_calls: obj.get("max_tool_calls").and_then(Value::as_u64),
        turns: obj.get("turns").and_then(Value::as_u64),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_single_object() {
        let cases = parse_cases(r#"{"name":"a","prompt":"do x"}"#).unwrap();
        assert_eq!(cases.len(), 1);
        assert_eq!(cases[0].name, "a");
        assert_eq!(cases[0].steps[0].prompt, "do x");
        assert!(cases[0].cwd.is_none());
    }

    #[test]
    fn parses_multi_step_case() {
        let cases = parse_cases(
            r#"{"name":"two-turn","steps":[
                {"prompt":"write note.txt","expect":{"tool_called":["Write"]}},
                {"prompt":"now read it","expect":{"tool_called":["Read"],"final_contains":"ok"}}
            ]}"#,
        )
        .unwrap();
        assert_eq!(cases[0].steps.len(), 2);
        assert_eq!(cases[0].steps[1].prompt, "now read it");
        assert_eq!(cases[0].steps[0].expect.tool_called, ["Write"]);
    }

    #[test]
    fn steps_reject_flat_prompt_mix_and_empty() {
        let mixed =
            parse_cases(r#"{"name":"x","prompt":"p","steps":[{"prompt":"a"}]}"#).unwrap_err();
        assert!(mixed.to_string().contains("doesn't mix"), "{mixed}");
        let empty = parse_cases(r#"{"name":"x","steps":[]}"#).unwrap_err();
        assert!(empty.to_string().contains("empty"), "{empty}");
        // a step without prompt names its index
        let bad = parse_cases(r#"{"name":"x","steps":[{"expect":{}}]}"#).unwrap_err();
        assert!(bad.to_string().contains("steps[0]"), "{bad}");
    }

    #[test]
    fn parses_array() {
        let cases =
            parse_cases(r#"[{"name":"a","prompt":"x"},{"name":"b","prompt":"y","cwd":"f"}]"#)
                .unwrap();
        assert_eq!(cases.len(), 2);
        assert_eq!(cases[1].cwd.as_deref(), Some("f"));
    }

    #[test]
    fn parses_jsonl_skipping_blanks_and_comments() {
        let text = "# heading\n\n{\"name\":\"a\",\"prompt\":\"x\"}\n// c++ style\n{\"name\":\"b\",\"prompt\":\"y\"}\n";
        let cases = parse_cases(text).unwrap();
        assert_eq!(cases.len(), 2);
        assert_eq!(cases[0].name, "a");
        assert_eq!(cases[1].name, "b");
    }

    #[test]
    fn malformed_jsonl_names_the_line() {
        let err = parse_cases("{\"name\":\"a\",\"prompt\":\"x\"}\n{oops}\n").unwrap_err();
        assert!(err.to_string().contains("line 2"), "{err}");
    }

    #[test]
    fn malformed_array_element_names_the_index() {
        let err = parse_cases(r#"[{"name":"a","prompt":"x"},{"prompt":"y"}]"#).unwrap_err();
        assert!(err.to_string().contains("case[1]"), "{err}");
        assert!(err.to_string().contains("name"), "{err}");
    }

    #[test]
    fn full_case_shape_parses() {
        let text = r#"{
            "name": "uses-read-before-write",
            "prompt": "fix the typo in note.txt then tell me DONE",
            "cwd": "fixtures/case1",
            "expect": {
                "final_contains": "DONE",
                "tool_called": ["Read", "Write"],
                "tool_not_called": ["Bash"],
                "max_tool_calls": 10,
                "turns": 1
            }
        }"#;
        let cases = parse_cases(text).unwrap();
        let e = &cases[0].steps[0].expect;
        assert_eq!(e.final_contains.as_deref(), Some("DONE"));
        assert_eq!(e.tool_called, ["Read", "Write"]);
        assert_eq!(e.tool_not_called, ["Bash"]);
        assert_eq!(e.max_tool_calls, Some(10));
        assert_eq!(e.turns, Some(1));
    }
}
