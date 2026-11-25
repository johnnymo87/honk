//! Isolated integration test for auto-attach environment scrubbing.
//!
//! Runs alone in its own test binary / process so mutating
//! `OPENCODE_SESSION_ID` and `AGENT_SESSION_ID` does not race with concurrent
//! test threads calling `Command::spawn` or reading the environment.

mod support;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use futures::{SinkExt, StreamExt};
use honk::launch::{AttachSettings, LaunchOpts, TagRequest, Timeouts, run};
use honk::pigeon::PigeonConfig;
use serde_json::{Value, json};
use support::pigeon_stub::{Behaviour, StubPigeon};
use tokio::net::TcpListener;
use tokio_tungstenite::tungstenite::protocol::Message;

const SID: &str = "20261104_9";

#[derive(Clone)]
enum PromptReply {
    Echo,
}

#[derive(Clone)]
struct Script {
    swarm_meta: Option<Value>,
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

struct FakeHonkd {
    url: String,
}

async fn fake_honkd(script: Script) -> FakeHonkd {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("ws://{}/acp", listener.local_addr().unwrap());
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
    });
    FakeHonkd { url }
}

fn fake_tags(dir: &Path) -> String {
    use std::io::Write;
    let bin = dir.join("session-tags");
    let body = r#"#!/bin/sh
if [ "$1" = which ]; then printf '\n'; exit 0; fi
if [ "$1" = set ]; then
  echo "Tagged session '$4' as '$3'"
fi
"#;
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
    bin.to_string_lossy().into_owned()
}

struct Rig {
    _tmp: tempfile::TempDir,
    dir: PathBuf,
    tags_bin: String,
    pigeon: Arc<StubPigeon>,
    honkd: FakeHonkd,
}

async fn rig() -> Rig {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().canonicalize().unwrap();
    let tags_bin = fake_tags(&dir);
    let pigeon = Arc::new(StubPigeon::start(Behaviour::default()).await);
    let honkd = fake_honkd(Script::default()).await;
    Rig {
        _tmp: tmp,
        dir,
        tags_bin,
        pigeon,
        honkd,
    }
}

fn opts(r: &Rig) -> LaunchOpts {
    LaunchOpts {
        dir: r.dir.clone(),
        prompt: "do the thing".into(),
        title: Some("Fix it".into()),
        model: None,
        default_model_cmd: None,
        effort: None,
        tag: TagRequest::Explicit("honk".into()),
        launcher: Some("ses_launcher".into()),
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
        attach: None,
        attach_skip_reason: None,
    }
}

struct EnvGuard {
    prior: Vec<(&'static str, Option<String>)>,
}

impl EnvGuard {
    fn set(vars: &[(&'static str, &str)]) -> Self {
        let mut prior = Vec::with_capacity(vars.len());
        for &(k, v) in vars {
            prior.push((k, std::env::var(k).ok()));
            unsafe {
                std::env::set_var(k, v);
            }
        }
        Self { prior }
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        for (k, prev) in &self.prior {
            unsafe {
                match prev {
                    Some(v) => std::env::set_var(k, v),
                    None => std::env::remove_var(k),
                }
            }
        }
    }
}

fn make_fake_auto_attach(dir: &Path, record_path: &Path) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let script_path = dir.join("fake-oc-auto-attach.sh");
    let script = format!(
        "#!/bin/sh\n\
         tmp=\"{}.tmp$$\"\n\
         {{\n\
           echo \"ARGS:$*\"\n\
           echo \"{}:${{{}-ABSENT}}\"\n\
           echo \"{}:${{{}-ABSENT}}\"\n\
         }} > \"$tmp\"\n\
         mv \"$tmp\" \"{}\"\n",
        record_path.display(),
        honk::octags::OPENCODE_SESSION_ID_ENV,
        honk::octags::OPENCODE_SESSION_ID_ENV,
        honk::octags::AGENT_SESSION_ID_ENV,
        honk::octags::AGENT_SESSION_ID_ENV,
        record_path.display(),
    );
    std::fs::write(&script_path, script).unwrap();
    std::fs::set_permissions(&script_path, std::fs::Permissions::from_mode(0o755)).unwrap();
    script_path
}

async fn wait_for_record(path: &Path) -> Option<String> {
    for _ in 0..100 {
        if let Ok(c) = std::fs::read_to_string(path)
            && !c.trim().is_empty()
        {
            return Some(c);
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    None
}

#[test]
fn auto_attach_happy_path_spawns_and_scrubs_env() {
    let _guard = EnvGuard::set(&[
        (honk::octags::OPENCODE_SESSION_ID_ENV, "ses_launcher_123"),
        (honk::octags::AGENT_SESSION_ID_ENV, "agent_launcher_456"),
    ]);

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();

    rt.block_on(async {
        let r = rig().await;
        let temp = tempfile::tempdir().unwrap();
        let record_path = temp.path().join("record.txt");
        let log_path = temp.path().join("attach.log");
        let script_path = make_fake_auto_attach(temp.path(), &record_path);

        let mut o = opts(&r);
        o.attach = Some(AttachSettings {
            bin: script_path.to_string_lossy().into_owned(),
            tmux_session: Some("honk-test-session".into()),
            log_file: log_path,
        });

        let out = run(&o).await.expect("launch");
        assert_eq!(out.native, SID);
        assert_eq!(out.prompt, honk::launch::PromptState::Ok);

        let recorded = wait_for_record(&record_path)
            .await
            .expect("fake oc-auto-attach should have written record file");
        assert!(
            recorded.contains(&format!(
                "ARGS:--tmux-session honk-test-session goose:{SID}"
            )),
            "recorded args mismatch: {recorded}"
        );
        assert!(
            recorded.contains(&format!("{}:ABSENT", honk::octags::OPENCODE_SESSION_ID_ENV)),
            "OPENCODE_SESSION_ID should be scrubbed: {recorded}"
        );
        assert!(
            recorded.contains(&format!("{}:ABSENT", honk::octags::AGENT_SESSION_ID_ENV)),
            "AGENT_SESSION_ID should be scrubbed: {recorded}"
        );
    });
}
