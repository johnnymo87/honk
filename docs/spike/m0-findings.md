# honk M0 findings

Status: in progress. Each probe section records the question, verdict, decision-rule
outcome, and fixture evidence.

## P1: Is goose's agent loop good enough? (human day)

Rubric drafted by the agent on the author's delegation (2026-10-10); **author to review
before running the day**. The comparison baseline is a typical OpenCode v1 day
(~48 root sessions, Opus-class model, allow-all permissions, frequent subagents,
self-compaction via a tool).

Run: one working day; route every root session that would have gone to OpenCode
through goose instead (`goose session` / pigeon launch), same model family. Keep a
one-line note per session in the table below.

| # | Criterion | How measured | Pass | Fail |
|---|---|---|---|---|
| C1 | Task completion | Sessions where goose finished the task without the author taking over in OpenCode | ≥ 80% of sessions | < 65% |
| C2 | Edit/shell/read tool reliability | Count of tool failures the agent could not self-recover (bad patch, wrong file, hung shell) | ≤ 1 per 10 sessions | ≥ 3 per 10 |
| C3 | Long-session survival | Any session past one compaction stays on-task (no lost goal, no repeated work) | All observed | Any session derails after compaction |
| C4 | Subagent delegation | Reviewer / explorer style subagents run and return usable results | Works for the 3 most-used roles (adversarial reviewer, explore, implementer) | Any of those three unusable |
| C5 | Cost per session | Day's spend ÷ sessions, vs. the OpenCode daily average | ≤ 1.3× | > 2× |
| C6 | Per-session MCP enablement | Enable Slack/Atlassian for one session without affecting others | Works | Not possible without global config edits |
| C7 | Skills & instructions | AGENTS.md / skills are discovered and followed | Followed in normal use | Ignored or need per-session pasting |

**Kill condition:** two or more criteria in the Fail column, or any single
criterion the author judges a blocker. Middle-band results are recorded as
follow-ups (patch, config, or recipe), not as failures.

| Session | Task | Outcome | Notes (criteria hit) |
|---|---|---|---|
| | | | |

**Verdict:** _pending_

## T5 WS check

**Question:** Does closing goose's WebSocket transport cancel in-flight agent turns?

**Verdict:** Yes. Closing the WebSocket transport immediately aborts active runs on the server.

**Methodology:**
- Connected to `ws://127.0.0.1:4180/acp` via `websocat` with header `X-Secret-Key`.
- Handshake sequence: `initialize` (id: 1), `session/new` (id: 2, session `20261010_6`), `session/set_mode` auto (id: 3), `session/set_config_option` model `claude-haiku-4-5@20251001` (id: 4).
- Sent standard long-turn prompt: `Run the shell command "sleep 40 && echo done", then reply with only the word FINISHED.` (id: 5).
- Upon receiving the `tool_call` session update frame (`toolu_vrtx_01Nr5jr49122yHBVVqV9sRJx`, running `sleep 40 && echo done`), immediately terminated the WebSocket client process.
- Waited 60 seconds for any background process or turn to complete.
- Over HTTP on a new connection (`2956d77e-9ede-40c6-90eb-15b30e24e952`), subscribed to the session stream and requested session export via `_goose/unstable/session/export`.
  - Finding on export requirements: `session/load` was **not** needed on the new connection; `_goose/unstable/session/export` directly queries goose's disk session store.
- Inspected exported session:
  - Total messages: 1 (only the user prompt).
  - No assistant message was produced at all; the word "FINISHED" appeared solely in the user prompt instructions.

**Decision-rule outcome:**
- Confirmed design premise: WebSocket transport cancels runs on client disconnect.
- Pigeon's runner assumption that turns survive disconnect does not hold over WebSocket.
- Honk's HTTP+SSE architecture is confirmed as the necessary foundation for reconnectable sessions.

## P2 HTTP disconnect

**Question:** Under HTTP+SSE transport, do active turns survive client disconnects? What is the 409 conflict window during reattach? Are frames lost when disconnected during streaming? Does an in-flight turn survive an extended 600s disconnect gap? How does a half-open / blackholed connection behave?

**Verdict:** Yes, in-flight turns survive client disconnects under HTTP+SSE across immediate reattachment, extended gaps (600s), and half-open freezes. However, half-open drops cause persistent 409 Conflict until the stale socket closes, and unread frames (chunks, run-end updates, and prompt replies) written into kernel buffers during the freeze are permanently lost.

**Caveat on P2a/b/c:** P2a, P2b, and P2c operated over local loopback with clean socket teardown (`hard_drop` aborts the client task, causing the OS to immediately send TCP FIN). This represents the easy case where the server learns of the client teardown immediately (0ms 409 window). P2d empirically probes the hard case of an ungraceful, half-open drop.

**Decision-rule outcomes:**
1. **Turn survival (P2a, P2c, P2d):** Turn survives across clean drops, long 600s gaps, and half-open socket freezes. Design §6 holds; the WS+drain fallback is not required.
2. **409 reattach window & half-open persistence:**
   - Per source analysis (`http_server.rs`) and P2d empirical measurements, Goose strictly returns HTTP 409 Conflict when a stream is already actively held by another subscriber.
   - When a connection drops cleanly (FIN sent), the 409 window is ~0-1ms.
   - When a connection is half-open / blackholed (sockets held open without reading or writing), the 409 Conflict persists indefinitely (never cleared within the 180s deadline across both runs).
   - **Can honkd force Goose to release the stale subscriber by sending DELETE? NO.** Sending DELETE terminates the connection and aborts in-flight runs (as shown in T5). Honkd cannot force Goose to release the stale subscriber from the outside; Goose holds the mailbox subscriber until kernel TCP teardown or write errors occur.
   - Therefore, honkd must retry with exponential backoff on 409. Reattach latency in half-open scenarios is gated on Goose's socket detection.
   - *Loopback note:* On loopback (honkd↔goose on the same host) a crashed/killed honkd closes its sockets (FIN → ~0-1ms window); the half-open case arises when honkd is alive-but-stuck (stalled runtime/deadlock) holding the socket unread — so honkd should keep its SSE read loops independent of slow consumers (never let backpressure stop reading the socket).
3. **Frame loss & reconciliation (P2b vs P2d):**
   - On clean drop with immediate reattach (P2b), no characters were lost (`chars_missing: 0`).
   - On half-open drop (P2d), Goose continues generating and writing frames into the open socket buffer. Because Goose dequeues frames before writing (at-most-once delivery), all chunks streamed during the freeze, the run-end notification, and the prompt reply are permanently lost into the stale socket buffer (`prompt_reply_received: false`, `run_end_received: false`, 213–649 characters missing).
   - **Implication:** Honkd **must reconcile session state after every reattach** (via session export/reload in P3) rather than relying on stream replay or waiting for prompt replies.

**Probe results:**

### P2a: Quiet drop (tool_call in-flight, clean FIN)
- **Run 1:**
  - Session: `20261010_8` | Connection: `05fcd963-f3b3-498d-a784-33f367a1e5c3`
  - Conn 409 window: 0ms | Session 409 window: 0ms
  - Prompt reply received: `true` | Run-end received: `true` | `FINISHED` present: `true`
  - Total duration: 41.46s
  - Fixture: `fixtures/goose-1.54.0-patched.2/p2a/1791675140.jsonl`
- **Run 2:**
  - Session: `20261010_9` | Connection: `eb772c62-0783-4b37-b707-44f32795dd8c`
  - Conn 409 window: 0ms | Session 409 window: 0ms
  - Prompt reply received: `true` | Run-end received: `true` | `FINISHED` present: `true`
  - Total duration: 42.26s
  - Fixture: `fixtures/goose-1.54.0-patched.2/p2a/1791675187.jsonl`

### P2b: Drop while streaming (after 20 chunks, clean FIN)
- **Run 1:**
  - Session: `20261010_10` | Connection: `7bd9f05c-6e5b-4c79-8d54-331f0c654523`
  - Conn 409 window: 0ms | Session 409 window: 0ms
  - Chunks before drop: 20 | Chunks after drop: 56
  - Streamed chars total: 869 | Exported all assistant chars: 869 | Chars missing: 0 | Chars abs diff: 0
  - Streamed equals exported all: `true` | Streamed equals exported final: `false` (run had tool calls across multiple assistant turns)
  - Exported final assistant chars: 637
  - Prompt reply received: `true` | Run-end received: `true` | `FINISHED` present: `true`
  - Total duration: 18.76s
  - Fixture: `fixtures/goose-1.54.0-patched.2/p2b/1791675233.jsonl`
- **Run 2:**
  - Session: `20261010_11` | Connection: `175cf047-09cc-40ec-a45f-726d099004c8`
  - Conn 409 window: 0ms | Session 409 window: 0ms
  - Chunks before drop: 20 | Chunks after drop: 105
  - Streamed chars total: 930 | Exported all assistant chars: 930 | Chars missing: 0 | Chars abs diff: 0
  - Streamed equals exported all: `true` | Streamed equals exported final: `false`
  - Exported final assistant chars: 181
  - Prompt reply received: `true` | Run-end received: `true` | `FINISHED` present: `true`
  - Total duration: 79.32s
  - Fixture: `fixtures/goose-1.54.0-patched.2/p2b/1791675319.jsonl`
- **Run 3:**
  - Session: `20261011_2` | Connection: `787c8cad-042f-46c5-aa9d-d833a235e7cc`
  - Conn 409 window: 0ms | Session 409 window: 0ms
  - Chunks before drop: 20 | Chunks after drop: 43
  - Streamed chars total: 719 | Exported all assistant chars: 719 | Chars missing: 0 | Chars abs diff: 0
  - Streamed equals exported all: `true` | Streamed equals exported final: `false`
  - Exported final assistant chars: 575
  - Prompt reply received: `true` | Run-end received: `true` | `FINISHED` present: `true`
  - Total duration: 15.66s
  - Fixture: `fixtures/goose-1.54.0-patched.2/p2b/1791677265.jsonl`

### P2c: Long gap (600s disconnect, clean FIN)
- Session: `20261010_7` | Connection: `b95059f6-2660-4afe-9122-7992a898b208`
- Disconnect gap: 600s (10 minutes)
- Conn 409 window: 0ms | Session 409 window: 0ms
- Prompt reply received: `true` | Run-end received: `true` | `FINISHED` present: `true`
- Total duration: 602.07s
- Fixture: `fixtures/goose-1.54.0-patched.2/p2c/1791675132.jsonl`

### P2d: Half-open / blackholed drop (TCP proxy freeze)
- **Methodology:**
  - A local TCP forwarding proxy ran on an ephemeral port in front of `127.0.0.1:4180`.
  - Upstream connected through the proxy, started a session, and issued the streaming tides prompt.
  - After 20 chunks, the proxy froze: stopped reading and writing on both sockets while keeping the TCP connections open in kernel state (no FIN/RST sent to Goose), and client tasks were hard-dropped.
  - Client immediately attempted to reattach directly to `127.0.0.1:4180` and subscribe with a 180s deadline.
  - When 409 did not clear within 180s, the proxy dropped the sockets (sending FIN). Time to subscribe after FIN was measured.
  - Resumed stream read for any arriving frames, exported session, and computed frame loss across streamed vs exported text.
- **Run 1:**
  - Session: `20261010_12` | Connection: `14abe2ad-9b78-4b89-9676-7c024f0df279`
  - Proxy port: 45933 | Half-open 409 probe: 181.255s (timed out; cleared while frozen: `false`)
  - Conn 409 ms: 181,255ms | Session 409 ms: 181,255ms
  - Time to subscribe after FIN: 1ms (`subscribe_ms_after_fin: 1`)
  - Chunks before drop: 20 (231 chars) | Chunks after reattach: 0
  - Streamed chars total: 231 | Exported all assistant chars: 880 | Exported final assistant chars: 728
  - Chars missing vs all: 649 | Chars missing vs final: 497 | Chars abs diff: 649
  - Streamed is prefix: `true` | Streamed is subsequence: `true`
  - Prompt reply received: `false` (lost to dead socket buffer)
  - Run-end received: `false` (lost to dead socket buffer)
  - `FINISHED` present in export: `true` (turn ran to completion on server)
  - Total duration: 255.45s
  - Fixture: `fixtures/goose-1.54.0-patched.2/p2d/1791676694.jsonl`
- **Run 2:**
  - Session: `20261011_1` | Connection: `53ce032d-a60c-4f34-b136-95e560f54a92`
  - Proxy port: 41099 | Half-open 409 probe: 181.268s (timed out; cleared while frozen: `false`)
  - Conn 409 ms: 181,268ms | Session 409 ms: 181,268ms
  - Time to subscribe after FIN: 1ms (`subscribe_ms_after_fin: 1`)
  - Chunks before drop: 20 (273 chars) | Chunks after reattach: 0
  - Streamed chars total: 273 | Exported all assistant chars: 486 | Exported final assistant chars: 415
  - Chars missing vs all: 213 | Chars missing vs final: 142 | Chars abs diff: 213
  - Streamed is prefix: `true` | Streamed is subsequence: `true`
  - Prompt reply received: `false` (lost to dead socket buffer)
  - Run-end received: `false` (lost to dead socket buffer)
  - `FINISHED` present in export: `true` (turn ran to completion on server)
  - Total duration: 251.04s
  - Fixture: `fixtures/goose-1.54.0-patched.2/p2d/1791676988.jsonl`

## P3: Same-connection reload mid-run

**Question:** Is `session/load` on the same (resumed) connection safe to reconcile session state while an agent run is still active? What are the replay semantics, process impact, CWD side effects, and cross-connection behavior?

**Verdict:**
1. **Reload during active run succeeds:** Goose accepts `session/load` while a turn is actively streaming or executing a tool call (`load_success: true`). The JSON-RPC reply arrives on the session stream (`session:<sid>`), not the connection stream.
2. **In-memory agent is preserved:** No secondary agent process or threads are spawned (thread diff = 0, child process PIDs unchanged, RSS delta < 125 kB). In-flight sub-processes (such as bash `sleep 40`) continue undisturbed to completion.
3. **Replay vs. live frames:** Replay re-sends **COMMITTED** messages of the in-flight turn (earlier steps already streamed live), never the uncommitted in-flight message; a client must replace/dedupe by `messageId`, not append. In-flight assistant chunks are not committed to storage mid-run and continue streaming live. While `session/load` is being processed, live chunks can interleave before the load reply. When live frames are classified properly (by checking whether `messageId` belongs to committed storage), all streamed live text (before drop + live after resume) matches final export with **0 missing characters** across both runs (`exact_match: true`). The previously observed "14 chars missing" was an artifact of lumping an interleaved in-flight live chunk (`' count and add'`) into replay frames.
4. **CWD side effect:** `session/load` rewrites `session.working_dir` in storage whenever the passed `cwd` differs from stored state (`cwd_rewritten: true`). Therefore, honkd must always pass the stored `cwd` queried from `session/list`.
5. **Cross-connection reload:** When `session/load` is issued on a fresh Connection B while Connection A is running a prompt, the load succeeds on B and receives historical replay, but **all subsequent live streaming chunks remain strictly pinned to Connection A**. Honkd cannot migrate an in-flight stream to a new connection via `session/load`.
6. **Export reconciliation path:** `_goose/unstable/session/export` succeeds mid-run (`mid_run_export_ok: true`) as a read-only query against committed storage with zero side effects. Waiting for turn end or reconciling via export is cleaner and avoids stitching an in-flight message.

**Decision-rule outcomes:**
1. **Reload rejected during a run:** Not rejected (`load_success: true`). Goose admits mid-run reloads on the same connection.
2. **Replay + live duplicates:** Replay re-sends earlier committed steps of the turn (overlapping 100% with the corresponding messageId already streamed before drop, e.g. 71–79 chars across `msg_vrtx_...`), but never the in-flight uncommitted message (`replay_contained_inflight: false`). If a client naively appends replay to live chunks, it duplicates earlier steps; a client must replace/dedupe by `messageId`. Live streaming chunks suffered zero loss across the disconnect when properly partitioned (`missing_chars: 0`, `exact_match: true`).
3. **Second agent spawned:** No (`children_changed: false`, thread diff 0). The in-memory agent is reused. However, cross-connection reloads do not divert live frames.
4. **CWD rewritten by load:** Confirmed (`cwd_rewritten: true`). Honkd must always pass the stored `cwd`.

**Probe results:**

### P3a1: Same-connection reload mid-run (streaming tides)
- **Run 1:**
  - Session: `20261011_19` | Connection: `edda307f-4fbc-4c92-bb96-1313ae8cff7a`
  - Chunks before drop: 10 (120 chars, in-flight msg: `msg_vrtx_011CfuXSAARKzsZdzs6hStFo`) | Reattach 409 window: 0ns
  - Mid-run export OK: `true` (committed msg IDs: 4, assistant chars in export: 71, convo len: 4)
  - `session/load` success: `true` | Reply stream: `session:20261011_19`
  - Replay frames: 4 (1 user chunk, 1 agent chunk [71 chars]) | Replay `_meta`: `["goose"]`
  - Replay overlap with before-drop text: 71 chars across `["msg_vrtx_011CfuXRX31sFezWMr4PY7Ym"]` (100% duplicate of committed step)
  - Replay contained in-flight msg: `false`
  - Interleaved live chunks before load reply: 1 (10 chars)
  - Live chunks after drop: 104 (973 chars)
  - Proc stats: thread diff: 0, RSS diff: +124 kB, children changed: `false`
  - Total streamed live text: 1093 chars (120 before drop + 973 live) | Final export: 1093 chars | Missing: 0 | Exact match: `true`
  - Subsequence: `true` | Prefix: `true` | `FINISHED` present: `true` | Duration: 35.07s
  - Fixture: `fixtures/goose-1.54.0-patched.2/p3a/1791679514.jsonl`
- **Run 2:**
  - Session: `20261011_20` | Connection: `d4f076b7-b65d-4a78-966c-e937bf8ab1a6`
  - Chunks before drop: 10 (103 chars, in-flight msg: `msg_vrtx_011CfuXV3G2BSTLUgwj79cGe`) | Reattach 409 window: 0ns
  - Mid-run export OK: `true` (committed msg IDs: 4, assistant chars in export: 79, convo len: 4)
  - `session/load` success: `true` | Reply stream: `session:20261011_20`
  - Replay frames: 4 (1 user chunk, 1 agent chunk [79 chars]) | Replay `_meta`: `["goose"]`
  - Replay overlap with before-drop text: 79 chars across `["msg_vrtx_011CfuXUM3aPuHJD6tNZFg2t"]` (100% duplicate of committed step)
  - Replay contained in-flight msg: `false`
  - Interleaved live chunks before load reply: 1 (16 chars)
  - Live chunks after drop: 68 (690 chars)
  - Proc stats: thread diff: 0, RSS diff: +4 kB, children changed: `false`
  - Total streamed live text: 793 chars (103 before drop + 690 live) | Final export: 793 chars | Missing: 0 | Exact match: `true`
  - Subsequence: `true` | Prefix: `true` | `FINISHED` present: `true` | Duration: 20.95s
  - Fixture: `fixtures/goose-1.54.0-patched.2/p3a/1791679554.jsonl`

### P3a2: Same-connection reload mid-run (tool call sleep 40)
- **Run 1:**
  - Session: `20261011_12` | Connection: `3e96a2ce-709a-492a-9377-8940d3fc433e`
  - Pre-load children: `["2684253 bash -c sleep 40 && echo done"]`
  - Post-load children: `["2684253 bash -c sleep 40 && echo done"]` (tool call process preserved)
  - `session/load` success: `true` | Reply stream: `session:20261011_12`
  - Prompt reply received: `true` | Run-end received: `true` | `FINISHED` present: `true` | Duration: 43.02s
  - Fixture: `fixtures/goose-1.54.0-patched.2/p3a_sleep/1791678568.jsonl`
- **Run 2:**
  - Session: `20261011_16` | Connection: `3ecb017c-cb3b-4a97-8c21-18bfc2b18e11`
  - Pre-load children: `["2746300 bash -c sleep 40 && echo done"]`
  - Post-load children: `["2746300 bash -c sleep 40 && echo done"]` (tool call process preserved)
  - `session/load` success: `true` | Reply stream: `session:20261011_16`
  - Prompt reply received: `true` | Run-end received: `true` | `FINISHED` present: `true` | Duration: 42.74s
  - Fixture: `fixtures/goose-1.54.0-patched.2/p3a_sleep/1791678727.jsonl`

### P3b: Stored CWD rewrite check
- **Run 1:**
  - Session: `20261011_13`
  - Initial CWD: `/tmp/nix-shell.Otyswj/.tmpuifGeh` | New CWD: `/tmp/nix-shell.Otyswj/.tmpxhDIUi`
  - Updated CWD from `session/list`: `/tmp/nix-shell.Otyswj/.tmpxhDIUi` | CWD rewritten: `true` | Duration: 0.66s
  - Fixture: `fixtures/goose-1.54.0-patched.2/p3b/1791678614.jsonl`
- **Run 2:**
  - Session: `20261011_17`
  - Initial CWD: `/tmp/nix-shell.dSVuGT/.tmp8eskcd` | New CWD: `/tmp/nix-shell.dSVuGT/.tmpL3VkvT`
  - Updated CWD from `session/list`: `/tmp/nix-shell.dSVuGT/.tmpL3VkvT` | CWD rewritten: `true` | Duration: 1.09s
  - Fixture: `fixtures/goose-1.54.0-patched.2/p3b/1791678773.jsonl`

### P3c: Cross-connection reload mid-run
- **Run 1:**
  - Session: `20261011_14` | Conn A: `08f2ca22-69d0-43af-8c8d-e1c9b70d1e29` | Conn B: `c2bf4926-f979-48aa-af54-cc6e9dfbbc13`
  - Conn B session stream 409 conflict: `false` (mailboxes scoped to connection)
  - Conn B `session/load` success: `true` | Reply stream: `session:20261011_14`
  - Live chunks after load on Conn A: 46 | Live chunks after load on Conn B: 1 (load replay only, 0 live stream deltas)
  - Conn A run-end received: `true` | Conn B run-end received: `false`
  - Total duration: 12.93s
  - Fixtures: `fixtures/goose-1.54.0-patched.2/p3c/1791678621.jsonl` & `p3c_b/1791678631.jsonl`
- **Run 2:**
  - Session: `20261011_18` | Conn A: `0671ae21-ba57-4444-949a-84ff2f9c282e` | Conn B: `1efa43ac-e2d2-4c03-8775-7de79451e923`
  - Conn B session stream 409 conflict: `false`
  - Conn B `session/load` success: `true` | Reply stream: `session:20261011_18`
  - Live chunks after load on Conn A: 48 | Live chunks after load on Conn B: 1 (load replay only, 0 live stream deltas)
  - Conn A run-end received: `true` | Conn B run-end received: `false`
  - Total duration: 12.52s
  - Fixtures: `fixtures/goose-1.54.0-patched.2/p3c/1791678776.jsonl` & `p3c_b/1791678785.jsonl`

## P4: Pending permission with no listener

**Question:** What happens when a tool execution requires confirmation in `approve` mode while no client is listening? Does goose hold the permission request or redeliver it upon reconnection? If dropped before delivery, does `session/load` re-send it? Can an answer be submitted after a 90s listener disconnect, and does the turn finish? After a cancel-style teardown (connection DELETE), is a persisted permission re-sent on a new connection, and does answering it execute anything (with state machine off vs on)?

**Verdict:**
1. **No redelivery over SSE:** When a connection drops while a `session/request_permission` is pending, the request frame is not redelivered over the resumed SSE stream (`redelivered: false`). Frames are at-most-once in `agent-client-protocol-http` (dequeued from internal mpsc before writing).
2. **`session/load` does NOT re-send permission requests during active runs:** Calling `session/load` on the resumed connection does NOT re-send `session/request_permission` (`resent_on_load: false`).
3. **Pending request survives on the same connection:** Goose's in-memory `ConnectionTo` holds the pending `SentRequest` route, and `ToolConfirmationRouter` continues awaiting confirmation indefinitely (tested across a 90-second listener drop without timing out). When the client answers the original request ID (`orig_perm_id`) on the resumed connection, goose accepts it (`respond_ok: true`), routes the answer to the waiting tool execution task, runs the tool (`P4OK` output captured in export), and completes the turn (`FINISHED`, `stopReason: end_turn`) in ~1.2–1.8 seconds post-answer!
4. **Lost permission frame is fatal to the turn:** If the connection drops before the client reads the permission request (e.g. right as `tool_call` notification appears), the client never learns the request ID. Because neither SSE resume nor `session/load` redelivers the request, the turn is stuck awaiting confirmation until timeout or cancellation. The client cannot complete the turn.
5. **Teardown (DELETE) cancels the run; cross-connection answering is ineffective:** When Connection A is DELETED, the active run is cancelled. On a new Connection B, `session/load` does NOT re-send any persisted permission requests (`is_resent_on_conn_b: false`), regardless of whether the state machine was enabled on the prompt (`_meta: { goose: { unrolledAgentLoop: true } }`) or disabled (`_meta: { goose: { unrolledAgentLoop: false } }`). If Connection B sends a `respond` to the old permission ID, the HTTP server returns 202 Accepted, but goose executes nothing (`tool_ran: false`, `activity_events: 0`, `turn_finished: false`).
6. **Elicitation forms (P4d):** Marked untested per spike plan §P4.4 (sandbox only carries the developer extension; no eliciting MCP is installed).

**Decision-rule outcomes:**
1. **Redelivery works:** FALSE. Goose does NOT redeliver pending request frames on SSE stream re-attachment.
2. **`session/load` re-sends:** FALSE. Mid-run `session/load` does not re-send pending tool permissions in the running agent.
3. **Implication for honkd:** Because neither redelivery nor `session/load` re-sends the request frame, **honkd must never lose or drop its internal permission request handle while waiting for user interaction**. If honkd itself drops the frame or crashes before recording the request ID, the request is permanently lost; honkd must surface "request lost; cancel and re-prompt" to viewers rather than waiting indefinitely. However, if honkd holds the request ID in memory/state, it CAN safely buffer and hold pending requests while no TUI/viewer is attached, re-attach to the same connection, and submit the decision even after long delays (90s+).
4. **An answer after reload/teardown executes nothing:** Confirmed. Submitting an answer on a new connection after teardown returns HTTP 202 but triggers no execution. Honkd must surface "request is stale, re-prompt" to viewers rather than presenting an answerable dialog for torn-down runs.

**Source References:**
- `goose-patched/crates/goose/src/acp/server.rs`:
  - `handle_tool_permission_request` (lines 1570–1635): Sends `RequestPermissionRequest` with options `[AllowAlways, AllowOnce, RejectOnce, RejectAlways]`, registers callback via `cx.send_request(...).on_receiving_result(...)`, which delivers result to `agent.submit_tool_confirmation`.
  - Lines 2324–2339 & 422–427: `use_state_machine_from_meta` checks `_meta.goose.unrolledAgentLoop`, falling back to `crate::agents::state_machine::enabled()` (`GOOSE_STATE_MACHINE` env var).
- `goose-patched/crates/goose/src/acp/server/load_session.rs`:
  - Lines 455–482: `handle_load_session` checks `crate::agents::state_machine::enabled()` (which only checks env var `GOOSE_STATE_MACHINE`, not prompt `_meta`). When disabled, calls `resend_pending_tool_permissions`, which inspects `session.conversation` for `ActionRequiredData::ToolConfirmation`. In a cancelled/torn down turn, uncommitted action items are not persisted, so no confirmations are re-sent.
- `goose-patched/crates/goose/src/agents/tool_confirmation_router.rs`:
  - Lines 8–52: `ToolConfirmationRouter` uses in-memory `tokio::sync::oneshot` channels keyed by `(session_id, request_id)`. Pending oneshots survive connection drops as long as the agent task remains alive.
- `/tmp/opencode/acphttp/agent-client-protocol-http-2.2.0/src/`:
  - `http_server.rs:370-396`: SSE receiver pulls frames from an internal mpsc channel. Once dequeued, frames are not preserved for new stream subscribers.
  - `connection.rs:307-355`: `pending_routes` maps request IDs to response routes within the same connection.
- `/tmp/opencode/acp-schema/agent-client-protocol-schema-1.9.1/src/v1/client.rs`:
  - Lines 1111–1178: `RequestPermissionResponse` expects `{"outcome": {"outcome": "selected", "optionId": "allow_once"}}`.

**Probe results:**

### P4a: No listener (90s drop while permission pending, reattach same connection)
- **Run 1:**
  - Session: `20261011_29` | Connection: `4cca73c1-7a78-43f4-8691-e5af42af88c9`
  - Original permission request ID: `dba7dcad-e605-48b2-9018-b01090c04141`
  - Options offered: `["allow_always", "allow_once", "reject_once", "reject_always"]`
  - Drop elapsed: 90.00s | Redelivered on stream: `false` | Re-sent on load: `false`
  - Answering original ID: `respond_ok: true`
  - Prompt reply received: `true` | Run-end received: `true` | `FINISHED` present: `true`
  - Tool output in export: `true` (`P4OK` found in shell `toolResponse`)
  - Post-answer duration: 1.81s | Total duration: 102.66s
  - Fixture: `fixtures/goose-1.54.0-patched.2/p4a/1791680707.jsonl`
- **Run 2:**
  - Session: `20261011_30` | Connection: `0b8fd744-f604-4960-a1fd-cbe57ae072bb`
  - Original permission request ID: `4b3c915f-9ee7-4673-b1e6-a0dd38caed5b`
  - Options offered: `["allow_always", "allow_once", "reject_once", "reject_always"]`
  - Drop elapsed: 90.00s | Redelivered on stream: `false` | Re-sent on load: `false`
  - Answering original ID: `respond_ok: true`
  - Prompt reply received: `true` | Run-end received: `true` | `FINISHED` present: `true`
  - Tool output in export: `true` (`P4OK` found in shell `toolResponse`)
  - Post-answer duration: 1.18s | Total duration: 102.09s
  - Fixture: `fixtures/goose-1.54.0-patched.2/p4a/1791680818.jsonl`

### P4b: Lost-permission frame (drop on tool_call before permission frame read)
- **Run 1:**
  - Session: `20261011_23` | Connection: `65b70671-3984-44b9-825d-ba1214576b1e`
  - Permission read before drop: `false`
  - Permission delivered on resumed stream: `null` (`false`)
  - `session/load` success: `true` | Permission re-sent after load: `false`
  - No permission ID available to answer: turn parked awaiting confirmation
  - Tool ran: `false` | Turn finished: `false` | Total duration: 73.95s
  - Fixture: `fixtures/goose-1.54.0-patched.2/p4b/1791680408.jsonl`
- **Run 2:**
  - Session: `20261011_24` | Connection: `3afebeeb-a242-4728-87f5-7cb04a52fbb6`
  - Permission read before drop: `false`
  - Permission delivered on resumed stream: `null` (`false`)
  - `session/load` success: `true` | Permission re-sent after load: `false`
  - No permission ID available to answer: turn parked awaiting confirmation
  - Tool ran: `false` | Turn finished: `false` | Total duration: 73.64s
  - Fixture: `fixtures/goose-1.54.0-patched.2/p4b/1791680518.jsonl`

### P4c: Teardown (DELETE connection, new connection, session/load)
- **Run 1 (State machine OFF):**
  - Session: `20261011_25` | Conn A: `1716539c-83f1-40eb-892a-aa2986c2ef2e` | Conn B: `354e3fe9-17d7-4569-a6be-bb1136a0b3d7`
  - Conn A permission ID: `288b7a65-5d8d-4c20-84f8-b0b711a36a5f`
  - Conn A DELETED (cancelling run)
  - Conn B `session/load`: `true` | Persisted permission re-sent on Conn B: `false`
  - Respond to Conn A ID on Conn B: HTTP 202 (`true`)
  - Conn B activity events: 0 | Tool executed: `false` | Turn finished: `false`
  - Total duration: 22.07s
  - Fixture: `fixtures/goose-1.54.0-patched.2/p4c_sm_off/1791680599.jsonl`
- **Run 2 (State machine OFF):**
  - Session: `20261011_26` | Conn A: `81c86f22-f828-47c4-9f97-59761ac75899` | Conn B: `44fbe62f-2764-42b6-9728-7db9f7a68431`
  - Conn A permission ID: `dad7b304-d8eb-47ba-8cd9-df8f019f5b77`
  - Conn A DELETED (cancelling run)
  - Conn B `session/load`: `true` | Persisted permission re-sent on Conn B: `false`
  - Respond to Conn A ID on Conn B: HTTP 202 (`true`)
  - Conn B activity events: 0 | Tool executed: `false` | Turn finished: `false`
  - Total duration: 20.90s
  - Fixture: `fixtures/goose-1.54.0-patched.2/p4c_sm_off/1791680626.jsonl`
- **Run 3 (State machine ON via `_meta.goose.unrolledAgentLoop: true`):**
  - Session: `20261011_27` | Conn A: `24a8d5a3-2930-4ebc-9459-e18535345cbd` | Conn B: `d6aa0e26-2dfa-4616-be33-ee6153e6e483`
  - Conn A permission ID: `b5b0a22b-bd02-4539-9b4f-6a3601556902`
  - Conn A DELETED (cancelling run)
  - Conn B `session/load`: `true` | Persisted permission re-sent on Conn B: `false`
  - Respond to Conn A ID on Conn B: HTTP 202 (`true`)
  - Conn B activity events: 0 | Tool executed: `false` | Turn finished: `false`
  - Total duration: 20.95s
  - Fixture: `fixtures/goose-1.54.0-patched.2/p4c_sm_on/1791680651.jsonl`
- **Run 4 (State machine ON via `_meta.goose.unrolledAgentLoop: true`):**
  - Session: `20261011_28` | Conn A: `761c824c-e15e-4c87-81ef-322e2d2e0997` | Conn B: `85aefb73-a086-4784-ba40-6c4d3a8e66cc`
  - Conn A permission ID: `119856dc-6fdc-4502-bf69-b986e539f9e2`
  - Conn A DELETED (cancelling run)
  - Conn B `session/load`: `true` | Persisted permission re-sent on Conn B: `false`
  - Respond to Conn A ID on Conn B: HTTP 202 (`true`)
  - Conn B activity events: 0 | Tool executed: `false` | Turn finished: `false`
  - Total duration: 21.67s
  - Fixture: `fixtures/goose-1.54.0-patched.2/p4c_sm_on/1791680681.jsonl`

### P4d: Elicitation forms
- Marked untested per spike plan §P4.4: Sandbox carries only the developer extension (no eliciting MCP server installed).

## P5: Cancel, steer, restart

### Executive Summary & Verdicts
1. **P5a: `session/cancel` notification stops the run immediately and kills child processes; works from any connection.**
   - Sending `session/cancel` stops the turn with `stopReason: "cancelled"` in 54–77ms.
   - The shell child process (`sleep 40`) is terminated immediately upon cancellation (0 children remaining under goose PID).
   - Crucially, `session/cancel` succeeds from a fresh Connection B both **without** `session/load` (75ms) and **with** `session/load` (54ms).
   - In all cancellation scenarios, the user prompt is persisted on disk (`export_user_persisted: true`), while in-flight uncommitted assistant turns or tool responses are discarded.
2. **P5b: `$/cancel_request` protocol notification is accepted by HTTP transport but completely ignored by goose.**
   - The notification is accepted with HTTP 202, but goose does not monitor request cancellation tokens for `session/prompt` (`on_prompt` ignores `responder.cancellation()`).
   - The `sleep 40` process continues running, no prompt reply is returned, and the run does not stop.
   - `honkd` must NOT use `$/cancel_request` to stop turns; prompt handles must remain alive until `session/cancel` or turn completion.
3. **P5c: `session/cancel` while parked on a permission request does NOT stop the run without an answer.**
   - When goose is parked awaiting user approval (`session/request_permission`), `session/cancel` alone leaves goose parked indefinitely (`stopped_without_answer: false` across 15s wait).
   - The moment the client responds to the permission request with `outcome: "cancelled"` (`{"outcome": {"outcome": "cancelled"}}`), the turn terminates immediately (2ms latency) with `stopReason: "cancelled"`.
   - `honkd`'s Cancel operation must both notify `session/cancel` and submit `cancelled` outcomes to all outstanding permission requests.
4. **P5d: `_goose/unstable/session/steer` is consumed early and late; goose resets chat exit if steers are queued.**
   - Early steers (during tool execution) succeed and steer subsequent model outputs (`assistant_steered: true`, text `P5D_EARLY_STEERED`, message persisted in conversation).
   - Late steers sent during the first `agent_message_chunk` are queued by goose; goose's loop resets `exit_chat = false` upon detecting pending steers, triggering a subsequent model inference that produces `P5D_LATE_STEERED`.
   - Because `clear_active_run` unconditionally discards pending steers once the agent stream terminates, steers that race with turn completion are dropped; `honkd` must verify consumption via stream/export and fall back to queueing as a regular prompt.
5. **P5e: Sandbox restart mid-turn returns 404, wipes in-flight processes, and leaves an unfulfilled turn that replays on load.**
   - Sandbox restart drops active connections: POST and GET with old connection IDs return HTTP 404 Not Found.
   - Goose accepts fresh `initialize` calls within ~205ms.
   - In-flight shell processes (`sleep 40`) die with the systemd cgroup and do NOT survive restart.
   - Calling `_goose/unstable/session/export` fails on a fresh connection before `session/load`.
   - `session/load` on the fresh connection succeeds and replays the interrupted user message (`user_message_chunk`).
   - The session remains fully functional: follow-up prompts execute normally.

**Decision-rule outcomes:**
1. **Cancel from B works:** TRUE. `honkd` may issue `session/cancel` from any connection; it does not need to proxy cancellations through the original prompting connection or issue `session/load` first.
2. **`$/cancel_request` honored:** FALSE. `honkd` must never rely on `$/cancel_request` to stop runs, and must prevent client handle drops from causing unintended cancellations.
3. **Cancel while parked doesn't stop the run:** TRUE. `honkd`'s Cancel handler must notify `session/cancel` AND explicitly answer all outstanding pending permissions with outcome `cancelled`.
4. **Steer consumption confirmed:** TRUE. `honkd` must track steer consumption via stream events or export, falling back to prompt queueing if rejected or dropped.
5. **Reconnect on restart:** On HTTP 404, `honkd` must re-initialize, reload sessions via `session/load` (passing stored CWD), mark interrupted turns as aborted/unanswered, and offer re-prompt.

**Source References:**
- `goose-patched/crates/goose/src/acp/server.rs`:
  - `on_cancel` (lines 2420–2437): Looks up `self.active_runs.agent_cancel_token(&session_id)` across the global server state, cancelling the token regardless of connection origin.
  - `on_steer_session` (lines 2378–2418): Method `_goose/unstable/session/steer` requires `expectedRunId` matching `active_runs.agent_run(session_id)`. Pushes message to `agent.steer(&session_id, message)`.
  - `clear_active_run` (lines 1929–1952): Removes run from `active_runs` and invokes `agent.discard_pending_steers(session_id)`.
  - `handle_tool_permission_request` (lines 1606–1632): Awaits response from client; on receive, delivers decision to `agent.submit_tool_confirmation`.
- `goose-patched/crates/goose/src/acp/server/dispatch.rs`:
  - Lines 124–141: `on_prompt` is spawned without inspecting `responder.cancellation()`, ignoring `$/cancel_request`.
  - Lines 143–147: `CancelNotification` maps to `agent.on_cancel(notif)`.
- `goose-patched/crates/goose/src/agents/agent.rs`:
  - Lines 2761–2786: `can_drain_pending_steers` drains queued steers and persists them into conversation before next tool/output.
  - Lines 3467 & 3644–3646: If `has_pending_steers` is true, `exit_chat` is reset to `false`, continuing the agent loop for the steered message.
- `goose-patched/crates/goose/src/agents/tool_execution.rs`:
  - Lines 211–227: `confirmation_rx.await` parks the agent awaiting tool confirmation without `select!` on `cancellation_token`.

**Probe results:**

### P5a: Cancel notification (`session/cancel`)
- **Run 1 (Conn A - Prompting connection):**
  - Session: `20261011_32` | Connection: `097525b1-f9d9-44b0-95f4-e515e1e831af`
  - Goose PID: `3727793` | Sleep child before cancel: `["3789298 bash -c sleep 40 && echo done"]`
  - Cancel latency: 74ms | Stop reason: `cancelled`
  - Sleep child alive after cancel: `false` (0 children)
  - Export: user persisted = `true`, assistant persisted = `false`, message count = 2
  - Fixture: `fixtures/goose-1.54.0-patched.2/p5a_conn_a/1791681911.jsonl`
- **Run 2 (Conn A - Prompting connection):**
  - Session: `20261011_33` | Connection: `cd3e0a01-5eac-4f29-9758-6474ee1d2d44`
  - Goose PID: `3727793` | Sleep child before cancel: `["3792252 bash -c sleep 40 && echo done"]`
  - Cancel latency: 77ms | Stop reason: `cancelled`
  - Sleep child alive after cancel: `false` (0 children)
  - Export: user persisted = `true`, assistant persisted = `false`, message count = 2
  - Fixture: `fixtures/goose-1.54.0-patched.2/p5a_conn_a/1791681917.jsonl`
- **Run 3 (Conn B - Fresh connection, without `session/load`):**
  - Session: `20261011_34` | Conn A: `5358061c-2b6d-4fbf-b1ef-d3f4b8760f27` | Conn B: `78103b18-3852-43c9-9154-a412570a3c96`
  - Cancel latency on Conn A: 75ms | Stop reason: `cancelled`
  - Sleep child alive after cancel: `false` (0 children)
  - Export: user persisted = `true`, assistant persisted = `false`, message count = 2
  - Fixture: `fixtures/goose-1.54.0-patched.2/p5a_conn_b_no_load/1791681922.jsonl`
- **Run 4 (Conn B - Fresh connection, with `session/load`):**
  - Session: `20261011_35` | Conn A: `9e457dbf-cd07-4c16-b518-c810b88170a8` | Conn B: `4e65efa3-33a3-4205-94a9-00a4a0b9b1a2`
  - Cancel latency on Conn A: 54ms | Stop reason: `cancelled`
  - Sleep child alive after cancel: `false` (0 children)
  - Export: user persisted = `true`, assistant persisted = `false`, message count = 2
  - Fixture: `fixtures/goose-1.54.0-patched.2/p5a_conn_b_with_load/1791681926.jsonl`

### P5b: `$/cancel_request` (protocol-level request cancellation)
- **Run 1:**
  - Session: `20261011_36` | Connection: `18d19014-5632-4925-9b88-c9e955875f2b`
  - Prompt Request ID: `5` | `$/cancel_request` accepted: `true` (HTTP 202)
  - Observed for 15s: prompt reply received = `false`, sleep child alive = `true` (`honored: false`)
  - Fallback `session/cancel` stopped turn immediately (`stopReason: cancelled`)
  - Export: user persisted = `true`, assistant persisted = `false`, message count = 2
  - Fixture: `fixtures/goose-1.54.0-patched.2/p5b/1791681931.jsonl`
- **Run 2:**
  - Session: `20261011_37` | Connection: `6c4c6849-d3fb-4d44-86c2-f934e1637b85`
  - Prompt Request ID: `5` | `$/cancel_request` accepted: `true` (HTTP 202)
  - Observed for 15s: prompt reply received = `false`, sleep child alive = `true` (`honored: false`)
  - Fallback `session/cancel` stopped turn immediately (`stopReason: cancelled`)
  - Export: user persisted = `true`, assistant persisted = `false`, message count = 2
  - Fixture: `fixtures/goose-1.54.0-patched.2/p5b/1791681951.jsonl`

### P5c: Cancel while parked on permission request
- **Run 1:**
  - Session: `20261011_38` | Connection: `a2940745-00af-418a-829a-e121c4129e58`
  - Permission ID: `5572ce7c-6769-4b66-8007-976145db7ebb`
  - `session/cancel` sent without answer; observed for 15s: stopped without answer = `false`
  - Answered permission with `outcome: "cancelled"`: respond OK = `true`
  - Post-answer latency: 2ms | Stop reason: `cancelled`
  - Export: user persisted = `true`, assistant persisted = `false`, message count = 2
  - Fixture: `fixtures/goose-1.54.0-patched.2/p5c/1791681970.jsonl`
- **Run 2:**
  - Session: `20261011_39` | Connection: `2184b84a-290c-4339-8dbd-19ac74d784bb`
  - Permission ID: `ed0699f1-43f5-4a3d-8703-2eba3be84a44`
  - `session/cancel` sent without answer; observed for 15s: stopped without answer = `false`
  - Answered permission with `outcome: "cancelled"`: respond OK = `true`
  - Post-answer latency: 2ms | Stop reason: `cancelled`
  - Export: user persisted = `true`, assistant persisted = `false`, message count = 2
  - Fixture: `fixtures/goose-1.54.0-patched.2/p5c/1791681989.jsonl`

### P5d: Steer lifecycle (`_goose/unstable/session/steer`)
- **Run 1 (Early steer control - during tool call):**
  - Session: `20261011_40` | Active Run ID: `run_43614ed5-eb7d-45b0-8e52-d599969a0149`
  - Steer response: `Ok({ messageId: "steer_7eb2bac7-a27a-4c07-afba-1682901cc9d4", runId: "..." })`
  - Assistant steered: `true` (replied `P5D_EARLY_STEERED`)
  - Steer in conversation export: `true` | Steer dropped: `false`
  - Total messages in export: 6 | Duration: 11.62s
  - Fixture: `fixtures/goose-1.54.0-patched.2/p5d_early/1791682010.jsonl`
- **Run 2 (Early steer control - during tool call):**
  - Session: `20261011_41` | Active Run ID: `run_d5868da0-dd45-4029-acd0-68784804bc03`
  - Steer response: `Ok({ messageId: "steer_e02048a9-6a2a-40f5-b596-0e2949c14b5c", runId: "..." })`
  - Assistant steered: `true` (replied `P5D_EARLY_STEERED`)
  - Steer in conversation export: `true` | Steer dropped: `false`
  - Total messages in export: 6 | Duration: 12.06s
  - Fixture: `fixtures/goose-1.54.0-patched.2/p5d_early/1791682026.jsonl`
- **Run 3 (Late steer - on first `agent_message_chunk`):**
  - Session: `20261011_43` | Active Run ID: `run_066e4464-43b5-47cd-bc52-5249ad9f5500`
  - Steer response: `Ok({ messageId: "steer_a5ea98a3-d9d0-450b-bd89-2a0ec47dab82", runId: "..." })`
  - Assistant steered: `true` (replied initial `FINISHED` followed by steered `P5D_LATE_STEERED`)
  - Steer in conversation export: `true` | Steer dropped: `false`
  - Total messages in export: 7 | Duration: 5.95s
  - Fixture: `fixtures/goose-1.54.0-patched.2/p5d_late/1791682069.jsonl`
- **Run 4 (Late steer - on first `agent_message_chunk`):**
  - Session: `20261011_44` | Active Run ID: `run_102be0f5-ace3-41b9-93cc-5593c187ec50`
  - Steer response: `Ok({ messageId: "steer_20c2fb41-b145-4335-b236-245eda423dfd", runId: "..." })`
  - Assistant steered: `true` (replied initial `FINISHED` followed by steered `P5D_LATE_STEERED`)
  - Steer in conversation export: `true` | Steer dropped: `false`
  - Total messages in export: 7 | Duration: 5.03s
  - Fixture: `fixtures/goose-1.54.0-patched.2/p5d_late/1791682095.jsonl`

### P5e: Goose sandbox restart mid-turn
- **Run 1:**
  - Session: `20261011_46` | Old Conn: `2f262ea5-364d-4cc7-b0bb-cba522219c24`
  - Initial goose PID: `3854699` | Sleep child: `["3890658 bash -c sleep 40 && echo done"]`
  - Restart command duration: 0.03s | Recovery duration to `initialize`: 205ms
  - New goose PID: `3890689` | New Conn: `ca495eba-3df9-4df9-88a7-1dcc5b88356f`
  - POST with old connection ID: HTTP 404 Not Found
  - GET with old connection ID: HTTP 404 Not Found
  - Sleep child survived under goose: `false` | System-wide: `false`
  - Export on fresh connection before `session/load`: `ok: false`
  - `session/load` on fresh connection: `ok: true` (replayed 4 frames including unfulfilled user prompt)
  - Export after `session/load`: user message persisted = `true`, assistant persisted = `false`, message count = 2
  - Follow-up prompt: succeeded with `stopReason: "end_turn"` (replied `RECOVERED`)
  - Fixture: `fixtures/goose-1.54.0-patched.2/p5e/1791682250.jsonl`
- **Run 2:**
  - Session: `20261011_47` | Old Conn: `734f72d4-0894-4dd4-b5f8-8d6ecbda1add`
  - Initial goose PID: `3890689` | Sleep child: `["3906168 bash -c sleep 40 && echo done"]`
  - Restart command duration: 0.03s | Recovery duration to `initialize`: 205ms
  - New goose PID: `3906172` | New Conn: `7e189a39-311c-45a6-94f7-4ffb5ebad8c2`
  - POST with old connection ID: HTTP 404 Not Found
  - GET with old connection ID: HTTP 404 Not Found
  - Sleep child survived under goose: `false` | System-wide: `false`
  - Export on fresh connection before `session/load`: `ok: false`
  - `session/load` on fresh connection: `ok: true` (replayed 4 frames including unfulfilled user prompt)
  - Export after `session/load`: user message persisted = `true`, assistant persisted = `false`, message count = 2
  - Follow-up prompt: succeeded with `stopReason: "end_turn"` (replied `RECOVERED`)
  - Fixture: `fixtures/goose-1.54.0-patched.2/p5e/1791682292.jsonl`

## P6: Message ids, resource cost, fixtures
_pending_

## P7: hydra-acp (optional)
_pending_

## Design amendments
_pending_
