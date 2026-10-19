use common::{Honkd, Viewer, new_session, prompt_params};
use honk_testgoose::{StubGoose, Turn};
use serde_json::Value;

mod common;

fn read_reply_sidecar(h: &Honkd, sid: &str) -> Vec<Value> {
    let path = h
        .state_dir
        .path()
        .join("sessions")
        .join(format!("{sid}.replies.jsonl"));
    let content = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("failed to read replies.jsonl at {}: {e}", path.display()));
    content
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l).expect("parse jsonl line"))
        .collect()
}

async fn goose_has_self_compact(v: &mut Viewer, sid: &str) -> bool {
    let tools_res = v
        .call(
            "_goose/unstable/tools/list",
            serde_json::json!({ "sessionId": sid }),
        )
        .await;
    assert!(
        tools_res.get("error").is_none(),
        "_goose/unstable/tools/list RPC error: {tools_res}"
    );
    let tools = tools_res
        .pointer("/result/tools")
        .and_then(Value::as_array)
        .unwrap_or_else(|| panic!("expected tools array in response, got: {tools_res}"));
    let has_tool = tools
        .iter()
        .any(|t| t.get("name").and_then(Value::as_str) == Some("self_compact_and_resume"));
    if !has_tool
        && std::env::var("HONK_REQUIRE_SELF_COMPACT")
            .map(|v| v == "1")
            .unwrap_or(false)
    {
        panic!(
            "self_compact_and_resume tool required by HONK_REQUIRE_SELF_COMPACT=1 but not offered"
        );
    }
    has_tool
}

#[tokio::test(flavor = "multi_thread")]
async fn self_compact_and_resume_e2e() {
    let Some(g) = StubGoose::start_or_skip().await else {
        return;
    };

    let h = Honkd::spawn(&g.url(), g.secret_file(), &[]).await;
    let mut v = Viewer::connect(&h).await;
    let sid = new_session(&mut v, g.cwd().to_str().unwrap()).await;

    if !goose_has_self_compact(&mut v, &sid).await {
        eprintln!("skipping test: self_compact_and_resume tool not offered by goose under test");
        return;
    }

    // Script:
    // Turn 1: tool call self_compact_and_resume with {"prompt":"R"}
    // Auxiliary: summarizer call during compaction (StubModel answers with "ok")
    // Turn 2: assistant reply after compaction resume
    g.model().push([
        Turn::ToolCall {
            tool: "self_compact_and_resume".into(),
            arguments: serde_json::json!({ "prompt": "R" }),
        },
        Turn::Text(vec!["after".into()]),
    ]);

    let r1 = v
        .call("session/prompt", prompt_params(&sid, "P1", None))
        .await;

    // Wait until run completes (activeRunId: null)
    v.wait_for(|f| {
        f.pointer("/params/update/sessionUpdate")
            .and_then(Value::as_str)
            == Some("session_info_update")
            && f.pointer("/params/update/_meta/goose/activeRunId") == Some(&serde_json::Value::Null)
    })
    .await;

    // Assert: prompt reply stopReason end_turn
    assert_eq!(r1["result"]["stopReason"], "end_turn");

    // Assert: _meta.honk.text receipt includes "after"
    let receipt1 = r1["result"]["_meta"]["honk"]["text"]
        .as_str()
        .expect("receipt text");
    assert!(
        receipt1.contains("after"),
        "receipt text must contain 'after', got: {receipt1}"
    );

    // Assert: exactly one run (one activeRunId, no idle between)
    let run_ids: Vec<Option<String>> = v
        .all_frames
        .iter()
        .filter_map(|f| {
            let u = f.pointer("/params/update")?;
            if u.get("sessionUpdate").and_then(Value::as_str) != Some("session_info_update") {
                return None;
            }
            let goose = u.pointer("/_meta/goose")?;
            let run_val = goose.get("activeRunId")?;
            if run_val.is_null() {
                Some(None)
            } else {
                Some(run_val.as_str().map(|s| s.to_string()))
            }
        })
        .collect();

    assert!(
        !run_ids.is_empty(),
        "expected at least one session_info_update with activeRunId"
    );
    let initial_run_id = run_ids[0]
        .as_ref()
        .expect("first activeRunId must be non-null")
        .clone();

    // All updates before the last must have the same activeRunId (no idle null between)
    for (i, run_id_opt) in run_ids[..run_ids.len() - 1].iter().enumerate() {
        assert_eq!(
            run_id_opt.as_ref(),
            Some(&initial_run_id),
            "update {i} had unexpected activeRunId (expected single run {initial_run_id})"
        );
    }
    assert_eq!(
        run_ids.last().unwrap(),
        &None,
        "final update must transition to activeRunId: null"
    );

    // Assert: honk-aware viewer sees the resume as a user_message_chunk with steer meta
    let resume_chunk = v.all_frames.iter().find(|f| {
        let Some(u) = f.pointer("/params/update") else {
            return false;
        };
        if u.get("sessionUpdate").and_then(Value::as_str) != Some("user_message_chunk") {
            return false;
        }
        let is_steer = u
            .pointer("/_meta/goose/steer")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let text = u.pointer("/content/text").and_then(Value::as_str);
        is_steer && text == Some("R")
    });
    assert!(
        resume_chunk.is_some(),
        "expected user_message_chunk with _meta.goose.steer == true and text 'R'"
    );

    // Assert: second client prompt gets a receipt equal to its export turn text
    // (proves honkd's prompt counter did not drift)
    g.model().push([Turn::Text(vec!["second reply".into()])]);
    let r2 = v
        .call("session/prompt", prompt_params(&sid, "second", None))
        .await;

    v.wait_for(|f| {
        f.pointer("/params/update/sessionUpdate")
            .and_then(Value::as_str)
            == Some("session_info_update")
            && f.pointer("/params/update/_meta/goose/activeRunId") == Some(&serde_json::Value::Null)
    })
    .await;

    assert_eq!(r2["result"]["stopReason"], "end_turn");
    let receipt2 = r2["result"]["_meta"]["honk"]["text"]
        .as_str()
        .expect("receipt2 text");
    assert_eq!(receipt2, "second reply");

    // Sidecar prompt_index assertions:
    // P1 prompt_index = 0
    // second prompt prompt_index = 1
    let sidecar_entries = read_reply_sidecar(&h, &sid);
    assert_eq!(
        sidecar_entries.len(),
        2,
        "expected exactly 2 prompt replies in sidecar, got: {sidecar_entries:?}"
    );
    assert_eq!(
        sidecar_entries[0]["prompt_index"], 0,
        "first prompt sidecar prompt_index must be 0"
    );
    assert_eq!(
        sidecar_entries[1]["prompt_index"], 1,
        "second prompt sidecar prompt_index must be 1 (proving prompt counter did not drift)"
    );

    let export = g.export(&sid).await;
    let export_turn1_text = honk_wire::export::turn_text_at_index(&export, 1, "second")
        .expect("export turn text at index 1");
    assert_eq!(
        receipt2, export_turn1_text,
        "second prompt receipt must equal export turn text at index 1"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn recipe_session_does_not_offer_self_compact() {
    let Some(g) = StubGoose::start_or_skip().await else {
        return;
    };

    let h = Honkd::spawn(&g.url(), g.secret_file(), &[]).await;
    let mut v = Viewer::connect(&h).await;
    let sid_res = v
        .call(
            "session/new",
            serde_json::json!({
                "cwd": g.cwd().to_str().unwrap(),
                "mcpServers": [],
                "_meta": {
                    "enabledExtensions": []
                }
            }),
        )
        .await;
    let sid = sid_res["result"]["sessionId"]
        .as_str()
        .expect("sessionId")
        .to_string();

    // Verify capability probe: on plain session without recipe, does goose support self-compact?
    // In this zero-extension session, plain tools would only be [self_compact_and_resume].
    let initial_tools_res = v
        .call(
            "_goose/unstable/tools/list",
            serde_json::json!({ "sessionId": sid }),
        )
        .await;
    assert!(
        initial_tools_res.get("error").is_none(),
        "_goose/unstable/tools/list RPC error: {initial_tools_res}"
    );
    let initial_tools = initial_tools_res
        .pointer("/result/tools")
        .and_then(Value::as_array)
        .unwrap_or_else(|| panic!("expected tools array in response, got: {initial_tools_res}"));
    let has_self_compact = initial_tools
        .iter()
        .any(|t| t.get("name").and_then(Value::as_str) == Some("self_compact_and_resume"));
    if !has_self_compact {
        if std::env::var("HONK_REQUIRE_SELF_COMPACT")
            .map(|v| v == "1")
            .unwrap_or(false)
        {
            panic!(
                "self_compact_and_resume tool required by HONK_REQUIRE_SELF_COMPACT=1 but not offered"
            );
        }
        eprintln!("skipping test: self_compact_and_resume tool not offered by goose under test");
        return;
    }

    let recipe = serde_json::json!({
        "title": "Structured Output Recipe",
        "description": "Recipe with schema",
        "instructions": "Follow instructions",
        "response": {
            "json_schema": {
                "type": "object",
                "properties": {
                    "summary": { "type": "string" }
                },
                "required": ["summary"]
            }
        }
    });

    let set_res = v
        .call(
            "_goose/unstable/session/settings/set",
            serde_json::json!({
                "sessionId": sid,
                "recipe": recipe,
            }),
        )
        .await;
    assert!(
        set_res.get("error").is_none(),
        "_goose/unstable/session/settings/set failed: {set_res}"
    );

    let tools_res = v
        .call(
            "_goose/unstable/tools/list",
            serde_json::json!({ "sessionId": sid }),
        )
        .await;
    assert!(
        tools_res.get("error").is_none(),
        "_goose/unstable/tools/list RPC error: {tools_res}"
    );
    let tools = tools_res
        .pointer("/result/tools")
        .and_then(Value::as_array)
        .unwrap_or_else(|| panic!("expected tools array in response, got: {tools_res}"));
    let tool_names: Vec<&str> = tools
        .iter()
        .filter_map(|t| t.get("name").and_then(Value::as_str))
        .collect();
    assert_eq!(
        tool_names,
        vec!["recipe__final_output"],
        "recipe session with response schema should list exactly [recipe__final_output]"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn manual_compact_over_honkd_e2e() {
    let Some(g) = StubGoose::start_or_skip().await else {
        return;
    };

    let h = Honkd::spawn(&g.url(), g.secret_file(), &[]).await;
    let mut v = Viewer::connect(&h).await;
    let sid = new_session(&mut v, g.cwd().to_str().unwrap()).await;

    // Turn 1: P1
    g.model().push([Turn::Text(vec!["p1 reply".into()])]);
    let r1 = v
        .call("session/prompt", prompt_params(&sid, "P1", None))
        .await;

    v.wait_for(|f| {
        f.pointer("/params/update/sessionUpdate")
            .and_then(Value::as_str)
            == Some("session_info_update")
            && f.pointer("/params/update/_meta/goose/activeRunId") == Some(&serde_json::Value::Null)
    })
    .await;

    assert_eq!(r1["result"]["stopReason"], "end_turn");
    assert_eq!(r1["result"]["_meta"]["honk"]["text"], "p1 reply");

    // Turn 2: "/compact"
    // Goose executes built-in compaction command without invoking a model turn;
    // summarization auxiliary call is answered by StubModel with "ok".
    let r_compact = v
        .call("session/prompt", prompt_params(&sid, "/compact", None))
        .await;

    v.wait_for(|f| {
        f.pointer("/params/update/sessionUpdate")
            .and_then(Value::as_str)
            == Some("session_info_update")
            && f.pointer("/params/update/_meta/goose/activeRunId") == Some(&serde_json::Value::Null)
    })
    .await;

    assert_eq!(r_compact["result"]["stopReason"], "end_turn");
    let receipt_compact = r_compact["result"]["_meta"]["honk"]["text"]
        .as_str()
        .expect("receipt text for /compact");
    assert_eq!(receipt_compact, "Compaction complete");

    // Turn 3: P2
    g.model().push([Turn::Text(vec!["p2 reply".into()])]);
    let r2 = v
        .call("session/prompt", prompt_params(&sid, "P2", None))
        .await;

    v.wait_for(|f| {
        f.pointer("/params/update/sessionUpdate")
            .and_then(Value::as_str)
            == Some("session_info_update")
            && f.pointer("/params/update/_meta/goose/activeRunId") == Some(&serde_json::Value::Null)
    })
    .await;

    assert_eq!(r2["result"]["stopReason"], "end_turn");
    let receipt2 = r2["result"]["_meta"]["honk"]["text"]
        .as_str()
        .expect("receipt text for P2");
    assert_eq!(receipt2, "p2 reply");

    // Sidecar prompt_index assertions:
    // P1 prompt_index = 0
    // /compact prompt_index = 1
    // P2 prompt_index = 2
    let sidecar_entries = read_reply_sidecar(&h, &sid);
    assert_eq!(
        sidecar_entries.len(),
        3,
        "expected exactly 3 prompt replies in sidecar, got: {sidecar_entries:?}"
    );
    assert_eq!(
        sidecar_entries[0]["prompt_index"], 0,
        "P1 sidecar prompt_index must be 0"
    );
    assert_eq!(
        sidecar_entries[1]["prompt_index"], 1,
        "/compact sidecar prompt_index must be 1"
    );
    assert_eq!(
        sidecar_entries[2]["prompt_index"], 2,
        "P2 sidecar prompt_index must be 2"
    );

    // P2 receipt equals export turn text
    let export = g.export(&sid).await;
    let export_turn2_text = honk_wire::export::turn_text_at_index(&export, 2, "P2")
        .expect("export turn text at index 2 for P2");
    assert_eq!(receipt2, export_turn2_text);
}
