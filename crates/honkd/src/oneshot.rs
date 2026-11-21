//! `_honk/oneshot`: one model call outside any held session (honk-9qf.31.4).
//!
//! honk chat's trimmed `/quote` needs the default model to read text and answer once, without
//! the text landing in the chat session's conversation. goose has no stateless completion over
//! ACP, so this runs a throwaway goose session on a side connection: hidden, no extensions,
//! goose's base system prompt replaced, no model override (goose's configured default). The
//! session is archived afterwards, not deleted: deleting would delete its usage records too, and
//! session-tags counts spend from them (honk-9qf.31.5). With `--session-tags-bin` and a
//! `tagFrom` session, it is tagged with that session's tag. Its id sits in
//! `State::oneshot_sessions` until it is archived, so a honkd that dies mid-call archives it at
//! the next start.

use anyhow::Context;
use honk_upstream::{Event, Recorder, Redactor, SubscribeOpts, Upstream, UpstreamOpts};
use serde_json::{Value, json};
use std::time::Duration;
use tokio::sync::oneshot;
use tokio::time::Instant;

use crate::hub::Hub;

/// How long the call may take, model included, unless the client asks for less.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(90);
pub const MAX_TIMEOUT: Duration = Duration::from_secs(300);
/// After a cancel, how long to wait for goose to end the prompt before cleaning up anyway.
const CANCEL_GRACE: Duration = Duration::from_secs(10);
const SETUP_TIMEOUT: Duration = Duration::from_secs(30);
const CLEANUP_TIMEOUT: Duration = Duration::from_secs(10);
pub const SESSION_TITLE: &str = "honk oneshot";
/// Ends a oneshot session but keeps its messages and usage records (unlike `session/delete`).
pub const ARCHIVE: &str = "_goose/unstable/session/archive";

#[derive(Debug, Clone, PartialEq)]
pub struct OneshotReq {
    pub system: String,
    pub prompt: String,
    /// A goose `thinking_effort` value (off, low, medium, high, max). None keeps goose's.
    pub effort: Option<String>,
    pub timeout: Duration,
    /// The goose session this call serves: its session-tags tag goes on the oneshot session.
    pub tag_from: Option<String>,
}

impl OneshotReq {
    /// Parses `_honk/oneshot` params: `{system, prompt, effort?, timeoutMs?, tagFrom?}`.
    pub fn from_params(params: &Value) -> Result<Self, String> {
        let text = |key: &str| {
            params
                .get(key)
                .and_then(Value::as_str)
                .map(str::to_string)
                .ok_or_else(|| format!("_honk/oneshot needs a string {key}"))
        };
        let prompt = text("prompt")?;
        if prompt.trim().is_empty() {
            return Err("_honk/oneshot prompt is empty".to_string());
        }
        let timeout = params
            .get("timeoutMs")
            .and_then(Value::as_u64)
            .map(Duration::from_millis)
            .unwrap_or(DEFAULT_TIMEOUT)
            .min(MAX_TIMEOUT);
        Ok(Self {
            system: text("system")?,
            prompt,
            effort: params
                .get("effort")
                .and_then(Value::as_str)
                .map(str::to_string),
            timeout,
            tag_from: params
                .get("tagFrom")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .map(str::to_string),
        })
    }
}

/// The agent's answer, and the model goose ran it on (from `session/new`'s config options).
#[derive(Debug, PartialEq)]
pub struct OneshotAnswer {
    pub text: String,
    pub model: Option<String>,
}

#[derive(Debug, PartialEq)]
pub enum OneshotError {
    Cancelled,
    Failed(String),
}

impl Hub {
    /// Runs one prompt on a throwaway session and returns the agent's text. `cancel` firing (or
    /// its sender being dropped) cancels the prompt; cleanup runs on every path.
    pub async fn oneshot(
        &self,
        req: OneshotReq,
        cancel: oneshot::Receiver<()>,
    ) -> Result<OneshotAnswer, OneshotError> {
        let init_opts = UpstreamOpts {
            allow_production: self.args.allow_production_upstream,
            client_capabilities: json!({
                "fs": { "readTextFile": false, "writeTextFile": false },
                "terminal": false
            }),
        };
        let recorder = match &self.args.record {
            Some(path) => Recorder::file(path, Redactor::from_env())
                .map_err(|e| OneshotError::Failed(format!("{e:#}")))?,
            None => Recorder::disabled(),
        };
        let mut up = Upstream::initialize_with(
            &self.upstream_url,
            self.upstream_secret.clone(),
            recorder,
            &init_opts,
        )
        .await
        .map_err(|e| OneshotError::Failed(format!("side connection: {e:#}")))?;
        let conn_id = up.conn_id().to_string();
        {
            let mut s = self.state.lock().await;
            if let Err(e) = s.mutate(|st| st.orphan_conns.push(conn_id.clone())) {
                let _ = up.delete().await;
                return Err(OneshotError::Failed(format!("state: {e:#}")));
            }
        }

        let mut sid: Option<String> = None;
        let mut tagging = None;
        let result = self
            .oneshot_on(&mut up, &mut sid, &mut tagging, req, cancel)
            .await;

        if let Some(sid) = sid {
            let archived = up
                .call(
                    ARCHIVE,
                    json!({ "sessionId": sid }),
                    Some(&sid),
                    CLEANUP_TIMEOUT,
                )
                .await;
            match archived {
                Ok(_) => {
                    let mut s = self.state.lock().await;
                    let _ = s.mutate(|st| st.oneshot_sessions.retain(|x| x != &sid));
                }
                Err(e) => tracing::warn!(%sid, "oneshot session archive failed: {e:#}"),
            }
        }
        if let Some(t) = tagging {
            let _ = tokio::time::timeout(CLEANUP_TIMEOUT, t).await;
        }
        if up.delete().await.is_ok() {
            let mut s = self.state.lock().await;
            let _ = s.mutate(|st| st.orphan_conns.retain(|x| x != &conn_id));
        }
        result
    }

    async fn oneshot_on(
        &self,
        up: &mut Upstream,
        sid_out: &mut Option<String>,
        tagging: &mut Option<tokio::task::JoinHandle<()>>,
        req: OneshotReq,
        mut cancel: oneshot::Receiver<()>,
    ) -> Result<OneshotAnswer, OneshotError> {
        let fail = |what: &str, e: anyhow::Error| OneshotError::Failed(format!("{what}: {e:#}"));
        let deadline = Instant::now() + req.timeout;

        let cwd = self.state_dir.join("oneshot");
        std::fs::create_dir_all(&cwd).map_err(|e| fail("cwd", e.into()))?;
        up.subscribe_with(
            None,
            SubscribeOpts {
                takeover: true,
                resume_after: None,
            },
        )
        .await
        .map_err(|e| fail("subscribe", e))?;
        let new_res = up
            .call(
                "session/new",
                json!({
                    "cwd": cwd.to_string_lossy(),
                    "mcpServers": [],
                    "_meta": {
                        "hidden": true,
                        "sessionTitle": SESSION_TITLE,
                        "enabledExtensions": [],
                    },
                }),
                None,
                SETUP_TIMEOUT,
            )
            .await
            .map_err(|e| fail("session/new", e))?;
        let sid = new_res
            .get("sessionId")
            .and_then(Value::as_str)
            .context("session/new reply has no sessionId")
            .map_err(|e| fail("session/new", e))?
            .to_string();
        let model = new_res
            .get("configOptions")
            .and_then(Value::as_array)
            .and_then(|opts| opts.iter().find(|o| o["id"] == "model"))
            .and_then(|o| o.get("currentValue"))
            .and_then(Value::as_str)
            .map(str::to_string);
        {
            let mut s = self.state.lock().await;
            s.mutate(|st| st.oneshot_sessions.push(sid.clone()))
                .map_err(|e| fail("state", e))?;
        }
        *sid_out = Some(sid.clone());
        if let (Some(bin), Some(from)) = (&self.args.session_tags_bin, &req.tag_from) {
            *tagging = Some(tokio::spawn(tag_like(
                bin.clone(),
                from.clone(),
                sid.clone(),
            )));
        }

        up.subscribe_with(
            Some(&sid),
            SubscribeOpts {
                takeover: true,
                resume_after: Some(0),
            },
        )
        .await
        .map_err(|e| fail("subscribe session", e))?;
        up.call(
            "_goose/unstable/session/system-prompt/set",
            json!({ "sessionId": sid, "mode": "set", "text": req.system }),
            Some(&sid),
            SETUP_TIMEOUT,
        )
        .await
        .map_err(|e| fail("system prompt", e))?;
        if let Some(effort) = &req.effort
            && let Err(e) = up
                .call(
                    "session/set_config_option",
                    json!({ "sessionId": sid, "configId": "thinking_effort", "value": effort }),
                    Some(&sid),
                    SETUP_TIMEOUT,
                )
                .await
        {
            tracing::warn!(%sid, "oneshot thinking_effort {effort} not set: {e:#}");
        }

        // A cancel during setup must not start the (paid) prompt.
        if !matches!(cancel.try_recv(), Err(oneshot::error::TryRecvError::Empty)) {
            return Err(OneshotError::Cancelled);
        }
        let prompt_id = up
            .request(
                "session/prompt",
                json!({ "sessionId": sid, "prompt": [{ "type": "text", "text": req.prompt }] }),
                Some(&sid),
            )
            .await
            .map_err(|e| fail("session/prompt", e))?;

        let mut text = String::new();
        let mut cancelled = false;
        let mut deadline = deadline;
        loop {
            let now = Instant::now();
            if now >= deadline {
                if cancelled {
                    return Err(OneshotError::Cancelled);
                }
                let _ = up
                    .notify("session/cancel", json!({ "sessionId": sid }), Some(&sid))
                    .await;
                return Err(OneshotError::Failed(format!(
                    "no answer within {}s",
                    req.timeout.as_secs()
                )));
            }
            let event = tokio::select! {
                ev = up.next(deadline - now) => ev,
                _ = &mut cancel, if !cancelled => {
                    cancelled = true;
                    let _ = up
                        .notify("session/cancel", json!({ "sessionId": sid }), Some(&sid))
                        .await;
                    deadline = Instant::now() + CANCEL_GRACE;
                    continue;
                }
            };
            match event.map_err(|e| fail("stream", e))? {
                None => continue,
                Some(Event::StreamEnded { key, error }) => {
                    return Err(OneshotError::Failed(format!(
                        "stream {key:?} ended: {}",
                        error.unwrap_or_default()
                    )));
                }
                Some(Event::Frame(_, frame, _)) => {
                    if frame
                        .get("id")
                        .is_some_and(|id| honk_upstream::rpc_id_matches(&prompt_id, id))
                    {
                        if cancelled {
                            return Err(OneshotError::Cancelled);
                        }
                        if let Some(err) = frame.get("error") {
                            let msg = err
                                .get("message")
                                .and_then(Value::as_str)
                                .unwrap_or("error");
                            return Err(OneshotError::Failed(format!("model: {msg}")));
                        }
                        let stop = frame
                            .pointer("/result/stopReason")
                            .and_then(Value::as_str)
                            .unwrap_or("");
                        if stop == "cancelled" {
                            return Err(OneshotError::Cancelled);
                        }
                        // goose reports a provider failure as ordinary agent text ("Ran into
                        // this error: ...") with the error in _meta; that text is no answer.
                        if let Some(err) = frame.pointer("/result/_meta/goose/turnError") {
                            let msg = err
                                .get("message")
                                .and_then(Value::as_str)
                                .map(str::to_string)
                                .unwrap_or_else(|| err.to_string());
                            return Err(OneshotError::Failed(format!("model: {msg}")));
                        }
                        if text.trim().is_empty() {
                            return Err(OneshotError::Failed(format!(
                                "empty answer (stop reason {stop:?})"
                            )));
                        }
                        return Ok(OneshotAnswer { text, model });
                    }
                    if let Some(chunk) = agent_text(&frame, &sid) {
                        text.push_str(chunk);
                    }
                }
            }
        }
    }

    /// Archives oneshot sessions a previous honkd left behind. Best effort, at startup.
    pub async fn sweep_oneshot_sessions(&self) {
        let left: Vec<String> = self.state.lock().await.oneshot_sessions.clone();
        for sid in left {
            match self
                .session_call(&sid, ARCHIVE, json!({ "sessionId": sid }), CLEANUP_TIMEOUT)
                .await
            {
                Ok(_) => {
                    let mut s = self.state.lock().await;
                    let _ = s.mutate(|st| st.oneshot_sessions.retain(|x| x != &sid));
                }
                Err(e) => tracing::warn!(%sid, "sweep of oneshot session failed: {e:#}"),
            }
        }
    }
}

/// Gives goose session `sid` the session-tags tag of goose session `from`. Best effort: an
/// untagged session (an `auto:` tag) or a failing binary leaves `sid` to session-tags' own
/// `auto:goose/honk-oneshot`.
async fn tag_like(bin: std::path::PathBuf, from: String, sid: String) {
    let run = |args: Vec<String>| {
        let bin = bin.clone();
        async move {
            let out = tokio::time::timeout(
                CLEANUP_TIMEOUT,
                tokio::process::Command::new(&bin)
                    .args(&args)
                    .kill_on_drop(true)
                    .output(),
            )
            .await
            .map_err(|_| anyhow::anyhow!("timed out"))??;
            anyhow::ensure!(
                out.status.success(),
                "{}: {}",
                out.status,
                String::from_utf8_lossy(&out.stderr).trim()
            );
            Ok::<_, anyhow::Error>(String::from_utf8_lossy(&out.stdout).into_owned())
        }
    };
    let which = match run(vec!["which".into(), format!("goose:{from}")]).await {
        Ok(out) => out,
        Err(e) => return tracing::warn!(%sid, %from, "session-tags which failed: {e:#}"),
    };
    let Some(tag) = which_tag(&which) else {
        return tracing::debug!(%sid, %from, "oneshot not tagged: {from} has no tag");
    };
    if let Err(e) = run(vec!["set".into(), tag.clone(), format!("goose:{sid}")]).await {
        tracing::warn!(%sid, %tag, "session-tags set failed: {e:#}");
    }
}

/// The tag in `session-tags which` output (`tag<TAB>source<TAB>root<TAB>kind`), unless auto.
fn which_tag(out: &str) -> Option<String> {
    let mut cols = out.lines().next()?.split('\t');
    let tag = cols.next()?.trim();
    let source = cols.next().unwrap_or("").trim();
    (!tag.is_empty() && !tag.starts_with("auto:") && source != "auto").then(|| tag.to_string())
}

fn agent_text<'a>(frame: &'a Value, sid: &str) -> Option<&'a str> {
    if frame.get("method").and_then(Value::as_str) != Some("session/update")
        || frame.pointer("/params/sessionId").and_then(Value::as_str) != Some(sid)
    {
        return None;
    }
    let update = frame.pointer("/params/update")?;
    if update.get("sessionUpdate").and_then(Value::as_str) != Some("agent_message_chunk") {
        return None;
    }
    update.pointer("/content/text").and_then(Value::as_str)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn which_tag_takes_manual_tags_only() {
        assert_eq!(
            which_tag("stall-watcher-runs\tmanual\tgoose:20261111_19\tsession\n"),
            Some("stall-watcher-runs".into())
        );
        assert_eq!(which_tag("auto:goose/x\tauto\tgoose:1\tsession\n"), None);
        assert_eq!(which_tag(""), None);
    }

    #[test]
    fn params_parse_and_cap_timeout() {
        let r = OneshotReq::from_params(&json!({
            "system": "s", "prompt": "p", "effort": "low", "timeoutMs": 999_999_999u64
        }))
        .unwrap();
        assert_eq!(r.system, "s");
        assert_eq!(r.effort.as_deref(), Some("low"));
        assert_eq!(r.timeout, MAX_TIMEOUT);
        assert_eq!(r.tag_from, None);
        let r = OneshotReq::from_params(
            &json!({ "system": "", "prompt": "p", "tagFrom": "20261111_19" }),
        )
        .unwrap();
        assert_eq!(r.timeout, DEFAULT_TIMEOUT);
        assert_eq!(r.tag_from.as_deref(), Some("20261111_19"));
        assert!(OneshotReq::from_params(&json!({ "system": "s" })).is_err());
        assert!(OneshotReq::from_params(&json!({ "system": "s", "prompt": " " })).is_err());
    }

    #[test]
    fn agent_text_only_takes_this_sessions_agent_chunks() {
        let chunk = |sid: &str, kind: &str| {
            json!({ "method": "session/update", "params": { "sessionId": sid,
                "update": { "sessionUpdate": kind, "content": { "type": "text", "text": "x" } } } })
        };
        assert_eq!(
            agent_text(&chunk("a", "agent_message_chunk"), "a"),
            Some("x")
        );
        assert_eq!(agent_text(&chunk("b", "agent_message_chunk"), "a"), None);
        assert_eq!(agent_text(&chunk("a", "agent_thought_chunk"), "a"), None);
        assert_eq!(agent_text(&chunk("a", "user_message_chunk"), "a"), None);
    }
}
