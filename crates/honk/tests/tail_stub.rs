use honk::{Client, TailOpts};
use honk_testgoose::{StubGoose, Turn};
use serde_json::json;
use std::io::{BufRead, BufReader, Write};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tempfile::TempDir;

/// Finds the honkd executable:
/// 1. $HONKD_BIN environment variable (set by CI or runner)
/// 2. $CARGO_TARGET_DIR/debug/honkd if CARGO_TARGET_DIR is set
/// 3. target/debug/honkd (or ../../target/debug/honkd) if it exists
///
/// Cross-crate binary dev-dependencies are not directly supported by Cargo,
/// so we inspect HONKD_BIN and target/debug/honkd.
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

#[derive(Clone)]
struct SharedBuf(Arc<Mutex<Vec<u8>>>);

impl Write for SharedBuf {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn test_tail_stub_turn_equals_export() {
    let Some(g) = StubGoose::start_or_skip().await else {
        eprintln!("skipping test_tail_stub_turn_equals_export: StubGoose unset");
        return;
    };
    let Some(honkd_bin) = find_honkd_bin() else {
        eprintln!(
            "skipping test_tail_stub_turn_equals_export: honkd binary not found (set HONKD_BIN)"
        );
        return;
    };

    g.model().push([Turn::Text(vec![
        "hello ".into(),
        "from ".into(),
        "stub!".into(),
    ])]);

    let h = HonkdHarness::spawn(&honkd_bin, &g.url(), g.secret_file(), None).await;

    // Create session
    let mut client = Client::connect(&h.ws_url(), &h.secret())
        .await
        .expect("connect");
    let res = client
        .call(
            "session/new",
            json!({
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
    let sid = res["result"]["sessionId"]
        .as_str()
        .expect("sessionId string")
        .to_string();
    drop(client);

    let output = Arc::new(Mutex::new(Vec::new()));
    let opts = TailOpts {
        url: h.ws_url(),
        secret: h.secret(),
        session: sid.clone(),
        text_only: true,
        thoughts: false,
        exit_on_turn_end: true,
    };

    let tail_handle = tokio::spawn(honk::run(opts, SharedBuf(output.clone())));

    // Send prompt
    let mut prompt_client = Client::connect(&h.ws_url(), &h.secret())
        .await
        .expect("connect");
    prompt_client
        .call(
            "session/prompt",
            json!({
                "sessionId": sid,
                "prompt": [{ "type": "text", "text": "say hello" }]
            }),
        )
        .await
        .expect("prompt");
    drop(prompt_client);

    tail_handle
        .await
        .expect("join tail")
        .expect("tail run successfully");

    let export_text = g.turn_text(&sid).await;
    let tail_text = String::from_utf8(output.lock().unwrap().clone()).unwrap();

    assert_eq!(tail_text, export_text);
    assert_eq!(tail_text, "hello from stub!");
}

#[tokio::test(flavor = "multi_thread")]
async fn test_tail_reconnect_mid_turn_no_duplicates() {
    let Some(g) = StubGoose::start_or_skip().await else {
        eprintln!("skipping test_tail_reconnect_mid_turn_no_duplicates: StubGoose unset");
        return;
    };
    let Some(honkd_bin) = find_honkd_bin() else {
        eprintln!(
            "skipping test_tail_reconnect_mid_turn_no_duplicates: honkd binary not found (set HONKD_BIN)"
        );
        return;
    };

    g.model().push([Turn::Gate(
        "gate_mid_turn".into(),
        Box::new(Turn::Text(vec![
            "part one ".into(),
            "part two ".into(),
            "part three".into(),
        ])),
    )]);

    let mut h = HonkdHarness::spawn(&honkd_bin, &g.url(), g.secret_file(), None).await;

    // Create session
    let mut client = Client::connect(&h.ws_url(), &h.secret())
        .await
        .expect("connect");
    let res = client
        .call(
            "session/new",
            json!({
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
    let sid = res["result"]["sessionId"]
        .as_str()
        .expect("sessionId string")
        .to_string();
    drop(client);

    let output = Arc::new(Mutex::new(Vec::new()));
    let opts = TailOpts {
        url: h.ws_url(),
        secret: h.secret(),
        session: sid.clone(),
        text_only: true,
        thoughts: false,
        exit_on_turn_end: true,
    };

    let tail_handle = tokio::spawn(honk::run(opts, SharedBuf(output.clone())));

    // Send prompt asynchronously
    let mut prompt_client = Client::connect(&h.ws_url(), &h.secret())
        .await
        .expect("connect");
    prompt_client
        .request(
            "session/prompt",
            json!({
                "sessionId": sid,
                "prompt": [{ "type": "text", "text": "start multi-part" }]
            }),
        )
        .await
        .expect("prompt request");

    // Wait until active run is announced
    loop {
        if let Some(frame) = prompt_client.next_frame().await.expect("frame")
            && frame
                .pointer("/params/update/sessionUpdate")
                .and_then(|v| v.as_str())
                == Some("session_info_update")
            && frame
                .pointer("/params/update/_meta/goose/activeRunId")
                .and_then(|v| v.as_str())
                .is_some()
        {
            break;
        }
    }

    // Kill honkd mid-turn while goose is paused at gate
    h.kill9();

    // Release gate so turn finishes on goose while honkd is down
    g.model().open("gate_mid_turn");

    // Small delay to let goose finish streaming
    tokio::time::sleep(Duration::from_millis(150)).await;

    // Restart honkd
    h.restart().await;

    // Tail should reconnect, resume with delta, and exit when turn ends
    tail_handle
        .await
        .expect("join tail")
        .expect("tail run successfully");

    let export_text = g.turn_text(&sid).await;
    let tail_text = String::from_utf8(output.lock().unwrap().clone()).unwrap();

    assert_eq!(tail_text, export_text);
    assert_eq!(tail_text, "part one part two part three");
}

#[tokio::test(flavor = "multi_thread")]
async fn test_list_sessions_format() {
    let Some(g) = StubGoose::start_or_skip().await else {
        eprintln!("skipping test_list_sessions_format: StubGoose unset");
        return;
    };
    let Some(honkd_bin) = find_honkd_bin() else {
        eprintln!("skipping test_list_sessions_format: honkd binary not found (set HONKD_BIN)");
        return;
    };

    let h = HonkdHarness::spawn(&honkd_bin, &g.url(), g.secret_file(), None).await;

    let mut client = Client::connect(&h.ws_url(), &h.secret())
        .await
        .expect("connect");

    // Create session 1
    let res1 = client
        .call(
            "session/new",
            json!({
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
        .expect("session/new 1");
    let sid1 = res1["result"]["sessionId"]
        .as_str()
        .expect("sessionId string")
        .to_string();

    g.model().push([Turn::Text(vec!["ready".into()])]);

    client
        .call(
            "session/prompt",
            json!({
                "sessionId": sid1,
                "prompt": [{ "type": "text", "text": "init session" }]
            }),
        )
        .await
        .expect("prompt to persist session in goose");

    let sessions = client.list_sessions().await.expect("list_sessions");
    let entry1 = sessions
        .iter()
        .find(|s| s.session_id == sid1)
        .expect("session 1 found in list");

    assert!(entry1.held);
    assert!(!entry1.running);

    let formatted = entry1.format_line();
    assert!(formatted.starts_with(&format!("{sid1}  held  ")));
    assert!(formatted.ends_with(g.cwd().to_str().unwrap()));
}
