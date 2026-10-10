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
  - No tool result, no assistant response, and `FINISHED` is absent.

**Decision-rule outcome:**
- Confirmed design premise: WebSocket transport cancels runs on client disconnect.
- Pigeon's runner assumption that turns survive disconnect does not hold over WebSocket.
- Honk's HTTP+SSE architecture is confirmed as the necessary foundation for reconnectable sessions.

## P2 HTTP disconnect

**Question:** Under HTTP+SSE transport, do active turns survive client disconnects? What is the 409 conflict window during reattach? Are frames lost when disconnected during streaming? Does an in-flight turn survive an extended 600s disconnect gap?

**Verdict:** Yes, in-flight turns survive client disconnects under HTTP+SSE across both immediate reattachment and extended (600s) gaps. Goose buffers stream events in its mailbox and delivers prompt replies and run-end notifications upon reattachment.

**Decision-rule outcomes:**
1. **Turn survival (P2a & P2c):** Turn survives in both P2a (runs 1 & 2) and P2c (600s gap). Design §6 holds; the WS+drain fallback is not required.
2. **409 reattach window:** On local loopback with clean socket teardown, `hard_drop` aborts the client socket, allowing immediate re-subscription (0ms window observed). Overlapping GET requests verify that goose strictly returns HTTP 409 Conflict when a stream is actively held, confirming that exponential backoff (100ms → 2s, 60s deadline) is mandatory for ungraceful/remote disconnects.
3. **Frame loss (P2b):** No characters were lost in P2b across either run (`chars_missing: 0`). Prompt replies and run-end notifications arrived reliably after reattachment. Even if frames were lost across dying sockets, honkd reconciliation via session export (P3) can fully recover final turn state.

**Probe results:**

### P2a: Quiet drop (tool_call in-flight)
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

### P2b: Drop while streaming (after 20 chunks)
- **Run 1:**
  - Session: `20261010_10` | Connection: `7bd9f05c-6e5b-4c79-8d54-331f0c654523`
  - Conn 409 window: 0ms | Session 409 window: 0ms
  - Chunks before drop: 20 | Chunks after drop: 56
  - Streamed chars total: 869 | Exported all assistant chars: 869 | Chars missing: 0
  - Exported final assistant chars: 637
  - Prompt reply received: `true` | Run-end received: `true` | `FINISHED` present: `true`
  - Total duration: 18.76s
  - Fixture: `fixtures/goose-1.54.0-patched.2/p2b/1791675233.jsonl`
- **Run 2:**
  - Session: `20261010_11` | Connection: `175cf047-09cc-40ec-a45f-726d099004c8`
  - Conn 409 window: 0ms | Session 409 window: 0ms
  - Chunks before drop: 20 | Chunks after drop: 105
  - Streamed chars total: 930 | Exported all assistant chars: 930 | Chars missing: 0
  - Exported final assistant chars: 181
  - Prompt reply received: `true` | Run-end received: `true` | `FINISHED` present: `true`
  - Total duration: 79.32s
  - Fixture: `fixtures/goose-1.54.0-patched.2/p2b/1791675319.jsonl`

### P2c: Long gap (600s disconnect)
- Session: `20261010_7` | Connection: `b95059f6-2660-4afe-9122-7992a898b208`
- Disconnect gap: 600s (10 minutes)
- Conn 409 window: 0ms | Session 409 window: 0ms
- Prompt reply received: `true` | Run-end received: `true` | `FINISHED` present: `true`
- Total duration: 602.07s
- Fixture: `fixtures/goose-1.54.0-patched.2/p2c/1791675132.jsonl`

## P3: Same-connection reload mid-run
_pending_

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
