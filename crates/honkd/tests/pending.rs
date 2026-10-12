use std::time::Duration;

use honk_testgoose::{StubGoose, Turn};
use serde_json::{Value, json};

mod common;
use common::{Honkd, Viewer, new_session, prompt_params};

#[tokio::test]
async fn fan_out_to_two_viewers_first_answer_wins_other_gets_request_resolved() {
    let Some(goose) = StubGoose::start_or_skip().await else {
        return;
    };

    goose.model().push([
        Turn::ToolCall {
            tool: "shell".to_string(),
            arguments: json!({"command": "echo v1_wins"}),
        },
        Turn::Text(vec!["approved and done".to_string()]),
    ]);

    let honkd = Honkd::spawn(&goose.url(), goose.secret_file(), &[]).await;

    let mut v1 = Viewer::connect(&honkd).await;
    let sid = new_session(&mut v1, goose.cwd().to_str().unwrap()).await;

    let mode_res = v1
        .call(
            "session/set_mode",
            json!({"sessionId": &sid, "modeId": "approve"}),
        )
        .await;
    assert!(
        mode_res.get("error").is_none(),
        "set_mode approve succeeded: {mode_res:?}"
    );

    let mut v2 = Viewer::connect(&honkd).await;
    v2.call(
        "session/load",
        json!({"sessionId": &sid, "cwd": ".", "mcpServers": []}),
    )
    .await;

    let pid = v1
        .request(
            "session/prompt",
            prompt_params(&sid, "run echo in approve mode", None),
        )
        .await;

    let req1 = v1
        .wait_for(|f| {
            f.get("method").and_then(Value::as_str) == Some("request_permission")
                || f.get("method").and_then(Value::as_str) == Some("session/request_permission")
        })
        .await;
    let req2 = v2
        .wait_for(|f| {
            f.get("method").and_then(Value::as_str) == Some("request_permission")
                || f.get("method").and_then(Value::as_str) == Some("session/request_permission")
        })
        .await;

    assert_eq!(
        req1["id"], req2["id"],
        "both viewers get the request verbatim with goose's id"
    );
    let goose_req_id = req1["id"].clone();

    // v1 answers first
    v1.respond(
        goose_req_id.clone(),
        json!({
            "outcome": {
                "outcome": "selected",
                "optionId": "allow_once"
            }
        }),
    )
    .await;

    // v2 receives _honk/request_resolved
    let resolved = v2
        .wait_for(|f| f.get("method").and_then(Value::as_str) == Some("_honk/request_resolved"))
        .await;
    assert_eq!(resolved["params"]["sessionId"], sid);
    assert_eq!(resolved["params"]["requestId"], goose_req_id);
    assert_eq!(resolved["params"]["reason"], "answered");

    // v2 now sends a late answer attempting to cancel
    v2.respond(
        goose_req_id,
        json!({
            "outcome": {
                "outcome": "cancelled"
            }
        }),
    )
    .await;

    // v1's turn completes because v1's answer won and v2's late answer was ignored
    let prompt_resp = v1.response(&pid).await;
    assert_eq!(
        prompt_resp["result"]["stopReason"], "end_turn",
        "turn completed normally"
    );

    let export = goose.export(&sid).await;
    let export_str = export.to_string();
    assert!(
        export_str.contains("v1_wins"),
        "tool was executed once per v1's answer: {export_str}"
    );
}

#[tokio::test]
async fn pending_request_survives_detach() {
    let Some(goose) = StubGoose::start_or_skip().await else {
        return;
    };

    goose.model().push([
        Turn::ToolCall {
            tool: "shell".to_string(),
            arguments: json!({"command": "echo detach_wins"}),
        },
        Turn::Text(vec!["detach done".to_string()]),
    ]);

    let honkd = Honkd::spawn(&goose.url(), goose.secret_file(), &[]).await;

    let mut v1 = Viewer::connect(&honkd).await;
    let sid = new_session(&mut v1, goose.cwd().to_str().unwrap()).await;

    let mode_res = v1
        .call(
            "session/set_mode",
            json!({"sessionId": &sid, "modeId": "approve"}),
        )
        .await;
    assert!(mode_res.get("error").is_none());

    let _pid = v1
        .request(
            "session/prompt",
            prompt_params(&sid, "run echo in approve mode", None),
        )
        .await;

    let req1 = v1
        .wait_for(|f| {
            f.get("method").and_then(Value::as_str) == Some("request_permission")
                || f.get("method").and_then(Value::as_str) == Some("session/request_permission")
        })
        .await;
    let goose_req_id = req1["id"].clone();

    // Detach v1
    v1.call("session/close", json!({"sessionId": &sid})).await;
    drop(v1);

    // No viewers for 300 ms
    tokio::time::sleep(Duration::from_millis(300)).await;

    // Connect v2 and load session
    let mut v2 = Viewer::connect(&honkd).await;
    v2.call(
        "session/load",
        json!({"sessionId": &sid, "cwd": ".", "mcpServers": []}),
    )
    .await;

    // v2 receives re-offered request
    let req2 = v2
        .wait_for(|f| {
            f.get("method").and_then(Value::as_str) == Some("request_permission")
                || f.get("method").and_then(Value::as_str) == Some("session/request_permission")
        })
        .await;
    assert_eq!(
        req2["id"], goose_req_id,
        "same request re-offered on attach"
    );

    // v2 answers
    v2.respond(
        goose_req_id,
        json!({
            "outcome": {
                "outcome": "selected",
                "optionId": "allow_once"
            }
        }),
    )
    .await;

    // Wait for run to finish
    v2.wait_for(|f| {
        f.get("method").and_then(Value::as_str) == Some("session/update")
            && f.pointer("/params/update/sessionUpdate")
                .and_then(Value::as_str)
                == Some("session_info_update")
            && f.pointer("/params/update/_meta/goose/activeRunId") == Some(&Value::Null)
    })
    .await;

    let export = goose.export(&sid).await;
    assert!(
        export.to_string().contains("detach_wins"),
        "tool was executed after late viewer answered"
    );
}

#[tokio::test]
async fn pending_request_survives_honkd_kill9() {
    let Some(goose) = StubGoose::start_or_skip().await else {
        return;
    };

    goose.model().push([
        Turn::ToolCall {
            tool: "shell".to_string(),
            arguments: json!({"command": "echo restart_wins"}),
        },
        Turn::Text(vec!["restart done".to_string()]),
    ]);

    let mut honkd = Honkd::spawn(&goose.url(), goose.secret_file(), &[]).await;

    let mut v1 = Viewer::connect(&honkd).await;
    let sid = new_session(&mut v1, goose.cwd().to_str().unwrap()).await;

    let mode_res = v1
        .call(
            "session/set_mode",
            json!({"sessionId": &sid, "modeId": "approve"}),
        )
        .await;
    assert!(mode_res.get("error").is_none());

    let _pid = v1
        .request(
            "session/prompt",
            prompt_params(&sid, "run echo in approve mode", None),
        )
        .await;

    let req1 = v1
        .wait_for(|f| {
            f.get("method").and_then(Value::as_str) == Some("request_permission")
                || f.get("method").and_then(Value::as_str) == Some("session/request_permission")
        })
        .await;
    let goose_req_id = req1["id"].clone();

    // Kill9 honkd
    honkd.kill9();

    // Restart honkd
    honkd.restart().await;

    // Connect new viewer to restarted honkd
    let mut v2 = Viewer::connect(&honkd).await;
    v2.call(
        "session/load",
        json!({"sessionId": &sid, "cwd": ".", "mcpServers": []}),
    )
    .await;

    // v2 receives re-offered request with identical goose id
    let req2 = v2
        .wait_for(|f| {
            f.get("method").and_then(Value::as_str) == Some("request_permission")
                || f.get("method").and_then(Value::as_str) == Some("session/request_permission")
        })
        .await;
    assert_eq!(
        req2["id"], goose_req_id,
        "identical goose id re-offered after restart"
    );

    // Answer the re-offered request
    v2.respond(
        goose_req_id,
        json!({
            "outcome": {
                "outcome": "selected",
                "optionId": "allow_once"
            }
        }),
    )
    .await;

    // Wait for run to finish
    v2.wait_for(|f| {
        f.get("method").and_then(Value::as_str) == Some("session/update")
            && f.pointer("/params/update/sessionUpdate")
                .and_then(Value::as_str)
                == Some("session_info_update")
            && f.pointer("/params/update/_meta/goose/activeRunId") == Some(&Value::Null)
    })
    .await;

    let export = goose.export(&sid).await;
    assert!(
        export.to_string().contains("restart_wins"),
        "tool output is in export after restart"
    );
}

#[tokio::test]
async fn legacy_viewer_auto_attach_during_parked_permission_gets_offer() {
    let Some(goose) = StubGoose::start_or_skip().await else {
        return;
    };

    goose.model().push([
        Turn::ToolCall {
            tool: "shell".to_string(),
            arguments: json!({"command": "echo legacy_wins"}),
        },
        Turn::Text(vec!["legacy done".to_string()]),
    ]);

    let honkd = Honkd::spawn(&goose.url(), goose.secret_file(), &[]).await;

    let mut v1 = Viewer::connect(&honkd).await;
    let sid = new_session(&mut v1, goose.cwd().to_str().unwrap()).await;

    let mode_res = v1
        .call(
            "session/set_mode",
            json!({"sessionId": &sid, "modeId": "approve"}),
        )
        .await;
    assert!(mode_res.get("error").is_none());

    let _pid = v1
        .request(
            "session/prompt",
            prompt_params(&sid, "run echo in approve mode", None),
        )
        .await;

    let req1 = v1
        .wait_for(|f| {
            f.get("method").and_then(Value::as_str) == Some("request_permission")
                || f.get("method").and_then(Value::as_str) == Some("session/request_permission")
        })
        .await;
    let goose_req_id = req1["id"].clone();

    // Connect legacy pigeon viewer
    let mut legacy = Viewer::connect_pigeon(&honkd).await;

    // Legacy viewer attempts a prompt while the permission is parked
    let prompt_id = legacy
        .request(
            "session/prompt",
            prompt_params(&sid, "another prompt", None),
        )
        .await;

    // Legacy viewer gets busy error
    let busy_resp = legacy.response(&prompt_id).await;
    assert_eq!(
        busy_resp["error"]["code"], -32602,
        "legacy viewer received busy error"
    );

    // Legacy viewer gets the parked request offered on auto-attach
    let req_legacy = legacy
        .wait_for(|f| {
            f.get("method").and_then(Value::as_str) == Some("request_permission")
                || f.get("method").and_then(Value::as_str) == Some("session/request_permission")
        })
        .await;
    assert_eq!(
        req_legacy["id"], goose_req_id,
        "legacy viewer got parked request offered"
    );

    // Legacy viewer answers it
    legacy
        .respond(
            goose_req_id,
            json!({
                "outcome": {
                    "outcome": "selected",
                    "optionId": "allow_once"
                }
            }),
        )
        .await;

    // Wait for run to finish on v1
    let prompt_resp = v1.response(&_pid).await;
    assert_eq!(prompt_resp["result"]["stopReason"], "end_turn");

    let export = goose.export(&sid).await;
    assert!(
        export.to_string().contains("legacy_wins"),
        "tool was executed after legacy viewer answered"
    );
}

#[tokio::test]
async fn steer_probe_post_failure_404_transitions_to_gone() {
    use honk_testgoose::FakeGoose;
    use reqwest::StatusCode;

    let fake = FakeGoose::start().await;
    let honkd = Honkd::spawn(
        &fake.url(),
        fake.secret_file(),
        &["--allow-unpatched-upstream"],
    )
    .await;

    let mut v = Viewer::connect(&honkd).await;
    let sid = new_session(&mut v, ".").await;

    v.call(
        "session/load",
        json!({"sessionId": &sid, "cwd": ".", "mcpServers": []}),
    )
    .await;

    // Script an active run with a tool call in FakeGoose
    fake.script_frames(vec![
        json!({
            "jsonrpc": "2.0",
            "method": "session/update",
            "params": {
                "sessionId": &sid,
                "update": {
                    "sessionUpdate": "session_info_update",
                    "_meta": {
                        "goose": {
                            "activeRunId": "run_probe_test"
                        }
                    }
                }
            }
        }),
        json!({
            "jsonrpc": "2.0",
            "method": "session/update",
            "params": {
                "sessionId": &sid,
                "update": {
                    "sessionUpdate": "tool_call",
                    "toolCallId": "call_1",
                    "title": "shell",
                    "status": "in_progress"
                }
            }
        }),
    ]);

    // Induce gap on resume, and 404 on steer probe POST
    fake.gap_on_next_resume(&sid);
    fake.set_steer_status(Some(StatusCode::NOT_FOUND));

    let pid = v
        .request(
            "session/prompt",
            prompt_params(&sid, "test_probe_404", None),
        )
        .await;

    // Wait until tool_call frame appears
    v.wait_for(|f| {
        f.pointer("/params/update/sessionUpdate")
            .and_then(Value::as_str)
            == Some("tool_call")
    })
    .await;

    // Kick stream while turn is in flight
    fake.kick_stream(&sid);

    // Watchdog or stream ended causes reattach -> gap -> steer probe POST returns 404 -> ReattachFailed { gone: true } -> Gone!
    // The in-flight prompt should get cancelled with reason upstream_gone
    let resp = v.response_timeout(&pid, Duration::from_secs(10)).await;
    assert_eq!(resp["result"]["stopReason"], "cancelled");
    assert_eq!(
        resp.pointer("/result/_meta/honk/reason")
            .and_then(Value::as_str),
        Some("upstream_gone")
    );
}
