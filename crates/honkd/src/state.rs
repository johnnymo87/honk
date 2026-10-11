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
}

impl State {
    pub fn prune_ledger(&mut self, now_unix: u64, ttl_secs: u64, max_entries: usize) {
        self.ledger.retain(|_, entry| match entry {
            LedgerEntry::Done { done_unix, .. } => now_unix.saturating_sub(*done_unix) <= ttl_secs,
            LedgerEntry::InFlight { .. } => true,
        });

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
}

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

    pub fn prune_ledger(&mut self, now_unix: u64, ttl_secs: u64, max_entries: usize) -> Result<()> {
        self.mutate(|st| st.prune_ledger(now_unix, ttl_secs, max_entries))
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
    fn prune_keeps_inflight() {
        let mut st = State::default();
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
        st.prune_ledger(100_000, 24 * 3600, 1000);
        assert!(!st.ledger.contains_key("old"));
        assert!(st.ledger.contains_key("live"));

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
        st.prune_ledger(100_000, 24 * 3600, 2);
        assert!(st.ledger.contains_key("live"));
        assert!(st.ledger.contains_key("live2"));
        assert_eq!(st.ledger.len(), 2);

        // If max_entries = 1, excess is 1, but all are InFlight so none can be dropped!
        st.prune_ledger(100_000, 24 * 3600, 1);
        assert!(st.ledger.contains_key("live"));
        assert!(st.ledger.contains_key("live2"));
        assert_eq!(st.ledger.len(), 2);
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
