//! `honk launch` against a scripted honkd, the pigeon stub, and a fake
//! session-tags.

mod support;

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::http::StatusCode;
use futures::{SinkExt, StreamExt};
use honk::launch::{LaunchError, LaunchOpts, TagRequest, Timeouts, run};
use honk::pigeon::PigeonConfig;
use serde_json::{Value, json};
use support::pigeon_stub::{Behaviour, StubPigeon};
use tokio::net::TcpListener;
use tokio_tungstenite::tungstenite::protocol::Message;

const SID: &str = "20261104_9";

#[derive(Clone)]
enum PromptReply {
    /// honkd's synthesized user_message_chunk carrying our client_req_id.
    Echo,
    /// An echo for someone else's prompt, then nothing.
    ForeignEcho,
    /// An immediate JSON-RPC error.
    Error(&'static str),
    /// Nothing at all.
    Silent,
}

#[derive(Clone)]
struct Script {
    /// `_meta.honk.swarm` on the session/new reply; `None` omits it.
    swarm_meta: Option<Value>,
    /// Reply to `_honk/mcp/enable` (the whole frame minus id/jsonrpc).
    enable: Value,
    prompt: PromptReply,
}

impl Default for Script {
    fn default() -> Self {
        Self {
            swarm_meta: Some(json!({"enabled": true})),
            enable: json!({"result": {"results": [{"name": "swarm", "ok": true}]}}),
            prompt: PromptReply::Echo,
        }
    }
}

#[derive(Clone, Debug)]
struct Req {
    method: String,
    params: Value,
    /// What the fake session-tags had logged when this request arrived.
    tags_log_then: String,
    /// How many pigeon requests had arrived when this request arrived.
    pigeon_seen_then: usize,
}

struct FakeHonkd {
    url: String,
    reqs: Arc<Mutex<Vec<Req>>>,
    /// Set once the client closed the socket.
    closed: Arc<Mutex<bool>>,
}

async fn fake_honkd(script: Script, tags_log: PathBuf, pigeon: Arc<StubPigeon>) -> FakeHonkd {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("ws://{}/acp", listener.local_addr().unwrap());
    let reqs = Arc::new(Mutex::new(Vec::new()));
    let closed = Arc::new(Mutex::new(false));
    let (r, c) = (reqs.clone(), closed.clone());
    tokio::spawn(async move {
        let (tcp, _) = listener.accept().await.unwrap();
        let mut ws = tokio_tungstenite::accept_async(tcp).await.unwrap();
        while let Some(Ok(msg)) = ws.next().await {
            let Message::Text(t) = msg else {
                if matches!(msg, Message::Close(_)) {
                    break;
                }
                continue;
            };
            let v: Value = serde_json::from_str(&t).unwrap();
            let method = v["method"].as_str().unwrap_or("").to_string();
            let id = v["id"].clone();
            r.lock().unwrap().push(Req {
                method: method.clone(),
                params: v["params"].clone(),
                tags_log_then: std::fs::read_to_string(&tags_log).unwrap_or_default(),
                pigeon_seen_then: pigeon.seen().len(),
            });
            let mut out: Vec<Value> = Vec::new();
            match method.as_str() {
                "initialize" => out.push(json!({"id": id, "result": {"protocolVersion": 1}})),
                "session/new" => {
                    let mut res = json!({"sessionId": SID});
                    if let Some(m) = &script.swarm_meta {
                        res["_meta"] = json!({"honk": {"swarm": m}});
                    }
                    out.push(json!({"id": id, "result": res}));
                }
                "_honk/mcp/enable" => {
                    let mut f = script.enable.clone();
                    f["id"] = id;
                    out.push(f);
                }
                "_goose/unstable/session/rename" => out.push(json!({"id": id, "result": {}})),
                "session/prompt" => {
                    let crid = v["params"]["_meta"]["honk"]["client_req_id"]
                        .as_str()
                        .unwrap()
                        .to_string();
                    let echo = |crid: &str| {
                        json!({"method": "session/update", "params": {"sessionId": SID, "update": {
                            "sessionUpdate": "user_message_chunk", "messageId": format!("synth_{crid}"),
                            "content": {"type": "text", "text": "x"},
                            "_meta": {"honk": {"source": {"client_req_id": crid}}}}}})
                    };
                    match &script.prompt {
                        PromptReply::Echo => out.push(echo(&crid)),
                        PromptReply::ForeignEcho => out.push(echo("someone-else")),
                        PromptReply::Error(m) => {
                            out.push(json!({"id": id, "error": {"code": -32000, "message": m}}))
                        }
                        PromptReply::Silent => {}
                    }
                }
                _ => out.push(json!({"id": id, "error": {"code": -32601, "message": "nope"}})),
            }
            for mut f in out {
                f["jsonrpc"] = json!("2.0");
                if ws.send(Message::Text(f.to_string().into())).await.is_err() {
                    break;
                }
            }
        }
        *c.lock().unwrap() = true;
    });
    FakeHonkd { url, reqs, closed }
}

/// A session-tags that logs `$1 $2 ...` per call and answers `which` with
/// `which_line`; `set_exit` non-zero makes `set` fail.
fn fake_tags(dir: &Path, which_line: &str, set_exit: i32) -> (String, PathBuf) {
    use std::io::Write;
    let log = dir.join("tags.log");
    let bin = dir.join("session-tags");
    let body = format!(
        r#"#!/bin/sh
echo "$*" >> '{log}'
if [ "$1" = which ]; then printf '{which_line}\n'; exit 0; fi
if [ "$1" = set ]; then
  if [ {set_exit} -ne 0 ]; then echo 'error: tags.db is locked' >&2; exit {set_exit}; fi
  echo "Tagged session '$4' as '$3'"
fi
"#,
        log = log.display()
    );
    // Written from a child process (see octags tests: avoids ETXTBSY).
    let mut child = std::process::Command::new("/bin/sh")
        .args(["-c", "cat > \"$1\" && chmod 755 \"$1\"", "sh"])
        .arg(&bin)
        .stdin(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(body.as_bytes())
        .unwrap();
    assert!(child.wait().unwrap().success());
    (bin.to_string_lossy().into_owned(), log)
}

struct Rig {
    _tmp: tempfile::TempDir,
    dir: PathBuf,
    tags_bin: String,
    tags_log: PathBuf,
    pigeon: Arc<StubPigeon>,
    honkd: FakeHonkd,
}

async fn rig(script: Script, pigeon: Behaviour, which_line: &str, set_exit: i32) -> Rig {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().canonicalize().unwrap();
    let (tags_bin, tags_log) = fake_tags(&dir, which_line, set_exit);
    let pigeon = Arc::new(StubPigeon::start(pigeon).await);
    let honkd = fake_honkd(script, tags_log.clone(), pigeon.clone()).await;
    Rig {
        _tmp: tmp,
        dir,
        tags_bin,
        tags_log,
        pigeon,
        honkd,
    }
}

fn opts(r: &Rig, tag: TagRequest, launcher: Option<&str>, title: Option<&str>) -> LaunchOpts {
    LaunchOpts {
        dir: r.dir.clone(),
        prompt: "do the thing".into(),
        title: title.map(str::to_string),
        tag,
        launcher: launcher.map(str::to_string),
        tags_bin: r.tags_bin.clone(),
        honk_url: r.honkd.url.clone(),
        secret: "honkd-secret".into(),
        pigeon: PigeonConfig {
            url: r.pigeon.url.clone(),
            token: Some("ptok".into()),
        },
        timeouts: Timeouts {
            prompt_confirm: Duration::from_secs(2),
            ..Timeouts::default()
        },
    }
}

fn methods(r: &Rig) -> Vec<String> {
    r.honkd
        .reqs
        .lock()
        .unwrap()
        .iter()
        .map(|q| q.method.clone())
        .collect()
}

fn tags_log(r: &Rig) -> String {
    std::fs::read_to_string(&r.tags_log).unwrap_or_default()
}

async fn wait_closed(r: &Rig) {
    for _ in 0..100 {
        if *r.honkd.closed.lock().unwrap() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("launch did not disconnect from honkd");
}

#[tokio::test]
async fn happy_path_tags_registers_then_prompts_and_disconnects() {
    let r = rig(Script::default(), Behaviour::default(), "", 0).await;
    let out = run(&opts(
        &r,
        TagRequest::Explicit("Honk".into()),
        Some("ses_launcher"),
        Some("Fix the thing"),
    ))
    .await
    .expect("launch");

    assert_eq!(out.native, SID);
    assert_eq!(out.gse.as_deref(), Some("gse_test"));
    assert_eq!(out.tag.as_deref(), Some("honk"), "session-tags lowercases");
    assert_eq!(out.line(), "native=20261104_9 gse=gse_test tag=honk");

    assert_eq!(
        methods(&r),
        [
            "initialize",
            "session/new",
            "_goose/unstable/session/rename",
            "session/prompt"
        ],
        "swarm was on at session/new, so no explicit enable"
    );
    let reqs = r.honkd.reqs.lock().unwrap().clone();
    assert_eq!(reqs[1].params["cwd"], json!(r.dir.to_string_lossy()));
    assert!(
        reqs[1].params.pointer("/_meta/honk/swarm").is_none(),
        "never opts out of swarm"
    );
    assert_eq!(
        reqs[2].params,
        json!({"sessionId": SID, "title": "Fix the thing"})
    );
    let p = &reqs[3];
    assert_eq!(
        p.params["prompt"],
        json!([{"type": "text", "text": "do the thing"}])
    );
    assert!(
        p.tags_log_then
            .contains(&format!("set -- Honk goose:{SID}")),
        "tagged before the prompt: {:?}",
        p.tags_log_then
    );
    assert_eq!(
        p.pigeon_seen_then, 2,
        "registered and announced before the prompt"
    );

    // Explicit tag wins: no `which` for the launcher.
    assert_eq!(tags_log(&r), format!("set -- Honk goose:{SID}\n"));

    let seen = r.pigeon.seen();
    assert_eq!(seen.len(), 2, "{seen:?}");
    assert_eq!(seen[0].path, "/goose/sessions");
    assert_eq!(seen[0].auth.as_deref(), Some("Bearer ptok"));
    assert_eq!(seen[0].body["session_id"], SID);
    assert_eq!(seen[0].body["endpoint"], r.honkd.url.as_str());
    assert_eq!(seen[0].body["auth_token"], "honkd-secret");
    assert_eq!(seen[0].body["cwd"], json!(r.dir.to_string_lossy()));
    assert_eq!(seen[1].path, "/stop");
    assert_eq!(seen[1].body["session_id"], "gse_test");
    assert_eq!(seen[1].body["title"], "Fix the thing");

    wait_closed(&r).await;
}

#[tokio::test]
async fn swarm_not_reported_by_honkd_is_enabled_explicitly() {
    let script = Script {
        swarm_meta: None,
        ..Script::default()
    };
    let r = rig(script, Behaviour::default(), "", 0).await;
    run(&opts(&r, TagRequest::Off, None, None))
        .await
        .expect("launch");
    assert_eq!(
        methods(&r),
        [
            "initialize",
            "session/new",
            "_honk/mcp/enable",
            "session/prompt"
        ]
    );
    let reqs = r.honkd.reqs.lock().unwrap().clone();
    assert_eq!(
        reqs[2].params,
        json!({"sessionId": SID, "names": ["swarm"]})
    );
    assert_eq!(tags_log(&r), "", "--tag auto runs no session-tags at all");
}

#[tokio::test]
async fn swarm_failure_is_partial_and_nothing_else_happens() {
    let script = Script {
        swarm_meta: Some(json!({"error": {"code": -32601, "message": "MCP not configured"}})),
        enable: json!({"error": {"code": -32601, "message": "MCP catalog not configured"}}),
        ..Script::default()
    };
    let r = rig(script, Behaviour::default(), "", 0).await;
    let err = run(&opts(&r, TagRequest::Explicit("honk".into()), None, None))
        .await
        .unwrap_err();
    assert_eq!(err.exit_code(), 3);
    let LaunchError::Partial { launched, error } = err else {
        panic!("expected partial")
    };
    assert_eq!(launched.line(), format!("native={SID} gse=- tag=-"));
    let msg = format!("{error:#}");
    assert!(msg.contains("swarm tools are not on"), "{msg}");
    assert!(msg.contains("MCP not configured"), "{msg}");
    assert!(!methods(&r).contains(&"session/prompt".to_string()));
    assert!(r.pigeon.seen().is_empty());
    assert_eq!(tags_log(&r), "");
}

#[tokio::test]
async fn tag_failure_is_partial_before_registration() {
    let r = rig(Script::default(), Behaviour::default(), "", 1).await;
    let err = run(&opts(&r, TagRequest::Explicit("honk".into()), None, None))
        .await
        .unwrap_err();
    let LaunchError::Partial { launched, error } = err else {
        panic!("expected partial")
    };
    assert_eq!(launched.line(), format!("native={SID} gse=- tag=-"));
    let msg = format!("{error:#}");
    assert!(msg.contains("tagging failed"), "{msg}");
    assert!(msg.contains("tags.db is locked"), "{msg}");
    assert!(r.pigeon.seen().is_empty());
    assert!(!methods(&r).contains(&"session/prompt".to_string()));
}

#[tokio::test]
async fn pigeon_refusal_is_partial_and_unprompted() {
    let pigeon = Behaviour {
        register: (StatusCode::CONFLICT, json!({"error": "already exists"})),
        ..Behaviour::default()
    };
    let r = rig(Script::default(), pigeon, "", 0).await;
    let err = run(&opts(&r, TagRequest::Explicit("honk".into()), None, None))
        .await
        .unwrap_err();
    let LaunchError::Partial { launched, error } = err else {
        panic!("expected partial")
    };
    assert_eq!(launched.line(), format!("native={SID} gse=- tag=honk"));
    assert!(format!("{error:#}").contains("registering with pigeon failed"));
    assert!(!methods(&r).contains(&"session/prompt".to_string()));
}

#[tokio::test]
async fn announce_failure_is_partial_with_the_gse_id() {
    let pigeon = Behaviour {
        stop: (StatusCode::INTERNAL_SERVER_ERROR, json!({"error": "boom"})),
        ..Behaviour::default()
    };
    let r = rig(Script::default(), pigeon, "", 0).await;
    let err = run(&opts(&r, TagRequest::Off, None, None))
        .await
        .unwrap_err();
    let LaunchError::Partial { launched, .. } = err else {
        panic!("expected partial")
    };
    assert_eq!(launched.gse.as_deref(), Some("gse_test"));
    assert!(!methods(&r).contains(&"session/prompt".to_string()));
}

#[tokio::test]
async fn prompt_refused_is_partial() {
    let script = Script {
        prompt: PromptReply::Error("busy"),
        ..Script::default()
    };
    let r = rig(script, Behaviour::default(), "", 0).await;
    let err = run(&opts(&r, TagRequest::Off, None, None))
        .await
        .unwrap_err();
    let LaunchError::Partial { launched, error } = err else {
        panic!("expected partial")
    };
    assert_eq!(launched.line(), format!("native={SID} gse=gse_test tag=-"));
    assert!(format!("{error:#}").contains("session/prompt refused: busy"));
}

#[tokio::test]
async fn unconfirmed_prompt_is_partial_and_ignores_foreign_echoes() {
    for reply in [PromptReply::Silent, PromptReply::ForeignEcho] {
        let script = Script {
            prompt: reply,
            ..Script::default()
        };
        let r = rig(script, Behaviour::default(), "", 0).await;
        let err = run(&opts(&r, TagRequest::Off, None, None))
            .await
            .unwrap_err();
        assert_eq!(err.exit_code(), 3);
        let LaunchError::Partial { error, .. } = err else {
            panic!("expected partial")
        };
        assert!(
            format!("{error:#}").contains("had not started the turn"),
            "{error:#}"
        );
    }
}

#[tokio::test]
async fn inherits_the_launchers_tag() {
    let r = rig(
        Script::default(),
        Behaviour::default(),
        "billing\\tmanual\\tses_root\\tsession",
        0,
    )
    .await;
    let out = run(&opts(&r, TagRequest::Inherit, Some("ses_parent"), None))
        .await
        .expect("launch");
    assert_eq!(out.tag.as_deref(), Some("billing"));
    assert_eq!(
        tags_log(&r),
        format!("which -- ses_parent\nset -- billing goose:{SID}\n")
    );
}

#[tokio::test]
async fn no_launcher_and_no_tag_means_untagged() {
    let r = rig(Script::default(), Behaviour::default(), "", 0).await;
    let out = run(&opts(&r, TagRequest::Inherit, None, None))
        .await
        .expect("launch");
    assert_eq!(out.tag, None);
    assert_eq!(tags_log(&r), "");
}

#[tokio::test]
async fn launcher_without_a_session_tag_means_untagged() {
    let r = rig(
        Script::default(),
        Behaviour::default(),
        "auto:honk\\tdir\\t\\tauto",
        0,
    )
    .await;
    let out = run(&opts(
        &r,
        TagRequest::Inherit,
        Some("goose:20261104_1"),
        None,
    ))
    .await
    .expect("launch");
    assert_eq!(out.tag, None);
    assert_eq!(tags_log(&r), "which -- goose:20261104_1\n");
}

fn coordinator_rows() -> Value {
    json!({"ok": true, "sessions": [
        {"session_id": "gse_other", "backend_kind": "goose-acp", "backend_session_id": "20261021_39",
         "bound_topic_key": "stallwatch:lgtm"},
        {"session_id": "gse_coord", "backend_kind": "goose-acp", "backend_session_id": "20261021_40",
         "bound_topic_key": "stallwatch:honk"},
        {"session_id": "gse_worker", "backend_kind": "goose-acp", "backend_session_id": "20261104_2",
         "bound_topic_key": null}
    ]})
}

#[tokio::test]
async fn coordinator_launcher_gives_its_program_tag() {
    let pigeon = Behaviour {
        sessions: coordinator_rows(),
        ..Behaviour::default()
    };
    // Its own tag would be stall-watcher-runs; `which` must not even be asked.
    let r = rig(
        Script::default(),
        pigeon,
        "stall-watcher-runs\\tmanual\\tgoose:20261021_40\\tsession",
        0,
    )
    .await;
    let out = run(&opts(
        &r,
        TagRequest::Inherit,
        Some("goose:20261021_40"),
        None,
    ))
    .await
    .expect("launch");
    assert_eq!(out.tag.as_deref(), Some("honk"));
    assert_eq!(tags_log(&r), format!("set -- honk goose:{SID}\n"));
}

#[tokio::test]
async fn coordinator_tag_without_a_bound_program_creates_nothing() {
    for sessions in [
        // A pigeon that does not report bound topics at all.
        json!({"ok": true, "sessions": [
            {"session_id": "gse_coord", "backend_kind": "goose-acp", "backend_session_id": "20261021_40"}
        ]}),
        // One that does, with no program bound to this launcher.
        coordinator_rows(),
    ] {
        let pigeon = Behaviour {
            sessions,
            ..Behaviour::default()
        };
        let r = rig(
            Script::default(),
            pigeon,
            "stall-watcher-runs\\tmanual\\tgoose:20261104_5\\tsession",
            0,
        )
        .await;
        let err = run(&opts(
            &r,
            TagRequest::Inherit,
            Some("goose:20261104_5"),
            None,
        ))
        .await
        .unwrap_err();
        assert_eq!(err.exit_code(), 1);
        let LaunchError::NotCreated(e) = err else {
            panic!("expected not-created")
        };
        let msg = format!("{e:#}");
        assert!(
            msg.contains("stall-watcher-runs") && msg.contains("pass --tag"),
            "{msg}"
        );
        assert!(methods(&r).is_empty(), "honkd never contacted");
    }
}

#[tokio::test]
async fn undecidable_inheritance_creates_nothing() {
    // session-tags missing.
    let r = rig(Script::default(), Behaviour::default(), "", 0).await;
    let mut o = opts(&r, TagRequest::Inherit, Some("ses_parent"), None);
    o.tags_bin = r.dir.join("nope").to_string_lossy().into_owned();
    let err = run(&o).await.unwrap_err();
    assert_eq!(err.exit_code(), 1);
    assert!(
        format!(
            "{:#}",
            match err {
                LaunchError::NotCreated(e) => e,
                _ => panic!(),
            }
        )
        .contains("not found")
    );
    assert!(methods(&r).is_empty());

    // pigeon unreachable: cannot tell whether the launcher is a coordinator.
    let r = rig(Script::default(), Behaviour::default(), "", 0).await;
    let mut o = opts(&r, TagRequest::Inherit, Some("ses_parent"), None);
    o.pigeon.url = "http://127.0.0.1:1".into();
    let err = run(&o).await.unwrap_err();
    assert_eq!(err.exit_code(), 1);
    assert!(methods(&r).is_empty());
}
