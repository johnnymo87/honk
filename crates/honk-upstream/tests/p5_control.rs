mod common;
use common::*;

use std::time::{Duration, Instant};

use honk_upstream::{Event, Upstream, rpc_id_matches};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

fn is_tool_call(frame: &Value) -> bool {
    let update_type = frame
        .pointer("/params/update/sessionUpdate")
        .and_then(|v| v.as_str());
    update_type == Some("tool_call") || update_type == Some("tool_call_update")
}

#[allow(dead_code)]
fn is_tool_call_completed(frame: &Value) -> bool {
    let update = frame.pointer("/params/update");
    if let Some(u) = update {
        if u.pointer("/toolCall/status").and_then(|s| s.as_str()) == Some("completed") {
            return true;
        }
        if u.pointer("/status").and_then(|s| s.as_str()) == Some("completed") {
            return true;
        }
    }
    false
}

#[allow(dead_code)]
fn is_run_end(frame: &Value) -> bool {
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

fn get_active_run_id(frame: &Value) -> Option<String> {
    if frame
        .pointer("/params/update/sessionUpdate")
        .and_then(|v| v.as_str())
        == Some("session_info_update")
    {
        frame
            .pointer("/params/update/_meta/goose/activeRunId")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
    } else {
        None
    }
}

fn is_permission_request(frame: &Value) -> bool {
    frame.get("method").and_then(|m| m.as_str()) == Some("session/request_permission")
}

fn is_agent_chunk(frame: &Value) -> bool {
    frame
        .pointer("/params/update/sessionUpdate")
        .and_then(|v| v.as_str())
        == Some("agent_message_chunk")
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExportSummary {
    pub message_count: usize,
    pub user_message_persisted: bool,
    pub assistant_message_persisted: bool,
    pub tool_request_persisted: bool,
    pub tool_response_persisted: bool,
    pub assistant_text: String,
    pub roles: Vec<String>,
}

pub fn summarize_export(session_json: &Value) -> ExportSummary {
    let convo = session_json
        .get("conversation")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    let message_count = convo.len();
    let mut user_message_persisted = false;
    let mut assistant_message_persisted = false;
    let mut tool_request_persisted = false;
    let mut tool_response_persisted = false;
    let mut assistant_text = String::new();
    let mut roles = Vec::new();

    for msg in &convo {
        if let Some(role) = msg.get("role").and_then(|r| r.as_str()) {
            roles.push(role.to_string());
            if role == "user" {
                user_message_persisted = true;
            } else if role == "assistant" {
                assistant_message_persisted = true;
            }
        }
        if let Some(contents) = msg.get("content").and_then(|c| c.as_array()) {
            for block in contents {
                if let Some(t) = block.get("type").and_then(|t| t.as_str()) {
                    if t == "toolRequest" {
                        tool_request_persisted = true;
                    } else if t == "toolResponse" {
                        tool_response_persisted = true;
                    }
                }
                if let Some(t) = block.get("text").and_then(|v| v.as_str()) {
                    assistant_text.push_str(t);
                }
            }
        }
    }

    ExportSummary {
        message_count,
        user_message_persisted,
        assistant_message_persisted,
        tool_request_persisted,
        tool_response_persisted,
        assistant_text,
        roles,
    }
}

pub fn check_child_processes(pid: u32) -> anyhow::Result<Vec<String>> {
    let stats = get_proc_stats(pid)?;
    Ok(stats.children)
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

pub async fn restart_sandbox_unit() -> anyhow::Result<()> {
    let res = tokio::process::Command::new("systemctl")
        .args(["--user", "restart", "honk-goose-sandbox"])
        .status()
        .await?;
    if !res.success() {
        let _ = tokio::process::Command::new("systemctl")
            .args(["--user", "stop", "honk-goose-sandbox"])
            .status()
            .await;
        let _ = tokio::process::Command::new("systemctl")
            .args(["--user", "reset-failed", "honk-goose-sandbox"])
            .status()
            .await;
        let manifest_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let repo_root = manifest_dir
            .parent()
            .and_then(|p| p.parent())
            .expect("finding repo root");
        let script = repo_root.join("scripts/goose-sandbox.sh");
        let path = std::env::var("PATH").unwrap_or_default();
        let home = std::env::var("HOME").unwrap_or_default();
        let uid_out = std::process::Command::new("id").arg("-u").output()?;
        let uid = String::from_utf8_lossy(&uid_out.stdout).trim().to_string();
        let xdg = std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| format!("/run/user/{uid}"));
        let status = tokio::process::Command::new("systemd-run")
            .args([
                "--user",
                "--unit=honk-goose-sandbox",
                "--collect",
                &format!("--setenv=PATH={path}"),
                &format!("--setenv=HOME={home}"),
                script.to_str().unwrap(),
            ])
            .env("XDG_RUNTIME_DIR", xdg)
            .status()
            .await?;
        if !status.success() {
            anyhow::bail!("failed to restart sandbox unit via systemd-run");
        }
    }
    Ok(())
}

// =========================================================================
// P5a: Cancel notification (Conn A and Conn B)
// =========================================================================

#[tokio::test]
#[ignore]
async fn p5a1_cancel_conn_a() -> anyhow::Result<()> {
    run_p5a_conn_a_case("p5a1", 41).await
}

#[tokio::test]
#[ignore]
async fn p5a2_cancel_conn_a() -> anyhow::Result<()> {
    run_p5a_conn_a_case("p5a2", 42).await
}

async fn run_p5a_conn_a_case(case_name: &str, sleep_secs: u64) -> anyhow::Result<()> {
    let Some(mut ts) = setup_session_with_mode("p5a_conn_a", "auto").await? else {
        return Ok(());
    };

    let goose_pid = get_goose_sandbox_pid()?;
    println!(
        "[{case_name}] Session {} started on Conn A: {}, goose_pid={}",
        ts.sid, ts.conn_id, goose_pid
    );

    let prompt = format!(
        "Run the shell command \"sleep {sleep_secs}; echo P5A_DONE_{case_name}\", then reply with only the word FINISHED."
    );
    let prompt_start = Instant::now();
    let prompt_req_id = ts
        .upstream
        .request(
            "session/prompt",
            json!({
                "sessionId": &ts.sid,
                "prompt": [{"type": "text", "text": &prompt}]
            }),
            Some(&ts.sid),
        )
        .await?;

    // Wait until tool call starts executing
    let wait_start = Instant::now();
    let timeout = Duration::from_secs(30);
    loop {
        if wait_start.elapsed() >= timeout {
            panic!("[{case_name}] timed out waiting for tool_call to start");
        }
        let remaining = timeout - wait_start.elapsed();
        let Some(event) = ts.upstream.next(remaining).await? else {
            panic!("[{case_name}] stream closed while waiting for tool_call");
        };

        if let Event::Frame(_, ref frame) = event
            && is_tool_call(frame)
        {
            println!("[{case_name}] Tool call frame observed");
            break;
        }
    }

    // Confirm sleep child process exists via sandbox cgroup matching
    let marker_pattern = format!("sleep {sleep_secs}");
    let mut initial_pids = Vec::new();
    for _ in 0..15 {
        initial_pids = pgrep_user(&marker_pattern)?;
        if !initial_pids.is_empty() {
            println!(
                "[{case_name}] Confirmed sleep command running with pids: {:?}",
                initial_pids
            );
            break;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    assert!(
        !initial_pids.is_empty(),
        "sleep command must be running before cancel"
    );

    // Send session/cancel notification from Connection A
    let cancel_start = Instant::now();
    println!("[{case_name}] Sending session/cancel notification from Conn A...");
    ts.upstream
        .notify(
            "session/cancel",
            json!({
                "sessionId": &ts.sid
            }),
            Some(&ts.sid),
        )
        .await?;

    // Wait for prompt response on Connection A
    let mut prompt_reply_frame: Option<Value> = None;
    let reply_timeout = Duration::from_secs(20);
    let reply_wait_start = Instant::now();
    while reply_wait_start.elapsed() < reply_timeout {
        let remaining = reply_timeout - reply_wait_start.elapsed();
        let Some(event) = ts.upstream.next(remaining).await? else {
            break;
        };
        if let Event::Frame(_, ref frame) = event
            && rpc_id_matches(&prompt_req_id, frame.get("id").unwrap_or(&Value::Null))
        {
            prompt_reply_frame = Some(frame.clone());
            println!("[{case_name}] Received prompt reply: {:?}", frame);
            break;
        }
    }

    let cancel_latency = cancel_start.elapsed();
    let stop_reason = prompt_reply_frame
        .as_ref()
        .and_then(|f| f.pointer("/result/stopReason"))
        .and_then(|s| s.as_str())
        .unwrap_or("unknown")
        .to_string();

    // Check immediately if sleep process survived as an orphan
    tokio::time::sleep(Duration::from_millis(500)).await;
    let pids_after_cancel = pgrep_user(&marker_pattern)?;
    let direct_children_after = check_child_processes(goose_pid)?;
    let orphan_survived_immediately = !pids_after_cancel.is_empty();

    println!(
        "[{case_name}] Direct children under goose: {:?}, system-wide pids: {:?}",
        direct_children_after, pids_after_cancel
    );

    if orphan_survived_immediately {
        for pid in &pids_after_cancel {
            let ppid_out = std::process::Command::new("ps")
                .args(["-o", "pid,ppid,cmd", "-p", &pid.to_string()])
                .output();
            if let Ok(out) = ppid_out {
                println!(
                    "[{case_name}] Orphan process details:\n{}",
                    String::from_utf8_lossy(&out.stdout)
                );
            }
        }
    }

    // Wait until sleep_secs + 4s has elapsed from start to check if orphan completes on its own
    let wait_target = Duration::from_secs(sleep_secs + 4);
    if prompt_start.elapsed() < wait_target {
        let sleep_remaining = wait_target - prompt_start.elapsed();
        println!(
            "[{case_name}] Waiting {:.1}s for orphan to complete natural sleep...",
            sleep_remaining.as_secs_f64()
        );
        tokio::time::sleep(sleep_remaining).await;
    }
    let pids_after_completion = pgrep_user(&marker_pattern)?;
    let orphan_completed = pids_after_completion.is_empty();
    println!(
        "[{case_name}] After {}s, orphan completed: {}",
        sleep_secs + 4,
        orphan_completed
    );

    // Export session
    let export_json = export_session(&mut ts.upstream, &ts.sid).await?;
    let export_summary = summarize_export(&export_json);

    let summary = json!({
        "case": case_name,
        "session_id": ts.sid,
        "conn_id": ts.conn_id,
        "sleep_secs": sleep_secs,
        "cancel_latency_ms": cancel_latency.as_millis(),
        "prompt_reply_received": prompt_reply_frame.is_some(),
        "stop_reason": stop_reason,
        "direct_children_after_cancel": direct_children_after,
        "orphan_pids_after_cancel": pids_after_cancel,
        "orphan_survived_immediately": orphan_survived_immediately,
        "orphan_completed_after_wait": orphan_completed,
        "export_message_count": export_summary.message_count,
        "export_user_persisted": export_summary.user_message_persisted,
        "export_assistant_persisted": export_summary.assistant_message_persisted,
        "export_tool_request_persisted": export_summary.tool_request_persisted,
        "export_tool_response_persisted": export_summary.tool_response_persisted,
        "export_roles": export_summary.roles,
        "fixture": ts.fixture_path.display().to_string(),
    });

    println!(
        "[{case_name}] SUMMARY: {}",
        serde_json::to_string_pretty(&summary)?
    );

    assert_eq!(
        stop_reason, "cancelled",
        "expected stopReason to be cancelled"
    );
    assert!(
        export_summary.user_message_persisted,
        "user message must be persisted"
    );

    Ok(())
}

#[tokio::test]
#[ignore]
async fn p5a3_cancel_conn_b_without_load() -> anyhow::Result<()> {
    run_p5a_conn_b_case("p5a3", false, 43).await
}

#[tokio::test]
#[ignore]
async fn p5a4_cancel_conn_b_with_load() -> anyhow::Result<()> {
    run_p5a_conn_b_case("p5a4", true, 44).await
}

async fn run_p5a_conn_b_case(
    case_name: &str,
    with_load: bool,
    sleep_secs: u64,
) -> anyhow::Result<()> {
    let subfolder = if with_load {
        "p5a_conn_b_with_load"
    } else {
        "p5a_conn_b_no_load"
    };
    let Some(mut ts) = setup_session_with_mode(subfolder, "auto").await? else {
        return Ok(());
    };

    let goose_pid = get_goose_sandbox_pid()?;
    println!(
        "[{case_name}] Session {} started on Conn A: {} (with_load={with_load})",
        ts.sid, ts.conn_id
    );

    let prompt = format!(
        "Run the shell command \"sleep {sleep_secs}; echo P5A_DONE_{case_name}\", then reply with only the word FINISHED."
    );
    let prompt_req_id = ts
        .upstream
        .request(
            "session/prompt",
            json!({
                "sessionId": &ts.sid,
                "prompt": [{"type": "text", "text": &prompt}]
            }),
            Some(&ts.sid),
        )
        .await?;

    // Wait until tool call starts
    let wait_start = Instant::now();
    let timeout = Duration::from_secs(30);
    loop {
        if wait_start.elapsed() >= timeout {
            panic!("[{case_name}] timed out waiting for tool_call to start");
        }
        let remaining = timeout - wait_start.elapsed();
        let Some(event) = ts.upstream.next(remaining).await? else {
            panic!("[{case_name}] stream closed while waiting for tool_call");
        };

        if let Event::Frame(_, ref frame) = event
            && is_tool_call(frame)
        {
            println!("[{case_name}] Tool call frame observed on Conn A");
            break;
        }
    }

    // Confirm sleep child via sandbox cgroup matching
    let marker_pattern = format!("sleep {sleep_secs}");
    let mut initial_pids = Vec::new();
    for _ in 0..15 {
        initial_pids = pgrep_user(&marker_pattern)?;
        if !initial_pids.is_empty() {
            println!(
                "[{case_name}] Confirmed sleep command running with pids: {:?}",
                initial_pids
            );
            break;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    assert!(
        !initial_pids.is_empty(),
        "sleep command must be running before cancel"
    );

    // Connect Connection B
    println!("[{case_name}] Initializing Conn B...");
    let mut upstream_b = Upstream::initialize(&ts.url, ts.secret.clone(), ts.rec.clone()).await?;
    let conn_b_id = upstream_b.conn_id().to_string();
    upstream_b.subscribe(None).await?;
    upstream_b.subscribe(Some(&ts.sid)).await?;

    if with_load {
        println!("[{case_name}] Calling session/load on Conn B...");
        let load_res = upstream_b
            .call(
                "session/load",
                json!({
                    "sessionId": &ts.sid,
                    "cwd": ts.temp_dir.path().to_str().unwrap(),
                    "mcpServers": []
                }),
                Some(&ts.sid),
                Duration::from_secs(30),
            )
            .await;
        println!(
            "[{case_name}] session/load on Conn B result: {:?}",
            load_res.is_ok()
        );
    }

    // Send session/cancel notification from Connection B
    let cancel_start = Instant::now();
    println!("[{case_name}] Sending session/cancel notification from Conn B...");
    upstream_b
        .notify(
            "session/cancel",
            json!({
                "sessionId": &ts.sid
            }),
            Some(&ts.sid),
        )
        .await?;

    // Wait for prompt response on Connection A
    let mut prompt_reply_frame: Option<Value> = None;
    let reply_timeout = Duration::from_secs(20);
    let reply_wait_start = Instant::now();
    while reply_wait_start.elapsed() < reply_timeout {
        let remaining = reply_timeout - reply_wait_start.elapsed();
        let Some(event) = ts.upstream.next(remaining).await? else {
            break;
        };
        if let Event::Frame(_, ref frame) = event
            && rpc_id_matches(&prompt_req_id, frame.get("id").unwrap_or(&Value::Null))
        {
            prompt_reply_frame = Some(frame.clone());
            println!("[{case_name}] Received prompt reply on Conn A: {:?}", frame);
            break;
        }
    }

    let cancel_latency = cancel_start.elapsed();
    let stop_reason = prompt_reply_frame
        .as_ref()
        .and_then(|f| f.pointer("/result/stopReason"))
        .and_then(|s| s.as_str())
        .unwrap_or("unknown")
        .to_string();

    tokio::time::sleep(Duration::from_millis(500)).await;
    let pids_after_cancel = pgrep_user(&marker_pattern)?;
    let direct_children_after = check_child_processes(goose_pid)?;
    let orphan_survived_immediately = !pids_after_cancel.is_empty();

    println!(
        "[{case_name}] Direct children under goose: {:?}, system-wide pids: {:?}",
        direct_children_after, pids_after_cancel
    );

    // Clean up orphan process so it does not linger
    for pid in &pids_after_cancel {
        let _ = std::process::Command::new("kill")
            .args(["-9", &pid.to_string()])
            .output();
    }

    let export_json = export_session(&mut ts.upstream, &ts.sid).await?;
    let export_summary = summarize_export(&export_json);

    let summary = json!({
        "case": case_name,
        "with_load": with_load,
        "session_id": ts.sid,
        "conn_a_id": ts.conn_id,
        "conn_b_id": conn_b_id,
        "sleep_secs": sleep_secs,
        "cancel_latency_ms": cancel_latency.as_millis(),
        "prompt_reply_received": prompt_reply_frame.is_some(),
        "stop_reason": stop_reason,
        "direct_children_after_cancel": direct_children_after,
        "orphan_pids_after_cancel": pids_after_cancel,
        "orphan_survived_immediately": orphan_survived_immediately,
        "export_message_count": export_summary.message_count,
        "export_user_persisted": export_summary.user_message_persisted,
        "export_assistant_persisted": export_summary.assistant_message_persisted,
        "export_tool_request_persisted": export_summary.tool_request_persisted,
        "export_tool_response_persisted": export_summary.tool_response_persisted,
        "export_roles": export_summary.roles,
        "fixture": ts.fixture_path.display().to_string(),
    });

    println!(
        "[{case_name}] SUMMARY: {}",
        serde_json::to_string_pretty(&summary)?
    );

    assert_eq!(
        stop_reason, "cancelled",
        "expected stopReason to be cancelled"
    );
    Ok(())
}

// =========================================================================
// P5b: $/cancel_request (protocol-level request cancellation)
// =========================================================================

#[tokio::test]
#[ignore]
async fn p5b1_cancel_request_protocol() -> anyhow::Result<()> {
    run_p5b_case("p5b1").await
}

#[tokio::test]
#[ignore]
async fn p5b2_cancel_request_protocol() -> anyhow::Result<()> {
    run_p5b_case("p5b2").await
}

async fn run_p5b_case(case_name: &str) -> anyhow::Result<()> {
    let Some(mut ts) = setup_session_with_mode("p5b", "auto").await? else {
        return Ok(());
    };

    let goose_pid = get_goose_sandbox_pid()?;
    println!(
        "[{case_name}] Session {} started on Conn A: {}, goose_pid={}",
        ts.sid, ts.conn_id, goose_pid
    );

    let prompt =
        "Run the shell command \"sleep 40 && echo done\", then reply with only the word FINISHED.";
    let prompt_req_id = ts
        .upstream
        .request(
            "session/prompt",
            json!({
                "sessionId": &ts.sid,
                "prompt": [{"type": "text", "text": prompt}]
            }),
            Some(&ts.sid),
        )
        .await?;

    // Wait until tool call starts
    let wait_start = Instant::now();
    let timeout = Duration::from_secs(30);
    loop {
        if wait_start.elapsed() >= timeout {
            panic!("[{case_name}] timed out waiting for tool_call to start");
        }
        let remaining = timeout - wait_start.elapsed();
        let Some(event) = ts.upstream.next(remaining).await? else {
            panic!("[{case_name}] stream closed while waiting for tool_call");
        };

        if let Event::Frame(_, ref frame) = event
            && is_tool_call(frame)
        {
            println!("[{case_name}] Tool call frame observed");
            break;
        }
    }

    // Confirm sleep child
    let mut sleep_child_detected = false;
    for _ in 0..15 {
        let children = check_child_processes(goose_pid)?;
        if children.iter().any(|c| c.contains("sleep")) {
            sleep_child_detected = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }

    // Send $/cancel_request notification
    println!(
        "[{case_name}] Sending $/cancel_request for prompt request id {:?}...",
        prompt_req_id
    );
    let cancel_request_res = ts
        .upstream
        .notify(
            "$/cancel_request",
            json!({
                "requestId": prompt_req_id.clone()
            }),
            None,
        )
        .await;
    let cancel_request_accepted = cancel_request_res.is_ok();
    println!("[{case_name}] $/cancel_request accepted: {cancel_request_accepted}");

    // Wait 15 seconds to observe if goose honors $/cancel_request
    println!("[{case_name}] Waiting 15s to observe if $/cancel_request stops the run...");
    let observe_timeout = Duration::from_secs(15);
    let observe_start = Instant::now();
    let mut prompt_reply_during_observe: Option<Value> = None;

    while observe_start.elapsed() < observe_timeout {
        let remaining = observe_timeout - observe_start.elapsed();
        if let Ok(Some(Event::Frame(_, ref frame))) = ts.upstream.next(remaining).await
            && rpc_id_matches(&prompt_req_id, frame.get("id").unwrap_or(&Value::Null))
        {
            prompt_reply_during_observe = Some(frame.clone());
            println!(
                "[{case_name}] Prompt reply received during observe: {:?}",
                frame
            );
            break;
        }
    }

    let children_after_observe = check_child_processes(goose_pid)?;
    let sleep_alive_after_observe = children_after_observe.iter().any(|c| c.contains("sleep"));
    let honored = prompt_reply_during_observe.is_some() || !sleep_alive_after_observe;

    println!(
        "[{case_name}] $/cancel_request honored: {honored} (reply_received={}, sleep_still_running={sleep_alive_after_observe})",
        prompt_reply_during_observe.is_some()
    );

    // If not honored, send session/cancel now to clean up without waiting 40s
    let mut final_stop_reason = prompt_reply_during_observe
        .as_ref()
        .and_then(|f| f.pointer("/result/stopReason"))
        .and_then(|s| s.as_str())
        .map(|s| s.to_string());

    if !honored {
        println!("[{case_name}] $/cancel_request was ignored; sending fallback session/cancel...");
        ts.upstream
            .notify(
                "session/cancel",
                json!({
                    "sessionId": &ts.sid
                }),
                Some(&ts.sid),
            )
            .await?;

        let cleanup_timeout = Duration::from_secs(20);
        let cleanup_start = Instant::now();
        while cleanup_start.elapsed() < cleanup_timeout {
            let remaining = cleanup_timeout - cleanup_start.elapsed();
            if let Ok(Some(Event::Frame(_, ref frame))) = ts.upstream.next(remaining).await
                && rpc_id_matches(&prompt_req_id, frame.get("id").unwrap_or(&Value::Null))
            {
                final_stop_reason = frame
                    .pointer("/result/stopReason")
                    .and_then(|s| s.as_str())
                    .map(|s| s.to_string());
                println!(
                    "[{case_name}] Prompt reply after fallback cancel: {:?}",
                    frame
                );
                break;
            }
        }
    }

    // Export session
    let export_json = export_session(&mut ts.upstream, &ts.sid).await?;
    let export_summary = summarize_export(&export_json);

    let summary = json!({
        "case": case_name,
        "session_id": ts.sid,
        "conn_id": ts.conn_id,
        "prompt_req_id": prompt_req_id,
        "sleep_child_detected_before_cancel": sleep_child_detected,
        "cancel_request_accepted": cancel_request_accepted,
        "honored": honored,
        "prompt_reply_during_15s_observe": prompt_reply_during_observe.is_some(),
        "sleep_alive_during_observe": sleep_alive_after_observe,
        "final_stop_reason": final_stop_reason,
        "export_message_count": export_summary.message_count,
        "export_user_persisted": export_summary.user_message_persisted,
        "export_assistant_persisted": export_summary.assistant_message_persisted,
        "export_tool_request_persisted": export_summary.tool_request_persisted,
        "export_tool_response_persisted": export_summary.tool_response_persisted,
        "fixture": ts.fixture_path.display().to_string(),
    });

    println!(
        "[{case_name}] SUMMARY: {}",
        serde_json::to_string_pretty(&summary)?
    );

    Ok(())
}

// =========================================================================
// P5c: Cancel while parked on permission request
// =========================================================================

#[tokio::test]
#[ignore]
async fn p5c1_cancel_parked_permission() -> anyhow::Result<()> {
    run_p5c_case("p5c1").await
}

#[tokio::test]
#[ignore]
async fn p5c2_cancel_parked_permission() -> anyhow::Result<()> {
    run_p5c_case("p5c2").await
}

async fn run_p5c_case(case_name: &str) -> anyhow::Result<()> {
    let Some(mut ts) = setup_approve_session("p5c").await? else {
        return Ok(());
    };

    println!("[{case_name}] Session {} started in approve mode", ts.sid);

    let prompt = "Run the shell command \"echo P5OK\", then reply with only the word FINISHED.";
    let prompt_req_id = ts
        .upstream
        .request(
            "session/prompt",
            json!({
                "sessionId": &ts.sid,
                "prompt": [{"type": "text", "text": prompt}]
            }),
            Some(&ts.sid),
        )
        .await?;

    // Wait for permission request
    let timeout = Duration::from_secs(30);
    let start_wait = Instant::now();
    let perm_id = loop {
        if start_wait.elapsed() >= timeout {
            panic!("[{case_name}] timed out waiting for session/request_permission");
        }
        let remaining = timeout - start_wait.elapsed();
        let Some(event) = ts.upstream.next(remaining).await? else {
            panic!("[{case_name}] stream closed while waiting for permission request");
        };

        if let Event::Frame(_, ref frame) = event
            && is_permission_request(frame)
        {
            let id = frame.get("id").cloned().expect("permission id");
            println!("[{case_name}] Received permission request: {:?}", id);
            break id;
        }
    };

    // Send session/cancel WITHOUT answering the permission request
    println!(
        "[{case_name}] Sending session/cancel notification WITHOUT answering permission request..."
    );
    let cancel_send_time = Instant::now();
    ts.upstream
        .notify(
            "session/cancel",
            json!({
                "sessionId": &ts.sid
            }),
            Some(&ts.sid),
        )
        .await?;

    // Check if the run stops without answering within 15 seconds
    println!("[{case_name}] Waiting 15s to observe if run stops without answering...");
    let observe_timeout = Duration::from_secs(15);
    let observe_start = Instant::now();
    let mut prompt_reply_without_answer: Option<Value> = None;

    while observe_start.elapsed() < observe_timeout {
        let remaining = observe_timeout - observe_start.elapsed();
        if let Ok(Some(Event::Frame(_, ref frame))) = ts.upstream.next(remaining).await
            && rpc_id_matches(&prompt_req_id, frame.get("id").unwrap_or(&Value::Null))
        {
            prompt_reply_without_answer = Some(frame.clone());
            println!(
                "[{case_name}] Prompt reply received without answer: {:?}",
                frame
            );
            break;
        }
    }

    let stopped_without_answer = prompt_reply_without_answer.is_some();
    println!(
        "[{case_name}] Run stopped without answering permission request: {stopped_without_answer}"
    );

    // Now answer the permission request with cancelled outcome
    println!("[{case_name}] Answering permission request with outcome: cancelled...");
    let answer_start = Instant::now();
    let respond_res = ts
        .upstream
        .respond(
            perm_id.clone(),
            json!({
                "outcome": {
                    "outcome": "cancelled"
                }
            }),
            Some(&ts.sid),
        )
        .await;
    println!("[{case_name}] respond result: {:?}", respond_res.is_ok());

    // Wait for prompt reply after answering
    let mut post_answer_reply: Option<Value> = None;
    let post_answer_timeout = Duration::from_secs(20);
    let post_answer_wait = Instant::now();
    while post_answer_wait.elapsed() < post_answer_timeout {
        let remaining = post_answer_timeout - post_answer_wait.elapsed();
        let Some(event) = ts.upstream.next(remaining).await? else {
            break;
        };
        if let Event::Frame(_, ref frame) = event
            && rpc_id_matches(&prompt_req_id, frame.get("id").unwrap_or(&Value::Null))
        {
            post_answer_reply = Some(frame.clone());
            println!(
                "[{case_name}] Prompt reply received after answer: {:?}",
                frame
            );
            break;
        }
    }

    let post_answer_latency = answer_start.elapsed();
    let final_reply = prompt_reply_without_answer.or(post_answer_reply);
    let stop_reason = final_reply
        .as_ref()
        .and_then(|f| f.pointer("/result/stopReason"))
        .and_then(|s| s.as_str())
        .unwrap_or("unknown")
        .to_string();

    // Export session
    let export_json = export_session(&mut ts.upstream, &ts.sid).await?;
    let export_summary = summarize_export(&export_json);

    let summary = json!({
        "case": case_name,
        "session_id": ts.sid,
        "conn_id": ts.conn_id,
        "perm_id": perm_id,
        "cancel_to_answer_duration_ms": cancel_send_time.elapsed().as_millis(),
        "stopped_without_answer": stopped_without_answer,
        "post_answer_latency_ms": post_answer_latency.as_millis(),
        "final_stop_reason": stop_reason,
        "export_message_count": export_summary.message_count,
        "export_user_persisted": export_summary.user_message_persisted,
        "export_assistant_persisted": export_summary.assistant_message_persisted,
        "export_tool_request_persisted": export_summary.tool_request_persisted,
        "export_tool_response_persisted": export_summary.tool_response_persisted,
        "fixture": ts.fixture_path.display().to_string(),
    });

    println!(
        "[{case_name}] SUMMARY: {}",
        serde_json::to_string_pretty(&summary)?
    );

    assert_eq!(
        stop_reason, "cancelled",
        "expected prompt to end with cancelled"
    );
    Ok(())
}

// =========================================================================
// P5d: Steer accepted then dropped
// =========================================================================

#[tokio::test]
#[ignore]
async fn p5d1_steer_early_control() -> anyhow::Result<()> {
    run_p5d_case("p5d1", false).await
}

#[tokio::test]
#[ignore]
async fn p5d2_steer_late_risky() -> anyhow::Result<()> {
    run_p5d_case("p5d2", true).await
}

#[tokio::test]
#[ignore]
async fn p5d3_steer_early_control() -> anyhow::Result<()> {
    run_p5d_case("p5d3", false).await
}

#[tokio::test]
#[ignore]
async fn p5d4_steer_late_risky() -> anyhow::Result<()> {
    run_p5d_case("p5d4", true).await
}

async fn run_p5d_case(case_name: &str, late_timing: bool) -> anyhow::Result<()> {
    let subfolder = if late_timing { "p5d_late" } else { "p5d_early" };
    let Some(mut ts) = setup_session_with_mode(subfolder, "auto").await? else {
        return Ok(());
    };

    println!(
        "[{case_name}] Session {} started (late_timing={late_timing})",
        ts.sid
    );

    let prompt = if late_timing {
        "Run the shell command \"sleep 3 && echo done\", then reply with only the word FINISHED."
    } else {
        "Run the shell command \"sleep 10 && echo done\", then reply with only the word FINISHED."
    };

    let prompt_req_id = ts
        .upstream
        .request(
            "session/prompt",
            json!({
                "sessionId": &ts.sid,
                "prompt": [{"type": "text", "text": prompt}]
            }),
            Some(&ts.sid),
        )
        .await?;

    let steer_marker = if late_timing {
        "P5D_LATE_STEERED"
    } else {
        "P5D_EARLY_STEERED"
    };
    let steer_text = format!("Wait! Instead of FINISHED, reply with only the word {steer_marker}.");

    let timeout = Duration::from_secs(60);
    let wait_start = Instant::now();
    let mut active_run_id: Option<String> = None;
    let mut steer_sent = false;
    let mut steer_response: Option<Value> = None;
    let mut prompt_reply_frame: Option<Value> = None;

    while wait_start.elapsed() < timeout {
        let remaining = timeout - wait_start.elapsed();
        let Some(event) = ts.upstream.next(remaining).await? else {
            break;
        };

        if let Event::Frame(_, ref frame) = event {
            if let Some(rid) = get_active_run_id(frame) {
                active_run_id = Some(rid);
            }

            // Early timing trigger: tool_call starts
            if !late_timing
                && !steer_sent
                && is_tool_call(frame)
                && let Some(ref rid) = active_run_id
            {
                println!(
                    "[{case_name}] Triggering early steer for run_id={:?}...",
                    rid
                );
                steer_sent = true;
                let steer_res = ts
                    .upstream
                    .call(
                        "_goose/unstable/session/steer",
                        json!({
                            "sessionId": &ts.sid,
                            "expectedRunId": rid,
                            "prompt": [{"type": "text", "text": &steer_text}]
                        }),
                        Some(&ts.sid),
                        Duration::from_secs(30),
                    )
                    .await;
                println!("[{case_name}] Early steer result: {:?}", steer_res);
                steer_response = steer_res.ok();
            }

            // Late timing trigger: first agent_message_chunk
            if late_timing
                && !steer_sent
                && let Some(ref rid) = active_run_id
                && is_agent_chunk(frame)
            {
                println!(
                    "[{case_name}] Triggering late steer on first agent_message_chunk for run_id={:?}...",
                    rid
                );
                steer_sent = true;
                let steer_res = ts
                    .upstream
                    .call(
                        "_goose/unstable/session/steer",
                        json!({
                            "sessionId": &ts.sid,
                            "expectedRunId": rid,
                            "prompt": [{"type": "text", "text": &steer_text}]
                        }),
                        Some(&ts.sid),
                        Duration::from_secs(30),
                    )
                    .await;
                println!("[{case_name}] Late steer result: {:?}", steer_res);
                steer_response = steer_res.ok();
            }

            if rpc_id_matches(&prompt_req_id, frame.get("id").unwrap_or(&Value::Null)) {
                prompt_reply_frame = Some(frame.clone());
                println!("[{case_name}] Received prompt reply: {:?}", frame);
                break;
            }
        }
    }

    // Export session
    let export_json = export_session(&mut ts.upstream, &ts.sid).await?;
    let export_summary = summarize_export(&export_json);

    // Check if the steered text was consumed into conversation
    let conversation = export_json
        .get("conversation")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    let steer_in_conversation = conversation.iter().any(|msg| {
        msg.get("content")
            .and_then(|c| c.as_array())
            .map(|arr| {
                arr.iter().any(|b| {
                    b.get("text")
                        .and_then(|t| t.as_str())
                        .map(|s| s.contains(steer_marker))
                        .unwrap_or(false)
                })
            })
            .unwrap_or(false)
    });

    let assistant_steered = export_summary.assistant_text.contains(steer_marker);
    let steer_accepted = steer_response.is_some();
    let steer_dropped = steer_accepted && !steer_in_conversation;

    let summary = json!({
        "case": case_name,
        "late_timing": late_timing,
        "session_id": ts.sid,
        "conn_id": ts.conn_id,
        "active_run_id": active_run_id,
        "steer_sent": steer_sent,
        "steer_accepted": steer_accepted,
        "steer_response": steer_response,
        "prompt_reply_received": prompt_reply_frame.is_some(),
        "steer_in_conversation": steer_in_conversation,
        "assistant_steered": assistant_steered,
        "steer_dropped": steer_dropped,
        "assistant_text": export_summary.assistant_text,
        "export_message_count": export_summary.message_count,
        "fixture": ts.fixture_path.display().to_string(),
    });

    println!(
        "[{case_name}] SUMMARY: {}",
        serde_json::to_string_pretty(&summary)?
    );

    if !late_timing {
        // Control case: steer must be accepted and consumed
        assert!(steer_accepted, "control steer must be accepted");
        assert!(
            steer_in_conversation,
            "control steer must appear in conversation"
        );
    } else {
        // Late steer: turn must end cleanly with prompt reply, steer accepted
        assert!(
            prompt_reply_frame.is_some(),
            "late steer turn must end cleanly with prompt reply"
        );
        assert!(
            steer_accepted,
            "late steer request must be accepted with runId and messageId"
        );
    }

    Ok(())
}

// =========================================================================
// P5e: Goose restart mid-turn
// =========================================================================

#[tokio::test]
#[ignore]
async fn p5e1_restart_mid_turn() -> anyhow::Result<()> {
    run_p5e_case("p5e1").await
}

#[tokio::test]
#[ignore]
async fn p5e2_restart_mid_turn() -> anyhow::Result<()> {
    run_p5e_case("p5e2").await
}

async fn run_p5e_case(case_name: &str) -> anyhow::Result<()> {
    let Some(mut ts) = setup_session_with_mode("p5e", "auto").await? else {
        return Ok(());
    };

    let initial_goose_pid = get_goose_sandbox_pid()?;
    println!(
        "[{case_name}] Session {} started on Conn A: {}, initial goose_pid={}",
        ts.sid, ts.conn_id, initial_goose_pid
    );

    let prompt =
        "Run the shell command \"sleep 40 && echo done\", then reply with only the word FINISHED.";
    let _prompt_req_id = ts
        .upstream
        .request(
            "session/prompt",
            json!({
                "sessionId": &ts.sid,
                "prompt": [{"type": "text", "text": prompt}]
            }),
            Some(&ts.sid),
        )
        .await?;

    // Wait until tool call starts
    let wait_start = Instant::now();
    let timeout = Duration::from_secs(30);
    loop {
        if wait_start.elapsed() >= timeout {
            panic!("[{case_name}] timed out waiting for tool_call to start");
        }
        let remaining = timeout - wait_start.elapsed();
        let Some(event) = ts.upstream.next(remaining).await? else {
            panic!("[{case_name}] stream closed while waiting for tool_call");
        };

        if let Event::Frame(_, ref frame) = event
            && is_tool_call(frame)
        {
            println!("[{case_name}] Tool call observed, verifying sleep child...");
            break;
        }
    }

    // Confirm sleep child process
    let mut sleep_child_detected = false;
    for _ in 0..15 {
        let children = check_child_processes(initial_goose_pid)?;
        if children.iter().any(|c| c.contains("sleep")) {
            sleep_child_detected = true;
            println!(
                "[{case_name}] Confirmed sleep child running: {:?}",
                children
            );
            break;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }

    // Restart the sandbox unit mid-turn
    println!("[{case_name}] Restarting sandbox unit honk-goose-sandbox mid-turn...");
    let restart_start = Instant::now();
    restart_sandbox_unit().await?;
    let restart_elapsed = restart_start.elapsed();
    println!(
        "[{case_name}] Sandbox restart command completed in {:.2}s",
        restart_elapsed.as_secs_f64()
    );

    // Poll initialize until goose is ready again
    let init_poll_start = Instant::now();
    let mut new_upstream = loop {
        if init_poll_start.elapsed() > Duration::from_secs(30) {
            panic!("[{case_name}] Timed out waiting for goose to accept initialize after restart");
        }
        match Upstream::initialize(&ts.url, ts.secret.clone(), ts.rec.clone()).await {
            Ok(u) => break u,
            Err(_) => tokio::time::sleep(Duration::from_millis(100)).await,
        }
    };
    let init_recovery_duration = init_poll_start.elapsed();
    let new_conn_id = new_upstream.conn_id().to_string();
    let new_goose_pid = get_goose_sandbox_pid()?;
    println!(
        "[{case_name}] Goose ready on new conn {new_conn_id} after {:.2}s. new_pid={new_goose_pid}",
        init_recovery_duration.as_secs_f64()
    );

    // Probe POST with old connection ID against running goose server
    let raw_client = reqwest::Client::new();
    let post_res = raw_client
        .post(&ts.url)
        .header("x-secret-key", &ts.secret)
        .header("acp-connection-id", &ts.conn_id)
        .header("content-type", "application/json")
        .json(&json!({
            "jsonrpc": "2.0",
            "id": 999,
            "method": "session/list",
            "params": {}
        }))
        .send()
        .await;
    let post_status = post_res.as_ref().map(|r| r.status().as_u16()).unwrap_or(0);
    println!("[{case_name}] POST with old conn_id returned status: {post_status}");

    // Probe GET with old connection ID against running goose server
    let get_res = raw_client
        .get(&ts.url)
        .header("x-secret-key", &ts.secret)
        .header("acp-connection-id", &ts.conn_id)
        .header("accept", "text/event-stream")
        .send()
        .await;
    let get_status = get_res.as_ref().map(|r| r.status().as_u16()).unwrap_or(0);
    println!("[{case_name}] GET with old conn_id returned status: {get_status}");

    // Check if sleep child process survived restart
    let new_children = check_child_processes(new_goose_pid)?;
    let sleep_survived_under_new_pid = new_children.iter().any(|c| c.contains("sleep"));
    // Also check sandbox cgroup for the exact sleep process
    let sleep_pids = sandbox_pids_matching("sleep 40")?;
    let sleep_survived_sandbox = !sleep_pids.is_empty();

    println!(
        "[{case_name}] Sleep child survived under goose: {sleep_survived_under_new_pid}, in sandbox cgroup: {sleep_survived_sandbox}"
    );

    // Subscribe new_upstream to session so we can receive RPC replies on session stream
    new_upstream.subscribe(None).await?;
    new_upstream.subscribe(Some(&ts.sid)).await?;

    // Export session on new connection BEFORE session/load
    let export_before_load = export_session(&mut new_upstream, &ts.sid).await;
    let export_before_load_ok = export_before_load.is_ok();
    let export_summary_before = export_before_load.as_ref().ok().map(summarize_export);
    println!(
        "[{case_name}] Export on fresh connection before session/load: ok={export_before_load_ok}, summary={:?}",
        export_summary_before
    );

    // session/load on fresh connection
    println!("[{case_name}] Calling session/load on new connection...");
    let load_res = new_upstream
        .call(
            "session/load",
            json!({
                "sessionId": &ts.sid,
                "cwd": ts.temp_dir.path().to_str().unwrap(),
                "mcpServers": []
            }),
            Some(&ts.sid),
            Duration::from_secs(30),
        )
        .await;
    println!("[{case_name}] session/load result: {:?}", load_res.is_ok());

    // Check what frames are re-sent during/after session/load
    let mut resent_frames = Vec::new();
    let drain_start = Instant::now();
    while drain_start.elapsed() < Duration::from_secs(3) {
        let remaining = Duration::from_secs(3) - drain_start.elapsed();
        if let Ok(Some(Event::Frame(_, frame))) = new_upstream.next(remaining).await {
            resent_frames.push(frame);
        }
    }
    println!(
        "[{case_name}] Received {} frames after session/load",
        resent_frames.len()
    );

    // Export session after session/load
    let export_after_load = export_session(&mut new_upstream, &ts.sid).await?;
    let export_summary_after = summarize_export(&export_after_load);

    // Send a new prompt to verify session recovers and accepts turns
    println!("[{case_name}] Sending follow-up prompt to verify session recovery...");
    let recover_reply = new_upstream
        .call(
            "session/prompt",
            json!({
                "sessionId": &ts.sid,
                "prompt": [{"type": "text", "text": "Do not run any previous commands. Reply with only the word RECOVERED."}]
            }),
            Some(&ts.sid),
            Duration::from_secs(60),
        )
        .await?;
    println!("[{case_name}] Recovery prompt reply: {:?}", recover_reply);

    let final_export = export_session(&mut new_upstream, &ts.sid).await?;
    let final_summary = summarize_export(&final_export);
    let recovered_text = final_summary.assistant_text;
    let recovery_successful = recovered_text.contains("RECOVERED");

    let summary = json!({
        "case": case_name,
        "session_id": ts.sid,
        "old_conn_id": ts.conn_id,
        "new_conn_id": new_conn_id,
        "initial_goose_pid": initial_goose_pid,
        "new_goose_pid": new_goose_pid,
        "sleep_child_detected_before_restart": sleep_child_detected,
        "post_status_with_old_conn": post_status,
        "get_status_with_old_conn": get_status,
        "init_recovery_duration_ms": init_recovery_duration.as_millis(),
        "sleep_survived_under_goose": sleep_survived_under_new_pid,
        "sleep_survived_sandbox": sleep_survived_sandbox,
        "export_before_load_ok": export_before_load_ok,
        "export_before_load_summary": export_summary_before,
        "session_load_ok": load_res.is_ok(),
        "resent_frames_count": resent_frames.len(),
        "export_after_load_summary": export_summary_after,
        "recovery_successful": recovery_successful,
        "fixture": ts.fixture_path.display().to_string(),
    });

    println!(
        "[{case_name}] SUMMARY: {}",
        serde_json::to_string_pretty(&summary)?
    );

    assert_eq!(post_status, 404, "expected 404 for POST with old conn_id");
    assert_eq!(get_status, 404, "expected 404 for GET with old conn_id");
    assert!(
        !sleep_survived_sandbox,
        "sleep process should not survive unit restart"
    );
    assert!(
        export_before_load_ok,
        "export must succeed on fresh connection before session/load"
    );
    assert!(
        recovery_successful,
        "session should be usable after reconnect"
    );

    Ok(())
}
