use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};

#[derive(Serialize, Deserialize, Default, Debug, Clone, PartialEq)]
pub struct State {
    pub version: u32,
    pub boot: u64,
    /// I7: a state dir belongs to one upstream; refuse to start on mismatch.
    pub upstream_url: Option<String>,
    pub control_conn: Option<String>,
    pub orphan_conns: Vec<String>, // A15
    pub sessions: BTreeMap<String, HeldSession>,
    /// Last epoch of sessions no longer held (evicted / gone), so the next activation is epoch+1.
    pub retired_epochs: BTreeMap<String, u64>,
    pub ledger: BTreeMap<String, LedgerEntry>, // client_req_id → entry
    /// Why each session's last turn failed, until a later turn starts or ends
    /// clean. Held or not, so the pickers can show it (honk-9qf.15.11.3).
    /// The default is load-bearing: a state.json from before this field must
    /// still load.
    #[serde(default)]
    pub turn_errors: BTreeMap<String, TurnErrorRecord>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct TurnErrorRecord {
    /// goose's `_meta.goose.turnError.kind`, or honkd's `rpc` / `interrupted`.
    pub kind: String,
    pub message: String,
    pub at_unix: u64,
}

/// `session/list` reports a turn error only this long: an abandoned session
/// whose last turn failed must not stay pinned in the pickers forever.
pub const TURN_ERROR_LIST_TTL_SECS: u64 = 48 * 3600;
/// Boot drops turn errors older than this.
pub const TURN_ERROR_PRUNE_SECS: u64 = 7 * 24 * 3600;

#[derive(Debug, Clone, PartialEq)]
pub struct SidecarRecoveryInfo {
    pub crid: String,
    pub session: String,
    pub user_text: Option<String>,
    pub prompt_index: Option<usize>,
}

impl State {
    pub fn prune_turn_errors(&mut self, now_unix: u64) {
        self.turn_errors
            .retain(|_, e| now_unix.saturating_sub(e.at_unix) <= TURN_ERROR_PRUNE_SECS);
    }

    /// The session's turn error if it is recent enough to report.
    pub fn listed_turn_error(&self, sid: &str, now_unix: u64) -> Option<&TurnErrorRecord> {
        self.turn_errors
            .get(sid)
            .filter(|e| now_unix.saturating_sub(e.at_unix) <= TURN_ERROR_LIST_TTL_SECS)
    }

    pub fn prune_ledger(
        &mut self,
        now_unix: u64,
        ttl_secs: u64,
        max_entries: usize,
        state_dir: Option<&std::path::Path>,
    ) -> Vec<SidecarRecoveryInfo> {
        let sessions = &self.sessions;
        let mut to_resolve_lost = Vec::new();

        self.ledger.retain(|crid, entry| match entry {
            LedgerEntry::Done { done_unix, .. } => now_unix.saturating_sub(*done_unix) <= ttl_secs,
            LedgerEntry::InFlight {
                session,
                upstream_id,
            } => {
                if state_dir.is_none() {
                    // Runtime prune: do not resolve InFlight entries to lost-track.
                    // The prompt-time inconsistent-inflight check covers runtime.
                    return sessions.contains_key(session);
                }
                if let Some(held) = sessions.get(session) {
                    if let Some(inf) = &held.inflight {
                        if inf.client_req_ids.contains(crid) {
                            true
                        } else {
                            to_resolve_lost.push((
                                crid.clone(),
                                session.clone(),
                                upstream_id.clone(),
                            ));
                            true
                        }
                    } else {
                        to_resolve_lost.push((crid.clone(), session.clone(), upstream_id.clone()));
                        true
                    }
                } else {
                    false // session not held -> prune
                }
            }
        });

        let mut resolved_from_sidecar = Vec::new();
        for (crid, session, upstream_id) in to_resolve_lost {
            // Check if replies sidecar has cached reply for this upstream_id
            let cached_reply = state_dir.and_then(|dir| {
                let sidecar_path = dir
                    .join("sessions")
                    .join(format!("{session}.replies.jsonl"));
                let Ok(content) = std::fs::read_to_string(sidecar_path) else {
                    return None;
                };
                for line in content.lines() {
                    if let Ok(v) = serde_json::from_str::<Value>(line)
                        && let Some(uid) = v.get("upstream_id").and_then(Value::as_str)
                        && uid == upstream_id
                        && let Some(reply) = v.get("reply")
                    {
                        let user_text =
                            v.get("user_text").and_then(Value::as_str).map(String::from);
                        let prompt_index = v
                            .get("prompt_index")
                            .and_then(Value::as_u64)
                            .map(|n| n as usize);
                        return Some((reply.clone(), user_text, prompt_index));
                    }
                }
                None
            });

            if let Some((reply, user_text, prompt_index)) = cached_reply {
                resolved_from_sidecar.push(SidecarRecoveryInfo {
                    crid: crid.clone(),
                    session: session.clone(),
                    user_text,
                    prompt_index,
                });
                self.ledger.insert(
                    crid,
                    LedgerEntry::Done {
                        session,
                        response: reply,
                        done_unix: now_unix,
                    },
                );
            } else {
                self.ledger.insert(
                    crid,
                    LedgerEntry::Done {
                        session,
                        response: crate::rpc::error(
                            Value::Null,
                            -32000,
                            "honkd lost track of this client_req_id; not resending",
                            None,
                        ),
                        done_unix: now_unix,
                    },
                );
            }
        }

        if self.ledger.len() > max_entries {
            let mut done_keys: Vec<(String, u64)> = self
                .ledger
                .iter()
                .filter_map(|(k, entry)| match entry {
                    LedgerEntry::Done { done_unix, .. } => Some((k.clone(), *done_unix)),
                    LedgerEntry::InFlight { .. } => None,
                })
                .collect();

            done_keys.sort_by_key(|(_, done_unix)| *done_unix);

            let excess = self.ledger.len().saturating_sub(max_entries);
            for (k, _) in done_keys.into_iter().take(excess) {
                self.ledger.remove(&k);
            }
        }

        resolved_from_sidecar
    }

    pub fn insert_done_entry(
        &mut self,
        crid: String,
        session: String,
        response: Value,
        done_unix: u64,
    ) {
        self.ledger.insert(
            crid,
            LedgerEntry::Done {
                session,
                response,
                done_unix,
            },
        );
        self.prune_ledger(
            done_unix,
            LEDGER_PRUNE_TTL_SECS,
            LEDGER_PRUNE_MAX_ENTRIES,
            None,
        );
    }
}

#[derive(Serialize, Deserialize, Default, Debug, Clone, PartialEq)]
pub struct HeldSession {
    pub conn_id: String,
    pub cwd: String,
    pub epoch: u64,                      // connection generation
    pub last_conn_event_id: Option<u64>, // conn stream; the session stream's id lives in the log
    pub pending: Vec<PendingRequest>,
    pub inflight: Option<InflightPrompt>,
    pub last_active_unix: u64,
    #[serde(default)]
    pub init_result: Option<Value>,
    /// Number of client user prompts already delivered to goose for this session.
    /// Maintained from export transcript at activation/rebuild (or 0 at session/new).
    /// Incremented when honkd's prompt is known delivered (run announced or reply received).
    #[serde(default)]
    pub client_prompts: usize,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct PendingRequest {
    pub id: Value,
    pub method: String,
    pub params: Value,
    pub received_unix: u64,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct InflightPrompt {
    pub upstream_id: String,
    pub client_req_ids: Vec<String>,
    pub params: Value,
    pub announced: bool,
    /// Expected index in goose's export `client_prompts()` list.
    /// Set to `HeldSession.client_prompts` when the prompt is dispatched.
    #[serde(default)]
    pub prompt_index: usize,
    #[serde(default)]
    pub run_id: Option<String>,
}

/// Boot-time ledger prune policy:
/// - Done entries older than 24 hours (86,400s) are dropped.
/// - If still more than 1,000 entries remain, the oldest excess Done entries are dropped.
/// - InFlight entries are never dropped by age or count.
pub const LEDGER_PRUNE_TTL_SECS: u64 = 24 * 3600;
pub const LEDGER_PRUNE_MAX_ENTRIES: usize = 1000;

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum LedgerEntry {
    InFlight {
        session: String,
        upstream_id: String,
    },
    Done {
        session: String,
        response: Value,
        done_unix: u64,
    },
}

impl LedgerEntry {
    pub fn session(&self) -> &str {
        match self {
            LedgerEntry::InFlight { session, .. } => session,
            LedgerEntry::Done { session, .. } => session,
        }
    }
}

fn save_atomic(dir: &Path, state: &State) -> Result<()> {
    let tmp_path = dir.join(".state.json.tmp");
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut file = opts.open(&tmp_path)?;
    let data = serde_json::to_vec_pretty(state)?;
    file.write_all(&data)?;
    file.write_all(b"\n")?;
    file.sync_all()?;
    drop(file);

    let target_path = dir.join("state.json");
    std::fs::rename(&tmp_path, &target_path)?;

    #[cfg(unix)]
    {
        let dir_file = std::fs::File::open(dir)?;
        dir_file.sync_all()?;
    }
    Ok(())
}

#[derive(Debug)]
pub struct StateStore {
    dir: PathBuf,
    state: State,
    _lock_file: std::fs::File,
}

impl std::ops::Deref for StateStore {
    type Target = State;
    fn deref(&self) -> &Self::Target {
        &self.state
    }
}

impl StateStore {
    pub fn open(dir: &Path, upstream_url: Option<&str>, discard_corrupt: bool) -> Result<Self> {
        std::fs::create_dir_all(dir)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
        }

        let lock_path = dir.join("lock");
        let mut lock_opts = std::fs::OpenOptions::new();
        lock_opts
            .read(true)
            .write(true)
            .create(true)
            .truncate(false);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            lock_opts.mode(0o600);
        }
        let lock_file = lock_opts.open(&lock_path)?;

        match fs4::FileExt::try_lock(&lock_file) {
            Ok(()) => {}
            Err(fs4::TryLockError::WouldBlock) => {
                anyhow::bail!("another honkd owns {}", dir.display());
            }
            Err(fs4::TryLockError::Error(e)) => {
                return Err(anyhow::anyhow!(
                    "failed to lock {}: {e}",
                    lock_path.display()
                ));
            }
        }

        let state_path = dir.join("state.json");
        let mut state = match std::fs::read_to_string(&state_path) {
            Ok(contents) => match serde_json::from_str::<State>(&contents) {
                Ok(s) => s,
                Err(e) if discard_corrupt => {
                    let now = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_secs();
                    let mut corrupt_path = dir.join(format!("state.json.corrupt-{now}"));
                    if corrupt_path.exists() {
                        let nanos = std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .unwrap_or_default()
                            .subsec_nanos();
                        corrupt_path = dir.join(format!("state.json.corrupt-{now}-{nanos}"));
                    }
                    std::fs::rename(&state_path, &corrupt_path)?;
                    eprintln!(
                        "WARNING: state file {} is corrupt ({e}); moved to {}",
                        state_path.display(),
                        corrupt_path.display()
                    );
                    State::default()
                }
                Err(e) => {
                    anyhow::bail!(
                        "state file {} is corrupt: {e}; refusing to start",
                        state_path.display()
                    );
                }
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => State::default(),
            Err(e) => return Err(e.into()),
        };

        if let Some(existing) = &state.upstream_url {
            if let Some(target) = upstream_url
                && target != existing
            {
                anyhow::bail!("state dir belongs to {existing}");
            }
        } else if let Some(target) = upstream_url {
            state.upstream_url = Some(target.to_string());
        }

        state.version = 1;
        state.boot += 1;
        state.prune_turn_errors(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs(),
        );
        save_atomic(dir, &state)?;

        Ok(Self {
            dir: dir.to_path_buf(),
            state,
            _lock_file: lock_file,
        })
    }

    pub fn state(&self) -> &State {
        &self.state
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn mutate<R>(&mut self, f: impl FnOnce(&mut State) -> R) -> Result<R> {
        let mut cloned = self.state.clone();
        let result = f(&mut cloned);
        save_atomic(&self.dir, &cloned)?;
        self.state = cloned;
        Ok(result)
    }

    pub fn prune_ledger(
        &mut self,
        now_unix: u64,
        ttl_secs: u64,
        max_entries: usize,
    ) -> Result<Vec<SidecarRecoveryInfo>> {
        let dir = self.dir.clone();
        self.mutate(|st| st.prune_ledger(now_unix, ttl_secs, max_entries, Some(&dir)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn missing_file_loads_default_and_boot_increments_on_open() {
        let d = tempfile::tempdir().unwrap();
        let s1 = StateStore::open(d.path(), None, false).unwrap();
        assert_eq!(s1.boot, 1);
        drop(s1);
        let s2 = StateStore::open(d.path(), None, false).unwrap();
        assert_eq!(s2.boot, 2);
    }

    #[test]
    fn mutate_is_durable_and_file_is_0600() {
        let d = tempfile::tempdir().unwrap();
        let mut s = StateStore::open(d.path(), None, false).unwrap();
        s.mutate(|st| {
            st.sessions.insert(
                "s1".into(),
                HeldSession {
                    conn_id: "c1".into(),
                    cwd: "/tmp/x".into(),
                    ..Default::default()
                },
            );
        })
        .unwrap();
        let raw = std::fs::read_to_string(d.path().join("state.json")).unwrap();
        assert!(raw.contains("\"c1\""));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(d.path().join("state.json"))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
            assert_eq!(
                std::fs::metadata(d.path()).unwrap().permissions().mode() & 0o777,
                0o700
            );
        }
        drop(s);
        let reopened = StateStore::open(d.path(), None, false).unwrap();
        assert_eq!(reopened.sessions["s1"].conn_id, "c1");
    }

    fn now() -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs()
    }

    #[test]
    fn state_json_from_before_turn_errors_still_loads() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(
            d.path().join("state.json"),
            r#"{"version":1,"boot":3,"upstream_url":null,"control_conn":null,
                "orphan_conns":[],"sessions":{},"retired_epochs":{},"ledger":{}}"#,
        )
        .unwrap();
        let s = StateStore::open(d.path(), None, false).unwrap();
        assert_eq!(s.boot, 4);
        assert!(s.turn_errors.is_empty());
    }

    #[test]
    fn turn_errors_survive_reopen_and_boot_prunes_week_old_ones() {
        let d = tempfile::tempdir().unwrap();
        let mut s = StateStore::open(d.path(), None, false).unwrap();
        let record = |at_unix| TurnErrorRecord {
            kind: "network".into(),
            message: "connection reset".into(),
            at_unix,
        };
        s.mutate(|st| {
            st.turn_errors.insert("recent".into(), record(now() - 3600));
            st.turn_errors
                .insert("old".into(), record(now() - TURN_ERROR_PRUNE_SECS - 60));
        })
        .unwrap();
        drop(s);
        let reopened = StateStore::open(d.path(), None, false).unwrap();
        assert_eq!(
            reopened.turn_errors.keys().collect::<Vec<_>>(),
            vec!["recent"]
        );
    }

    #[test]
    fn listed_turn_error_expires_after_the_list_ttl() {
        let mut st = State::default();
        st.turn_errors.insert(
            "s1".into(),
            TurnErrorRecord {
                kind: "provider".into(),
                message: "HTTP 500".into(),
                at_unix: 1_000,
            },
        );
        assert!(st.listed_turn_error("s1", 1_000 + 60).is_some());
        assert!(
            st.listed_turn_error("s1", 1_000 + TURN_ERROR_LIST_TTL_SECS + 1)
                .is_none()
        );
        assert!(st.listed_turn_error("other", 1_000).is_none());
    }

    #[test]
    fn stale_tmp_file_is_ignored() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join(".state.json.tmp"), "{garbage").unwrap();
        assert!(StateStore::open(d.path(), None, false).is_ok());
    }

    #[test]
    fn corrupt_state_refuses_unless_discard_flag() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("state.json"), "{garbage").unwrap();
        assert!(StateStore::open(d.path(), None, false).is_err());
        assert!(StateStore::open(d.path(), None, true).is_ok());
        assert!(std::fs::read_dir(d.path()).unwrap().any(|e| {
            e.unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with("state.json.corrupt-")
        }));
    }

    #[test]
    fn second_open_fails_while_first_alive() {
        let d = tempfile::tempdir().unwrap();
        let s1 = StateStore::open(d.path(), None, false).unwrap();
        let s2 = StateStore::open(d.path(), None, false);
        assert!(s2.is_err());
        assert!(s2.unwrap_err().to_string().contains("another honkd owns"));
        drop(s1);
        let s3 = StateStore::open(d.path(), None, false);
        assert!(s3.is_ok());
    }

    #[test]
    fn upstream_url_mismatch_refuses() {
        let d = tempfile::tempdir().unwrap();
        let s1 = StateStore::open(d.path(), Some("http://127.0.0.1:4181/acp"), false).unwrap();
        drop(s1);
        let s2 = StateStore::open(d.path(), Some("http://127.0.0.1:4180/acp"), false);
        assert!(s2.is_err());
        assert_eq!(
            s2.unwrap_err().to_string(),
            "state dir belongs to http://127.0.0.1:4181/acp"
        );
        let s3 = StateStore::open(d.path(), Some("http://127.0.0.1:4181/acp"), false);
        assert!(s3.is_ok());
    }

    #[test]
    fn test_inflight_prompt_serde_backward_compat() {
        let old_json = serde_json::json!({
            "upstream_id": "hk1-1",
            "client_req_ids": ["c1"],
            "params": {},
            "announced": true,
            "prompt_index": 2
        });
        let inflight: InflightPrompt = serde_json::from_value(old_json).unwrap();
        assert_eq!(inflight.run_id, None);
        assert_eq!(inflight.upstream_id, "hk1-1");
    }

    #[test]
    fn prune_keeps_inflight() {
        let mut st = State::default();
        let held = HeldSession {
            inflight: Some(InflightPrompt {
                upstream_id: "u1".into(),
                client_req_ids: vec!["live".into(), "live2".into()],
                params: json!({}),
                announced: true,
                prompt_index: 0,
                run_id: None,
            }),
            ..Default::default()
        };
        st.sessions.insert("s".into(), held);

        st.ledger.insert(
            "old".into(),
            LedgerEntry::Done {
                session: "s".into(),
                response: json!({}),
                done_unix: 0,
            },
        );
        st.ledger.insert(
            "live".into(),
            LedgerEntry::InFlight {
                session: "s".into(),
                upstream_id: "u1".into(),
            },
        );
        st.ledger.insert(
            "orphan_unheld".into(),
            LedgerEntry::InFlight {
                session: "unheld_ses".into(),
                upstream_id: "u_orphan".into(),
            },
        );
        st.ledger.insert(
            "orphan_mismatched_crid".into(),
            LedgerEntry::InFlight {
                session: "s".into(),
                upstream_id: "u_mismatched".into(),
            },
        );

        st.prune_ledger(100_000, 24 * 3600, 1000, None);
        assert!(!st.ledger.contains_key("old"));
        assert!(st.ledger.contains_key("live"));
        assert!(
            !st.ledger.contains_key("orphan_unheld"),
            "InFlight for unheld session must be pruned"
        );
        assert!(
            matches!(
                st.ledger.get("orphan_mismatched_crid"),
                Some(LedgerEntry::InFlight { .. })
            ),
            "Runtime prune (state_dir == None) must leave InFlight alone"
        );

        // Boot prune (state_dir == Some) resolves mismatched InFlight to Done
        let temp_dir = tempfile::tempdir().unwrap();
        st.prune_ledger(100_000, 24 * 3600, 1000, Some(temp_dir.path()));
        assert!(
            matches!(
                st.ledger.get("orphan_mismatched_crid"),
                Some(LedgerEntry::Done { .. })
            ),
            "Boot prune (state_dir == Some) must resolve mismatched InFlight to Done"
        );

        // When exceeding max_entries, old Done entries are dropped, but never InFlight
        st.ledger.insert(
            "done1".into(),
            LedgerEntry::Done {
                session: "s".into(),
                response: json!({}),
                done_unix: 50_000,
            },
        );
        st.ledger.insert(
            "done2".into(),
            LedgerEntry::Done {
                session: "s".into(),
                response: json!({}),
                done_unix: 60_000,
            },
        );
        st.ledger.insert(
            "live2".into(),
            LedgerEntry::InFlight {
                session: "s".into(),
                upstream_id: "u2".into(),
            },
        );
        // Total entries = 4: 2 InFlight, 2 Done.
        // If max_entries = 2, both Done entries should be dropped, leaving both InFlight
        st.prune_ledger(100_000, 24 * 3600, 2, None);
        assert!(st.ledger.contains_key("live"));
        assert!(st.ledger.contains_key("live2"));
        assert_eq!(st.ledger.len(), 2);

        // If max_entries = 1, excess is 1, but all are InFlight so none can be dropped!
        st.prune_ledger(100_000, 24 * 3600, 1, None);
        assert!(st.ledger.contains_key("live"));
        assert!(st.ledger.contains_key("live2"));
        assert_eq!(st.ledger.len(), 2);
    }

    #[test]
    fn insert_done_entry_prunes_old_entries() {
        let mut st = State::default();
        let held = HeldSession::default();
        st.sessions.insert("s".into(), held);

        let now = 100_000;
        // Insert an entry older than 24h
        st.insert_done_entry("old".into(), "s".into(), json!({}), now - (25 * 3600));
        assert!(st.ledger.contains_key("old"));

        // Inserting a new entry prunes the old one
        st.insert_done_entry("new".into(), "s".into(), json!({}), now);
        assert!(!st.ledger.contains_key("old"));
        assert!(st.ledger.contains_key("new"));
    }

    #[test]
    fn prune_ledger_consults_sidecar_for_inflight_without_held_inflight() {
        let temp_dir = tempfile::tempdir().unwrap();
        let sessions_dir = temp_dir.path().join("sessions");
        std::fs::create_dir_all(&sessions_dir).unwrap();

        // Write a reply in the sidecar for upstream_id "u_completed"
        let sidecar_file = sessions_dir.join("s.replies.jsonl");
        let line = json!({
            "upstream_id": "u_completed",
            "reply": { "result": { "stopReason": "end_turn" } },
            "user_text": "cached prompt",
            "prompt_index": 0
        });
        std::fs::write(&sidecar_file, format!("{line}\n")).unwrap();

        let mut st = State::default();
        // Session is held, but held.inflight is None (e.g. crash between reply and ReplyText export)
        let held = HeldSession::default();
        st.sessions.insert("s".into(), held);

        st.ledger.insert(
            "crid1".into(),
            LedgerEntry::InFlight {
                session: "s".into(),
                upstream_id: "u_completed".into(),
            },
        );
        st.ledger.insert(
            "crid2".into(),
            LedgerEntry::InFlight {
                session: "s".into(),
                upstream_id: "u_other".into(),
            },
        );

        let resolved = st.prune_ledger(100_000, 24 * 3600, 1000, Some(temp_dir.path()));
        assert_eq!(
            resolved,
            vec![SidecarRecoveryInfo {
                crid: "crid1".to_string(),
                session: "s".to_string(),
                user_text: Some("cached prompt".to_string()),
                prompt_index: Some(0),
            }]
        );

        // crid1 matched sidecar reply -> resolved to Done with cached reply!
        match st.ledger.get("crid1") {
            Some(LedgerEntry::Done { response, .. }) => {
                assert_eq!(response["result"]["stopReason"], "end_turn");
                assert!(response["result"].get("text").is_none());
            }
            other => panic!("expected Done with cached reply, got {other:?}"),
        }

        // crid2 did not match sidecar -> resolved to lost track error
        match st.ledger.get("crid2") {
            Some(LedgerEntry::Done { response, .. }) => {
                assert!(response.to_string().contains("lost track"));
            }
            other => panic!("expected Done with lost track error, got {other:?}"),
        }
    }

    #[test]
    fn mutate_rolls_back_in_memory_state_on_save_failure() {
        let d = tempfile::tempdir().unwrap();
        let mut s = StateStore::open(d.path(), None, false).unwrap();
        assert_eq!(s.boot, 1);

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(d.path(), std::fs::Permissions::from_mode(0o500)).unwrap();
            let res = s.mutate(|st| {
                st.boot = 999;
            });
            assert!(res.is_err());
            assert_eq!(s.boot, 1);
            std::fs::set_permissions(d.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        }
    }
}
