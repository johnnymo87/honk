use axum::extract::ws::{CloseFrame, Message, WebSocket};
use futures::{SinkExt, StreamExt};
use serde_json::{Value, json};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::hub::Hub;

static NEXT_VIEWER_ID: AtomicU64 = AtomicU64::new(1);

pub struct Viewer {
    pub id: u64,
    pub is_honk_aware: bool,
    pub tx: tokio::sync::mpsc::Sender<Message>,
}

pub async fn viewer_loop(socket: WebSocket, hub: Arc<Hub>) {
    let viewer_id = NEXT_VIEWER_ID.fetch_add(1, Ordering::SeqCst);
    let (mut ws_sender, mut ws_receiver) = socket.split();
    let (tx, mut rx) = tokio::sync::mpsc::channel::<Message>(hub.args.viewer_queue);

    let write_task = tokio::spawn(async move {
        while let Some(msg) = rx.recv().await {
            if ws_sender.send(msg).await.is_err() {
                break;
            }
        }
    });

    let mut viewer = Viewer {
        id: viewer_id,
        is_honk_aware: false,
        tx: tx.clone(),
    };

    while let Some(msg_res) = ws_receiver.next().await {
        let msg = match msg_res {
            Ok(m) => m,
            Err(_) => break,
        };

        match msg {
            Message::Text(text) => {
                let frame: Value = match serde_json::from_str(&text) {
                    Ok(v) => v,
                    Err(_) => {
                        let err_resp = json!({
                            "jsonrpc": "2.0",
                            "id": Value::Null,
                            "error": {
                                "code": -32700,
                                "message": "Parse error"
                            }
                        });
                        let _ = tx.send(Message::Text(err_resp.to_string().into())).await;
                        continue;
                    }
                };

                let method = frame.get("method").and_then(Value::as_str);
                let id = frame.get("id").cloned().unwrap_or(Value::Null);
                let params = frame.get("params").cloned().unwrap_or(json!({}));

                if method == Some("initialize") {
                    if params.pointer("/_meta/honk").is_some() {
                        viewer.is_honk_aware = true;
                    }
                    let res = hub.handle_initialize(&params);
                    let resp_frame = json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "result": res
                    });
                    if let Err(e) = viewer
                        .tx
                        .try_send(Message::Text(resp_frame.to_string().into()))
                    {
                        match e {
                            tokio::sync::mpsc::error::TrySendError::Full(_) => {
                                let _ = viewer.tx.try_send(Message::Close(Some(CloseFrame {
                                    code: 4001,
                                    reason: "queue overflow".into(),
                                })));
                                break;
                            }
                            tokio::sync::mpsc::error::TrySendError::Closed(_) => break,
                        }
                    }
                } else if method == Some("session/load") {
                    viewer.is_honk_aware = true;
                    let err_resp = json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "error": {
                            "code": -32601,
                            "message": "Method not implemented in A7"
                        }
                    });
                    let _ = viewer
                        .tx
                        .send(Message::Text(err_resp.to_string().into()))
                        .await;
                } else {
                    let err_resp = json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "error": {
                            "code": -32601,
                            "message": "Method not implemented in A7"
                        }
                    });
                    let _ = viewer
                        .tx
                        .send(Message::Text(err_resp.to_string().into()))
                        .await;
                }
            }
            Message::Ping(payload) => {
                let _ = tx.send(Message::Pong(payload)).await;
            }
            Message::Close(_) => {
                break;
            }
            _ => {}
        }
    }

    drop(tx);
    let _ = write_task.await;
}
