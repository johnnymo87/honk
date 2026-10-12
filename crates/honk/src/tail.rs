//! Session tailing logic with reconnect backoff and cursor resume.

use crate::client::{Client, normalize_ws_url};
use crate::render::Renderer;
use honk_wire::Cursor;
use serde_json::{Value, json};
use std::io::Write;
use std::time::Duration;

const MIN_BACKOFF: Duration = Duration::from_millis(250);
const MAX_BACKOFF: Duration = Duration::from_secs(5);

#[derive(Clone, Debug)]
pub struct TailOpts {
    pub url: String,
    pub secret: String,
    pub session: String,
    pub text_only: bool,
    pub thoughts: bool,
    pub exit_on_turn_end: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TailOutcome {
    pub resets: u32,
    pub reconnect_resets: u32,
}

enum RunOutcome {
    TurnEnded,
    Disconnected,
}

/// Runs the tail loop:
/// - Connects to honkd via WebSocket with header auth and honk-aware initialize.
/// - Loads the session resuming from the last cursor (`_meta.honk.after`).
/// - Renders frames streaming to `out`.
/// - On disconnect, reconnects with backoff (250 ms -> 5 s) resuming from the last cursor.
/// - When `exit_on_turn_end` is true, exits after `activeRunId` goes from non-null to null.
pub async fn run<W: Write>(opts: TailOpts, mut out: W) -> anyhow::Result<TailOutcome> {
    let mut renderer = Renderer::new(opts.text_only, opts.thoughts);
    let mut last_cursor: Option<Cursor> = None;
    let mut current_backoff = MIN_BACKOFF;

    let mut had_active_run = false;
    let mut consecutive_failures = 0;
    let mut is_reconnect = false;
    let mut reconnect_resets = 0;

    loop {
        let resets_before = renderer.reset_count();
        match run_once(
            &opts,
            &mut renderer,
            &mut last_cursor,
            &mut had_active_run,
            &mut current_backoff,
            &mut out,
        )
        .await
        {
            Ok(RunOutcome::TurnEnded) => {
                if is_reconnect {
                    reconnect_resets += renderer.reset_count().saturating_sub(resets_before);
                }
                renderer.flush_open_line(&mut out)?;
                return Ok(TailOutcome {
                    resets: renderer.reset_count(),
                    reconnect_resets,
                });
            }
            Ok(RunOutcome::Disconnected) => {
                if is_reconnect {
                    reconnect_resets += renderer.reset_count().saturating_sub(resets_before);
                }
                is_reconnect = true;
                consecutive_failures = 0;
            }
            Err(e) => {
                if is_reconnect {
                    reconnect_resets += renderer.reset_count().saturating_sub(resets_before);
                }
                let err_str = e.to_string();
                if let Some(msg) = err_str.strip_prefix("fatal: ") {
                    eprintln!("honk: {msg}");
                    return Err(anyhow::anyhow!("{msg}"));
                }
                consecutive_failures += 1;
                if consecutive_failures >= 3 {
                    eprintln!(
                        "honk: connection failed ({consecutive_failures} attempts), retrying in {current_backoff:?}..."
                    );
                }
            }
        }

        tokio::time::sleep(current_backoff).await;
        current_backoff = (current_backoff * 2).min(MAX_BACKOFF);
    }
}

async fn run_once<W: Write>(
    opts: &TailOpts,
    renderer: &mut Renderer,
    last_cursor: &mut Option<Cursor>,
    had_active_run: &mut bool,
    current_backoff: &mut Duration,
    out: &mut W,
) -> anyhow::Result<RunOutcome> {
    let ws_url = normalize_ws_url(&opts.url);
    let mut client = Client::connect(&ws_url, &opts.secret).await?;

    let load_params = match last_cursor {
        Some(cursor) => json!({
            "sessionId": opts.session,
            "_meta": {
                "honk": {
                    "after": {
                        "epoch": cursor.epoch,
                        "seq": cursor.seq,
                    }
                }
            }
        }),
        None => json!({
            "sessionId": opts.session,
            "_meta": {
                "honk": {}
            }
        }),
    };

    let load_req_id = client.request("session/load", load_params).await?;
    let mut initial_replay_done = false;
    let was_running_before_reconnect = *had_active_run;
    let mut ended_during_replay = false;

    while let Some(frame) = client.next_frame().await? {
        // Check for session/load response
        if frame.get("id") == Some(&json!(load_req_id)) {
            if let Some(err) = frame.get("error") {
                let err_msg = err
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown error");
                anyhow::bail!("fatal: session/load failed: {err_msg}");
            }
            // Successful session/load: reset backoff to MIN_BACKOFF immediately
            *current_backoff = MIN_BACKOFF;
        }

        // Track cursor
        if let Some(c) = honk_wire::frame_cursor(&frame) {
            *last_cursor = Some(c);
        } else if frame.get("method").and_then(Value::as_str)
            == Some(honk_wire::method::REPLAY_DONE)
        {
            if let (Some(epoch), Some(seq)) = (
                frame.pointer("/params/epoch").and_then(Value::as_u64),
                frame.pointer("/params/seq").and_then(Value::as_u64),
            ) {
                *last_cursor = Some(Cursor { epoch, seq });
            }
            initial_replay_done = true;
            if ended_during_replay {
                // The in-flight run ended during the replay frames; exit now that replay is complete
                renderer.handle_frame(&frame, out)?;
                if opts.exit_on_turn_end {
                    return Ok(RunOutcome::TurnEnded);
                }
                continue;
            }
        }

        // Check run state for activeRunId transitions
        let mut turn_ended = false;
        if let Some(u) = frame.pointer("/params/update")
            && let Some(rs) = honk_wire::run_state(u)
        {
            match rs {
                Some(_run_id) => {
                    *had_active_run = true;
                }
                None => {
                    if initial_replay_done {
                        if *had_active_run {
                            turn_ended = true;
                        }
                        *had_active_run = false;
                    } else {
                        // Received during replay before this connection's replay_done
                        if was_running_before_reconnect && *had_active_run {
                            ended_during_replay = true;
                        }
                        *had_active_run = false;
                    }
                }
            }
        }

        // Feed frame to renderer
        renderer.handle_frame(&frame, out)?;

        if opts.exit_on_turn_end && turn_ended {
            return Ok(RunOutcome::TurnEnded);
        }
    }

    Ok(RunOutcome::Disconnected)
}
