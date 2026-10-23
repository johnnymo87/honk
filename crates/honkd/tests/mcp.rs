use common::{Honkd, Viewer, prompt_params};
use honk_testgoose::FakeGoose;
use serde_json::{Value, json};
use std::time::Duration;

mod common;

fn sample_catalog_content() -> String {
    json!({
        "mcp": {
            "local-test": {
                "type": "local",
                "command": ["/bin/sh"],
                "args": ["-c", "echo hello"]
            },
            "slack": {
                "type": "local",
                "command": ["/bin/sh"],
                "args": ["-c", "echo slack"]
            },
            "slack-ro": {
                "type": "local",
                "command": ["/bin/sh"],
                "args": ["-c", "echo slack-ro"]
            }
        }
    })
    .to_string()
}

#[tokio::test(flavor = "multi_thread")]
async fn test_mcp_unconfigured_catalog_refusal() {
    let fake = FakeGoose::start().await;
    let h = Honkd::spawn(
        &fake.url(),
        fake.secret_file(),
        &["--allow-unpatched-upstream"],
    )
    .await;
    let mut v = Viewer::connect(&h).await;

    let new_res = v
        .call(
            "session/new",
            json!({
                "cwd": "/tmp",
            }),
        )
        .await;
    let sid = new_res["result"]["sessionId"].as_str().unwrap();

    let list_res = v.call("_honk/mcp/list", json!({ "sessionId": sid })).await;
    assert_eq!(list_res["error"]["code"], -32601);
    assert!(
        list_res["error"]["message"]
            .as_str()
            .unwrap()
            .contains("MCP catalog not configured (honkd --mcp-catalog)")
    );

    // session/new with _meta.honk.mcp returns _meta.honk.mcp.error
    let new_mcp_res = v
        .call(
            "session/new",
            json!({
                "cwd": "/tmp",
                "_meta": {
                    "honk": {
                        "mcp": {
                            "names": ["local-test"]
                        }
                    }
                }
            }),
        )
        .await;
    assert_eq!(
        new_mcp_res["result"]["_meta"]["honk"]["mcp"]["error"]["code"],
        -32601
    );
    assert!(
        new_mcp_res["result"]["_meta"]["honk"]["mcp"]["error"]["message"]
            .as_str()
            .unwrap()
            .contains("MCP catalog not configured (honkd --mcp-catalog)")
    );

    // Honkd with --mcp-self-grant but no catalog: POST /mcp/list returns -32601
    let h_self_grant = Honkd::spawn(
        &fake.url(),
        fake.secret_file(),
        &["--allow-unpatched-upstream", "--mcp-self-grant"],
    )
    .await;
    let mut v_self = Viewer::connect(&h_self_grant).await;
    let new_self = v_self.call("session/new", json!({ "cwd": "/tmp" })).await;
    let sid_self = new_self["result"]["sessionId"].as_str().unwrap();

    let client = reqwest::Client::new();
    let res = client
        .post(format!("http://{}/mcp/list", h_self_grant.addr))
        .header("x-secret-key", h_self_grant.secret())
        .json(&json!({ "sessionId": sid_self }))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), reqwest::StatusCode::OK);
    let res_json: Value = res.json().await.unwrap();
    assert_eq!(res_json["error"]["code"], -32601);
    assert!(
        res_json["error"]["message"]
            .as_str()
            .unwrap()
            .contains("MCP catalog not configured (honkd --mcp-catalog)")
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn test_mcp_lifecycle_e2e() {
    let fake = FakeGoose::start().await;
    let temp_cat_dir = tempfile::tempdir().unwrap();
    let cat_path = temp_cat_dir.path().join("opencode.json");
    std::fs::write(&cat_path, sample_catalog_content()).unwrap();

    let cat_str = cat_path.to_str().unwrap();
    let h = Honkd::spawn(
        &fake.url(),
        fake.secret_file(),
        &[
            "--allow-unpatched-upstream",
            "--mcp-catalog",
            cat_str,
            "--mcp-no-confirm",
            "local-test,slack-ro",
        ],
    )
    .await;

    let mut v1 = Viewer::connect(&h).await;

    // 1. Create a session
    let new_res = v1
        .call(
            "session/new",
            json!({
                "cwd": "/tmp",
            }),
        )
        .await;
    let sid = new_res["result"]["sessionId"].as_str().unwrap().to_string();

    // Attach second viewer to the same session
    let mut v2 = Viewer::connect(&h).await;
    let _ = v2
        .call(
            "_honk/mcp/list",
            json!({
                "sessionId": sid
            }),
        )
        .await;

    // 2. Initial list shows servers off
    let list_res = v1.call("_honk/mcp/list", json!({ "sessionId": sid })).await;
    assert!(list_res.get("error").is_none());
    let servers = list_res["result"]["servers"].as_array().unwrap();
    assert_eq!(servers.len(), 3);
    for s in servers {
        assert_eq!(s["enabled"], false);
    }
    let slack = servers.iter().find(|s| s["name"] == "slack").unwrap();
    assert_eq!(slack["confirm"], true);
    assert_eq!(slack["sibling"], "slack-ro");

    // 3. Confirm-required refusal
    let confirm_refuse = v1
        .call(
            "_honk/mcp/enable",
            json!({
                "sessionId": sid,
                "names": ["slack"]
            }),
        )
        .await;
    assert_eq!(confirm_refuse["error"]["code"], -32602);
    assert!(
        confirm_refuse["error"]["message"]
            .as_str()
            .unwrap()
            .contains("'slack' can write; use /mcp enable slack! to confirm")
    );

    // 3b. Non-array confirm rejection
    let non_array_enable = v1
        .call(
            "_honk/mcp/enable",
            json!({
                "sessionId": sid,
                "names": ["slack"],
                "confirm": true
            }),
        )
        .await;
    assert_eq!(non_array_enable["error"]["code"], -32602);

    let non_array_new = v1
        .call(
            "session/new",
            json!({
                "cwd": "/tmp",
                "_meta": {
                    "honk": {
                        "mcp": {
                            "names": ["local-test"],
                            "confirm": true
                        }
                    }
                }
            }),
        )
        .await;
    assert_eq!(non_array_new["error"]["code"], -32602);

    // 4. Unknown name refusal
    let unknown_refuse = v1
        .call(
            "_honk/mcp/enable",
            json!({
                "sessionId": sid,
                "names": ["unknown-srv"]
            }),
        )
        .await;
    assert_eq!(unknown_refuse["error"]["code"], -32602);
    assert!(
        unknown_refuse["error"]["message"]
            .as_str()
            .unwrap()
            .contains("unknown MCP server 'unknown-srv'")
    );

    // 5. Enable local-test
    let enable_res = v1
        .call(
            "_honk/mcp/enable",
            json!({
                "sessionId": sid,
                "names": ["local-test"]
            }),
        )
        .await;
    assert!(
        enable_res.get("error").is_none(),
        "enable failed: {enable_res}"
    );
    assert_eq!(enable_res["result"]["results"][0]["name"], "local-test");
    assert_eq!(enable_res["result"]["results"][0]["ok"], true);
    assert_eq!(enable_res["result"]["enabled"], json!(["local-test"]));

    // Viewer 1 receives notification with origin "you"
    let notif1 = v1
        .wait_for(|f| f.get("method").and_then(Value::as_str) == Some("_honk/mcp_changed"))
        .await;
    assert_eq!(notif1["params"]["origin"], "you");
    assert_eq!(notif1["params"]["change"], "enabled local-test");
    assert_eq!(notif1["params"]["enabled"], json!(["local-test"]));

    // Viewer 2 receives notification with origin "other"
    let notif2 = v2
        .wait_for(|f| f.get("method").and_then(Value::as_str) == Some("_honk/mcp_changed"))
        .await;
    assert_eq!(notif2["params"]["origin"], "other");
    assert_eq!(notif2["params"]["change"], "enabled local-test");
    assert_eq!(notif2["params"]["enabled"], json!(["local-test"]));

    // Verify list shows local-test is on
    let list_res2 = v1.call("_honk/mcp/list", json!({ "sessionId": sid })).await;
    let srv_local = list_res2["result"]["servers"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["name"] == "local-test")
        .unwrap();
    assert_eq!(srv_local["enabled"], true);

    // 6. Disable local-test
    let disable_res = v1
        .call(
            "_honk/mcp/disable",
            json!({
                "sessionId": sid,
                "names": ["local-test"]
            }),
        )
        .await;
    assert!(disable_res.get("error").is_none());
    assert_eq!(disable_res["result"]["results"][0]["name"], "local-test");
    assert_eq!(disable_res["result"]["results"][0]["ok"], true);

    let notif_dis1 = v1
        .wait_for(|f| f.get("method").and_then(Value::as_str) == Some("_honk/mcp_changed"))
        .await;
    assert_eq!(notif_dis1["params"]["origin"], "you");
    assert_eq!(notif_dis1["params"]["change"], "disabled local-test");

    let notif_dis2 = v2
        .wait_for(|f| f.get("method").and_then(Value::as_str) == Some("_honk/mcp_changed"))
        .await;
    assert_eq!(notif_dis2["params"]["origin"], "other");
    assert_eq!(notif_dis2["params"]["change"], "disabled local-test");

    // 7. Enable slack-ro, then perform raw remove forwarded upstream
    let enable_ro = v1
        .call(
            "_honk/mcp/enable",
            json!({
                "sessionId": sid,
                "names": ["slack-ro"]
            }),
        )
        .await;
    assert!(enable_ro.get("error").is_none());

    // Drain notifications for slack-ro enable
    let _ = v1
        .wait_for(|f| f.get("method").and_then(Value::as_str) == Some("_honk/mcp_changed"))
        .await;
    let _ = v2
        .wait_for(|f| f.get("method").and_then(Value::as_str) == Some("_honk/mcp_changed"))
        .await;

    // Send raw _goose/unstable/session/extensions/remove forwarded through honkd
    let raw_rm = v1
        .call(
            "_goose/unstable/session/extensions/remove",
            json!({
                "sessionId": sid,
                "extensionKey": "slack-ro"
            }),
        )
        .await;
    assert!(raw_rm.get("error").is_none());

    // Both viewers should receive _honk/mcp_changed with NO enabled field
    let notif_raw1 = v1
        .wait_for(|f| f.get("method").and_then(Value::as_str) == Some("_honk/mcp_changed"))
        .await;
    assert_eq!(notif_raw1["params"]["origin"], "you");
    assert!(notif_raw1["params"].get("enabled").is_none());

    let notif_raw2 = v2
        .wait_for(|f| f.get("method").and_then(Value::as_str) == Some("_honk/mcp_changed"))
        .await;
    assert_eq!(notif_raw2["params"]["origin"], "other");
    assert!(notif_raw2["params"].get("enabled").is_none());

    // 8. Launch: session/new with _meta.honk.mcp
    let launch_res = v1
        .call(
            "session/new",
            json!({
                "cwd": "/tmp",
                "_meta": {
                    "honk": {
                        "mcp": {
                            "names": ["local-test"]
                        }
                    }
                }
            }),
        )
        .await;
    assert!(
        launch_res.get("error").is_none(),
        "session/new failed: {launch_res}"
    );
    let launch_sid = launch_res["result"]["sessionId"]
        .as_str()
        .unwrap()
        .to_string();
    let mcp_launch_out = &launch_res["result"]["_meta"]["honk"]["mcp"];
    assert!(mcp_launch_out.get("error").is_none());
    let launch_results = mcp_launch_out["results"].as_array().unwrap();
    assert_eq!(launch_results[0]["name"], "local-test");
    assert_eq!(launch_results[0]["ok"], true);

    // Creating viewer receives _honk/mcp_changed with origin "you"
    let notif_launch = v1
        .wait_for(|f| {
            f.get("method").and_then(Value::as_str) == Some("_honk/mcp_changed")
                && f.pointer("/params/sessionId").and_then(Value::as_str) == Some(&launch_sid)
        })
        .await;
    assert_eq!(notif_launch["params"]["origin"], "you");
    assert_eq!(notif_launch["params"]["enabled"], json!(["local-test"]));

    // List on launch session shows local-test enabled
    let list_launch = v1
        .call("_honk/mcp/list", json!({ "sessionId": launch_sid }))
        .await;
    let srv_launch_local = list_launch["result"]["servers"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["name"] == "local-test")
        .unwrap();
    assert_eq!(srv_launch_local["enabled"], true);

    // 9. Busy refusal: start a prompt with run_announcement_delay on FakeGoose
    fake.set_run_announcement_delay(Some(Duration::from_millis(600)));
    let prompt_id = v1
        .request(
            "session/prompt",
            prompt_params(&sid, "test busy turn", Some("crid-busy-test")),
        )
        .await;

    // Immediately while turn is in-flight:
    let busy_res = v1
        .call(
            "_honk/mcp/enable",
            json!({
                "sessionId": sid,
                "names": ["local-test"]
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
}

#[tokio::test(flavor = "multi_thread")]
async fn test_mcp_self_grant_e2e() {
    let fake = FakeGoose::start().await;
    let temp_cat_dir = tempfile::tempdir().unwrap();
    let cat_path = temp_cat_dir.path().join("opencode.json");
    std::fs::write(&cat_path, sample_catalog_content()).unwrap();
    let cat_str = cat_path.to_str().unwrap();

    let client = reqwest::Client::new();

    // 1. Honkd WITHOUT --mcp-self-grant -> returns 404
    {
        let h_no_grant = Honkd::spawn(
            &fake.url(),
            fake.secret_file(),
            &["--allow-unpatched-upstream", "--mcp-catalog", cat_str],
        )
        .await;

        let res_list = client
            .post(format!("http://{}/mcp/list", h_no_grant.addr))
            .header("x-secret-key", h_no_grant.secret())
            .json(&json!({ "sessionId": "any" }))
            .send()
            .await
            .unwrap();
        assert_eq!(res_list.status(), reqwest::StatusCode::NOT_FOUND);

        let res_enable = client
            .post(format!("http://{}/mcp/enable", h_no_grant.addr))
            .header("x-secret-key", h_no_grant.secret())
            .json(&json!({ "sessionId": "any", "names": ["local-test"] }))
            .send()
            .await
            .unwrap();
        assert_eq!(res_enable.status(), reqwest::StatusCode::NOT_FOUND);

        let res_disable = client
            .post(format!("http://{}/mcp/disable", h_no_grant.addr))
            .header("x-secret-key", h_no_grant.secret())
            .json(&json!({ "sessionId": "any", "names": ["local-test"] }))
            .send()
            .await
            .unwrap();
        assert_eq!(res_disable.status(), reqwest::StatusCode::NOT_FOUND);
    }

    // 2. Honkd WITH --mcp-self-grant
    let h = Honkd::spawn(
        &fake.url(),
        fake.secret_file(),
        &[
            "--allow-unpatched-upstream",
            "--mcp-catalog",
            cat_str,
            "--mcp-no-confirm",
            "slack-ro,local-test",
            "--mcp-self-grant",
        ],
    )
    .await;
    let secret = h.secret();

    // Auth tests
    // 2a. Missing secret header -> 401
    let res_no_secret = client
        .post(format!("http://{}/mcp/list", h.addr))
        .json(&json!({ "sessionId": "any" }))
        .send()
        .await
        .unwrap();
    assert_eq!(res_no_secret.status(), reqwest::StatusCode::UNAUTHORIZED);

    // 2b. Wrong secret header -> 401
    let res_wrong_secret = client
        .post(format!("http://{}/mcp/list", h.addr))
        .header("x-secret-key", "wrong-secret-12345678901234567890")
        .json(&json!({ "sessionId": "any" }))
        .send()
        .await
        .unwrap();
    assert_eq!(res_wrong_secret.status(), reqwest::StatusCode::UNAUTHORIZED);

    // 2c. ?token= alone without header -> 401
    let res_token_query = client
        .post(format!("http://{}/mcp/list?token={secret}", h.addr))
        .json(&json!({ "sessionId": "any" }))
        .send()
        .await
        .unwrap();
    assert_eq!(res_token_query.status(), reqwest::StatusCode::UNAUTHORIZED);

    // Connect a honk-aware viewer to create a session
    let mut v = Viewer::connect(&h).await;
    let new_res = v.call("session/new", json!({ "cwd": "/tmp" })).await;
    let sid = new_res["result"]["sessionId"].as_str().unwrap().to_string();

    // 2d. Invalid body -> HTTP 200 with -32602 error object
    let res_bad_body = client
        .post(format!("http://{}/mcp/list", h.addr))
        .header("x-secret-key", &secret)
        .json(&json!({ "sessionId": 12345 }))
        .send()
        .await
        .unwrap();
    assert_eq!(res_bad_body.status(), reqwest::StatusCode::OK);
    let bad_body_json: Value = res_bad_body.json().await.unwrap();
    assert_eq!(bad_body_json["error"]["code"], -32602);

    // 2e. List shape
    let res_list = client
        .post(format!("http://{}/mcp/list", h.addr))
        .header("x-secret-key", &secret)
        .json(&json!({ "sessionId": sid }))
        .send()
        .await
        .unwrap();
    assert_eq!(res_list.status(), reqwest::StatusCode::OK);
    let list_json: Value = res_list.json().await.unwrap();
    assert!(list_json.get("servers").is_some());
    assert!(list_json.get("other").is_some());

    // 2f. Enable confirm-class server without confirm -> hint error (-32602)
    let res_hint = client
        .post(format!("http://{}/mcp/enable", h.addr))
        .header("x-secret-key", &secret)
        .json(&json!({
            "sessionId": sid,
            "names": ["slack"]
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(res_hint.status(), reqwest::StatusCode::OK);
    let hint_json: Value = res_hint.json().await.unwrap();
    assert_eq!(hint_json["error"]["code"], -32602);
    assert!(
        hint_json["error"]["message"]
            .as_str()
            .unwrap()
            .contains("use /mcp enable slack! to confirm")
    );

    // 2g. Enable confirm-class server with confirm but no reason -> reason error (-32602)
    let res_no_reason = client
        .post(format!("http://{}/mcp/enable", h.addr))
        .header("x-secret-key", &secret)
        .json(&json!({
            "sessionId": sid,
            "names": ["slack"],
            "confirm": ["slack"]
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(res_no_reason.status(), reqwest::StatusCode::OK);
    let no_reason_json: Value = res_no_reason.json().await.unwrap();
    assert_eq!(no_reason_json["error"]["code"], -32602);
    assert_eq!(
        no_reason_json["error"]["message"],
        "'slack' can write; give a reason saying what you will do with it"
    );

    // 2h. Enable confirm-class server with confirm and whitespace-only reason -> reason error (-32602)
    let res_ws_reason = client
        .post(format!("http://{}/mcp/enable", h.addr))
        .header("x-secret-key", &secret)
        .json(&json!({
            "sessionId": sid,
            "names": ["slack"],
            "confirm": ["slack"],
            "reason": "    "
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(res_ws_reason.status(), reqwest::StatusCode::OK);
    let ws_reason_json: Value = res_ws_reason.json().await.unwrap();
    assert_eq!(ws_reason_json["error"]["code"], -32602);

    // 2i. Enable confirm-class server with confirm + reason -> OK
    let res_ok_slack = client
        .post(format!("http://{}/mcp/enable", h.addr))
        .header("x-secret-key", &secret)
        .json(&json!({
            "sessionId": sid,
            "names": ["slack"],
            "confirm": ["slack"],
            "reason": "check alert thread"
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(res_ok_slack.status(), reqwest::StatusCode::OK);
    let ok_slack_json: Value = res_ok_slack.json().await.unwrap();
    assert!(ok_slack_json.get("results").is_some());
    assert!(
        ok_slack_json["enabled"]
            .as_array()
            .unwrap()
            .iter()
            .any(|v| v == "slack")
    );

    // Verify honk-aware viewer receives _honk/mcp_changed with origin: "agent" and reason
    let changed_frame = v
        .wait_for(|f| f.get("method").and_then(Value::as_str) == Some("_honk/mcp_changed"))
        .await;
    assert_eq!(changed_frame["params"]["origin"], "agent");
    assert_eq!(changed_frame["params"]["reason"], "check alert thread");

    // 2j. Enable no-confirm server works and needs no reason
    let res_no_confirm = client
        .post(format!("http://{}/mcp/enable", h.addr))
        .header("x-secret-key", &secret)
        .json(&json!({
            "sessionId": sid,
            "names": ["local-test"]
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(res_no_confirm.status(), reqwest::StatusCode::OK);
    let no_confirm_json: Value = res_no_confirm.json().await.unwrap();
    assert!(no_confirm_json.get("results").is_some());

    // Drain viewer notification
    let _ = v
        .wait_for(|f| f.get("method").and_then(Value::as_str) == Some("_honk/mcp_changed"))
        .await;

    // 2k. Disable works and needs no reason
    let res_disable = client
        .post(format!("http://{}/mcp/disable", h.addr))
        .header("x-secret-key", &secret)
        .json(&json!({
            "sessionId": sid,
            "names": ["local-test"]
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(res_disable.status(), reqwest::StatusCode::OK);
    let disable_json: Value = res_disable.json().await.unwrap();
    assert!(disable_json.get("results").is_some());

    // Drain viewer notification
    let _ = v
        .wait_for(|f| f.get("method").and_then(Value::as_str) == Some("_honk/mcp_changed"))
        .await;

    // 2l. Enable succeeds WHILE a turn is running
    fake.set_run_announcement_delay(Some(Duration::from_millis(800)));
    let prompt_id = v
        .request(
            "session/prompt",
            prompt_params(
                &sid,
                "test turn while self-granting",
                Some("crid-self-busy"),
            ),
        )
        .await;

    // Immediately while turn is running, call self-grant enable:
    let res_mid_turn = client
        .post(format!("http://{}/mcp/enable", h.addr))
        .header("x-secret-key", &secret)
        .json(&json!({
            "sessionId": sid,
            "names": ["local-test"]
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(res_mid_turn.status(), reqwest::StatusCode::OK);
    let mid_turn_json: Value = res_mid_turn.json().await.unwrap();
    assert!(
        mid_turn_json["enabled"]
            .as_array()
            .unwrap()
            .iter()
            .any(|v| v == "local-test")
    );

    let _ = v.response(&prompt_id).await;
    fake.set_run_announcement_delay(None);
}
