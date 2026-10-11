# honk: a ratatui TUI and session relay for goose over ACP

Status: design approved 2026-10-10, revised after M0 spike (2026-10-11), see m0-findings.md.
Supporting research: [`2026-10-10-honk-research.md`](2026-10-10-honk-research.md) (deep-research survey of
ACP clients, ratatui components, relay architecture, effort).

## 1. Why

The author is moving daily coding-agent work from a heavily patched OpenCode v1 to
[goose](https://github.com/aaif-goose/goose). OpenCode v2 (promoted to its main branch
2026-10-10) breaks the plugins, HTTP integrations, DB tooling and multi-serve topology the
author depends on. Goose is already in use headlessly via `goose serve` (ACP over
WebSocket).

What goose lacks is a terminal UI the author likes. Its CLI is line-oriented
(rustyline/cliclack), and upstream removed its TypeScript ACP TUI in mid-2026, pointing
users at third-party ACP clients. None of the surveyed clients supports the workflow
below, chiefly *thin-client reattach from many terminals to long-lived sessions, with
live visibility of turns started by other clients*.

## 2. Requirements

Derived from an interview plus 60 days of usage data from the author's OpenCode
session DB (~90 sessions/day, ~48% subagent sessions, every root session on one agent,
permissions allow-all, question tool ~3/day, sidebar hidden, slash commands / @file /
agent cycling / share / fork / undo unused).

**MVP**
- Thin client: attach / detach / reattach any session from any terminal tab (nvim
  terminal buffers, tmux); several viewers per session.
- Live view of turns started by *other* clients, especially
  [pigeon](https://github.com/johnnymo87/pigeon), which injects inter-session swarm
  messages (~24% of root user messages).
- Prompt composer with vim modes via `edtui` (superset of today's patched vim:
  `i I a A o O S`, `h j k l 0 ^ $ w b e W B E`, `x dd dw cc cw`; Enter submits).
- Normal-mode transcript navigation: ctrl-e/y/d/u/f/b, gg/G.
- Streamed markdown rendering incl. tables, syntax-highlighted code, good colors.
- Thinking display; tool-call details; unified diffs for edits (word-wrapped).
- Question / form (elicitation) and permission UI.
- Interrupt (cancel turn); in-session model switch; per-session MCP extension toggle;
  prompt history recall.
- Cost / context readout.
- Ecosystem hooks: nvim telescope session switcher, headless "launch session with
  prompt in dir", restore attached TUIs after a nightly reset, terminal title = session
  name, external "scroll to message".
- Inline subagent progress in the parent transcript (cheap: goose emits it as
  `tool_call_update` with `_meta.toolNotification`, logger `subagent:<id>`).

**Post-MVP**: drill into subagent sessions; edit prompt in real nvim (`$EDITOR`);
"dump transcript to nvim" key.

**Non-goals**: sidebar, themes, share, fork/timeline, undo/revert, agent cycling, @file
autocomplete, slash-command palette, LSP, images, inline (scrollback) screen mode,
advertising ACP `terminal/*` or `fs/*` client capabilities, forking goose.

## 3. Key facts about goose (v1.54.0) that shape the design

Verified by reading source (`git show v1.54.0:<path>`) and empirical probe measurements (`m0-findings.md`).

- **Transports.** `goose serve --port N` exposes ACP at `/acp` over WebSocket **and**
  HTTP+SSE (`agent-client-protocol-http` 2.2.0). Auth is the `X-Secret-Key` header or a
  `?token=` query param (`crates/goose/src/acp/transport/auth.rs:20-31`), not a bearer
  token. Use the header.
- **One run per session, server-wide.** All connections share one `ActiveRunRegistry`
  (`server_factory.rs`); a second prompt during a run is refused with "use steer"
  (`server.rs:1899-1920`). `_goose/unstable/session/steer` is compare-and-swap on the
  run id and reaches a run owned by another connection (`server.rs:2378-2418`).
  `session/cancel` is a **notification** (not a request) and also crosses connections
  (`dispatch.rs:143`, `server.rs:2420-2436`). `session/close` cancels whatever run is
  active, even another connection's (`server.rs:2639`): never use it to evict.
- **Updates for a turn go only to the prompting connection** (`on_prompt`,
  `forward_agent_stream`, `server.rs:2126-2357`; P3c). No broadcast, no subscribe;
  cross-connection `session/load` does not divert live frames.
- **Losing the WebSocket cancels the turn (T5).** WS close → `conn.shutdown()` aborts the
  handler task → `ActiveRunDropGuard` cancels the token (`server.rs:258-280`). This is
  deliberate upstream ("roaming"), and was already true in 1.48; pigeon's comment that
  turns survive disconnect is stale (and pigeon's reconnect path therefore cancels its
  own turns today).
- **Over HTTP+SSE the logical connection outlives the socket:** Turns survive clean
  disconnects and extended 600s gaps (P2a-c). POSTs return 202, outbound frames queue in
  per-connection/per-session mailboxes keyed by `Acp-Connection-Id`/`Acp-Session-Id`, a
  new GET drains the queue, and connections live until DELETE. On clean disconnect (FIN
  sent), the 409 conflict window is ~0-1ms; on ungraceful half-open drops, 409 persists
  indefinitely until TCP teardown (P2d). DELETE terminates the connection and cancels
  active runs (T5, P2d), so it cannot be used to evict a stale subscriber. Delivery is
  at-most-once: frames written to a dead/frozen socket buffer (chunks, prompt reply,
  run-end update) are permanently lost (P2d), requiring state reconciliation after every
  reattach.
- **Each connection gets its own in-memory agent** (`AgentManager` per
  `GooseAcpAgent`, `server.rs:973`), with its own MCP extension processes. Two
  connections on one session = two agents; extension state is last-writer-wins.
- **`session/load` is a mutating activation, not a read.** It rewrites the stored cwd
  if the passed cwd differs (`server.rs:1176-1178`; honkd must pass the stored cwd from
  `session/list`, P3b) and spawns every MCP extension (`load_session.rs:414`).
  Same-connection reload mid-run is safe: the running agent is preserved, no extra
  processes/threads spawn, and in-flight tool subprocesses continue (P3a). Replay delivers
  COMMITTED messages of the in-flight turn, never the uncommitted in-flight message (P3a).
  Replay is bounded by `_meta.replayTail` (cuts at turn boundaries, e.g. last turn for N=1,
  reports `replaySkipped`, `:88-105`, `:443-448`, P6a). Read-only alternative:
  `_goose/unstable/session/export` (`manage_sessions.rs:105-118`) reads SQLite directly with
  zero side effects, working mid-run, on fresh connections, before load, and immediately
  after a goose restart (P3, P5e).
- **Pending state.** Tool permissions are **NOT persisted mid-turn, NOT re-sent by
  `session/load`, and NOT redelivered on SSE resume** (P4). Goose's in-memory
  `ToolConfirmationRouter` awaits confirmation indefinitely on the same connection, and
  answering the original request ID after a long disconnect (tested 90s) executes the tool
  and finishes the turn cleanly (P4a). However, a permission frame lost in transit leaves
  the run parked forever (P4b); answers on a different connection return HTTP 202 but
  execute nothing (P4c). Elicitation forms time out after 300 s server-side
  (`mcp_client.rs:424`) and are auto-cancelled unless the client advertises elicitation
  (`elicitation.rs:26-44`). Steers are consumed early and late during active runs (P5d);
  unconsumed steers are discarded on run completion or cancellation (`server.rs:1929-1935`).
- **Cancellation semantics.** `session/cancel` works from any connection in 54–79ms (P5a),
  but does NOT unblock a run parked on a pending permission request; responding to the
  permission with `outcome: "cancelled"` terminates it in ~2ms (P5c). Cancel SIGKILLs only
  the immediate shell process (`bash`); descendant processes are orphaned to init/systemd
  and continue running (P5a; goose bug, patch candidate #5).
- **Client-library hazard.** In `agent-client-protocol` 3.x, dropping a `SentRequest`
  sends `$/cancel_request`. Goose ignores `$/cancel_request` completely (P5b), so the drop
  hazard is benign today, but keep the pin to goose's own versions (`agent-client-protocol`
  2.2.0, schema `=1.9.1`) and hold prompt handles explicitly.
- **Resource footprint.** An active goose session consumes ~0.5–1.7 MB VmRSS and ~9–12 OS
  threads (P6b). The builtin `developer` extension runs in-process with 0 child processes.
  All threads are reaped when the connection is DELETED.
- **Model switch is per session** (`recreate_provider_for_session`). **Extension
  toggles**: use the session-scoped add/remove methods (`extensions.rs:8-36`), never
  the global `set_config_extension_enabled`; there is no "disable", re-enable means
  re-adding the full `ExtensionConfig`.
- **Subagents** are hidden from `session/list` (`server.rs:144`); their live progress
  arrives as `tool_call_update` with `_meta.toolNotification`.
- If a client advertises `terminal`, goose routes *all* shell execution to it.

Consequences:
1. A long-lived process must own each session's goose connection and fan out its
   updates: `honkd`.
2. **honkd's lifetime must not be the turns' lifetime.** Hence HTTP+SSE upstream with
   persisted connection ids, so a honkd restart re-attaches instead of cancelling.
3. honkd must be the **only** goose client for interactive sessions (pigeon included),
   or sessions get two divergent in-memory agents.

## 4. Architecture

```
 honk (TUI) ×N   nvim switcher   pigeon   honk new/list/tail
        \             |            |           /
         └───── ACP (+ _meta.honk / _honk/*) over WS, 127.0.0.1 or unix ─────┘
                               │
                        honkd (ACP proxy)
          per session: seq log of session/update, pending requests,
          prompt queue, upstream prompt handle
          state file: {connection_id, held sessions, queue, req-id ledger}
                               │  ACP over HTTP+SSE (X-Secret-Key)
                         goose serve (pinned version)
```

**honkd is a mostly transparent ACP proxy**, not a bespoke protocol. It routes raw
JSON-RPC and decodes only `method`, `id`, `sessionId` and the handful of messages it
owns. Downstream clients speak ACP; honkd adds three behaviors:

1. **Attach.** A downstream `session/load` for a session honkd holds is served from
   honkd's per-session log of `session/update`s (replay, then live), never by
   re-loading upstream. `_meta.honk.after = {epoch, seq}` resumes from a cursor.
   Mirrors ACP's draft `session/resume` + `replayFrom`.
2. **Pending requests.** `session/request_permission` and elicitation go to every
   attached viewer that advertised the capability; first answer goes upstream, the
   rest get `_honk/request_resolved`. Held while no viewer is attached (forms still
   expire at goose's 300 s; honkd surfaces the expiry).
3. **Prompt ownership.** honkd holds the upstream `session/prompt`; other viewers get
   a synthesized `user_message_chunk` tagged `_meta.honk.source`. Queue vs steer via
   `_meta.honk.mode`. honkd sends its own fixed client capabilities upstream
   (elicitation yes; terminal/fs no).

Why a proxy rather than `honk-proto`: pigeon (an ACP-over-WS client) needs almost no
change; cold replay and live stream are the same event shape so no separate snapshot
type; goose-specific messages pass through untouched; and if goose ever adds
subscriptions, the TUI points at goose and honkd is deleted.

Cargo workspace:

| Crate | Role |
|---|---|
| `honk-acp` | Pinned ACP types (`agent-client-protocol` 2.2.0 / schema `=1.9.1`, matching goose) plus helpers for the `_goose/unstable/*` and `_honk/*` methods used. |
| `honk-transcript` | Reducer: ACP `session/update` stream → semantic transcript blocks with stable ids. Shared by honkd (snapshots after ring-buffer eviction) and the TUI. |
| `honkd` | The proxy daemon (systemd user service, **excluded from the nightly reset**). |
| `honk` | ratatui TUI (`honk attach <session>`, alternate screen) and CLI: `list`, `new --dir --prompt`, `tail`, `scroll-to`, `status`. |

## 5. Data flow and semantics

**Upstream connection.** One HTTP+SSE logical connection per held session (fault
isolation; pooling is a later optimization). Live frames for a run are strictly pinned to
the connection that sent `session/prompt` (P3c; A4); honkd must prompt on and reattach to
the same persisted connection id per session. `{connection_id, session_id}` is persisted to
honkd's state file BEFORE use (A15).

On reattach, clean drops clear 409 within 0–1ms, while half-open socket freezes cause 409
indefinitely until TCP teardown (P2d; A2). honkd retries 409 with exponential backoff and
never sends DELETE to evict a stale subscriber (DELETE cancels active runs; A2). To avoid
loopback half-open deadlocks from an alive-but-stuck consumer, honkd decouples SSE socket
reading from downstream fan-out via large/unbounded internal buffering and slow-viewer
eviction (A2).

*Orphaned-connection rule (A15)*: On startup, honkd DELETEs connection ids recorded in its
state file that it is not resuming. Because goose provides no connection-listing endpoint,
a lost state file leaks connections until goose restarts (accepted and documented).

**Session limits & eviction.** Evicting a session = DELETE its connection (never
`session/close`, which cancels runs). Resource footprint per session is ~0.5–1.7 MB VmRSS and
~9–12 threads, with 0 children for builtin extensions (reaped on DELETE; P6b; A14). Initial
honkd limits: idle TTL 15 minutes (900s), max 16 held sessions (LRU evict idle sessions only;
never evict while a run or request is pending). Limits to be re-measured when external MCP
servers are configured.

**Cold attach.** For a session honkd does not hold, a viewer gets a read-only snapshot
from `_goose/unstable/session/export` (reads SQLite directly, works mid-run, on fresh
connections, before load, and after restart; P3, P5e; A6). honkd only activates the session
upstream (`session/load` with the **stored** cwd from `session/list`, `mcpServers: []`, bounded
`replayTail`) on the first prompt, answer, or config change (A5).

**Sequencing and IDs.** Every logged event carries `(epoch, seq)`; `epoch` changes on each
honkd start and on every upstream reconcile. A cursor from another epoch gets `_honk/reset` +
full replay, never a merge. Text/thought `messageId`s and all `toolCallId`s are stable and match
in identical order across live stream, replay, and export (P6a; A7); the `messageId` carried on a
tool call is NOT stable between live and replay, so transcript block id = `messageId` for
text/thought blocks and `toolCallId` for tool blocks (never `messageId:toolCallId`). Replay delivers accumulated blocks and includes
`user_message_chunk`, but omits `session_info_update` and intermediate `tool_call_update` chunks
(P6a; A7). `_meta.replayTail: N` cuts at turn boundaries (last turn for N=1).

**Reconcile.** Frames are delivered at-most-once; unread frames in kernel buffers on disconnect
are lost, meaning prompt replies and run-end notifications can be missed (P2d; A3). Therefore,
honkd must reconcile session state after EVERY reattach (A3). Turn end is detected from
`session_info_update` `activeRunId=null` OR the prompt reply OR export polling, never the
reply alone (A3). Reconcile = rebuild honkd's log keyed by `messageId` (replace committed
messages from `session/load` or export, keep streaming the in-flight uncommitted message, and
reconcile again at turn end), still emitting `_honk/reset` + replay to viewers (A5).

**Prompts.** `client_req_id` (in `_meta.honk`) dedupes retries; the ledger is in the
state file. Semantics:
- `queue` (TUI default): held until the active run ends; **dropped** if the user
  cancels the run.
- `steer` (pigeon's default): `_goose/unstable/session/steer`. Early and late steers are
  consumed during active runs (P5d; A12). Because loop teardown can discard pending steers,
  honkd confirms consumption (checks for user message with the steer text in stream or
  export); if unconfirmed or dropped, honkd re-queues the message as a regular prompt (A12).
- A steer may overtake earlier queued prompts (intended: it targets the current run).
- The `session/prompt` response (with `stopReason`) goes to the originating client
  when the turn ends, as pigeon expects; for queued prompts it is late by design.

**Pending requests.** Tool permissions are NOT persisted mid-turn, NOT re-sent by `session/load`,
and NOT redelivered on SSE resume; goose's in-memory `ToolConfirmationRouter` waits indefinitely
on the same connection, and answering the original id after reconnect works (P4a; A8). Answers
on another connection return HTTP 202 and do nothing (P4c). Therefore, honkd must persist received
pending requests (`id` + payload) in its state file BEFORE fanning out to viewers so it can answer
across restarts on the same connection (A8).

If a permission frame is lost in transit before honkd reads it, the run parks forever; on a
detected lost request (run active, tool call pending, no request frame seen after reattach),
honkd surfaces "request lost" and offers cancel = DELETE the connection (which cancels even a
parked run, unlike `session/cancel`) then re-activates (A8). Elicitation forms time out after
300s server-side; honkd surfaces the expiry.

**Cancel.** Forwarded as the `session/cancel` notification (works from any connection, 54–79ms,
P5a; A9). However, `session/cancel` alone does not unblock a run parked on a pending permission
(P5c); answering `{"outcome": {"outcome": "cancelled"}}` terminates the parked run in ~2ms (A9).
Therefore, Cancel = `session/cancel` notification + explicitly answering all outstanding
permission requests with outcome `cancelled` (confirming design §5; A9). Note that cancel
currently leaves descendant processes of shell commands running due to a goose bug (P5a; A11),
which candidate patch #5 resolves.

**Viewers.** Each has a bounded outbound queue; overflow disconnects it (it
reattaches with its cursor). The hello carries a protocol version so long-lived TUIs
survive honkd upgrades or are told to restart.

**TUI model.** `honk-transcript` blocks with stable ids; layout cached per (block,
width); only visible blocks rendered; scroll anchored to `(message_id, line_offset)`,
follow-tail unless scrolled away. Streaming markdown commits on newlines and holds back
incomplete table rows (after codex-rs `markdown_stream.rs`). Components: `edtui`,
`tui-markdown` + `syntect`/`two-face`, `similar`, `nucleo`, crossterm bracketed
paste, OSC 52.

## 6. Failure model

- **honkd restart / crash**: turns keep running in goose (HTTP+SSE connections
  survive clean drops and 600s gaps; P2a-c; A1). The WS fallback is deleted. On startup,
  honkd reads its state file, re-attaches to existing connection ids via GET, and drains
  queued frames. If 409 is returned, honkd retries with exponential backoff (A2).
  Once re-attached, honkd reconciles session state (A3, A5), bumps epoch, and emits
  `_honk/reset` + full replay to viewers. Any pending permissions persisted in the state
  file before restart can still be answered on the re-attached connection (A8). Orphaned
  connection ids in the state file not being resumed are explicitly DELETED (A15).
- **goose serve restart**: all runs and connections die (in-flight shell processes die
  with the cgroup; P5e; A13). Old connection ids return HTTP 404 on POST and GET. Goose
  accepts fresh `initialize` calls within ~200ms. On 404, honkd re-initializes, reloads
  sessions via `session/load` passing stored cwd from `session/list`, marks interrupted
  turns as aborted/unanswered, and offers re-prompt to viewers (A13). Export can be
  queried immediately on a fresh connection before `session/load` to inspect committed
  history (A6). honkd reconnects with backoff and staggers re-activation so N sessions
  don't spawn N×extensions at once.
- **Turns started outside honkd** (goose CLI, scheduler): invisible until the next
  reconcile; honkd's prompt gets "busy" and falls back to queue-and-poll. Accepted.
- **Unknown ACP messages**: passed through; the reducer renders unknown updates as a
  dim raw block. Never panic.
- **Accepted losses on honkd restart**: none of queue / req-id ledger / held sessions /
  pending requests (state file); attached-viewer set is not persisted (TUIs reconnect
  themselves; nightly restore is the switcher's job).
- **Security**: downstream listener on a 0700 directory under `$XDG_RUNTIME_DIR` (unix
  socket) or 127.0.0.1 with a token; peer uid checked. This is not a boundary against
  the agent itself (same uid); with allow-all permissions that is accepted.

### Reattach rules: patched and unpatched goose

Added after the goose patch track (fork release `patched-1.54.0.3`; results in
`docs/spike/goose-patches.md`, "Results: patched-1.54.0.3"). This supersedes the
409-backoff and reconcile-after-every-reattach rules above when goose is patched.

**The signal.** Patched goose sends `Acp-Stream-Features: takeover, replay` on every
SSE `GET /acp` response, `200` and `409`. Unpatched goose sends nothing.
- The signal comes from the crate, which is where the behaviour lives.
- It costs no extra request.
- It does not depend on version strings. `agentInfo.version` is `1.54.0` in both
  builds; log it for diagnostics only.
- `honk-upstream` exposes it as `Upstream::stream_features()` and
  `StreamFeatures::is_patched()`.

**M1 policy: honkd requires patched goose.**
- After its first subscribe on each goose process, honkd checks
  `stream_features().is_patched()`. If that is false, it **refuses to serve**, with an
  error naming the release to deploy.
- The escape hatch is **`--allow-unpatched-upstream`**. It makes honkd run in
  degraded mode instead: 409 backoff with no DELETE (A2), and every reattach treated
  as a gap, i.e. a coarse rebuild from export (A3). There is no precise in-flight
  recovery and no lost-permission heuristic (M1 plan, Task A12; refinement in M1c).
- honkd re-checks after every `404`, i.e. after a goose restart (A13). A restart may
  have swapped the binary in either direction.

**Sending the headers is always safe. Trusting the result is not.**
- Unpatched goose ignores `Acp-Takeover` (it answers 409) and `Last-Event-ID` (no
  replay).
- Gate behaviour **only** on positive signals: `took_over`, and
  `ReplayOutcome::Complete`. Never treat the absence of a gap signal as "no loss".

| goose advertises | On a dead stream (no bytes for > 20 s; goose keeps alive every 15 s) | After reattach |
|---|---|---|
| nothing (unpatched): only with `--allow-unpatched-upstream` | 409 retry with backoff, never DELETE (A2) | Always rebuild from export (gap) |
| `takeover` only (not a released combination; treat as degraded) | `subscribe_with{takeover}` | Always rebuild from export |
| `takeover, replay` (`patched-1.54.0.3`+) | `subscribe_with{takeover, resume_after: Some(last processed id, or 0)}` | Rebuild from export **only** on the session stream's `Gap`/`Unsupported` (at turn end if a run is active), or on any new connection (404, activation) |

**Where honkd persists what (matches the M1 plan).**
- **The last processed session-stream event id lives in the session's append-only log**
  (`<state>/sessions/<sid>.jsonl`). Each line is appended *after* its frame was
  processed. Frames that aren't logged (responses, requests) get an id-only line
  `{"seq":null,"id":N}`. On restart, the last id in the log is exactly what to pass as
  `resume_after`.
- **Effects that must survive a crash live in `state.json`**: pending permissions (A8)
  and the prompt ledger, each written *before* fan-out or POST. The log line comes
  after, so a crash in between re-delivers the frame on resume. Re-processing is
  therefore **idempotent**: pending requests dedupe by id, prompt replies by upstream
  id, and the actor drops any frame with an id ≤ the stream's last processed id.
- **The connection stream** is low-rate (`session/new` replies). Its last id lives
  next to the connection id in `state.json` (A15).

**Still required with replay:**
- Persisted pending permissions (A8). Replay only helps while the connection lives; a
  goose restart loses everything in memory.
- Takeover is for honkd's own dead streams only. honkd owns its connection ids, and two
  honkd instances must never share one: the last subscriber silently wins.

## 7. Testing

- **Wire fixtures** recorded from the pinned goose (redacted) drive `honk-acp` and
  `honk-transcript` tests.
- **Proxy tests** against a fake upstream: zero-viewer survival, two viewers
  converge, pending permission survives detach, epoch reset on restart, no duplicated
  text after reconcile, idempotent prompt retry, queue dropped on cancel, steer→prompt
  fallback, slow-viewer eviction.
- **TUI**: ratatui `TestBackend` snapshot tests per block type; `edtui` key-sequence
  tests for the vim subset.
- **Live smoke** (gated) against the pinned `goose serve`; required before any goose
  version bump.
- CI: GitHub Actions (fmt, clippy `-D warnings`, test) on push/PR; release binaries on
  tags.

## 8. Milestones

| | Scope | Exit criterion |
|---|---|---|
| **M0 spike (1-2 days)** | **DONE (2026-10-11)**. Probes T5, P2–P6 complete, P7 skipped, P1 pending author day. Findings written (`m0-findings.md`), patch candidates ranked (`goose-patches.md`), design amended (§10). | Findings written; §3 confirmed or design amended. (P1 pending author run; T5/P2–P6 confirmed HTTP+SSE and amended design). |
| **M1** | honkd proxy (HTTP+SSE upstream, state file, attach/replay, pending requests, queue/steer); pigeon repointed at honkd; ~100-line `honk tail`. | A pigeon turn is visible in `honk tail`; `kill -9 honkd` mid-turn, restart: the turn completes and `tail` resumes without duplicated text; a permission pending across a honkd restart is still answerable; restart goose mid-turn: `cancelled`/interrupted shown, retry not duplicated. |
| **M2** | TUI daily-driver: edtui composer, markdown/tables/colors, thinking, tool blocks, inline subagent progress, scrolling, history, interrupt. Used alongside OpenCode v1. | Author uses it for a full day. |
| **M3** | Forms/permissions, model switch, MCP toggle, usage line, diffs. | All MVP interaction features present. |
| **M4** | Ecosystem: nvim switcher, `honk new`, nightly restore, terminal title, scroll-to. | OpenCode TUI no longer needed day-to-day. |

**Parallel-run policy.** OpenCode v1 (on upstream's `v1` branch) stays primary until
M2 exits. It is retired when M4 exits, or earlier if v1 sees no upstream commits for
6 weeks or a needed model/provider stops working on it.

**Out of scope here but on the migration path**: goose equivalents for the author's
session-search, spend-attribution and stall-watch tooling and OpenCode plugins
(shell env injection, subagent-failure handling). These may cost more than honk and
are tracked separately.

Effort signal (research §2; estimates, not measurements): 6-10k LOC TUI + ~1.5-3k LOC
proxy; ~100-180 h with substantial reuse, 200-300+ h owning everything.

## 9. Alternatives considered

- **hydra-acp as the relay** (TypeScript, already does attach/detach, fan-out,
  permission arbitration): fastest if it works with an existing authenticated
  `goose serve`; rejected as the primary path for the two-language dependency on an
  experimental project, kept as M0 prior art.
- **Pigeon becomes the relay**: reuses its goose ACP client and sees swarm turns for
  free, but makes a messaging daemon load-bearing for interactive use. (The chosen
  design has the mirror-image cost: honkd becomes load-bearing for swarm delivery.
  Accepted, because honkd is small and single-purpose and pigeon is not.)
- **Patch goose** to decouple turns from connections and broadcast updates: permanent
  fork burden; still needs subscription semantics. Revised by the author: patches *are*
  carried in the existing goose fork alongside honkd for practice, then considered for
  upstream.
- **One TUI per session kept alive by tmux, no relay**: cheapest, but fails the
  thin-client and live-pigeon-turn requirements.
- **Inline (native scrollback) rendering**: preferred for vim-native yank, but makes
  reattach redraw, scroll-to-message and live-updating tool blocks hard; replaced by a
  post-MVP "dump transcript to nvim" key.

## 10. Review record (2026-10-10)

The first draft was reviewed by an adversarial reviewer and a strategic oracle, both
reading goose v1.54.0 source. Changes they drove:

- honkd's lifetime was implicitly every turn's lifetime (WS drop cancels runs). Now
  HTTP+SSE upstream with persisted connection ids, plus a WS fallback with explicit
  drain semantics; honkd is excluded from the nightly reset.
- A bespoke `honk-proto` local protocol was replaced by a mostly transparent ACP proxy
  with `_meta.honk` / `_honk/*` additions (pigeon nearly unchanged; aligns with ACP
  `session/resume`).
- `session/load` treated as a read was wrong: it rewrites cwd, spawns extensions and
  can start runs. Cold viewing now uses `session/export`; activation is lazy and passes
  the stored cwd.
- Seq numbers now carry an epoch; reconcile emits reset + replay instead of merging
  (replay granularity differs from live deltas).
- A small state file replaces "no second DB" (queue, req-id ledger, held connections).
- Pigeon moves from M4 to M1, since honkd must be the only interactive goose client and
  live pigeon turns are the core requirement.
- M0 re-aimed at the real unknowns, adds a goose-quality kill gate and an upstream
  proposal; disconnect-cancels-turn over WS is no longer an open question.
- Pins `agent-client-protocol` 2.2.0 / schema 1.9.1 to match goose (3.x drops of
  `SentRequest` emit `$/cancel_request`); auth is `X-Secret-Key`, not bearer.
- Inline subagent progress added to the MVP (cheap via `toolNotification`).

### M0 revisions (2026-10-11)

Empirical probes on `goose` v1.54.0 (`docs/spike/m0-findings.md`) drove design amendments A1–A16:
- **Transport confirmed, fallback deleted (A1):** HTTP+SSE turns survive clean disconnects and 600s gaps (P2a-c). The WS fallback in §6 is eliminated.
- **Reattach and reconciliation (A2, A3, A5):** Clean disconnects clear 409 in 0–1ms; half-open drops cause persistent 409 until TCP teardown, and unread frames in kernel buffers are lost (P2d). honkd retries 409 with backoff, never DELETEs to evict, decouples socket reads from viewer fan-out, and reconciles session state after every reattach via messageId replacement.
- **Connection binding (A4):** Live stream frames are pinned to the prompting connection (P3c). honkd must prompt on and reattach to the same persisted connection ID.
- **Permissions (A8):** Pending permissions are NOT persisted mid-turn and NOT re-sent by `session/load`, but wait indefinitely in memory on the same connection (P4a). honkd persists pending requests in its state file before viewer fan-out to answer across restarts; lost requests require cancel-via-DELETE.
- **Cancellation (A9, A11):** `session/cancel` works across connections (54–79ms) but does not unblock permission-parked runs without an explicit `cancelled` outcome answer (P5a, P5c). Cancel performs both. Orphan descendant leak identified as goose bug (patch candidate #5).
- **IDs & Limits (A7, A14, A15):** text `messageId`s and `toolCallId`s are stable across live, replay, export (P6), so `block_id = messageId` for text and `toolCallId` for tools (a tool call's own `messageId` differs between live and replay). Resource cost (~0.5–1.7 MB RSS, ~9–12 threads/conn) sets initial limits at 15m idle TTL and max 16 held sessions. Orphaned connections managed via state file DELETE on startup.
