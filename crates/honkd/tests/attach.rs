mod common;

use common::{Honkd, Viewer};
use honk_testgoose::{StubGoose, Turn};
use serde_json::{Value, json};
use std::time::Duration;

fn new_session_params(cwd: &str) -> Value {
    json!({
        "cwd": cwd,
        "mcpServers": [],
        "_meta": {
            "enabledExtensions": [{
                "type": "platform",
                "name": "developer"
            }]
        }
    })
}

async fn new_session(v: &mut Viewer, cwd: &str) -> String {
    let res = v.call("session/new", new_session_params(cwd)).await;
    res["result"]["sessionId"]
        .as_str()
        .expect("sessionId string")
        .to_string()
}

fn pigeon_init() -> Value {
    json!({
        "protocolVersion": 1,
        "clientCapabilities": {
            "fs": {
                "readTextFile": false,
                "writeTextFile": false
            }
        }
    })
}

fn prompt_params(sid: &str, text: &str, client_req_id: Option<&str>) -> Value {
    let mut p = json!({
        "sessionId": sid,
        "prompt": [{
            "type": "text",
            "text": text,
        }]
    });
    if let Some(crid) = client_req_id {
        p["_meta"] = json!({
            "honk": {
                "client_req_id": crid
            }
        });
    }
    p
}

#[tokio::test(flavor = "multi_thread")]
async fn turn_survives_zero_viewers_and_late_viewer_gets_whole_transcript() {
    let Some(g) = StubGoose::start_or_skip().await else {
        return;
    };
    g.model().push([Turn::Gate(
        "g".into(),
        Box::new(Turn::Text(vec!["hello ".into(), "world".into()])),
    )]);

    let h = Honkd::spawn(&g.url(), g.secret_file(), &[]).await;
    let mut a = Viewer::connect(&h).await;
    let sid = new_session(&mut a, g.cwd().to_str().unwrap()).await;

    // Send prompt
    a.request("session/prompt", prompt_params(&sid, "hi", None))
        .await;

    // Wait until goose starts the run
    a.wait_for(|f| {
        f.pointer("/params/update/sessionUpdate")
            .and_then(Value::as_str)
            == Some("session_info_update")
            && f.pointer("/params/update/_meta/goose/activeRunId")
                .and_then(Value::as_str)
                .is_some()
    })
    .await;

    // Drop viewer a -> zero viewers attached now!
    drop(a);

    // Open gate so turn completes
    g.model().open("g");

    // Wait until turn finishes on model
    let start = std::time::Instant::now();
    while start.elapsed() < Duration::from_secs(10) {
        if g.model().turn_requests().len() == 1 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    tokio::time::sleep(Duration::from_millis(200)).await;

    // Connect viewer b late and load session
    let mut b = Viewer::connect(&h).await;
    let load_res = b
        .call(
            "session/load",
            json!({
                "sessionId": sid,
                "cwd": ".",
                "mcpServers": []
            }),
        )
        .await;

    assert!(
        load_res.get("result").is_some(),
        "load must succeed: {load_res:?}"
    );
    assert_eq!(
        load_res.pointer("/result/_meta/honk/epoch"),
        Some(&json!(1))
    );
    assert_eq!(b.blocks.agent_text(), "hello world");
}

#[tokio::test(flavor = "multi_thread")]
async fn two_viewers_converge() {
    let Some(g) = StubGoose::start_or_skip().await else {
        return;
    };
    g.model().push([
        Turn::ToolCall {
            tool: "shell".to_string(),
            arguments: json!({"command": "echo hi"}),
        },
        Turn::Gate("g".into(), Box::new(Turn::Text(vec!["done".to_string()]))),
    ]);

    let h = Honkd::spawn(&g.url(), g.secret_file(), &[]).await;
    let mut a = Viewer::connect(&h).await;
    let sid = new_session(&mut a, g.cwd().to_str().unwrap()).await;

    let p_id = a
        .request("session/prompt", prompt_params(&sid, "go", None))
        .await;

    // a waits until tool call appears
    a.wait_for(|f| {
        f.pointer("/params/update/sessionUpdate")
            .and_then(Value::as_str)
            == Some("tool_call")
    })
    .await;

    // b connects mid-turn and loads session
    let mut b = Viewer::connect(&h).await;
    let load_res = b
        .call(
            "session/load",
            json!({
                "sessionId": sid,
                "cwd": ".",
                "mcpServers": []
            }),
        )
        .await;
    assert!(
        load_res.get("result").is_some(),
        "load must succeed: {load_res:?}"
    );

    // Open gate so turn completes
    g.model().open("g");

    // a gets prompt response
    let a_resp = a.response(&p_id).await;
    assert_eq!(a_resp["result"]["stopReason"], "end_turn");

    // b waits until active run ends
    b.wait_for(|f| {
        f.pointer("/params/update/sessionUpdate")
            .and_then(Value::as_str)
            == Some("session_info_update")
            && f.pointer("/params/update/_meta/goose/activeRunId")
                .is_some_and(Value::is_null)
    })
    .await;

    assert_eq!(a.blocks.agent_text(), "done");
    assert_eq!(b.blocks.agent_text(), "done");
    assert_eq!(a.blocks.tool_status, b.blocks.tool_status);
}

#[tokio::test(flavor = "multi_thread")]
async fn cursor_resume_same_epoch_sends_only_delta() {
    let Some(g) = StubGoose::start_or_skip().await else {
        return;
    };
    g.model().push([
        Turn::Text(vec!["one".into()]),
        Turn::Text(vec!["two".into()]),
    ]);

    let h = Honkd::spawn(&g.url(), g.secret_file(), &[]).await;
    let mut a = Viewer::connect(&h).await;
    let sid = new_session(&mut a, g.cwd().to_str().unwrap()).await;

    // Turn 1
    let r1 = a
        .call("session/prompt", prompt_params(&sid, "p1", None))
        .await;
    assert_eq!(r1["result"]["stopReason"], "end_turn");
    assert_eq!(a.blocks.agent_text(), "one");
    let cur = a.blocks.cursor.clone().expect("cursor after turn 1");

    // Turn 2
    let r2 = a
        .call("session/prompt", prompt_params(&sid, "p2", None))
        .await;
    assert_eq!(r2["result"]["stopReason"], "end_turn");
    assert_eq!(a.blocks.agent_text(), "onetwo");

    // Connect viewer b with cursor from turn 1
    let mut b = Viewer::connect(&h).await;
    let load_res = b
        .call(
            "session/load",
            json!({
                "sessionId": sid,
                "cwd": ".",
                "mcpServers": [],
                "_meta": {
                    "honk": {
                        "after": cur
                    }
                }
            }),
        )
        .await;

    assert!(
        load_res.get("result").is_some(),
        "load must succeed: {load_res:?}"
    );
    assert_eq!(
        b.blocks.resets, 0,
        "cursor resume in same epoch must NOT reset"
    );
    assert_eq!(
        b.blocks.agent_text(),
        "two",
        "viewer b must only receive delta"
    );
    assert_eq!(
        b.blocks.user_texts(),
        vec!["p2".to_string()],
        "viewer b must only receive user text from delta"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn legacy_viewer_never_gets_reset_or_replay() {
    let Some(g) = StubGoose::start_or_skip().await else {
        return;
    };
    g.model().push([
        Turn::Text(vec!["turn one".into()]),
        Turn::Text(vec!["turn two".into()]),
    ]);

    let h = Honkd::spawn(&g.url(), g.secret_file(), &[]).await;
    let mut a = Viewer::connect(&h).await;
    let sid = new_session(&mut a, g.cwd().to_str().unwrap()).await;

    // Turn 1 with honk-aware viewer
    let r1 = a
        .call("session/prompt", prompt_params(&sid, "first", None))
        .await;
    assert_eq!(r1["result"]["stopReason"], "end_turn");

    // Connect legacy viewer b
    let mut b = Viewer::connect_pigeon(&h).await;
    b.call("initialize", pigeon_init()).await;

    // Legacy viewer sends prompt for turn 2 (auto-attaches)
    let r2 = b
        .call("session/prompt", prompt_params(&sid, "second", None))
        .await;
    assert_eq!(r2["result"]["stopReason"], "end_turn");
    assert_eq!(b.accumulated_text(), "turn two");

    // Legacy viewer must never receive any _honk/* frame, never replay of turn 1, never SynthUser
    for frame in &b.blocks.order {
        if let honk_wire::BlockKey::Text(_) = frame {
            let text = b.blocks.text.get(frame).unwrap();
            assert!(
                !text.contains("turn one"),
                "legacy viewer must not get replay from before attach"
            );
        }
    }
    assert_eq!(
        b.blocks.resets, 0,
        "legacy viewer must never receive _honk/reset"
    );
    for m in &b.raw_methods {
        assert!(
            !m.starts_with("_honk/"),
            "legacy viewer received _honk method: {m}"
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn slow_viewer_evicted_with_4001_without_stalling_turn() {
    let Some(g) = StubGoose::start_or_skip().await else {
        return;
    };
    let chunk_512 = "a".repeat(512);
    let chunks = vec![chunk_512; 20_000];
    g.model().push([Turn::Text(chunks)]);

    let h = Honkd::spawn(&g.url(), g.secret_file(), &["--viewer-queue", "8"]).await;

    let mut fast = Viewer::connect(&h).await;
    let sid = new_session(&mut fast, g.cwd().to_str().unwrap()).await;

    let mut slow = Viewer::connect(&h).await;
    let load_res = slow
        .call(
            "session/load",
            json!({
                "sessionId": sid,
                "cwd": ".",
                "mcpServers": []
            }),
        )
        .await;
    assert!(
        load_res.get("result").is_some(),
        "load must succeed: {load_res:?}"
    );

    // fast viewer sends prompt. slow viewer stops reading!
    let prompt_id = fast
        .request("session/prompt", prompt_params(&sid, "stream", None))
        .await;

    let resp = fast
        .response_timeout(&prompt_id, Duration::from_secs(30))
        .await;
    assert_eq!(resp["result"]["stopReason"], "end_turn");
    assert_eq!(fast.accumulated_text().len(), 20_000 * 512);

    // Read from slow viewer until closed
    let start = std::time::Instant::now();
    while start.elapsed() < Duration::from_secs(15) {
        if slow.recv_frame().await.is_none() {
            break;
        }
    }
    assert_eq!(
        slow.closed,
        Some(4001),
        "slow viewer must be evicted with 4001"
    );

    // Verify log file holds all 20 000 chunks
    let log_path = h
        .state_dir
        .path()
        .join("sessions")
        .join(format!("{sid}.jsonl"));
    let log_content = std::fs::read_to_string(&log_path).expect("read log");
    let line_count = log_content.lines().count();
    assert!(
        line_count >= 20_000,
        "log file must hold all frames, found {line_count} lines"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn log_file_holds_every_frame_with_event_id() {
    let Some(g) = StubGoose::start_or_skip().await else {
        return;
    };
    g.model().push([Turn::Text(vec!["hello".into()])]);

    let h = Honkd::spawn(&g.url(), g.secret_file(), &[]).await;
    let mut v = Viewer::connect(&h).await;
    let sid = new_session(&mut v, g.cwd().to_str().unwrap()).await;

    let r = v
        .call("session/prompt", prompt_params(&sid, "hi", None))
        .await;
    assert_eq!(r["result"]["stopReason"], "end_turn");

    let log_path = h
        .state_dir
        .path()
        .join("sessions")
        .join(format!("{sid}.jsonl"));
    let log_content = std::fs::read_to_string(&log_path).expect("read log");
    let lines: Vec<&str> = log_content.lines().collect();

    assert_eq!(
        lines.len(),
        10,
        "exact line count: header + 8 frames + 1 processed response"
    );
    // Line 0 is header
    let header: Value = serde_json::from_str(lines[0]).expect("parse header");
    assert_eq!(header["epoch"], 1);

    // Remaining lines must have ascending event ids
    let mut last_id = 0u64;
    for (idx, line) in lines[1..].iter().enumerate() {
        let parsed: Value = serde_json::from_str(line)
            .unwrap_or_else(|e| panic!("invalid json on line {}: {e}", idx + 2));
        if let Some(id) = parsed.get("id").and_then(Value::as_u64) {
            assert!(
                id >= last_id,
                "event ids must be ascending: {id} < {last_id}"
            );
            last_id = id;
        }
        if let Some(frame) = parsed.get("frame") {
            assert_eq!(frame["jsonrpc"], "2.0");
            let honk_meta = frame
                .pointer("/params/_meta/honk")
                .expect("honk meta in frame");
            assert_eq!(honk_meta["epoch"], 1);
            assert!(honk_meta["seq"].as_u64().is_some());
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn attach_sends_replay_batch_before_live_frames_fanned_out() {
    let fake = honk_testgoose::FakeGoose::start().await;
    fake.advertise_patched(true);

    let h = Honkd::spawn(&fake.url(), fake.secret_file(), &[]).await;
    let mut v1 = Viewer::connect(&h).await;
    let sid = new_session(&mut v1, ".").await;

    // Send a prompt to generate some log frames
    fake.script_prompt(vec!["history 1".into(), "history 2".into()]);
    let pid = v1
        .request("session/prompt", prompt_params(&sid, "test", None))
        .await;
    let _ = v1.response_timeout(&pid, Duration::from_secs(5)).await;

    // Now connect a new viewer v2. v2 connects and issues session/load.
    let mut v2 = Viewer::connect(&h).await;
    v2.call(
        "session/load",
        json!({"sessionId": sid, "cwd": ".", "mcpServers": []}),
    )
    .await;
    v2.wait_for(|f| f.get("method").and_then(Value::as_str) == Some("_honk/replay_done"))
        .await;

    // Verify raw methods received by v2:
    // First must be _honk/reset, followed by session/update history frames, then session/load response, then _honk/replay_done.
    let reset_idx = v2.raw_methods.iter().position(|m| m == "_honk/reset");
    let done_idx = v2.raw_methods.iter().position(|m| m == "_honk/replay_done");
    assert!(reset_idx.is_some(), "must have received _honk/reset");
    assert!(done_idx.is_some(), "must have received _honk/replay_done");
    assert!(
        reset_idx.unwrap() < done_idx.unwrap(),
        "_honk/reset must precede _honk/replay_done"
    );
}
