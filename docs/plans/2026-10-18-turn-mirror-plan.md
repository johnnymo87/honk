# Turn mirror (honkd turn hook + pigeon /goose/turn-event) Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development to implement this plan task-by-task.

**Goal:** Every turn of a pigeon-registered goose session — whichever client started it (honk chat, a coordinator wake, any other honkd client) — shows up in the session's Telegram topic: the prompt when the turn starts, the final reply when it ends. This is the parity pigeon's opencode plugin already gives opencode sessions.

**Architecture:** honkd gains a generic, pigeon-agnostic *turn hook*: with `--turn-hook-url`, every session actor reports `turn.started` / `turn.ended` events to a bounded in-memory queue drained by one sequential HTTP sender with retries. pigeon gains `POST /goose/turn-event`, which maps the goose session id to its registered `gse_` row, skips turns pigeon's own goose runner started, mirrors the prompt through the `/mirror` pipeline and posts the reply through the `/stop` pipeline. Each repo ships as its own PR; pigeon deploys first.

**Tech Stack:** Rust (tokio, axum, reqwest, serde, clap) in `honk`; TypeScript (Bun-style `Request`/`Response` handlers, vitest, better-sqlite3-like storage) in `pigeon`.

**Decisions (bead honk-9qf.13):** mirror everything, including automated turns, and accept the noise and the `markAllRead` side effect. No origin tagging, no quiet path for goose sessions. Known gaps, accepted: steer messages typed into a running turn are not mirrored; runs a client starts by talking to goose directly (bypassing honkd) are not seen; an event can be lost if honkd crashes between a turn ending and the POST, or if pigeon is unreachable for longer than the retry window (same loss class as the opencode plugin's in-memory stop queue); a turn that finishes faster than pigeon's outbox drains its mirror row can show its reply above its prompt (pigeon's outbox orders `stop` before `mirror` within a session).

---

## Wire contract: `turn-hook v1`

honkd → receiver, `POST <turn-hook-url>`, `Content-Type: application/json`, `Authorization: Bearer <token>` when a token file is configured. One event per request.

```json
{
  "v": 1,
  "type": "turn.started",
  "event_id": "<goose_sid>:<upstream_id>:started",
  "session_id": "<goose session id>",
  "upstream_id": "hk3_7-12",
  "run_id": "run_abc",
  "client_req_ids": ["crid-1"],
  "prompt_text": "user text of the prompt (capped)",
  "at_ms": 1792162675775
}
```

```json
{
  "v": 1,
  "type": "turn.ended",
  "event_id": "<goose_sid>:<upstream_id>:ended",
  "session_id": "...", "upstream_id": "...", "run_id": "run_abc or null",
  "client_req_ids": [], "prompt_text": "...", "at_ms": 0,
  "stop_reason": "end_turn | cancelled | max_tokens | ... | null",
  "honk_reason": "upstream_restart | delivery_unknown | cancelled | synthesized | null",
  "final_text": "last assistant message text, or null",
  "final_text_truncated": false,
  "error": "JSON-RPC error message when goose answered the prompt with an error, else null"
}
```

Rules:
- `upstream_id` is honkd's id for the upstream `session/prompt` (`hk{boot}_{actor}-{n}`, `crates/honkd/src/rpc.rs:193-204`); unique per prompt across restarts. It is the turn key on both sides.
- `prompt_text` and `final_text` are capped at 256 KiB (UTF-8 bytes, cut on a char boundary); `final_text_truncated` says whether the final was cut. `prompt_text` is cut silently.
- `client_req_ids` are the deduplicated `_meta.honk.client_req_id`s of the turn's owners (pigeon's goose runner sends its `commandId` there). May be empty.
- honkd emits `turn.started` at most once per `upstream_id`, when goose announces the run (the point where honkd synthesizes the user-message echo). A prompt that is never announced produces no `turn.started`.
- honkd emits `turn.ended` only for turns that ran or may have run: a reply from goose (result or error, except the busy rejection), the synthesized gap reply, or an interrupt of the in-flight prompt that was announced or whose reason is `upstream_restart` / `delivery_unknown`. It emits nothing for parked prompts that never ran, busy rejections, send failures, `not_delivered`, and `cancelled` on an unannounced prompt.
- After a honkd restart a persisted reply can be replayed, so the same `turn.ended` (same `event_id`) may arrive twice. Receivers must be idempotent on `event_id`.
- Receiver responses: any 2xx = accepted (including "ignored"); 4xx = permanent, honkd drops the event; 5xx / connection error / timeout = transient, honkd retries.

---

# Part A — pigeon PR (`~/projects/pigeon/.worktrees/goose-turn-event`, branch `goose-turn-event`)

Read pigeon's `AGENTS.md` first. Commands (run npm and git in separate bash calls, per AGENTS.md):
- one file: `cd packages/daemon && npx vitest run test/<file>.test.ts --maxWorkers=4`
- package: `npm run --workspace @pigeon/daemon test` and `npm run --workspace @pigeon/daemon typecheck`
- before PR: `npm run typecheck` and `npm run test` at the repo root.
Run anything longer than ~1 minute under `systemd-run --user --collect -p MemoryMax=8G` with output to a log file (see the ground rules).

### Task A1: Shared honk-receipt renderer

**Files:**
- Create: `packages/daemon/src/goose/render-turn.ts`
- Modify: `packages/daemon/src/goose/session-runner.ts` (receipt rendering in `finishTurn`, ~1546-1642; constants at ~217-218)
- Test: `packages/daemon/test/goose-render-turn.test.ts`

Extract the pure, reusable parts of the runner's receipt rendering so the hook path and the runner word things identically:
- `capText(text: string, cap: number): { text: string; truncated: boolean }` — the existing slice that avoids splitting a surrogate pair; `TRUNCATION_NOTICE` and `DEFAULT_MAX_TRANSCRIPT_BYTES` move here (re-export from session-runner if tests import them from there).
- `honkReasonDetail(stopReason, honkReason): string | undefined` — the existing `not_delivered` / `upstream_restart` / `delivery_unknown` sentences.
- `emptyReplyText(stopReason)` — the existing "goose ended the turn without a reply…" wording.
- `renderHookTurn(ev): { message: string; event: "Stop" | "Error"; errorKind?: string }` for `turn.ended` events:
  - body = `final_text` (capped with `capText` at `DEFAULT_MAX_TRANSCRIPT_BYTES`, plus `TRUNCATION_NOTICE` if capped here or `final_text_truncated` is true); if empty/null, `emptyReplyText(stop_reason)`.
  - append (blank line separated) `honkReasonDetail(...)` when it returns something; append `error` when present.
  - `honk_reason == "cancelled"` or (`stop_reason == "cancelled"` and no `honk_reason`): user cancelled → `event: "Stop"`, and append `"(cancelled)"`; if the body would be the empty-reply text, use `"Turn cancelled."` alone.
  - `event: "Error"` when `error` is set, or `honk_reason` is `upstream_restart` / `delivery_unknown`, or `stop_reason` is set and is neither `end_turn` nor `cancelled`. `errorKind`: `goose-turn-failed` (error), `honk-<honk_reason>`, or `goose-stop-<stop_reason>`.
  - `honk_reason == "synthesized"` renders like `end_turn`.

Steps:
1. Write failing tests for `capText` (surrogate boundary), `renderHookTurn` (end_turn with text; empty text; cancelled; upstream_restart; delivery_unknown; max_tokens; error; truncated flag; synthesized).
2. Run them; expect failures (module missing).
3. Implement `render-turn.ts`; change `session-runner.ts` to call `capText` / `honkReasonDetail` / `emptyReplyText` without changing its output.
4. Run the new tests plus `goose-session-runner.test.ts`, `goose-stop-reporting.test.ts`, `goose-inflight-runner.test.ts`; all pass unchanged.
5. Commit: `goose: share the receipt wording between the runner and the turn hook`.

### Task A2: Callable stop/mirror enqueue inside the app

**Files:**
- Modify: `packages/daemon/src/app.ts` (`/mirror` ~1434-1605, `/stop` ~1607-1750)
- Test: existing `mirror-route.test.ts`, `stop-idempotency.test.ts`, `anchor-capture.test.ts`, `ancillary-gate.test.ts` must stay green.

Refactor without behaviour change: move the bodies of the two routes into closures inside `createApp`, e.g. `handleStopBody(body: Record<string, unknown>): Promise<Response>` and `handleMirrorBody(body): Promise<Response>`, which the routes call after `readJsonBody`. The new route (A3) calls these directly, so it inherits idempotency, quiet policy (`resolveEffectivePolicy`), `markAllRead`, anchors, title handling and outbox writes. Keep every load-bearing ordering comment in place.

Steps: refactor → run the four test files plus the whole daemon package → commit `daemon: make the /stop and /mirror bodies callable in-process`.

### Task A3: `POST /goose/turn-event`

**Files:**
- Modify: `packages/daemon/src/app.ts` (new route next to `/goose/sessions`)
- Test: `packages/daemon/test/goose-turn-event.test.ts`

Behaviour (auth is the global `checkAuth`, nothing per route):
1. Parse body. 400 `{error}` if `v !== 1`, `type` not `turn.started`/`turn.ended`, `session_id` or `upstream_id` not a non-empty string.
2. `row = storage.sessions.getByBackendSessionId(session_id)`; if missing or `row.backendKind !== GOOSE_BACKEND_KIND` → 200 `{ok:true, ignored:"unregistered"}`.
3. Own-turn skip: for each `crid` in `client_req_ids`, if the goose in-flight store has a row for `crid` (`get(commandId)`, find how storage exposes it — it is the store the runner gets as `opts.inflight`) or `storage.outbox.getByNotificationId(\`s:${row.sessionId}:${sanitiseIdPart(crid)}\`)` exists → 200 `{ok:true, ignored:"own_turn"}`. (The runner writes the in-flight row before it sends the prompt and writes its outbox row before it retires the in-flight row, so one of the two always exists.)
4. `id = sanitiseIdPart(upstream_id)` (prefix `h-`).
5. `turn.started`: call `handleMirrorBody({sessionId: row.sessionId, messageId: \`h-${id}\`, text: prompt_text ?? ""})` and return its response.
6. `turn.ended`: `r = renderHookTurn(body)`; call `handleStopBody({session_id: row.sessionId, notification_id: \`s:${row.sessionId}:h-${id}\`, event: r.event, message: r.message, ...(r.errorKind ? {error_kind: r.errorKind} : {})})` and return its response. Do **not** pass `title` or `label`.
7. Log one line per event: `[goose-turn-event] type=… sessionId=… upstream=… outcome=…`.

Tests (build the app as in `stop-idempotency.test.ts`; register the goose row via `POST /goose/sessions` as in `goose-session-registration.test.ts`):
- unregistered goose id → 200 ignored, no outbox rows.
- `turn.started` → one `mirror` row `m:<gse>:h-<id>` whose text contains the prompt; `lastHumanMsgId` set.
- `turn.ended` → one `stop` row `s:<gse>:h-<id>`, payload text contains the final; repeat with the same body → still one row (idempotent).
- cancelled → Stop with "(cancelled)"; upstream_restart → Error.
- own-turn skip via an in-flight row for the crid; and via an existing outbox row `s:<gse>:<crid>`; a foreign crid is not skipped.
- validation 400s; auth: with `authToken` set, no bearer → 401.

Steps: failing tests → implement → green → commit `goose: mirror turns other honkd clients run into the session's topic`.

### Task A4: Docs

**Files:**
- Modify: `AGENTS.md` (Notifications section: new paragraph "Turns from other honkd clients" — what the route does, own-turn skip, idempotency, deploy-order note that honkd's hook needs this route first)
- Modify: `.agents/skills/daemon-architecture/SKILL.md` (Route Surface list)
- Create: `docs/contracts/honkd-turn-hook-v1.md` (the wire contract above, with pigeon's handling)

Commit: `docs: document /goose/turn-event`. Redact check (see Ground rules) on every changed doc.

---

# Part B — honk PR (`~/projects/honk/.worktrees/turn-mirror`, branch `turn-mirror`)

Ground rules: `~/.local/state/honk-m1a-ground-rules.md`, but the worktree is `/home/dev/projects/honk/.worktrees/turn-mirror` and the plan is this file. Commands:
- `nix develop -c cargo test -p honkd --lib turn::` / `--test <name>`
- `HONK_GOOSE_BIN=$HOME/.local/state/honk-goose-bin/patched-1.54.0.3/pkg/bin/goose nix develop -c cargo test --workspace` (under systemd-run, MemoryMax=10G)
- `nix develop -c cargo clippy --workspace --all-targets -- -D warnings`, `nix develop -c cargo fmt --all`.

### Task B1: Turn metadata out of TurnCore

**Files:**
- Modify: `crates/honkd/src/turn.rs`, `crates/honkd/src/state.rs` (`InflightPrompt`, `#[serde(default)] run_id`)
- Modify: `crates/honkd/src/session.rs` only as far as needed to compile (pass the new field through; no hook yet)
- Test: unit tests in `turn.rs`

Changes:
- `Inflight` gets `run_id: Option<String>`, set by `on_run_state(Some(r))` whenever an in-flight prompt exists (first announcement and later ones); persisted via `InflightPrompt.run_id` (serde default for old state files).
- New `pub struct TurnMeta { upstream_id: String, client_req_ids: Vec<String>, prompt_text: String, run_id: Option<String>, honk_reason: Option<String> }` built from an `Inflight` (`client_req_ids` deduped in order, like `to_persisted`; `prompt_text` via `crate::rpc::extract_user_text`).
- `Action::Reply` gains `turn: Option<TurnMeta>`:
  - `on_prompt_reply`: `Some` unless the reply is a busy error; `honk_reason` = `"synthesized"` if `result._meta.honk.synthesized == true`, else `result._meta.honk.reason` if a string, else `None`.
  - `on_prompt_send_failed`, `on_prompt_not_delivered`, `on_prompt` busy rejection: `None`.
  - `interrupt(reason)`: the in-flight owners get their own `Reply` with `turn: Some(meta with honk_reason = reason)` if `inflight.announced || reason == "upstream_restart" || reason == "delivery_unknown"`, else `None`; parked owners get a second `Reply` (same body) with `turn: None`. Downstream responses stay byte-identical.
- `SynthUser` gains `upstream_id: Option<String>` (the in-flight prompt's id when produced by `on_run_state`), so the session can key `turn.started`. Check the two other `SynthUser` producers in `session.rs` (~1626 probe `actualRunId`, ~2889 export shows delivered) and give them the in-flight `upstream_id` too.

Tests: run id recorded on the inflight and survives `to_persisted`/`from_persisted`; reply carries `TurnMeta` with crids/prompt/run; synthesized gap reply → `honk_reason "synthesized"`; busy → `None`; send failed / not delivered → `None`; interrupt with in-flight announced + parked → two replies, first `Some`, second `None`; interrupt `"cancelled"` unannounced → `None`; `"upstream_restart"` unannounced → `Some`. Update existing tests that destructure `Reply`/`SynthUser`.

Commit: `honkd: carry turn metadata on TurnCore replies`.

### Task B2: The hook sender

**Files:**
- Create: `crates/honkd/src/turn_hook.rs` (add `pub mod turn_hook;` in `lib.rs`)
- Modify: `crates/honkd/Cargo.toml` (`reqwest.workspace = true` under `[dependencies]`)
- Modify: `crates/honkd/src/config.rs` (flags), `crates/honkd/src/main.rs` (token file + construction), `crates/honkd/src/hub.rs` (`Hub.turn_hook: Option<TurnHook>`, passed into `Hub::start`)
- Test: unit tests in `turn_hook.rs` (in-process axum receiver on 127.0.0.1:0), config tests in `config.rs`

API:
- `#[derive(Serialize, Clone, Debug, PartialEq)] pub struct TurnEvent { v: u8 /*1*/, r#type: &'static str, event_id, session_id, upstream_id, run_id, client_req_ids, prompt_text, at_ms: u64, #[serde(skip_serializing_if = Option::is_none)] for the ended-only fields stop_reason, honk_reason, final_text, final_text_truncated: Option<bool>, error }` with constructors `TurnEvent::started(sid, &TurnMeta, at_ms)` and `TurnEvent::ended(sid, &TurnMeta, stop_reason, final_text, error, at_ms)`; both apply the 256 KiB cap (`cap_utf8(s, max) -> (String, bool)`, char-boundary safe).
- `#[derive(Clone)] pub struct TurnHook { tx: mpsc::Sender<TurnEvent> }`; `TurnHook::spawn(url, token: Option<String>, opts: HookOpts) -> TurnHook`; `fn emit(&self, ev)` uses `try_send` and on `Full` logs `tracing::warn!` with the event id and drops (never awaits, never blocks the actor).
- `HookOpts { queue: usize /*256*/, request_timeout: Duration /*10s*/, retry_window: Duration /*20 min*/, backoff_initial: Duration /*1s*/, backoff_max: Duration /*60s*/ }`.
- Sender task: one event at a time, in order. 2xx → next. 4xx → `warn!` and drop. 5xx / connect error / timeout → sleep backoff (doubling, capped) and retry until `retry_window` elapsed since the first attempt, then `warn!` and drop. Never log the token.
- Flags (`config.rs`): `--turn-hook-url <URL>` (`Option<String>`), `--turn-hook-token-file <PATH>` (`Option<PathBuf>`), hidden `--turn-hook-retry-secs` (default 1200) for tests. In `main.rs`: token file read like `--upstream-secret-file` (trim; unreadable or empty → `eprintln!` and exit 1); token file without URL → exit 1 with a message; URL that does not parse as http(s) → exit 1; host not loopback (`127.0.0.0/8`, `::1`, `localhost`) → `tracing::warn!` once. Log at startup `turn hook enabled url=<url>` (no token).

Tests: delivers in order with bearer header; 4xx dropped then next delivered; 503 twice then 200 → delivered once after retries (short backoff); unreachable then reachable within window → delivered; window exceeded → dropped and next delivered; `emit` on a full queue returns immediately; cap function on multibyte boundary; config parse of the flags.

Commit: `honkd: add an optional turn hook that reports turn starts and ends`.

### Task B3: Emit from the session actor

**Files:**
- Modify: `crates/honkd/src/session.rs` (`SynthUser` arm ~1944, `Reply` arm ~1985, `SessionMsg::ReplyText` ~104 and ~2537, `handle_reply_text` ~2928)
- Create: `crates/honkd/tests/turn_hook.rs`, `crates/honkd/tests/common/hook_stub.rs` (or inside `common/mod.rs`): a tiny axum receiver recording `(auth, body)` and answering a configurable status
- Modify: `crates/honkd/tests/common/mod.rs` if a helper is needed

Changes:
- `turn.started`: in the `SynthUser` arm, when `upstream_id` is `Some` and differs from `self.last_started_upstream` (new `Option<String>` field on `Session`, initialised in all three constructors in `hub.rs`), set it and `emit(TurnEvent::started(...))` from the in-flight `TurnMeta`.
- `turn.ended`: thread `turn: Option<TurnMeta>` through the `Reply` arm into `SessionMsg::ReplyText { owners, body, text, turn }` and into `handle_reply_text(owners, body, text, turn)`. In `handle_reply_text`, after the owner loop, if `turn` is `Some` and a hook is configured, emit `TurnEvent::ended` with `stop_reason` from `result.stopReason` (Result branch) or `None` (Error branch), `final_text` = the exported `text`, `error` = `err.message` (Error branch). Emit exactly once per call (not per owner).
- No emission when `--turn-hook-url` is absent (and no extra work: build events only when the hook exists).

Integration tests (`FakeGoose` so they always run; `StubGoose` variant gated on `HONK_GOOSE_BIN` for the normal turn):
1. Normal prompt with crid `c1` → receiver gets `turn.started` then `turn.ended` with the same `upstream_id`, `client_req_ids == ["c1"]`, `prompt_text`, `stop_reason "end_turn"`, `final_text` equal to the receipt's `_meta.honk.text`; bearer header present.
2. Two viewers on one session, one prompt → exactly one `turn.started` and one `turn.ended`.
3. Cancel an announced turn → `turn.ended` with `stop_reason "cancelled"`.
4. Goose restart mid-turn (`FakeGoose::restart`) → `turn.ended` with `honk_reason "upstream_restart"`.
5. Hook receiver down for the whole test → prompts still answered normally and promptly.
6. No `--turn-hook-url` → receiver sees nothing (spawn without the flag, receiver still running).

Commit: `honkd: report every turn to the turn hook`.

### Task B4: Docs

**Files:**
- Create: `docs/turn-hook.md` (the wire contract above + flags + delivery semantics + accepted gaps)
- Modify: `docs/chat.md` (~198-219, "Telegram via pigeon": turns typed in honk chat are mirrored when honkd runs with `--turn-hook-url` pointing at pigeon's `/goose/turn-event`; drop the "not mirrored" sentence)
- Modify: `crates/honk/src/pigeon.rs` (~10-11 module docs, ~118 the `Registered::summary` notice text) — say replies and prompts from honk chat appear in the topic when honkd's turn hook is enabled
- Modify: `docs/plans/HANDOFF.md` (one status line)

Redact check on every changed doc. Commit: `docs: describe the honkd turn hook`.

---

# Part C — deploy (after both PRs merge, with Jonathan's OK)

1. pigeon: deploy per pigeon's AGENTS.md; verify `POST /goose/turn-event` with an unknown session id returns 200 ignored (read-only in effect).
2. workstation PR: bump the `honk` flake input; add to the honkd unit's `ExecStart` in `users/dev/home.cloudbox.nix`: `--turn-hook-url http://127.0.0.1:4731/goose/turn-event --turn-hook-token-file /run/secrets/pigeon_daemon_auth_token`. The token is the pigeon daemon token (honkd → pigeon). Loopback only.
3. After merge: `home-manager switch`; honkd is `X-SwitchMethod=keep-old`, so restart it explicitly in a quiet window (no turns in flight), then verify the startup log line and run one prompt in a registered session.

## Ground rules (both repos)

- Both repos are public: no private org/repo names, no `/home/<user>` paths in committed files. `grep -rniFf ~/.config/honk/redact.txt <changed files>` must print nothing; never print the needle file.
- Bare `git commit`, explicit paths staged, no destructive git, no push (the orchestrator pushes).
- Never touch goose-serve (:4080), the deployed honkd (:4280), or the live pigeon daemon (:4731).
- Delete build output (`target/`, `node_modules/.vite`, coverage) you created when done if it is large.
