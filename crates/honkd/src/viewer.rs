use axum::extract::ws::{CloseFrame, Message, WebSocket};
use futures::{SinkExt, StreamExt};
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::sync::{Notify, mpsc};

use crate::hub::Hub;

pub type ViewerId = u64;

static NEXT_VIEWER_ID: AtomicU64 = AtomicU64::new(1);

#[derive(Clone)]
pub struct ViewerTx {
    tx: mpsc::Sender<Message>,
    replay_tx: mpsc::Sender<ReplayBatch>,
    evict: Arc<Notify>,
}

impl ViewerTx {
    pub fn new(
        tx: mpsc::Sender<Message>,
        replay_tx: mpsc::Sender<ReplayBatch>,
        evict: Arc<Notify>,
    ) -> Self {
        Self {
            tx,
            replay_tx,
            evict,
        }
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

    pub fn send_replay(&self, batch: ReplayBatch) -> bool {
        self.replay_tx.try_send(batch).is_ok()
    }

    pub fn offer(&self, msg: Message) -> bool {
        self.send_frame(msg)
    }
}

pub struct ReplayBatch {
    pub session_id: String,
    pub head: honk_wire::Cursor,
    pub msgs: Vec<Message>,
    pub done_tx: Option<tokio::sync::oneshot::Sender<()>>,
}

fn is_duplicate(msg: &Message, heads: &HashMap<String, honk_wire::Cursor>) -> bool {
    if heads.is_empty() {
        return false;
    }
    let Message::Text(text) = msg else {
        return false;
    };
    let Ok(frame) = serde_json::from_str::<Value>(text) else {
        return false;
    };
    let Some(sid) = frame.pointer("/params/sessionId").and_then(Value::as_str) else {
        return false;
    };
    let Some(head) = heads.get(sid) else {
        return false;
    };
    if let Some(cursor) = honk_wire::frame_cursor(&frame)
        && (cursor.epoch < head.epoch || (cursor.epoch == head.epoch && cursor.seq <= head.seq))
    {
        return true;
    }
    false
}

pub struct Viewer {
    pub id: u64,
    pub is_honk_aware: bool,
    pub attached_sessions: HashSet<String>,
    pub tx: ViewerTx,
}

pub async fn viewer_loop(socket: WebSocket, hub: Arc<Hub>) {
    let viewer_id = NEXT_VIEWER_ID.fetch_add(1, Ordering::SeqCst);
    let (mut ws_sender, mut ws_receiver) = socket.split();
    let (tx, mut rx) = mpsc::channel::<Message>(hub.args.viewer_queue);
    let (replay_tx, mut replay_rx) = mpsc::channel::<ReplayBatch>(16);
    let evict = Arc::new(Notify::new());
    let evict_clone = evict.clone();

    let write_task = tokio::spawn(async move {
        let mut replayed_heads: HashMap<String, honk_wire::Cursor> = HashMap::new();
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
                Some(batch) = replay_rx.recv() => {
                    let mut evicted = false;
                    for msg in batch.msgs {
                        tokio::select! {
                            biased;
                            _ = evict_clone.notified() => {
                                let _ = ws_sender
                                    .send(Message::Close(Some(CloseFrame {
                                        code: 4001,
                                        reason: "queue overflow".into(),
                                    })))
                                    .await;
                                evicted = true;
                                break;
                            }
                            res = ws_sender.send(msg) => {
                                if res.is_err() {
                                    evicted = true;
                                    break;
                                }
                            }
                        }
                    }
                    if evicted {
                        break;
                    }
                    replayed_heads.insert(batch.session_id, batch.head);
                    if let Some(done_tx) = batch.done_tx {
                        let _ = done_tx.send(());
                    }
                }
                msg = rx.recv() => {
                    match msg {
                        Some(m) => {
                            if !is_duplicate(&m, &replayed_heads)
                                && ws_sender.feed(m).await.is_err()
                            {
                                break;
                            }
                            let mut err = false;
                            while let Ok(m) = rx.try_recv() {
                                if !is_duplicate(&m, &replayed_heads)
                                    && ws_sender.feed(m).await.is_err()
                                {
                                    err = true;
                                    break;
                                }
                            }
                            if err || ws_sender.flush().await.is_err() {
                                break;
                            }
                        }
                        None => break,
                    }
                }
            }
        }
    });

    let viewer_tx = ViewerTx::new(tx, replay_tx.clone(), evict);

    let mut viewer = Viewer {
        id: viewer_id,
        is_honk_aware: false,
        attached_sessions: HashSet::new(),
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
                        let err_resp = crate::rpc::error(Value::Null, -32700, "Parse error", None);
                        if !viewer
                            .tx
                            .send_frame(Message::Text(err_resp.to_string().into()))
                        {
                            break;
                        }
                        continue;
                    }
                };

                let kind = crate::rpc::kind(&frame);
                let id = frame.get("id").cloned().unwrap_or(Value::Null);

                match kind {
                    crate::rpc::Kind::Invalid => {
                        let err_resp = crate::rpc::error(id, -32600, "Invalid Request", None);
                        if !viewer
                            .tx
                            .send_frame(Message::Text(err_resp.to_string().into()))
                        {
                            break;
                        }
                        continue;
                    }
                    crate::rpc::Kind::Response => {
                        // Downstream response to a server request issued by a session (A13).
                        let id = frame.get("id").cloned().unwrap_or(Value::Null);
                        if !id.is_null() {
                            let session_tx = if viewer.attached_sessions.len() == 1 {
                                let sid = viewer.attached_sessions.iter().next().unwrap();
                                let sessions = hub.sessions.read().await;
                                sessions.get(sid).map(|h| h.tx.clone())
                            } else if viewer.attached_sessions.len() > 1 {
                                let sid_opt = {
                                    let s = hub.state.lock().await;
                                    let mut found = None;
                                    for sid in &viewer.attached_sessions {
                                        if let Some(h) = s.sessions.get(sid)
                                            && h.pending
                                                .iter()
                                                .any(|p| honk_upstream::rpc_id_matches(&p.id, &id))
                                        {
                                            found = Some(sid.clone());
                                            break;
                                        }
                                    }
                                    found
                                };
                                if let Some(sid) = sid_opt {
                                    let sessions = hub.sessions.read().await;
                                    sessions.get(&sid).map(|h| h.tx.clone())
                                } else {
                                    let sessions = hub.sessions.read().await;
                                    for sid in &viewer.attached_sessions {
                                        if let Some(h) = sessions.get(sid) {
                                            let _ =
                                                h.tx.send(crate::session::SessionMsg::Answer {
                                                    viewer_id: viewer.id,
                                                    frame: frame.clone(),
                                                })
                                                .await;
                                        }
                                    }
                                    None
                                }
                            } else {
                                None
                            };

                            let session_tx = if let Some(tx) = session_tx {
                                Some(tx)
                            } else {
                                let sid_opt = {
                                    let s = hub.state.lock().await;
                                    s.sessions.iter().find_map(|(sid, h)| {
                                        if h.pending
                                            .iter()
                                            .any(|p| honk_upstream::rpc_id_matches(&p.id, &id))
                                        {
                                            Some(sid.clone())
                                        } else {
                                            None
                                        }
                                    })
                                };
                                if let Some(sid) = sid_opt {
                                    match hub.sessions.read().await.get(&sid).map(|h| h.tx.clone())
                                    {
                                        Some(tx) => Some(tx),
                                        None => hub
                                            .get_or_activate_session(&sid)
                                            .await
                                            .ok()
                                            .map(|h| h.tx),
                                    }
                                } else {
                                    None
                                }
                            };

                            if let Some(tx) = session_tx {
                                let _ = tx
                                    .send(crate::session::SessionMsg::Answer {
                                        viewer_id: viewer.id,
                                        frame,
                                    })
                                    .await;
                            }
                        }
                        continue;
                    }
                    crate::rpc::Kind::Request | crate::rpc::Kind::Notification => {}
                }

                let method = frame.get("method").and_then(Value::as_str);
                let params = frame.get("params").cloned().unwrap_or(json!({}));

                if method == Some("$/cancel_request") {
                    // $/cancel_request is dropped per wire facts
                    continue;
                }

                if method == Some("session/close") {
                    // Detach session(s); reply {}; never forwarded; keep socket open
                    let sid_opt = params.get("sessionId").and_then(Value::as_str);
                    if let Some(sid) = sid_opt {
                        viewer.attached_sessions.remove(sid);
                        let session_tx = {
                            let sessions = hub.sessions.read().await;
                            sessions.get(sid).map(|h| h.tx.clone())
                        };
                        if let Some(tx) = session_tx {
                            let _ = tx
                                .send(crate::session::SessionMsg::Detach {
                                    viewer_id: viewer.id,
                                })
                                .await;
                        }
                    } else {
                        for sid in viewer.attached_sessions.drain() {
                            let session_tx = {
                                let sessions = hub.sessions.read().await;
                                sessions.get(&sid).map(|h| h.tx.clone())
                            };
                            if let Some(tx) = session_tx {
                                let _ = tx
                                    .send(crate::session::SessionMsg::Detach {
                                        viewer_id: viewer.id,
                                    })
                                    .await;
                            }
                        }
                    }

                    if !id.is_null() {
                        let resp = crate::rpc::response(id, json!({}));
                        if !viewer.tx.send_frame(Message::Text(resp.to_string().into())) {
                            break;
                        }
                    }
                    continue;
                }

                if method == Some("session/cancel") {
                    let sid = params.get("sessionId").and_then(Value::as_str);
                    if let Some(sid) = sid {
                        let session_tx = {
                            let sessions = hub.sessions.read().await;
                            sessions.get(sid).map(|h| h.tx.clone())
                        };
                        if let Some(tx) = session_tx {
                            let _ = tx.send(crate::session::SessionMsg::Cancel).await;
                        }
                    }
                    if !id.is_null() {
                        let resp = crate::rpc::response(id, json!({}));
                        if !viewer.tx.send_frame(Message::Text(resp.to_string().into())) {
                            break;
                        }
                    }
                    continue;
                }

                if method == Some("initialize") {
                    if params.pointer("/_meta/honk").is_some() {
                        viewer.is_honk_aware = true;
                    }
                    let res = hub.handle_initialize(&params);
                    let resp_frame = crate::rpc::response(id, res);
                    if !viewer
                        .tx
                        .send_frame(Message::Text(resp_frame.to_string().into()))
                    {
                        break;
                    }
                } else if method == Some("session/load") {
                    viewer.is_honk_aware = true;
                    let sid_opt = params.get("sessionId").and_then(Value::as_str);
                    let Some(sid) = sid_opt else {
                        let err_resp = crate::rpc::error(
                            id,
                            -32602,
                            "Missing sessionId in session/load",
                            None,
                        );
                        if !viewer
                            .tx
                            .send_frame(Message::Text(err_resp.to_string().into()))
                        {
                            break;
                        }
                        continue;
                    };

                    let session_tx = {
                        let sessions = hub.sessions.read().await;
                        sessions.get(sid).map(|h| h.tx.clone())
                    };

                    let tx = match session_tx {
                        Some(tx) => tx,
                        None => match hub.get_or_activate_session(sid).await {
                            Ok(h) => h.tx,
                            Err(e) => {
                                let err_resp = crate::rpc::error(id, e.code, e.message, None);
                                if !viewer
                                    .tx
                                    .send_frame(Message::Text(err_resp.to_string().into()))
                                {
                                    break;
                                }
                                continue;
                            }
                        },
                    };

                    let after = honk_wire::honk_meta(&params).after;
                    let (reply_tx, mut reply_rx) = tokio::sync::oneshot::channel();
                    let mut tx = tx;
                    if tx
                        .send(crate::session::SessionMsg::Attach {
                            viewer_id: viewer.id,
                            viewer: viewer.tx.clone(),
                            after: after.clone(),
                            reply: reply_tx,
                        })
                        .await
                        .is_err()
                    {
                        match hub.get_or_activate_session(sid).await {
                            Ok(new_h) => {
                                tx = new_h.tx;
                                let (reply_tx2, reply_rx2) = tokio::sync::oneshot::channel();
                                let _ = tx
                                    .send(crate::session::SessionMsg::Attach {
                                        viewer_id: viewer.id,
                                        viewer: viewer.tx.clone(),
                                        after,
                                        reply: reply_tx2,
                                    })
                                    .await;
                                reply_rx = reply_rx2;
                            }
                            Err(e) => {
                                let err_resp = crate::rpc::error(id, e.code, e.message, None);
                                if !viewer
                                    .tx
                                    .send_frame(Message::Text(err_resp.to_string().into()))
                                {
                                    break;
                                }
                                continue;
                            }
                        }
                    }

                    let snapshot = match reply_rx.await {
                        Ok(s) => s,
                        Err(_) => {
                            let err_resp =
                                crate::rpc::error(id, -32603, "Session actor terminated", None);
                            if !viewer
                                .tx
                                .send_frame(Message::Text(err_resp.to_string().into()))
                            {
                                break;
                            }
                            continue;
                        }
                    };

                    let mut msgs = Vec::new();
                    if snapshot.reset {
                        let reset_frame = json!({
                            "jsonrpc": "2.0",
                            "method": "_honk/reset",
                            "params": {
                                "sessionId": sid,
                                "epoch": snapshot.head.epoch,
                            }
                        });
                        msgs.push(Message::Text(reset_frame.to_string().into()));
                    }

                    for entry in snapshot.entries {
                        msgs.push(Message::Text(entry.frame.to_string().into()));
                    }

                    let mut load_res = snapshot.init_result;
                    if !load_res.is_object() {
                        load_res = json!({ "sessionId": sid });
                    }
                    if let Some(meta) = load_res.get_mut("_meta").and_then(Value::as_object_mut) {
                        meta.insert(
                            "honk".to_string(),
                            json!({
                                "epoch": snapshot.head.epoch,
                                "seq": snapshot.head.seq,
                            }),
                        );
                    } else {
                        load_res["_meta"] = json!({
                            "honk": {
                                "epoch": snapshot.head.epoch,
                                "seq": snapshot.head.seq,
                            }
                        });
                    }
                    let load_reply_frame = crate::rpc::response(id, load_res);
                    msgs.push(Message::Text(load_reply_frame.to_string().into()));

                    let replay_done_frame = json!({
                        "jsonrpc": "2.0",
                        "method": "_honk/replay_done",
                        "params": {
                            "sessionId": sid,
                            "epoch": snapshot.head.epoch,
                            "seq": snapshot.head.seq,
                        }
                    });
                    msgs.push(Message::Text(replay_done_frame.to_string().into()));

                    for p in snapshot.pending {
                        let pending_frame = json!({
                            "jsonrpc": "2.0",
                            "id": p.id,
                            "method": p.method,
                            "params": p.params,
                        });
                        msgs.push(Message::Text(pending_frame.to_string().into()));
                    }

                    let (done_tx, done_rx) = tokio::sync::oneshot::channel();
                    if replay_tx
                        .send(ReplayBatch {
                            session_id: sid.to_string(),
                            head: snapshot.head,
                            msgs,
                            done_tx: Some(done_tx),
                        })
                        .await
                        .is_err()
                    {
                        break;
                    }
                    viewer.attached_sessions.insert(sid.to_string());
                    if done_rx.await.is_err() {
                        break;
                    }
                } else if method == Some("session/new") {
                    match hub.create_session(params).await {
                        Ok(new_res) => {
                            if let Some(sid) = new_res.get("sessionId").and_then(Value::as_str) {
                                viewer.attached_sessions.insert(sid.to_string());
                                let session_tx = {
                                    let sessions = hub.sessions.read().await;
                                    sessions.get(sid).map(|h| h.tx.clone())
                                };

                                if let Some(tx) = session_tx {
                                    let _ = tx
                                        .send(crate::session::SessionMsg::AttachLive {
                                            viewer_id: viewer.id,
                                            viewer: viewer.tx.clone(),
                                            is_honk_aware: viewer.is_honk_aware,
                                        })
                                        .await;
                                }
                            }
                            let resp_frame = crate::rpc::response(id, new_res);
                            if !viewer
                                .tx
                                .send_frame(Message::Text(resp_frame.to_string().into()))
                            {
                                break;
                            }
                        }
                        Err(e) => {
                            let (code, msg) =
                                if e.to_string().contains("honkd: held-session limit reached") {
                                    (-32000, "honkd: held-session limit reached".to_string())
                                } else {
                                    (-32603, format!("Failed to create session: {e:#}"))
                                };
                            let err_resp = crate::rpc::error(id, code, msg, None);
                            if !viewer
                                .tx
                                .send_frame(Message::Text(err_resp.to_string().into()))
                            {
                                break;
                            }
                        }
                    }
                } else if method == Some("session/prompt") {
                    if hub
                        .upstream_unpatched_refused
                        .load(std::sync::atomic::Ordering::SeqCst)
                    {
                        tracing::error!("upstream goose is no longer patched");
                        let err_resp = crate::rpc::error(
                            id,
                            -32000,
                            "upstream goose is no longer patched",
                            None,
                        );
                        if !viewer
                            .tx
                            .send_frame(Message::Text(err_resp.to_string().into()))
                        {
                            break;
                        }
                        continue;
                    }

                    let sid_opt = params.get("sessionId").and_then(Value::as_str);
                    if let Some(sid) = sid_opt {
                        let session_tx = {
                            let sessions = hub.sessions.read().await;
                            sessions.get(sid).map(|h| h.tx.clone())
                        };

                        let tx = match session_tx {
                            Some(tx) => Some(tx),
                            None => match hub.get_or_activate_session(sid).await {
                                Ok(h) => Some(h.tx),
                                Err(e) => {
                                    let err_resp = crate::rpc::error(id, e.code, e.message, None);
                                    if !viewer
                                        .tx
                                        .send_frame(Message::Text(err_resp.to_string().into()))
                                    {
                                        break;
                                    }
                                    continue;
                                }
                            },
                        };

                        if let Some(tx) = tx {
                            // Auto-attach if not already attached
                            if !viewer.attached_sessions.contains(sid) {
                                let _ = tx
                                    .send(crate::session::SessionMsg::AttachLive {
                                        viewer_id: viewer.id,
                                        viewer: viewer.tx.clone(),
                                        is_honk_aware: viewer.is_honk_aware,
                                    })
                                    .await;
                                viewer.attached_sessions.insert(sid.to_string());
                            }

                            if tx
                                .send(crate::session::SessionMsg::Prompt {
                                    viewer_id: viewer.id,
                                    viewer: viewer.tx.clone(),
                                    is_honk_aware: viewer.is_honk_aware,
                                    id: id.clone(),
                                    params: params.clone(),
                                })
                                .await
                                .is_err()
                            {
                                match hub.get_or_activate_session(sid).await {
                                    Ok(new_h) => {
                                        let _ = new_h
                                            .tx
                                            .send(crate::session::SessionMsg::Prompt {
                                                viewer_id: viewer.id,
                                                viewer: viewer.tx.clone(),
                                                is_honk_aware: viewer.is_honk_aware,
                                                id,
                                                params,
                                            })
                                            .await;
                                    }
                                    Err(e) => {
                                        let err_resp =
                                            crate::rpc::error(id, e.code, e.message, None);
                                        if !viewer
                                            .tx
                                            .send_frame(Message::Text(err_resp.to_string().into()))
                                        {
                                            break;
                                        }
                                    }
                                }
                            }
                        }
                    } else {
                        let err_resp = crate::rpc::error(
                            id,
                            -32602,
                            "Missing sessionId in session/prompt",
                            None,
                        );
                        if !viewer
                            .tx
                            .send_frame(Message::Text(err_resp.to_string().into()))
                        {
                            break;
                        }
                    }
                } else if let Some(sid) = params.get("sessionId").and_then(Value::as_str) {
                    let session_tx = {
                        let sessions = hub.sessions.read().await;
                        sessions.get(sid).map(|h| h.tx.clone())
                    };

                    let tx = match session_tx {
                        Some(tx) => Some(tx),
                        None => match hub.get_or_activate_session(sid).await {
                            Ok(h) => Some(h.tx),
                            Err(e) => {
                                let err_resp = crate::rpc::error(id, e.code, e.message, None);
                                if !viewer
                                    .tx
                                    .send_frame(Message::Text(err_resp.to_string().into()))
                                {
                                    break;
                                }
                                continue;
                            }
                        },
                    };

                    if let Some(tx) = tx {
                        // Auto-attach if not already attached
                        if !viewer.attached_sessions.contains(sid) {
                            let _ = tx
                                .send(crate::session::SessionMsg::AttachLive {
                                    viewer_id: viewer.id,
                                    viewer: viewer.tx.clone(),
                                    is_honk_aware: viewer.is_honk_aware,
                                })
                                .await;
                            viewer.attached_sessions.insert(sid.to_string());
                        }

                        if method == Some("_goose/unstable/session/steer") {
                            if tx
                                .send(crate::session::SessionMsg::Steer {
                                    viewer_id: viewer.id,
                                    viewer: viewer.tx.clone(),
                                    id: id.clone(),
                                    params: params.clone(),
                                })
                                .await
                                .is_err()
                            {
                                match hub.get_or_activate_session(sid).await {
                                    Ok(new_h) => {
                                        let _ = new_h
                                            .tx
                                            .send(crate::session::SessionMsg::Steer {
                                                viewer_id: viewer.id,
                                                viewer: viewer.tx.clone(),
                                                id,
                                                params,
                                            })
                                            .await;
                                    }
                                    Err(e) => {
                                        let err_resp =
                                            crate::rpc::error(id, e.code, e.message, None);
                                        if !viewer
                                            .tx
                                            .send_frame(Message::Text(err_resp.to_string().into()))
                                        {
                                            break;
                                        }
                                    }
                                }
                            }
                        } else if tx
                            .send(crate::session::SessionMsg::Forward {
                                viewer_id: viewer.id,
                                viewer: viewer.tx.clone(),
                                id: id.clone(),
                                method: method.unwrap_or("").to_string(),
                                params: params.clone(),
                            })
                            .await
                            .is_err()
                        {
                            match hub.get_or_activate_session(sid).await {
                                Ok(new_h) => {
                                    let _ = new_h
                                        .tx
                                        .send(crate::session::SessionMsg::Forward {
                                            viewer_id: viewer.id,
                                            viewer: viewer.tx.clone(),
                                            id,
                                            method: method.unwrap_or("").to_string(),
                                            params,
                                        })
                                        .await;
                                }
                                Err(e) => {
                                    let err_resp = crate::rpc::error(id, e.code, e.message, None);
                                    if !viewer
                                        .tx
                                        .send_frame(Message::Text(err_resp.to_string().into()))
                                    {
                                        break;
                                    }
                                }
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
                            let err_resp = crate::rpc::error(
                                id,
                                -32603,
                                format!("Control connection error: {e:#}"),
                                None,
                            );
                            if !viewer
                                .tx
                                .send_frame(Message::Text(err_resp.to_string().into()))
                            {
                                break;
                            }
                        }
                    }
                } else {
                    let err_resp =
                        crate::rpc::error(id, -32601, "Method not implemented in A8", None);
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

    for sid in viewer.attached_sessions.drain() {
        let session_tx = {
            let sessions = hub.sessions.read().await;
            sessions.get(&sid).map(|h| h.tx.clone())
        };
        if let Some(tx) = session_tx {
            let _ = tx
                .send(crate::session::SessionMsg::Detach {
                    viewer_id: viewer.id,
                })
                .await;
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
        let (replay_tx, _replay_rx) = mpsc::channel::<ReplayBatch>(1);
        let evict = Arc::new(Notify::new());
        let viewer_tx = ViewerTx::new(tx, replay_tx, evict.clone());

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
