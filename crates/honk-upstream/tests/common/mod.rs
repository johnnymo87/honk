#![allow(dead_code, unused_imports)]

pub mod proxy;

pub const FIXTURES_PATCHED_2: &str = "goose-1.54.0-patched.2";
pub const FIXTURES_PATCHED_3: &str = "goose-1.54.0-patched.3";

pub use std::path::{Path, PathBuf};
pub use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

pub use honk_upstream::{
    Event, Recorder, Redactor, ReplayOutcome, StreamFeatures, StreamKey, SubscribeOpts,
    SubscribeOutcome, Upstream, rpc_id_matches,
};
pub use serde_json::{Value, json};

pub struct TestSession {
    pub upstream: Upstream,
    pub url: String,
    pub secret: String,
    pub conn_id: String,
    pub sid: String,
    pub rec: Recorder,
    pub fixture_path: PathBuf,
    pub temp_dir: tempfile::TempDir,
}

pub fn goose_url() -> String {
    std::env::var("HONK_GOOSE_URL").unwrap_or_else(|_| "http://127.0.0.1:4180/acp".into())
}

pub fn goose_port() -> u16 {
    reqwest::Url::parse(&goose_url())
        .ok()
        .and_then(|u| u.port())
        .unwrap_or(4180)
}

pub fn fixtures_root() -> PathBuf {
    if let Ok(root) = std::env::var("HONK_FIXTURES_ROOT")
        && !root.trim().is_empty()
    {
        return PathBuf::from(root.trim());
    }
    let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    let repo_root = manifest_dir
        .parent()
        .and_then(|p| p.parent())
        .expect("finding repo root");
    repo_root.join("fixtures")
}

pub async fn setup_session_at(
    url: &str,
    fixtures: &str,
    probe_name: &str,
    mode: &str,
) -> anyhow::Result<Option<TestSession>> {
    let secret = match std::env::var("HONK_GOOSE_SECRET") {
        Ok(s) if !s.trim().is_empty() => s.trim().to_string(),
        _ => {
            eprintln!("skipping {probe_name}: HONK_GOOSE_SECRET unset");
            return Ok(None);
        }
    };

    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();

    let fixture_path = fixtures_root()
        .join(fixtures)
        .join(probe_name)
        .join(format!("{ts}.jsonl"));
    if let Some(parent) = fixture_path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let redactor = Redactor::from_env();
    let rec = Recorder::file(&fixture_path, redactor)?;

    let mut upstream = Upstream::initialize(url, secret.clone(), rec.clone()).await?;
    let conn_id = upstream.conn_id().to_string();

    upstream.subscribe(None).await?;

    let temp_dir = tempfile::tempdir()?;
    let cwd = temp_dir.path().to_str().unwrap().to_string();
    let new_res = upstream
        .call(
            "session/new",
            json!({
                "cwd": cwd,
                "mcpServers": []
            }),
            None,
            Duration::from_secs(30),
        )
        .await?;

    let sid = new_res
        .get("sessionId")
        .and_then(|v| v.as_str())
        .expect("session/new reply missing sessionId")
        .to_string();

    upstream.subscribe(Some(&sid)).await?;

    upstream
        .call(
            "session/set_mode",
            json!({
                "sessionId": &sid,
                "modeId": mode
            }),
            Some(&sid),
            Duration::from_secs(30),
        )
        .await?;

    upstream
        .call(
            "session/set_config_option",
            json!({
                "sessionId": &sid,
                "configId": "model",
                "value": "claude-haiku-4-5@20251001"
            }),
            Some(&sid),
            Duration::from_secs(30),
        )
        .await?;

    Ok(Some(TestSession {
        upstream,
        url: url.to_string(),
        secret,
        conn_id,
        sid,
        rec,
        fixture_path,
        temp_dir,
    }))
}

pub async fn setup_session_with_mode(
    probe_name: &str,
    mode: &str,
) -> anyhow::Result<Option<TestSession>> {
    setup_session_at(&goose_url(), FIXTURES_PATCHED_2, probe_name, mode).await
}

pub async fn setup_session(probe_name: &str) -> anyhow::Result<Option<TestSession>> {
    setup_session_with_mode(probe_name, "auto").await
}

pub async fn setup_approve_session(probe_name: &str) -> anyhow::Result<Option<TestSession>> {
    setup_session_with_mode(probe_name, "approve").await
}

pub async fn export_session(upstream: &mut Upstream, sid: &str) -> anyhow::Result<Value> {
    let export_res = upstream
        .call(
            "_goose/unstable/session/export",
            json!({
                "sessionId": sid,
                "format": "json"
            }),
            Some(sid),
            Duration::from_secs(30),
        )
        .await?;

    let data_str = export_res
        .get("data")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("export response missing data string: {export_res}"))?;

    let session_val: Value = serde_json::from_str(data_str)?;
    Ok(session_val)
}

pub fn extract_assistant_text(session_json: &Value) -> String {
    let mut text = String::new();
    if let Some(convo) = session_json.get("conversation").and_then(|v| v.as_array()) {
        for msg in convo.iter().rev() {
            if msg.get("role").and_then(|r| r.as_str()) == Some("assistant")
                && let Some(contents) = msg.get("content").and_then(|c| c.as_array())
            {
                for block in contents {
                    if let Some(t) = block.get("text").and_then(|v| v.as_str()) {
                        text.push_str(t);
                    }
                }
                break;
            }
        }
    }
    text
}

pub fn extract_all_assistant_text(session_json: &Value) -> String {
    let mut text = String::new();
    if let Some(convo) = session_json.get("conversation").and_then(|v| v.as_array()) {
        for msg in convo {
            if msg.get("role").and_then(|r| r.as_str()) == Some("assistant")
                && let Some(contents) = msg.get("content").and_then(|c| c.as_array())
            {
                for block in contents {
                    if let Some(t) = block.get("text").and_then(|v| v.as_str()) {
                        text.push_str(t);
                    }
                }
            }
        }
    }
    text
}

pub fn is_permission_request(frame: &Value) -> bool {
    frame.get("method").and_then(|m| m.as_str()) == Some("session/request_permission")
}

pub fn is_tool_call_chunk(frame: &Value) -> bool {
    let update_type = frame
        .pointer("/params/update/sessionUpdate")
        .and_then(|v| v.as_str());
    update_type == Some("tool_call") || update_type == Some("tool_call_update")
}

pub fn is_run_end(frame: &Value) -> bool {
    if frame
        .pointer("/params/update/sessionUpdate")
        .and_then(|v| v.as_str())
        == Some("session_info_update")
    {
        let active_run = frame.pointer("/params/update/_meta/goose/activeRunId");
        active_run == Some(&Value::Null) || active_run.is_none()
    } else {
        false
    }
}

pub fn is_agent_chunk(frame: &Value) -> bool {
    frame
        .pointer("/params/update/sessionUpdate")
        .and_then(|v| v.as_str())
        == Some("agent_message_chunk")
}

pub fn allow_once_outcome() -> Value {
    json!({
        "outcome": {
            "outcome": "selected",
            "optionId": "allow_once"
        }
    })
}

pub fn chunk_text(frame: &Value) -> Option<&str> {
    if !is_agent_chunk(frame) {
        return None;
    }
    frame
        .pointer("/params/update/content/text")
        .and_then(|v| v.as_str())
}

/// Fails loudly if the sandbox is not running the patched build.
pub fn require_patched(upstream: &Upstream) -> anyhow::Result<()> {
    let features = upstream
        .stream_features()
        .ok_or_else(|| anyhow::anyhow!("no GET /acp has been made yet"))?;
    anyhow::ensure!(
        features.is_patched(),
        "goose at {} does not advertise `Acp-Stream-Features: takeover, replay` (got {features:?}); \
         point HONK_GOOSE_URL at honk-goose-sandbox-patched (port 4181)",
        goose_url()
    );
    Ok(())
}

/// Polls export on a fresh connection until the turn's FINISHED reply is committed
/// (export reads sqlite and works on any connection, A6). Returns the wait.
pub async fn wait_until_turn_committed(
    ts: &TestSession,
    within: Duration,
) -> anyhow::Result<Duration> {
    let t0 = Instant::now();
    let mut probe = Upstream::initialize(&goose_url(), ts.secret.clone(), ts.rec.clone()).await?;
    probe.subscribe(None).await?;
    probe.subscribe(Some(&ts.sid)).await?;
    loop {
        let export = export_session(&mut probe, &ts.sid).await?;
        if extract_all_assistant_text(&export).contains("FINISHED") {
            break;
        }
        anyhow::ensure!(
            t0.elapsed() < within,
            "turn not committed within {within:?}"
        );
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
    let _ = probe.delete().await;
    Ok(t0.elapsed())
}

pub fn is_subsequence(sub: &str, full: &str) -> bool {
    let mut full_chars = full.chars();
    for c in sub.chars() {
        if !full_chars.any(|fc| fc == c) {
            return false;
        }
    }
    true
}

#[derive(Debug, Clone)]
pub struct ProcStats {
    pub threads: Option<u64>,
    pub vm_rss_kb: Option<u64>,
    pub children: Vec<String>,
}

pub fn get_sandbox_unit() -> String {
    std::env::var("HONK_SANDBOX_UNIT").unwrap_or_else(|_| "honk-goose-sandbox".to_string())
}

pub fn get_sandbox_cgroup() -> anyhow::Result<PathBuf> {
    let unit = get_sandbox_unit();
    let output = std::process::Command::new("systemctl")
        .args(["--user", "show", "-p", "ControlGroup", "--value", &unit])
        .output()?;
    let cg = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if cg.is_empty() {
        anyhow::bail!("no ControlGroup found for unit '{unit}'");
    }
    let base = Path::new("/sys/fs/cgroup");
    Ok(base.join(cg.trim_start_matches('/')))
}

fn read_cgroup_procs_recursive(dir: &Path, pids: &mut Vec<u32>) {
    let procs_file = dir.join("cgroup.procs");
    if let Ok(content) = std::fs::read_to_string(&procs_file) {
        for line in content.lines() {
            if let Ok(pid) = line.trim().parse::<u32>() {
                pids.push(pid);
            }
        }
    }
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            if let Ok(ft) = entry.file_type()
                && ft.is_dir()
            {
                read_cgroup_procs_recursive(&entry.path(), pids);
            }
        }
    }
}

pub fn get_sandbox_cgroup_pids() -> anyhow::Result<Vec<u32>> {
    let cg_dir = get_sandbox_cgroup()?;
    let mut pids = Vec::new();
    read_cgroup_procs_recursive(&cg_dir, &mut pids);
    pids.sort_unstable();
    pids.dedup();
    Ok(pids)
}

pub fn sandbox_pids_matching(pattern: &str) -> anyhow::Result<Vec<u32>> {
    let cgroup_pids = get_sandbox_cgroup_pids()?;
    let mut matching = Vec::new();
    for pid in cgroup_pids {
        let cmdline_path = format!("/proc/{pid}/cmdline");
        if let Ok(raw) = std::fs::read(&cmdline_path) {
            let args: Vec<&str> = raw
                .split(|&b| b == 0)
                .filter(|a| !a.is_empty())
                .map(|a| std::str::from_utf8(a).unwrap_or(""))
                .collect();
            // Match exact sleep N command
            if let Some(sec_str) = pattern.strip_prefix("sleep ")
                && let Some(cmd) = args.first()
                && (*cmd == "sleep" || cmd.ends_with("/sleep"))
                && args.get(1) == Some(&sec_str)
            {
                matching.push(pid);
                continue;
            }

            let cmd_str = args.join(" ");
            if cmd_str.contains(pattern) {
                matching.push(pid);
            }
        }
    }
    Ok(matching)
}

pub fn pgrep_user(pattern: &str) -> anyhow::Result<Vec<u32>> {
    sandbox_pids_matching(pattern)
}

pub fn get_goose_sandbox_pid() -> anyhow::Result<u32> {
    let unit = get_sandbox_unit();
    let output = std::process::Command::new("systemctl")
        .args(["--user", "show", "-p", "MainPID", "--value", &unit])
        .output()?;
    let s = String::from_utf8(output.stdout)?.trim().to_string();
    let pid: u32 = s.parse()?;
    Ok(pid)
}

pub fn require_redact_needles_for_fixtures_with_env(
    path: &Path,
    allow_no_redact: Option<&str>,
    redact_file: Option<&Path>,
) {
    let is_fixtures = path.components().any(|c| c.as_os_str() == "fixtures");
    if !is_fixtures {
        return;
    }
    if allow_no_redact == Some("1") {
        return;
    }
    let has_needles = match redact_file {
        Some(p) if p.is_file() => !honk_upstream::parse_redact_file(p).is_empty(),
        _ => false,
    };
    if !has_needles {
        let path_str = redact_file
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "~/.config/honk/redact.txt ($HONK_REDACT_FILE)".to_string());
        panic!(
            "probe recorder writing under fixtures/ requires a non-empty needle file at {path_str}. \
             Set HONK_ALLOW_NO_REDACT=1 to override."
        );
    }
}

pub fn require_redact_needles_for_fixtures(path: &Path) {
    let allow = std::env::var("HONK_ALLOW_NO_REDACT").ok();
    let file = honk_upstream::resolve_redact_file();
    require_redact_needles_for_fixtures_with_env(path, allow.as_deref(), file.as_deref());
}

pub fn get_proc_stats(pid: u32) -> anyhow::Result<ProcStats> {
    let status_path = format!("/proc/{pid}/status");
    let status_content = std::fs::read_to_string(&status_path).unwrap_or_default();
    let mut threads = None;
    let mut vm_rss_kb = None;
    for line in status_content.lines() {
        if let Some(rest) = line.strip_prefix("Threads:") {
            threads = rest.trim().parse().ok();
        } else if let Some(rest) = line.strip_prefix("VmRSS:") {
            let part = rest.trim().trim_end_matches("kB").trim();
            vm_rss_kb = part.parse().ok();
        }
    }

    let ps_output = std::process::Command::new("ps")
        .args(["--ppid", &pid.to_string(), "-o", "pid,cmd"])
        .output()?;
    let ps_str = String::from_utf8_lossy(&ps_output.stdout);
    let mut children = Vec::new();
    for line in ps_str.lines().skip(1) {
        let trimmed = line.trim();
        if !trimmed.is_empty() {
            children.push(trimmed.to_string());
        }
    }

    Ok(ProcStats {
        threads,
        vm_rss_kb,
        children,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    #[should_panic(
        expected = "probe recorder writing under fixtures/ requires a non-empty needle file"
    )]
    fn test_fixtures_recorder_panics_without_needle_file() {
        require_redact_needles_for_fixtures_with_env(
            Path::new("repo/fixtures/p1/out.jsonl"),
            None,
            None,
        );
    }

    #[test]
    #[should_panic(
        expected = "probe recorder writing under fixtures/ requires a non-empty needle file"
    )]
    fn test_fixtures_recorder_panics_with_empty_needle_file() {
        let mut tmp = tempfile::NamedTempFile::new().unwrap();
        writeln!(tmp, "# comment only").unwrap();
        writeln!(tmp).unwrap();

        require_redact_needles_for_fixtures_with_env(
            Path::new("repo/fixtures/p1/out.jsonl"),
            Some("0"),
            Some(tmp.path()),
        );
    }

    #[test]
    fn test_fixtures_recorder_allows_override() {
        require_redact_needles_for_fixtures_with_env(
            Path::new("repo/fixtures/p1/out.jsonl"),
            Some("1"),
            None,
        );
    }

    #[test]
    fn test_non_fixtures_path_does_not_panic() {
        require_redact_needles_for_fixtures_with_env(
            Path::new("repo/other_dir/out.jsonl"),
            None,
            None,
        );
    }

    #[test]
    fn test_fixtures_recorder_succeeds_with_valid_needle_file() {
        let mut tmp = tempfile::NamedTempFile::new().unwrap();
        writeln!(tmp, "my_org_needle").unwrap();

        require_redact_needles_for_fixtures_with_env(
            Path::new("repo/fixtures/p1/out.jsonl"),
            None,
            Some(tmp.path()),
        );
    }
}
