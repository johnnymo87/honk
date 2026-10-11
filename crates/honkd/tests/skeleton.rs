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
    std::fs::write(&dummy_secret, "test-secret").unwrap();

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
