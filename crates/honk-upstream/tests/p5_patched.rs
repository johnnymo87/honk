mod common;
use common::*;

/// P5a′: session/cancel must leave no shell descendant alive (patch 1).
#[tokio::test]
#[ignore]
async fn p5a_prime_cancel_leaves_no_orphans() -> anyhow::Result<()> {
    let Some(mut ts) =
        setup_session_at(&goose_url(), FIXTURES_PATCHED_3, "p5a_prime", "auto").await?
    else {
        return Ok(());
    };
    require_patched(&ts.upstream)?;
    let prompt = "Run the shell command \"sleep 61 & sleep 62; echo P5A_PRIME_DONE\", then reply with only the word FINISHED.";
    let prompt_id = ts
        .upstream
        .request(
            "session/prompt",
            json!({"sessionId": &ts.sid, "prompt": [{"type": "text", "text": prompt}]}),
            Some(&ts.sid),
        )
        .await?;

    let start = Instant::now();
    let (a, b) = loop {
        anyhow::ensure!(
            start.elapsed() < Duration::from_secs(60),
            "sleeps never started"
        );
        let _ = ts.upstream.next(Duration::from_millis(200)).await?;
        let (a, b) = (pgrep_user("sleep 61")?, pgrep_user("sleep 62")?);
        if !a.is_empty() && !b.is_empty() {
            break (a, b);
        }
    };

    let t_cancel = Instant::now();
    ts.upstream
        .notify(
            "session/cancel",
            json!({"sessionId": &ts.sid}),
            Some(&ts.sid),
        )
        .await?;
    let mut stop_reason = None;
    while t_cancel.elapsed() < Duration::from_secs(10) {
        if let Some(Event::Frame(_, f, _)) = ts.upstream.next(Duration::from_millis(200)).await?
            && f.get("id").is_some_and(|id| rpc_id_matches(&prompt_id, id))
        {
            stop_reason = f
                .pointer("/result/stopReason")
                .and_then(|v| v.as_str())
                .map(str::to_owned);
            break;
        }
    }
    let cancel_ms = t_cancel.elapsed().as_millis();

    let mut survivors = Vec::new();
    for _ in 0..15 {
        survivors = [pgrep_user("sleep 61")?, pgrep_user("sleep 62")?].concat();
        survivors.sort_unstable();
        survivors.dedup();
        if survivors.is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    ts.rec.record_marker(
        "probe",
        json!({
            "probe": "p5a_prime", "initial_pids": [a, b], "cancel_ms": cancel_ms,
            "stop_reason": stop_reason, "survivors_after_3s": survivors,
        })
        .to_string(),
    );
    for pid in &survivors {
        let _ = std::process::Command::new("kill")
            .args(["-9", &pid.to_string()])
            .status();
    }
    assert_eq!(stop_reason.as_deref(), Some("cancelled"));
    assert!(
        survivors.is_empty(),
        "orphaned shell descendants survived cancel: {survivors:?}"
    );
    Ok(())
}
