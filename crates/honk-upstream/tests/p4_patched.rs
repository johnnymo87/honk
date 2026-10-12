mod common;
use common::*;
use honk_upstream::{ReplayOutcome, SubscribeOpts};
use std::collections::HashMap;

/// P4b′: drop right after the tool_call, before processing the permission request;
/// resume with Last-Event-ID; the permission must arrive and answering it must finish.
#[tokio::test]
#[ignore]
async fn p4b_prime_lost_permission_is_replayed() -> anyhow::Result<()> {
    let Some(mut ts) =
        setup_session_at(&goose_url(), FIXTURES_PATCHED_3, "p4b_prime", "approve").await?
    else {
        return Ok(());
    };
    let prompt = "Run the shell command \"echo P4OK\", then reply with only the word FINISHED.";
    let prompt_id = ts
        .upstream
        .request(
            "session/prompt",
            json!({"sessionId": &ts.sid, "prompt": [{"type": "text", "text": prompt}]}),
            Some(&ts.sid),
        )
        .await?;

    // Process frames up to and including the tool_call; never look past it.
    let mut last_ids: HashMap<StreamKey, u64> = HashMap::new();
    let start = Instant::now();
    loop {
        anyhow::ensure!(start.elapsed() < Duration::from_secs(60), "no tool_call");
        let Some(Event::Frame(key, frame, id)) = ts.upstream.next(Duration::from_secs(60)).await?
        else {
            anyhow::bail!("stream ended before tool_call");
        };
        anyhow::ensure!(
            !is_permission_request(&frame),
            "permission arrived before tool_call; re-run"
        );
        if let Some(id) = id {
            last_ids.insert(key, id);
        }
        if is_tool_call_chunk(&frame) {
            break;
        }
    }
    // Simulate a client crash: whatever the reader parsed after the tool_call is
    // discarded unprocessed, along with the old Upstream.
    ts.upstream.hard_drop();
    tokio::time::sleep(Duration::from_secs(2)).await;

    let mut resumed = Upstream::resume(
        &goose_url(),
        ts.secret.clone(),
        ts.conn_id.clone(),
        ts.rec.clone(),
    )?;
    let sess_last = last_ids.get(&StreamKey::Session(ts.sid.clone())).copied();
    anyhow::ensure!(
        sess_last.is_some(),
        "no session-stream id processed before the drop"
    );
    resumed
        .subscribe_with(
            None,
            SubscribeOpts {
                takeover: true,
                resume_after: Some(last_ids.get(&StreamKey::Conn).copied().unwrap_or(0)),
            },
        )
        .await?;
    let sess = resumed
        .subscribe_with(
            Some(&ts.sid),
            SubscribeOpts {
                takeover: true,
                resume_after: sess_last,
            },
        )
        .await?;

    let mut permission = None;
    let t_wait = Instant::now();
    while permission.is_none() && t_wait.elapsed() < Duration::from_secs(10) {
        if let Some(Event::Frame(_, frame, _)) = resumed.next(Duration::from_millis(500)).await?
            && is_permission_request(&frame)
        {
            permission = Some(frame);
        }
    }
    let permission =
        permission.ok_or_else(|| anyhow::anyhow!("permission request was not replayed"))?;
    let perm_id = permission.get("id").cloned().expect("request has an id");
    resumed
        .respond(perm_id.clone(), allow_once_outcome(), Some(&ts.sid))
        .await?;

    let mut prompt_reply = false;
    let t_end = Instant::now();
    while !prompt_reply && t_end.elapsed() < Duration::from_secs(60) {
        if let Some(Event::Frame(_, frame, _)) = resumed.next(Duration::from_secs(5)).await?
            && frame
                .get("id")
                .is_some_and(|id| rpc_id_matches(&prompt_id, id))
            && frame.get("result").is_some()
        {
            prompt_reply = true;
        }
    }
    let export = export_session(&mut resumed, &ts.sid).await?;
    let tool_ran = export.to_string().contains("P4OK");
    let finished = extract_all_assistant_text(&export).contains("FINISHED");
    ts.rec.record_marker(
        "probe",
        json!({
            "probe": "p4b_prime", "session_resume_after": sess_last,
            "session_replay": format!("{:?}", sess.replay), "permission_replayed": true,
            "prompt_reply": prompt_reply, "tool_ran": tool_ran, "finished": finished,
        })
        .to_string(),
    );
    assert_eq!(sess.replay, ReplayOutcome::Complete);
    assert!(
        prompt_reply && tool_ran && finished,
        "answering the replayed permission must finish the run"
    );
    Ok(())
}
