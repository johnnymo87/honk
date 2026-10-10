# honk: a ratatui TUI and session relay for goose over ACP

Status: design approved 2026-10-10, **revised the same day after adversarial and
oracle review** (see §10), pre-spike. Supporting research:
[`2026-10-10-honk-research.md`](2026-10-10-honk-research.md) (deep-research survey of
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

Verified by reading source (`git show v1.54.0:<path>`); items marked *M0* still need a
live measurement.

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
  `forward_agent_stream`, `server.rs:2126-2357`). No broadcast, no subscribe.
- **Losing the WebSocket cancels the turn.** WS close → `conn.shutdown()` aborts the
  handler task → `ActiveRunDropGuard` cancels the token (`server.rs:258-280`). This is
  deliberate upstream ("roaming"), and was already true in 1.48; pigeon's comment that
  turns survive disconnect is stale (and pigeon's reconnect path therefore cancels its
  own turns today).
- **Over HTTP+SSE the logical connection outlives the socket** (oracle reading of the
  2.2.0 crate): POSTs return 202, outbound frames queue in per-connection/per-session
  mailboxes keyed by `Acp-Connection-Id`/`Acp-Session-Id`, a new GET takes over the
  stream and drains the queue; the connection lives until DELETE. One listener per
  stream (second GET → 409); delivery is at-most-once if a socket dies mid-write;
  mailboxes are unbounded. *M0.*
- **Each connection gets its own in-memory agent** (`AgentManager` per
  `GooseAcpAgent`, `server.rs:973`), with its own MCP extension processes. Two
  connections on one session = two agents; extension state is last-writer-wins.
- **`session/load` is a mutating activation, not a read.** It rewrites the stored cwd
  if the passed cwd differs (`server.rs:1176-1178`), spawns every MCP extension
  (`load_session.rs:414`), re-sends persisted pending permissions (`:452-481`), and
  under `GOOSE_STATE_MACHINE=1` may start a run (`:460-467`). Replay is bounded by
  `_meta.replayTail` (cuts at a turn boundary, reports `replaySkipped`, `:88-105`,
  `:443-448`). Read-only alternative: `_goose/unstable/session/export`
  (`manage_sessions.rs:105-118`).
- **Pending state.** Tool permissions are persisted and re-sent on load. Elicitation
  forms time out after 300 s server-side (`mcp_client.rs:424`) and are auto-cancelled
  unless the client advertises elicitation (`elicitation.rs:26-44`). Unconsumed steers
  are discarded when a run ends or is cancelled (`server.rs:1929-1935`).
- **Model switch is per session** (`recreate_provider_for_session`). **Extension
  toggles**: use the session-scoped add/remove methods (`extensions.rs:8-36`), never
  the global `set_config_extension_enabled`; there is no "disable", re-enable means
  re-adding the full `ExtensionConfig`.
- **Subagents** are hidden from `session/list` (`server.rs:144`); their live progress
  arrives as `tool_call_update` with `_meta.toolNotification`.
- If a client advertises `terminal`, goose routes *all* shell execution to it.
- **Client-library hazard.** In `agent-client-protocol` 3.x, dropping a `SentRequest`
  sends `$/cancel_request`. Pin to goose's own versions (`agent-client-protocol`
  2.2.0, schema `=1.9.1`) and hold prompt handles explicitly.

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
isolation; pooling is a later optimization). `{connection_id, session_id}` is persisted
to honkd's state file; on restart honkd re-GETs each stream to drain queued frames,
then reconciles (below). Evicting a session = DELETE its connection (never
`session/close`, which cancels runs); idle sessions are evicted after a TTL to stop
their MCP processes, never while a run or request is pending.

**Cold attach.** For a session honkd does not hold, a viewer gets a read-only snapshot
from `_goose/unstable/session/export` (no extension spawn, no cwd rewrite). honkd
only activates the session upstream (`session/load` with the **stored** cwd from
`session/list`, bounded `replayTail`) on the first prompt, answer, or config change.

**Sequencing.** Every logged event carries `(epoch, seq)`; `epoch` changes on each
honkd start and on every upstream reconcile. A cursor from another epoch gets
`_honk/reset` + full replay, never a merge. Reconcile after reconnect = drop the log,
reload, emit reset; never append a replay to live deltas (replay is per content item,
live is per delta, so merging duplicates text). Stream vs replay message-id
equality is an *M0* check; block ids and scroll-to depend on it.

**Prompts.** `client_req_id` (in `_meta.honk`) dedupes retries; the ledger is in the
state file. Semantics:
- `queue` (TUI default): held until the active run ends; **dropped** if the user
  cancels the run.
- `steer` (pigeon's default): `_goose/unstable/session/steer`; if it loses the race
  ("no active run"), it becomes a prompt-now.
- A steer may overtake earlier queued prompts (intended: it targets the current run).
- The `session/prompt` response (with `stopReason`) goes to the originating client
  when the turn ends, as pigeon expects; for queued prompts it is late by design.

**Cancel.** Forwarded as the `session/cancel` notification; honkd also answers any
outstanding permission/form with reject/cancel so a run parked on a request stops
(*M0*: verify cancel alone does not).

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
  survive); honkd re-GETs, reconciles, bumps epoch; viewers get reset + replay. Frames
  lost mid-write are recovered by the reconcile reload. If M0 shows HTTP connections do
  *not* survive, fallback: WS upstream, honkd treated as an always-up service whose
  restart is an explicit drain ("wait for no active runs"), and `TurnEnded{cancelled,
  reason: relay_restart}` made visible to viewers and pigeon.
- **goose serve restart**: all runs die (goose's own behavior). honkd reconnects with
  backoff and staggers re-activation so N sessions don't spawn N×extensions at once;
  viewers see `cancelled`.
- **Turns started outside honkd** (goose CLI, scheduler): invisible until the next
  reconcile; honkd's prompt gets "busy" and falls back to queue-and-poll. Accepted.
- **Unknown ACP messages**: passed through; the reducer renders unknown updates as a
  dim raw block. Never panic.
- **Accepted losses on honkd restart**: none of queue / req-id ledger / held sessions
  (state file); attached-viewer set is not persisted (TUIs reconnect themselves;
  nightly restore is the switcher's job).
- **Security**: downstream listener on a 0700 directory under `$XDG_RUNTIME_DIR` (unix
  socket) or 127.0.0.1 with a token; peer uid checked. This is not a boundary against
  the agent itself (same uid); with allow-all permissions that is accepted.

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
| **M0 spike (1-2 days)** | (1) Is goose's agent loop good enough: one real day of root sessions on existing tooling. (2) HTTP+SSE vs WS disconnect behavior on the pinned goose. (3) Two connections on one session: visibility, extension state, `session/load` during another's run. (4) Permission/form with no listener: buffered? answerable later? (5) Cancel while parked on a permission; `$/cancel_request` honored? (6) Stream vs replay message ids. (7) Hydra-acp against `goose serve` (2 h timebox, prior art). (8) Write goose patch notes (detach-tolerant WS turns, `session/subscribe`) for a patch track carried in the author's goose fork; upstream proposal deferred until after living with the patches. | Findings written; §3 confirmed or design amended. (1) failing kills the project. |
| **M1** | honkd proxy (HTTP+SSE upstream, state file, attach/replay, pending requests, queue/steer); pigeon repointed at honkd; ~100-line `honk tail`. | A pigeon turn is visible in `honk tail`; `kill -9 honkd` mid-turn, restart: the turn completes and `tail` resumes without duplicated text; restart goose mid-turn: `cancelled` shown, retry not duplicated. |
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
