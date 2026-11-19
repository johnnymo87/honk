use common::{Honkd, Viewer, prompt_params};
use honk_testgoose::FakeGoose;
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
