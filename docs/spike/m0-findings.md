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
_pending_

## P5: Cancel, steer, restart
_pending_

## P6: Message ids, resource cost, fixtures
_pending_

## P7: hydra-acp (optional)
_pending_

## Design amendments
_pending_
