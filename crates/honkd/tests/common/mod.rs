#![allow(dead_code)]

use futures::{SinkExt, StreamExt};
use serde_json::{Value, json};
use std::io::{BufRead, BufReader};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tempfile::TempDir;
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::protocol::Message;

pub struct Honkd {
    child: Option<Child>,
    pub addr: SocketAddr,
    pub state_dir: TempDir,
    upstream: String,
    upstream_secret: PathBuf,
    extra: Vec<String>,
    pub stderr_buf: Arc<Mutex<String>>,
    pub stdout_buf: Arc<Mutex<String>>,
}

impl Honkd {
    pub async fn spawn(upstream: &str, upstream_secret: &Path, extra: &[&str]) -> Self {
        Self::spawn_with_env(upstream, upstream_secret, extra, &[]).await
    }

    pub async fn spawn_with_env(
        upstream: &str,
        upstream_secret: &Path,
        extra: &[&str],
        env: &[(&str, &str)],
    ) -> Self {
        let state_dir = tempfile::tempdir().expect("create temp dir");
        let (child, addr, stderr_buf, stdout_buf) = spawn_process(
            None, // listen on 127.0.0.1:0
            upstream,
            upstream_secret,
            state_dir.path(),
            extra,
            env,
        )
        .await;

        Self {
            child: Some(child),
            addr,
            state_dir,
            upstream: upstream.to_string(),
            upstream_secret: upstream_secret.to_path_buf(),
            extra: extra.iter().map(|s| s.to_string()).collect(),
            stderr_buf,
            stdout_buf,
        }
    }

    pub fn stderr_text(&self) -> String {
        self.stderr_buf
            .lock()
            .map(|s| s.clone())
            .unwrap_or_default()
    }

    pub fn stdout_text(&self) -> String {
        self.stdout_buf
            .lock()
            .map(|s| s.clone())
            .unwrap_or_default()
    }

    pub fn secret(&self) -> String {
        let path = self.state_dir.path().join("secret");
        std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("failed to read secret at {}: {e}", path.display()))
            .trim()
            .to_string()
    }

    pub fn ws_url(&self) -> String {
        format!("ws://{}/acp", self.addr)
    }

    pub fn kill9(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }

    pub async fn wait_for_exit(&mut self, timeout: Duration) -> Option<std::process::ExitStatus> {
        let start = Instant::now();
        while start.elapsed() < timeout {
            if let Some(child) = self.child.as_mut() {
                match child.try_wait() {
                    Ok(Some(status)) => return Some(status),
                    Ok(None) => {}
                    Err(_) => return None,
                }
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        None
    }

    pub async fn restart(&mut self) {
        self.restart_with_env(&[]).await;
    }

    pub async fn restart_with_env(&mut self, env: &[(&str, &str)]) {
        self.kill9();

        let bind_start = Instant::now();
        loop {
            match std::net::TcpListener::bind(self.addr) {
                Ok(l) => {
                    drop(l);
                    break;
                }
                Err(_) => {
                    if bind_start.elapsed() > Duration::from_secs(5) {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
            }
        }

        let extra_refs: Vec<&str> = self.extra.iter().map(|s| s.as_str()).collect();
        let (child, addr, stderr_buf, stdout_buf) = spawn_process(
            Some(self.addr), // restart reuses explicit port!
            &self.upstream,
            &self.upstream_secret,
            self.state_dir.path(),
            &extra_refs,
            env,
        )
        .await;

        assert_eq!(addr, self.addr, "restarted honkd bound expected address");
        self.child = Some(child);
        self.stderr_buf = stderr_buf;
        self.stdout_buf = stdout_buf;
    }

    pub fn spawn_expect_exit(upstream: &str, secret: &Path, extra: &[&str]) -> (i32, String) {
        let temp_dir = tempfile::tempdir().expect("create temp dir");

        let mut cmd = Command::new(env!("CARGO_BIN_EXE_honkd"));
        let mut has_listen = false;
        let mut has_state_dir = false;
        let mut has_secret_file = false;

        for arg in extra {
            if *arg == "--listen" || arg.starts_with("--listen=") {
                has_listen = true;
            }
            if *arg == "--state-dir" || arg.starts_with("--state-dir=") {
                has_state_dir = true;
            }
            if *arg == "--upstream-secret-file" || arg.starts_with("--upstream-secret-file=") {
                has_secret_file = true;
            }
        }

        if !has_listen {
            cmd.arg("--listen").arg("127.0.0.1:0");
        }
        cmd.arg("--upstream").arg(upstream);
        if !has_secret_file {
            cmd.arg("--upstream-secret-file").arg(secret);
        }
        if !has_state_dir {
            cmd.arg("--state-dir").arg(temp_dir.path());
        }
        for arg in extra {
            cmd.arg(arg);
        }

        cmd.stdout(Stdio::piped());
        cmd.stderr(Stdio::piped());

        let output = cmd.output().expect("run honkd");
        let code = output.status.code().unwrap_or(-1);
        let stderr = String::from_utf8_lossy(&output.stderr).to_string();
        (code, stderr)
    }
}

impl Drop for Honkd {
    fn drop(&mut self) {
        self.kill9();
    }
}

async fn spawn_process(
    explicit_addr: Option<SocketAddr>,
    upstream: &str,
    upstream_secret: &Path,
    state_dir: &Path,
    extra: &[&str],
    env: &[(&str, &str)],
) -> (Child, SocketAddr, Arc<Mutex<String>>, Arc<Mutex<String>>) {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_honkd"));
    for (k, v) in env {
        cmd.env(k, v);
    }
    let mut has_listen = false;
    for arg in extra {
        if *arg == "--listen" || arg.starts_with("--listen=") {
            has_listen = true;
        }
    }

    if !has_listen {
        let listen_addr = explicit_addr
            .map(|a| a.to_string())
            .unwrap_or_else(|| "127.0.0.1:0".to_string());
        cmd.arg("--listen").arg(listen_addr);
    }
    cmd.arg("--upstream")
        .arg(upstream)
        .arg("--upstream-secret-file")
        .arg(upstream_secret)
        .arg("--state-dir")
        .arg(state_dir);
    for arg in extra {
        cmd.arg(arg);
    }
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());

    let mut child = cmd.spawn().expect("spawn honkd");
    let stdout = child.stdout.take().expect("stdout");
    let stderr = child.stderr.take().expect("stderr");

    let stderr_buf = Arc::new(Mutex::new(String::new()));
    let stderr_clone = stderr_buf.clone();
    std::thread::spawn(move || {
        let reader = BufReader::new(stderr);
        for l in reader.lines().map_while(Result::ok) {
            if let Ok(mut lock) = stderr_clone.lock() {
                lock.push_str(&l);
                lock.push('\n');
            }
        }
    });

    let stdout_buf = Arc::new(Mutex::new(String::new()));
    let stdout_clone = stdout_buf.clone();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let reader = BufReader::new(stdout);
        let mut sent_addr = false;
        for line in reader.lines().map_while(Result::ok) {
            if let Ok(mut lock) = stdout_clone.lock() {
                lock.push_str(&line);
                lock.push('\n');
            }
            if !sent_addr
                && let Some(rest) = line.strip_prefix("honkd listening on ")
                && let Ok(addr) = rest.trim().parse::<SocketAddr>()
            {
                let _ = tx.send(Ok(addr));
                sent_addr = true;
            }
        }
        if !sent_addr {
            let _ = tx.send(Err(
                "process exited without 'honkd listening on <addr>'".to_string()
            ));
        }
    });

    let res = tokio::task::spawn_blocking(move || rx.recv_timeout(Duration::from_secs(10)))
        .await
        .expect("join wait thread");

    match res {
        Ok(Ok(addr)) => (child, addr, stderr_buf, stdout_buf),
        Ok(Err(err)) => {
            let _ = child.kill();
            let _ = child.wait();
            let stderr_msg = stderr_buf.lock().map(|s| s.clone()).unwrap_or_default();
            panic!("honkd failed to report listening: {err}\nstderr:\n{stderr_msg}");
        }
        Err(timeout_err) => {
            let _ = child.kill();
            let _ = child.wait();
            let stderr_msg = stderr_buf.lock().map(|s| s.clone()).unwrap_or_default();
            panic!(
                "timed out waiting for honkd listening on: {timeout_err}\nstderr:\n{stderr_msg}"
            );
        }
    }
}

pub struct Viewer {
    ws: WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>,
    pub blocks: honk_wire::Blocks,
    pub closed: Option<u16>,
    pub raw_methods: Vec<String>,
    next_req_id: u64,
    incoming: Vec<Value>,
    accumulated_agent_text: String,
    last_prompt_id: Option<Value>,
    prompt_reply_text: Option<String>,
}

impl Viewer {
    fn new(ws: WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>) -> Self {
        Self {
            ws,
            blocks: honk_wire::Blocks::default(),
            closed: None,
            raw_methods: Vec::new(),
            next_req_id: 1,
            incoming: Vec::new(),
            accumulated_agent_text: String::new(),
            last_prompt_id: None,
            prompt_reply_text: None,
        }
    }

    pub async fn connect(honkd: &Honkd) -> Self {
        let url = honkd.ws_url();
        let mut req = url.into_client_request().expect("valid request");
        req.headers_mut().insert(
            "x-secret-key",
            honkd.secret().parse().expect("valid header value"),
        );
        let (ws, _) = tokio_tungstenite::connect_async(req)
            .await
            .expect("connect ws");
        let mut viewer = Self::new(ws);
        let _ = viewer
            .call(
                "initialize",
                json!({
                    "protocolVersion": 1,
                    "clientCapabilities": {},
                    "_meta": { "honk": {} }
                }),
            )
            .await;
        viewer
    }

    pub async fn connect_pigeon(honkd: &Honkd) -> Self {
        let url = format!("{}?token={}", honkd.ws_url(), honkd.secret());
        let (ws, _) = tokio_tungstenite::connect_async(url)
            .await
            .expect("connect ws");
        let mut viewer = Self::new(ws);
        let _ = viewer
            .call(
                "initialize",
                json!({
                    "protocolVersion": 1,
                    "clientCapabilities": {
                        "fs": {
                            "readTextFile": false,
                            "writeTextFile": false
                        }
                    }
                }),
            )
            .await;
        viewer
    }

    pub async fn request(&mut self, method: &str, params: Value) -> Value {
        let id = json!(self.next_req_id);
        self.next_req_id += 1;
        if method == "session/prompt" {
            self.last_prompt_id = Some(id.clone());
            self.accumulated_agent_text.clear();
            self.prompt_reply_text = None;
        }
        let req = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });
        self.ws
            .send(Message::Text(req.to_string().into()))
            .await
            .expect("send ws message");
        id
    }

    pub async fn recv_frame(&mut self) -> Option<Value> {
        while let Some(msg_res) = self.ws.next().await {
            match msg_res {
                Ok(Message::Text(text)) => {
                    if let Ok(v) = serde_json::from_str::<Value>(&text) {
                        if let Some(m) = v.get("method").and_then(Value::as_str) {
                            self.raw_methods.push(m.to_string());
                        }
                        self.blocks.apply(&v);
                        if v.get("method").and_then(Value::as_str) == Some("session/update")
                            && let Some(chunk) = v.pointer("/params/update")
                            && chunk.get("sessionUpdate").and_then(Value::as_str)
                                == Some("agent_message_chunk")
                            && let Some(t) = chunk.pointer("/content/text").and_then(Value::as_str)
                        {
                            self.accumulated_agent_text.push_str(t);
                        }
                        if let Some(last_id) = &self.last_prompt_id
                            && honk_upstream::rpc_id_matches(
                                last_id,
                                v.get("id").unwrap_or(&Value::Null),
                            )
                            && let Some(txt) =
                                v.pointer("/result/_meta/honk/text").and_then(Value::as_str)
                        {
                            self.prompt_reply_text = Some(txt.to_string());
                        }
                        return Some(v);
                    }
                }
                Ok(Message::Close(frame)) => {
                    self.closed = frame.map(|f| f.code.into());
                    return None;
                }
                Ok(Message::Ping(payload)) => {
                    let _ = self.ws.send(Message::Pong(payload)).await;
                }
                Ok(_) => {}
                Err(_) => {
                    return None;
                }
            }
        }
        None
    }

    pub async fn response(&mut self, id: &Value) -> Value {
        self.response_timeout(id, Duration::from_secs(15)).await
    }

    pub async fn try_response_timeout(&mut self, id: &Value, timeout: Duration) -> Option<Value> {
        if let Some(pos) = self
            .incoming
            .iter()
            .position(|v| honk_upstream::rpc_id_matches(id, v.get("id").unwrap_or(&Value::Null)))
        {
            return Some(self.incoming.remove(pos));
        }

        let start = Instant::now();
        while start.elapsed() < timeout {
            let remaining = timeout.saturating_sub(start.elapsed());
            match tokio::time::timeout(remaining, self.recv_frame()).await {
                Ok(Some(frame)) => {
                    if honk_upstream::rpc_id_matches(id, frame.get("id").unwrap_or(&Value::Null)) {
                        return Some(frame);
                    }
                    if frame
                        .pointer("/params/update/sessionUpdate")
                        .and_then(Value::as_str)
                        != Some("agent_message_chunk")
                    {
                        self.incoming.push(frame);
                    }
                }
                Ok(None) => break,
                Err(_) => break,
            }
        }
        None
    }

    pub async fn response_timeout(&mut self, id: &Value, timeout: Duration) -> Value {
        self.try_response_timeout(id, timeout)
            .await
            .unwrap_or_else(|| panic!("timed out waiting for response to id {id}"))
    }

    pub async fn call(&mut self, method: &str, params: Value) -> Value {
        let id = self.request(method, params).await;
        self.response(&id).await
    }

    pub async fn wait_for<F>(&mut self, pred: F) -> Value
    where
        F: Fn(&Value) -> bool,
    {
        if let Some(pos) = self.incoming.iter().position(&pred) {
            return self.incoming.remove(pos);
        }

        let start = Instant::now();
        while start.elapsed() < Duration::from_secs(30) {
            match tokio::time::timeout(Duration::from_secs(30), self.recv_frame()).await {
                Ok(Some(frame)) => {
                    if pred(&frame) {
                        return frame;
                    }
                    self.incoming.push(frame);
                }
                _ => break,
            }
        }
        panic!("timed out in wait_for");
    }

    pub async fn respond(&mut self, id: Value, result: Value) {
        let resp = json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": result,
        });
        self.ws
            .send(Message::Text(resp.to_string().into()))
            .await
            .expect("send ws message");
    }

    pub fn accumulated_text(&self) -> &str {
        &self.accumulated_agent_text
    }

    pub fn prompt_reply_text(&self) -> Option<&str> {
        self.prompt_reply_text.as_deref()
    }

    pub fn receipt_text(&self) -> String {
        if let Some(txt) = &self.prompt_reply_text {
            txt.clone()
        } else {
            self.accumulated_agent_text.clone()
        }
    }
}

pub fn prompt_params(sid: &str, text: &str, client_req_id: Option<&str>) -> Value {
    let mut p = json!({
        "sessionId": sid,
        "prompt": [{
            "type": "text",
            "text": text,
        }]
    });
    if let Some(crid) = client_req_id {
        p["_meta"] = json!({
            "honk": {
                "client_req_id": crid
            }
        });
    }
    p
}

pub async fn new_session(v: &mut Viewer, cwd: &str) -> String {
    let res = v
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
    res["result"]["sessionId"]
        .as_str()
        .expect("sessionId string")
        .to_string()
}
