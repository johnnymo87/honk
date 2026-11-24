//! `_honk/quote` (honk-9qf.31.7): `/quote`'s pick and trim in honkd, read from the session's
//! log on disk, for honk chat and pigeon alike.

use common::{Honkd, Viewer};
use honk_testgoose::FakeGoose;
use serde_json::{Value, json};
use std::path::Path;
use std::time::Duration;

mod common;

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

/// A honkd session log: your prompt, then three long wake replies (well over the size
/// `/quote` trims at), and a tool call and thought that must not be quoted.
fn write_log(state_dir: &Path, sid: &str) {
    let dir = state_dir.join("sessions");
    std::fs::create_dir_all(&dir).unwrap();
    let chunk = |kind: &str, id: &str, text: &str| {
        json!({ "jsonrpc": "2.0", "method": "session/update", "params": { "sessionId": sid,
            "update": { "sessionUpdate": kind, "messageId": id,
                "content": { "type": "text", "text": text },
                "_meta": { "goose": { "created": 1_795_121_237 } } } } })
    };
    let mut frames = vec![chunk("user_message_chunk", "u1", "status?")];
    for i in 0..3 {
        let body = format!(
            "head{i}\n\n{}\nQUESTION{i}?",
            format!("filler line {i} with plenty of words\n").repeat(40)
        );
        frames.push(chunk(
            "user_message_chunk",
            &format!("w{i}"),
            "[sw wake run=r reason=mail] Woken",
        ));
        frames.push(chunk("agent_thought_chunk", &format!("t{i}"), "THOUGHT"));
        frames.push(
            json!({ "method": "session/update", "params": { "sessionId": sid,
            "update": { "sessionUpdate": "tool_call", "toolCallId": format!("c{i}"),
                "title": "TOOL" } } }),
        );
        frames.push(chunk("agent_message_chunk", &format!("a{i}"), &body));
    }
    let mut out = json!({ "epoch": 1, "resume_from": null }).to_string();
    for (i, f) in frames.iter().enumerate() {
        out.push('\n');
        out.push_str(&json!({ "seq": i + 1, "id": null, "frame": f }).to_string());
    }
    out.push('\n');
    std::fs::write(dir.join(format!("{sid}.jsonl")), out).unwrap();
}

async fn honkd(fake: &FakeGoose, extra: &[&str]) -> Honkd {
    let mut args = vec!["--allow-unpatched-upstream"];
    args.extend_from_slice(extra);
    Honkd::spawn(&fake.url(), fake.secret_file(), &args).await
}

#[tokio::test(flavor = "multi_thread")]
async fn full_quote_reads_the_log_without_loading_the_session() {
    let fake = FakeGoose::start().await;
    let h = honkd(&fake, &[]).await;
    write_log(h.state_dir.path(), "20261124_1");
    let mut v = Viewer::connect(&h).await;
    let res = v
        .call(
            "_honk/quote",
            json!({ "sessionId": "20261124_1", "pick": { "kind": "last", "n": 2 }, "full": true }),
        )
        .await;
    let r = &res["result"];
    assert_eq!(r["summary"], Value::Null, "{res}");
    assert_eq!(r["empty"], Value::Null);
    assert_eq!(r["model"], Value::Null);
    assert_eq!(
        r["notices"],
        json!(["quoted the last 2 replies, untrimmed"])
    );
    let replies = r["replies"].as_array().unwrap();
    assert_eq!(replies.len(), 2);
    let header = replies[0]["header"].as_str().unwrap();
    assert!(
        header.starts_with("# ") && header.ends_with(" (wake: mail)"),
        "{header}"
    );
    assert_eq!(replies[0]["inProgress"], false);
    let lines = replies[1]["passages"][0].as_array().unwrap();
    assert_eq!(lines[0], "head2");
    assert_eq!(lines[1], "", "blank lines kept, unquoted");
    assert_eq!(lines.last().unwrap(), "QUESTION2?");
    assert!(!res.to_string().contains("THOUGHT") && !res.to_string().contains("TOOL"));
    assert!(
        sent(&fake, "session/load").is_empty(),
        "not loaded into goose"
    );
    assert!(sent(&fake, "session/new").is_empty(), "no model call");
}

/// The trim runs on goose's default model, charged to the `tag` asked for (not the quoted
/// session's), and the same replies again reuse the answer.
#[tokio::test(flavor = "multi_thread")]
async fn trimmed_quote_uses_the_model_tags_its_spend_and_caches() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let calls = dir.path().join("calls");
    let bin = dir.path().join("session-tags");
    std::fs::write(
        &bin,
        format!(
            "#!/bin/sh\necho \"$@\" >> {}\n[ \"$1\" = which ] && printf 'stall-watcher-runs\\tmanual\\t%s\\tsession\\n' \"$2\"\nexit 0\n",
            calls.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
    let fake = FakeGoose::start().await;
    fake.script_prompt(vec![
        r#"{"keep":[{"reply":1,"lines":[[1,1]],"tag":"fyi"},"#.into(),
        r#"{"reply":3,"lines":[[44,44]],"tag":"ask"}]}"#.into(),
    ]);
    let bin_arg = bin.to_string_lossy().to_string();
    let h = honkd(&fake, &["--session-tags-bin", &bin_arg]).await;
    write_log(h.state_dir.path(), "20261124_2");
    let mut v = Viewer::connect(&h).await;
    let req = json!({ "sessionId": "20261124_2", "pick": { "kind": "since" }, "tag": "honk",
        "quoteId": "q1" });
    let res = v.call("_honk/quote", req.clone()).await;
    let r = &res["result"];
    assert_eq!(
        r["summary"], "# honk: trimmed by claude-opus-5-5, kept 2 of 126 lines from 2 of 3 replies",
        "{res}"
    );
    assert_eq!(r["model"], "claude-opus-5-5");
    assert_eq!(
        r["notices"],
        json!(["trimmed 3 replies since your last prompt to what needs you"])
    );
    let replies = r["replies"].as_array().unwrap();
    assert!(
        replies[0]["header"]
            .as_str()
            .unwrap()
            .ends_with("(wake: mail) · fyi")
    );
    assert_eq!(replies[0]["passages"], json!([["head0"]]));
    assert!(
        replies[1]["header"]
            .as_str()
            .unwrap()
            .ends_with("(wake: mail) · ask")
    );
    assert_eq!(replies[1]["passages"], json!([["QUESTION2?"]]));
    let prompt = sent(&fake, "session/prompt");
    assert_eq!(prompt.len(), 1);
    let text = prompt[0]["params"]["prompt"][0]["text"].as_str().unwrap();
    assert!(
        text.contains("<<<\nstatus?\n>>>") && text.contains("L44: QUESTION2?"),
        "{text}"
    );
    let oneshot = prompt[0]["params"]["sessionId"]
        .as_str()
        .unwrap()
        .to_string();
    assert_eq!(
        std::fs::read_to_string(&calls).unwrap_or_default(),
        format!("set honk goose:{oneshot}\n"),
        "the explicit tag, not the quoted session's"
    );
    wait_until("archived", || fake.archived_sessions().contains(&oneshot)).await;

    let again = v.call("_honk/quote", req).await;
    assert_eq!(again["result"]["replies"], res["result"]["replies"]);
    assert!(
        again["result"]["notices"][0]
            .as_str()
            .unwrap()
            .contains("reused that answer"),
        "{again}"
    );
    assert_eq!(
        sent(&fake, "session/prompt").len(),
        1,
        "cached: no second call"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn cancel_stops_the_model_calls_and_cleans_up() {
    let fake = FakeGoose::start().await;
    fake.script_prompt((0..500).map(|i| format!("{i} ")).collect());
    let h = honkd(&fake, &[]).await;
    write_log(h.state_dir.path(), "20261124_3");
    let mut v = Viewer::connect(&h).await;
    let id = v
        .request(
            "_honk/quote",
            json!({ "sessionId": "20261124_3", "pick": { "kind": "since" }, "quoteId": "q3" }),
        )
        .await;
    wait_until("prompt sent", || !sent(&fake, "session/prompt").is_empty()).await;
    let c = v
        .call("_honk/quote/cancel", json!({ "quoteId": "q3" }))
        .await;
    assert_eq!(c["result"]["cancelled"], true);
    let res = v.response(&id).await;
    assert_eq!(res["error"]["code"], -32800, "{res}");
    assert_eq!(sent(&fake, "session/cancel").len(), 1);
    let oneshot = sent(&fake, "session/prompt")[0]["params"]["sessionId"]
        .as_str()
        .unwrap()
        .to_string();
    wait_until("archived", || fake.archived_sessions().contains(&oneshot)).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn empty_and_bad_requests() {
    let fake = FakeGoose::start().await;
    let h = honkd(&fake, &[]).await;
    write_log(h.state_dir.path(), "20261124_4");
    let mut v = Viewer::connect(&h).await;
    // Your last prompt was the first; `prev` counts from it, saying so.
    let res = v
        .call(
            "_honk/quote",
            json!({ "sessionId": "20261124_4", "pick": { "kind": "prev" }, "full": true }),
        )
        .await;
    assert!(
        res["result"]["notices"][0]
            .as_str()
            .unwrap()
            .starts_with("only one prompt of yours in this session, so counting from it; "),
        "{res}"
    );
    for (params, want) in [
        (
            json!({ "sessionId": "20990101_9", "pick": { "kind": "since" } }),
            "unknown session",
        ),
        (
            json!({ "sessionId": "../state", "pick": { "kind": "since" } }),
            "invalid sessionId",
        ),
        (
            json!({ "sessionId": "20261124_4", "pick": { "kind": "last", "n": 0 } }),
            "at least 1",
        ),
        (
            json!({ "sessionId": "20261124_4", "pick": { "kind": "newest" } }),
            "invalid _honk/quote params",
        ),
        (
            json!({ "pick": { "kind": "since" } }),
            "invalid _honk/quote params",
        ),
    ] {
        let res = v.call("_honk/quote", params.clone()).await;
        assert_eq!(res["error"]["code"], -32602, "{params}: {res}");
        assert!(
            res["error"]["message"].as_str().unwrap().contains(want),
            "{params}: {res}"
        );
    }
}
