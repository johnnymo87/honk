use common::{Honkd, Viewer, new_session, prompt_params};
use honk_testgoose::{FakeGoose, StubGoose};
use serde_json::{Value, json};
use std::time::Duration;

mod common;

#[tokio::test(flavor = "multi_thread")]
async fn test_model_switch_and_fresh_init_result() {
    let fake = FakeGoose::start().await;
    let h = Honkd::spawn(
        &fake.url(),
        fake.secret_file(),
        &["--allow-unpatched-upstream"],
    )
    .await;
    let mut v1 = Viewer::connect(&h).await;

    let new_res = v1
        .call(
            "session/new",
            json!({
                "cwd": "/tmp",
            }),
        )
        .await;
    let sid = new_res["result"]["sessionId"].as_str().unwrap().to_string();

    // Verify initial model option in session/new is claude-opus-5-5
    let opts = new_res["result"]["configOptions"]
        .as_array()
        .expect("configOptions array");
    let model_opt = opts
        .iter()
        .find(|o| o["id"] == "model")
        .expect("model option");
    assert_eq!(model_opt["currentValue"], "claude-opus-5-5");

    // Connect a second viewer v2 attached to the same session
    let mut v2 = Viewer::connect(&h).await;
    let load_v2 = v2
        .call(
            "session/load",
            json!({
                "sessionId": sid,
            }),
        )
        .await;
    let opts_v2 = load_v2["result"]["configOptions"]
        .as_array()
        .expect("configOptions array");
    let model_v2 = opts_v2
        .iter()
        .find(|o| o["id"] == "model")
        .expect("model option");
    assert_eq!(model_v2["currentValue"], "claude-opus-5-5");

    // (d) Idle set is forwarded and both attached viewers receive the config_option_update frame.
    let switch_res = v1
        .call(
            "session/set_config_option",
            json!({
                "sessionId": sid,
                "configId": "model",
                "value": "gemini-3.8-flash",
            }),
        )
        .await;
    assert!(switch_res.get("result").is_some());
    assert_eq!(
        switch_res["result"]["configOptions"]
            .as_array()
            .unwrap()
            .iter()
            .find(|o| o["id"] == "model")
            .unwrap()["currentValue"],
        "gemini-3.8-flash"
    );

    // Both v1 and v2 should receive the config_option_update frame
    let is_config_update = |f: &Value| {
        f.get("method").and_then(Value::as_str) == Some("session/update")
            && f.pointer("/params/update/sessionUpdate")
                .and_then(Value::as_str)
                == Some("config_option_update")
    };
    let update_v1 = v1.wait_for(is_config_update).await;
    let update_v2 = v2.wait_for(is_config_update).await;
    assert_eq!(
        update_v1
            .pointer("/params/update/configOptions")
            .and_then(Value::as_array)
            .unwrap()
            .iter()
            .find(|o| o["id"] == "model")
            .unwrap()["currentValue"],
        "gemini-3.8-flash"
    );
    assert_eq!(
        update_v2
            .pointer("/params/update/configOptions")
            .and_then(Value::as_array)
            .unwrap()
            .iter()
            .find(|o| o["id"] == "model")
            .unwrap()["currentValue"],
        "gemini-3.8-flash"
    );

    // (a) Viewer B (v3) attaching afterwards gets the new currentValue in its session/load reply.
    let mut v3 = Viewer::connect(&h).await;
    let load_v3 = v3
        .call(
            "session/load",
            json!({
                "sessionId": sid,
            }),
        )
        .await;
    let opts_v3 = load_v3["result"]["configOptions"]
        .as_array()
        .expect("configOptions array");
    let model_v3 = opts_v3
        .iter()
        .find(|o| o["id"] == "model")
        .expect("model option");
    assert_eq!(model_v3["currentValue"], "gemini-3.8-flash");

    // (b) honkd's persisted state file contains it.
    let state_bytes = std::fs::read(h.state_dir.path().join("state.json")).expect("read state");
    let state_val: Value = serde_json::from_slice(&state_bytes).expect("parse state");
    let held_opts = state_val["sessions"][&sid]["init_result"]["configOptions"]
        .as_array()
        .expect("persisted configOptions array");
    let held_model = held_opts
        .iter()
        .find(|o| o["id"] == "model")
        .expect("held model option");
    assert_eq!(held_model["currentValue"], "gemini-3.8-flash");

    // (c) set_config_option during a running turn -> -32002.
    fake.set_run_announcement_delay(Some(Duration::from_millis(600)));
    let prompt_id = v1
        .request(
            "session/prompt",
            prompt_params(&sid, "test busy turn", Some("crid-busy-model-test")),
        )
        .await;

    // Immediately while turn is in-flight:
    let busy_res = v1
        .call(
            "session/set_config_option",
            json!({
                "sessionId": sid,
                "configId": "model",
                "value": "claude-opus-5-5",
            }),
        )
        .await;
    assert_eq!(busy_res["error"]["code"], -32002);
    assert_eq!(
        busy_res["error"]["message"],
        "session is busy; try after this turn"
    );
    assert_eq!(busy_res["error"]["data"], json!({"honk": "busy"}));

    // Await prompt completion
    let _ = v1.response(&prompt_id).await;
    fake.set_run_announcement_delay(None);

    // Restart honkd and verify attach still returns the updated currentValue
    drop(v1);
    drop(v2);
    drop(v3);
    let mut h = h;
    h.restart().await;
    let mut v_after_restart = Viewer::connect(&h).await;
    let load_after_restart = v_after_restart
        .call(
            "session/load",
            json!({
                "sessionId": sid,
            }),
        )
        .await;
    let opts_after = load_after_restart["result"]["configOptions"]
        .as_array()
        .expect("configOptions array after restart");
    let model_after = opts_after
        .iter()
        .find(|o| o["id"] == "model")
        .expect("model option after restart");
    assert_eq!(model_after["currentValue"], "gemini-3.8-flash");
}

#[tokio::test(flavor = "multi_thread")]
async fn test_model_list_e2e() {
    let fake = FakeGoose::start().await;

    // 1. With no --models-file flag: only goose models returned, no warning
    let h_no_flag = Honkd::spawn(
        &fake.url(),
        fake.secret_file(),
        &["--allow-unpatched-upstream"],
    )
    .await;
    let mut v_no_flag = Viewer::connect(&h_no_flag).await;

    let new_res = v_no_flag
        .call("session/new", json!({ "cwd": "/tmp" }))
        .await;
    let sid = new_res["result"]["sessionId"].as_str().unwrap().to_string();

    let list_no_flag = v_no_flag
        .call("_honk/model/list", json!({ "sessionId": sid }))
        .await;
    assert_eq!(list_no_flag["result"]["provider"], "gcp_vertex_ai");
    assert_eq!(list_no_flag["result"]["current"], "claude-opus-5-5");
    assert!(list_no_flag["result"].get("warning").is_none());
    let models_no_flag = list_no_flag["result"]["models"]
        .as_array()
        .expect("models array");
    assert_eq!(models_no_flag.len(), 2);
    assert_eq!(models_no_flag[0]["id"], "claude-opus-5-5");
    assert_eq!(models_no_flag[0]["source"], "goose");
    assert_eq!(models_no_flag[1]["id"], "gemini-3.8-flash");
    assert_eq!(models_no_flag[1]["source"], "goose");

    // 2. With --models-file flag: configured first then goose, current reflects switch
    let temp_dir = tempfile::tempdir().unwrap();
    let models_path = temp_dir.path().join("models.json");
    let initial_models_json = json!({
        "providers": {
            "gcp_vertex_ai": [
                { "id": "custom-model", "name": "Custom Model" },
                { "id": "claude-opus-5-5", "name": "Claude Opus Configured" }
            ]
        }
    });
    std::fs::write(&models_path, initial_models_json.to_string()).unwrap();

    let h = Honkd::spawn(
        &fake.url(),
        fake.secret_file(),
        &[
            "--allow-unpatched-upstream",
            "--models-file",
            models_path.to_str().unwrap(),
        ],
    )
    .await;
    let mut v = Viewer::connect(&h).await;

    let new_res2 = v.call("session/new", json!({ "cwd": "/tmp" })).await;
    let sid2 = new_res2["result"]["sessionId"]
        .as_str()
        .unwrap()
        .to_string();

    let list_res = v
        .call("_honk/model/list", json!({ "sessionId": sid2 }))
        .await;
    assert_eq!(list_res["result"]["provider"], "gcp_vertex_ai");
    assert_eq!(list_res["result"]["current"], "claude-opus-5-5");
    assert!(list_res["result"].get("warning").is_none());

    let models = list_res["result"]["models"]
        .as_array()
        .expect("models array");
    assert_eq!(models.len(), 3);
    // Configured first
    assert_eq!(models[0]["id"], "custom-model");
    assert_eq!(models[0]["name"], "Custom Model");
    assert_eq!(models[0]["source"], "configured");
    assert_eq!(models[1]["id"], "claude-opus-5-5");
    assert_eq!(models[1]["name"], "Claude Opus Configured");
    assert_eq!(models[1]["source"], "configured");
    // Goose option that wasn't already listed
    assert_eq!(models[2]["id"], "gemini-3.8-flash");
    assert_eq!(models[2]["source"], "goose");

    // Switch model and verify current reflects prior switch
    let switch_res = v
        .call(
            "session/set_config_option",
            json!({
                "sessionId": sid2,
                "configId": "model",
                "value": "gemini-3.8-flash",
            }),
        )
        .await;
    assert!(switch_res.get("result").is_some());

    let list_after_switch = v
        .call("_honk/model/list", json!({ "sessionId": sid2 }))
        .await;
    assert_eq!(list_after_switch["result"]["current"], "gemini-3.8-flash");

    // Corrupt the models file: list still succeeds with goose models and a warning
    std::fs::write(&models_path, "not valid json").unwrap();
    let list_corrupt = v
        .call("_honk/model/list", json!({ "sessionId": sid2 }))
        .await;
    assert_eq!(list_corrupt["result"]["provider"], "gcp_vertex_ai");
    assert_eq!(list_corrupt["result"]["current"], "gemini-3.8-flash");
    assert!(list_corrupt["result"].get("warning").is_some());
    let models_corrupt = list_corrupt["result"]["models"]
        .as_array()
        .expect("models array");
    assert_eq!(models_corrupt.len(), 2);
    assert_eq!(models_corrupt[0]["source"], "goose");
    assert_eq!(models_corrupt[1]["source"], "goose");
}

#[tokio::test(flavor = "multi_thread")]
async fn test_model_list_after_actor_reactivation_with_attached_viewer() {
    let mut fake = FakeGoose::start().await;
    fake.advertise_patched(true);

    let h = Honkd::spawn(
        &fake.url(),
        fake.secret_file(),
        &["--allow-unpatched-upstream"],
    )
    .await;
    let mut v = Viewer::connect(&h).await;

    let new_res = v.call("session/new", json!({ "cwd": "/tmp" })).await;
    let sid = new_res["result"]["sessionId"].as_str().unwrap().to_string();

    // v is attached to sid (v.attached_sessions contains sid).
    // Simulate actor retirement: remove session from fake and restart so actor terminates
    fake.remove_session(&sid);
    fake.restart().await;

    // Viewer receives evicted notification
    let evicted = v
        .wait_for(|f| f.get("method").and_then(Value::as_str) == Some(honk_wire::method::EVICTED))
        .await;
    assert_eq!(evicted["params"]["sessionId"], sid);

    // Re-register sid in FakeGoose so get_or_activate_session can succeed
    fake.insert_session(&sid, "/tmp");

    // Call _honk/model/list: fresh actor activated, v.attached_sessions already contains sid.
    // ModelList must auto-attach unknown viewer and reply successfully (not hang).
    let list_res = v
        .call("_honk/model/list", json!({ "sessionId": sid }))
        .await;
    assert_eq!(list_res["result"]["provider"], "gcp_vertex_ai");
    assert_eq!(list_res["result"]["current"], "claude-opus-5-5");
}

#[tokio::test(flavor = "multi_thread")]
async fn test_live_model_switch_with_stub_goose() {
    let Some(goose) = StubGoose::start_or_skip().await else {
        return;
    };
    let h = Honkd::spawn(&goose.url(), goose.secret_file(), &[]).await;
    let mut v1 = Viewer::connect(&h).await;

    // session/new
    let sid = new_session(&mut v1, goose.cwd().to_str().unwrap()).await;

    // session/set_config_option configId model value "stub-model-2"
    let switch_res = v1
        .call(
            "session/set_config_option",
            json!({
                "sessionId": sid,
                "configId": "model",
                "value": "stub-model-2",
            }),
        )
        .await;
    assert!(
        switch_res.get("result").is_some(),
        "switch_res error: {:?}",
        switch_res.get("error")
    );
    let opts = switch_res["result"]["configOptions"]
        .as_array()
        .expect("configOptions array");
    let model_opt = opts
        .iter()
        .find(|o| o["id"] == "model")
        .expect("model option");
    assert_eq!(model_opt["currentValue"], "stub-model-2");

    // _honk/model/list reports current = that id
    let list_res = v1
        .call("_honk/model/list", json!({ "sessionId": sid }))
        .await;
    assert_eq!(list_res["result"]["current"], "stub-model-2");

    // a second viewer attaching sees it in its session/load reply
    let mut v2 = Viewer::connect(&h).await;
    let load_v2 = v2.call("session/load", json!({ "sessionId": sid })).await;
    let opts_v2 = load_v2["result"]["configOptions"]
        .as_array()
        .expect("configOptions array in session/load");
    let model_v2 = opts_v2
        .iter()
        .find(|o| o["id"] == "model")
        .expect("model option in load");
    assert_eq!(model_v2["currentValue"], "stub-model-2");
}
