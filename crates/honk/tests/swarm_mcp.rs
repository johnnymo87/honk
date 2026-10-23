mod support;

use axum::http::StatusCode;
use honk::swarm_mcp::{
    SwarmMcpServerConfig, honk_mcp_url, parse_enable_names, run_server, strip_disable_names,
};
use serde_json::{Value, json};
use support::honkd_stub::{HonkdBehaviour, StubHonkd};
use support::pigeon_stub::{Behaviour, StubPigeon};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, duplex};

async fn send_recv(
    client_tx: &mut (impl AsyncWriteExt + Unpin),
    client_rx: &mut (impl AsyncBufReadExt + Unpin),
    req: Value,
) -> Value {
    let line = format!("{}\n", serde_json::to_string(&req).unwrap());
    client_tx.write_all(line.as_bytes()).await.unwrap();
    client_tx.flush().await.unwrap();

    let mut line = String::new();
    client_rx.read_line(&mut line).await.unwrap();
    serde_json::from_str(&line).unwrap()
}

#[tokio::test]
async fn test_protocol_initialize_ping_and_unknown() {
    let (server_io, client_io) = duplex(4096);
    let (server_rx, server_tx) = tokio::io::split(server_io);
    let (client_rx, mut client_tx) = tokio::io::split(client_io);
    let mut client_rx = BufReader::new(client_rx);

    let config = SwarmMcpServerConfig {
        session: "20261020_owner".into(),
        pigeon_url: "http://127.0.0.1:4731".into(),
        pigeon_token_file: None,
        honk_url: "http://127.0.0.1:4280/acp".into(),
        honk_secret_file: None,
    };

    let server_handle = tokio::spawn(run_server(config, BufReader::new(server_rx), server_tx));

    // initialize
    let init_resp = send_recv(
        &mut client_tx,
        &mut client_rx,
        json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2024-11-05"
            }
        }),
    )
    .await;

    assert_eq!(init_resp["id"], 1);
    assert_eq!(init_resp["result"]["protocolVersion"], "2024-11-05");
    assert_eq!(init_resp["result"]["serverInfo"]["name"], "honk-swarm");
    assert!(init_resp["result"]["capabilities"]["tools"].is_object());

    // notification should produce no response
    client_tx
        .write_all(b"{\"jsonrpc\":\"2.0\",\"method\":\"notifications/initialized\"}\n")
        .await
        .unwrap();
    client_tx.flush().await.unwrap();

    // ping
    let ping_resp = send_recv(
        &mut client_tx,
        &mut client_rx,
        json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "ping"
        }),
    )
    .await;
    assert_eq!(ping_resp["id"], 2);
    assert_eq!(ping_resp["result"], json!({}));

    // unknown method -> -32601
    let unknown_resp = send_recv(
        &mut client_tx,
        &mut client_rx,
        json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "nonexistent"
        }),
    )
    .await;
    assert_eq!(unknown_resp["id"], 3);
    assert_eq!(unknown_resp["error"]["code"], -32601);

    drop(client_tx);
    drop(client_rx);
    let _ = server_handle.await;
}

#[tokio::test]
async fn test_tools_list() {
    let (server_io, client_io) = duplex(4096);
    let (server_rx, server_tx) = tokio::io::split(server_io);
    let (client_rx, mut client_tx) = tokio::io::split(client_io);
    let mut client_rx = BufReader::new(client_rx);

    let config = SwarmMcpServerConfig {
        session: "20261020_owner".into(),
        pigeon_url: "http://127.0.0.1:4731".into(),
        pigeon_token_file: None,
        honk_url: "http://127.0.0.1:4280/acp".into(),
        honk_secret_file: None,
    };

    let server_handle = tokio::spawn(run_server(config, BufReader::new(server_rx), server_tx));

    let resp = send_recv(
        &mut client_tx,
        &mut client_rx,
        json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/list"
        }),
    )
    .await;

    let tools = resp["result"]["tools"].as_array().expect("tools array");
    let names: Vec<&str> = tools.iter().filter_map(|t| t["name"].as_str()).collect();
    assert_eq!(
        names,
        vec![
            "swarm_send",
            "swarm_read",
            "swarm_list",
            "mcp_list",
            "mcp_enable",
            "mcp_disable",
        ]
    );

    // Descriptions should mention message economy rules
    let send_tool = tools.iter().find(|t| t["name"] == "swarm_send").unwrap();
    let desc = send_tool["description"].as_str().unwrap();
    assert!(
        desc.contains("/swarm_message"),
        "must mention </swarm_message> rule: {desc}"
    );

    let enable_tool = tools.iter().find(|t| t["name"] == "mcp_enable").unwrap();
    let enable_desc = enable_tool["description"].as_str().unwrap();
    assert!(
        enable_desc.contains("slack-ro"),
        "must mention slack-ro: {enable_desc}"
    );
    assert!(
        enable_desc.contains("reason"),
        "must mention reason: {enable_desc}"
    );

    drop(client_tx);
    drop(client_rx);
    let _ = server_handle.await;
}

#[tokio::test]
async fn test_subagent_mismatch_refused_without_http_call() {
    let p = StubPigeon::start(Behaviour::default()).await;
    let (server_io, client_io) = duplex(4096);
    let (server_rx, server_tx) = tokio::io::split(server_io);
    let (client_rx, mut client_tx) = tokio::io::split(client_io);
    let mut client_rx = BufReader::new(client_rx);

    let config = SwarmMcpServerConfig {
        session: "20261020_owner".into(),
        pigeon_url: p.url.clone(),
        pigeon_token_file: None,
        honk_url: "http://127.0.0.1:4280/acp".into(),
        honk_secret_file: None,
    };

    let server_handle = tokio::spawn(run_server(config, BufReader::new(server_rx), server_tx));

    // Call with _meta from subagent
    let resp = send_recv(
        &mut client_tx,
        &mut client_rx,
        json!({
            "jsonrpc": "2.0",
            "id": 10,
            "method": "tools/call",
            "params": {
                "name": "swarm_send",
                "arguments": {
                    "to": "ses_target",
                    "message": "hello"
                },
                "_meta": {
                    "agent-session-id": "20261020_subagent"
                }
            }
        }),
    )
    .await;

    assert_eq!(resp["id"], 10);
    assert_eq!(resp["result"]["isError"], true);
    let text = resp["result"]["content"][0]["text"].as_str().unwrap();
    assert!(
        text.contains(
            "swarm tools belong to goose session 20261020_owner; subagents cannot use them"
        ),
        "unexpected error text: {text}"
    );

    // Verify stub saw ZERO HTTP calls
    assert!(
        p.seen().is_empty(),
        "must not make any HTTP calls when subagent mismatches"
    );

    drop(client_tx);
    drop(client_rx);
    let _ = server_handle.await;
}

#[tokio::test]
async fn test_swarm_send_banked_and_pending_count() {
    let p = StubPigeon::start(Behaviour {
        swarm_mailbox: (
            StatusCode::OK,
            json!({"ok": true, "session_id": "gse_sender1", "backend_session_id": "20261020_owner", "created": false, "notify": false}),
        ),
        swarm_send: (
            StatusCode::ACCEPTED,
            json!({"accepted": true, "msg_id": "msg_abc123", "banked": true}),
        ),
        pull_pending: (
            StatusCode::OK,
            json!({"ok": true, "session_id": "gse_sender1", "session_known": true, "backend_kind": "goose-acp", "pending": 2, "by_source": {"swarm": 2}}),
        ),
        ..Behaviour::default()
    })
    .await;

    let (server_io, client_io) = duplex(4096);
    let (server_rx, server_tx) = tokio::io::split(server_io);
    let (client_rx, mut client_tx) = tokio::io::split(client_io);
    let mut client_rx = BufReader::new(client_rx);

    let temp_secret = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(temp_secret.path(), "honkd-test-secret\n").unwrap();

    let temp_token = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(temp_token.path(), "pigeon-bearer-token\n").unwrap();

    let config = SwarmMcpServerConfig {
        session: "20261020_owner".into(),
        pigeon_url: p.url.clone(),
        pigeon_token_file: Some(temp_token.path().to_path_buf()),
        honk_url: "http://127.0.0.1:4280/acp".into(),
        honk_secret_file: Some(temp_secret.path().to_path_buf()),
    };

    let server_handle = tokio::spawn(run_server(config, BufReader::new(server_rx), server_tx));

    let resp = send_recv(
        &mut client_tx,
        &mut client_rx,
        json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": {
                "name": "swarm_send",
                "arguments": {
                    "to": "gse_recipient",
                    "message": "hello peer",
                    "kind": "task.assign",
                    "priority": "urgent"
                },
                "_meta": {
                    "agent-session-id": "20261020_owner",
                    "agent-working-dir": "/test/working/dir"
                }
            }
        }),
    )
    .await;

    assert_eq!(resp["id"], 1);
    assert_ne!(resp["result"]["isError"], true);
    let text = resp["result"]["content"][0]["text"].as_str().unwrap();
    assert!(
        text.contains("Queued msg_abc123 -> gse_recipient"),
        "expected queued message: {text}"
    );
    assert!(
        text.contains("(waiting in its mailbox)"),
        "expected banked suffix: {text}"
    );
    assert!(
        text.contains("You have 2 unread swarm messages — call swarm_read."),
        "expected pending notice: {text}"
    );

    let seen = p.seen();
    assert_eq!(
        seen.len(),
        3,
        "expected 3 requests: mailbox, send, pending: {seen:?}"
    );

    // 1. /goose/swarm-mailbox
    assert_eq!(seen[0].path, "/goose/swarm-mailbox");
    assert_eq!(seen[0].auth.as_deref(), Some("Bearer pigeon-bearer-token"));
    assert_eq!(seen[0].body["session_id"], "20261020_owner");
    assert_eq!(seen[0].body["label"], "20261020_owner");
    assert_eq!(seen[0].body["cwd"], "/test/working/dir");
    assert_eq!(seen[0].body["endpoint"], "ws://127.0.0.1:4280/acp");
    assert_eq!(seen[0].body["auth_token"], "honkd-test-secret");

    // 2. /swarm/send
    assert_eq!(seen[1].path, "/swarm/send");
    assert_eq!(seen[1].auth.as_deref(), Some("Bearer pigeon-bearer-token"));
    assert_eq!(seen[1].body["from"], "gse_sender1");
    assert_eq!(seen[1].body["to"], "gse_recipient");
    assert_eq!(seen[1].body["payload"], "hello peer");
    assert_eq!(seen[1].body["kind"], "task.assign");
    assert_eq!(seen[1].body["priority"], "urgent");

    // 3. /pull/pending
    assert_eq!(seen[2].path, "/pull/pending");
    assert_eq!(seen[2].body["session"], "gse_sender1");

    drop(client_tx);
    drop(client_rx);
    let _ = server_handle.await;
}

#[tokio::test]
async fn test_swarm_send_surfaces_pigeon_409() {
    let p = StubPigeon::start(Behaviour {
        swarm_mailbox: (
            StatusCode::OK,
            json!({"ok": true, "session_id": "gse_sender1", "backend_session_id": "20261020_owner"}),
        ),
        swarm_send: (
            StatusCode::CONFLICT,
            json!({"error": "goose-acp session gse_target has no swarm mailbox"}),
        ),
        ..Behaviour::default()
    })
    .await;

    let (server_io, client_io) = duplex(4096);
    let (server_rx, server_tx) = tokio::io::split(server_io);
    let (client_rx, mut client_tx) = tokio::io::split(client_io);
    let mut client_rx = BufReader::new(client_rx);

    let config = SwarmMcpServerConfig {
        session: "20261020_owner".into(),
        pigeon_url: p.url.clone(),
        pigeon_token_file: None,
        honk_url: "http://127.0.0.1:4280/acp".into(),
        honk_secret_file: None,
    };

    let server_handle = tokio::spawn(run_server(config, BufReader::new(server_rx), server_tx));

    let resp = send_recv(
        &mut client_tx,
        &mut client_rx,
        json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": {
                "name": "swarm_send",
                "arguments": {
                    "to": "gse_target",
                    "message": "hello"
                },
                "_meta": {
                    "agent-session-id": "20261020_owner"
                }
            }
        }),
    )
    .await;

    assert_eq!(resp["result"]["isError"], true);
    let text = resp["result"]["content"][0]["text"].as_str().unwrap();
    assert!(
        text.contains("goose-acp session gse_target has no swarm mailbox"),
        "expected 409 error text: {text}"
    );

    drop(client_tx);
    drop(client_rx);
    let _ = server_handle.await;
}

#[test]
fn test_honk_mcp_url_replacement() {
    assert_eq!(
        honk_mcp_url("http://127.0.0.1:4280/acp", "list")
            .unwrap()
            .as_str(),
        "http://127.0.0.1:4280/mcp/list"
    );
    assert_eq!(
        honk_mcp_url("ws://127.0.0.1:4280/acp", "enable")
            .unwrap()
            .as_str(),
        "http://127.0.0.1:4280/mcp/enable"
    );
    assert_eq!(
        honk_mcp_url("wss://example.com/acp?token=foo#frag", "disable")
            .unwrap()
            .as_str(),
        "https://example.com/mcp/disable"
    );
    assert_eq!(
        honk_mcp_url("http://127.0.0.1:4280/some/deep/path", "list")
            .unwrap()
            .as_str(),
        "http://127.0.0.1:4280/mcp/list"
    );
    assert!(honk_mcp_url("ftp://example.com/acp", "list").is_err());
    assert!(honk_mcp_url("not a url", "list").is_err());
}

#[test]
fn test_mcp_name_parsing_helpers() {
    let (names, confirm) = parse_enable_names(["slack"]).unwrap();
    assert_eq!(names, vec!["slack"]);
    assert_eq!(confirm, Vec::<String>::new());

    let (names, confirm) = parse_enable_names(["slack!"]).unwrap();
    assert_eq!(names, vec!["slack"]);
    assert_eq!(confirm, vec!["slack"]);

    let (names, confirm) = parse_enable_names(["slack-ro", "slack!", "atlassian!"]).unwrap();
    assert_eq!(names, vec!["slack-ro", "slack", "atlassian"]);
    assert_eq!(confirm, vec!["slack", "atlassian"]);

    assert!(parse_enable_names(["!"]).is_err());
    assert!(parse_enable_names([""]).is_err());

    let disabled = strip_disable_names(["slack!", "pagerduty"]).unwrap();
    assert_eq!(disabled, vec!["slack", "pagerduty"]);
    assert!(strip_disable_names(["!"]).is_err());
}

#[tokio::test]
async fn test_mcp_list_success_and_auth_header() {
    let h = StubHonkd::start(HonkdBehaviour::default()).await;
    let secret_file = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(secret_file.path(), "my-secret-key-123\n").unwrap();

    let (server_io, client_io) = duplex(4096);
    let (server_rx, server_tx) = tokio::io::split(server_io);
    let (client_rx, mut client_tx) = tokio::io::split(client_io);
    let mut client_rx = BufReader::new(client_rx);

    let config = SwarmMcpServerConfig {
        session: "20261023_owner".into(),
        pigeon_url: "http://127.0.0.1:4731".into(),
        pigeon_token_file: None,
        honk_url: h.url.clone(),
        honk_secret_file: Some(secret_file.path().to_path_buf()),
    };

    let server_handle = tokio::spawn(run_server(config, BufReader::new(server_rx), server_tx));

    let resp = send_recv(
        &mut client_tx,
        &mut client_rx,
        json!({
            "jsonrpc": "2.0",
            "id": 100,
            "method": "tools/call",
            "params": {
                "name": "mcp_list",
                "arguments": {},
                "_meta": {
                    "agent-session-id": "20261023_owner"
                }
            }
        }),
    )
    .await;

    assert_eq!(resp["result"]["isError"], Value::Null);
    let text = resp["result"]["content"][0]["text"].as_str().unwrap();
    assert!(text.contains("off"), "must show state: {text}");
    assert!(text.contains("slack"), "must show slack: {text}");
    assert!(text.contains("slack-ro"), "must show slack-ro: {text}");
    assert!(text.contains("!"), "must show confirm marker: {text}");
    assert!(
        text.contains("ro sibling: slack-ro") || text.contains("slack-ro"),
        "must show sibling: {text}"
    );
    assert!(text.contains("developer"), "must show other key: {text}");

    // Verify StubHonkd saw request with x-secret-key header and sessionId
    {
        let seen = h.seen.lock().unwrap();
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].path, "/mcp/list");
        assert_eq!(seen[0].secret.as_deref(), Some("my-secret-key-123"));
        assert_eq!(seen[0].body["sessionId"], "20261023_owner");
    }

    drop(client_tx);
    drop(client_rx);
    let _ = server_handle.await;
}

#[tokio::test]
async fn test_mcp_enable_names_confirm_and_reason_passthrough() {
    let h = StubHonkd::start(HonkdBehaviour::default()).await;
    let secret_file = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(secret_file.path(), "test-secret\n").unwrap();

    let (server_io, client_io) = duplex(4096);
    let (server_rx, server_tx) = tokio::io::split(server_io);
    let (client_rx, mut client_tx) = tokio::io::split(client_io);
    let mut client_rx = BufReader::new(client_rx);

    let config = SwarmMcpServerConfig {
        session: "20261023_owner".into(),
        pigeon_url: "http://127.0.0.1:4731".into(),
        pigeon_token_file: None,
        honk_url: h.url.clone(),
        honk_secret_file: Some(secret_file.path().to_path_buf()),
    };

    let server_handle = tokio::spawn(run_server(config, BufReader::new(server_rx), server_tx));

    // Even if tool arguments pass a different sessionId, config.session must be used
    let resp = send_recv(
        &mut client_tx,
        &mut client_rx,
        json!({
            "jsonrpc": "2.0",
            "id": 101,
            "method": "tools/call",
            "params": {
                "name": "mcp_enable",
                "arguments": {
                    "sessionId": "attacker_session",
                    "names": ["slack!", "slack-ro"],
                    "reason": "investigate alert thread"
                },
                "_meta": {
                    "agent-session-id": "20261023_owner"
                }
            }
        }),
    )
    .await;

    assert_eq!(resp["result"]["isError"], Value::Null);
    let text = resp["result"]["content"][0]["text"].as_str().unwrap();
    assert!(
        text.contains("enabled slack, disabled slack-ro"),
        "must include summary: {text}"
    );
    assert!(
        text.contains("New tools are usable from your next step."),
        "must include next step advice: {text}"
    );
    assert!(
        text.contains("Reason: investigate alert thread"),
        "must echo reason: {text}"
    );

    {
        let seen = h.seen.lock().unwrap();
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].path, "/mcp/enable");
        // Target is ALWAYS config.session
        assert_eq!(seen[0].body["sessionId"], "20261023_owner");
        assert_eq!(seen[0].body["names"], json!(["slack", "slack-ro"]));
        assert_eq!(seen[0].body["confirm"], json!(["slack"]));
        assert_eq!(seen[0].body["reason"], "investigate alert thread");
    }

    drop(client_tx);
    drop(client_rx);
    let _ = server_handle.await;
}

#[tokio::test]
async fn test_mcp_disable_strips_bang_and_success_output() {
    let h = StubHonkd::start(HonkdBehaviour::default()).await;
    let secret_file = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(secret_file.path(), "test-secret\n").unwrap();

    let (server_io, client_io) = duplex(4096);
    let (server_rx, server_tx) = tokio::io::split(server_io);
    let (client_rx, mut client_tx) = tokio::io::split(client_io);
    let mut client_rx = BufReader::new(client_rx);

    let config = SwarmMcpServerConfig {
        session: "20261023_owner".into(),
        pigeon_url: "http://127.0.0.1:4731".into(),
        pigeon_token_file: None,
        honk_url: h.url.clone(),
        honk_secret_file: Some(secret_file.path().to_path_buf()),
    };

    let server_handle = tokio::spawn(run_server(config, BufReader::new(server_rx), server_tx));

    let resp = send_recv(
        &mut client_tx,
        &mut client_rx,
        json!({
            "jsonrpc": "2.0",
            "id": 102,
            "method": "tools/call",
            "params": {
                "name": "mcp_disable",
                "arguments": {
                    "names": ["slack!"]
                },
                "_meta": {
                    "agent-session-id": "20261023_owner"
                }
            }
        }),
    )
    .await;

    assert_eq!(resp["result"]["isError"], Value::Null);
    let text = resp["result"]["content"][0]["text"].as_str().unwrap();
    assert!(
        text.contains("disabled slack"),
        "must include summary: {text}"
    );
    assert!(text.contains("New tools are usable from your next step."));

    {
        let seen = h.seen.lock().unwrap();
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].path, "/mcp/disable");
        assert_eq!(seen[0].body["sessionId"], "20261023_owner");
        assert_eq!(seen[0].body["names"], json!(["slack"]));
    }

    drop(client_tx);
    drop(client_rx);
    let _ = server_handle.await;
}

#[tokio::test]
async fn test_mcp_disable_swarm_refused_without_http() {
    let h = StubHonkd::start(HonkdBehaviour::default()).await;
    let secret_file = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(secret_file.path(), "test-secret\n").unwrap();

    let (server_io, client_io) = duplex(4096);
    let (server_rx, server_tx) = tokio::io::split(server_io);
    let (client_rx, mut client_tx) = tokio::io::split(client_io);
    let mut client_rx = BufReader::new(client_rx);

    let config = SwarmMcpServerConfig {
        session: "20261023_owner".into(),
        pigeon_url: "http://127.0.0.1:4731".into(),
        pigeon_token_file: None,
        honk_url: h.url.clone(),
        honk_secret_file: Some(secret_file.path().to_path_buf()),
    };

    let server_handle = tokio::spawn(run_server(config, BufReader::new(server_rx), server_tx));

    // Try disabling "swarm!"
    let resp = send_recv(
        &mut client_tx,
        &mut client_rx,
        json!({
            "jsonrpc": "2.0",
            "id": 103,
            "method": "tools/call",
            "params": {
                "name": "mcp_disable",
                "arguments": {
                    "names": ["swarm!"]
                },
                "_meta": {
                    "agent-session-id": "20261023_owner"
                }
            }
        }),
    )
    .await;

    assert_eq!(resp["result"]["isError"], true);
    let text = resp["result"]["content"][0]["text"].as_str().unwrap();
    assert_eq!(
        text,
        "refused: disabling swarm would remove these tools; ask the human to /mcp disable swarm"
    );

    // ZERO HTTP calls made to honkd
    assert_eq!(h.seen.lock().unwrap().len(), 0);

    drop(client_tx);
    drop(client_rx);
    let _ = server_handle.await;
}

#[tokio::test]
async fn test_mcp_enable_swarm_ro_refused_without_http() {
    let h = StubHonkd::start(HonkdBehaviour::default()).await;
    let secret_file = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(secret_file.path(), "test-secret\n").unwrap();

    let (server_io, client_io) = duplex(4096);
    let (server_rx, server_tx) = tokio::io::split(server_io);
    let (client_rx, mut client_tx) = tokio::io::split(client_io);
    let mut client_rx = BufReader::new(client_rx);

    let config = SwarmMcpServerConfig {
        session: "20261023_owner".into(),
        pigeon_url: "http://127.0.0.1:4731".into(),
        pigeon_token_file: None,
        honk_url: h.url.clone(),
        honk_secret_file: Some(secret_file.path().to_path_buf()),
    };

    let server_handle = tokio::spawn(run_server(config, BufReader::new(server_rx), server_tx));

    // Try enabling "swarm-ro!"
    let resp = send_recv(
        &mut client_tx,
        &mut client_rx,
        json!({
            "jsonrpc": "2.0",
            "id": 109,
            "method": "tools/call",
            "params": {
                "name": "mcp_enable",
                "arguments": {
                    "names": ["swarm-ro!"]
                },
                "_meta": {
                    "agent-session-id": "20261023_owner"
                }
            }
        }),
    )
    .await;

    assert_eq!(resp["result"]["isError"], true);
    let text = resp["result"]["content"][0]["text"].as_str().unwrap();
    assert_eq!(
        text,
        "refused: 'swarm-ro' would replace swarm and remove these tools"
    );

    // ZERO HTTP calls made to honkd
    assert_eq!(h.seen.lock().unwrap().len(), 0);

    drop(client_tx);
    drop(client_rx);
    let _ = server_handle.await;
}

#[tokio::test]
async fn test_mcp_errors_404_401_error_body_transport_and_secret_file() {
    // 1. 404
    let h_404 = StubHonkd::start(HonkdBehaviour {
        list: (StatusCode::NOT_FOUND, json!({"error": "not found"})),
        ..HonkdBehaviour::default()
    })
    .await;
    let secret_file = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(secret_file.path(), "secret\n").unwrap();

    let (server_io, client_io) = duplex(4096);
    let (server_rx, server_tx) = tokio::io::split(server_io);
    let (client_rx, mut client_tx) = tokio::io::split(client_io);
    let mut client_rx = BufReader::new(client_rx);

    let config = SwarmMcpServerConfig {
        session: "20261023_owner".into(),
        pigeon_url: "http://127.0.0.1:4731".into(),
        pigeon_token_file: None,
        honk_url: h_404.url.clone(),
        honk_secret_file: Some(secret_file.path().to_path_buf()),
    };

    let server_handle = tokio::spawn(run_server(config, BufReader::new(server_rx), server_tx));

    let resp = send_recv(
        &mut client_tx,
        &mut client_rx,
        json!({
            "jsonrpc": "2.0",
            "id": 104,
            "method": "tools/call",
            "params": {
                "name": "mcp_list",
                "arguments": {},
                "_meta": {
                    "agent-session-id": "20261023_owner"
                }
            }
        }),
    )
    .await;

    assert_eq!(resp["result"]["isError"], true);
    assert_eq!(
        resp["result"]["content"][0]["text"].as_str().unwrap(),
        "this honkd has no self-grant (too old, or --mcp-self-grant is off); ask the human for /mcp in honk chat"
    );

    drop(client_tx);
    drop(client_rx);
    let _ = server_handle.await;

    // 2. 401
    let h_401 = StubHonkd::start(HonkdBehaviour {
        list: (StatusCode::UNAUTHORIZED, json!({"error": "unauthorized"})),
        ..HonkdBehaviour::default()
    })
    .await;

    let (server_io, client_io) = duplex(4096);
    let (server_rx, server_tx) = tokio::io::split(server_io);
    let (client_rx, mut client_tx) = tokio::io::split(client_io);
    let mut client_rx = BufReader::new(client_rx);

    let config_401 = SwarmMcpServerConfig {
        session: "20261023_owner".into(),
        pigeon_url: "http://127.0.0.1:4731".into(),
        pigeon_token_file: None,
        honk_url: h_401.url.clone(),
        honk_secret_file: Some(secret_file.path().to_path_buf()),
    };

    let server_handle = tokio::spawn(run_server(config_401, BufReader::new(server_rx), server_tx));

    let resp = send_recv(
        &mut client_tx,
        &mut client_rx,
        json!({
            "jsonrpc": "2.0",
            "id": 105,
            "method": "tools/call",
            "params": {
                "name": "mcp_list",
                "arguments": {},
                "_meta": {
                    "agent-session-id": "20261023_owner"
                }
            }
        }),
    )
    .await;

    assert_eq!(resp["result"]["isError"], true);
    let text = resp["result"]["content"][0]["text"].as_str().unwrap();
    assert!(
        text.to_lowercase().contains("auth"),
        "401 must mention auth: {text}"
    );

    drop(client_tx);
    drop(client_rx);
    let _ = server_handle.await;

    // 3. Error body { error: { message } } verbatim
    let h_err_body = StubHonkd::start(HonkdBehaviour {
        enable: (
            StatusCode::OK,
            json!({
                "error": {
                    "code": -32602,
                    "message": "'slack' can write; give a reason saying what you will do with it"
                }
            }),
        ),
        ..HonkdBehaviour::default()
    })
    .await;

    let (server_io, client_io) = duplex(4096);
    let (server_rx, server_tx) = tokio::io::split(server_io);
    let (client_rx, mut client_tx) = tokio::io::split(client_io);
    let mut client_rx = BufReader::new(client_rx);

    let config_err = SwarmMcpServerConfig {
        session: "20261023_owner".into(),
        pigeon_url: "http://127.0.0.1:4731".into(),
        pigeon_token_file: None,
        honk_url: h_err_body.url.clone(),
        honk_secret_file: Some(secret_file.path().to_path_buf()),
    };

    let server_handle = tokio::spawn(run_server(config_err, BufReader::new(server_rx), server_tx));

    let resp = send_recv(
        &mut client_tx,
        &mut client_rx,
        json!({
            "jsonrpc": "2.0",
            "id": 106,
            "method": "tools/call",
            "params": {
                "name": "mcp_enable",
                "arguments": {
                    "names": ["slack!"]
                },
                "_meta": {
                    "agent-session-id": "20261023_owner"
                }
            }
        }),
    )
    .await;

    assert_eq!(resp["result"]["isError"], true);
    assert_eq!(
        resp["result"]["content"][0]["text"].as_str().unwrap(),
        "'slack' can write; give a reason saying what you will do with it"
    );

    drop(client_tx);
    drop(client_rx);
    let _ = server_handle.await;

    // 4. Missing secret file
    let (server_io, client_io) = duplex(4096);
    let (server_rx, server_tx) = tokio::io::split(server_io);
    let (client_rx, mut client_tx) = tokio::io::split(client_io);
    let mut client_rx = BufReader::new(client_rx);

    let config_no_secret = SwarmMcpServerConfig {
        session: "20261023_owner".into(),
        pigeon_url: "http://127.0.0.1:4731".into(),
        pigeon_token_file: None,
        honk_url: h_err_body.url.clone(),
        honk_secret_file: None,
    };

    let server_handle = tokio::spawn(run_server(
        config_no_secret,
        BufReader::new(server_rx),
        server_tx,
    ));

    let resp = send_recv(
        &mut client_tx,
        &mut client_rx,
        json!({
            "jsonrpc": "2.0",
            "id": 107,
            "method": "tools/call",
            "params": {
                "name": "mcp_list",
                "arguments": {},
                "_meta": {
                    "agent-session-id": "20261023_owner"
                }
            }
        }),
    )
    .await;

    assert_eq!(resp["result"]["isError"], true);
    let text = resp["result"]["content"][0]["text"].as_str().unwrap();
    assert!(
        text.contains("secret"),
        "missing secret file must report error: {text}"
    );

    drop(client_tx);
    drop(client_rx);
    let _ = server_handle.await;

    // 5. Transport error
    let (server_io, client_io) = duplex(4096);
    let (server_rx, server_tx) = tokio::io::split(server_io);
    let (client_rx, mut client_tx) = tokio::io::split(client_io);
    let mut client_rx = BufReader::new(client_rx);

    let config_transport_err = SwarmMcpServerConfig {
        session: "20261023_owner".into(),
        pigeon_url: "http://127.0.0.1:4731".into(),
        pigeon_token_file: None,
        honk_url: "http://127.0.0.1:1".into(), // guaranteed port unreachable
        honk_secret_file: Some(secret_file.path().to_path_buf()),
    };

    let server_handle = tokio::spawn(run_server(
        config_transport_err,
        BufReader::new(server_rx),
        server_tx,
    ));

    let resp = send_recv(
        &mut client_tx,
        &mut client_rx,
        json!({
            "jsonrpc": "2.0",
            "id": 108,
            "method": "tools/call",
            "params": {
                "name": "mcp_list",
                "arguments": {},
                "_meta": {
                    "agent-session-id": "20261023_owner"
                }
            }
        }),
    )
    .await;

    assert_eq!(resp["result"]["isError"], true);
    let text = resp["result"]["content"][0]["text"].as_str().unwrap();
    assert!(
        !text.is_empty(),
        "transport error should return non-empty error text"
    );

    drop(client_tx);
    drop(client_rx);
    let _ = server_handle.await;
}

#[tokio::test]
async fn test_mcp_caller_identity_refusal() {
    let (server_io, client_io) = duplex(4096);
    let (server_rx, server_tx) = tokio::io::split(server_io);
    let (client_rx, mut client_tx) = tokio::io::split(client_io);
    let mut client_rx = BufReader::new(client_rx);

    let config = SwarmMcpServerConfig {
        session: "20261023_owner".into(),
        pigeon_url: "http://127.0.0.1:4731".into(),
        pigeon_token_file: None,
        honk_url: "http://127.0.0.1:4280/acp".into(),
        honk_secret_file: None,
    };

    let server_handle = tokio::spawn(run_server(config, BufReader::new(server_rx), server_tx));

    for tool_name in ["mcp_list", "mcp_enable", "mcp_disable"] {
        let resp = send_recv(
            &mut client_tx,
            &mut client_rx,
            json!({
                "jsonrpc": "2.0",
                "id": 200,
                "method": "tools/call",
                "params": {
                    "name": tool_name,
                    "arguments": { "names": ["slack"] },
                    "_meta": {
                        "agent-session-id": "foreign_subagent"
                    }
                }
            }),
        )
        .await;

        assert_eq!(resp["result"]["isError"], true);
        let text = resp["result"]["content"][0]["text"].as_str().unwrap();
        assert!(
            text.contains("subagents cannot use them"),
            "{tool_name} should refuse foreign caller: {text}"
        );
    }

    drop(client_tx);
    drop(client_rx);
    let _ = server_handle.await;
}

#[tokio::test]
async fn test_ensure_mailbox_rejects_non_gse_session_id() {
    let p = StubPigeon::start(Behaviour {
        swarm_mailbox: (
            StatusCode::OK,
            json!({"ok": true, "session_id": "legacy_session_id", "backend_session_id": "20261020_owner"}),
        ),
        ..Behaviour::default()
    })
    .await;

    let (server_io, client_io) = duplex(4096);
    let (server_rx, server_tx) = tokio::io::split(server_io);
    let (client_rx, mut client_tx) = tokio::io::split(client_io);
    let mut client_rx = BufReader::new(client_rx);

    let config = SwarmMcpServerConfig {
        session: "20261020_owner".into(),
        pigeon_url: p.url.clone(),
        pigeon_token_file: None,
        honk_url: "http://127.0.0.1:4280/acp".into(),
        honk_secret_file: None,
    };

    let server_handle = tokio::spawn(run_server(config, BufReader::new(server_rx), server_tx));

    let resp = send_recv(
        &mut client_tx,
        &mut client_rx,
        json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": {
                "name": "swarm_send",
                "arguments": {
                    "to": "gse_target",
                    "message": "hello"
                },
                "_meta": {
                    "agent-session-id": "20261020_owner"
                }
            }
        }),
    )
    .await;

    assert_eq!(resp["result"]["isError"], true);
    let text = resp["result"]["content"][0]["text"].as_str().unwrap();
    assert!(
        text.contains("honk telegram 20261020_owner --force"),
        "expected re-register instructions in error text: {text}"
    );

    drop(client_tx);
    drop(client_rx);
    let _ = server_handle.await;
}

#[tokio::test]
async fn test_swarm_read_drains_acks_and_formats_envelope() {
    let p = StubPigeon::start(Behaviour {
        swarm_mailbox: (
            StatusCode::OK,
            json!({"ok": true, "session_id": "gse_reader", "backend_session_id": "20261020_owner"}),
        ),
        pull_drain: (
            StatusCode::OK,
            json!({
                "ok": true,
                "session_id": "gse_reader",
                "pending_total": 3,
                "messages": [
                    {
                        "msg_id": "msg_msg1",
                        "source": "swarm",
                        "sender_id": "ses_alice",
                        "kind": "chat",
                        "payload": "ping from alice",
                        "reply_to": "msg_orig",
                        "created_at": 1700000000000i64,
                    },
                    {
                        "msg_id": "msg_msg2",
                        "source": "swarm",
                        "sender_id": "ses_bob",
                        "kind": "task.assign",
                        "payload": "urgent task",
                        "priority": "urgent",
                        "created_at": 1700000000001i64,
                    }
                ]
            }),
        ),
        pull_ack: (
            StatusCode::OK,
            json!({"ok": true, "acked": ["msg_msg1", "msg_msg2"], "rejected": []}),
        ),
        ..Behaviour::default()
    })
    .await;

    let (server_io, client_io) = duplex(4096);
    let (server_rx, server_tx) = tokio::io::split(server_io);
    let (client_rx, mut client_tx) = tokio::io::split(client_io);
    let mut client_rx = BufReader::new(client_rx);

    let config = SwarmMcpServerConfig {
        session: "20261020_owner".into(),
        pigeon_url: p.url.clone(),
        pigeon_token_file: None,
        honk_url: "http://127.0.0.1:4280/acp".into(),
        honk_secret_file: None,
    };

    let server_handle = tokio::spawn(run_server(config, BufReader::new(server_rx), server_tx));

    let resp = send_recv(
        &mut client_tx,
        &mut client_rx,
        json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": {
                "name": "swarm_read",
                "arguments": {},
                "_meta": {
                    "agent-session-id": "20261020_owner"
                }
            }
        }),
    )
    .await;

    assert_eq!(resp["result"]["isError"], Value::Null);
    let text = resp["result"]["content"][0]["text"].as_str().unwrap();

    assert!(
        text.contains("<swarm_message v=\"1\" kind=\"chat\" from=\"ses_alice\" msg_id=\"msg_msg1\" reply_to=\"msg_orig\">"),
        "envelope header: {text}"
    );
    assert!(
        text.contains("<swarm_message v=\"1\" kind=\"task.assign\" from=\"ses_bob\" msg_id=\"msg_msg2\" priority=\"urgent\">"),
        "urgent envelope header: {text}"
    );
    assert!(text.contains("ping from alice"), "envelope payload: {text}");
    assert!(text.contains("urgent task"), "envelope payload 2: {text}");
    assert!(text.contains("</swarm_message>"), "envelope close: {text}");
    assert!(
        text.contains("[pigeon] Message economy"),
        "economy footer: {text}"
    );
    assert!(
        text.contains("1 more unread message in inbox — call swarm_read again to drain."),
        "pending total note: {text}"
    );

    let seen = p.seen();
    assert_eq!(seen.len(), 3, "expected mailbox, drain, ack: {seen:?}");
    assert_eq!(seen[0].path, "/goose/swarm-mailbox");
    assert_eq!(seen[0].body["label"], "20261020_owner");
    let current_dir = std::env::current_dir().unwrap();
    assert_eq!(seen[0].body["cwd"], current_dir.to_str().unwrap());
    assert_eq!(seen[1].path, "/pull/drain");
    assert_eq!(seen[1].body["session_id"], "gse_reader");
    assert_eq!(seen[2].path, "/pull/ack");
    assert_eq!(seen[2].body["session_id"], "gse_reader");
    assert_eq!(seen[2].body["msg_ids"], json!(["msg_msg1", "msg_msg2"]));

    drop(client_tx);
    drop(client_rx);
    let _ = server_handle.await;
}

#[tokio::test]
async fn test_swarm_read_empty_inbox() {
    let p = StubPigeon::start(Behaviour {
        swarm_mailbox: (
            StatusCode::OK,
            json!({"ok": true, "session_id": "gse_reader", "backend_session_id": "20261020_owner"}),
        ),
        pull_drain: (
            StatusCode::OK,
            json!({"ok": true, "session_id": "gse_reader", "pending_total": 0, "messages": []}),
        ),
        ..Behaviour::default()
    })
    .await;

    let (server_io, client_io) = duplex(4096);
    let (server_rx, server_tx) = tokio::io::split(server_io);
    let (client_rx, mut client_tx) = tokio::io::split(client_io);
    let mut client_rx = BufReader::new(client_rx);

    let config = SwarmMcpServerConfig {
        session: "20261020_owner".into(),
        pigeon_url: p.url.clone(),
        pigeon_token_file: None,
        honk_url: "http://127.0.0.1:4280/acp".into(),
        honk_secret_file: None,
    };

    let server_handle = tokio::spawn(run_server(config, BufReader::new(server_rx), server_tx));

    let resp = send_recv(
        &mut client_tx,
        &mut client_rx,
        json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": {
                "name": "swarm_read",
                "arguments": {},
                "_meta": {
                    "agent-session-id": "20261020_owner"
                }
            }
        }),
    )
    .await;

    let text = resp["result"]["content"][0]["text"].as_str().unwrap();
    assert_eq!(text, "Inbox is empty.");

    // drain called, but NO ack called
    let seen = p.seen();
    assert_eq!(
        seen.len(),
        2,
        "only mailbox and drain, no ack on empty: {seen:?}"
    );
    assert_eq!(seen[1].path, "/pull/drain");

    drop(client_tx);
    drop(client_rx);
    let _ = server_handle.await;
}

#[tokio::test]
async fn test_swarm_list_marks_you_and_does_not_call_mailbox() {
    let now = 1700000000000i64;
    let p = StubPigeon::start(Behaviour {
        sessions: json!({
            "ok": true,
            "sessions": [
                {
                    "session_id": "gse_me",
                    "backend_kind": "goose-acp",
                    "backend_session_id": "20261020_owner",
                    "pull_sources": ["swarm"],
                    "updated_at": now - 120_000, // 2m ago
                    "cwd": "/work/proj",
                    "title": "My Task"
                },
                {
                    "session_id": "gse_other",
                    "backend_kind": "goose-acp",
                    "backend_session_id": "20261020_other",
                    "pull_sources": [],
                    "updated_at": now - 3_600_000, // 1h ago
                    "cwd": "/work/other",
                    "title": "Other Task"
                },
                {
                    "session_id": "ses_peer",
                    "backend_kind": "opencode-plugin-direct",
                    "updated_at": now - 30_000, // 30s ago
                    "cwd": "/work/open",
                    "title": "Peer Session"
                }
            ]
        }),
        ..Behaviour::default()
    })
    .await;

    let (server_io, client_io) = duplex(4096);
    let (server_rx, server_tx) = tokio::io::split(server_io);
    let (client_rx, mut client_tx) = tokio::io::split(client_io);
    let mut client_rx = BufReader::new(client_rx);

    let config = SwarmMcpServerConfig {
        session: "20261020_owner".into(),
        pigeon_url: p.url.clone(),
        pigeon_token_file: None,
        honk_url: "http://127.0.0.1:4280/acp".into(),
        honk_secret_file: None,
    };

    let server_handle = tokio::spawn(run_server(config, BufReader::new(server_rx), server_tx));

    let resp = send_recv(
        &mut client_tx,
        &mut client_rx,
        json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": {
                "name": "swarm_list",
                "arguments": {},
                "_meta": {
                    "agent-session-id": "20261020_owner"
                }
            }
        }),
    )
    .await;

    let text = resp["result"]["content"][0]["text"].as_str().unwrap();

    // Check sessions format
    assert!(
        text.contains("gse_me (you)"),
        "owner marked with (you): {text}"
    );
    assert!(text.contains("goose: mailbox"), "owner has mailbox: {text}");
    assert!(text.contains("gse_other"), "other session present: {text}");
    assert!(
        text.contains("goose: no mailbox"),
        "other goose session has no mailbox: {text}"
    );
    assert!(
        !text.contains("gse_other (you)"),
        "other not marked: {text}"
    );
    assert!(
        text.contains("ses_peer"),
        "opencode session present: {text}"
    );
    assert!(text.contains("opencode"), "opencode kind: {text}");

    // Stub seen must NOT contain /goose/swarm-mailbox
    let seen = p.seen();
    assert_eq!(seen.len(), 1, "only /sessions requested: {seen:?}");
    assert_eq!(seen[0].path, "/sessions");

    drop(client_tx);
    drop(client_rx);
    let _ = server_handle.await;
}

#[tokio::test]
async fn test_caller_id_from_env_var() {
    let p = StubPigeon::start(Behaviour::default()).await;
    let (server_io, client_io) = duplex(4096);
    let (server_rx, server_tx) = tokio::io::split(server_io);
    let (client_rx, mut client_tx) = tokio::io::split(client_io);
    let mut client_rx = BufReader::new(client_rx);

    let config = SwarmMcpServerConfig {
        session: "20261020_owner".into(),
        pigeon_url: p.url.clone(),
        pigeon_token_file: None,
        honk_url: "http://127.0.0.1:4280/acp".into(),
        honk_secret_file: None,
    };

    let server_handle = tokio::spawn(run_server(config, BufReader::new(server_rx), server_tx));

    // When neither _meta nor env var is provided, should be refused
    let resp = send_recv(
        &mut client_tx,
        &mut client_rx,
        json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": {
                "name": "swarm_send",
                "arguments": {
                    "to": "ses_target",
                    "message": "hello"
                }
            }
        }),
    )
    .await;

    assert_eq!(resp["result"]["isError"], true);
    assert!(
        resp["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("subagents cannot use them")
    );

    drop(client_tx);
    drop(client_rx);
    let _ = server_handle.await;
}

#[tokio::test]
async fn test_swarm_read_renders_redelivered_attribute() {
    let p = StubPigeon::start(Behaviour {
        swarm_mailbox: (
            StatusCode::OK,
            json!({"ok": true, "session_id": "gse_reader", "backend_session_id": "20261020_owner"}),
        ),
        pull_drain: (
            StatusCode::OK,
            json!({
                "ok": true,
                "session_id": "gse_reader",
                "pending_total": 1,
                "messages": [
                    {
                        "msg_id": "msg_redelivered",
                        "source": "swarm",
                        "sender_id": "ses_alice",
                        "kind": "chat",
                        "payload": "retry payload",
                        "redelivered": true,
                    }
                ]
            }),
        ),
        ..Behaviour::default()
    })
    .await;

    let (server_io, client_io) = duplex(4096);
    let (server_rx, server_tx) = tokio::io::split(server_io);
    let (client_rx, mut client_tx) = tokio::io::split(client_io);
    let mut client_rx = BufReader::new(client_rx);

    let config = SwarmMcpServerConfig {
        session: "20261020_owner".into(),
        pigeon_url: p.url.clone(),
        pigeon_token_file: None,
        honk_url: "http://127.0.0.1:4280/acp".into(),
        honk_secret_file: None,
    };

    let server_handle = tokio::spawn(run_server(config, BufReader::new(server_rx), server_tx));

    let resp = send_recv(
        &mut client_tx,
        &mut client_rx,
        json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": {
                "name": "swarm_read",
                "arguments": {},
                "_meta": {
                    "agent-session-id": "20261020_owner"
                }
            }
        }),
    )
    .await;

    let text = resp["result"]["content"][0]["text"].as_str().unwrap();
    assert!(
        text.contains(r#"redelivered="true""#),
        "expected redelivered attribute: {text}"
    );

    drop(client_tx);
    drop(client_rx);
    let _ = server_handle.await;
}

#[tokio::test]
async fn test_swarm_read_ack_http_failure_appends_warning() {
    let p = StubPigeon::start(Behaviour {
        swarm_mailbox: (
            StatusCode::OK,
            json!({"ok": true, "session_id": "gse_reader", "backend_session_id": "20261020_owner"}),
        ),
        pull_drain: (
            StatusCode::OK,
            json!({
                "ok": true,
                "session_id": "gse_reader",
                "pending_total": 1,
                "messages": [
                    {
                        "msg_id": "msg_fail_ack",
                        "source": "swarm",
                        "sender_id": "ses_alice",
                        "kind": "chat",
                        "payload": "hello",
                    }
                ]
            }),
        ),
        pull_ack: (
            StatusCode::INTERNAL_SERVER_ERROR,
            json!({"error": "db failed"}),
        ),
        ..Behaviour::default()
    })
    .await;

    let (server_io, client_io) = duplex(4096);
    let (server_rx, server_tx) = tokio::io::split(server_io);
    let (client_rx, mut client_tx) = tokio::io::split(client_io);
    let mut client_rx = BufReader::new(client_rx);

    let config = SwarmMcpServerConfig {
        session: "20261020_owner".into(),
        pigeon_url: p.url.clone(),
        pigeon_token_file: None,
        honk_url: "http://127.0.0.1:4280/acp".into(),
        honk_secret_file: None,
    };

    let server_handle = tokio::spawn(run_server(config, BufReader::new(server_rx), server_tx));

    let resp = send_recv(
        &mut client_tx,
        &mut client_rx,
        json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": {
                "name": "swarm_read",
                "arguments": {},
                "_meta": {
                    "agent-session-id": "20261020_owner"
                }
            }
        }),
    )
    .await;

    let text = resp["result"]["content"][0]["text"].as_str().unwrap();
    assert!(
        text.contains("warning: 1 message could not be acknowledged and may be shown again"),
        "expected warning in text: {text}"
    );

    drop(client_tx);
    drop(client_rx);
    let _ = server_handle.await;
}

#[tokio::test]
async fn test_swarm_read_ack_rejected_appends_warning() {
    let p = StubPigeon::start(Behaviour {
        swarm_mailbox: (
            StatusCode::OK,
            json!({"ok": true, "session_id": "gse_reader", "backend_session_id": "20261020_owner"}),
        ),
        pull_drain: (
            StatusCode::OK,
            json!({
                "ok": true,
                "session_id": "gse_reader",
                "pending_total": 2,
                "messages": [
                    {
                        "msg_id": "msg_rej1",
                        "source": "swarm",
                        "sender_id": "ses_alice",
                        "kind": "chat",
                        "payload": "m1",
                    },
                    {
                        "msg_id": "msg_rej2",
                        "source": "swarm",
                        "sender_id": "ses_bob",
                        "kind": "chat",
                        "payload": "m2",
                    }
                ]
            }),
        ),
        pull_ack: (
            StatusCode::OK,
            json!({"ok": true, "acked": ["msg_rej1"], "rejected": ["msg_rej2"]}),
        ),
        ..Behaviour::default()
    })
    .await;

    let (server_io, client_io) = duplex(4096);
    let (server_rx, server_tx) = tokio::io::split(server_io);
    let (client_rx, mut client_tx) = tokio::io::split(client_io);
    let mut client_rx = BufReader::new(client_rx);

    let config = SwarmMcpServerConfig {
        session: "20261020_owner".into(),
        pigeon_url: p.url.clone(),
        pigeon_token_file: None,
        honk_url: "http://127.0.0.1:4280/acp".into(),
        honk_secret_file: None,
    };

    let server_handle = tokio::spawn(run_server(config, BufReader::new(server_rx), server_tx));

    let resp = send_recv(
        &mut client_tx,
        &mut client_rx,
        json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": {
                "name": "swarm_read",
                "arguments": {},
                "_meta": {
                    "agent-session-id": "20261020_owner"
                }
            }
        }),
    )
    .await;

    let text = resp["result"]["content"][0]["text"].as_str().unwrap();
    assert!(
        text.contains("warning: 1 message could not be acknowledged and may be shown again"),
        "expected warning in text: {text}"
    );

    drop(client_tx);
    drop(client_rx);
    let _ = server_handle.await;
}
