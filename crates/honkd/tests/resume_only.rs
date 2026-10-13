use common::{Honkd, Viewer, new_session};
use honk_testgoose::{FakeGoose, StubGoose, Turn};
use serde_json::{Value, json};
use std::time::Duration;

mod common;

fn resume_prompt_params(
    sid: &str,
    text: &str,
    client_req_id: Option<&str>,
    resume_only: bool,
) -> Value {
    let mut p = json!({
        "sessionId": sid,
        "prompt": [{
            "type": "text",
            "text": text,
        }]
    });
    let mut honk_obj = serde_json::Map::new();
    if let Some(crid) = client_req_id {
        honk_obj.insert("client_req_id".to_string(), json!(crid));
    }
    if resume_only {
        honk_obj.insert("resume_only".to_string(), json!(true));
    }
    if !honk_obj.is_empty() {
        p["_meta"] = json!({ "honk": honk_obj });
    }
    p
}

#[tokio::test(flavor = "multi_thread")]
async fn initialize_advertises_resume_only_feature() {
    let fake = FakeGoose::start().await;
    fake.advertise_patched(true);

    let h = Honkd::spawn(&fake.url(), fake.secret_file(), &[]).await;
    let mut v = Viewer::connect(&h).await;

    let res = v
        .call(
            "initialize",
            json!({
                "protocolVersion": 1,
                "clientCapabilities": {},
            }),
        )
        .await;

    let features = res
        .pointer("/result/_meta/honk/features")
        .and_then(Value::as_array)
        .expect("features array in _meta.honk");
    assert!(
        features.contains(&json!(honk_wire::FEATURE_RESUME_ONLY)),
        "initialize must advertise FEATURE_RESUME_ONLY: {res:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn initialize_advertises_resume_only_feature_with_stub_goose() {
    let Some(g) = StubGoose::start_or_skip().await else {
        return;
    };
    let h = Honkd::spawn(&g.url(), g.secret_file(), &[]).await;
    let mut v = Viewer::connect(&h).await;

    let res = v
        .call(
            "initialize",
            json!({
                "protocolVersion": 1,
                "clientCapabilities": {},
            }),
        )
        .await;

    let features = res
        .pointer("/result/_meta/honk/features")
        .and_then(Value::as_array)
        .expect("features array in _meta.honk");
    assert!(
        features.contains(&json!(honk_wire::FEATURE_RESUME_ONLY)),
        "initialize must advertise FEATURE_RESUME_ONLY: {res:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn initialize_advertises_resume_only_feature_to_legacy_viewer() {
    let fake = FakeGoose::start().await;
    fake.advertise_patched(true);

    let h = Honkd::spawn(&fake.url(), fake.secret_file(), &[]).await;
    let mut v = Viewer::connect_pigeon(&h).await;

    let res = v
        .call(
            "initialize",
            json!({
                "protocolVersion": 1,
                "clientCapabilities": {
                    "fs": {
                        "readTextFile": false,
                        "writeTextFile": false
                    }
                },
            }),
        )
        .await;

    let features = res
        .pointer("/result/_meta/honk/features")
        .and_then(Value::as_array)
        .expect("features array in _meta.honk");
    assert!(
        features.contains(&json!(honk_wire::FEATURE_RESUME_ONLY)),
        "initialize to legacy viewer must advertise FEATURE_RESUME_ONLY: {res:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn resume_only_prompt_with_unknown_crid_returns_32004_no_upstream_prompt() {
    let Some(g) = StubGoose::start_or_skip().await else {
        return;
    };
    let h = Honkd::spawn(&g.url(), g.secret_file(), &[]).await;
    let mut v = Viewer::connect(&h).await;
    let sid = new_session(&mut v, g.cwd().to_str().unwrap()).await;

    let res = v
        .call(
            "session/prompt",
            resume_prompt_params(&sid, "test resume unknown", Some("unknown-crid"), true),
        )
        .await;

    assert_eq!(res["error"]["code"], honk_wire::ERR_UNKNOWN_CLIENT_REQ_ID);
    assert_eq!(
        res["error"]["message"].as_str().unwrap(),
        "unknown client_req_id; resume_only prompt not started"
    );
    assert_eq!(
        g.model().turn_requests().len(),
        0,
        "no upstream prompt must reach the model"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn resume_only_prompt_with_no_crid_returns_32004_no_upstream_prompt() {
    let Some(g) = StubGoose::start_or_skip().await else {
        return;
    };
    let h = Honkd::spawn(&g.url(), g.secret_file(), &[]).await;
    let mut v = Viewer::connect(&h).await;
    let sid = new_session(&mut v, g.cwd().to_str().unwrap()).await;

    let res = v
        .call(
            "session/prompt",
            resume_prompt_params(&sid, "test resume no crid", None, true),
        )
        .await;

    assert_eq!(res["error"]["code"], honk_wire::ERR_UNKNOWN_CLIENT_REQ_ID);
    assert_eq!(
        res["error"]["message"].as_str().unwrap(),
        "unknown client_req_id; resume_only prompt not started"
    );
    assert_eq!(
        g.model().turn_requests().len(),
        0,
        "no upstream prompt must reach the model"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn resume_only_prompt_with_numeric_crid_returns_32004_no_upstream_prompt() {
    let Some(g) = StubGoose::start_or_skip().await else {
        return;
    };
    let h = Honkd::spawn(&g.url(), g.secret_file(), &[]).await;
    let mut v = Viewer::connect(&h).await;
    let sid = new_session(&mut v, g.cwd().to_str().unwrap()).await;

    let p = json!({
        "sessionId": sid,
        "prompt": [{
            "type": "text",
            "text": "test resume numeric crid",
        }],
        "_meta": {
            "honk": {
                "client_req_id": 12345,
                "resume_only": true,
            }
        }
    });

    let res = v.call("session/prompt", p).await;

    assert_eq!(res["error"]["code"], honk_wire::ERR_UNKNOWN_CLIENT_REQ_ID);
    assert_eq!(
        res["error"]["message"].as_str().unwrap(),
        "unknown client_req_id; resume_only prompt not started"
    );
    assert_eq!(
        g.model().turn_requests().len(),
        0,
        "no upstream prompt must reach the model"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn resume_only_reask_of_completed_turn_returns_cached_reply() {
    let Some(g) = StubGoose::start_or_skip().await else {
        return;
    };
    g.model()
        .push([Turn::Text(vec!["cached response text".into()])]);

    let h = Honkd::spawn(&g.url(), g.secret_file(), &[]).await;
    let mut v = Viewer::connect(&h).await;
    let sid = new_session(&mut v, g.cwd().to_str().unwrap()).await;

    let res1 = v
        .call(
            "session/prompt",
            resume_prompt_params(&sid, "initial prompt", Some("crid-completed-1"), false),
        )
        .await;
    assert_eq!(res1["result"]["stopReason"], "end_turn");
    assert_eq!(
        res1["result"]["_meta"]["honk"]["text"],
        "cached response text"
    );

    // Re-ask with resume_only: true
    let res2 = v
        .call(
            "session/prompt",
            resume_prompt_params(&sid, "initial prompt reask", Some("crid-completed-1"), true),
        )
        .await;
    assert_eq!(res2["result"]["stopReason"], "end_turn");
    assert_eq!(
        res2["result"]["_meta"]["honk"]["text"],
        "cached response text"
    );

    assert_eq!(
        g.model().turn_requests().len(),
        1,
        "only the initial prompt reached the upstream model"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn resume_only_reask_while_running_attaches_and_gets_reply_one_upstream_prompt() {
    let Some(g) = StubGoose::start_or_skip().await else {
        return;
    };
    g.model().push([Turn::Gate(
        "gate_resume_active".into(),
        Box::new(Turn::Text(vec!["resume active text".into()])),
    )]);

    let h = Honkd::spawn(&g.url(), g.secret_file(), &[]).await;
    let mut v1 = Viewer::connect(&h).await;
    let sid = new_session(&mut v1, g.cwd().to_str().unwrap()).await;

    let req1 = v1
        .request(
            "session/prompt",
            resume_prompt_params(&sid, "turn running", Some("crid-running-1"), false),
        )
        .await;

    v1.wait_for(|f| {
        f.pointer("/params/update/sessionUpdate")
            .and_then(Value::as_str)
            == Some("session_info_update")
            && f.pointer("/params/update/_meta/goose/activeRunId")
                .and_then(Value::as_str)
                .is_some()
    })
    .await;

    // Wait until prompt has reached the stub model and is parked at the gate
    let start = std::time::Instant::now();
    while g.model().turn_requests().is_empty() {
        if start.elapsed() > Duration::from_secs(5) {
            panic!("timed out waiting for turn request to reach model");
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    let mut v2 = Viewer::connect(&h).await;
    let req2 = v2
        .request(
            "session/prompt",
            resume_prompt_params(&sid, "turn running reask", Some("crid-running-1"), true),
        )
        .await;

    // Pin: confirm turn is still in flight (exactly 1 turn request, gate still closed)
    assert_eq!(g.model().turn_requests().len(), 1);
    tokio::time::sleep(Duration::from_millis(50)).await;

    g.model().open("gate_resume_active");

    let res1 = v1.response(&req1).await;
    let res2 = v2.response(&req2).await;

    assert_eq!(res1["id"], req1);
    assert_eq!(res1["result"]["stopReason"], "end_turn");
    assert_eq!(
        res1["result"]["_meta"]["honk"]["text"],
        "resume active text"
    );

    assert_eq!(res2["id"], req2);
    assert_eq!(res2["result"]["stopReason"], "end_turn");
    assert_eq!(
        res2["result"]["_meta"]["honk"]["text"],
        "resume active text"
    );

    assert_eq!(
        g.model().turn_requests().len(),
        1,
        "only 1 prompt reached upstream"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn plain_duplicate_while_parked_attaches_and_both_get_reply() {
    let fake = FakeGoose::start().await;
    fake.advertise_patched(true);
    fake.set_run_announcement_delay(Some(Duration::from_millis(600)));

    let h = Honkd::spawn(&fake.url(), fake.secret_file(), &[]).await;
    let mut v1 = Viewer::connect(&h).await;
    let sid = new_session(&mut v1, ".").await;

    // Prompt 1 is dispatched to upstream
    let req1 = v1
        .request(
            "session/prompt",
            resume_prompt_params(&sid, "first prompt", Some("crid-park-first"), false),
        )
        .await;

    // While prompt 1 is in-flight (unannounced), prompt 2 is parked
    let req2_v1 = v1
        .request(
            "session/prompt",
            resume_prompt_params(&sid, "second parked prompt", Some("crid-park-dup"), false),
        )
        .await;

    // Prompt 2 duplicate arrives from another viewer while prompt 2 is parked
    let mut v2 = Viewer::connect(&h).await;
    let req2_v2 = v2
        .request(
            "session/prompt",
            resume_prompt_params(&sid, "second parked dup", Some("crid-park-dup"), false),
        )
        .await;

    let res1 = v1.response(&req1).await;
    assert_eq!(res1["result"]["stopReason"], "end_turn");

    let res2_1 = v1.response(&req2_v1).await;
    let res2_2 = v2.response(&req2_v2).await;

    assert_eq!(res2_1["result"]["stopReason"], "end_turn");
    assert_eq!(res2_2["result"]["stopReason"], "end_turn");
    assert_eq!(res2_1["id"], req2_v1);
    assert_eq!(res2_2["id"], req2_v2);

    // Only 2 prompts reached FakeGoose (one for first, one for second)
    assert_eq!(fake.received_prompts(&sid).len(), 2);
}

#[tokio::test(flavor = "multi_thread")]
async fn resume_only_reask_while_parked_attaches_and_gets_reply() {
    let fake = FakeGoose::start().await;
    fake.advertise_patched(true);
    fake.set_run_announcement_delay(Some(Duration::from_millis(600)));

    let h = Honkd::spawn(&fake.url(), fake.secret_file(), &[]).await;
    let mut v1 = Viewer::connect(&h).await;
    let sid = new_session(&mut v1, ".").await;

    // Prompt 1 is dispatched to upstream
    let req1 = v1
        .request(
            "session/prompt",
            resume_prompt_params(&sid, "first prompt", Some("crid-park-first-2"), false),
        )
        .await;

    // Prompt 2 is parked while prompt 1 is in flight
    let req2_v1 = v1
        .request(
            "session/prompt",
            resume_prompt_params(
                &sid,
                "second parked prompt",
                Some("crid-park-resume"),
                false,
            ),
        )
        .await;

    // Resume-only re-ask arrives while prompt 2 is parked
    let mut v2 = Viewer::connect(&h).await;
    let req2_v2 = v2
        .request(
            "session/prompt",
            resume_prompt_params(&sid, "second parked reask", Some("crid-park-resume"), true),
        )
        .await;

    let res1 = v1.response(&req1).await;
    assert_eq!(res1["result"]["stopReason"], "end_turn");

    let res2_1 = v1.response(&req2_v1).await;
    let res2_2 = v2.response(&req2_v2).await;

    assert_eq!(res2_1["result"]["stopReason"], "end_turn");
    assert_eq!(res2_2["result"]["stopReason"], "end_turn");
    assert_eq!(res2_1["id"], req2_v1);
    assert_eq!(res2_2["id"], req2_v2);

    assert_eq!(fake.received_prompts(&sid).len(), 2);
}

#[tokio::test(flavor = "multi_thread")]
async fn resume_only_reask_during_finalizing_window_attaches_and_gets_reply() {
    let fake = FakeGoose::start().await;
    fake.advertise_patched(true);
    fake.set_export_delay(Some(Duration::from_millis(600)));

    let h = Honkd::spawn(&fake.url(), fake.secret_file(), &[]).await;
    let mut v1 = Viewer::connect(&h).await;
    let sid = new_session(&mut v1, ".").await;

    fake.set_export_data(
        &sid,
        json!({
            "sessionId": sid,
            "conversation": [{
                "id": "m1",
                "role": "user",
                "created": 1,
                "content": [{"type": "text", "text": "finalizing prompt"}]
            }, {
                "id": "m2",
                "role": "assistant",
                "created": 2,
                "content": [{"type": "text", "text": "finalizing reply text"}]
            }]
        }),
    );

    let req1 = v1
        .request(
            "session/prompt",
            resume_prompt_params(&sid, "finalizing prompt", Some("crid-finalizing"), false),
        )
        .await;

    let start = std::time::Instant::now();
    while fake.received_prompts(&sid).is_empty() {
        if start.elapsed() > Duration::from_secs(5) {
            panic!("timed out waiting for prompt to reach fake goose");
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    tokio::time::sleep(Duration::from_millis(150)).await;

    let mut v2 = Viewer::connect(&h).await;
    let req2 = v2
        .request(
            "session/prompt",
            resume_prompt_params(
                &sid,
                "finalizing prompt reask",
                Some("crid-finalizing"),
                true,
            ),
        )
        .await;

    let res1 = v1.response(&req1).await;
    let res2 = v2.response(&req2).await;

    assert_eq!(res1["result"]["stopReason"], "end_turn");
    assert_eq!(
        res1["result"]["_meta"]["honk"]["text"],
        "finalizing reply text"
    );

    assert_eq!(res2["result"]["stopReason"], "end_turn");
    assert_eq!(
        res2["result"]["_meta"]["honk"]["text"],
        "finalizing reply text"
    );
    assert_eq!(res2["id"], req2);

    assert_eq!(fake.received_prompts(&sid).len(), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn cold_session_resume_only_reask_serves_cached_done_zero_upstream_session_load() {
    let fake = FakeGoose::start().await;
    fake.advertise_patched(true);

    let h = Honkd::spawn(&fake.url(), fake.secret_file(), &["--idle-ttl-secs", "1"]).await;
    let mut v = Viewer::connect(&h).await;
    let sid = new_session(&mut v, ".").await;

    let res = v
        .call(
            "session/prompt",
            resume_prompt_params(&sid, "cold test", Some("crid-cold-1"), false),
        )
        .await;
    assert_eq!(res["result"]["stopReason"], "end_turn");

    // Close session and wait for idle eviction
    v.call("session/close", json!({ "sessionId": &sid })).await;

    let mut evicted = false;
    for _ in 0..30 {
        tokio::time::sleep(Duration::from_millis(100)).await;
        let state_path = h.state_dir.path().join("state.json");
        if let Ok(content) = std::fs::read_to_string(&state_path)
            && let Ok(st) = serde_json::from_str::<Value>(&content)
            && st.pointer(&format!("/sessions/{sid}")).is_none()
        {
            evicted = true;
            break;
        }
    }
    assert!(evicted, "session must be evicted after idle TTL");

    let loads_before = fake
        .inbound()
        .iter()
        .filter(|f| f.get("method").and_then(Value::as_str) == Some("session/load"))
        .count();

    // Re-ask with resume_only: true
    let mut v2 = Viewer::connect(&h).await;
    let res2 = v2
        .call(
            "session/prompt",
            resume_prompt_params(&sid, "cold test reask", Some("crid-cold-1"), true),
        )
        .await;

    assert_eq!(res2["result"]["stopReason"], "end_turn");

    let loads_after = fake
        .inbound()
        .iter()
        .filter(|f| f.get("method").and_then(Value::as_str) == Some("session/load"))
        .count();

    assert_eq!(
        loads_after, loads_before,
        "resume_only re-ask for cold Done session must NOT trigger upstream session/load"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn cold_session_resume_only_missing_crid_returns_32004_zero_upstream_session_load() {
    let fake = FakeGoose::start().await;
    fake.advertise_patched(true);

    let h = Honkd::spawn(&fake.url(), fake.secret_file(), &["--idle-ttl-secs", "1"]).await;
    let mut v = Viewer::connect(&h).await;
    let sid = new_session(&mut v, ".").await;

    v.call("session/close", json!({ "sessionId": &sid })).await;

    let mut evicted = false;
    for _ in 0..30 {
        tokio::time::sleep(Duration::from_millis(100)).await;
        let state_path = h.state_dir.path().join("state.json");
        if let Ok(content) = std::fs::read_to_string(&state_path)
            && let Ok(st) = serde_json::from_str::<Value>(&content)
            && st.pointer(&format!("/sessions/{sid}")).is_none()
        {
            evicted = true;
            break;
        }
    }
    assert!(evicted, "session must be evicted after idle TTL");

    let loads_before = fake
        .inbound()
        .iter()
        .filter(|f| f.get("method").and_then(Value::as_str) == Some("session/load"))
        .count();

    let mut v2 = Viewer::connect(&h).await;
    let res = v2
        .call(
            "session/prompt",
            resume_prompt_params(&sid, "cold missing crid", Some("crid-missing-cold"), true),
        )
        .await;

    assert_eq!(res["error"]["code"], honk_wire::ERR_UNKNOWN_CLIENT_REQ_ID);

    let loads_after = fake
        .inbound()
        .iter()
        .filter(|f| f.get("method").and_then(Value::as_str) == Some("session/load"))
        .count();

    assert_eq!(
        loads_after, loads_before,
        "resume_only prompt with unknown crid for cold session must NOT trigger upstream session/load"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn resume_only_reask_for_boot_pruned_lost_track_returns_cached_32000() {
    let fake = FakeGoose::start().await;
    fake.advertise_patched(true);

    let mut h = Honkd::spawn(&fake.url(), fake.secret_file(), &[]).await;
    let mut v = Viewer::connect(&h).await;
    let sid = new_session(&mut v, ".").await;
    v.call(
        "session/load",
        json!({"sessionId": sid, "cwd": ".", "mcpServers": []}),
    )
    .await;

    // Mutate state.json to add an InFlight ledger entry for this session with no turn inflight
    {
        let state_path = h.state_dir.path().join("state.json");
        let content = std::fs::read_to_string(&state_path).expect("read state");
        let mut parsed: Value = serde_json::from_str(&content).expect("parse state");
        parsed["ledger"]["lost_crid_resume"] = json!({
            "state": "in_flight",
            "session": sid,
            "upstream_id": "hk_lost_resume",
        });
        std::fs::write(&state_path, serde_json::to_string_pretty(&parsed).unwrap())
            .expect("write state");
    }

    h.restart().await;

    let mut w = Viewer::connect(&h).await;
    let resp = w
        .call(
            "session/prompt",
            resume_prompt_params(&sid, "test", Some("lost_crid_resume"), true),
        )
        .await;

    assert_eq!(resp["error"]["code"], -32000);
    assert!(
        resp["error"]["message"]
            .as_str()
            .unwrap()
            .contains("honkd lost track of this client_req_id; not resending")
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn resume_only_prompt_with_unknown_crid_on_busy_session_returns_32004_not_busy() {
    let Some(g) = StubGoose::start_or_skip().await else {
        return;
    };
    g.model().push([Turn::Gate(
        "gate_busy_test".into(),
        Box::new(Turn::Text(vec!["busy test text".into()])),
    )]);

    let h = Honkd::spawn(&g.url(), g.secret_file(), &[]).await;
    let mut v1 = Viewer::connect(&h).await;
    let sid = new_session(&mut v1, g.cwd().to_str().unwrap()).await;

    let req1 = v1
        .request(
            "session/prompt",
            resume_prompt_params(&sid, "turn running", Some("crid-active"), false),
        )
        .await;

    v1.wait_for(|f| {
        f.pointer("/params/update/sessionUpdate")
            .and_then(Value::as_str)
            == Some("session_info_update")
            && f.pointer("/params/update/_meta/goose/activeRunId")
                .and_then(Value::as_str)
                .is_some()
    })
    .await;

    // While busy with crid-active, send resume_only prompt with unknown crid
    let mut v2 = Viewer::connect(&h).await;
    let res2 = v2
        .call(
            "session/prompt",
            resume_prompt_params(&sid, "turn resume unknown", Some("unknown-crid"), true),
        )
        .await;

    assert_eq!(res2["error"]["code"], honk_wire::ERR_UNKNOWN_CLIENT_REQ_ID);
    assert_eq!(
        res2["error"]["message"].as_str().unwrap(),
        "unknown client_req_id; resume_only prompt not started"
    );

    // Release gate and finish initial prompt
    g.model().open("gate_busy_test");
    let res1 = v1.response(&req1).await;
    assert_eq!(res1["result"]["stopReason"], "end_turn");
}
