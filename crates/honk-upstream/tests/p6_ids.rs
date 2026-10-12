mod common;
use common::*;

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use honk_upstream::{Direction, Event, RecordEntry, Recorder, Redactor, Upstream};
use serde::{Deserialize, Serialize};
use serde_json::json;

// =========================================================================
// Resource snapshot & child process inspection
// =========================================================================

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResourceSnapshot {
    pub pid: u32,
    pub vm_rss_kb: u64,
    pub vm_rss_mb: f64,
    pub threads: u64,
    pub child_count: usize,
    pub children: Vec<(u32, String)>,
}

pub fn get_descendant_pids(root_pid: u32) -> anyhow::Result<Vec<(u32, String)>> {
    let output = std::process::Command::new("ps")
        .args(["-eo", "pid,ppid,args"])
        .output()?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut tree: HashMap<u32, Vec<(u32, String)>> = HashMap::new();
    for line in stdout.lines().skip(1) {
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.len() >= 3
            && let (Ok(pid), Ok(ppid)) = (parts[0].parse::<u32>(), parts[1].parse::<u32>())
        {
            let cmd = parts[2..].join(" ");
            tree.entry(ppid).or_default().push((pid, cmd));
        }
    }
    let mut descendants = Vec::new();
    let mut queue = vec![root_pid];
    while let Some(current) = queue.pop() {
        if let Some(children) = tree.get(&current) {
            for (c_pid, c_cmd) in children {
                descendants.push((*c_pid, c_cmd.clone()));
                queue.push(*c_pid);
            }
        }
    }
    Ok(descendants)
}

pub fn snapshot_resources(pid: u32) -> anyhow::Result<ResourceSnapshot> {
    let status_path = format!("/proc/{pid}/status");
    let status_content = std::fs::read_to_string(&status_path)?;
    let mut threads = 0;
    let mut vm_rss_kb = 0;
    for line in status_content.lines() {
        if let Some(rest) = line.strip_prefix("Threads:") {
            threads = rest.trim().parse().unwrap_or(0);
        } else if let Some(rest) = line.strip_prefix("VmRSS:") {
            let part = rest.trim().trim_end_matches("kB").trim();
            vm_rss_kb = part.parse().unwrap_or(0);
        }
    }
    let children = get_descendant_pids(pid)?;
    Ok(ResourceSnapshot {
        pid,
        vm_rss_kb,
        vm_rss_mb: vm_rss_kb as f64 / 1024.0,
        threads,
        child_count: children.len(),
        children,
    })
}

// =========================================================================
// Frame inspection and ID analysis
// =========================================================================

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExtractedEvent {
    pub stream: String,
    pub update_type: String,
    pub message_id: Option<String>,
    pub tool_call_id: Option<String>,
    pub tool_name: Option<String>,
    pub tool_status: Option<String>,
    pub text_snippet: Option<String>,
}

pub fn read_fixture_events(path: &Path) -> anyhow::Result<Vec<ExtractedEvent>> {
    let content = std::fs::read_to_string(path)?;
    let mut events = Vec::new();
    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let entry: RecordEntry = serde_json::from_str(trimmed)?;
        if entry.dir != Direction::In {
            continue;
        }
        let Some(frame) = entry.frame else {
            continue;
        };
        let Some(update) = frame.pointer("/params/update") else {
            continue;
        };
        let update_type = update
            .get("sessionUpdate")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown")
            .to_string();

        let message_id = update
            .get("messageId")
            .and_then(|v| v.as_str())
            .or_else(|| {
                update
                    .pointer("/_meta/goose/messageId")
                    .and_then(|v| v.as_str())
            })
            .map(|s| s.to_string());

        let tool_call_id = update
            .get("toolCallId")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());

        let tool_name = update
            .pointer("/_meta/goose/toolCall/toolName")
            .and_then(|v| v.as_str())
            .or_else(|| {
                update
                    .pointer("/toolCall/toolName")
                    .and_then(|v| v.as_str())
            })
            .map(|s| s.to_string());

        let tool_status = update
            .get("status")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());

        let text_snippet = update
            .pointer("/content/text")
            .and_then(|v| v.as_str())
            .or_else(|| {
                update
                    .pointer("/content/0/content/text")
                    .and_then(|v| v.as_str())
            })
            .map(|s| {
                let first_line = s.lines().next().unwrap_or(s);
                if first_line.len() > 60 {
                    format!("{}...", &first_line[..60])
                } else {
                    first_line.to_string()
                }
            });

        events.push(ExtractedEvent {
            stream: entry.stream,
            update_type,
            message_id,
            tool_call_id,
            tool_name,
            tool_status,
            text_snippet,
        });
    }
    Ok(events)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IdStabilityReport {
    pub live_event_count: usize,
    pub replay_full_event_count: usize,
    pub replay_tail1_event_count: usize,
    pub live_message_ids: Vec<String>,
    pub replay_full_message_ids: Vec<String>,
    pub replay_tail1_message_ids: Vec<String>,
    pub message_ids_stable: bool,
    pub message_ids_order_preserved: bool,
    pub live_tool_call_ids: Vec<String>,
    pub replay_tool_call_ids: Vec<String>,
    pub tool_call_ids_stable: bool,
    pub update_kinds_live: Vec<String>,
    pub update_kinds_replay_full: Vec<String>,
    pub update_kinds_replay_tail1: Vec<String>,
    pub kinds_only_in_live: Vec<String>,
    pub kinds_only_in_replay: Vec<String>,
    pub replay_tail1_covers: String,
}

pub fn analyze_stability(
    live_path: &Path,
    replay_full_path: &Path,
    replay_tail1_path: &Path,
) -> anyhow::Result<IdStabilityReport> {
    let live_events = read_fixture_events(live_path)?;
    let replay_full_events = read_fixture_events(replay_full_path)?;
    let replay_tail1_events = read_fixture_events(replay_tail1_path)?;

    // Collect messageIds preserving first appearance order
    let mut live_msg_ids = Vec::new();
    let mut seen_live = HashSet::new();
    for e in &live_events {
        if let Some(ref mid) = e.message_id
            && seen_live.insert(mid.clone())
        {
            live_msg_ids.push(mid.clone());
        }
    }

    let mut replay_msg_ids = Vec::new();
    let mut seen_replay = HashSet::new();
    for e in &replay_full_events {
        if let Some(ref mid) = e.message_id
            && seen_replay.insert(mid.clone())
        {
            replay_msg_ids.push(mid.clone());
        }
    }

    let mut tail1_msg_ids = Vec::new();
    let mut seen_tail1 = HashSet::new();
    for e in &replay_tail1_events {
        if let Some(ref mid) = e.message_id
            && seen_tail1.insert(mid.clone())
        {
            tail1_msg_ids.push(mid.clone());
        }
    }

    // Collect toolCallIds
    let mut live_tool_ids = Vec::new();
    let mut seen_live_tools = HashSet::new();
    for e in &live_events {
        if let Some(ref tid) = e.tool_call_id
            && seen_live_tools.insert(tid.clone())
        {
            live_tool_ids.push(tid.clone());
        }
    }

    let mut replay_tool_ids = Vec::new();
    let mut seen_replay_tools = HashSet::new();
    for e in &replay_full_events {
        if let Some(ref tid) = e.tool_call_id
            && seen_replay_tools.insert(tid.clone())
        {
            replay_tool_ids.push(tid.clone());
        }
    }

    // Check messageId stability:
    // In live, all generated assistant / thought / tool message IDs.
    // In replay, user message ID is prepended, and assistant IDs follow.
    // Every live message ID should appear in replay in the same relative order.
    let mut message_ids_stable = true;
    for mid in &live_msg_ids {
        if !replay_msg_ids.contains(mid) {
            message_ids_stable = false;
        }
    }

    // Check relative ordering of live message IDs in replay
    let mut last_idx = 0;
    let mut message_ids_order_preserved = true;
    for mid in &live_msg_ids {
        if let Some(pos) = replay_msg_ids.iter().position(|r| r == mid) {
            if pos < last_idx {
                message_ids_order_preserved = false;
                break;
            }
            last_idx = pos;
        }
    }

    let tool_call_ids_stable = !live_tool_ids.is_empty() && live_tool_ids == replay_tool_ids;

    // Collect update kinds
    let mut live_kinds_set: HashSet<String> = HashSet::new();
    let mut live_kinds = Vec::new();
    for e in &live_events {
        if live_kinds_set.insert(e.update_type.clone()) {
            live_kinds.push(e.update_type.clone());
        }
    }

    let mut replay_kinds_set: HashSet<String> = HashSet::new();
    let mut replay_kinds = Vec::new();
    for e in &replay_full_events {
        if replay_kinds_set.insert(e.update_type.clone()) {
            replay_kinds.push(e.update_type.clone());
        }
    }

    let mut tail1_kinds_set: HashSet<String> = HashSet::new();
    let mut tail1_kinds = Vec::new();
    for e in &replay_tail1_events {
        if tail1_kinds_set.insert(e.update_type.clone()) {
            tail1_kinds.push(e.update_type.clone());
        }
    }

    let kinds_only_in_live: Vec<String> = live_kinds_set
        .difference(&replay_kinds_set)
        .cloned()
        .collect();
    let kinds_only_in_replay: Vec<String> = replay_kinds_set
        .difference(&live_kinds_set)
        .cloned()
        .collect();

    // Check replayTail:1 coverage
    let replay_tail1_covers = if replay_tail1_events.len() == replay_full_events.len() {
        "Full turn (entire single-turn history; turn boundary preserved)".to_string()
    } else {
        format!(
            "Last turn (replayed {} frames vs {} full)",
            replay_tail1_events.len(),
            replay_full_events.len()
        )
    };

    Ok(IdStabilityReport {
        live_event_count: live_events.len(),
        replay_full_event_count: replay_full_events.len(),
        replay_tail1_event_count: replay_tail1_events.len(),
        live_message_ids: live_msg_ids,
        replay_full_message_ids: replay_msg_ids,
        replay_tail1_message_ids: tail1_msg_ids,
        message_ids_stable,
        message_ids_order_preserved,
        live_tool_call_ids: live_tool_ids,
        replay_tool_call_ids: replay_tool_ids,
        tool_call_ids_stable,
        update_kinds_live: live_kinds,
        update_kinds_replay_full: replay_kinds,
        update_kinds_replay_tail1: tail1_kinds,
        kinds_only_in_live,
        kinds_only_in_replay,
        replay_tail1_covers,
    })
}

// =========================================================================
// Canonical Prompt Runner
// =========================================================================

pub struct CanonicalRunPaths {
    pub live_jsonl: PathBuf,
    pub replay_full_jsonl: PathBuf,
    pub replay_tail1_jsonl: PathBuf,
    pub export_json: PathBuf,
}

pub async fn run_canonical_turn_and_replays(
    url: &str,
    secret: &str,
    paths: &CanonicalRunPaths,
) -> anyhow::Result<IdStabilityReport> {
    // Ensure parent directories exist
    if let Some(p) = paths.live_jsonl.parent() {
        std::fs::create_dir_all(p)?;
    }
    if let Some(p) = paths.replay_full_jsonl.parent() {
        std::fs::create_dir_all(p)?;
    }
    if let Some(p) = paths.replay_tail1_jsonl.parent() {
        std::fs::create_dir_all(p)?;
    }
    if let Some(p) = paths.export_json.parent() {
        std::fs::create_dir_all(p)?;
    }

    // Clean previous files if any exist
    let _ = std::fs::remove_file(&paths.live_jsonl);
    let _ = std::fs::remove_file(&paths.replay_full_jsonl);
    let _ = std::fs::remove_file(&paths.replay_tail1_jsonl);
    let _ = std::fs::remove_file(&paths.export_json);

    let temp_dir = tempfile::tempdir()?;
    let temp_dir_str = temp_dir.path().to_str().unwrap().to_string();

    let mut redactor = Redactor::from_env();
    redactor.add(&temp_dir_str, "/tmp/honk-canonical");
    if let Ok(canon) = std::fs::canonicalize(temp_dir.path())
        && let Some(s) = canon.to_str()
        && s != temp_dir_str
    {
        redactor.add(s, "/tmp/honk-canonical");
    }
    redactor.add("cloudbox", "host");
    redactor.add("devbox", "host");

    // ---------------------------------------------------------------------
    // 1. Live Turn (Conn 1)
    // ---------------------------------------------------------------------
    let live_rec = Recorder::file(&paths.live_jsonl, redactor.clone())?;
    let mut conn1 = Upstream::initialize(url, secret.to_string(), live_rec).await?;
    conn1.subscribe(None).await?;

    let new_res = conn1
        .call(
            "session/new",
            json!({
                "cwd": &temp_dir_str,
                "mcpServers": []
            }),
            None,
            Duration::from_secs(30),
        )
        .await?;
    let sid = new_res["sessionId"]
        .as_str()
        .expect("missing sessionId")
        .to_string();
    conn1.subscribe(Some(&sid)).await?;

    conn1
        .call(
            "session/set_mode",
            json!({
                "sessionId": &sid,
                "modeId": "auto"
            }),
            Some(&sid),
            Duration::from_secs(30),
        )
        .await?;

    conn1
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

    // Enable thinking_effort: low
    conn1
        .call(
            "session/set_config_option",
            json!({
                "sessionId": &sid,
                "configId": "thinking_effort",
                "value": "low"
            }),
            Some(&sid),
            Duration::from_secs(30),
        )
        .await?;

    // Multi-step prompt: thinking + shell (`ls`) + write (`notes.md`) + edit (`notes.md`) + 3-row markdown table
    let prompt = "Think first about what to do. Then execute these steps in order:\n\
1. Run `ls` using the shell tool in the current directory.\n\
2. Create a scratch file named `notes.md` using the write tool with content:\n# Notes\nLine 1: Initial\n\
3. Edit `notes.md` using the edit tool, replacing `Line 1: Initial` with `Line 1: Modified with diff`.\n\
4. Finally, reply with a markdown answer containing a 3-row table with columns: ID, Status, Summary.";

    let prompt_req_id = conn1
        .request(
            "session/prompt",
            json!({
                "sessionId": &sid,
                "prompt": [{"type": "text", "text": prompt}]
            }),
            Some(&sid),
        )
        .await?;

    let start = Instant::now();
    let mut saw_thinking = false;
    let mut saw_shell = false;
    let mut saw_write = false;
    let mut saw_edit = false;
    let mut text = String::new();

    while start.elapsed() < Duration::from_secs(60) {
        let Some(ev) = conn1.next(Duration::from_secs(10)).await? else {
            break;
        };
        if let Event::Frame(_, frame) = ev {
            if let Some(update) = frame.pointer("/params/update") {
                let su = update.get("sessionUpdate").and_then(|v| v.as_str());
                if su == Some("agent_thought_chunk") {
                    saw_thinking = true;
                } else if su == Some("tool_call") {
                    let tool = update
                        .pointer("/_meta/goose/toolCall/toolName")
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    if tool == "shell" {
                        saw_shell = true;
                    }
                    if tool == "write" {
                        saw_write = true;
                    }
                    if tool == "edit" {
                        saw_edit = true;
                    }
                } else if su == Some("agent_message_chunk")
                    && let Some(t) = update.pointer("/content/text").and_then(|v| v.as_str())
                {
                    text.push_str(t);
                }
            }
            if frame.get("id") == Some(&prompt_req_id) {
                break;
            }
        }
    }

    assert!(saw_thinking, "Expected agent_thought_chunk in live turn");
    assert!(saw_shell, "Expected shell tool_call in live turn");
    assert!(saw_write, "Expected write tool_call in live turn");
    assert!(saw_edit, "Expected edit tool_call in live turn");
    assert!(
        text.contains("|") && text.contains("---"),
        "Expected markdown table in answer: {text}"
    );

    // Delete Conn 1
    let _ = conn1.delete().await;

    // ---------------------------------------------------------------------
    // 2. Full Replay (Conn 2)
    // ---------------------------------------------------------------------
    let replay_full_rec = Recorder::file(&paths.replay_full_jsonl, redactor.clone())?;
    let mut conn2 = Upstream::initialize(url, secret.to_string(), replay_full_rec).await?;
    conn2.subscribe(None).await?;
    conn2.subscribe(Some(&sid)).await?;

    let load_full_res = conn2
        .call(
            "session/load",
            json!({
                "sessionId": &sid,
                "cwd": &temp_dir_str,
                "mcpServers": []
            }),
            Some(&sid),
            Duration::from_secs(30),
        )
        .await?;
    assert!(load_full_res.get("modes").is_some());

    // Drain remaining trailing notifications (usage_update, available_commands_update)
    while let Ok(Ok(Some(_))) = tokio::time::timeout(
        Duration::from_millis(600),
        conn2.next(Duration::from_millis(200)),
    )
    .await
    {}

    let _ = conn2.delete().await;

    // ---------------------------------------------------------------------
    // 3. Replay with replayTail: 1 & Export (Conn 3)
    // ---------------------------------------------------------------------
    let replay_tail1_rec = Recorder::file(&paths.replay_tail1_jsonl, redactor.clone())?;
    let mut conn3 = Upstream::initialize(url, secret.to_string(), replay_tail1_rec).await?;
    conn3.subscribe(None).await?;
    conn3.subscribe(Some(&sid)).await?;

    let load_tail_res = conn3
        .call(
            "session/load",
            json!({
                "sessionId": &sid,
                "cwd": &temp_dir_str,
                "mcpServers": [],
                "_meta": {
                    "replayTail": 1
                }
            }),
            Some(&sid),
            Duration::from_secs(30),
        )
        .await?;
    assert!(load_tail_res.get("modes").is_some());

    while let Ok(Ok(Some(_))) = tokio::time::timeout(
        Duration::from_millis(600),
        conn3.next(Duration::from_millis(200)),
    )
    .await
    {}

    let _ = conn3.delete().await;

    // ---------------------------------------------------------------------
    // 4. Export Session (conn_export)
    // ---------------------------------------------------------------------
    let mut conn_export =
        Upstream::initialize(url, secret.to_string(), Recorder::disabled()).await?;
    conn_export.subscribe(None).await?;
    conn_export.subscribe(Some(&sid)).await?;

    let export_val = export_session(&mut conn_export, &sid).await?;
    let redacted_export = redactor.apply(export_val);
    std::fs::write(
        &paths.export_json,
        serde_json::to_string_pretty(&redacted_export)?,
    )?;

    let _ = conn_export.delete().await;

    // ---------------------------------------------------------------------
    // 5. Analyze ID stability
    // ---------------------------------------------------------------------
    let report = analyze_stability(
        &paths.live_jsonl,
        &paths.replay_full_jsonl,
        &paths.replay_tail1_jsonl,
    )?;

    Ok(report)
}

// =========================================================================
// Tests
// =========================================================================

#[tokio::test]
#[ignore]
async fn p6_canonical_and_id_stability_run1() -> anyhow::Result<()> {
    let secret = match std::env::var("HONK_GOOSE_SECRET") {
        Ok(s) if !s.trim().is_empty() => s.trim().to_string(),
        _ => {
            eprintln!("skipping: HONK_GOOSE_SECRET unset");
            return Ok(());
        }
    };
    let url =
        std::env::var("HONK_GOOSE_URL").unwrap_or_else(|_| "http://127.0.0.1:4180/acp".to_string());

    let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    let repo_root = manifest_dir
        .parent()
        .and_then(|p| p.parent())
        .expect("finding repo root");

    let canonical_dir = repo_root.join("fixtures/goose-1.54.0-patched.2/canonical");
    let paths = CanonicalRunPaths {
        live_jsonl: canonical_dir.join("live.jsonl"),
        replay_full_jsonl: canonical_dir.join("replay-full.jsonl"),
        replay_tail1_jsonl: canonical_dir.join("replay-tail1.jsonl"),
        export_json: canonical_dir.join("export.json"),
    };

    println!(
        "Starting P6 Run 1 (Canonical) to {}",
        canonical_dir.display()
    );
    let report = run_canonical_turn_and_replays(&url, &secret, &paths).await?;

    println!("\n=== P6 RUN 1 (CANONICAL) STABILITY REPORT ===");
    println!("Live events: {}", report.live_event_count);
    println!("Replay-full events: {}", report.replay_full_event_count);
    println!("Replay-tail1 events: {}", report.replay_tail1_event_count);
    println!("Message IDs stable: {}", report.message_ids_stable);
    println!(
        "Message IDs order preserved: {}",
        report.message_ids_order_preserved
    );
    println!("Tool Call IDs stable: {}", report.tool_call_ids_stable);
    println!("Live message IDs: {:?}", report.live_message_ids);
    println!("Replay message IDs: {:?}", report.replay_full_message_ids);
    println!("Live tool call IDs: {:?}", report.live_tool_call_ids);
    println!("Replay tool call IDs: {:?}", report.replay_tool_call_ids);
    println!("Update kinds in Live: {:?}", report.update_kinds_live);
    println!(
        "Update kinds in Replay: {:?}",
        report.update_kinds_replay_full
    );
    println!("Kinds only in Live: {:?}", report.kinds_only_in_live);
    println!("Kinds only in Replay: {:?}", report.kinds_only_in_replay);
    println!("ReplayTail:1 covers: {}", report.replay_tail1_covers);

    assert!(
        report.message_ids_stable,
        "Message IDs must be stable across live and replay"
    );
    assert!(
        report.tool_call_ids_stable,
        "Tool Call IDs must be stable across live and replay"
    );

    Ok(())
}

#[tokio::test]
#[ignore]
async fn p6_id_stability_run2() -> anyhow::Result<()> {
    let secret = match std::env::var("HONK_GOOSE_SECRET") {
        Ok(s) if !s.trim().is_empty() => s.trim().to_string(),
        _ => {
            eprintln!("skipping: HONK_GOOSE_SECRET unset");
            return Ok(());
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

    let p6_dir = repo_root.join("fixtures/goose-1.54.0-patched.2/p6");
    let paths = CanonicalRunPaths {
        live_jsonl: p6_dir.join(format!("{ts}_live.jsonl")),
        replay_full_jsonl: p6_dir.join(format!("{ts}_replay-full.jsonl")),
        replay_tail1_jsonl: p6_dir.join(format!("{ts}_replay-tail1.jsonl")),
        export_json: p6_dir.join(format!("{ts}_export.json")),
    };

    println!("Starting P6 Run 2 (Repeatability) to {}", p6_dir.display());
    let report = run_canonical_turn_and_replays(&url, &secret, &paths).await?;

    println!("\n=== P6 RUN 2 (REPEATABILITY) STABILITY REPORT ===");
    println!("Live events: {}", report.live_event_count);
    println!("Replay-full events: {}", report.replay_full_event_count);
    println!("Replay-tail1 events: {}", report.replay_tail1_event_count);
    println!("Message IDs stable: {}", report.message_ids_stable);
    println!(
        "Message IDs order preserved: {}",
        report.message_ids_order_preserved
    );
    println!("Tool Call IDs stable: {}", report.tool_call_ids_stable);
    println!("Live message IDs: {:?}", report.live_message_ids);
    println!("Replay message IDs: {:?}", report.replay_full_message_ids);
    println!("Live tool call IDs: {:?}", report.live_tool_call_ids);
    println!("Replay tool call IDs: {:?}", report.replay_tool_call_ids);
    println!("ReplayTail:1 covers: {}", report.replay_tail1_covers);

    assert!(
        report.message_ids_stable,
        "Message IDs must be stable across live and replay in Run 2"
    );
    assert!(
        report.tool_call_ids_stable,
        "Tool Call IDs must be stable across live and replay in Run 2"
    );

    Ok(())
}

#[tokio::test]
#[ignore]
async fn p6_multiturn_replay_tail_check() -> anyhow::Result<()> {
    let secret = match std::env::var("HONK_GOOSE_SECRET") {
        Ok(s) if !s.trim().is_empty() => s.trim().to_string(),
        _ => {
            eprintln!("skipping: HONK_GOOSE_SECRET unset");
            return Ok(());
        }
    };
    let url =
        std::env::var("HONK_GOOSE_URL").unwrap_or_else(|_| "http://127.0.0.1:4180/acp".to_string());

    let temp_dir = tempfile::tempdir()?;
    let cwd = temp_dir.path().to_str().unwrap().to_string();

    let mut conn1 = Upstream::initialize(&url, secret.clone(), Recorder::disabled()).await?;
    conn1.subscribe(None).await?;

    let new_res = conn1
        .call(
            "session/new",
            json!({
                "cwd": &cwd,
                "mcpServers": []
            }),
            None,
            Duration::from_secs(30),
        )
        .await?;
    let sid = new_res["sessionId"].as_str().unwrap().to_string();
    conn1.subscribe(Some(&sid)).await?;

    conn1
        .call(
            "session/set_mode",
            json!({ "sessionId": &sid, "modeId": "auto" }),
            Some(&sid),
            Duration::from_secs(30),
        )
        .await?;

    conn1
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

    // Turn 1
    let t1_id = conn1
        .request(
            "session/prompt",
            json!({ "sessionId": &sid, "prompt": [{"type": "text", "text": "Reply with only TURN1_DONE."}] }),
            Some(&sid),
        )
        .await?;
    while let Some(ev) = conn1.next(Duration::from_secs(15)).await? {
        if let Event::Frame(_, f) = ev
            && f.get("id") == Some(&t1_id)
        {
            break;
        }
    }

    // Turn 2
    let t2_id = conn1
        .request(
            "session/prompt",
            json!({ "sessionId": &sid, "prompt": [{"type": "text", "text": "Reply with only TURN2_DONE."}] }),
            Some(&sid),
        )
        .await?;
    while let Some(ev) = conn1.next(Duration::from_secs(15)).await? {
        if let Event::Frame(_, f) = ev
            && f.get("id") == Some(&t2_id)
        {
            break;
        }
    }
    let _ = conn1.delete().await;

    // Now test Replay on Conn 2 with replayTail: 1
    let mut conn2 = Upstream::initialize(&url, secret.clone(), Recorder::disabled()).await?;
    conn2.subscribe(None).await?;
    conn2.subscribe(Some(&sid)).await?;

    let load_res = conn2
        .call(
            "session/load",
            json!({
                "sessionId": &sid,
                "cwd": &cwd,
                "mcpServers": [],
                "_meta": {
                    "replayTail": 1
                }
            }),
            Some(&sid),
            Duration::from_secs(30),
        )
        .await?;

    let replay_skipped = load_res
        .pointer("/_meta/replaySkipped")
        .and_then(|v| v.as_u64());
    println!(
        "Multi-turn session with 2 turns: replayTail:1 returned _meta.replaySkipped = {replay_skipped:?}"
    );

    let mut replayed_user_chunks = Vec::new();
    while let Ok(Ok(Some(ev))) = tokio::time::timeout(
        Duration::from_millis(500),
        conn2.next(Duration::from_millis(200)),
    )
    .await
    {
        if let Event::Frame(_, f) = ev
            && f.pointer("/params/update/sessionUpdate")
                .and_then(|v| v.as_str())
                == Some("user_message_chunk")
        {
            let text = f
                .pointer("/params/update/content/text")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            replayed_user_chunks.push(text.to_string());
        }
    }
    println!("Replayed user chunks: {:?}", replayed_user_chunks);
    let _ = conn2.delete().await;

    // Assert that Turn 1 was skipped and only Turn 2 was replayed!
    assert!(
        replay_skipped.is_some() && replay_skipped.unwrap() > 0,
        "replaySkipped should be > 0 when prior turns exist"
    );
    assert_eq!(
        replayed_user_chunks.len(),
        1,
        "Exactly 1 user turn should be replayed with replayTail: 1"
    );
    assert!(
        replayed_user_chunks[0].contains("TURN2_DONE"),
        "Only Turn 2 should be replayed"
    );

    Ok(())
}

#[tokio::test]
#[ignore]
async fn p6_resource_cost() -> anyhow::Result<()> {
    let secret = match std::env::var("HONK_GOOSE_SECRET") {
        Ok(s) if !s.trim().is_empty() => s.trim().to_string(),
        _ => {
            eprintln!("skipping: HONK_GOOSE_SECRET unset");
            return Ok(());
        }
    };
    let url =
        std::env::var("HONK_GOOSE_URL").unwrap_or_else(|_| "http://127.0.0.1:4180/acp".to_string());
    let pid = get_goose_sandbox_pid()?;

    let snap_before = snapshot_resources(pid)?;
    println!(
        "BEFORE: VmRSS={:.2} MB ({} kB), Threads={}, Children={}",
        snap_before.vm_rss_mb, snap_before.vm_rss_kb, snap_before.threads, snap_before.child_count
    );

    let mut sessions = Vec::new();
    for i in 1..=5 {
        let mut upstream = Upstream::initialize(&url, secret.clone(), Recorder::disabled()).await?;
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
        let sid = new_res["sessionId"].as_str().unwrap().to_string();
        upstream.subscribe(Some(&sid)).await?;

        upstream
            .call(
                "session/set_mode",
                json!({ "sessionId": &sid, "modeId": "auto" }),
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

        let prompt_req_id = upstream
            .request(
                "session/prompt",
                json!({
                    "sessionId": &sid,
                    "prompt": [{"type": "text", "text": "Reply with only OK."}]
                }),
                Some(&sid),
            )
            .await?;

        let start = Instant::now();
        while start.elapsed() < Duration::from_secs(30) {
            let Some(ev) = upstream.next(Duration::from_secs(10)).await? else {
                break;
            };
            if let Event::Frame(_, f) = ev
                && f.get("id") == Some(&prompt_req_id)
            {
                break;
            }
        }
        println!("Activated session {i}/5 (sid={sid}, conn={conn_id})");
        sessions.push((upstream, conn_id, sid, temp_dir));
    }

    let snap_after = snapshot_resources(pid)?;
    println!(
        "AFTER 5 SESSIONS: VmRSS={:.2} MB ({} kB), Threads={}, Children={}",
        snap_after.vm_rss_mb, snap_after.vm_rss_kb, snap_after.threads, snap_after.child_count
    );
    println!("Children list: {:?}", snap_after.children);

    for (upstream, conn_id, sid, _temp) in sessions {
        let status = upstream.delete().await?;
        println!("Deleted conn {conn_id} for session {sid}: {status}");
    }

    tokio::time::sleep(Duration::from_secs(2)).await;

    let snap_deleted = snapshot_resources(pid)?;
    println!(
        "AFTER DELETING 5 CONNS: VmRSS={:.2} MB ({} kB), Threads={}, Children={}",
        snap_deleted.vm_rss_mb,
        snap_deleted.vm_rss_kb,
        snap_deleted.threads,
        snap_deleted.child_count
    );

    let delta_active_kb = snap_after.vm_rss_kb as i64 - snap_before.vm_rss_kb as i64;
    let delta_active_mb = delta_active_kb as f64 / 1024.0;
    let delta_deleted_kb = snap_deleted.vm_rss_kb as i64 - snap_before.vm_rss_kb as i64;
    let delta_deleted_mb = delta_deleted_kb as f64 / 1024.0;
    let per_session_mb = delta_active_mb / 5.0;

    println!("\nRESOURCE SUMMARY:");
    println!(
        "Initial: VmRSS = {:.2} MB, Threads = {}",
        snap_before.vm_rss_mb, snap_before.threads
    );
    println!(
        "5 active sessions: VmRSS = {:.2} MB (+{:.2} MB, ~{:.2} MB/session), Threads = {} (+{})",
        snap_after.vm_rss_mb,
        delta_active_mb,
        per_session_mb,
        snap_after.threads,
        snap_after.threads as i64 - snap_before.threads as i64
    );
    println!(
        "After delete: VmRSS = {:.2} MB (delta vs initial: {:+.2} MB), Threads = {}",
        snap_deleted.vm_rss_mb, delta_deleted_mb, snap_deleted.threads
    );
    println!(
        "Extension children: {} (developer is in-process built-in)",
        snap_after.child_count
    );

    Ok(())
}
