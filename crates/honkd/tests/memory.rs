//! Memory profile of honkd under realistic load: several sessions with large transcripts,
//! each with an attached viewer, then a goose restart (the reattach burst that OOM-killed
//! honkd on 2026-11-24, honk-9qf.37). Ignored by default; it prints honkd's VmRSS/VmHWM
//! at each phase so builds can be compared:
//!
//!   HONK_GOOSE_BIN=... cargo test -p honkd --release --test memory -- --ignored --nocapture
//!
//! Knobs: HONK_MEM_SESSIONS / _TURNS / _CHUNKS / _CHUNK_BYTES size the load,
//! HONK_MEM_SETTLE_SECS=65 waits out the malloc trim interval, and HONK_MEM_ENV="K=V,..."
//! is passed to honkd (e.g. `LD_PRELOAD=<heaptrack>/lib/heaptrack/libheaptrack_preload.so,
//! DUMP_HEAPTRACK_OUTPUT=/tmp/ht.raw`, or `MALLOC_ARENA_MAX=8` to compare allocator setups).
//!
//! The only assertion is a ceiling on peak RSS: 64 MB plus 4x the transcripts on disk.
//! Before honk-9qf.37 the default load (~17 MB of logs) peaked at ~310 MB and stayed
//! there; after it, ~70 MB, settling to ~45 MB once trimmed.

mod common;

use common::{Honkd, Viewer, new_session, prompt_params};
use honk_testgoose::{StubGoose, StubGooseConfig, StubModel, Turn};
use serde_json::{Value, json};
use std::time::Duration;

fn proc_kb(pid: u32, key: &str) -> u64 {
    let s = std::fs::read_to_string(format!("/proc/{pid}/status")).unwrap_or_default();
    s.lines()
        .find(|l| l.starts_with(key))
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|v| v.parse().ok())
        .unwrap_or(0)
}

fn report(h: &Honkd, phase: &str) -> (u64, u64) {
    let pid = h.pid().expect("honkd pid");
    let (rss, hwm) = (proc_kb(pid, "VmRSS:"), proc_kb(pid, "VmHWM:"));
    eprintln!(
        "[memory] {phase:<28} VmRSS={:>7} KB  VmHWM={:>7} KB",
        rss, hwm
    );
    (rss, hwm)
}

fn env_usize(key: &str, default: usize) -> usize {
    std::env::var(key)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "memory profile; run explicitly with --ignored --nocapture"]
async fn memory_profile_large_sessions_and_goose_restart() {
    let sessions = env_usize("HONK_MEM_SESSIONS", 8);
    let turns = env_usize("HONK_MEM_TURNS", 6);
    // Many small chunks, like real streaming: per-frame overhead is what a Value tree inflates.
    let chunk_bytes = env_usize("HONK_MEM_CHUNK_BYTES", 400);
    let chunks = env_usize("HONK_MEM_CHUNKS", 300);

    let model = StubModel::start().await;
    let config = StubGooseConfig {
        extra_config_yaml: "GOOSE_AUTO_COMPACT_THRESHOLD: 0.0\nGOOSE_CONTEXT_LIMIT: 100000000\n"
            .into(),
        ..Default::default()
    };
    let Some(mut g) = StubGoose::start_with_model_and_config_or_skip(model, config).await else {
        return;
    };
    // Profiling hook: HONK_MEM_ENV="K=V,K=V" is passed to honkd (e.g. a heaptrack LD_PRELOAD).
    let env_spec = std::env::var("HONK_MEM_ENV").unwrap_or_default();
    let env: Vec<(&str, &str)> = env_spec
        .split(',')
        .filter_map(|kv| kv.split_once('='))
        .collect();
    let h = Honkd::spawn_with_env(&g.url(), g.secret_file(), &[], &env).await;
    report(&h, "boot");

    let cwd = g.cwd().to_str().unwrap().to_string();
    let filler = |tag: &str, i: usize| -> String {
        // Varied text so nothing downstream can dedupe it.
        let mut s = String::with_capacity(chunk_bytes);
        let mut n = 0usize;
        while s.len() < chunk_bytes {
            s.push_str(&format!("{tag}-{i}-{n} lorem ipsum dolor sit amet. "));
            n += 1;
        }
        s
    };

    let mut viewers = Vec::new();
    let mut sids = Vec::new();
    for s in 0..sessions {
        let mut v = Viewer::connect(&h).await;
        let sid = new_session(&mut v, &cwd).await;
        v.call(
            "session/load",
            json!({"sessionId": sid, "cwd": ".", "mcpServers": []}),
        )
        .await;
        for t in 0..turns {
            let reply: Vec<String> = (0..chunks)
                .map(|c| filler(&format!("a{s}t{t}"), c))
                .collect();
            g.model().push([Turn::Text(reply)]);
            let prompt = (0..chunks / 2)
                .map(|c| filler(&format!("u{s}t{t}"), c))
                .collect::<String>();
            let id = v
                .request(
                    "session/prompt",
                    prompt_params(&sid, &prompt, Some(&format!("crid-{s}-{t}"))),
                )
                .await;
            let r = v.response_timeout(&id, Duration::from_secs(60)).await;
            assert!(r.get("result").is_some(), "prompt failed: {r}");
        }
        sids.push(sid);
        viewers.push(v);
    }
    let log_bytes: u64 = std::fs::read_dir(h.state_dir.path().join("sessions"))
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_name().to_string_lossy().ends_with(".jsonl"))
        .filter_map(|e| e.metadata().ok())
        .map(|m| m.len())
        .sum();
    eprintln!("[memory] session logs on disk: {} KB", log_bytes / 1024);
    report(&h, "after load (viewers live)");

    // A second viewer per session forces a full replay of every transcript.
    let mut late = Vec::new();
    for sid in &sids {
        let mut v = Viewer::connect(&h).await;
        v.call(
            "session/load",
            json!({"sessionId": sid, "cwd": ".", "mcpServers": []}),
        )
        .await;
        late.push(v);
    }
    report(&h, "after replay to 2nd viewers");

    g.restart().await;
    for v in viewers.iter_mut().chain(late.iter_mut()) {
        v.wait_for(|f| {
            f.get("method").and_then(Value::as_str) == Some(honk_wire::method::REPLAY_DONE)
                && f.pointer("/params/epoch").and_then(Value::as_u64) >= Some(2)
        })
        .await;
    }
    let (_, hwm) = report(&h, "after goose restart rebuild");
    // Past honkd::malloc::TRIM_INTERVAL to see freed pages go back: HONK_MEM_SETTLE_SECS=65.
    tokio::time::sleep(Duration::from_secs(
        env_usize("HONK_MEM_SETTLE_SECS", 3) as u64
    ))
    .await;
    let (rss, _) = report(&h, "settled");

    drop(late);
    drop(viewers);
    let ceiling_kb = 64 * 1024 + 4 * log_bytes / 1024;
    eprintln!("[memory] ceiling {} KB (64 MB + 4x logs)", ceiling_kb);
    assert!(
        hwm < ceiling_kb,
        "honkd peak {hwm} KB exceeds {ceiling_kb} KB for {} KB of logs",
        log_bytes / 1024
    );
    let _ = rss;
}
