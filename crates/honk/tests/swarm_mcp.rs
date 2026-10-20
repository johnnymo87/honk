mod support;

use axum::http::StatusCode;
use honk::swarm_mcp::{SwarmMcpServerConfig, run_server};
use serde_json::{Value, json};
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
    assert_eq!(names, vec!["swarm_send", "swarm_read", "swarm_list"]);

    // Descriptions should mention message economy rules
    let send_tool = tools.iter().find(|t| t["name"] == "swarm_send").unwrap();
    let desc = send_tool["description"].as_str().unwrap();
    assert!(
        desc.contains("/swarm_message"),
        "must mention </swarm_message> rule: {desc}"
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
                        "priority": "normal",
                        "created_at": 1700000000000i64,
                    }
                ]
            }),
        ),
        pull_ack: (
            StatusCode::OK,
            json!({"ok": true, "acked": ["msg_msg1"], "rejected": []}),
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
                "arguments": {
                    "limit": 1
                },
                "_meta": {
                    "agent-session-id": "20261020_owner"
                }
            }
        }),
    )
    .await;

    assert_eq!(resp["result"]["isError"], Value::Null);
    let text = resp["result"]["content"][0]["text"].as_str().unwrap();

    assert!(text.contains("<swarm_message v=\"1\" kind=\"chat\" from=\"ses_alice\" msg_id=\"msg_msg1\" priority=\"normal\" reply_to=\"msg_orig\">"), "envelope header: {text}");
    assert!(text.contains("ping from alice"), "envelope payload: {text}");
    assert!(text.contains("</swarm_message>"), "envelope close: {text}");
    assert!(
        text.contains("[pigeon] Message economy"),
        "economy footer: {text}"
    );
    assert!(
        text.contains("2 more unread messages in inbox — call swarm_read again to drain."),
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
    assert_eq!(seen[1].body["limit"], 1);
    assert_eq!(seen[2].path, "/pull/ack");
    assert_eq!(seen[2].body["session_id"], "gse_reader");
    assert_eq!(seen[2].body["msg_ids"], json!(["msg_msg1"]));

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
