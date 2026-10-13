# honk chat: minimal interactive client (plan)

Bead: honk-9qf.2. Revised after adversarial review (2026-10-13).

**Goal.** Today `honk` has `list` and a read-only `tail`. Add `honk chat`, the smallest
client that lets a person hold a conversation with a goose session through honkd:

- attach to a session honkd holds, see its transcript, then stream it live;
- start a new session in a cwd (`session/new`);
- send a prompt when idle, steer when a turn is running;
- answer permission requests;
- interrupt a turn (`session/cancel`);
- detach without affecting the session.

This front-loads the useful core of M2 (design §8). It is deliberately not the M2 TUI:
no scrollback widget, no markdown rendering, no editor widget.

**Sessions are shared.** The sessions worth chatting with are also driven by automated
clients (pigeon runners, stall-watch timers). honkd does not check whose turn a
`session/cancel` targets, takes the first answer to a permission request, and counts
every attached viewer against its held-session cap. So this client must not take
destructive actions on turns it does not own, must not answer a request the human has not
seen, and must not pin capacity it is not using. Those three rules shape the design below.

## Scope decision: line-mode REPL first (recommended)

| | Line-mode REPL (`rustyline`) | ratatui TUI |
|---|---|---|
| Time to daily use | ~2-3 days | ~1-2 weeks |
| Transcript | Terminal scrollback; same renderer as `honk tail` | Own scroll model, markdown, layout cache (design §5 "TUI model") |
| Composer | rustyline: history, editing, Ctrl-D, bracketed paste | edtui |
| Reuse by M2 | The session core (Tasks 2-3) is frontend-free and is what M2 reuses | — |

Recommendation: **REPL first.** Everything hard about chatting through honkd (attach,
replay/reset dedupe, reconnect with cursors, prompt vs steer, ownership, permissions,
cancel) lives in a frontend-free core. The REPL is a thin shell over it, so M2 swaps the
frontend rather than starting over. The cost is cosmetic: plain text, and streaming is
line-granular while the composer is shown.

## Command line

```
honk chat <session-id> [--activate]   # attach (refuses an unheld session without --activate)
honk chat --new [--cwd DIR] [--ext NAME]...   # session/new in DIR (default: current directory)
   [--url ...] [--secret ...] [--thoughts]
```

Composer input:

| Input | Effect |
|---|---|
| text + Enter, no run active | `session/prompt` (fresh `_meta.honk.client_req_id`) |
| text + Enter, **own** run active | `_goose/unstable/session/steer` into that run |
| text + Enter, **another client's** run active (or ownership unknown) | queued locally; sent as `session/prompt` when the run ends (`/drop` discards the queue) |
| text + Enter, own prompt sent but its run not started | held locally; steered once the run starts |
| line ending in `\` | continue on the next line (multi-line message) |
| Ctrl-C or `/cancel` | cancel the run **only if it is this client's own** (below); otherwise print "this turn belongs to another client; `/cancel!` cancels it anyway" |
| `/cancel!` | `session/cancel` regardless of owner |
| `/rK N` | answer pending request `rK` with option N (requests are printed as `[rK]` with numbered options) |
| `/rK deny` | answer `rK` with its first `reject_*` option, else `cancelled` |
| `/N` | shorthand, accepted only when exactly one request is pending **and** it is the one most recently printed; otherwise refused with the list re-printed |
| Ctrl-D or `/quit` | detach (`session/close`) and exit. The session and any running turn continue |
| `/help` | list commands |
| `//text` | send `/text` literally |

Ctrl-C at an idle composer clears the line (rustyline drops the draft on Ctrl-C; accepted).

Output (stdout, above the composer): the existing `Renderer` lines (`> user`, assistant
text, `▸ tool ✓/✗`, optional thoughts) plus status lines prefixed `[honk]`: turn
started/ended (stopReason), steer sent / steer not consumed (text printed back),
permission request `[rK]` with options, request resolved (by whom: `_honk/request_resolved`
reason), reconnecting, detached-for-idle, errors.

## Protocol use (honk-aware viewer; see M1 plan "Downstream contract")

1. **Connect**: `x-secret-key` header, honk-aware `initialize` (`_meta.honk: {}`).
2. **Attach**: before `session/load`, check `session/list` `_meta.honk.held`. If the
   session is not held and `--activate` was not given, refuse: attaching to an unheld
   session *activates* it upstream (goose `session/load`, extensions spawned) and takes
   one of honkd's 16 held slots. Then `session/load {sessionId, _meta.honk: {after?}}` →
   replay, `_honk/replay_done`, pending requests re-offered, live.
3. **New**: `session/new {cwd, mcpServers: [], _meta.enabledExtensions}` (auto-attached;
   then `session/load` like any other so reconnects are uniform). **Never retried
   automatically** (not idempotent: a retry leaks a session holding a slot).
   `enabledExtensions` is always sent: without it goose loads every extension in the host
   config (including ones that spawn unsupervised subagents or enable further extensions).
   Default set = pigeon's (`developer`, `analyze`, `todo`, `skills`, `type: platform`);
   `--ext NAME` (repeatable) replaces it. **Fail closed**: if the reply has no
   `_meta.extensionResults`, or reports anything not requested, the client prints the
   session id and the surplus and exits without prompting. A requested extension missing
   or failed is a warning. (On patched goose the loaded set should equal the requested
   set exactly; Task 5 confirms this against the sandbox.)
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
   steers arrive as `user_message_chunk` with `_meta.goose.steer: true`. The renderer prints
   them; the client prints only a `[honk]` status line for its own input.
9. **Permissions** (`session/request_permission` server requests, goose's id): stored in
   order with a local tag `rK` (K increments per client run), printed with options.
   Answered `{"outcome":{"outcome":"selected","optionId":…}}` /
   `{"outcome":{"outcome":"cancelled"}}`. Removed on our answer, on
   `_honk/request_resolved`, and on reconnect (honkd re-offers what is still pending). A
   request id that comes back after we answered (honkd re-offers when its upstream
   answer failed) is accepted again with a fresh tag. Other server requests (elicitation
   etc.) are printed as "unsupported; answer from another client" and left unanswered.
10. **Cancel**: `session/cancel {sessionId}` notification. honkd also answers every
    pending permission request `cancelled` and fails queued/unstarted prompts — hence
    the ownership rule.
11. **Detach**: `session/close`, then close the socket. Never forwarded; only removes this
    viewer.
12. **Idle auto-detach**: after 30 min with no input, no run, no pending request and no
    prompt in flight, detach ("[honk] detached (idle); press Enter to re-attach"), so a
    forgotten terminal doesn't pin a held slot. The next input line re-attaches (from the
    cursor) before it is sent.
13. **Frames from honkd**: `_honk/reset` / `replay_done` → renderer; `_honk/gap` → status
    line; `_honk/evicted` → treated as a disconnect (reconnect, which re-activates; the
    session was held when we attached).
14. **Ids**: a frame is a response only if it has an `id` and **no** `method` (goose's
    server-request ids share the numeric space with ours).

## Reconnects

- Backoff 250 ms → 5 s (as `tail`), then `session/load` with the last cursor; the renderer's
  reset/replay dedupe prints only new text.
- Errors on the **first** attach (auth, unknown session, unheld without `--activate`) are
  fatal. On a **re**connect, `session/load` errors such as "held-session limit reached" or
  an activation failure after a goose restart are retried with backoff and shown; auth
  errors stay fatal.
- A prompt in `PromptSent` with no reply is re-sent **with the same `client_req_id`**;
  honkd joins the original (in-flight, queued, or finalizing) or replays the cached reply.
  `-32000 "honkd lost track of this client_req_id"` is terminal: print the text back, no
  retry. Not `resume_only` (that refuses the first-delivery case).
- A steer in flight at disconnect is not re-sent; "[honk] steer may not have been
  delivered" + text.
- A permission answer in flight is not re-sent; honkd re-offers the request if still
  pending.
- Known renderer gap: honkd's echo key is `synth_<crid>`, the export uses goose's message
  id, so after a `_honk/reset` the prompt line could print twice. Task 3 teaches the
  renderer to match a rebuilt user block to an already-printed synth echo by text.

## Interaction with other clients of the same session

- Another client's turn is running: human text is queued and prompted when that turn ends
  (never steered into it, see Protocol 5); Ctrl-C does not cancel it.
- Coordinator sessions force a `final_output` tool on every turn; replies show as a
  `recipe: final output` tool line plus the closing agent text. Legible rendering of that
  is M2 polish, not this plan.
- Another client prompts while the human's turn runs: it gets the busy error; its policy
  (honk-9qf.1 for stall-watch).
- Pigeon-launched sessions: pigeon's runner auto-answers permissions with `options[0]`, so
  requests usually show as resolved immediately; the tagging rule keeps `/N` from landing on
  a different request.

## Architecture (crates/honk, no new crates)

- `conn.rs` — `Conn`: WebSocket with a reader task and a writer; `request()` returns the id,
  responses come back as frames; `notify()`, `respond(id, result)`. Shares `Client::connect`'s
  handshake.
- `chat/core.rs` — **pure state machine** (no I/O, no async, injectable clock). Inputs:
  `Line(String) | Interrupt | Eof | Tick(now) | Frame(Value) | Connected | Disconnected`.
  Output: `Effect`s — `Request(method, params) | Notify | Respond(id, result) | Print |
  Status | Detach | Reattach | Exit(code)`. Owns a `Renderer` and turns its output into
  `Print`.
- `chat/run.rs` — async driver: connect/attach/reconnect, executes effects, feeds frames,
  input and ticks to the core. Input: `mpsc::Receiver<Input>`; output: a `ChatOut` trait (plain
  `Write`, or the rustyline external printer).
- `chat/repl.rs` — rustyline on a dedicated `std::thread` (not `spawn_blocking`: a
  thread blocked in `readline()` can't be cancelled and would hang runtime shutdown; exit with
  `std::process::exit` after the driver finishes). `ExternalPrinter` for output, line-buffered:
  completed lines print as they arrive; a partial line is flushed at turn end, before a
  permission prompt, and after 500 ms without new text. Non-TTY stdin → plain line reader and
  direct stdout (tests, scripts).

## Tasks

TDD; integration tests use the honkd binary + `honk-testgoose` (`StubGoose`/`FakeGoose`), as
`crates/honk/tests/tail_stub.rs` and `crates/honkd/tests/common` do.

1. **Conn.** Split socket, async request/response, server-request responses, response =
   id-without-method. `list`/`tail` unchanged (their tests pass).
2. **Chat core.** Unit tests (frames as JSON literals) for: prompt/steer selection per
   run state incl. `Unknown`; held line in the sent-not-started window, steered when the run
   starts; busy → one steer conversion (expectedRunId from data); no-run steer → one prompt;
   mismatch → printed back; ownership (own via PromptSent, own via echo crid, foreign);
   Ctrl-C own → cancel, foreign → refusal, `/cancel!` → cancel; permission tags, `/rK N`,
   `/rK deny`, `/N` accepted/refused, request resolved between display and answer,
   re-offered id; reconnect re-send with same crid, `-32000` terminal, `not_delivered` printed
   back, steer warning; unconsumed-steer detection at run end; idle auto-detach and re-attach on
   input; `_honk/evicted`; `//x`; Eof → detach + exit.
3. **Driver + `honk chat` (non-TTY).** Integration tests via honkd + StubGoose:
   (a) `--new` in a temp cwd → prompt → assistant text, turn-end status; extension check
   passes with the default set; surplus extensions → refused (FakeGoose reply);
   (b) attach to a session with history → printed once; unheld session refused without
   `--activate`;
   (c) steer during a gated turn, with a second model turn queued so the steer is actually
   consumed (as `honkd/tests/prompt_e2e.rs` does);
   (d) own turn: Ctrl-C → cancelled; foreign turn (second viewer prompted): Ctrl-C does
   nothing, `/cancel!` cancels;
   (e) permission (session mode `approve`, as `honkd/tests/pending.rs`) → `/r1 1` → tool
   runs; a legacy viewer auto-answering races `/1` → no double answer, no wrong target;
   (f) honkd `kill -9` + restart mid-turn → reconnect, no duplicated text or `> prompt` line
   (count them), turn end seen;
   (g) a second honk-aware client prompts while ours is sent-not-started → no duplicate turn.
   Includes the renderer fix for synth-echo vs exported user block.
4. **REPL frontend.** rustyline thread, external printer, line buffer, `\` continuation;
   unit tests for the line buffer; TTY path (incl. bracketed multi-line paste) checked by hand.
5. **Docs + live check.** Usage note (`docs/chat.md`) and `--help` text. By hand against a
   throwaway honkd (`--state-dir $(mktemp -d)`) in front of the patched sandbox goose
   (:4181): new session (confirm loaded extension set == requested), prompt, steer,
   permission, own cancel, Ctrl-D, re-attach. Never against the deployed honkd.

One PR for the feature; adversarial review of the diff before it.

## Out of scope (M2 and later)

Markdown/colour, scrollback widget, session picker, model/mode switching, elicitation forms,
queue mode (`_meta.honk.mode`), steer re-queue, `honk new --prompt` one-shot, terminal title.
