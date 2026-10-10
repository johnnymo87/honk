use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use anyhow::Context;
use futures::StreamExt;
use reqwest::{StatusCode, Url};
use serde_json::Value;

use crate::guard_url;
use crate::record::{Direction, Recorder};

/// Identifies whether a stream belongs to the connection mailbox or a specific session mailbox.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum StreamKey {
    Conn,
    Session(String),
}

impl StreamKey {
    pub fn stream_record_name(&self) -> String {
        match self {
            StreamKey::Conn => "conn".to_string(),
            StreamKey::Session(id) => format!("session:{id}"),
        }
    }
}

/// Upstream ACP HTTP+SSE client.
pub struct Upstream {
    url: Url,
    secret: String,
    conn_id: String,
    init_result: Option<Value>,
    client: reqwest::Client,
    next_request_id: AtomicU64,
    rec: Recorder,
    stream_tasks: HashMap<StreamKey, tokio::task::JoinHandle<()>>,
    rx: tokio::sync::mpsc::UnboundedReceiver<(StreamKey, Value)>,
    tx: tokio::sync::mpsc::UnboundedSender<(StreamKey, Value)>,
    buffered_frames: VecDeque<(StreamKey, Value)>,
    subscribe_deadline: Duration,
    subscribe_initial_backoff: Duration,
    subscribe_max_backoff: Duration,
    last_call_stream: Option<StreamKey>,
}

impl Upstream {
    /// Connects and initializes a new ACP connection.
    ///
    /// Validates `url` via [`guard_url`], sends `initialize` JSON-RPC POST,
    /// stores the resulting `Acp-Connection-Id`, and records all frames.
    pub async fn initialize(url: &str, secret: String, rec: Recorder) -> anyhow::Result<Self> {
        let parsed_url = guard_url(url)?;
        let client = reqwest::Client::builder().build()?;

        let init_body = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": 1,
                "clientCapabilities": {}
            }
        });

        let res = client
            .post(parsed_url.clone())
            .header("x-secret-key", &secret)
            .header("content-type", "application/json")
            .json(&init_body)
            .send()
            .await
            .with_context(|| format!("failed to send POST initialize to {url}"))?;

        let status = res.status();
        rec.record(
            Direction::Out,
            "post",
            Some(status.as_u16()),
            Some(init_body),
            None,
        );

        if !status.is_success() {
            let err_text = res.text().await.unwrap_or_default();
            anyhow::bail!("initialize POST failed with status {status}: {err_text}");
        }

        let conn_id = res
            .headers()
            .get("acp-connection-id")
            .ok_or_else(|| anyhow::anyhow!("initialize response missing Acp-Connection-Id header"))?
            .to_str()
            .with_context(|| "invalid Acp-Connection-Id header")?
            .to_string();

        let init_response: Value = res
            .json()
            .await
            .with_context(|| "failed to parse initialize response JSON body")?;

        rec.record_in("conn", init_response.clone());

        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();

        Ok(Self {
            url: parsed_url,
            secret,
            conn_id,
            init_result: Some(init_response),
            client,
            next_request_id: AtomicU64::new(2),
            rec,
            stream_tasks: HashMap::new(),
            rx,
            tx,
            buffered_frames: VecDeque::new(),
            subscribe_deadline: Duration::from_secs(60),
            subscribe_initial_backoff: Duration::from_millis(100),
            subscribe_max_backoff: Duration::from_secs(2),
            last_call_stream: None,
        })
    }

    /// Resumes an existing connection id without re-running initialize.
    pub fn resume(
        url: &str,
        secret: String,
        conn_id: String,
        rec: Recorder,
    ) -> anyhow::Result<Self> {
        let parsed_url = guard_url(url)?;
        let client = reqwest::Client::builder().build()?;
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();

        rec.record_marker("conn", format!("resumed connection {conn_id}"));

        Ok(Self {
            url: parsed_url,
            secret,
            conn_id,
            init_result: None,
            client,
            next_request_id: AtomicU64::new(1000),
            rec,
            stream_tasks: HashMap::new(),
            rx,
            tx,
            buffered_frames: VecDeque::new(),
            subscribe_deadline: Duration::from_secs(60),
            subscribe_initial_backoff: Duration::from_millis(100),
            subscribe_max_backoff: Duration::from_secs(2),
            last_call_stream: None,
        })
    }

    pub fn conn_id(&self) -> &str {
        &self.conn_id
    }

    pub fn init_result(&self) -> Option<&Value> {
        self.init_result.as_ref()
    }

    pub fn last_call_stream(&self) -> Option<&StreamKey> {
        self.last_call_stream.as_ref()
    }

    pub fn set_subscribe_deadline(&mut self, deadline: Duration) {
        self.subscribe_deadline = deadline;
    }

    pub fn set_subscribe_backoff(&mut self, initial: Duration, max: Duration) {
        self.subscribe_initial_backoff = initial;
        self.subscribe_max_backoff = max;
    }

    pub fn buffered_frame_count(&self) -> usize {
        self.buffered_frames.len()
    }

    /// Sends a JSON-RPC request frame with a monotonic integer id.
    /// Expects HTTP 202 Accepted.
    pub async fn request(
        &self,
        method: &str,
        params: Value,
        session: Option<&str>,
    ) -> anyhow::Result<Value> {
        let id_num = self.next_request_id.fetch_add(1, Ordering::SeqCst);
        let id = Value::from(id_num);
        let frame = serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });

        let mut req = self
            .client
            .post(self.url.clone())
            .header("x-secret-key", &self.secret)
            .header("acp-connection-id", &self.conn_id)
            .header("content-type", "application/json");

        if let Some(sid) = session {
            req = req.header("acp-session-id", sid);
        }

        let res = req
            .json(&frame)
            .send()
            .await
            .with_context(|| format!("failed to send POST request '{method}'"))?;

        let status = res.status();
        self.rec.record(
            Direction::Out,
            "post",
            Some(status.as_u16()),
            Some(frame),
            None,
        );

        if status != StatusCode::ACCEPTED {
            let err_text = res.text().await.unwrap_or_default();
            anyhow::bail!("request '{method}' failed with status {status}: {err_text}");
        }

        Ok(id)
    }

    /// Sends a JSON-RPC notification frame (no `id`).
    /// Expects HTTP 202 Accepted.
    pub async fn notify(
        &self,
        method: &str,
        params: Value,
        session: Option<&str>,
    ) -> anyhow::Result<()> {
        let frame = serde_json::json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params,
        });

        let mut req = self
            .client
            .post(self.url.clone())
            .header("x-secret-key", &self.secret)
            .header("acp-connection-id", &self.conn_id)
            .header("content-type", "application/json");

        if let Some(sid) = session {
            req = req.header("acp-session-id", sid);
        }

        let res = req
            .json(&frame)
            .send()
            .await
            .with_context(|| format!("failed to send POST notification '{method}'"))?;

        let status = res.status();
        self.rec.record(
            Direction::Out,
            "post",
            Some(status.as_u16()),
            Some(frame),
            None,
        );

        if status != StatusCode::ACCEPTED {
            let err_text = res.text().await.unwrap_or_default();
            anyhow::bail!("notification '{method}' failed with status {status}: {err_text}");
        }

        Ok(())
    }

    /// Sends a JSON-RPC response frame back to the server.
    /// Expects HTTP 202 Accepted.
    pub async fn respond(
        &self,
        id: Value,
        result: Value,
        session: Option<&str>,
    ) -> anyhow::Result<()> {
        let frame = serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": result,
        });

        let mut req = self
            .client
            .post(self.url.clone())
            .header("x-secret-key", &self.secret)
            .header("acp-connection-id", &self.conn_id)
            .header("content-type", "application/json");

        if let Some(sid) = session {
            req = req.header("acp-session-id", sid);
        }

        let res = req
            .json(&frame)
            .send()
            .await
            .with_context(|| "failed to send POST respond")?;

        let status = res.status();
        self.rec.record(
            Direction::Out,
            "post",
            Some(status.as_u16()),
            Some(frame),
            None,
        );

        if status != StatusCode::ACCEPTED {
            let err_text = res.text().await.unwrap_or_default();
            anyhow::bail!("respond failed with status {status}: {err_text}");
        }

        Ok(())
    }

    /// Subscribes to the connection stream (`session == None`) or a session stream (`session == Some(...)`).
    ///
    /// On HTTP 409 Conflict, retries with backoff up to `subscribe_deadline`.
    /// Spawns a background task that streams parsed JSON frames into the client receiver.
    pub async fn subscribe(&mut self, session: Option<&str>) -> anyhow::Result<()> {
        let stream_key = session
            .map(|s| StreamKey::Session(s.to_string()))
            .unwrap_or(StreamKey::Conn);

        // Cancel previous task for this key if one was active
        if let Some(prev) = self.stream_tasks.remove(&stream_key) {
            prev.abort();
        }

        let stream_name = stream_key.stream_record_name();
        let start = Instant::now();
        let mut backoff = self.subscribe_initial_backoff;

        loop {
            let mut req = self
                .client
                .get(self.url.clone())
                .header("accept", "text/event-stream")
                .header("x-secret-key", &self.secret)
                .header("acp-connection-id", &self.conn_id);

            if let Some(sid) = session {
                req = req.header("acp-session-id", sid);
            }

            let res = req
                .send()
                .await
                .with_context(|| format!("failed to send GET request for {stream_name}"))?;

            let status = res.status();
            if status == StatusCode::CONFLICT {
                self.rec.record_marker(
                    &stream_name,
                    format!("409 Conflict subscribing to {stream_name}"),
                );
                if start.elapsed() >= self.subscribe_deadline {
                    anyhow::bail!(
                        "timed out after {:?} waiting for 409 conflict to resolve on {stream_name}",
                        self.subscribe_deadline
                    );
                }
                tokio::time::sleep(backoff).await;
                backoff = std::cmp::min(backoff * 2, self.subscribe_max_backoff);
                continue;
            }

            if !status.is_success() {
                self.rec.record(
                    Direction::Marker,
                    &stream_name,
                    Some(status.as_u16()),
                    None,
                    Some(format!("GET stream failed with status {status}")),
                );
                anyhow::bail!("GET stream for {stream_name} failed with status {status}");
            }

            self.rec
                .record_marker(&stream_name, format!("subscribed to {stream_name}"));

            let sse_stream = crate::sse::sse_to_json_stream(res.bytes_stream());
            let tx = self.tx.clone();
            let rec = self.rec.clone();
            let key = stream_key.clone();
            let s_name = stream_name.clone();

            let handle = tokio::spawn(async move {
                let mut stream = sse_stream;
                while let Some(item) = stream.next().await {
                    match item {
                        Ok(val) => {
                            rec.record_in(&s_name, val.clone());
                            if tx.send((key.clone(), val)).is_err() {
                                break;
                            }
                        }
                        Err(err) => {
                            rec.record_marker(&s_name, format!("stream error: {err}"));
                            break;
                        }
                    }
                }
                rec.record_marker(&s_name, format!("stream {s_name} ended"));
            });

            self.stream_tasks.insert(stream_key, handle);
            return Ok(());
        }
    }

    /// Reads the next frame, checking buffered frames first, then waiting on SSE streams up to `timeout`.
    /// Returns `Ok(None)` if `timeout` expires or streams are closed.
    pub async fn next(&mut self, timeout: Duration) -> anyhow::Result<Option<(StreamKey, Value)>> {
        if let Some(buffered) = self.buffered_frames.pop_front() {
            return Ok(Some(buffered));
        }

        match tokio::time::timeout(timeout, self.rx.recv()).await {
            Ok(Some(item)) => Ok(Some(item)),
            Ok(None) => Ok(None),
            Err(_) => Ok(None),
        }
    }

    /// Convenience method: sends a request and waits on `next()` for the matching response id.
    ///
    /// Any non-matching frames received while waiting are buffered into internal storage and
    /// drained by subsequent calls to `next()` or `call()`.
    /// Returns the `"result"` field of the response, or fails if the response contains an `"error"`.
    pub async fn call(
        &mut self,
        method: &str,
        params: Value,
        session: Option<&str>,
        timeout: Duration,
    ) -> anyhow::Result<Value> {
        let (stream_key, frame) = self.call_full(method, params, session, timeout).await?;
        self.last_call_stream = Some(stream_key);

        if let Some(err) = frame.get("error") {
            anyhow::bail!("RPC error calling '{method}': {err}");
        }

        if let Some(result) = frame.get("result") {
            Ok(result.clone())
        } else {
            Ok(frame)
        }
    }

    /// Like [`call`](Self::call), but returns both the [`StreamKey`] the reply arrived on
    /// and the full JSON-RPC response frame.
    pub async fn call_full(
        &mut self,
        method: &str,
        params: Value,
        session: Option<&str>,
        timeout: Duration,
    ) -> anyhow::Result<(StreamKey, Value)> {
        let req_id = self.request(method, params, session).await?;
        let deadline = Instant::now() + timeout;
        let mut unmatched = Vec::new();

        loop {
            let now = Instant::now();
            if now >= deadline {
                for item in unmatched.into_iter().rev() {
                    self.buffered_frames.push_front(item);
                }
                anyhow::bail!("timed out waiting for response to request id {req_id}");
            }
            let remaining = deadline - now;
            match self.next(remaining).await {
                Ok(Some((stream_key, frame))) => {
                    if frame.get("id") == Some(&req_id) {
                        for item in unmatched.into_iter().rev() {
                            self.buffered_frames.push_front(item);
                        }
                        return Ok((stream_key, frame));
                    } else {
                        unmatched.push((stream_key, frame));
                    }
                }
                Ok(None) => {
                    for item in unmatched.into_iter().rev() {
                        self.buffered_frames.push_front(item);
                    }
                    anyhow::bail!(
                        "stream ended or timed out waiting for response to request id {req_id}"
                    );
                }
                Err(err) => {
                    for item in unmatched.into_iter().rev() {
                        self.buffered_frames.push_front(item);
                    }
                    return Err(err);
                }
            }
        }
    }

    /// Aborts all running SSE stream tasks without sending a DELETE request to the server.
    pub fn hard_drop(&mut self) {
        for (_, handle) in self.stream_tasks.drain() {
            handle.abort();
        }
        self.rec
            .record_marker("conn", "hard_drop: aborted all SSE stream tasks");
    }

    /// Terminates the connection via DELETE /acp.
    pub async fn delete(mut self) -> anyhow::Result<StatusCode> {
        self.hard_drop();
        let res = self
            .client
            .delete(self.url.clone())
            .header("x-secret-key", &self.secret)
            .header("acp-connection-id", &self.conn_id)
            .send()
            .await
            .with_context(|| "failed to send DELETE /acp")?;

        let status = res.status();
        self.rec.record(
            Direction::Out,
            "post",
            Some(status.as_u16()),
            None,
            Some("DELETE /acp".into()),
        );

        Ok(status)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resume_refuses_production_port() {
        let res = Upstream::resume(
            "http://127.0.0.1:4080/acp",
            "secret".into(),
            "conn-1".into(),
            Recorder::disabled(),
        );
        assert!(res.is_err());
    }

    #[tokio::test]
    async fn initialize_refuses_production_port() {
        let res = Upstream::initialize(
            "http://127.0.0.1:4080/acp",
            "secret".into(),
            Recorder::disabled(),
        )
        .await;
        assert!(res.is_err());
    }

    #[test]
    fn stream_key_record_names() {
        assert_eq!(StreamKey::Conn.stream_record_name(), "conn");
        assert_eq!(
            StreamKey::Session("s123".into()).stream_record_name(),
            "session:s123"
        );
    }

    #[tokio::test]
    async fn next_drains_buffered_frames_first() {
        let mut upstream = Upstream::resume(
            "http://127.0.0.1:4180/acp",
            "secret".into(),
            "conn-test".into(),
            Recorder::disabled(),
        )
        .expect("resume ok");

        upstream
            .buffered_frames
            .push_back((StreamKey::Conn, serde_json::json!({"buffered": 1})));
        upstream.buffered_frames.push_back((
            StreamKey::Session("s1".into()),
            serde_json::json!({"buffered": 2}),
        ));

        let first = upstream
            .next(Duration::from_millis(50))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(first.0, StreamKey::Conn);
        assert_eq!(first.1, serde_json::json!({"buffered": 1}));

        let second = upstream
            .next(Duration::from_millis(50))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(second.0, StreamKey::Session("s1".into()));
        assert_eq!(second.1, serde_json::json!({"buffered": 2}));

        let third = upstream.next(Duration::from_millis(10)).await.unwrap();
        assert!(third.is_none());
    }

    #[tokio::test]
    async fn hard_drop_aborts_tasks_and_records_marker() {
        let mut upstream = Upstream::resume(
            "http://127.0.0.1:4180/acp",
            "secret".into(),
            "conn-test".into(),
            Recorder::disabled(),
        )
        .expect("resume ok");

        let handle = tokio::spawn(async {
            tokio::time::sleep(Duration::from_secs(10)).await;
        });
        upstream.stream_tasks.insert(StreamKey::Conn, handle);

        upstream.hard_drop();
        assert!(upstream.stream_tasks.is_empty());
    }
}
