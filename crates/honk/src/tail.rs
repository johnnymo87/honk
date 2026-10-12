//! Session tailing logic with reconnect backoff and cursor resume.

use crate::client::{Client, normalize_ws_url};
use crate::render::Renderer;
use honk_wire::Cursor;
use serde_json::{Value, json};
use std::io::Write;
use std::time::Duration;

#[derive(Clone, Debug)]
pub struct TailOpts {
    pub url: String,
    pub secret: String,
    pub session: String,
    pub text_only: bool,
    pub thoughts: bool,
    pub exit_on_turn_end: bool,
}

enum TailOutcome {
    TurnEnded,
    Disconnected,
}

/// Runs the tail loop:
/// - Connects to honkd via WebSocket with header auth and honk-aware initialize.
/// - Loads the session resuming from the last cursor (`_meta.honk.after`).
/// - Renders frames streaming to `out`.
/// - On disconnect, reconnects with backoff (250 ms -> 5 s) resuming from the last cursor.
/// - When `exit_on_turn_end` is true, exits after `activeRunId` goes from non-null to null.
pub async fn run<W: Write>(opts: TailOpts, mut out: W) -> anyhow::Result<()> {
    let mut renderer = Renderer::new(opts.text_only, opts.thoughts);
    let mut last_cursor: Option<Cursor> = None;
    let mut current_backoff = Duration::from_millis(250);
    const MIN_BACKOFF: Duration = Duration::from_millis(250);
    const MAX_BACKOFF: Duration = Duration::from_secs(5);

    let mut had_active_run = false;

    loop {
        match run_once(
            &opts,
            &mut renderer,
            &mut last_cursor,
            &mut had_active_run,
            &mut out,
        )
        .await
        {
            Ok(TailOutcome::TurnEnded) => {
                return Ok(());
            }
            Ok(TailOutcome::Disconnected) => {
                current_backoff = MIN_BACKOFF;
            }
            Err(_e) => {
                // Connection or handshake failure; backoff and retry
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
    out: &mut W,
) -> anyhow::Result<TailOutcome> {
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
    let mut initial_replay_done = last_cursor.is_some();

    while let Some(frame) = client.next_frame().await? {
        // Check for session/load response error
        if frame.get("id") == Some(&json!(load_req_id))
            && let Some(err) = frame.get("error")
        {
            anyhow::bail!("session/load error: {err}");
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
                    if *had_active_run && initial_replay_done {
                        turn_ended = true;
                    }
                    *had_active_run = false;
                }
            }
        }

        // Feed frame to renderer
        renderer.handle_frame(&frame, out)?;

        if opts.exit_on_turn_end && turn_ended {
            return Ok(TailOutcome::TurnEnded);
        }
    }

    Ok(TailOutcome::Disconnected)
}
