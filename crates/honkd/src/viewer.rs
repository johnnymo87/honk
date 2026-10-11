use axum::extract::ws::{CloseFrame, Message, WebSocket};
use futures::{SinkExt, StreamExt};
use serde_json::{Value, json};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::sync::{Notify, mpsc};

use crate::hub::Hub;

static NEXT_VIEWER_ID: AtomicU64 = AtomicU64::new(1);

#[derive(Clone)]
pub struct ViewerTx {
    tx: mpsc::Sender<Message>,
    evict: Arc<Notify>,
}

impl ViewerTx {
    pub fn new(tx: mpsc::Sender<Message>, evict: Arc<Notify>) -> Self {
        Self { tx, evict }
    }

    pub fn send_frame(&self, msg: Message) -> bool {
        match self.tx.try_send(msg) {
            Ok(()) => true,
            Err(mpsc::error::TrySendError::Full(_)) => {
                self.evict.notify_one();
                false
            }
            Err(mpsc::error::TrySendError::Closed(_)) => false,
        }
    }
}

pub struct Viewer {
    pub id: u64,
    pub is_honk_aware: bool,
    pub tx: ViewerTx,
}

pub async fn viewer_loop(socket: WebSocket, hub: Arc<Hub>) {
    let viewer_id = NEXT_VIEWER_ID.fetch_add(1, Ordering::SeqCst);
    let (mut ws_sender, mut ws_receiver) = socket.split();
    let (tx, mut rx) = mpsc::channel::<Message>(hub.args.viewer_queue);
    let evict = Arc::new(Notify::new());
    let evict_clone = evict.clone();

    let write_task = tokio::spawn(async move {
        loop {
            tokio::select! {
                biased;
                _ = evict_clone.notified() => {
                    let _ = ws_sender
                        .send(Message::Close(Some(CloseFrame {
                            code: 4001,
                            reason: "queue overflow".into(),
                        })))
                        .await;
                    break;
                }
                msg = rx.recv() => {
                    match msg {
                        Some(m) => {
                            if ws_sender.send(m).await.is_err() {
                                break;
                            }
                        }
                        None => break,
                    }
                }
            }
        }
    });

    let viewer_tx = ViewerTx::new(tx, evict);

    let mut viewer = Viewer {
        id: viewer_id,
        is_honk_aware: false,
        tx: viewer_tx,
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
                        if !viewer
                            .tx
                            .send_frame(Message::Text(err_resp.to_string().into()))
                        {
                            break;
                        }
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
                    if !viewer
                        .tx
                        .send_frame(Message::Text(resp_frame.to_string().into()))
                    {
                        break;
                    }
                } else if method == Some("session/new") {
                    match hub.create_session(params).await {
                        Ok(new_res) => {
                            let resp_frame = json!({
                                "jsonrpc": "2.0",
                                "id": id,
                                "result": new_res,
                            });
                            if !viewer
                                .tx
                                .send_frame(Message::Text(resp_frame.to_string().into()))
                            {
                                break;
                            }
                        }
                        Err(e) => {
                            let err_resp = json!({
                                "jsonrpc": "2.0",
                                "id": id,
                                "error": {
                                    "code": -32603,
                                    "message": format!("Failed to create session: {e:#}"),
                                }
                            });
                            if !viewer
                                .tx
                                .send_frame(Message::Text(err_resp.to_string().into()))
                            {
                                break;
                            }
                        }
                    }
                } else if method == Some("session/list")
                    || !params
                        .as_object()
                        .is_some_and(|o| o.contains_key("sessionId"))
                {
                    match hub
                        .control_request(method.unwrap_or(""), params, id.clone())
                        .await
                    {
                        Ok(resp) => {
                            if !viewer.tx.send_frame(Message::Text(resp.to_string().into())) {
                                break;
                            }
                        }
                        Err(e) => {
                            let err_resp = json!({
                                "jsonrpc": "2.0",
                                "id": id,
                                "error": {
                                    "code": -32603,
                                    "message": format!("Control connection error: {e:#}"),
                                }
                            });
                            if !viewer
                                .tx
                                .send_frame(Message::Text(err_resp.to_string().into()))
                            {
                                break;
                            }
                        }
                    }
                } else if method == Some("session/load") {
                    viewer.is_honk_aware = true;
                    let err_resp = json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "error": {
                            "code": -32601,
                            "message": "Method not implemented in A8"
                        }
                    });
                    if !viewer
                        .tx
                        .send_frame(Message::Text(err_resp.to_string().into()))
                    {
                        break;
                    }
                } else {
                    let err_resp = json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "error": {
                            "code": -32601,
                            "message": "Method not implemented in A8"
                        }
                    });
                    if !viewer
                        .tx
                        .send_frame(Message::Text(err_resp.to_string().into()))
                    {
                        break;
                    }
                }
            }
            Message::Ping(payload) => {
                if !viewer.tx.send_frame(Message::Pong(payload)) {
                    break;
                }
            }
            Message::Close(_) => {
                break;
            }
            _ => {}
        }
    }

    drop(viewer);
    let _ = write_task.await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_send_frame_overflow_triggers_evict() {
        let (tx, _rx) = mpsc::channel::<Message>(1);
        let evict = Arc::new(Notify::new());
        let viewer_tx = ViewerTx::new(tx, evict.clone());

        // First message fits in buffer
        assert!(viewer_tx.send_frame(Message::Text("1".into())));
        // Second message overflows
        assert!(!viewer_tx.send_frame(Message::Text("2".into())));

        // Verify evict was notified
        let notified =
            tokio::time::timeout(std::time::Duration::from_millis(100), evict.notified())
                .await
                .is_ok();
        assert!(notified);
    }
}
