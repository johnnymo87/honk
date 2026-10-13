use crate::client::connect_and_initialize;
use futures::{SinkExt, StreamExt};
use serde_json::{Value, json};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::protocol::Message;

#[derive(Clone, Debug)]
pub struct Conn {
    write_tx: mpsc::UnboundedSender<Message>,
    next_req_id: Arc<AtomicU64>,
    init_result: Value,
    closed: Arc<AtomicBool>,
}

impl Conn {
    pub async fn connect(
        url: &str,
        secret: &str,
    ) -> anyhow::Result<(Self, mpsc::UnboundedReceiver<Value>, Value)> {
        let (ws, init_result) = connect_and_initialize(url, secret).await?;
        let (mut ws_sink, mut ws_stream) = ws.split();

        let (write_tx, mut write_rx) = mpsc::unbounded_channel::<Message>();
        let (frame_tx, frame_rx) = mpsc::unbounded_channel::<Value>();

        let write_tx_weak = write_tx.downgrade();
        let closed = Arc::new(AtomicBool::new(false));
        let closed_for_writer = Arc::clone(&closed);
        let closed_for_reader = Arc::clone(&closed);

        // Writer task: sends outgoing messages to ws_sink
        tokio::spawn(async move {
            while let Some(msg) = write_rx.recv().await {
                let is_close = matches!(msg, Message::Close(_));
                if ws_sink.send(msg).await.is_err() {
                    closed_for_writer.store(true, Ordering::SeqCst);
                    break;
                }
                if is_close {
                    break;
                }
            }
            closed_for_writer.store(true, Ordering::SeqCst);
            let _ = ws_sink.close().await;
        });

        // Reader task: receives incoming messages from ws_stream
        tokio::spawn(async move {
            while let Some(msg_res) = ws_stream.next().await {
                match msg_res {
                    Ok(Message::Text(t)) => {
                        if let Ok(v) = serde_json::from_str::<Value>(&t)
                            && frame_tx.send(v).is_err()
                        {
                            break;
                        }
                    }
                    Ok(Message::Ping(payload)) => {
                        if let Some(tx) = write_tx_weak.upgrade() {
                            let _ = tx.send(Message::Pong(payload));
                        } else {
                            break;
                        }
                    }
                    Ok(Message::Pong(_)) => {}
                    Ok(Message::Close(frame)) => {
                        if let Some(tx) = write_tx_weak.upgrade() {
                            let _ = tx.send(Message::Close(frame));
                        }
                        break;
                    }
                    Ok(Message::Binary(_)) => {}
                    Ok(Message::Frame(_)) => {}
                    Err(_) => {
                        break;
                    }
                }
            }
            closed_for_reader.store(true, Ordering::SeqCst);
        });

        let conn = Self {
            write_tx,
            next_req_id: Arc::new(AtomicU64::new(2)),
            init_result: init_result.clone(),
            closed,
        };

        Ok((conn, frame_rx, init_result))
    }

    pub async fn request(&self, method: &str, params: Value) -> anyhow::Result<u64> {
        if self.closed.load(Ordering::SeqCst) {
            anyhow::bail!("failed to send {method} request: connection closed");
        }
        let id = self.next_req_id.fetch_add(1, Ordering::SeqCst);
        let req = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });
        self.write_tx
            .send(Message::Text(req.to_string().into()))
            .map_err(|_| {
                self.closed.store(true, Ordering::SeqCst);
                anyhow::anyhow!("failed to send {method} request: connection closed")
            })?;
        Ok(id)
    }

    pub async fn notify(&self, method: &str, params: Value) -> anyhow::Result<()> {
        if self.closed.load(Ordering::SeqCst) {
            anyhow::bail!("failed to send {method} notification: connection closed");
        }
        let notif = json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params,
        });
        self.write_tx
            .send(Message::Text(notif.to_string().into()))
            .map_err(|_| {
                self.closed.store(true, Ordering::SeqCst);
                anyhow::anyhow!("failed to send {method} notification: connection closed")
            })?;
        Ok(())
    }

    pub async fn respond(&self, id: Value, result: Value) -> anyhow::Result<()> {
        if self.closed.load(Ordering::SeqCst) {
            anyhow::bail!("failed to send response for request {id}: connection closed");
        }
        let resp = json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": result,
        });
        self.write_tx
            .send(Message::Text(resp.to_string().into()))
            .map_err(|_| {
                self.closed.store(true, Ordering::SeqCst);
                anyhow::anyhow!("failed to send response for request {id}: connection closed")
            })?;
        Ok(())
    }

    pub async fn close(&self) -> anyhow::Result<()> {
        self.closed.store(true, Ordering::SeqCst);
        let _ = self.write_tx.send(Message::Close(None));
        Ok(())
    }

    pub fn is_closed(&self) -> bool {
        self.closed.load(Ordering::SeqCst)
    }

    pub fn init_result(&self) -> &Value {
        &self.init_result
    }
}

pub fn is_response(frame: &Value) -> bool {
    let Some(obj) = frame.as_object() else {
        return false;
    };
    obj.contains_key("id") && !obj.contains_key("method")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_response() {
        assert!(is_response(&json!({"id": 1, "result": {}})));
        assert!(is_response(&json!({"id": "abc", "error": {}})));
        assert!(!is_response(
            &json!({"id": 1, "method": "req", "params": {}})
        ));
        assert!(!is_response(&json!({"method": "notif"})));
        assert!(!is_response(&json!(123)));
    }
}
