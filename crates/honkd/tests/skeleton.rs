use common::{Honkd, Viewer};
use honk_testgoose::{FakeGoose, StubGoose};

mod common;

#[tokio::test(flavor = "multi_thread")]
async fn auth_and_preflight_codes() {
    let Some(g) = StubGoose::start_or_skip().await else {
        return;
    };
    let h = Honkd::spawn(&g.url(), g.secret_file(), &[]).await;
    let c = reqwest::Client::new();
    let url = |q: String| format!("http://{}/acp{q}", h.addr);
    assert_eq!(
        c.get(url("?token=nope".into()))
            .send()
            .await
            .unwrap()
            .status(),
        401
    );
    // Empty token must be rejected with 401
    assert_eq!(
        c.get(url("?token=".into()))
            .send()
            .await
            .unwrap()
            .status(),
        401
    );
    assert_eq!(
        c.get(url("".into()))
            .header("x-secret-key", "")
            .send()
            .await
            .unwrap()
            .status(),
        401
    );
    assert_eq!(
        c.get(url(format!("?token={}", h.secret())))
            .send()
            .await
            .unwrap()
            .status(),
        406
    );
    let mut v = Viewer::connect_pigeon(&h).await;
    let r = v
        .call(
            "initialize",
            serde_json::json!({"protocolVersion":1,"clientCapabilities":{}}),
        )
        .await;
    assert_eq!(r["result"]["_meta"]["honk"]["protocol"], 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn refuses_unpatched_upstream_unless_allowed() {
    let fake = FakeGoose::start().await; // advertises no stream features
    let (code, stderr) = Honkd::spawn_expect_exit(&fake.url(), fake.secret_file(), &[]);
    assert_ne!(code, 0);
    assert!(stderr.contains("Acp-Stream-Features"));
    let _ok = Honkd::spawn(
        &fake.url(),
        fake.secret_file(),
        &["--allow-unpatched-upstream"],
    )
    .await;
}

#[test]
fn refuses_production_port_and_non_loopback_listen() {
    let dummy_dir = tempfile::tempdir().unwrap();
    let dummy_secret = dummy_dir.path().join("secret");
    std::fs::write(&dummy_secret, "0123456789abcdef0123456789abcdef").unwrap();

    let (code1, stderr1) =
        Honkd::spawn_expect_exit("http://127.0.0.1:4080/acp", &dummy_secret, &[]);
    assert_ne!(code1, 0);
    assert!(stderr1.contains("4080") || stderr1.contains("production"));

    let (code2, stderr2) = Honkd::spawn_expect_exit(
        "http://127.0.0.1:4181/acp",
        &dummy_secret,
        &["--listen", "0.0.0.0:0"],
    );
    assert_ne!(code2, 0);
    assert!(stderr2.contains("loopback"));
}

#[test]
fn refuses_short_secret_files() {
    let dummy_dir = tempfile::tempdir().unwrap();
    let short_secret = dummy_dir.path().join("short_secret");
    std::fs::write(&short_secret, "short").unwrap();

    // Short upstream secret file refused
    let (code1, stderr1) = Honkd::spawn_expect_exit(
        "http://127.0.0.1:4181/acp",
        &short_secret,
        &[],
    );
    assert_ne!(code1, 0);
    assert!(stderr1.contains("invalid (empty or shorter than 32 characters)"));

    // Short downstream secret file in state dir refused
    let valid_secret = dummy_dir.path().join("valid_secret");
    std::fs::write(&valid_secret, "0123456789abcdef0123456789abcdef").unwrap();

    let state_dir = tempfile::tempdir().unwrap();
    let downstream_secret = state_dir.path().join("secret");
    std::fs::write(&downstream_secret, "short").unwrap();

    let (code2, stderr2) = Honkd::spawn_expect_exit(
        "http://127.0.0.1:4181/acp",
        &valid_secret,
        &["--state-dir", state_dir.path().to_str().unwrap()],
    );
    assert_ne!(code2, 0);
    assert!(stderr2.contains("invalid (empty or shorter than 32 characters)"));
}

#[tokio::test(flavor = "multi_thread")]
async fn second_honkd_on_same_state_dir_refuses() {
    let fake = FakeGoose::start().await;
    let h = Honkd::spawn(
        &fake.url(),
        fake.secret_file(),
        &["--allow-unpatched-upstream"],
    )
    .await;
    let (code, stderr) = Honkd::spawn_expect_exit(
        &fake.url(),
        fake.secret_file(),
        &[
            "--allow-unpatched-upstream",
            "--state-dir",
            h.state_dir.path().to_str().unwrap(),
        ],
    );
    assert_ne!(code, 0);
    assert!(stderr.contains("another honkd owns"));
}

#[tokio::test(flavor = "multi_thread")]
async fn header_auth_and_honk_aware_connect() {
    let fake = FakeGoose::start().await;
    let h = Honkd::spawn(
        &fake.url(),
        fake.secret_file(),
        &["--allow-unpatched-upstream"],
    )
    .await;
    let c = reqwest::Client::new();
    let url = format!("http://{}/acp", h.addr);
    assert_eq!(
        c.get(&url)
            .header("x-secret-key", "wrong")
            .send()
            .await
            .unwrap()
            .status(),
        401
    );
    assert_eq!(
        c.get(&url)
            .header("x-secret-key", h.secret())
            .send()
            .await
            .unwrap()
            .status(),
        406
    );

    let mut v = Viewer::connect(&h).await;
    let r = v
        .call(
            "initialize",
            serde_json::json!({
                "protocolVersion": 1,
                "clientCapabilities": {},
                "_meta": { "honk": {} }
            }),
        )
        .await;
    assert_eq!(r["result"]["_meta"]["honk"]["protocol"], 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn viewer_overflow_closes_with_4001() {
    use futures::{SinkExt, StreamExt};

    let fake = FakeGoose::start().await;
    let h = Honkd::spawn(
        &fake.url(),
        fake.secret_file(),
        &["--allow-unpatched-upstream", "--viewer-queue", "1"],
    )
    .await;

    let url = format!("{}?token={}", h.ws_url(), h.secret());
    let (mut ws, _) = tokio_tungstenite::connect_async(url)
        .await
        .expect("connect");

    // Flood with initialize requests without reading responses
    for i in 1..=50 {
        let req = serde_json::json!({
            "jsonrpc": "2.0",
            "id": i,
            "method": "initialize",
            "params": {"protocolVersion": 1, "clientCapabilities": {}}
        });
        let _ = ws
            .send(tokio_tungstenite::tungstenite::protocol::Message::Text(
                req.to_string().into(),
            ))
            .await;
    }

    // Read frames until Close
    let mut close_code = None;
    while let Some(msg) = ws.next().await {
        match msg {
            Ok(tokio_tungstenite::tungstenite::protocol::Message::Close(frame)) => {
                close_code = frame.map(|f| u16::from(f.code));
                break;
            }
            Err(_) => break,
            _ => {}
        }
    }
    assert_eq!(close_code, Some(4001));
}
