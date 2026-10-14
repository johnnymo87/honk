#![allow(clippy::result_large_err)]

use std::io::{BufRead, BufReader};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};
use tempfile::TempDir;

use futures::{SinkExt, StreamExt};
use honk::chat::{ConnectionState, DriverOpts, RunState, Target, UiCommand, UiEvent, start};
use honk_testgoose::{StubGoose, Turn};
use serde_json::{Value, json};
use tokio::net::TcpListener;
use tokio_tungstenite::tungstenite::handshake::server::{Request, Response};
use tokio_tungstenite::tungstenite::protocol::Message;

async fn fake_ws_server<F>(handler: F) -> (String, tokio::task::JoinHandle<()>)
where
    F: FnOnce(
            TcpListener,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + 'static>>
        + Send
        + 'static,
{
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let url = format!("ws://{addr}/acp");
    let handle = tokio::spawn(handler(listener));
    (url, handle)
}

fn find_honkd_bin() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("HONKD_BIN") {
        let pb = PathBuf::from(p);
        if pb.is_file() {
            return Some(pb);
        }
    }
    if let Ok(td) = std::env::var("CARGO_TARGET_DIR") {
        let pb = PathBuf::from(td).join("debug/honkd");
        if pb.is_file() {
            return Some(pb);
        }
    }
    let candidates = [
        PathBuf::from("target/debug/honkd"),
        PathBuf::from("../../target/debug/honkd"),
    ];
    for c in &candidates {
        if c.is_file() {
            return Some(c.canonicalize().unwrap_or_else(|_| c.clone()));
        }
    }
    None
}

#[allow(dead_code)]
struct HonkdHarness {
    child: Option<Child>,
    pub addr: SocketAddr,
    pub state_dir: TempDir,
    upstream: String,
    upstream_secret: PathBuf,
    bin_path: PathBuf,
}

impl HonkdHarness {
    pub async fn spawn(
        bin: &Path,
        upstream: &str,
        upstream_secret: &Path,
        explicit_addr: Option<SocketAddr>,
    ) -> Self {
        let state_dir = tempfile::tempdir().expect("create state dir");
        let (child, addr) = spawn_honkd_process(
            bin,
            upstream,
            upstream_secret,
            state_dir.path(),
            explicit_addr,
        )
        .await;

        Self {
            child: Some(child),
            addr,
            state_dir,
            upstream: upstream.to_string(),
            upstream_secret: upstream_secret.to_path_buf(),
            bin_path: bin.to_path_buf(),
        }
    }

    pub fn ws_url(&self) -> String {
        format!("ws://{}/acp", self.addr)
    }

    pub fn secret(&self) -> String {
        let p = self.state_dir.path().join("secret");
        std::fs::read_to_string(&p)
            .unwrap_or_else(|e| panic!("failed to read secret at {}: {e}", p.display()))
            .trim()
            .to_string()
    }

    pub fn kill9(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }

    pub async fn restart(&mut self) {
        self.kill9();

        let start = std::time::Instant::now();
        loop {
            match std::net::TcpListener::bind(self.addr) {
                Ok(l) => {
                    drop(l);
                    break;
                }
                Err(_) => {
                    if start.elapsed() > Duration::from_secs(5) {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
            }
        }

        let (child, addr) = spawn_honkd_process(
            &self.bin_path,
            &self.upstream,
            &self.upstream_secret,
            self.state_dir.path(),
            Some(self.addr),
        )
        .await;

        assert_eq!(addr, self.addr, "restarted honkd bound same address");
        self.child = Some(child);
    }
}

impl Drop for HonkdHarness {
    fn drop(&mut self) {
        self.kill9();
    }
}

async fn spawn_honkd_process(
    bin: &Path,
    upstream: &str,
    upstream_secret: &Path,
    state_dir: &Path,
    explicit_addr: Option<SocketAddr>,
) -> (Child, SocketAddr) {
    let mut cmd = Command::new(bin);
    if let Some(a) = explicit_addr {
        cmd.arg("--listen").arg(a.to_string());
    } else {
        cmd.arg("--listen").arg("127.0.0.1:0");
    }
    cmd.arg("--upstream").arg(upstream);
    cmd.arg("--upstream-secret-file").arg(upstream_secret);
    cmd.arg("--state-dir").arg(state_dir);
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());

    let mut child = cmd.spawn().expect("spawn honkd");
    let stdout = child.stdout.take().expect("honkd stdout");
    if let Some(stderr) = child.stderr.take() {
        std::thread::spawn(move || {
            let reader = BufReader::new(stderr);
            for _ in reader.lines().map_while(Result::ok) {}
        });
    }

    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let reader = BufReader::new(stdout);
        for line in reader.lines().map_while(Result::ok) {
            if let Some(rest) = line.strip_prefix("honkd listening on ")
                && let Ok(addr) = rest.trim().parse::<SocketAddr>()
            {
                let _ = tx.send(addr);
                return;
            }
        }
    });

    let addr = tokio::task::spawn_blocking(move || rx.recv_timeout(Duration::from_secs(10)))
        .await
        .expect("join wait thread")
        .expect("timed out waiting for honkd listening on");

    (child, addr)
}

#[tokio::test(flavor = "multi_thread")]
async fn test_scenario_a_new_session_prompt_assistant_text_and_idle() {
    let Some(g) = StubGoose::start_or_skip().await else {
        eprintln!("skipping test: StubGoose unset");
        return;
    };
    let Some(honkd_bin) = find_honkd_bin() else {
        eprintln!("skipping test: honkd binary not found");
        return;
    };

    g.model().push([Turn::Text(vec![
        "hello ".into(),
        "from ".into(),
        "driver!".into(),
    ])]);

    let h = HonkdHarness::spawn(&honkd_bin, &g.url(), g.secret_file(), None).await;

    let tmp = tempfile::tempdir().expect("create tmp cwd");
    let opts = DriverOpts::new(
        h.ws_url(),
        h.secret(),
        Target::New {
            cwd: tmp.path().to_path_buf(),
            extensions: None,
        },
    );

    let (handle, mut rx) = start(opts).await.expect("start driver");

    // We expect SessionNew and Attached to be emitted
    let mut session_new_emitted = false;
    let mut attached_emitted = false;

    // Collect events until attached and replay done
    let start_time = std::time::Instant::now();
    while let Some(evt) = rx.recv().await {
        match &evt {
            UiEvent::SessionNew { .. } => {
                session_new_emitted = true;
            }
            UiEvent::Attached { .. } => {
                attached_emitted = true;
            }
            UiEvent::State(s) => {
                if !s.replaying && attached_emitted {
                    break;
                }
            }
            UiEvent::Fatal(f) => panic!("fatal error during init: {f}"),
            _ => {}
        }
        if start_time.elapsed() > Duration::from_secs(10) {
            panic!("timed out waiting for session init");
        }
    }

    assert!(session_new_emitted, "SessionNew event must be emitted");
    assert!(attached_emitted, "Attached event must be emitted");

    // Send line prompt
    handle
        .send(UiCommand::Line("say hello".to_string()))
        .expect("send line");

    // Wait for assistant text and idle state
    let mut user_echo_text = String::new();
    let mut assistant_text = String::new();
    let mut became_idle = false;
    let prompt_time = std::time::Instant::now();

    while let Some(evt) = rx.recv().await {
        match evt {
            UiEvent::Transcript(f) => {
                if let Some(u) = f.pointer("/params/update") {
                    let kind = u.get("sessionUpdate").and_then(Value::as_str);
                    if kind == Some("user_message_chunk") {
                        if let Some(text) = u.pointer("/content/text").and_then(Value::as_str) {
                            user_echo_text.push_str(text);
                        }
                    } else if kind == Some("agent_message_chunk")
                        && let Some(text) = u.pointer("/content/text").and_then(Value::as_str)
                    {
                        assistant_text.push_str(text);
                    }
                }
            }
            UiEvent::State(s) => {
                if s.run_state == RunState::Idle && !assistant_text.is_empty() {
                    became_idle = true;
                    break;
                }
            }
            UiEvent::Fatal(f) => panic!("fatal error during prompt: {f}"),
            _ => {}
        }
        if prompt_time.elapsed() > Duration::from_secs(10) {
            panic!("timed out waiting for prompt response; text so far: {assistant_text}");
        }
    }

    assert_eq!(user_echo_text, "say hello");
    assert_eq!(assistant_text, "hello from driver!");
    assert!(became_idle, "State must transition back to Idle");

    // Clean shutdown
    handle.send(UiCommand::Shutdown).expect("send shutdown");
}

#[tokio::test(flavor = "multi_thread")]
async fn test_scenario_b_attach_session_with_history_replay_not_live() {
    let Some(g) = StubGoose::start_or_skip().await else {
        eprintln!("skipping test: StubGoose unset");
        return;
    };
    let Some(honkd_bin) = find_honkd_bin() else {
        eprintln!("skipping test: honkd binary not found");
        return;
    };

    g.model()
        .push([Turn::Text(vec!["history response".into()])]);

    let h = HonkdHarness::spawn(&honkd_bin, &g.url(), g.secret_file(), None).await;

    // 1. Create and prompt session with a raw client first
    let mut client = honk::Client::connect(&h.ws_url(), &h.secret())
        .await
        .expect("raw connect");

    let new_res = client
        .call(
            "session/new",
            serde_json::json!({
                "cwd": g.cwd().to_str().unwrap(),
                "mcpServers": [],
                "_meta": {
                    "enabledExtensions": [{
                        "type": "platform",
                        "name": "developer"
                    }]
                }
            }),
        )
        .await
        .expect("session/new");
    let sid = new_res["result"]["sessionId"]
        .as_str()
        .expect("sessionId")
        .to_string();

    let prompt_res = client
        .call(
            "session/prompt",
            serde_json::json!({
                "sessionId": sid,
                "prompt": [{ "type": "text", "text": "initial question" }]
            }),
        )
        .await
        .expect("session/prompt");
    assert_eq!(prompt_res["result"]["stopReason"], "end_turn");
    drop(client);

    // 2. Attach driver to this session
    let opts = DriverOpts::new(h.ws_url(), h.secret(), Target::Attach(sid.clone()));
    let (handle, mut rx) = start(opts).await.expect("start driver attach");

    let mut attached_emitted = false;
    let mut replay_done_seen = false;
    let mut replayed_texts = Vec::new();

    let start_time = std::time::Instant::now();
    while let Some(evt) = rx.recv().await {
        match evt {
            UiEvent::Attached { .. } => {
                attached_emitted = true;
            }
            UiEvent::Transcript(f) => {
                if let Some(text) = f
                    .pointer("/params/update/content/text")
                    .and_then(Value::as_str)
                {
                    replayed_texts.push(text.to_string());
                }
            }
            UiEvent::State(s) => {
                if !s.replaying && attached_emitted {
                    replay_done_seen = true;
                    assert!(
                        !s.prompt_in_flight,
                        "no prompt should be in flight on attach"
                    );
                    assert_eq!(s.run_state, RunState::Idle);
                    break;
                }
            }
            UiEvent::Fatal(f) => panic!("fatal error during attach: {f}"),
            _ => {}
        }
        if start_time.elapsed() > Duration::from_secs(10) {
            panic!("timed out waiting for replay on attach");
        }
    }

    assert!(attached_emitted, "Attached must be emitted");
    assert!(replay_done_seen, "Replay must finish with replaying=false");
    let full_replayed = replayed_texts.join("");
    assert!(
        full_replayed.contains("history response"),
        "history transcript must be received; got: {full_replayed}"
    );

    handle.send(UiCommand::Shutdown).expect("send shutdown");
}

#[tokio::test(flavor = "multi_thread")]
async fn test_scenario_c_steer_consumed_in_own_gated_turn() {
    let Some(g) = StubGoose::start_or_skip().await else {
        eprintln!("skipping test: StubGoose unset");
        return;
    };
    let Some(honkd_bin) = find_honkd_bin() else {
        eprintln!("skipping test: honkd binary not found");
        return;
    };

    g.model().push([
        Turn::Gate(
            "gate_steer".into(),
            Box::new(Turn::Text(vec!["first turn completion ".into()])),
        ),
        Turn::Text(vec!["steer answer".into()]),
    ]);

    let h = HonkdHarness::spawn(&honkd_bin, &g.url(), g.secret_file(), None).await;
    let tmp = tempfile::tempdir().expect("create tmp cwd");
    let opts = DriverOpts::new(
        h.ws_url(),
        h.secret(),
        Target::New {
            cwd: tmp.path().to_path_buf(),
            extensions: None,
        },
    );

    let (handle, mut rx) = start(opts).await.expect("start driver");

    // Wait until attached and live
    while let Some(evt) = rx.recv().await {
        if let UiEvent::State(s) = evt
            && !s.replaying
            && matches!(s.run_state, RunState::Idle | RunState::Unknown)
        {
            break;
        }
    }

    // Send initial prompt
    handle
        .send(UiCommand::Line("run first turn".to_string()))
        .expect("send line");

    // Wait until run starts and is Own
    let mut run_active = false;
    let start_wait = std::time::Instant::now();

    while let Some(evt) = rx.recv().await {
        if let UiEvent::State(s) = evt
            && s.run_state.is_running()
            && s.ownership == honk::chat::Ownership::Own
        {
            run_active = true;
            break;
        }
        if start_wait.elapsed() > Duration::from_secs(10) {
            panic!("timed out waiting for own run to become active");
        }
    }

    assert!(run_active, "run should be active and Own");

    // Send steer into own running turn
    handle
        .send(UiCommand::Line("please also do this".to_string()))
        .expect("send steer");

    // Give a brief moment for the steer request to be sent and acknowledged
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Open gate to let goose finish first turn and process steer
    g.model().open("gate_steer");

    // Wait until turn finishes and steer answer is in transcript
    let mut full_transcript = String::new();
    let turn_wait = std::time::Instant::now();
    let mut became_idle = false;

    while let Some(evt) = rx.recv().await {
        match evt {
            UiEvent::Transcript(f) => {
                if let Some(text) = f
                    .pointer("/params/update/content/text")
                    .and_then(Value::as_str)
                {
                    full_transcript.push_str(text);
                }
            }
            UiEvent::State(s)
                if s.run_state == RunState::Idle && full_transcript.contains("steer answer") =>
            {
                became_idle = true;
                break;
            }
            _ => {}
        }
        if turn_wait.elapsed() > Duration::from_secs(10) {
            panic!("timed out waiting for turn completion; transcript: {full_transcript}");
        }
    }

    assert!(became_idle, "driver state should be Idle");
    assert!(
        full_transcript.contains("steer answer"),
        "steer response must be in transcript: {full_transcript}"
    );

    handle.send(UiCommand::Shutdown).expect("send shutdown");
}

#[tokio::test(flavor = "multi_thread")]
async fn test_scenario_d_foreign_gated_turn_queues_line_prompted_after_run_end() {
    let Some(g) = StubGoose::start_or_skip().await else {
        eprintln!("skipping test: StubGoose unset");
        return;
    };
    let Some(honkd_bin) = find_honkd_bin() else {
        eprintln!("skipping test: honkd binary not found");
        return;
    };

    g.model().push([
        Turn::Gate(
            "gate_foreign".into(),
            Box::new(Turn::Text(vec!["foreign completion".into()])),
        ),
        Turn::Text(vec!["our queued answer".into()]),
    ]);

    let h = HonkdHarness::spawn(&honkd_bin, &g.url(), g.secret_file(), None).await;

    // Create session with raw client
    let mut raw_client = honk::Client::connect(&h.ws_url(), &h.secret())
        .await
        .expect("raw connect");
    let new_res = raw_client
        .call(
            "session/new",
            serde_json::json!({
                "cwd": g.cwd().to_str().unwrap(),
                "mcpServers": [],
                "_meta": {
                    "enabledExtensions": [{
                        "type": "platform",
                        "name": "developer"
                    }]
                }
            }),
        )
        .await
        .expect("session/new");
    let sid = new_res["result"]["sessionId"]
        .as_str()
        .expect("sessionId")
        .to_string();

    // Start driver attached to session
    let opts = DriverOpts::new(h.ws_url(), h.secret(), Target::Attach(sid.clone()));
    let (handle, mut rx) = start(opts).await.expect("start driver");

    // Wait until driver attached
    while let Some(evt) = rx.recv().await {
        if let UiEvent::State(s) = evt
            && !s.replaying
            && matches!(s.run_state, RunState::Idle | RunState::Unknown)
        {
            break;
        }
    }

    // Start foreign turn via raw client asynchronously (will pause at gate_foreign)
    let _foreign_req_id = raw_client
        .request(
            "session/prompt",
            serde_json::json!({
                "sessionId": sid,
                "prompt": [{ "type": "text", "text": "foreign prompt" }]
            }),
        )
        .await
        .expect("raw prompt");

    // Wait until driver sees foreign run active
    let mut foreign_seen = false;
    let start_wait = std::time::Instant::now();
    while let Some(evt) = rx.recv().await {
        if let UiEvent::State(s) = evt
            && s.run_state.is_running()
            && s.ownership == honk::chat::Ownership::Foreign
        {
            foreign_seen = true;
            break;
        }
        if start_wait.elapsed() > Duration::from_secs(10) {
            panic!("timed out waiting for foreign run state");
        }
    }
    assert!(foreign_seen, "driver should recognize foreign run");

    // Send line via driver: should be queued
    handle
        .send(UiCommand::Line("our queued prompt".to_string()))
        .expect("send line");

    // Wait for state showing queued_len == 1 and Foreign ownership
    let mut queued_seen = false;
    let queue_wait = std::time::Instant::now();
    while let Some(evt) = rx.recv().await {
        if let UiEvent::State(s) = evt
            && s.queued_len == 1
            && s.ownership == honk::chat::Ownership::Foreign
        {
            queued_seen = true;
            break;
        }
        if queue_wait.elapsed() > Duration::from_secs(10) {
            panic!("timed out waiting for queued_len == 1");
        }
    }
    assert!(queued_seen, "line should be queued with queued_len == 1");

    // Open gate to complete foreign turn
    g.model().open("gate_foreign");

    // Driver should automatically send our queued prompt and receive answer
    let mut transcript = String::new();
    let mut answer_seen = false;
    let prompt_wait = std::time::Instant::now();

    while let Some(evt) = rx.recv().await {
        match evt {
            UiEvent::Transcript(f) => {
                if let Some(text) = f
                    .pointer("/params/update/content/text")
                    .and_then(Value::as_str)
                {
                    transcript.push_str(text);
                }
            }
            UiEvent::State(s)
                if s.run_state == RunState::Idle
                    && s.queued_len == 0
                    && transcript.contains("our queued answer") =>
            {
                answer_seen = true;
                break;
            }
            _ => {}
        }
        if prompt_wait.elapsed() > Duration::from_secs(10) {
            panic!("timed out waiting for queued prompt completion; transcript: {transcript}");
        }
    }

    assert!(answer_seen, "queued prompt must complete and be answered");
    handle.send(UiCommand::Shutdown).expect("send shutdown");
}

#[tokio::test(flavor = "multi_thread")]
async fn test_scenario_e_interrupt_own_vs_foreign() {
    let Some(g) = StubGoose::start_or_skip().await else {
        eprintln!("skipping test: StubGoose unset");
        return;
    };
    let Some(honkd_bin) = find_honkd_bin() else {
        eprintln!("skipping test: honkd binary not found");
        return;
    };

    let h = HonkdHarness::spawn(&honkd_bin, &g.url(), g.secret_file(), None).await;

    // --- Part 1: Interrupt during own gated turn -> turn ends cancelled ---
    g.model().push([Turn::Gate(
        "gate_own_cancel".into(),
        Box::new(Turn::Text(vec!["should not reach here".into()])),
    )]);

    let tmp = tempfile::tempdir().expect("create tmp cwd");
    let opts = DriverOpts::new(
        h.ws_url(),
        h.secret(),
        Target::New {
            cwd: tmp.path().to_path_buf(),
            extensions: None,
        },
    );

    let (handle, mut rx) = start(opts).await.expect("start driver own");

    // Wait until live
    while let Some(evt) = rx.recv().await {
        if let UiEvent::State(s) = evt
            && !s.replaying
            && matches!(s.run_state, RunState::Idle | RunState::Unknown)
        {
            break;
        }
    }

    handle
        .send(UiCommand::Line("own prompt to cancel".to_string()))
        .expect("send line");

    // Wait until run starts and is Own
    let start_wait = std::time::Instant::now();
    while let Some(evt) = rx.recv().await {
        if let UiEvent::State(s) = evt
            && s.run_state.is_running()
            && s.ownership == honk::chat::Ownership::Own
        {
            break;
        }
        if start_wait.elapsed() > Duration::from_secs(10) {
            panic!("timed out waiting for own run");
        }
    }

    // Send Interrupt
    handle.send(UiCommand::Interrupt).expect("send interrupt");

    // Open gate on goose: goose sees cancel and finishes turn with stopReason cancelled
    tokio::time::sleep(Duration::from_millis(50)).await;
    g.model().open("gate_own_cancel");

    // Expect turn to end with stopReason cancelled and state returning to Idle
    let mut own_cancelled = false;
    let cancel_wait = std::time::Instant::now();
    while let Some(evt) = rx.recv().await {
        match evt {
            UiEvent::Notice(n) => {
                if n.text.contains("cancelled") {
                    own_cancelled = true;
                }
            }
            UiEvent::State(s) if s.run_state == RunState::Idle && own_cancelled => {
                break;
            }
            _ => {}
        }
        if cancel_wait.elapsed() > Duration::from_secs(10) {
            panic!("timed out waiting for own cancellation");
        }
    }
    assert!(own_cancelled, "own turn should end cancelled");
    handle.send(UiCommand::Shutdown).expect("send shutdown");

    // --- Part 2: Interrupt during foreign turn -> not cancelled ---
    g.model().push([Turn::Gate(
        "gate_foreign_cancel".into(),
        Box::new(Turn::Text(vec!["foreign completed successfully".into()])),
    )]);

    let mut raw_client = honk::Client::connect(&h.ws_url(), &h.secret())
        .await
        .expect("raw connect");
    let new_res = raw_client
        .call(
            "session/new",
            serde_json::json!({
                "cwd": g.cwd().to_str().unwrap(),
                "mcpServers": [],
                "_meta": {
                    "enabledExtensions": [{
                        "type": "platform",
                        "name": "developer"
                    }]
                }
            }),
        )
        .await
        .expect("session/new");
    let sid2 = new_res["result"]["sessionId"]
        .as_str()
        .expect("sessionId")
        .to_string();

    let opts2 = DriverOpts::new(h.ws_url(), h.secret(), Target::Attach(sid2.clone()));
    let (handle2, mut rx2) = start(opts2).await.expect("start driver foreign");

    while let Some(evt) = rx2.recv().await {
        if let UiEvent::State(s) = evt
            && !s.replaying
            && matches!(s.run_state, RunState::Idle | RunState::Unknown)
        {
            break;
        }
    }

    // Raw client starts turn (pauses at gate_foreign_cancel)
    let prompt_req_id = raw_client
        .request(
            "session/prompt",
            serde_json::json!({
                "sessionId": sid2,
                "prompt": [{ "type": "text", "text": "foreign prompt" }]
            }),
        )
        .await
        .expect("foreign prompt");

    // Wait until driver sees foreign run
    let f_wait = std::time::Instant::now();
    while let Some(evt) = rx2.recv().await {
        if let UiEvent::State(s) = evt
            && s.run_state.is_running()
            && s.ownership == honk::chat::Ownership::Foreign
        {
            break;
        }
        if f_wait.elapsed() > Duration::from_secs(10) {
            panic!("timed out waiting for foreign run");
        }
    }

    // Driver sends Interrupt: should NOT cancel foreign turn
    handle2.send(UiCommand::Interrupt).expect("send interrupt");

    // Expect notice saying "another client" / "/cancel!"
    let mut notice_received = false;
    let n_wait = std::time::Instant::now();
    while let Ok(Some(evt)) = tokio::time::timeout(Duration::from_secs(5), rx2.recv()).await {
        if let UiEvent::Notice(n) = evt
            && (n.text.contains("another client") || n.text.contains("/cancel!"))
        {
            notice_received = true;
            break;
        }
        if n_wait.elapsed() > Duration::from_secs(5) {
            break;
        }
    }
    assert!(notice_received, "should receive notice refusing cancel");

    // Open gate on goose: foreign turn completes normally
    g.model().open("gate_foreign_cancel");

    // Wait until raw client prompt receives response
    let raw_resp = loop {
        let f = raw_client.next_frame().await.expect("next frame");
        match f {
            Some(f) => {
                if f.get("id") == Some(&serde_json::json!(prompt_req_id)) {
                    break f;
                }
            }
            None => panic!("raw client disconnected prematurely"),
        }
    };
    assert_eq!(
        raw_resp["result"]["stopReason"], "end_turn",
        "foreign turn should not be cancelled"
    );

    handle2.send(UiCommand::Shutdown).expect("send shutdown");
}

#[tokio::test(flavor = "multi_thread")]
async fn test_scenario_f_own_turn_in_approve_mode_auto_approved() {
    let Some(g) = StubGoose::start_or_skip().await else {
        eprintln!("skipping test: StubGoose unset");
        return;
    };
    let Some(honkd_bin) = find_honkd_bin() else {
        eprintln!("skipping test: honkd binary not found");
        return;
    };

    g.model().push([
        Turn::ToolCall {
            tool: "shell".to_string(),
            arguments: serde_json::json!({"command": "echo perm_tool_executed"}),
        },
        Turn::Text(vec!["tool finished".to_string()]),
    ]);

    let h = HonkdHarness::spawn(&honkd_bin, &g.url(), g.secret_file(), None).await;

    // Create session and set mode to "approve"
    let mut client = honk::Client::connect(&h.ws_url(), &h.secret())
        .await
        .expect("connect");

    let new_res = client
        .call(
            "session/new",
            serde_json::json!({
                "cwd": g.cwd().to_str().unwrap(),
                "mcpServers": [],
                "_meta": {
                    "enabledExtensions": [{
                        "type": "platform",
                        "name": "developer"
                    }]
                }
            }),
        )
        .await
        .expect("session/new");
    let sid = new_res["result"]["sessionId"]
        .as_str()
        .expect("sessionId")
        .to_string();

    let mode_res = client
        .call(
            "session/set_mode",
            serde_json::json!({
                "sessionId": sid,
                "modeId": "approve"
            }),
        )
        .await
        .expect("session/set_mode");
    assert!(mode_res.get("error").is_none(), "mode approve set");
    drop(client);

    // Start driver attached to session
    let opts = DriverOpts::new(h.ws_url(), h.secret(), Target::Attach(sid.clone()));
    let (handle, mut rx) = start(opts).await.expect("start driver");

    // Wait until live
    while let Some(evt) = rx.recv().await {
        if let UiEvent::State(s) = evt
            && !s.replaying
            && matches!(s.run_state, RunState::Idle | RunState::Unknown)
        {
            break;
        }
    }

    // Send prompt
    handle
        .send(UiCommand::Line("run tool in approve mode".to_string()))
        .expect("send line");

    // Wait for turn completion
    let mut transcript = String::new();
    let mut auto_approved_notice = false;
    let mut became_idle = false;
    let turn_wait = std::time::Instant::now();

    while let Some(evt) = rx.recv().await {
        match evt {
            UiEvent::Notice(n) => {
                if n.text.contains("auto-approved") {
                    auto_approved_notice = true;
                }
            }
            UiEvent::Transcript(f) => {
                if let Some(text) = f
                    .pointer("/params/update/content/text")
                    .and_then(Value::as_str)
                {
                    transcript.push_str(text);
                }
            }
            UiEvent::State(s)
                if s.run_state == RunState::Idle && transcript.contains("tool finished") =>
            {
                became_idle = true;
                break;
            }
            _ => {}
        }
        if turn_wait.elapsed() > Duration::from_secs(10) {
            panic!("timed out waiting for turn completion; transcript: {transcript}");
        }
    }

    assert!(
        auto_approved_notice,
        "auto-approved notice should be emitted"
    );
    assert!(
        became_idle,
        "turn should complete and driver return to Idle"
    );
    assert!(
        transcript.contains("tool finished"),
        "tool finished in transcript"
    );

    // Verify tool actually executed in goose export
    let export = g.export(&sid).await;
    let export_str = export.to_string();
    assert!(
        export_str.contains("perm_tool_executed"),
        "tool execution output should be present in export: {export_str}"
    );

    handle.send(UiCommand::Shutdown).expect("send shutdown");
}

#[tokio::test(flavor = "multi_thread")]
async fn test_scenario_g_kill9_honkd_mid_turn_reconnects_resends_crid_single_assistant_text() {
    let Some(g) = StubGoose::start_or_skip().await else {
        eprintln!("skipping test: StubGoose unset");
        return;
    };
    let Some(honkd_bin) = find_honkd_bin() else {
        eprintln!("skipping test: honkd binary not found");
        return;
    };

    g.model().push([Turn::TextThenGate {
        before: vec!["part one ".into()],
        gate: "gate_mid_turn_restart".into(),
        after: vec!["part two".into()],
    }]);

    let mut h = HonkdHarness::spawn(&honkd_bin, &g.url(), g.secret_file(), None).await;

    let tmp = tempfile::tempdir().expect("create tmp cwd");
    let opts = DriverOpts::new(
        h.ws_url(),
        h.secret(),
        Target::New {
            cwd: tmp.path().to_path_buf(),
            extensions: None,
        },
    );

    let (handle, mut rx) = start(opts).await.expect("start driver");

    // Wait until live
    while let Some(evt) = rx.recv().await {
        if let UiEvent::State(s) = evt
            && !s.replaying
            && matches!(s.run_state, RunState::Idle | RunState::Unknown)
        {
            break;
        }
    }

    // Send prompt
    handle
        .send(UiCommand::Line("start multi-part prompt".to_string()))
        .expect("send line");

    // Wait until driver receives pre-gate chunk
    let mut pre_kill_chunks = String::new();
    let pre_wait = std::time::Instant::now();
    let mut blocks = honk_wire::Blocks::default();

    while let Some(evt) = rx.recv().await {
        if let UiEvent::Transcript(f) = evt {
            blocks.apply(&f);
            if let Some(text) = f
                .pointer("/params/update/content/text")
                .and_then(Value::as_str)
            {
                pre_kill_chunks.push_str(text);
            }
            if pre_kill_chunks.contains("part one ") {
                break;
            }
        }
        if pre_wait.elapsed() > Duration::from_secs(10) {
            panic!("timed out waiting for pre-gate chunk; got: {pre_kill_chunks}");
        }
    }

    assert!(
        pre_kill_chunks.contains("part one "),
        "pre-gate chunk received before kill"
    );

    // Kill -9 honkd mid turn
    h.kill9();

    // Release gate so goose completes turn while honkd is down
    g.model().open("gate_mid_turn_restart");
    tokio::time::sleep(Duration::from_millis(150)).await;

    // Observe driver entering Reconnecting state
    let mut reconnecting_seen = false;
    let rec_wait = std::time::Instant::now();
    while let Ok(Some(evt)) = tokio::time::timeout(Duration::from_secs(2), rx.recv()).await {
        if let UiEvent::State(s) = evt
            && matches!(s.connection, ConnectionState::Reconnecting { .. })
        {
            reconnecting_seen = true;
            break;
        }
        if rec_wait.elapsed() > Duration::from_secs(2) {
            break;
        }
    }
    assert!(reconnecting_seen, "driver should enter Reconnecting state");

    // Restart honkd
    h.restart().await;

    // Wait until driver transitions Reconnecting -> Live, prompt is resent with same crid,
    // turn end seen (Idle), and final assistant text is in blocks
    let mut live_seen = false;
    let mut idle_seen = false;
    let post_wait = std::time::Instant::now();

    while let Ok(Some(evt)) = tokio::time::timeout(Duration::from_secs(25), rx.recv()).await {
        match evt {
            UiEvent::Transcript(f) => {
                blocks.apply(&f);
            }
            UiEvent::State(s) => {
                if s.connection == ConnectionState::Live {
                    live_seen = true;
                }
                if live_seen && s.run_state == RunState::Idle && !s.prompt_in_flight {
                    idle_seen = true;
                    break;
                }
            }
            _ => {}
        }
        if post_wait.elapsed() > Duration::from_secs(25) {
            panic!("timed out waiting for reconnect and turn completion");
        }
    }

    assert!(live_seen, "driver should return to Live state");
    assert!(
        idle_seen,
        "turn end should be seen with state returning to Idle"
    );

    // Use honk_wire::Blocks to assert assistant text arrives exactly once
    let agent_text = blocks.agent_text();
    assert_eq!(
        agent_text, "part one part two",
        "assistant text must arrive exactly once in blocks model: {agent_text}"
    );

    handle.send(UiCommand::Shutdown).expect("send shutdown");
}

#[tokio::test(flavor = "multi_thread")]
async fn test_driver_first_attach_nonexistent_session_is_fatal() {
    let Some(g) = StubGoose::start_or_skip().await else {
        return;
    };
    let Some(honkd_bin) = find_honkd_bin() else {
        return;
    };

    let h = HonkdHarness::spawn(&honkd_bin, &g.url(), g.secret_file(), None).await;
    let opts = DriverOpts::new(
        h.ws_url(),
        h.secret(),
        Target::Attach("non_existent_session_99999".to_string()),
    );

    let (_handle, mut rx) = start(opts).await.expect("start driver");

    let mut fatal_seen = false;
    let mut exit_code = None;

    let wait = std::time::Instant::now();
    while let Some(evt) = rx.recv().await {
        match evt {
            UiEvent::Fatal(err) => {
                assert!(
                    err.contains("session/load failed"),
                    "fatal error must mention session/load failed: {err}"
                );
                fatal_seen = true;
            }
            UiEvent::Exit(code) => {
                exit_code = Some(code);
                break;
            }
            _ => {}
        }
        if wait.elapsed() > Duration::from_secs(5) {
            panic!("timed out waiting for fatal event on nonexistent session");
        }
    }

    assert!(fatal_seen, "UiEvent::Fatal must be emitted");
    assert_eq!(exit_code, Some(1), "exit code must be 1 on fatal attach");
}

#[tokio::test(flavor = "multi_thread")]
async fn test_driver_bad_secret_auth_failure_is_fatal() {
    let Some(g) = StubGoose::start_or_skip().await else {
        return;
    };
    let Some(honkd_bin) = find_honkd_bin() else {
        return;
    };

    let h = HonkdHarness::spawn(&honkd_bin, &g.url(), g.secret_file(), None).await;
    let opts = DriverOpts::new(
        h.ws_url(),
        "invalid_secret_token".to_string(),
        Target::Attach("any_session".to_string()),
    );

    let (_handle, mut rx) = start(opts).await.expect("start driver");

    let mut fatal_seen = false;
    let mut exit_code = None;

    let wait = std::time::Instant::now();
    while let Some(evt) = rx.recv().await {
        match evt {
            UiEvent::Fatal(err) => {
                assert!(
                    err.contains("authentication failed") || err.contains("401"),
                    "fatal error must indicate auth failure: {err}"
                );
                fatal_seen = true;
            }
            UiEvent::Exit(code) => {
                exit_code = Some(code);
                break;
            }
            _ => {}
        }
        if wait.elapsed() > Duration::from_secs(5) {
            panic!("timed out waiting for auth failure fatal event");
        }
    }

    assert!(fatal_seen, "UiEvent::Fatal must be emitted on auth failure");
    assert_eq!(exit_code, Some(1), "exit code must be 1 on fatal auth");
}

#[tokio::test(flavor = "multi_thread")]
async fn test_driver_idle_detach_and_reattach() {
    let Some(g) = StubGoose::start_or_skip().await else {
        return;
    };
    let Some(honkd_bin) = find_honkd_bin() else {
        return;
    };

    let h = HonkdHarness::spawn(&honkd_bin, &g.url(), g.secret_file(), None).await;
    let tmp = tempfile::tempdir().expect("create tmp cwd");

    let mut opts = DriverOpts::new(
        h.ws_url(),
        h.secret(),
        Target::New {
            cwd: tmp.path().to_path_buf(),
            extensions: None,
        },
    );
    // Short idle timeout so it detaches on ticks
    opts.idle_timeout = Some(Duration::from_millis(50));

    let (handle, mut rx) = start(opts).await.expect("start driver");

    // Wait until live
    while let Some(evt) = rx.recv().await {
        if let UiEvent::State(s) = evt
            && !s.replaying
            && matches!(s.run_state, RunState::Idle | RunState::Unknown)
        {
            break;
        }
    }

    // Wait for Detached state
    let mut detached_seen = false;
    let wait_detach = std::time::Instant::now();
    while let Some(evt) = rx.recv().await {
        if let UiEvent::State(s) = evt
            && s.connection == ConnectionState::Detached
        {
            detached_seen = true;
            break;
        }
        if wait_detach.elapsed() > Duration::from_secs(5) {
            panic!("timed out waiting for idle detach");
        }
    }
    assert!(
        detached_seen,
        "driver should transition to Detached on idle"
    );

    // Send Line to reattach
    handle
        .send(UiCommand::Line("wake up".to_string()))
        .expect("send line to reattach");

    // Wait until Live again
    let mut live_again = false;
    let wait_live = std::time::Instant::now();
    while let Some(evt) = rx.recv().await {
        if let UiEvent::State(s) = evt
            && s.connection == ConnectionState::Live
        {
            live_again = true;
            break;
        }
        if wait_live.elapsed() > Duration::from_secs(5) {
            panic!("timed out waiting for reattach to Live");
        }
    }
    assert!(
        live_again,
        "driver should transition back to Live after line"
    );

    handle.send(UiCommand::Shutdown).expect("send shutdown");
}

#[tokio::test(flavor = "multi_thread")]
async fn test_driver_reattach_failures_stopped_then_retry_attach() {
    let (url, server_task) = fake_ws_server(|listener| {
        Box::pin(async move {
            // Connection 1: initial connect + attach succeeds
            let (tcp, _) = listener.accept().await.unwrap();
            let mut ws =
                tokio_tungstenite::accept_hdr_async(tcp, |_req: &Request, resp: Response| Ok(resp))
                    .await
                    .unwrap();

            let msg = ws.next().await.unwrap().unwrap();
            let v: Value = serde_json::from_str(msg.to_text().unwrap()).unwrap();
            assert_eq!(v["method"], "initialize");
            ws.send(Message::Text(
                json!({
                    "jsonrpc": "2.0",
                    "id": v["id"],
                    "result": { "protocolVersion": 1 }
                })
                .to_string()
                .into(),
            ))
            .await
            .unwrap();

            let msg = ws.next().await.unwrap().unwrap();
            let v: Value = serde_json::from_str(msg.to_text().unwrap()).unwrap();
            assert_eq!(v["method"], "session/load");
            let load_id = v["id"].clone();
            ws.send(Message::Text(
                json!({
                    "jsonrpc": "2.0",
                    "id": load_id,
                    "result": { "sessionId": "s1" }
                })
                .to_string()
                .into(),
            ))
            .await
            .unwrap();

            ws.send(Message::Text(
                json!({
                    "jsonrpc": "2.0",
                    "method": "_honk/replay_done",
                    "params": { "sessionId": "s1", "epoch": 1, "seq": 0 }
                })
                .to_string()
                .into(),
            ))
            .await
            .unwrap();

            // Close connection 1 to force reconnect
            let _ = ws.close(None).await;
            drop(ws);

            // Connection 2: reconnect succeeds, but session/load fails 5 times
            let (tcp2, _) = listener.accept().await.unwrap();
            let mut ws2 =
                tokio_tungstenite::accept_hdr_async(tcp2, |_req: &Request, resp: Response| {
                    Ok(resp)
                })
                .await
                .unwrap();

            let msg = ws2.next().await.unwrap().unwrap();
            let v: Value = serde_json::from_str(msg.to_text().unwrap()).unwrap();
            assert_eq!(v["method"], "initialize");
            ws2.send(Message::Text(
                json!({
                    "jsonrpc": "2.0",
                    "id": v["id"],
                    "result": { "protocolVersion": 1 }
                })
                .to_string()
                .into(),
            ))
            .await
            .unwrap();

            // 5 failed session/load attempts
            for _ in 0..5 {
                let msg = ws2.next().await.unwrap().unwrap();
                let v: Value = serde_json::from_str(msg.to_text().unwrap()).unwrap();
                assert_eq!(v["method"], "session/load");
                ws2.send(Message::Text(
                    json!({
                        "jsonrpc": "2.0",
                        "id": v["id"],
                        "error": { "code": -32000, "message": "held-session limit reached" }
                    })
                    .to_string()
                    .into(),
                ))
                .await
                .unwrap();
            }

            // Driver enters Stopped. Wait for RetryAttach -> send successful session/load
            let msg = ws2.next().await.unwrap().unwrap();
            let v: Value = serde_json::from_str(msg.to_text().unwrap()).unwrap();
            assert_eq!(v["method"], "session/load");
            ws2.send(Message::Text(
                json!({
                    "jsonrpc": "2.0",
                    "id": v["id"],
                    "result": { "sessionId": "s1" }
                })
                .to_string()
                .into(),
            ))
            .await
            .unwrap();

            // Drain close
            while let Some(msg_res) = ws2.next().await {
                if let Ok(Message::Close(_)) = msg_res {
                    break;
                }
            }
        })
    })
    .await;

    let opts = DriverOpts::new(url, "secret", Target::Attach("s1".to_string()));
    let (handle, mut rx) = start(opts).await.expect("start driver");

    // Wait until Live initially
    while let Some(evt) = rx.recv().await {
        if let UiEvent::State(s) = evt
            && s.connection == ConnectionState::Live
            && !s.replaying
        {
            break;
        }
    }

    // Wait for driver to transition through 5 failed attempts to Stopped
    let mut stopped_seen = false;
    let wait_stopped = Instant::now();
    while let Some(evt) = rx.recv().await {
        if let UiEvent::State(s) = evt
            && matches!(s.connection, ConnectionState::Stopped { .. })
        {
            stopped_seen = true;
            break;
        }
        if wait_stopped.elapsed() > Duration::from_secs(10) {
            panic!("timed out waiting for Stopped state");
        }
    }
    assert!(
        stopped_seen,
        "driver should enter Stopped after 5 failed re-attach attempts"
    );

    // Send RetryAttach
    handle
        .send(UiCommand::RetryAttach)
        .expect("send retry attach");

    // Wait until Live again
    let mut live_again = false;
    let wait_live = Instant::now();
    while let Some(evt) = rx.recv().await {
        if let UiEvent::State(s) = evt
            && s.connection == ConnectionState::Live
        {
            live_again = true;
            break;
        }
        if wait_live.elapsed() > Duration::from_secs(5) {
            panic!("timed out waiting for Live state after RetryAttach");
        }
    }
    assert!(live_again, "driver should return to Live after RetryAttach");

    handle.send(UiCommand::Shutdown).expect("send shutdown");
    let _ = server_task.await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_driver_shutdown_during_attach_backoff_returns_promptly() {
    let (url, server_task) = fake_ws_server(|listener| {
        Box::pin(async move {
            let (tcp, _) = listener.accept().await.unwrap();
            let mut ws =
                tokio_tungstenite::accept_hdr_async(tcp, |_req: &Request, resp: Response| Ok(resp))
                    .await
                    .unwrap();

            let msg = ws.next().await.unwrap().unwrap();
            let v: Value = serde_json::from_str(msg.to_text().unwrap()).unwrap();
            ws.send(Message::Text(
                json!({
                    "jsonrpc": "2.0",
                    "id": v["id"],
                    "result": { "protocolVersion": 1 }
                })
                .to_string()
                .into(),
            ))
            .await
            .unwrap();

            let msg = ws.next().await.unwrap().unwrap();
            let v: Value = serde_json::from_str(msg.to_text().unwrap()).unwrap();
            ws.send(Message::Text(
                json!({
                    "jsonrpc": "2.0",
                    "id": v["id"],
                    "result": { "sessionId": "s1" }
                })
                .to_string()
                .into(),
            ))
            .await
            .unwrap();

            ws.send(Message::Text(
                json!({
                    "jsonrpc": "2.0",
                    "method": "_honk/replay_done",
                    "params": { "sessionId": "s1", "epoch": 1, "seq": 0 }
                })
                .to_string()
                .into(),
            ))
            .await
            .unwrap();

            let _ = ws.close(None).await;
            drop(ws);

            let (tcp2, _) = listener.accept().await.unwrap();
            let mut ws2 =
                tokio_tungstenite::accept_hdr_async(tcp2, |_req: &Request, resp: Response| {
                    Ok(resp)
                })
                .await
                .unwrap();

            let msg = ws2.next().await.unwrap().unwrap();
            let v: Value = serde_json::from_str(msg.to_text().unwrap()).unwrap();
            ws2.send(Message::Text(
                json!({
                    "jsonrpc": "2.0",
                    "id": v["id"],
                    "result": { "protocolVersion": 1 }
                })
                .to_string()
                .into(),
            ))
            .await
            .unwrap();

            // Fail session/load attempt -> driver enters backoff
            let msg = ws2.next().await.unwrap().unwrap();
            let v: Value = serde_json::from_str(msg.to_text().unwrap()).unwrap();
            ws2.send(Message::Text(
                json!({
                    "jsonrpc": "2.0",
                    "id": v["id"],
                    "error": { "code": -32000, "message": "held-session limit reached" }
                })
                .to_string()
                .into(),
            ))
            .await
            .unwrap();

            while let Some(msg_res) = ws2.next().await {
                if let Ok(Message::Close(_)) = msg_res {
                    break;
                }
            }
        })
    })
    .await;

    let opts = DriverOpts::new(url, "secret", Target::Attach("s1".to_string()));
    let (handle, mut rx) = start(opts).await.expect("start driver");

    while let Some(evt) = rx.recv().await {
        if let UiEvent::State(s) = evt
            && s.connection == ConnectionState::Live
            && !s.replaying
        {
            break;
        }
    }

    // Wait until driver sees attach failed notice and enters backoff
    while let Some(evt) = rx.recv().await {
        if let UiEvent::Notice(n) = evt
            && n.text.contains("attach failed")
            && n.text.contains("retrying in")
        {
            break;
        }
    }

    // Shutdown should return promptly (<200ms)
    let start_shutdown = Instant::now();
    handle.send(UiCommand::Shutdown).expect("send shutdown");

    let join_res = tokio::time::timeout(Duration::from_millis(500), handle.join()).await;
    assert!(
        join_res.is_ok(),
        "handle.join() must complete promptly during backoff"
    );
    assert!(start_shutdown.elapsed() < Duration::from_millis(500));
    let _ = server_task.await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_driver_quit_while_stopped_ends_task() {
    let (url, server_task) = fake_ws_server(|listener| {
        Box::pin(async move {
            let (tcp, _) = listener.accept().await.unwrap();
            let mut ws =
                tokio_tungstenite::accept_hdr_async(tcp, |_req: &Request, resp: Response| Ok(resp))
                    .await
                    .unwrap();

            let msg = ws.next().await.unwrap().unwrap();
            let v: Value = serde_json::from_str(msg.to_text().unwrap()).unwrap();
            ws.send(Message::Text(
                json!({
                    "jsonrpc": "2.0",
                    "id": v["id"],
                    "result": { "protocolVersion": 1 }
                })
                .to_string()
                .into(),
            ))
            .await
            .unwrap();

            let msg = ws.next().await.unwrap().unwrap();
            let v: Value = serde_json::from_str(msg.to_text().unwrap()).unwrap();
            ws.send(Message::Text(
                json!({
                    "jsonrpc": "2.0",
                    "id": v["id"],
                    "result": { "sessionId": "s1" }
                })
                .to_string()
                .into(),
            ))
            .await
            .unwrap();

            ws.send(Message::Text(
                json!({
                    "jsonrpc": "2.0",
                    "method": "_honk/replay_done",
                    "params": { "sessionId": "s1", "epoch": 1, "seq": 0 }
                })
                .to_string()
                .into(),
            ))
            .await
            .unwrap();

            let _ = ws.close(None).await;
            drop(ws);

            let (tcp2, _) = listener.accept().await.unwrap();
            let mut ws2 =
                tokio_tungstenite::accept_hdr_async(tcp2, |_req: &Request, resp: Response| {
                    Ok(resp)
                })
                .await
                .unwrap();

            let msg = ws2.next().await.unwrap().unwrap();
            let v: Value = serde_json::from_str(msg.to_text().unwrap()).unwrap();
            ws2.send(Message::Text(
                json!({
                    "jsonrpc": "2.0",
                    "id": v["id"],
                    "result": { "protocolVersion": 1 }
                })
                .to_string()
                .into(),
            ))
            .await
            .unwrap();

            // 5 failed session/load attempts
            for _ in 0..5 {
                let msg = ws2.next().await.unwrap().unwrap();
                let v: Value = serde_json::from_str(msg.to_text().unwrap()).unwrap();
                ws2.send(Message::Text(
                    json!({
                        "jsonrpc": "2.0",
                        "id": v["id"],
                        "error": { "code": -32000, "message": "held-session limit reached" }
                    })
                    .to_string()
                    .into(),
                ))
                .await
                .unwrap();
            }

            // Driver is in Stopped state, receives /quit
            while let Some(msg_res) = ws2.next().await {
                if let Ok(Message::Close(_)) = msg_res {
                    break;
                }
            }
        })
    })
    .await;

    let opts = DriverOpts::new(url, "secret", Target::Attach("s1".to_string()));
    let (handle, mut rx) = start(opts).await.expect("start driver");

    while let Some(evt) = rx.recv().await {
        if let UiEvent::State(s) = evt
            && s.connection == ConnectionState::Live
            && !s.replaying
        {
            break;
        }
    }

    while let Some(evt) = rx.recv().await {
        if let UiEvent::State(s) = evt
            && matches!(s.connection, ConnectionState::Stopped { .. })
        {
            break;
        }
    }

    // Send /quit line
    handle
        .send(UiCommand::Line("/quit".to_string()))
        .expect("send /quit");

    // Join handle must complete cleanly
    let join_res = tokio::time::timeout(Duration::from_secs(2), handle.join()).await;
    assert!(
        join_res.is_ok(),
        "/quit while Stopped must terminate driver task"
    );
    let _ = server_task.await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_driver_attach_failed_auth_error_on_reconnect_is_fatal() {
    let (url, server_task) = fake_ws_server(|listener| {
        Box::pin(async move {
            let (tcp, _) = listener.accept().await.unwrap();
            let mut ws =
                tokio_tungstenite::accept_hdr_async(tcp, |_req: &Request, resp: Response| Ok(resp))
                    .await
                    .unwrap();

            let msg = ws.next().await.unwrap().unwrap();
            let v: Value = serde_json::from_str(msg.to_text().unwrap()).unwrap();
            ws.send(Message::Text(
                json!({
                    "jsonrpc": "2.0",
                    "id": v["id"],
                    "result": { "protocolVersion": 1 }
                })
                .to_string()
                .into(),
            ))
            .await
            .unwrap();

            let msg = ws.next().await.unwrap().unwrap();
            let v: Value = serde_json::from_str(msg.to_text().unwrap()).unwrap();
            ws.send(Message::Text(
                json!({
                    "jsonrpc": "2.0",
                    "id": v["id"],
                    "result": { "sessionId": "s1" }
                })
                .to_string()
                .into(),
            ))
            .await
            .unwrap();

            ws.send(Message::Text(
                json!({
                    "jsonrpc": "2.0",
                    "method": "_honk/replay_done",
                    "params": { "sessionId": "s1", "epoch": 1, "seq": 0 }
                })
                .to_string()
                .into(),
            ))
            .await
            .unwrap();

            let _ = ws.close(None).await;
            drop(ws);

            let (tcp2, _) = listener.accept().await.unwrap();
            let mut ws2 =
                tokio_tungstenite::accept_hdr_async(tcp2, |_req: &Request, resp: Response| {
                    Ok(resp)
                })
                .await
                .unwrap();

            let msg = ws2.next().await.unwrap().unwrap();
            let v: Value = serde_json::from_str(msg.to_text().unwrap()).unwrap();
            ws2.send(Message::Text(
                json!({
                    "jsonrpc": "2.0",
                    "id": v["id"],
                    "result": { "protocolVersion": 1 }
                })
                .to_string()
                .into(),
            ))
            .await
            .unwrap();

            // Fail session/load with AUTH error (401)
            let msg = ws2.next().await.unwrap().unwrap();
            let v: Value = serde_json::from_str(msg.to_text().unwrap()).unwrap();
            ws2.send(Message::Text(json!({
                "jsonrpc": "2.0",
                "id": v["id"],
                "error": { "code": 401, "message": "authentication failed: session unauthorized" }
            }).to_string().into())).await.unwrap();

            while let Some(msg_res) = ws2.next().await {
                if let Ok(Message::Close(_)) = msg_res {
                    break;
                }
            }
        })
    })
    .await;

    let opts = DriverOpts::new(url, "secret", Target::Attach("s1".to_string()));
    let (handle, mut rx) = start(opts).await.expect("start driver");

    while let Some(evt) = rx.recv().await {
        if let UiEvent::State(s) = evt
            && s.connection == ConnectionState::Live
            && !s.replaying
        {
            break;
        }
    }

    // Driver should receive Fatal event due to auth failure on reconnect
    let mut fatal_seen = false;
    let mut exit_seen = false;
    while let Some(evt) = rx.recv().await {
        match evt {
            UiEvent::Fatal(msg) => {
                assert!(
                    msg.contains("authentication failed") || msg.contains("401"),
                    "fatal msg: {msg}"
                );
                fatal_seen = true;
            }
            UiEvent::Exit(code) => {
                assert_eq!(code, 1);
                exit_seen = true;
                break;
            }
            _ => {}
        }
    }
    assert!(fatal_seen, "auth failure on reconnect must emit Fatal");
    assert!(exit_seen, "auth failure on reconnect must emit Exit(1)");

    let join_res = tokio::time::timeout(Duration::from_secs(2), handle.join()).await;
    assert!(join_res.is_ok(), "task must complete on fatal auth error");
    let _ = server_task.await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_session_info_with_cwd_emitted_on_attach_and_new() {
    let Some(g) = StubGoose::start_or_skip().await else {
        eprintln!("skipping test: StubGoose unset");
        return;
    };
    let Some(honkd_bin) = find_honkd_bin() else {
        eprintln!("skipping test: honkd binary not found");
        return;
    };

    let h = HonkdHarness::spawn(&honkd_bin, &g.url(), g.secret_file(), None).await;

    // Part 1: Target::Attach
    let mut client = honk::Client::connect(&h.ws_url(), &h.secret())
        .await
        .expect("raw connect");

    let new_res = client
        .call(
            "session/new",
            serde_json::json!({
                "cwd": g.cwd().to_str().unwrap(),
                "mcpServers": [],
            }),
        )
        .await
        .expect("session/new");
    let sid = new_res["result"]["sessionId"]
        .as_str()
        .expect("sessionId")
        .to_string();

    let _ = client
        .call(
            "session/prompt",
            serde_json::json!({
                "sessionId": sid,
                "prompt": [{ "type": "text", "text": "initial prompt" }]
            }),
        )
        .await
        .expect("session/prompt");
    drop(client);

    let opts = DriverOpts::new(h.ws_url(), h.secret(), Target::Attach(sid.clone()));
    let (handle, mut rx) = start(opts).await.expect("start driver attach");

    let mut session_info_seen = false;
    let mut seen_cwd = None;

    let start_time = std::time::Instant::now();
    while let Some(evt) = rx.recv().await {
        if let UiEvent::SessionInfo { title: _, cwd } = evt {
            session_info_seen = true;
            seen_cwd = cwd;
            break;
        }
        if start_time.elapsed() > Duration::from_secs(10) {
            panic!("timed out waiting for SessionInfo on attach");
        }
    }

    assert!(session_info_seen, "SessionInfo must be emitted on attach");
    assert_eq!(seen_cwd.as_deref(), Some(g.cwd().to_str().unwrap()));
    handle.send(UiCommand::Shutdown).expect("send shutdown");

    // Part 2: Target::New
    let tmp = tempfile::tempdir().expect("create tmp cwd");
    let tmp_cwd = tmp.path().to_path_buf();
    let opts_new = DriverOpts::new(
        h.ws_url(),
        h.secret(),
        Target::New {
            cwd: tmp_cwd.clone(),
            extensions: None,
        },
    );
    let (handle_new, mut rx_new) = start(opts_new).await.expect("start driver new");

    let mut session_info_new_seen = false;
    let mut seen_cwd_new = None;

    let start_time = std::time::Instant::now();
    while let Some(evt) = rx_new.recv().await {
        if let UiEvent::SessionInfo { title: _, cwd } = evt {
            session_info_new_seen = true;
            seen_cwd_new = cwd;
            break;
        }
        if start_time.elapsed() > Duration::from_secs(10) {
            panic!("timed out waiting for SessionInfo on new");
        }
    }

    assert!(session_info_new_seen, "SessionInfo must be emitted on new");
    assert_eq!(seen_cwd_new.as_deref(), Some(tmp_cwd.to_str().unwrap()));
    handle_new.send(UiCommand::Shutdown).expect("send shutdown");
}

#[tokio::test(flavor = "multi_thread")]
async fn test_reconnect_backoff_successive_delays_grow() {
    let (url, server_task) = fake_ws_server(|listener| {
        Box::pin(async move {
            // Connection 1: initialize + session/load, then drop immediately
            let (tcp1, _) = listener.accept().await.unwrap();
            let mut ws1 =
                tokio_tungstenite::accept_hdr_async(tcp1, |_req: &Request, resp: Response| {
                    Ok(resp)
                })
                .await
                .unwrap();

            let msg = ws1.next().await.unwrap().unwrap();
            let v: Value = serde_json::from_str(msg.to_text().unwrap()).unwrap();
            ws1.send(Message::Text(
                json!({ "jsonrpc": "2.0", "id": v["id"], "result": { "protocolVersion": 1 } })
                    .to_string()
                    .into(),
            ))
            .await
            .unwrap();

            let msg = ws1.next().await.unwrap().unwrap();
            let v: Value = serde_json::from_str(msg.to_text().unwrap()).unwrap();
            let load_id = v["id"].clone();
            ws1.send(Message::Text(
                json!({ "jsonrpc": "2.0", "id": load_id, "result": { "sessionId": "s1" } })
                    .to_string()
                    .into(),
            ))
            .await
            .unwrap();

            let drop_1_time = Instant::now();
            drop(ws1);

            // Connection 2: must reconnect after backoff (250ms)
            let (tcp2, _) = listener.accept().await.unwrap();
            let connect_2_time = Instant::now();
            let delay1 = connect_2_time.duration_since(drop_1_time);

            let mut ws2 =
                tokio_tungstenite::accept_hdr_async(tcp2, |_req: &Request, resp: Response| {
                    Ok(resp)
                })
                .await
                .unwrap();

            let msg = ws2.next().await.unwrap().unwrap();
            let v: Value = serde_json::from_str(msg.to_text().unwrap()).unwrap();
            ws2.send(Message::Text(
                json!({ "jsonrpc": "2.0", "id": v["id"], "result": { "protocolVersion": 1 } })
                    .to_string()
                    .into(),
            ))
            .await
            .unwrap();

            let msg = ws2.next().await.unwrap().unwrap();
            let v: Value = serde_json::from_str(msg.to_text().unwrap()).unwrap();
            let load_id = v["id"].clone();
            ws2.send(Message::Text(
                json!({ "jsonrpc": "2.0", "id": load_id, "result": { "sessionId": "s1" } })
                    .to_string()
                    .into(),
            ))
            .await
            .unwrap();

            let drop_2_time = Instant::now();
            drop(ws2);

            // Connection 3: must reconnect after larger backoff (500ms)
            let (tcp3, _) = listener.accept().await.unwrap();
            let connect_3_time = Instant::now();
            let delay2 = connect_3_time.duration_since(drop_2_time);

            assert!(
                delay1 >= Duration::from_millis(180),
                "delay 1 should be around 250ms, got: {delay1:?}"
            );
            assert!(
                delay2 >= Duration::from_millis(380),
                "delay 2 should be around 500ms, got: {delay2:?}"
            );
            assert!(
                delay2 > delay1,
                "successive reconnect delays must grow: delay1={delay1:?}, delay2={delay2:?}"
            );

            let mut ws3 =
                tokio_tungstenite::accept_hdr_async(tcp3, |_req: &Request, resp: Response| {
                    Ok(resp)
                })
                .await
                .unwrap();
            let msg = ws3.next().await.unwrap().unwrap();
            let v: Value = serde_json::from_str(msg.to_text().unwrap()).unwrap();
            ws3.send(Message::Text(
                json!({ "jsonrpc": "2.0", "id": v["id"], "result": { "protocolVersion": 1 } })
                    .to_string()
                    .into(),
            ))
            .await
            .unwrap();
            drop(ws3);
        })
    })
    .await;

    let opts = DriverOpts::new(url, "secret", Target::Attach("s1".to_string()));
    let (handle, mut rx) = start(opts).await.expect("start driver");

    // Server task handles connection 1, connection 2, connection 3 and verifies delays
    tokio::time::timeout(Duration::from_secs(5), server_task)
        .await
        .expect("server task timed out")
        .unwrap();

    let mut attempt_2_seen = false;
    let mut attempt_3_seen = false;
    while let Ok(evt) = rx.try_recv() {
        if let UiEvent::State(s) = evt
            && let ConnectionState::Reconnecting { attempt } = s.connection
        {
            if attempt == 2 {
                attempt_2_seen = true;
            }
            if attempt == 3 {
                attempt_3_seen = true;
            }
        }
    }

    assert!(attempt_2_seen, "must see Reconnecting attempt 2");
    assert!(attempt_3_seen, "must see Reconnecting attempt 3");

    let _ = handle.send(UiCommand::Shutdown);
}

#[tokio::test(flavor = "multi_thread")]
async fn test_driver_session_info_finds_target_on_page_3() {
    let (url, server_task) = fake_ws_server(|listener| {
        Box::pin(async move {
            let (tcp, _) = listener.accept().await.unwrap();
            let mut ws =
                tokio_tungstenite::accept_hdr_async(tcp, |_req: &Request, resp: Response| Ok(resp))
                    .await
                    .unwrap();

            // 1. initialize
            let msg = ws.next().await.unwrap().unwrap();
            let v: Value = serde_json::from_str(msg.to_text().unwrap()).unwrap();
            assert_eq!(v["method"], "initialize");
            ws.send(Message::Text(
                json!({ "jsonrpc": "2.0", "id": v["id"], "result": { "protocolVersion": 1 } })
                    .to_string()
                    .into(),
            ))
            .await
            .unwrap();

            // 2. session/load
            let msg = ws.next().await.unwrap().unwrap();
            let v: Value = serde_json::from_str(msg.to_text().unwrap()).unwrap();
            assert_eq!(v["method"], "session/load");
            let load_id = v["id"].clone();
            ws.send(Message::Text(
                json!({ "jsonrpc": "2.0", "id": load_id, "result": { "sessionId": "target-ses" } })
                    .to_string()
                    .into(),
            ))
            .await
            .unwrap();

            // 3. session/list page 1 (50 dummy sessions)
            let msg = ws.next().await.unwrap().unwrap();
            let v1: Value = serde_json::from_str(msg.to_text().unwrap()).unwrap();
            assert_eq!(v1["method"], "session/list");
            let mut p1_sessions = Vec::new();
            for i in 0..50 {
                p1_sessions.push(json!({
                    "sessionId": format!("dummy_p1_{i}"),
                    "title": format!("Dummy {i}"),
                    "cwd": "/dummy",
                }));
            }
            ws.send(Message::Text(
                json!({
                    "jsonrpc": "2.0",
                    "id": v1["id"],
                    "result": {
                        "sessions": p1_sessions,
                        "nextCursor": "page2_cursor"
                    }
                })
                .to_string()
                .into(),
            ))
            .await
            .unwrap();

            // 4. session/list page 2 (50 dummy sessions)
            let msg = ws.next().await.unwrap().unwrap();
            let v2: Value = serde_json::from_str(msg.to_text().unwrap()).unwrap();
            assert_eq!(v2["method"], "session/list");
            assert_eq!(v2["params"]["cursor"], "page2_cursor");
            let mut p2_sessions = Vec::new();
            for i in 0..50 {
                p2_sessions.push(json!({
                    "sessionId": format!("dummy_p2_{i}"),
                    "title": format!("Dummy {i}"),
                    "cwd": "/dummy",
                }));
            }
            ws.send(Message::Text(
                json!({
                    "jsonrpc": "2.0",
                    "id": v2["id"],
                    "result": {
                        "sessions": p2_sessions,
                        "nextCursor": "page3_cursor"
                    }
                })
                .to_string()
                .into(),
            ))
            .await
            .unwrap();

            // 5. session/list page 3 (contains target-ses)
            let msg = ws.next().await.unwrap().unwrap();
            let v3: Value = serde_json::from_str(msg.to_text().unwrap()).unwrap();
            assert_eq!(v3["method"], "session/list");
            assert_eq!(v3["params"]["cursor"], "page3_cursor");
            ws.send(Message::Text(
                json!({
                    "jsonrpc": "2.0",
                    "id": v3["id"],
                    "result": {
                        "sessions": [
                            {
                                "sessionId": "target-ses",
                                "title": "Deep Session Title",
                                "cwd": "/deep/work/dir"
                            }
                        ]
                    }
                })
                .to_string()
                .into(),
            ))
            .await
            .unwrap();
        })
    })
    .await;

    let opts = DriverOpts::new(url, "secret", Target::Attach("target-ses".to_string()));
    let (handle, mut rx) = start(opts).await.expect("start driver");

    let mut found_session_info = false;
    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline {
        match tokio::time::timeout(Duration::from_millis(500), rx.recv()).await {
            Ok(Some(UiEvent::SessionInfo { title, cwd })) => {
                if title.as_deref() == Some("Deep Session Title")
                    && cwd.as_deref() == Some("/deep/work/dir")
                {
                    found_session_info = true;
                    break;
                }
            }
            Ok(Some(_)) => {}
            Ok(None) => break,
            Err(_) => {}
        }
    }

    assert!(
        found_session_info,
        "driver must follow nextCursor to page 3 and find session title and cwd"
    );

    let _ = handle.send(UiCommand::Shutdown);
    let _ = server_task.await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_driver_forwards_known_cwd_to_session_list() {
    let (url, server_task) = fake_ws_server(|listener| {
        Box::pin(async move {
            let (tcp, _) = listener.accept().await.unwrap();
            let mut ws =
                tokio_tungstenite::accept_hdr_async(tcp, |_req: &Request, resp: Response| Ok(resp))
                    .await
                    .unwrap();

            // 1. initialize
            let msg = ws.next().await.unwrap().unwrap();
            let v: Value = serde_json::from_str(msg.to_text().unwrap()).unwrap();
            assert_eq!(v["method"], "initialize");
            ws.send(Message::Text(
                json!({ "jsonrpc": "2.0", "id": v["id"], "result": { "protocolVersion": 1 } })
                    .to_string()
                    .into(),
            ))
            .await
            .unwrap();

            // 2. session/new
            let msg = ws.next().await.unwrap().unwrap();
            let v: Value = serde_json::from_str(msg.to_text().unwrap()).unwrap();
            assert_eq!(v["method"], "session/new");
            let new_id = v["id"].clone();
            ws.send(Message::Text(
                json!({
                    "jsonrpc": "2.0",
                    "id": new_id,
                    "result": {
                        "sessionId": "s_new",
                        "cwd": "/test/known/driver/cwd"
                    }
                })
                .to_string()
                .into(),
            ))
            .await
            .unwrap();

            // 3. session/load
            let msg = ws.next().await.unwrap().unwrap();
            let v: Value = serde_json::from_str(msg.to_text().unwrap()).unwrap();
            assert_eq!(v["method"], "session/load");
            let load_id = v["id"].clone();
            ws.send(Message::Text(
                json!({ "jsonrpc": "2.0", "id": load_id, "result": { "sessionId": "s_new" } })
                    .to_string()
                    .into(),
            ))
            .await
            .unwrap();

            // 4. session/list must have cwd forwarded
            let msg = ws.next().await.unwrap().unwrap();
            let v: Value = serde_json::from_str(msg.to_text().unwrap()).unwrap();
            assert_eq!(v["method"], "session/list");
            assert_eq!(v["params"]["cwd"], "/test/known/driver/cwd");
            ws.send(Message::Text(
                json!({
                    "jsonrpc": "2.0",
                    "id": v["id"],
                    "result": {
                        "sessions": [
                            {
                                "sessionId": "s_new",
                                "title": "New Session",
                                "cwd": "/test/known/driver/cwd"
                            }
                        ]
                    }
                })
                .to_string()
                .into(),
            ))
            .await
            .unwrap();
        })
    })
    .await;

    let opts = DriverOpts::new(
        url,
        "secret",
        Target::New {
            cwd: PathBuf::from("/test/known/driver/cwd"),
            extensions: None,
        },
    );
    let (handle, mut rx) = start(opts).await.expect("start driver");

    let mut found_session_info = false;
    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline {
        match tokio::time::timeout(Duration::from_millis(500), rx.recv()).await {
            Ok(Some(UiEvent::SessionInfo { title, cwd })) => {
                if title.as_deref() == Some("New Session")
                    && cwd.as_deref() == Some("/test/known/driver/cwd")
                {
                    found_session_info = true;
                    break;
                }
            }
            Ok(Some(_)) => {}
            Ok(None) => break,
            Err(_) => {}
        }
    }

    assert!(
        found_session_info,
        "driver must forward known cwd to session/list and emit SessionInfo"
    );

    let _ = handle.send(UiCommand::Shutdown);
    let _ = server_task.await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_driver_session_info_terminates_on_repeated_cursor() {
    let (url, server_task) = fake_ws_server(|listener| {
        Box::pin(async move {
            let (tcp, _) = listener.accept().await.unwrap();
            let mut ws =
                tokio_tungstenite::accept_hdr_async(tcp, |_req: &Request, resp: Response| Ok(resp))
                    .await
                    .unwrap();

            // 1. initialize
            let msg = ws.next().await.unwrap().unwrap();
            let v: Value = serde_json::from_str(msg.to_text().unwrap()).unwrap();
            ws.send(Message::Text(
                json!({ "jsonrpc": "2.0", "id": v["id"], "result": { "protocolVersion": 1 } })
                    .to_string()
                    .into(),
            ))
            .await
            .unwrap();

            // 2. session/load
            let msg = ws.next().await.unwrap().unwrap();
            let v: Value = serde_json::from_str(msg.to_text().unwrap()).unwrap();
            let load_id = v["id"].clone();
            ws.send(Message::Text(
                json!({ "jsonrpc": "2.0", "id": load_id, "result": { "sessionId": "target-ses" } })
                    .to_string()
                    .into(),
            ))
            .await
            .unwrap();

            // 3. session/list page 1 returns loop_cursor
            let msg = ws.next().await.unwrap().unwrap();
            let v1: Value = serde_json::from_str(msg.to_text().unwrap()).unwrap();
            assert_eq!(v1["method"], "session/list");
            ws.send(Message::Text(
                json!({
                    "jsonrpc": "2.0",
                    "id": v1["id"],
                    "result": {
                        "sessions": [],
                        "nextCursor": "loop_cursor"
                    }
                })
                .to_string()
                .into(),
            ))
            .await
            .unwrap();

            // 4. session/list page 2 returns loop_cursor again
            let msg = ws.next().await.unwrap().unwrap();
            let v2: Value = serde_json::from_str(msg.to_text().unwrap()).unwrap();
            assert_eq!(v2["method"], "session/list");
            assert_eq!(v2["params"]["cursor"], "loop_cursor");
            ws.send(Message::Text(
                json!({
                    "jsonrpc": "2.0",
                    "id": v2["id"],
                    "result": {
                        "sessions": [],
                        "nextCursor": "loop_cursor"
                    }
                })
                .to_string()
                .into(),
            ))
            .await
            .unwrap();
        })
    })
    .await;

    let opts = DriverOpts::new(url, "secret", Target::Attach("target-ses".to_string()));
    let (handle, mut rx) = start(opts).await.expect("start driver");

    let mut session_info_seen = false;
    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline {
        match tokio::time::timeout(Duration::from_millis(500), rx.recv()).await {
            Ok(Some(UiEvent::SessionInfo { .. })) => {
                session_info_seen = true;
                break;
            }
            Ok(Some(_)) => {}
            Ok(None) => break,
            Err(_) => {}
        }
    }

    assert!(
        session_info_seen,
        "driver must terminate paging loop on repeated cursor and emit SessionInfo"
    );

    let _ = handle.send(UiCommand::Shutdown);
    let _ = server_task.await;
}
