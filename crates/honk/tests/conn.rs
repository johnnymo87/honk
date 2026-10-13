use futures::{SinkExt, StreamExt};
use honk::{Client, Conn, is_response};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio_tungstenite::tungstenite::handshake::server::{Request, Response};
use tokio_tungstenite::tungstenite::protocol::Message;

#[allow(clippy::result_large_err)]
async fn accept_test_ws<F>(listener: TcpListener, handler: F) -> tokio::task::JoinHandle<()>
where
    F: FnOnce(
            tokio_tungstenite::WebSocketStream<tokio::net::TcpStream>,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + 'static>>
        + Send
        + 'static,
{
    tokio::spawn(async move {
        let (tcp_stream, _) = listener.accept().await.unwrap();
        let ws =
            tokio_tungstenite::accept_hdr_async(tcp_stream, |req: &Request, resp: Response| {
                assert_eq!(
                    req.headers()
                        .get("x-secret-key")
                        .and_then(|v| v.to_str().ok()),
                    Some("test-secret-123")
                );
                Ok(resp)
            })
            .await
            .unwrap();
        handler(ws).await;
    })
}

async fn handle_initialize(ws: &mut tokio_tungstenite::WebSocketStream<tokio::net::TcpStream>) {
    let msg = ws.next().await.unwrap().unwrap();
    match msg {
        Message::Text(t) => {
            let v: Value = serde_json::from_str(&t).unwrap();
            assert_eq!(v["method"], "initialize");
            assert_eq!(v["id"], 1);
            assert!(v["params"]["_meta"]["honk"].is_object());

            let resp = json!({
                "jsonrpc": "2.0",
                "id": 1,
                "result": {
                    "protocolVersion": 1,
                    "_meta": {
                        "honk": {
                            "features": ["resume_only"]
                        }
                    }
                }
            });
            ws.send(Message::Text(resp.to_string().into()))
                .await
                .unwrap();
        }
        other => panic!("expected text message, got {other:?}"),
    }
}

#[tokio::test]
async fn test_handshake_sends_initialize_with_meta_honk_and_returns_result() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    let server_handle = accept_test_ws(listener, |mut ws| {
        Box::pin(async move {
            handle_initialize(&mut ws).await;
        })
    })
    .await;

    let (conn, _rx, init_res) = Conn::connect(&addr.to_string(), "test-secret-123")
        .await
        .unwrap();

    assert_eq!(
        init_res["_meta"]["honk"]["features"],
        json!(["resume_only"])
    );
    assert_eq!(
        conn.init_result()["_meta"]["honk"]["features"],
        json!(["resume_only"])
    );

    server_handle.await.unwrap();
}

#[test]
fn test_is_response_helper() {
    // has id and no method -> true
    assert!(is_response(&json!({"id": 1, "result": {"foo": "bar"}})));
    assert!(is_response(&json!({"id": "str-id", "result": 123})));
    assert!(is_response(&json!({"id": 2, "error": {"code": -32600}})));

    // has id and has method (server request from goose) -> false
    assert!(!is_response(&json!({
        "id": 1,
        "method": "session/request_permission",
        "params": {}
    })));
    assert!(!is_response(&json!({
        "id": "req-1",
        "method": "custom_request"
    })));

    // no id (notification) -> false
    assert!(!is_response(&json!({
        "method": "session_info_update",
        "params": {}
    })));

    // non-object -> false
    assert!(!is_response(&json!("not an object")));
    assert!(!is_response(&json!(42)));
    assert!(!is_response(&json!(null)));
}

#[tokio::test]
async fn test_request_ids_increase_and_response_arrives_on_receiver() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    let server_handle = accept_test_ws(listener, |mut ws| {
        Box::pin(async move {
            handle_initialize(&mut ws).await;

            // Handle two requests
            for _ in 0..2 {
                let msg = ws.next().await.unwrap().unwrap();
                match msg {
                    Message::Text(t) => {
                        let v: Value = serde_json::from_str(&t).unwrap();
                        let id = &v["id"];
                        let resp = json!({
                            "jsonrpc": "2.0",
                            "id": id,
                            "result": { "echo": v["params"] }
                        });
                        ws.send(Message::Text(resp.to_string().into()))
                            .await
                            .unwrap();
                    }
                    other => panic!("expected text message, got {other:?}"),
                }
            }
        })
    })
    .await;

    let (conn, mut rx, _) = Conn::connect(&addr.to_string(), "test-secret-123")
        .await
        .unwrap();

    let id1 = conn.request("method_a", json!({"n": 1})).await.unwrap();
    let id2 = conn.request("method_b", json!({"n": 2})).await.unwrap();

    assert!(id2 > id1, "request ids must increase: {id2} > {id1}");

    let resp1 = rx.recv().await.unwrap();
    let resp2 = rx.recv().await.unwrap();

    assert_eq!(resp1["id"], json!(id1));
    assert_eq!(resp1["result"]["echo"]["n"], 1);
    assert_eq!(resp2["id"], json!(id2));
    assert_eq!(resp2["result"]["echo"]["n"], 2);

    server_handle.await.unwrap();
}

#[tokio::test]
async fn test_respond_echoes_string_id_verbatim() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    let server_handle = accept_test_ws(listener, |mut ws| {
        Box::pin(async move {
            handle_initialize(&mut ws).await;

            // Send a server request with a string id
            let srv_req = json!({
                "jsonrpc": "2.0",
                "id": "goose-server-req-xyz",
                "method": "session/request_permission",
                "params": {"tool": "bash"}
            });
            ws.send(Message::Text(srv_req.to_string().into()))
                .await
                .unwrap();

            // Wait for client's respond
            let msg = ws.next().await.unwrap().unwrap();
            match msg {
                Message::Text(t) => {
                    let v: Value = serde_json::from_str(&t).unwrap();
                    assert_eq!(v["id"], "goose-server-req-xyz");
                    assert_eq!(v["result"]["outcome"], "selected");
                    assert!(is_response(&v));
                }
                other => panic!("expected text message, got {other:?}"),
            }
        })
    })
    .await;

    let (conn, mut rx, _) = Conn::connect(&addr.to_string(), "test-secret-123")
        .await
        .unwrap();

    let frame = rx.recv().await.unwrap();
    assert!(!is_response(&frame));
    assert_eq!(frame["method"], "session/request_permission");
    assert_eq!(frame["id"], "goose-server-req-xyz");

    conn.respond(frame["id"].clone(), json!({"outcome": "selected"}))
        .await
        .unwrap();

    server_handle.await.unwrap();
}

#[tokio::test]
async fn test_concurrent_send_while_frames_stream() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    let server_handle = accept_test_ws(listener, |mut ws| {
        Box::pin(async move {
            handle_initialize(&mut ws).await;

            let (mut sink, mut stream) = ws.split();

            // Background task to send 50 notifications to client
            let send_task = tokio::spawn(async move {
                for i in 0..50 {
                    let notif = json!({
                        "jsonrpc": "2.0",
                        "method": "session_info_update",
                        "params": { "i": i }
                    });
                    sink.send(Message::Text(notif.to_string().into()))
                        .await
                        .unwrap();
                    tokio::time::sleep(tokio::time::Duration::from_millis(2)).await;
                }
            });

            // Reader task to reply to client requests
            let mut req_count = 0;
            while let Some(Ok(msg)) = stream.next().await {
                if let Message::Text(t) = msg {
                    let v: Value = serde_json::from_str(&t).unwrap();
                    if let Some(_id) = v.get("id") {
                        req_count += 1;
                        // Pong / reply to client
                        if req_count == 10 {
                            break;
                        }
                    }
                }
            }

            send_task.await.unwrap();
        })
    })
    .await;

    let (conn, mut rx, _) = Conn::connect(&addr.to_string(), "test-secret-123")
        .await
        .unwrap();

    // Client concurrently sends 10 requests while notifications stream in
    let conn_clone = conn.clone();
    let client_send = tokio::spawn(async move {
        for i in 0..10 {
            conn_clone
                .request("client_method", json!({"i": i}))
                .await
                .unwrap();
            tokio::time::sleep(tokio::time::Duration::from_millis(5)).await;
        }
    });

    let mut notifs_received = 0;
    while let Some(frame) = rx.recv().await {
        if frame.get("method").and_then(Value::as_str) == Some("session_info_update") {
            notifs_received += 1;
            if notifs_received == 50 {
                break;
            }
        }
    }

    client_send.await.unwrap();
    assert_eq!(notifs_received, 50);
    conn.close().await.unwrap();
    server_handle.await.unwrap();
}

#[tokio::test]
async fn test_receiver_ends_on_server_close() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    let server_handle = accept_test_ws(listener, |mut ws| {
        Box::pin(async move {
            handle_initialize(&mut ws).await;
            // Send one frame then close
            let frame = json!({
                "jsonrpc": "2.0",
                "method": "farewell",
                "params": {}
            });
            ws.send(Message::Text(frame.to_string().into()))
                .await
                .unwrap();
            ws.close(None).await.unwrap();
        })
    })
    .await;

    let (_conn, mut rx, _) = Conn::connect(&addr.to_string(), "test-secret-123")
        .await
        .unwrap();

    let frame = rx.recv().await.unwrap();
    assert_eq!(frame["method"], "farewell");

    // Next recv should be None because server closed
    assert_eq!(rx.recv().await, None);
    server_handle.await.unwrap();
}

#[tokio::test]
async fn test_auth_failure_401_returns_fatal_error() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    let server_handle = tokio::spawn(async move {
        let (mut tcp_stream, _) = listener.accept().await.unwrap();
        let mut buf = [0u8; 1024];
        let _ = tcp_stream.read(&mut buf).await;
        tcp_stream
            .write_all(b"HTTP/1.1 401 Unauthorized\r\nContent-Length: 0\r\n\r\n")
            .await
            .unwrap();
    });

    let err = Conn::connect(&addr.to_string(), "bad-secret")
        .await
        .unwrap_err();

    assert!(
        err.to_string().starts_with("fatal: "),
        "expected fatal: prefix, got: {err}"
    );

    server_handle.await.unwrap();
}

#[tokio::test]
async fn test_auth_failure_403_returns_fatal_error() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    let server_handle = tokio::spawn(async move {
        let (mut tcp_stream, _) = listener.accept().await.unwrap();
        let mut buf = [0u8; 1024];
        let _ = tcp_stream.read(&mut buf).await;
        tcp_stream
            .write_all(b"HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\n\r\n")
            .await
            .unwrap();
    });

    let err = Conn::connect(&addr.to_string(), "bad-secret")
        .await
        .unwrap_err();

    assert!(
        err.to_string().starts_with("fatal: "),
        "expected fatal: prefix, got: {err}"
    );

    server_handle.await.unwrap();
}

#[tokio::test]
async fn test_client_connect_shares_handshake() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    let server_handle = accept_test_ws(listener, |mut ws| {
        Box::pin(async move {
            handle_initialize(&mut ws).await;
        })
    })
    .await;

    let client = Client::connect(&addr.to_string(), "test-secret-123")
        .await
        .unwrap();

    drop(client);
    server_handle.await.unwrap();
}

#[tokio::test]
async fn test_ping_pong_handled_automatically() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    let server_handle = accept_test_ws(listener, |mut ws| {
        Box::pin(async move {
            handle_initialize(&mut ws).await;

            // Server sends Ping
            ws.send(Message::Ping(vec![42, 43, 44].into()))
                .await
                .unwrap();

            // Server expects Pong back from client reader/writer
            let msg = ws.next().await.unwrap().unwrap();
            match msg {
                Message::Pong(payload) => {
                    assert_eq!(payload.as_ref(), &[42, 43, 44]);
                }
                other => panic!("expected pong message, got {other:?}"),
            }
        })
    })
    .await;

    let (conn, _rx, _) = Conn::connect(&addr.to_string(), "test-secret-123")
        .await
        .unwrap();

    server_handle.await.unwrap();
    conn.close().await.unwrap();
}

#[tokio::test]
async fn test_initialize_error_returns_fatal_error() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    let server_handle = accept_test_ws(listener, |mut ws| {
        Box::pin(async move {
            let msg = ws.next().await.unwrap().unwrap();
            match msg {
                Message::Text(_) => {
                    let resp = json!({
                        "jsonrpc": "2.0",
                        "id": 1,
                        "error": {
                            "code": -32600,
                            "message": "protocol version unsupported"
                        }
                    });
                    ws.send(Message::Text(resp.to_string().into()))
                        .await
                        .unwrap();
                }
                other => panic!("expected text message, got {other:?}"),
            }
        })
    })
    .await;

    let err = Conn::connect(&addr.to_string(), "test-secret-123")
        .await
        .unwrap_err();

    assert!(
        err.to_string()
            .starts_with("fatal: honkd initialize failed:"),
        "expected fatal initialize error, got: {err}"
    );

    server_handle.await.unwrap();
}

#[tokio::test]
async fn test_notify_sends_notification_frame() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    let server_handle = accept_test_ws(listener, |mut ws| {
        Box::pin(async move {
            handle_initialize(&mut ws).await;

            let msg = ws.next().await.unwrap().unwrap();
            match msg {
                Message::Text(t) => {
                    let v: Value = serde_json::from_str(&t).unwrap();
                    assert_eq!(v["method"], "session/cancel");
                    assert_eq!(v["params"]["sessionId"], "ses_123");
                    assert!(v.get("id").is_none());
                    assert!(!is_response(&v));
                }
                other => panic!("expected text message, got {other:?}"),
            }
        })
    })
    .await;

    let (conn, _rx, _) = Conn::connect(&addr.to_string(), "test-secret-123")
        .await
        .unwrap();

    conn.notify("session/cancel", json!({"sessionId": "ses_123"}))
        .await
        .unwrap();

    server_handle.await.unwrap();
}
