# honk chat: TUI client (plan)

Bead: honk-9qf.2. Revised 2026-10-13: the author chose a ratatui TUI over a line-mode
REPL, wants no permission prompts, no extension friction, and >30 concurrently held
sessions. Earlier revisions (REPL) were adversarially reviewed; the protocol sections
below carry that review's fixes.

**Goal.** `honk chat`: a full-screen ratatui client for a goose session held by honkd.
Attach to any session (or start one in a cwd), watch its transcript live, including
turns started by other clients (pigeon, stall-watch coordinators), prompt, steer your
own turns, interrupt. This is the first, daily-usable slice of milestone M2 (design §8)
and follows design §2 requirements and §5 "TUI model". OpenCode stays primary until M2
exits.

**Sessions are shared.** The sessions worth chatting with are also driven by automated
clients. honkd does not check whose turn a `session/cancel` targets and takes the first
answer to a permission request. So the client never cancels a turn it does not own
(without an explicit `/cancel!`), never steers into another client's turn, and never
answers another client's permission requests.

## Scope of this bead (first PR) vs later

**In (TUI v1):**
- `honk chat <session-id>`, `honk chat --new [--cwd DIR]`.
- Layout: transcript pane / one-line status bar / composer (grows to ~8 lines).
- Transcript: user, assistant (markdown via `tui-markdown`, newline-gated streaming:
  committed lines are markdown, the incomplete tail is plain), thinking (dim, collapsed to
  one line by default, `zt` toggles all), tool calls (one row: status glyph + title;
  `zo`/`zc` on the focused block or `za` all expands input/output text), honk notices
  (dim `[honk] …` rows). Blocks keyed by `BlockKey`; `_honk/reset` rebuilds the model and
  keeps the viewport anchor if the block still exists.
- Viewport: layout cache per (block, width); follow-tail unless the user scrolled away;
  anchor = (block key, line offset). Keys in normal mode: `ctrl-e/y/d/u/f/b`, `gg`/`G`
  (when the composer is empty), plus mouse wheel.
- Composer: `edtui` (vim modes). Insert mode: Enter submits, Alt-Enter / Shift-Enter
  newline; bracketed paste inserts verbatim. Normal mode: edtui grammar; app keys above
  only where edtui has no binding. Prompt history: `ctrl-p`/`ctrl-n`.
- Status bar: session title (or id), cwd, run state (`idle` / `running` / `running
  (other client)` / `queued: N`), connection state (`live` / `reconnecting…`).
- Ctrl-C: own run → cancel; foreign → notice "turn belongs to another client; `/cancel!`";
  idle → clears the composer. Quit: `:q` in normal mode or Ctrl-D on an empty composer.
  Slash commands in the composer: `/cancel`, `/cancel!`, `/drop`, `/quit`, `/help`,
  `//text`.
- Terminal title = session title. Panic hook + Drop guard restore the terminal.
- Permissions: none shown. Requests in own turns are auto-approved (`allow_always` >
  `allow_once` > first option) with a notice; others' requests are left to their owner
  (notice only).
- `--new`: `session/new {cwd, mcpServers: []}` **without** `_meta.enabledExtensions`, so
  goose loads every extension enabled in the host goose config (author's choice: maximum
  experimentation). `--ext NAME` (repeatable) narrows to named platform extensions. The
  loaded set (`_meta.extensionResults`) is shown as a notice. Never retried automatically.
- honkd: default `--max-sessions` 16 → 64 (per-session goose cost measured at
  0.5–1.7 MB with builtin extensions; external MCP extensions add per-session processes,
  so re-measure if the host config enables many).

**Later (own beads):** syntax highlighting in code blocks, table holdback, unified diffs
for edit tools, model switch, per-session extension toggle, cost/context readout, inline
subagent progress, elicitation forms, session picker / nvim switcher, scroll-to-message,
restore after nightly reset, `$EDITOR` compose, `final_output` (recipe) reply rendering.

## Protocol use (honk-aware viewer; see M1 plan "Downstream contract")

All wire ids come from the core's single allocator (`attach_request`, `allocate_id`);
`client_req_id`s are `honk-chat-<random seed>-<n>` (honkd's ledger is server-wide, 24 h).

1. **Connect**: `x-secret-key`, honk-aware `initialize` (`_meta.honk: {}`).
2. **Attach**: `session/load {sessionId, _meta.honk: {after?}}` → replay (history: shown,
   but no live side effects), `_honk/replay_done`, pending requests re-offered, live.
   Attaching to a session honkd does not hold activates it upstream (goose loads it and
   spawns its extensions) and holds a slot; no gate (author: >30 held is fine; cap 64).
3. **New**: see scope above.
4. **Run state**: `Unknown | Idle | Running(run_id)`, from
   `session_info_update._meta.goose.activeRunId` (`honk_wire::run_state`). It starts
   `Unknown`: honk-aware viewers get no synthesized run state on attach, and a log rebuilt
   from export has none. `Unknown` behaves like Idle for input (send a prompt; a busy
   reply reveals the run).
5. **Prompt**: `session/prompt {sessionId, prompt:[{type:text,text}], _meta.honk.client_req_id}`.
   One own prompt in flight at a time (state `PromptSent{crid, text, req_id, run_seen}`).
   The reply arrives at turn end (stopReason; `_meta.honk.text` unused — the transcript
   already shows it).
   - Busy reply (detected by the `data` text "session already has active run", not by
     the code: `-32602` is also used for other errors) → the run is foreign: the text goes
     to the local queue and is re-prompted (fresh crid) when `activeRunId` goes null. The
     prompt leaves the in-flight slot immediately (its ledger entry is gone). **Never steer
     into a foreign run**: in recipe/`final_output` sessions (stall-watch coordinators) goose
     drains pending steers and then ends the turn without another model call, so the steer
     is recorded as consumed but never answered (goose `agent.rs` loop; honk-9qf.1 design
     §3.2).
   - Reply `stopReason: cancelled` with `_meta.honk.reason` `not_delivered` or
     `upstream_restart` → "[honk] your message did not run:" + text printed back.
6. **Steer**: `_goose/unstable/session/steer {sessionId, expectedRunId, prompt:[…]}`.
   - Only into own runs. "no active run to steer" → convert once to a prompt.
   - run-id mismatch → **not** retargeted: print the text back ("the turn you were
     steering ended; re-send to send it to the current one").
   - At most one prompt↔steer conversion per line; after that the text is printed back.
   - The steer reply's `messageId` is remembered; at run end, any accepted steer that
     never appeared as a `user_message_chunk` with that id is reported unconsumed with
     its text (works across reconnects, unlike `_honk/steer_unconsumed`, which goes only
     to the sending socket; that notification is also honoured).
7. **Ownership**: a run is *own* if this client's prompt is in `PromptSent` and its run
   was announced while in that state, or the run's `user_message_chunk` echo carries
   `_meta.honk.source.client_req_id` equal to one of this client's crids. Only own runs
   are cancelled by Ctrl-C / `/cancel`.
8. **Echo**: goose does not echo the sender's prompt; honkd synthesizes a
   `user_message_chunk` (`synth_<crid>`, `_meta.honk.source`) when the run starts. Consumed
   steers arrive as `user_message_chunk` with `_meta.goose.steer: true`. The transcript shows
   them; the client adds only a notice for its own input.
9. **Permissions**: auto-approve in own turns, nothing for others' (scope above). Other
   server requests (elicitation) → notice "answer it from another client", unanswered.
10. **Cancel**: `session/cancel {sessionId}` notification. honkd also answers every
    pending permission request `cancelled` and fails queued/unstarted prompts — hence
    the ownership rule.
11. **Detach**: `session/close`, then close the socket. Never forwarded; only removes this
    viewer.
13. **Frames from honkd**: `_honk/reset` / `replay_done` → transcript model; `_honk/gap` → status
    line; `_honk/evicted` → treated as a disconnect (reconnect, which re-activates; the
    session was held when we attached).
14. **Ids**: a frame is a response only if it has an `id` and **no** `method` (goose's
    server-request ids share the numeric space with ours).

## Reconnects

- Backoff 250 ms → 5 s (as `tail`), then `session/load` with the last cursor; the transcript model
  rebuilds on reset.
- Errors on the **first** attach (auth, unknown session) are
  fatal. On a **re**connect, `session/load` errors such as "held-session limit reached" or
  an activation failure after a goose restart are retried with backoff and shown; auth
  errors stay fatal.
- A prompt in `PromptSent` with no reply is re-sent **with the same `client_req_id`**;
  honkd joins the original (in-flight, queued, or finalizing) or replays the cached reply.
  `-32000 "honkd lost track of this client_req_id"` is terminal: print the text back, no
  retry. Not `resume_only` (that refuses the first-delivery case).
- A steer in flight at disconnect is not re-sent; "[honk] steer may not have been
  delivered" + text.
- Duplicate user block after a rebuild: handled in the transcript model (Task 4).

## Interaction with other clients of the same session

- Another client's turn is running: human text is queued and prompted when that turn ends
  (never steered into it, see Protocol 5); Ctrl-C does not cancel it.
- Coordinator sessions force a `final_output` tool on every turn; replies show as a
  `recipe: final output` tool line plus the closing agent text. Legible rendering of that
  is a later bead.
- Another client prompts while the human's turn runs: it gets the busy error; its policy
  (honk-9qf.1 for stall-watch).
- Pigeon-launched sessions: pigeon's runner auto-answers its own turns' permission
  requests; this client auto-answers only its own turns' requests.

## Architecture (crates/honk)

- `conn.rs` (done) — split WebSocket; `send_request(id, …)`, `notify`, `respond`.
- `chat/core.rs` (done) — pure protocol state machine. Inputs `Line | Interrupt | Eof |
  Tick | Frame | Connected | Disconnected`; effects `Request | Notify | Respond |
  Transcript(frame) | Status | Detach | Reattach | Exit`. Idle auto-detach is disabled in
  the TUI (the live view of other clients' turns is the point).
- `chat/driver.rs` — async: connect, attach, reconnect with backoff, run effects, feed
  frames/input/ticks to the core; exposes a channel of `UiEvent`s (transcript frames,
  notices, run/connection state) and accepts `UiCommand`s. No terminal code: testable
  headless against honkd + stub goose.
- `tui/transcript.rs` — block model fed by transcript frames (reset/replay aware),
  markdown rendering with newline-gated streaming, layout cache, viewport/anchor/follow.
  Pure; tested with ratatui `TestBackend` / buffer assertions.
- `tui/app.rs` — terminal setup/restore, event loop (crossterm `EventStream` + driver
  events + tick), focus and key map, edtui composer, status bar, history, title.

## Tasks

1. ✅ Conn. 2. ✅ Core (incl. auto-approve, replay phase, race fixes).
3. **Driver.** Integration tests via honkd binary + `honk-testgoose` (as
   `crates/honk/tests/tail_stub.rs` / `crates/honkd/tests/common`): `--new` → prompt →
   assistant text and turn end; attach to a session with history (shown once, replay not
   live); steer into own gated turn (a second model turn queued so it is consumed); a
   second viewer's turn → our line queued, prompted after it ends; Ctrl-C own → cancelled,
   foreign → not; permission in own turn (session mode `approve`) auto-approved; honkd
   `kill -9` + restart mid-turn → reconnect, same crid, turn end seen, no duplicate user
   block.
4. **Transcript model + rendering.** Streaming markdown (incomplete fences, inline code
   across chunks, long lines), tool rows + expansion, thoughts toggle, reset rebuild keeps
   anchor, follow vs scrolled, resize invalidation, synth-echo vs exported user block
   dedupe (honkd's echo key `synth_<crid>` vs goose's message id after a rebuild).
5. **App shell + `honk chat` CLI.** edtui composer and key map, status bar, history,
   slash commands, terminal title, restore on panic. Key handling unit-tested by feeding
   crossterm events to the app with a `TestBackend`.
6. **honkd max-sessions 64, docs (`docs/chat.md`), live check** against a throwaway honkd
   (`--state-dir $(mktemp -d)`) in front of the patched sandbox goose (:4181) in a real
   terminal (tmux): new session, prompt, steer, cancel, quit, re-attach, a second client's
   turn. Never against the deployed honkd.

One PR; adversarial review of the diff before it.
