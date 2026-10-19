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
            .contains("'slack' can write; use /mcp on! slack to confirm")
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
