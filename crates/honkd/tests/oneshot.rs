//! `_honk/oneshot` (honk-9qf.31.4): a model call on a throwaway, deleted goose session.

use common::{Honkd, Viewer};
use honk_testgoose::FakeGoose;
use serde_json::{Value, json};
use std::time::Duration;

mod common;

fn state(h: &Honkd) -> Value {
    let raw = std::fs::read_to_string(h.state_dir.path().join("state.json")).unwrap();
    serde_json::from_str(&raw).unwrap()
}

fn sent(fake: &FakeGoose, method: &str) -> Vec<Value> {
    fake.inbound()
        .into_iter()
        .filter(|f| f.get("method").and_then(Value::as_str) == Some(method))
        .collect()
}

async fn wait_until(what: &str, f: impl Fn() -> bool) {
    let start = std::time::Instant::now();
    while !f() {
        assert!(start.elapsed() < Duration::from_secs(10), "never: {what}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn oneshot_returns_text_and_deletes_its_session() {
    let fake = FakeGoose::start().await;
    fake.script_prompt(vec!["{\"keep\"".into(), ":[]}".into()]);
    let h = Honkd::spawn(
        &fake.url(),
        fake.secret_file(),
        &["--allow-unpatched-upstream"],
    )
    .await;
    let mut v = Viewer::connect(&h).await;

    let res = v
        .call(
            "_honk/oneshot",
            json!({ "oneshotId": "q1", "system": "SYS", "prompt": "PROMPT", "effort": "low" }),
        )
        .await;
    assert_eq!(res["result"]["text"], "{\"keep\":[]}", "{res}");
    assert_eq!(res["result"]["model"], "claude-opus-5-5", "{res}");

    let new = sent(&fake, "session/new");
    assert_eq!(new.len(), 1);
    let meta = &new[0]["params"]["_meta"];
    assert_eq!(meta["hidden"], true);
    assert_eq!(meta["enabledExtensions"], json!([]));
    assert!(meta["sessionTitle"].is_string());
    let sys = sent(&fake, "_goose/unstable/session/system-prompt/set");
    assert_eq!(sys[0]["params"]["mode"], "set");
    assert_eq!(sys[0]["params"]["text"], "SYS");
    let cfg = sent(&fake, "session/set_config_option");
    assert_eq!(cfg[0]["params"]["configId"], "thinking_effort");
    assert_eq!(cfg[0]["params"]["value"], "low");
    assert!(
        cfg.iter().all(|f| f["params"]["configId"] != "model"),
        "goose's default model is used"
    );
    let prompt = sent(&fake, "session/prompt");
    assert_eq!(prompt[0]["params"]["prompt"][0]["text"], "PROMPT");

    let sid = prompt[0]["params"]["sessionId"]
        .as_str()
        .unwrap()
        .to_string();
    wait_until("session deleted", || fake.deleted_sessions().contains(&sid)).await;
    wait_until("state clean", || {
        let st = state(&h);
        st["oneshot_sessions"] == json!([]) && st["orphan_conns"] == json!([])
    })
    .await;
    assert!(
        state(&h)["sessions"].as_object().unwrap().is_empty(),
        "not a held session"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn oneshot_cancel_cancels_the_prompt_and_still_cleans_up() {
    let fake = FakeGoose::start().await;
    fake.script_prompt((0..500).map(|i| format!("{i} ")).collect());
    let h = Honkd::spawn(
        &fake.url(),
        fake.secret_file(),
        &["--allow-unpatched-upstream"],
    )
    .await;
    let mut v = Viewer::connect(&h).await;

    let id = v
        .request(
            "_honk/oneshot",
            json!({ "oneshotId": "q2", "system": "", "prompt": "P" }),
        )
        .await;
    wait_until("prompt sent", || !sent(&fake, "session/prompt").is_empty()).await;
    let c = v
        .call("_honk/oneshot/cancel", json!({ "oneshotId": "q2" }))
        .await;
    assert_eq!(c["result"]["cancelled"], true);
    let res = v.response(&id).await;
    assert_eq!(res["error"]["code"], -32800, "{res}");
    assert_eq!(sent(&fake, "session/cancel").len(), 1);
    let sid = sent(&fake, "session/prompt")[0]["params"]["sessionId"]
        .as_str()
        .unwrap()
        .to_string();
    wait_until("session deleted", || fake.deleted_sessions().contains(&sid)).await;

    let c = v
        .call("_honk/oneshot/cancel", json!({ "oneshotId": "nope" }))
        .await;
    assert_eq!(c["result"]["cancelled"], false);
}

#[tokio::test(flavor = "multi_thread")]
async fn oneshot_rejects_missing_prompt() {
    let fake = FakeGoose::start().await;
    let h = Honkd::spawn(
        &fake.url(),
        fake.secret_file(),
        &["--allow-unpatched-upstream"],
    )
    .await;
    let mut v = Viewer::connect(&h).await;
    let res = v.call("_honk/oneshot", json!({ "system": "s" })).await;
    assert_eq!(res["error"]["code"], -32602);
    assert!(sent(&fake, "session/new").is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn startup_sweeps_oneshot_sessions_a_dead_honkd_left() {
    let fake = FakeGoose::start().await;
    let mut h = Honkd::spawn(
        &fake.url(),
        fake.secret_file(),
        &["--allow-unpatched-upstream"],
    )
    .await;
    h.kill9();
    let path = h.state_dir.path().join("state.json");
    let mut st: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    st["oneshot_sessions"] = json!(["ses_left"]);
    std::fs::write(&path, st.to_string()).unwrap();
    h.restart().await;
    wait_until("swept", || {
        fake.deleted_sessions().contains(&"ses_left".to_string())
    })
    .await;
    wait_until("state clean", || state(&h)["oneshot_sessions"] == json!([])).await;
}

/// Against a real goose serve (stub model): the side session starts no tools, uses our system
/// prompt, answers, and is gone from goose afterwards.
#[tokio::test(flavor = "multi_thread")]
async fn oneshot_against_real_goose_uses_no_tools_and_our_system_prompt() {
    use honk_testgoose::{StubGoose, Turn};
    let Some(g) = StubGoose::start_or_skip().await else {
        return;
    };
    g.model()
        .push([Turn::Text(vec!["{\"keep\":".into(), "[]}".into()])]);
    let h = Honkd::spawn(&g.url(), g.secret_file(), &[]).await;
    let mut v = Viewer::connect(&h).await;
    let res = v
        .call(
            "_honk/oneshot",
            json!({ "system": "ONESHOT-SYSTEM-MARKER answer JSON", "prompt": "the input" }),
        )
        .await;
    // The stub only scripts requests that offer tools. goose before self_compact_and_resume
    // (CI's patched-1.54.0.3) offers none here, so the stub answers its default "ok".
    let text = res["result"]["text"].as_str().unwrap_or_default();
    assert!(text == "{\"keep\":[]}" || text == "ok", "{res}");
    let reqs: Vec<Value> = g
        .model()
        .classification_log()
        .into_iter()
        .map(|(_, r)| r)
        .filter(|r| r.to_string().contains("ONESHOT-SYSTEM-MARKER"))
        .collect();
    assert_eq!(reqs.len(), 1, "one model call carries our system prompt");
    assert_eq!(
        g.model().classification_log().len(),
        1,
        "no other model call (sessionTitle suppresses goose's naming call)"
    );
    // goose-patched always offers its own self_compact_and_resume; no extension tools.
    let tools: Vec<&str> = reqs[0]["tools"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|t| t["function"]["name"].as_str())
                .collect()
        })
        .unwrap_or_default();
    assert!(
        tools.iter().all(|t| *t == "self_compact_and_resume"),
        "no extension tools offered: {tools:?}"
    );
    wait_until("state clean", || state(&h)["oneshot_sessions"] == json!([])).await;
    let list = v.call("session/list", json!({})).await;
    assert!(
        !list.to_string().contains("honk oneshot"),
        "oneshot session not listed: {list}"
    );
}
