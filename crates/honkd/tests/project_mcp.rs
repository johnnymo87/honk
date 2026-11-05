//! `--project-mcp`: the session cwd's opencode.json MCP servers on session/new.

use common::{Honkd, Viewer};
use honk_testgoose::FakeGoose;
use serde_json::{Value, json};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

mod common;

/// A local MCP stand-in: every POST gets `status` after `delay`.
/// Returns its url and a counter of the requests it has seen.
async fn stub_mcp(status: u16, delay: Duration) -> (String, Arc<AtomicUsize>) {
    let hits = Arc::new(AtomicUsize::new(0));
    let h = hits.clone();
    let router = axum::Router::new().route(
        "/mcp",
        axum::routing::post(move || {
            let h = h.clone();
            async move {
                h.fetch_add(1, Ordering::SeqCst);
                tokio::time::sleep(delay).await;
                axum::http::StatusCode::from_u16(status).unwrap()
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    (format!("http://{addr}/mcp"), hits)
}

/// A url on 127.0.0.1 where nothing listens.
async fn closed_port_url() -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    drop(listener);
    format!("http://{addr}/mcp")
}

/// A git project whose opencode.json declares `tec` at `url`.
fn project(url: &str) -> tempfile::TempDir {
    let td = tempfile::tempdir().unwrap();
    std::fs::create_dir(td.path().join(".git")).unwrap();
    std::fs::write(
        td.path().join("opencode.json"),
        json!({"mcp": {"tec": {"type": "remote", "url": url, "enabled": true}}}).to_string(),
    )
    .unwrap();
    td
}

async fn spawn_honkd(fake: &FakeGoose, extra: &[&str], env: &[(&str, &str)]) -> Honkd {
    let mut args = vec!["--allow-unpatched-upstream"];
    args.extend_from_slice(extra);
    Honkd::spawn_with_env(&fake.url(), fake.secret_file(), &args, env).await
}

async fn session_new(v: &mut Viewer, cwd: &Path, meta: Option<Value>) -> Value {
    let mut params = json!({"cwd": cwd.to_str().unwrap(), "mcpServers": []});
    if let Some(m) = meta {
        params["_meta"] = m;
    }
    let res = v.call("session/new", params).await;
    assert!(res.get("error").is_none(), "session/new failed: {res}");
    res["result"].clone()
}

fn tec_extension(fake: &FakeGoose, sid: &str) -> Option<Value> {
    fake.get_session_extensions(sid)
        .into_iter()
        .find(|e| e.get("extensionKey").and_then(Value::as_str) == Some("tec"))
}

#[tokio::test(flavor = "multi_thread")]
async fn project_server_is_enabled_on_session_new() {
    let fake = FakeGoose::start().await;
    let (url, hits) = stub_mcp(200, Duration::ZERO).await;
    let proj = project(&url);
    let h = spawn_honkd(&fake, &["--project-mcp"], &[]).await;
    let mut v = Viewer::connect(&h).await;

    let result = session_new(&mut v, proj.path(), None).await;
    let sid = result["sessionId"].as_str().unwrap();

    let meta = &result["_meta"]["honk"]["projectMcp"];
    assert_eq!(meta["servers"][0]["name"], "tec", "{result}");
    assert_eq!(meta["servers"][0]["ok"], true, "{result}");
    assert!(
        meta["servers"][0]["source"]
            .as_str()
            .unwrap()
            .starts_with("project:"),
        "{result}"
    );
    assert_eq!(meta["skipped"], json!([]));
    assert_eq!(meta["problems"], json!([]));
    assert!(hits.load(Ordering::SeqCst) >= 1, "the server was probed");

    let ext = tec_extension(&fake, sid).expect("goose got the tec extension");
    assert_eq!(ext["extension"]["server"]["type"], "http");
    assert_eq!(ext["extension"]["server"]["url"], url.as_str());
    assert_eq!(ext["extension"]["description"], "honk-mcp:tec");

    // The goose side never sees honk's meta.
    for f in fake.inbound() {
        if f.get("method").and_then(Value::as_str) == Some("session/new") {
            assert!(f.pointer("/params/_meta/honk").is_none(), "{f}");
        }
    }

    // /mcp lists it as on, trusted, and from the project.
    let list = v.call("_honk/mcp/list", json!({"sessionId": sid})).await["result"].clone();
    let tec = list["servers"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["name"] == "tec")
        .expect("tec listed");
    assert_eq!(tec["enabled"], true);
    assert_eq!(tec["confirm"], false);
    assert!(tec["source"].as_str().unwrap().starts_with("project:"));
}

#[tokio::test(flavor = "multi_thread")]
async fn session_can_opt_out() {
    let fake = FakeGoose::start().await;
    let (url, hits) = stub_mcp(200, Duration::ZERO).await;
    let proj = project(&url);
    let h = spawn_honkd(&fake, &["--project-mcp"], &[]).await;
    let mut v = Viewer::connect(&h).await;

    let result = session_new(
        &mut v,
        proj.path(),
        Some(json!({"honk": {"projectMcp": false}})),
    )
    .await;
    let sid = result["sessionId"].as_str().unwrap();

    assert!(
        result.pointer("/_meta/honk/projectMcp").is_none(),
        "{result}"
    );
    assert!(tec_extension(&fake, sid).is_none());
    assert_eq!(hits.load(Ordering::SeqCst), 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn nothing_happens_without_the_flag() {
    let fake = FakeGoose::start().await;
    let (url, hits) = stub_mcp(200, Duration::ZERO).await;
    let proj = project(&url);
    let h = spawn_honkd(&fake, &[], &[]).await;
    let mut v = Viewer::connect(&h).await;

    let result = session_new(&mut v, proj.path(), None).await;
    let sid = result["sessionId"].as_str().unwrap();

    assert!(result.get("_meta").is_none(), "{result}");
    assert!(tec_extension(&fake, sid).is_none());
    assert_eq!(hits.load(Ordering::SeqCst), 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_down_server_is_reported_and_session_new_still_succeeds() {
    let fake = FakeGoose::start().await;
    let url = closed_port_url().await;
    let proj = project(&url);
    let h = spawn_honkd(&fake, &["--project-mcp"], &[]).await;
    let mut v = Viewer::connect(&h).await;

    let result = session_new(&mut v, proj.path(), None).await;
    let sid = result["sessionId"].as_str().unwrap();

    let server = &result["_meta"]["honk"]["projectMcp"]["servers"][0];
    assert_eq!(server["name"], "tec", "{result}");
    assert_eq!(server["ok"], false, "{result}");
    assert!(
        server["error"].as_str().unwrap().contains("not reachable"),
        "{result}"
    );
    assert!(tec_extension(&fake, sid).is_none());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_server_asking_for_oauth_is_not_added() {
    let fake = FakeGoose::start().await;
    let (url, _) = stub_mcp(401, Duration::ZERO).await;
    let proj = project(&url);
    let h = spawn_honkd(&fake, &["--project-mcp"], &[]).await;
    let mut v = Viewer::connect(&h).await;

    let result = session_new(&mut v, proj.path(), None).await;
    let sid = result["sessionId"].as_str().unwrap();

    let server = &result["_meta"]["honk"]["projectMcp"]["servers"][0];
    assert_eq!(server["ok"], false, "{result}");
    assert!(
        server["error"].as_str().unwrap().contains("OAuth"),
        "{result}"
    );
    assert!(tec_extension(&fake, sid).is_none());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_slow_server_is_reported_pending() {
    let fake = FakeGoose::start().await;
    let (url, _) = stub_mcp(200, Duration::from_millis(1500)).await;
    let proj = project(&url);
    let h = spawn_honkd(
        &fake,
        &["--project-mcp"],
        &[("HONKD_TEST_PROJECT_MCP_LAUNCH_TIMEOUT_MS", "200")],
    )
    .await;
    let mut v = Viewer::connect(&h).await;

    let result = session_new(&mut v, proj.path(), None).await;
    let server = &result["_meta"]["honk"]["projectMcp"]["servers"][0];
    assert_eq!(server["pending"], true, "{result}");
    assert_eq!(server["ok"], false, "{result}");
    assert!(
        server["error"].as_str().unwrap().contains("run /mcp"),
        "{result}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_server_that_left_the_project_file_can_still_be_disabled() {
    let fake = FakeGoose::start().await;
    let (url, _) = stub_mcp(200, Duration::ZERO).await;
    let proj = project(&url);
    let h = spawn_honkd(&fake, &["--project-mcp"], &[]).await;
    let mut v = Viewer::connect(&h).await;

    let result = session_new(&mut v, proj.path(), None).await;
    let sid = result["sessionId"].as_str().unwrap();
    assert!(tec_extension(&fake, sid).is_some());

    std::fs::remove_file(proj.path().join("opencode.json")).unwrap();

    let res = v
        .call(
            "_honk/mcp/disable",
            json!({"sessionId": sid, "names": ["tec"]}),
        )
        .await;
    assert!(res.get("error").is_none(), "{res}");
    assert_eq!(res["result"]["results"][0]["name"], "tec");
    assert_eq!(res["result"]["results"][0]["ok"], true);
    assert!(tec_extension(&fake, sid).is_none());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_non_loopback_server_is_skipped_and_needs_a_person() {
    let fake = FakeGoose::start().await;
    let proj = project("https://example.com/mcp");
    let h = spawn_honkd(&fake, &["--project-mcp"], &[]).await;
    let mut v = Viewer::connect(&h).await;

    let result = session_new(&mut v, proj.path(), None).await;
    let sid = result["sessionId"].as_str().unwrap();

    let meta = &result["_meta"]["honk"]["projectMcp"];
    assert_eq!(meta["servers"], json!([]), "{result}");
    assert_eq!(meta["skipped"][0]["name"], "tec");
    assert_eq!(
        meta["skipped"][0]["reason"],
        "not a loopback url; enable with /mcp enable tec!"
    );
    assert!(tec_extension(&fake, sid).is_none());

    // Without the `!`, /mcp enable refuses and says how.
    let res = v
        .call(
            "_honk/mcp/enable",
            json!({"sessionId": sid, "names": ["tec"]}),
        )
        .await;
    assert_eq!(res["error"]["code"], -32602, "{res}");
    assert!(
        res["error"]["message"]
            .as_str()
            .unwrap()
            .contains("/mcp enable tec!"),
        "{res}"
    );
    assert!(tec_extension(&fake, sid).is_none());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_project_without_mcp_entries_adds_no_meta() {
    let fake = FakeGoose::start().await;
    let td = tempfile::tempdir().unwrap();
    std::fs::create_dir(td.path().join(".git")).unwrap();
    std::fs::write(td.path().join("opencode.json"), r#"{"theme":"dark"}"#).unwrap();
    let h = spawn_honkd(&fake, &["--project-mcp"], &[]).await;
    let mut v = Viewer::connect(&h).await;

    let result = session_new(&mut v, td.path(), None).await;
    assert!(result.get("_meta").is_none(), "{result}");
}

#[tokio::test(flavor = "multi_thread")]
async fn an_unusable_project_file_is_reported_as_a_problem() {
    let fake = FakeGoose::start().await;
    let td = tempfile::tempdir().unwrap();
    std::fs::create_dir(td.path().join(".git")).unwrap();
    std::fs::write(td.path().join("opencode.json"), "{ not json").unwrap();
    let h = spawn_honkd(&fake, &["--project-mcp"], &[]).await;
    let mut v = Viewer::connect(&h).await;

    let result = session_new(&mut v, td.path(), None).await;
    let meta = &result["_meta"]["honk"]["projectMcp"];
    assert_eq!(meta["servers"], json!([]), "{result}");
    assert_eq!(meta["problems"].as_array().unwrap().len(), 1, "{result}");
}

/// A git project declaring each `(name, url)` as a remote server.
fn project_of(entries: &[(&str, &str)]) -> tempfile::TempDir {
    let td = tempfile::tempdir().unwrap();
    std::fs::create_dir(td.path().join(".git")).unwrap();
    let mut mcp = serde_json::Map::new();
    for (name, url) in entries {
        mcp.insert(name.to_string(), json!({"type": "remote", "url": url}));
    }
    std::fs::write(
        td.path().join("opencode.json"),
        json!({ "mcp": mcp }).to_string(),
    )
    .unwrap();
    td
}

fn extension_keys(fake: &FakeGoose, sid: &str) -> Vec<String> {
    let mut keys: Vec<String> = fake
        .get_session_extensions(sid)
        .iter()
        .filter_map(|e| e.get("extensionKey").and_then(Value::as_str))
        .map(String::from)
        .collect();
    keys.sort();
    keys
}

fn skipped_reason(meta: &Value, name: &str) -> String {
    meta["skipped"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["name"] == name)
        .unwrap_or_else(|| panic!("{name} not skipped: {meta}"))["reason"]
        .as_str()
        .unwrap()
        .to_string()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_project_sibling_of_a_user_server_neither_shadows_nor_removes_it() {
    let fake = FakeGoose::start().await;
    let (url, _) = stub_mcp(200, Duration::ZERO).await;
    let proj = project_of(&[("slack-ro", &url), ("tec", &url)]);
    let cat_dir = tempfile::tempdir().unwrap();
    let cat = cat_dir.path().join("opencode.json");
    std::fs::write(
        &cat,
        r#"{"mcp":{"slack":{"type":"local","command":["slack-mcp"]}}}"#,
    )
    .unwrap();
    let h = spawn_honkd(
        &fake,
        &[
            "--project-mcp",
            "--mcp-catalog",
            cat.to_str().unwrap(),
            "--mcp-no-confirm",
            "slack",
        ],
        &[],
    )
    .await;
    let mut v = Viewer::connect(&h).await;

    // The user's slack is on before the project launch runs.
    let result = session_new(
        &mut v,
        proj.path(),
        Some(json!({"honk": {"mcp": {"names": ["slack"]}}})),
    )
    .await;
    let sid = result["sessionId"].as_str().unwrap();
    assert_eq!(
        result["_meta"]["honk"]["mcp"]["results"][0]["ok"], true,
        "{result}"
    );

    let meta = &result["_meta"]["honk"]["projectMcp"];
    assert_eq!(meta["servers"].as_array().unwrap().len(), 1, "{result}");
    assert_eq!(meta["servers"][0]["name"], "tec");
    assert_eq!(
        skipped_reason(meta, "slack-ro"),
        "pairs with user catalog entry slack"
    );
    assert_eq!(extension_keys(&fake, sid), vec!["slack", "tec"]);

    // Asking for the project's slack-ro by hand finds nothing of that name.
    let res = v
        .call(
            "_honk/mcp/enable",
            json!({"sessionId": sid, "names": ["slack-ro"]}),
        )
        .await;
    assert_eq!(res["error"]["code"], -32602, "{res}");
    assert_eq!(extension_keys(&fake, sid), vec!["slack", "tec"]);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_project_sibling_pair_enables_only_the_ro_half() {
    let fake = FakeGoose::start().await;
    let (url, _) = stub_mcp(200, Duration::ZERO).await;
    let proj = project_of(&[("foo", &url), ("foo-ro", &url), ("tec", &url)]);
    let h = spawn_honkd(&fake, &["--project-mcp"], &[]).await;
    let mut v = Viewer::connect(&h).await;

    let result = session_new(&mut v, proj.path(), None).await;
    let sid = result["sessionId"].as_str().unwrap();
    let meta = &result["_meta"]["honk"]["projectMcp"];

    let ok: Vec<_> = meta["servers"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| (s["name"].as_str().unwrap(), s["ok"].as_bool().unwrap()))
        .collect();
    assert_eq!(ok, vec![("foo-ro", true), ("tec", true)], "{result}");
    assert_eq!(
        skipped_reason(meta, "foo"),
        "sibling of foo-ro; enable it with /mcp enable foo!"
    );
    assert_eq!(extension_keys(&fake, sid), vec!["foo-ro", "tec"]);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_project_swarm_entry_is_never_taken() {
    let fake = FakeGoose::start().await;
    let (url, _) = stub_mcp(200, Duration::ZERO).await;
    let proj = project_of(&[("swarm", &url), ("tec", &url)]);
    let h = spawn_honkd(&fake, &["--project-mcp"], &[]).await;
    let mut v = Viewer::connect(&h).await;

    let result = session_new(&mut v, proj.path(), None).await;
    let sid = result["sessionId"].as_str().unwrap();
    let meta = &result["_meta"]["honk"]["projectMcp"];
    assert_eq!(skipped_reason(meta, "swarm"), "reserved name");
    assert_eq!(extension_keys(&fake, sid), vec!["tec"]);
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn a_config_symlinked_to_a_device_does_not_wedge_honkd() {
    let fake = FakeGoose::start().await;
    let td = tempfile::tempdir().unwrap();
    std::fs::create_dir(td.path().join(".git")).unwrap();
    std::os::unix::fs::symlink("/dev/zero", td.path().join("opencode.json")).unwrap();
    let h = spawn_honkd(&fake, &["--project-mcp"], &[]).await;
    let mut v = Viewer::connect(&h).await;

    let started = std::time::Instant::now();
    let result = session_new(&mut v, td.path(), None).await;
    assert!(started.elapsed() < Duration::from_secs(5));
    let meta = &result["_meta"]["honk"]["projectMcp"];
    assert_eq!(meta["servers"], json!([]), "{result}");
    assert_eq!(meta["problems"].as_array().unwrap().len(), 1, "{result}");

    // honkd still answers.
    let sid = result["sessionId"].as_str().unwrap();
    let list = v.call("_honk/mcp/list", json!({"sessionId": sid})).await;
    assert!(list.get("error").is_none(), "{list}");
}
