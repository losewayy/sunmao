use super::*;
use sunmao_llm::types::Message;

#[tokio::test]
async fn fold_replays_messages_and_tool_results() {
    let mut log = SessionLog::ephemeral();
    log.append(&SessionEvent::Message {
        message: Message::user("hi"),
    })
    .await
    .unwrap();
    log.append(&SessionEvent::ToolResult {
        call_id: "c1".into(),
        name: "Read".into(),
        ok: true,
        output: "x".into(),
        depth: 0,
        lane: 0,
    })
    .await
    .unwrap();
    let msgs = log.messages().await.unwrap();
    assert_eq!(msgs.len(), 2);
    assert_eq!(msgs[1].tool_call_id.as_deref(), Some("c1"));
}

#[tokio::test]
async fn compacted_boundary_clears_prior_transcript() {
    let mut log = SessionLog::ephemeral();
    log.append(&SessionEvent::Message {
        message: Message::system("identity"),
    })
    .await
    .unwrap();
    log.append(&SessionEvent::Message {
        message: Message::user("old1"),
    })
    .await
    .unwrap();
    log.append(&SessionEvent::Message {
        message: Message::user("old2"),
    })
    .await
    .unwrap();
    log.append(&SessionEvent::Compacted {
        summary: "summary text".into(),
    })
    .await
    .unwrap();
    log.append(&SessionEvent::Message {
        message: Message::user("new"),
    })
    .await
    .unwrap();
    let msgs = log.messages().await.unwrap();
    // system prompt survives the fold — it's identity, not history
    assert_eq!(msgs.len(), 3);
    assert_eq!(msgs[0].role, sunmao_llm::types::Role::System);
    assert_eq!(msgs[0].content.as_deref(), Some("identity"));
    assert!(msgs[1].content.as_deref().unwrap().contains("summary text"));
    assert_eq!(msgs[2].content.as_deref(), Some("new"));
}

/// A crash mid-append strands a partial JSON fragment at EOF without a
/// newline — the next append must not glue its event onto that fragment
/// (which would corrupt BOTH). open_path seals the tail with '\n' so the
/// fragment stays one skippable bad line.
#[tokio::test]
async fn open_path_heals_crash_truncated_tail() {
    let dir = crate::fresh_test_dir("tail");
    let path = dir.join("t.jsonl");
    tokio::fs::create_dir_all(&dir).await.unwrap();
    {
        let ev = SessionEvent::Message {
            message: Message::user("first"),
        };
        let mut bytes = serde_json::to_vec(&ev).unwrap();
        bytes.push(b'\n');
        // crash cut the second write halfway — no newline
        bytes.extend_from_slice(b"{\"type\":\"message\",\"mes");
        tokio::fs::write(&path, &bytes).await.unwrap();
    }
    let mut log = SessionLog::open_path(&path).await.unwrap();
    log.append(&SessionEvent::Message {
        message: Message::user("second"),
    })
    .await
    .unwrap();
    drop(log);

    let log = SessionLog::open_path(&path).await.unwrap();
    let msgs = log.messages().await.unwrap();
    let texts: Vec<_> = msgs.iter().filter_map(|m| m.content.as_deref()).collect();
    assert_eq!(texts, vec!["first", "second"]);
    let _ = std::fs::remove_dir_all(&dir);
}

/// LocalShell folds into the message stream on BOTH paths — ephemeral
/// (in-mem) and file-backed (replayed from disk). The invariant list in
/// AGENTS.md holds them to identical fold semantics, so one test asserts
/// both.
#[tokio::test]
async fn local_shell_folds_into_messages_both_paths() {
    let ev = SessionEvent::LocalShell {
        command: "echo hi".into(),
        exit_code: 0,
        output: "hi".into(),
    };
    // ephemeral
    let mut log = SessionLog::ephemeral();
    log.append(&ev).await.unwrap();
    let msgs = log.messages().await.unwrap();
    assert_eq!(msgs.len(), 1);
    let c = msgs[0].content.as_deref().unwrap();
    assert!(c.contains("$ echo hi") && c.contains("[exit 0]"));

    // file-backed — same fold through the disk replay path. Unique dir
    // name: tests share a pid and run in parallel, a generic name here
    // once deleted a sibling test's fixture mid-assert.
    let dir = crate::fresh_test_dir("ls");
    let mut log = SessionLog::open(&dir, "ls-fold").await.unwrap();
    log.append(&ev).await.unwrap();
    drop(log);
    let log = SessionLog::open(&dir, "ls-fold").await.unwrap();
    let msgs = log.messages().await.unwrap();
    assert_eq!(msgs.len(), 1);
    assert_eq!(msgs[0].role, sunmao_llm::types::Role::User);
    let _ = std::fs::remove_dir_all(&dir);
}

/// Sub-agent depth survives the disk round-trip AND logs written before
/// the field existed still parse (serde default → depth 0). Frontends
/// replay depth>0 as ↳ blocks — losing it silently flattens transcripts.
#[tokio::test]
async fn tool_event_depth_roundtrips_and_defaults() {
    let dir = crate::fresh_test_dir("sess-depth");
    let call = ToolCall {
        id: "c1".into(),
        kind: "function".into(),
        function: sunmao_llm::types::FunctionCall {
            name: "Glob".into(),
            arguments: "{}".into(),
        },
    };
    let mut log = SessionLog::open(&dir, "d").await.unwrap();
    log.append(&SessionEvent::ToolCall {
        call,
        depth: 1,
        lane: 2,
    })
    .await
    .unwrap();
    // a pre-depth log line: same shape, no `depth`/`lane` keys
    let legacy = r#"{"type":"tool_result","call_id":"c1","name":"Glob","ok":true,"output":"x"}"#;
    {
        use tokio::io::AsyncWriteExt;
        let mut f = tokio::fs::OpenOptions::new()
            .append(true)
            .open(log.path())
            .await
            .unwrap();
        f.write_all(legacy.as_bytes()).await.unwrap();
        f.write_all(b"\n").await.unwrap();
    }
    drop(log);

    let log = SessionLog::open(&dir, "d").await.unwrap();
    let events = log.events().await.unwrap();
    assert!(matches!(
        &events[0],
        SessionEvent::ToolCall {
            depth: 1,
            lane: 2,
            ..
        }
    ));
    assert!(matches!(
        &events[1],
        SessionEvent::ToolResult {
            depth: 0,
            lane: 0,
            ..
        }
    ));
    let _ = std::fs::remove_dir_all(&dir);
}

/// A corrupt JSONL line must not brick every future turn — the fold
/// skips it and keeps the good events on both sides.
#[tokio::test]
async fn corrupt_line_is_skipped_not_fatal() {
    let dir = crate::fresh_test_dir("corrupt");
    let mut log = SessionLog::open(&dir, "c").await.unwrap();
    log.append(&SessionEvent::Message {
        message: Message::user("before"),
    })
    .await
    .unwrap();
    {
        use tokio::io::AsyncWriteExt;
        let mut f = tokio::fs::OpenOptions::new()
            .append(true)
            .open(log.path())
            .await
            .unwrap();
        f.write_all(b"{not json\n").await.unwrap();
    }
    log.append(&SessionEvent::Message {
        message: Message::user("after"),
    })
    .await
    .unwrap();
    drop(log);

    let log = SessionLog::open(&dir, "c").await.unwrap();
    let msgs = log.messages().await.unwrap();
    let texts: Vec<_> = msgs.iter().filter_map(|m| m.content.as_deref()).collect();
    assert_eq!(texts, vec!["before", "after"]);
    let _ = std::fs::remove_dir_all(&dir);
}

/// A crash-stranded assistant tool_call (no ToolResult event) would make
/// providers reject the whole transcript — the fold synthesizes an
/// interrupted result so resume-after-crash still works. Both paths.
#[tokio::test]
async fn dangling_tool_call_gets_interrupted_result() {
    let call = ToolCall {
        id: "c-dead".into(),
        kind: "function".into(),
        function: sunmao_llm::types::FunctionCall {
            name: "Bash".into(),
            arguments: "{}".into(),
        },
    };
    let evs = [
        SessionEvent::Message {
            message: Message::user("run it"),
        },
        SessionEvent::Message {
            message: Message::assistant(None, vec![call.clone()]),
        },
        // crash happens here — no ToolResult event
        SessionEvent::Message {
            message: Message::user("next prompt after resume"),
        },
    ];

    // ephemeral path
    let mut log = SessionLog::ephemeral();
    for e in &evs {
        log.append(e).await.unwrap();
    }
    let msgs = log.messages().await.unwrap();
    let result = msgs
        .iter()
        .find(|m| m.tool_call_id.as_deref() == Some("c-dead"))
        .expect("orphaned tool_call must gain a synthetic result");
    assert!(result.content.as_deref().unwrap().contains("interrupted"));
    // the repair lands before the following user message (adjacency)
    let idx = msgs
        .iter()
        .position(|m| m.tool_call_id.as_deref() == Some("c-dead"))
        .unwrap();
    assert_eq!(msgs[idx + 1].role, sunmao_llm::types::Role::User);

    // file-backed path
    let dir = crate::fresh_test_dir("dangle");
    let mut log = SessionLog::open(&dir, "d").await.unwrap();
    for e in &evs {
        log.append(e).await.unwrap();
    }
    drop(log);
    let log = SessionLog::open(&dir, "d").await.unwrap();
    let msgs = log.messages().await.unwrap();
    assert!(
        msgs.iter()
            .any(|m| m.tool_call_id.as_deref() == Some("c-dead")
                && m.content.as_deref().unwrap().contains("interrupted")),
        "file-backed fold must repair dangling calls too"
    );
    let _ = std::fs::remove_dir_all(&dir);
}
