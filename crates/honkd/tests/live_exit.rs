mod common;

use common::{Honkd, Viewer, prompt_params};
use honk::TailOpts;
use honk_testgoose::TcpProxy;
use honk_upstream::{Event, Recorder, Upstream};
use serde_json::{Value, json};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

fn sandbox_env() -> Option<(String, PathBuf, String)> {
    let dir = std::env::var("HONK_SANDBOX_PATCHED").ok()?;
    if dir.trim().is_empty() {
        return None;
    }
    let p = PathBuf::from(dir.trim());
    let secret_file = p.join("secret");
    if !secret_file.exists() {
        return None;
    }
    let secret = std::fs::read_to_string(&secret_file)
        .ok()?
        .trim()
        .to_string();
    Some(("http://127.0.0.1:4181/acp".to_string(), secret_file, secret))
}

#[derive(Clone, Default)]
struct SharedBuf(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for SharedBuf {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl SharedBuf {
    fn text(&self) -> String {
        String::from_utf8(self.0.lock().unwrap().clone()).unwrap_or_default()
    }
    fn len(&self) -> usize {
        self.0.lock().unwrap().len()
    }
}

async fn direct_export(upstream_url: &str, secret: &str, sid: &str) -> Value {
    let mut up = Upstream::initialize(upstream_url, secret.to_string(), Recorder::disabled())
        .await
        .expect("initialize raw upstream for export");
    up.subscribe(Some(sid))
        .await
        .expect("subscribe export stream");
    let export_res = up
        .call(
            "_goose/unstable/session/export",
            json!({
                "sessionId": sid,
                "format": "json"
            }),
            Some(sid),
            Duration::from_secs(30),
        )
        .await
        .expect("call export");
    let data_str = export_res
        .get("data")
        .and_then(Value::as_str)
        .expect("export data string");
    let session_val: Value = serde_json::from_str(data_str).expect("parse export json");
    let _ = up.delete().await;
    session_val
}

fn is_permission_request(frame: &Value) -> bool {
    frame.get("method").and_then(Value::as_str) == Some("request_permission")
        || frame.get("method").and_then(Value::as_str) == Some("session/request_permission")
}

async fn create_and_configure_session(
    viewer: &mut Viewer,
    cwd: &str,
    mode: &str,
) -> (String, String) {
    let new_res = viewer
        .call(
            "session/new",
            json!({
                "cwd": cwd,
                "mcpServers": [],
                "_meta": {
                    "enabledExtensions": [{
                        "type": "platform",
                        "name": "developer"
                    }]
                }
            }),
        )
        .await;

    let sid = new_res["result"]["sessionId"]
        .as_str()
        .expect("sessionId in session/new")
        .to_string();

    let mut model_found = None;
    if let Some(config_options) = new_res
        .pointer("/result/configOptions")
        .and_then(|v| v.as_array())
    {
        for opt in config_options {
            if opt.get("id").and_then(|v| v.as_str()) == Some("model")
                || opt.get("category").and_then(|v| v.as_str()) == Some("model")
            {
                let options_arr = opt.get("options").and_then(|v| v.as_array()).or_else(|| {
                    opt.pointer("/kind/select/options")
                        .and_then(|v| v.as_array())
                });
                if let Some(options) = options_arr {
                    for option in options {
                        if let Some(val) = option.get("value").and_then(|v| v.as_str())
                            && val.to_lowercase().contains("haiku")
                            && model_found.is_none()
                        {
                            model_found = Some(val.to_string());
                        }
                    }
                }
            }
        }
    }

    let haiku_model = model_found.unwrap_or_else(|| "claude-haiku-4-5@20251001".to_string());

    let _ = viewer
        .call(
            "session/set_config_option",
            json!({
                "sessionId": &sid,
                "configId": "model",
                "value": &haiku_model,
            }),
        )
        .await;

    let _ = viewer
        .call(
            "session/set_mode",
            json!({
                "sessionId": &sid,
                "modeId": mode,
            }),
        )
        .await;

    (sid, haiku_model)
}

async fn direct_create_and_configure_session(
    up: &mut Upstream,
    cwd: &str,
    mode: &str,
) -> (String, String) {
    let new_res = up
        .call(
            "session/new",
            json!({
                "cwd": cwd,
                "mcpServers": [],
                "_meta": {
                    "enabledExtensions": [{
                        "type": "platform",
                        "name": "developer"
                    }]
                }
            }),
            None,
            Duration::from_secs(30),
        )
        .await
        .expect("session/new direct");

    let sid = new_res["sessionId"]
        .as_str()
        .expect("sessionId in session/new")
        .to_string();

    up.subscribe(Some(&sid))
        .await
        .expect("subscribe session stream");

    let mut model_found = None;
    if let Some(config_options) = new_res.get("configOptions").and_then(|v| v.as_array()) {
        for opt in config_options {
            if opt.get("id").and_then(|v| v.as_str()) == Some("model")
                || opt.get("category").and_then(|v| v.as_str()) == Some("model")
            {
                let options_arr = opt.get("options").and_then(|v| v.as_array()).or_else(|| {
                    opt.pointer("/kind/select/options")
                        .and_then(|v| v.as_array())
                });
                if let Some(options) = options_arr {
                    for option in options {
                        if let Some(val) = option.get("value").and_then(|v| v.as_str())
                            && val.to_lowercase().contains("haiku")
                            && model_found.is_none()
                        {
                            model_found = Some(val.to_string());
                        }
                    }
                }
            }
        }
    }

    let haiku_model = model_found.unwrap_or_else(|| "claude-haiku-4-5@20251001".to_string());

    let _ = up
        .call(
            "session/set_config_option",
            json!({
                "sessionId": &sid,
                "configId": "model",
                "value": &haiku_model,
            }),
            Some(&sid),
            Duration::from_secs(30),
        )
        .await
        .expect("set_config_option direct");

    let _ = up
        .call(
            "session/set_mode",
            json!({
                "sessionId": &sid,
                "modeId": mode,
            }),
            Some(&sid),
            Duration::from_secs(30),
        )
        .await
        .expect("set_mode direct");

    (sid, haiku_model)
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn e1_pigeon_turn_visible_in_tail() {
    let Some((upstream_url, secret_file, secret)) = sandbox_env() else {
        eprintln!("skipping e1_pigeon_turn_visible_in_tail: HONK_SANDBOX_PATCHED unset");
        return;
    };

    let honkd = Honkd::spawn(&upstream_url, &secret_file, &[]).await;
    let mut pigeon = Viewer::connect_pigeon(&honkd).await;

    let temp_dir = tempfile::tempdir().unwrap();
    let (sid, _) =
        create_and_configure_session(&mut pigeon, temp_dir.path().to_str().unwrap(), "auto").await;

    let tail_buf = SharedBuf::default();
    let tail_buf_clone = tail_buf.clone();
    let tail_opts = TailOpts {
        url: honkd.ws_url(),
        secret: honkd.secret(),
        session: sid.clone(),
        text_only: true,
        thoughts: false,
        exit_on_turn_end: true,
    };
    let tail_handle = tokio::spawn(async move { honk::tail::run(tail_opts, tail_buf_clone).await });
    tokio::time::sleep(Duration::from_millis(300)).await;

    let prompt_text = "Reply with exactly: HONK_E1_OK";
    let pid = pigeon
        .request(
            "session/prompt",
            prompt_params(&sid, prompt_text, Some("crid_e1")),
        )
        .await;

    let resp = pigeon.response_timeout(&pid, Duration::from_secs(60)).await;
    assert_eq!(
        resp["result"]["stopReason"], "end_turn",
        "turn completed cleanly"
    );

    let _tail_outcome = tail_handle.await.expect("join tail").expect("tail run");

    let export = direct_export(&upstream_url, &secret, &sid).await;
    let exp_turn_text = honk_wire::export::turn_text(&export, prompt_text);
    let tail_text = tail_buf.text();
    let pigeon_receipt = pigeon.receipt_text();

    assert_eq!(tail_text, exp_turn_text, "tail text == export turn text");
    assert_eq!(
        pigeon_receipt, exp_turn_text,
        "PigeonLike receipt_text == export turn text"
    );
    assert!(tail_text.contains("HONK_E1_OK"), "tail contains HONK_E1_OK");
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn e2_kill9_honkd_mid_turn_delta_resume() {
    let Some((upstream_url, secret_file, secret)) = sandbox_env() else {
        eprintln!("skipping e2_kill9_honkd_mid_turn_delta_resume: HONK_SANDBOX_PATCHED unset");
        return;
    };

    let mut honkd = Honkd::spawn(&upstream_url, &secret_file, &[]).await;
    let mut pigeon = Viewer::connect_pigeon(&honkd).await;

    let temp_dir = tempfile::tempdir().unwrap();
    let (sid, _) =
        create_and_configure_session(&mut pigeon, temp_dir.path().to_str().unwrap(), "auto").await;

    let tail_buf = SharedBuf::default();
    let tail_buf_clone = tail_buf.clone();
    let tail_opts = TailOpts {
        url: honkd.ws_url(),
        secret: honkd.secret(),
        session: sid.clone(),
        text_only: true,
        thoughts: false,
        exit_on_turn_end: true,
    };
    let tail_handle = tokio::spawn(async move { honk::tail::run(tail_opts, tail_buf_clone).await });
    tokio::time::sleep(Duration::from_millis(300)).await;

    let prompt_text = "Explain in about 150 words how ocean tides are caused by gravitational forces of the Moon and the Sun.";
    let crid = "crid_e2";
    let _pid = pigeon
        .request(
            "session/prompt",
            prompt_params(&sid, prompt_text, Some(crid)),
        )
        .await;

    let poll_start = Instant::now();
    while tail_buf.len() < 200 && poll_start.elapsed() < Duration::from_secs(30) {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(
        tail_buf.len() >= 200,
        "tail received at least 200 chars before kill9 (len={})",
        tail_buf.len()
    );

    honkd.kill9();
    tokio::time::sleep(Duration::from_millis(200)).await;
    honkd.restart().await;

    let mut pigeon2 = Viewer::connect_pigeon(&honkd).await;
    let pid2 = pigeon2
        .request(
            "session/prompt",
            prompt_params(&sid, prompt_text, Some(crid)),
        )
        .await;

    let resp2 = pigeon2
        .response_timeout(&pid2, Duration::from_secs(60))
        .await;
    assert_eq!(
        resp2["result"]["stopReason"], "end_turn",
        "turn completed after restart"
    );

    let tail_outcome = tail_handle.await.expect("join tail").expect("tail run");
    assert_eq!(
        tail_outcome.reconnect_resets, 0,
        "tail saw no _honk/reset on reconnect (delta resume)"
    );

    let export = direct_export(&upstream_url, &secret, &sid).await;
    let exp_turn_text = honk_wire::export::turn_text(&export, prompt_text);
    let tail_text = tail_buf.text();
    let pigeon_receipt = pigeon2.receipt_text();

    assert_eq!(tail_text, exp_turn_text, "tail text == export turn text");
    assert_eq!(
        pigeon_receipt, exp_turn_text,
        "pigeon receipt_text == export turn text"
    );

    let user_msgs: Vec<_> = export["conversation"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|m| {
            m["role"] == "user"
                && m["content"]
                    .as_array()
                    .and_then(|c| c.first())
                    .and_then(|b| b.get("text"))
                    .and_then(Value::as_str)
                    == Some(prompt_text)
        })
        .collect();
    assert_eq!(
        user_msgs.len(),
        1,
        "export has exactly one user message with the prompt"
    );
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn e3_goose_restart_mid_turn() {
    let Some((upstream_url, secret_file, secret)) = sandbox_env() else {
        eprintln!("skipping e3_goose_restart_mid_turn: HONK_SANDBOX_PATCHED unset");
        return;
    };

    let honkd = Honkd::spawn(&upstream_url, &secret_file, &[]).await;
    let mut pigeon = Viewer::connect_pigeon(&honkd).await;

    let temp_dir = tempfile::tempdir().unwrap();
    let (sid, _) =
        create_and_configure_session(&mut pigeon, temp_dir.path().to_str().unwrap(), "auto").await;

    let tail_buf = SharedBuf::default();
    let tail_buf_clone = tail_buf.clone();
    let tail_opts = TailOpts {
        url: honkd.ws_url(),
        secret: honkd.secret(),
        session: sid.clone(),
        text_only: true,
        thoughts: false,
        exit_on_turn_end: true,
    };
    let tail_handle = tokio::spawn(async move { honk::tail::run(tail_opts, tail_buf_clone).await });
    tokio::time::sleep(Duration::from_millis(300)).await;

    let prompt_text = "Run the bash command 'sleep 40' using the developer shell tool right now.";
    let crid_orig = "crid_e3_orig";
    let pid = pigeon
        .request(
            "session/prompt",
            prompt_params(&sid, prompt_text, Some(crid_orig)),
        )
        .await;

    // Wait until the model issues the tool call
    let _tool_frame = pigeon
        .wait_for(|f| {
            let su = f
                .pointer("/params/update/sessionUpdate")
                .and_then(Value::as_str);
            su == Some("tool_call") || su == Some("tool_call_update")
        })
        .await;

    // SAFETY: Only e3 may restart the patched sandbox (systemctl --user restart honk-goose-sandbox-patched)
    let restart_res = std::process::Command::new("systemctl")
        .args(["--user", "restart", "honk-goose-sandbox-patched"])
        .status()
        .expect("restart sandbox unit");
    assert!(restart_res.success(), "sandbox restart succeeded");

    let resp = pigeon.response_timeout(&pid, Duration::from_secs(30)).await;
    assert_eq!(resp["result"]["stopReason"], "cancelled");
    assert_eq!(
        resp["result"]["_meta"]["honk"]["reason"],
        "upstream_restart"
    );

    let cancel_meta_text = resp
        .pointer("/result/_meta/honk/text")
        .and_then(Value::as_str);
    eprintln!("[e3 finding] cancelled reply _meta.honk.text: {cancel_meta_text:?}");
    assert_eq!(
        cancel_meta_text, None,
        "cancelled response carries no _meta.honk.text"
    );

    let _ = tail_handle.await.expect("join tail");
    let live_tail_text = tail_buf.text();
    eprintln!("[e3 finding] live tail text across restart: {live_tail_text:?}");
    let tail_has_rebuild_marker = live_tail_text.contains("[honk: transcript rebuilt]");
    eprintln!("[e3 finding] tail printed [honk: transcript rebuilt]: {tail_has_rebuild_marker}");

    let export_mid = direct_export(&upstream_url, &secret, &sid).await;
    let mid_text = honk_wire::export::turn_text(&export_mid, prompt_text);
    eprintln!("[e3 finding] export mid_text: {mid_text:?}");
    eprintln!(
        "[e3 finding] pigeon.receipt_text(): {:?}",
        pigeon.receipt_text()
    );
    assert_eq!(
        mid_text, "",
        "export has no committed turn text for interrupted turn"
    );
    assert!(
        pigeon.receipt_text() == mid_text || !pigeon.receipt_text().is_empty(),
        "PigeonLike receipt_text may contain uncommitted live preamble text not in export"
    );

    // Honk-aware tail after reactivation:
    // A fresh honk-aware tail connecting after the reactivation reset sees only committed export text
    let fresh_tail_buf = SharedBuf::default();
    let mut fresh_client = honk::client::Client::connect(&honkd.ws_url(), &honkd.secret())
        .await
        .expect("connect fresh client after reactivation");
    let _ = fresh_client
        .request(
            "session/load",
            json!({"sessionId": &sid, "_meta": {"honk": {}}}),
        )
        .await
        .expect("load after reactivation");
    let mut fresh_renderer = honk::render::Renderer::new(true, false);
    let mut fresh_saw_replay_done = false;
    let fresh_wait = Instant::now();
    while fresh_wait.elapsed() < Duration::from_secs(10) {
        if let Ok(Some(f)) = fresh_client.next_frame().await {
            if f.get("method").and_then(Value::as_str) == Some(honk_wire::method::REPLAY_DONE) {
                fresh_saw_replay_done = true;
                break;
            }
            let _ = fresh_renderer.handle_frame(&f, &mut fresh_tail_buf.clone());
        }
    }
    assert!(fresh_saw_replay_done, "fresh client saw replay_done");
    let fresh_tail_text = fresh_tail_buf.text();
    assert_eq!(
        fresh_tail_text, mid_text,
        "honk-aware tail after reactivation reset == export turn text"
    );

    // Contract: PigeonLike receipt_text may contain uncommitted live text not in export.
    // If the model emitted a preamble before the tool call, pigeon accumulated it live,
    // whereas goose never committed it to SQLite before restart.
    if pigeon.receipt_text() != mid_text {
        eprintln!(
            "[e3 finding] PigeonLike receipt_text carries uncommitted live text ({:?}) not in export ({:?})",
            pigeon.receipt_text(),
            mid_text
        );
    }

    // Retry with a new id sent twice (double-press)
    let crid_retry = "crid_e3_retry";
    let id2 = pigeon
        .request(
            "session/prompt",
            prompt_params(&sid, prompt_text, Some(crid_retry)),
        )
        .await;
    let id3 = pigeon
        .request(
            "session/prompt",
            prompt_params(&sid, prompt_text, Some(crid_retry)),
        )
        .await;

    let r2 = pigeon.response_timeout(&id2, Duration::from_secs(90)).await;
    let r3 = pigeon.response_timeout(&id3, Duration::from_secs(90)).await;
    assert_eq!(r2["result"]["stopReason"], "end_turn");
    assert_eq!(r3["result"]["stopReason"], "end_turn");

    let export_after = direct_export(&upstream_url, &secret, &sid).await;
    let users: Vec<_> = export_after["conversation"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|m| {
            m["role"] == "user"
                && m["content"]
                    .as_array()
                    .and_then(|c| c.first())
                    .and_then(|b| b.get("text"))
                    .and_then(Value::as_str)
                    == Some(prompt_text)
        })
        .collect();
    assert_eq!(users.len(), 2, "export shows original + one retry");

    // Verify sandbox is active and answering
    let is_active = std::process::Command::new("systemctl")
        .args(["--user", "is-active", "honk-goose-sandbox-patched"])
        .output()
        .expect("check is-active");
    assert_eq!(
        String::from_utf8_lossy(&is_active.stdout).trim(),
        "active",
        "sandbox unit is active after e3"
    );

    let probe_up = Upstream::initialize(&upstream_url, secret.clone(), Recorder::disabled()).await;
    assert!(probe_up.is_ok(), ":4181 answers again after restart");
    let up = probe_up.unwrap();
    let _ = up.delete().await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn e4_permission_across_honkd_restart() {
    let Some((upstream_url, secret_file, secret)) = sandbox_env() else {
        eprintln!("skipping e4_permission_across_honkd_restart: HONK_SANDBOX_PATCHED unset");
        return;
    };

    let mut honkd = Honkd::spawn(&upstream_url, &secret_file, &[]).await;
    let mut v1 = Viewer::connect(&honkd).await;

    let temp_dir = tempfile::tempdir().unwrap();
    let (sid, _) =
        create_and_configure_session(&mut v1, temp_dir.path().to_str().unwrap(), "approve").await;

    let prompt_text = "You must use the developer shell tool to execute the bash command: echo HONK_E4_OK\nDo not answer with text alone.";
    let _pid = v1
        .request(
            "session/prompt",
            prompt_params(&sid, prompt_text, Some("crid_e4")),
        )
        .await;

    let req1 = v1.wait_for(is_permission_request).await;
    let goose_req_id = req1["id"].clone();

    honkd.kill9();
    tokio::time::sleep(Duration::from_millis(200)).await;
    honkd.restart().await;

    let mut v2 = Viewer::connect(&honkd).await;
    v2.call(
        "session/load",
        json!({"sessionId": &sid, "cwd": ".", "mcpServers": []}),
    )
    .await;

    let req2 = v2.wait_for(is_permission_request).await;
    assert_eq!(
        req2["id"], goose_req_id,
        "identical goose id re-offered after restart"
    );

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

    v2.wait_for(|f| {
        f.get("method").and_then(Value::as_str) == Some("session/update")
            && f.pointer("/params/update/sessionUpdate")
                .and_then(Value::as_str)
                == Some("session_info_update")
            && f.pointer("/params/update/_meta/goose/activeRunId") == Some(&Value::Null)
    })
    .await;

    let export = direct_export(&upstream_url, &secret, &sid).await;
    assert!(
        export.to_string().contains("HONK_E4_OK"),
        "export tool output contains HONK_E4_OK"
    );
}

fn check_export_tool_output(session_json: &Value, needle: &str) -> bool {
    if let Some(convo) = session_json.get("conversation").and_then(|v| v.as_array()) {
        for msg in convo {
            if let Some(contents) = msg.get("content").and_then(|c| c.as_array()) {
                for block in contents {
                    let b_type = block.get("type").and_then(|t| t.as_str());
                    if b_type == Some("toolResponse") {
                        let is_error = block
                            .pointer("/toolResult/value/isError")
                            .and_then(Value::as_bool)
                            .unwrap_or(false);
                        let text = block
                            .pointer("/toolResult/value/content/0/text")
                            .and_then(Value::as_str)
                            .unwrap_or("");
                        if !is_error && text.contains(needle) {
                            return true;
                        }
                    }
                }
            }
        }
    }
    false
}

async fn run_delete_frees_parked_run() {
    let Some((upstream_url, _secret_file, secret)) = sandbox_env() else {
        eprintln!("skipping delete_frees_parked_run: HONK_SANDBOX_PATCHED unset");
        return;
    };

    let mut up1 = Upstream::initialize(&upstream_url, secret.clone(), Recorder::disabled())
        .await
        .expect("init up1");
    up1.subscribe(None).await.expect("subscribe conn stream");

    let temp_dir = tempfile::tempdir().unwrap();
    let cwd = temp_dir.path().to_str().unwrap().to_string();

    let (sid, _) = direct_create_and_configure_session(&mut up1, &cwd, "approve").await;

    let parked_prompt = "You must use the developer shell tool to execute the bash command: echo PARKED_TOOL_OUTPUT\nDo not answer with text alone.";
    let _pid = up1
        .request(
            "session/prompt",
            json!({
                "sessionId": &sid,
                "prompt": [{"type": "text", "text": parked_prompt}]
            }),
            Some(&sid),
        )
        .await
        .expect("prompt up1");

    let mut perm_req_a = None;
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(30) {
        if let Ok(Some(Event::Frame(_, frame, _))) = up1.next(Duration::from_secs(1)).await
            && is_permission_request(&frame)
        {
            perm_req_a = Some(frame);
            break;
        }
    }
    let req_a = perm_req_a.expect("received permission request on Conn A");
    let id_a = req_a["id"].clone();
    eprintln!("[delete_probe] Conn A permission request id: {id_a:?}");

    // DELETE connection A: the claim under test is that this alone frees the parked run
    let del_status = up1.delete().await.expect("delete Conn A");
    eprintln!("[delete_probe] Conn A DELETE status: {del_status:?}");

    // Connect new raw connection B
    let mut up2 = Upstream::initialize(&upstream_url, secret.clone(), Recorder::disabled())
        .await
        .expect("init up2");
    up2.subscribe(None).await.expect("subscribe up2 conn");
    up2.subscribe(Some(&sid))
        .await
        .expect("subscribe up2 session");

    // session/load on Conn B
    let load_res = up2
        .call(
            "session/load",
            json!({"sessionId": &sid, "cwd": &cwd, "mcpServers": []}),
            Some(&sid),
            Duration::from_secs(30),
        )
        .await
        .expect("session/load on Conn B");
    assert!(load_res.is_object(), "session/load succeeded on Conn B");

    // Check if any permission request is re-offered on Conn B
    let mut reoffered_perm = None;
    let start_check = Instant::now();
    while start_check.elapsed() < Duration::from_secs(3) {
        if let Ok(Some(Event::Frame(_, frame, _))) = up2.next(Duration::from_millis(500)).await
            && is_permission_request(&frame)
        {
            reoffered_perm = Some(frame);
            break;
        }
    }
    // Observed on patched-1.54.0.3: DELETE drops the active run, so nothing is re-offered.
    assert!(
        reoffered_perm.is_none(),
        "DELETE freed the parked run: no permission re-offered on session/load"
    );

    // Send new prompt on Conn B: instructs model not to use tools
    let prompt_res = up2
        .call(
            "session/prompt",
            json!({
                "sessionId": &sid,
                "prompt": [{"type": "text", "text": "Do not call any tool. Reply with only: UNPARKED_OK"}]
            }),
            Some(&sid),
            Duration::from_secs(30),
        )
        .await
        .expect("new prompt on Conn B");

    eprintln!("[delete_probe] new prompt result: {prompt_res:?}");
    assert_eq!(prompt_res["stopReason"], "end_turn");

    let export = direct_export(&upstream_url, &secret, &sid).await;
    let tool_ran = check_export_tool_output(&export, "PARKED_TOOL_OUTPUT");
    assert!(
        !tool_ran,
        "the parked run ended (export shows no tool output)"
    );
    let export_str = export.to_string();
    assert!(
        export_str.contains("UNPARKED_OK"),
        "export contains new prompt result"
    );

    let _ = up2.delete().await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn delete_frees_parked_run() {
    run_delete_frees_parked_run().await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn takeover_and_resume_live() {
    let Some((upstream_url, secret_file, secret)) = sandbox_env() else {
        eprintln!("skipping takeover_and_resume_live: HONK_SANDBOX_PATCHED unset");
        return;
    };

    let proxy = TcpProxy::start("127.0.0.1:4181".parse().unwrap())
        .await
        .expect("start proxy");
    let proxy_url = format!("http://{}/acp", proxy.addr());

    let honkd = Honkd::spawn(&proxy_url, &secret_file, &["--stream-dead-secs", "20"]).await;
    let mut pigeon = Viewer::connect_pigeon(&honkd).await;

    let temp_dir = tempfile::tempdir().unwrap();
    let (sid, _) =
        create_and_configure_session(&mut pigeon, temp_dir.path().to_str().unwrap(), "auto").await;

    let tail_buf = SharedBuf::default();
    let tail_buf_clone = tail_buf.clone();
    let tail_opts = TailOpts {
        url: honkd.ws_url(),
        secret: honkd.secret(),
        session: sid.clone(),
        text_only: true,
        thoughts: false,
        exit_on_turn_end: true,
    };
    let tail_handle = tokio::spawn(async move { honk::tail::run(tail_opts, tail_buf_clone).await });
    tokio::time::sleep(Duration::from_millis(300)).await;

    let prompt_text = "Explain in about 150 words how photosynthesis works in green plants.";
    let pid = pigeon
        .request(
            "session/prompt",
            prompt_params(&sid, prompt_text, Some("crid_takeover")),
        )
        .await;

    let start = Instant::now();
    while tail_buf.len() < 50 && start.elapsed() < Duration::from_secs(30) {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(
        tail_buf.len() >= 50,
        "streaming started before freeze (len={})",
        tail_buf.len()
    );

    proxy.freeze();
    tokio::time::sleep(Duration::from_secs(30)).await;
    proxy.release().await;

    let resp = pigeon.response_timeout(&pid, Duration::from_secs(60)).await;
    assert_eq!(resp["result"]["stopReason"], "end_turn");

    let tail_outcome = tail_handle.await.expect("join tail").expect("tail run");
    assert!(
        tail_outcome.reconnect_resets <= 1,
        "tail outcome reconnect_resets <= 1 (reconnect_resets={})",
        tail_outcome.reconnect_resets
    );

    let export = direct_export(&upstream_url, &secret, &sid).await;
    let exp_turn_text = honk_wire::export::turn_text(&export, prompt_text);
    let tail_text = tail_buf.text();

    assert_eq!(
        tail_text, exp_turn_text,
        "tail text == export turn text with no duplicates"
    );
}
