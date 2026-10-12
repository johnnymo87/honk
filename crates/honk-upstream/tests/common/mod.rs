#![allow(dead_code, unused_imports)]

pub use std::path::{Path, PathBuf};
pub use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

pub use honk_upstream::{Event, Recorder, Redactor, StreamKey, Upstream, rpc_id_matches};
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

pub async fn setup_session_with_mode(
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

    let url =
        std::env::var("HONK_GOOSE_URL").unwrap_or_else(|_| "http://127.0.0.1:4180/acp".to_string());

    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();

    let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    let repo_root = manifest_dir
        .parent()
        .and_then(|p| p.parent())
        .expect("finding repo root");
    let fixture_path = repo_root.join(format!(
        "fixtures/goose-1.54.0-patched.2/{probe_name}/{ts}.jsonl"
    ));
    if let Some(parent) = fixture_path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let redactor = Redactor::from_env();
    let rec = Recorder::file(&fixture_path, redactor)?;

    let mut upstream = Upstream::initialize(&url, secret.clone(), rec.clone()).await?;
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
        url,
        secret,
        conn_id,
        sid,
        rec,
        fixture_path,
        temp_dir,
    }))
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

pub fn get_goose_sandbox_pid() -> anyhow::Result<u32> {
    let output = std::process::Command::new("systemctl")
        .args([
            "--user",
            "show",
            "-p",
            "MainPID",
            "--value",
            "honk-goose-sandbox",
        ])
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
