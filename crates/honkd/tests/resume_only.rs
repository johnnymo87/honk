use common::{Honkd, Viewer, new_session};
use honk_testgoose::{FakeGoose, StubGoose, Turn};
use serde_json::{Value, json};

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

    let mut v2 = Viewer::connect(&h).await;
    let req2 = v2
        .request(
            "session/prompt",
            resume_prompt_params(&sid, "turn running reask", Some("crid-running-1"), true),
        )
        .await;

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
async fn resume_only_reask_for_inconsistent_inflight_returns_lost_track_32000() {
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
