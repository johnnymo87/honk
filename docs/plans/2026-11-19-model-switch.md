# In-session model switch (`/model` in honk chat) — design

Bead: `honk-9qf.15.7` (P1: blocks the author's migration off opencode). Follow-ups:
`honk-9qf.15.23` (Telegram `/model` via pigeon), `honk-9qf.15.24` (OpenAI/codex-lb models
as a provider; deferred).

**Goal.** In honk chat the author always sees the goose session's current model and can
switch it, the way opencode lets him: a filterable picker (`/model`, `ctrl+x m`), a direct
`/model <id>`, `F2`/`shift+F2` to cycle recent models, and `--model` to start a session on
a model (`honk chat --new`, `honk launch`). UX approved by the author 2026-11-19.

## Facts (verified 2026-11-19; deployed goose `patched-1.54.0.14`)

- goose has **no `session/set_model`**. `session/new|load|fork` results carry
  `configOptions[]`, each a select `{id, name, category?, type:"select", currentValue,
  options:[{value,name}]}`; ids `provider`, `mode`, `model`, `thinking_effort`
  (`goose/src/acp/server/response_builder.rs:131-331`).
- Switch = `session/set_config_option {sessionId, configId:"model", value:"<bare id>"}` →
  result `{configOptions}`; goose first emits `session/update` `{sessionUpdate:
  "config_option_update", configOptions}` (`dispatch.rs:153-216`, `server.rs:2491-2569`).
  `configOptions` may be absent when the session has no known provider.
- The model switch stays **within the session's provider**; `configId:"provider"` is a
  separate call that resets the model to that provider's default. Out of scope here.
- goose **does not validate** the id against `options`; any id is accepted and a bad one
  fails only at the next model call.
- The switch is **persisted** in goose's session DB; `session/load` returns it.
- goose's option list for `gcp_vertex_ai` is a stale static list (Claude 4.x, Gemini
  2.5/3.x) plus the current model prepended. On cloudbox `claude-fable-5-2`,
  `claude-haiku-5-5`, `gemini-3.8-flash` and `claude-opus-4-8` all answer through it
  (`GOOSE_PROVIDER=gcp_vertex_ai GOOSE_MODEL=<id> goose run --no-session`). So honk needs
  its own list of the author's models.
- honkd already forwards `session/set_config_option` raw (`viewer.rs` Forward;
  `session.rs` Forward handler) with **no busy check**, and fans the `config_option_update`
  frame out to every viewer and into the log. But the actor's `init_result` (what a later
  attach's `session/load` returns, also persisted as `HeldSession.init_result`) is never
  refreshed, and a new log generation (reactivation, rebuild) drops earlier live frames.
- honk ignores `configOptions` today; the status bar has no model
  (`docs/plans/2026-10-18-context-usage-plan.md:69` deferred it for exactly this reason).
- opencode: `/models` / `ctrl+x m` picker (Favorites, Recent, then per provider; fuzzy
  filter; current highlighted), `F2`/`shift+F2` cycle recent, model under the prompt and in
  each assistant footer; recent list in `~/.local/state/opencode/model.json`.

## Design

### honkd

1. **`--models-file <path>`** (optional). JSON:
   `{"providers": {"gcp_vertex_ai": [{"id": "claude-opus-5-5", "name": "Claude Opus 5.5"}, …]}}`.
   Read on every list request (like the MCP catalog), so an edit needs no restart. A
   missing or unparsable file is a warning in the list reply, never a failure. Workstation
   Nix writes it (seeded from opencode's Vertex Claude and Gemini models, `@default`
   stripped) and adds the flag to the honkd unit.
2. **Keep `init_result.configOptions` fresh.** In `on_upstream`, a session-stream
   `session/update` with `sessionUpdate == "config_option_update"` replaces
   `init_result["configOptions"]` in memory and in the persisted `HeldSession`. Then every
   later attach (honk, pigeon) learns the current model from its `session/load` reply.
3. **`_honk/model/list {sessionId}`** (read-only; get-or-activate + auto-attach, like
   `_honk/mcp/*`, but not gated on the MCP catalog). Reply:
   `{provider, current, models: [{id, name, source: "configured"|"goose"}], warning?}`.
   Configured models for the session's provider come first in file order, then goose's
   options not already listed. `provider`/`current` come from `init_result.configOptions`
   (null when absent).
4. **Busy guard.** Forward refuses `session/set_config_option` with -32002 "session is
   busy; try after this turn" while a turn runs (`turn.is_running()`), as `_honk/mcp/*`
   mutations do. Other forwarded methods are unchanged.

The switch itself stays a raw forwarded `session/set_config_option`: no new mutation
method, and goose's own `config_option_update` already reaches every viewer.

### honk chat

- **State.** `ChatCore` tracks the current model (and provider) from `configOptions` in the
  `session/new` / `session/load` result and from live `config_option_update` frames. When
  it changes outside replay it adds a notice `model: A → B`. The driver's `StateSnapshot`
  carries it to the TUI.
- **Status bar.** A model segment (the bare id, e.g. `claude-opus-5-5`), dropped before the
  title is squeezed below its minimum, in the existing width-fallback scheme.
- **`/model <id>`** (also `:model <id>`): sends `session/set_config_option`. While a turn
  runs (or the local queue is non-empty) the line is queued with a `queued:` notice and
  flushed at turn end, exactly like `/mcp enable`; a -32002 reply re-queues (max 3). An id
  outside the known list is still sent, with a warning that goose does not check it.
- **`/model`** (no argument), **`ctrl+x m`** (both vim modes): request `_honk/model/list`,
  then open a **picker overlay** above the composer: a filter line (case-insensitive
  subsequence match on id and name), sections *Recent*, *Configured*, *goose* (Recent and
  section headers hidden while filtering), current model marked `●`. `j/k`/arrows move,
  Enter switches (through the `/model <id>` path), Esc closes. `ctrl+x` becomes a prefix
  key; `ctrl+x` followed by anything else is ignored.
- **`F2` / `shift+F2`**: switch to the next/previous model in the recent list (relative to
  the current one; no-op with a notice if the recent list has fewer than two models).
- **Recent list**: client file `$XDG_STATE_HOME/honk/model-recent.json` (fallback
  `~/.local/state/honk/`), at most 10 ids, most recent first, updated when a switch
  succeeds. Shared by all honk chats on the host.
- **`honk chat --new --model <id>`**: after `session/new` succeeds and before any prompt,
  send `session/set_config_option`; a failure is an error notice, the session stays open.
- **`honk launch --model <id>`**: same call as a `step!` after `rename`, before the
  prompt. A failure aborts the launch before prompting (the session exists but has not
  started work on the wrong model) and prints the session id.

### Not in scope

Provider switching and GPT models (`honk-9qf.15.24`); Telegram `/model`
(`honk-9qf.15.23`); per-message model footers; favorites; `thinking_effort`.

## Testing

- honkd unit/integration (FakeGoose): list merges configured + goose options and reports
  `current`; missing/garbled models file → warning; `config_option_update` refreshes the
  `session/load` reply for a second viewer and the persisted state; set during a running
  turn → -32002; set when idle forwards and both viewers see the update.
- honk core tests: model parsed from new/load result and from `config_option_update`;
  notice on change, none during replay; `/model x` queues during a turn and flushes at end;
  -32002 re-queue.
- TUI tests: picker filtering/sections/selection, `ctrl+x m`, `F2` cycling, status-bar
  fallback widths; recent-file round trip.
- launch: `--model` sends the call before the prompt; failure aborts.
- Live (StubGoose, `HONK_GOOSE_BIN`): a real `set_config_option` round-trip through honkd.

## Rollout

Merge (author's OK) → workstation PR: models file + `--models-file` flag in the honkd unit
(cloudbox and devbox) → home-manager switch + honkd restart (author's OK).

## Tasks (subagent-driven; one commit per task, TDD)

Build/test: `nix develop -c cargo test -p <crate> --test <file>`; live tests need
`HONK_GOOSE_BIN=$HOME/.local/state/honk-goose-bin/patched-1.54.0.3/pkg/bin/goose`. Before
the PR: `cargo fmt --all --check`, `cargo clippy --workspace --all-targets -- -D warnings`,
`cargo test --workspace`.

- **T1 honkd: fresh `init_result` + busy guard.** `session.rs` `on_upstream` session-stream
  block: on `config_option_update` replace `self.init_result["configOptions"]` and the
  persisted `HeldSession.init_result` (same `state_store.mutate` shape as
  `last_conn_event_id`). Forward: `session/set_config_option` while `self.turn.is_running()`
  → -32002 "session is busy; try after this turn". Tests (`honkd/tests/model.rs`,
  FakeGoose): viewer B attaching after A's switch sees the new `currentValue` in its
  `session/load` reply; state file has it; set during a running turn → -32002; idle set is
  forwarded and both viewers get the update frame.
- **T2 honkd: `--models-file` + `_honk/model/list`.** `config.rs` flag;
  `honkd/src/models.rs` pure `load(path) -> Result<ModelsFile>` and
  `merge(configured, init_result) -> ListReply` (unit tests: order, dedupe, missing
  provider, absent `configOptions`, garbled file → warning). `viewer.rs` branch before the
  generic Forward (get-or-activate + auto-attach as `_honk/mcp/*`, not gated on
  `hub.mcp`); `SessionMsg::ModelList` answered from `init_result`. e2e in `model.rs`.
- **T3 honk core: model state, notices, status bar.** Parse `configOptions` (`model`,
  `provider` selects) from the `session/new` result, `Attached` result and
  `config_option_update`; `ChatCore` field + `StateSnapshot.model`; notice
  `model: A → B` on change when not replaying; `status.rs` segment with fallback.
  Tests: `chat_core.rs`, status width tests.
- **T4 honk: `/model <id>`, queueing, recent file.** `cmd.rs` `/model <id>` and `:model`;
  `PendingModelReq`; queue/flush/-32002 re-queue like `/mcp` (`check_queued_prompts`
  learns the `/model ` prefix); warning for ids not in the last known list; on success
  update `honk/src/chat/recent.rs` (`model-recent.json`, max 10, MRU). Tests:
  `chat_core.rs` + recent-file unit tests (tempdir via `XDG_STATE_HOME`).
- **T5 honk TUI: picker, `ctrl+x m`, `F2`.** `/model` with no arg → `_honk/model/list` →
  `UiEvent::ModelList` → `tui/model_picker.rs` overlay (filter, sections, `●` current,
  j/k/arrows, Enter → `UiCommand::Line("/model <id>")`, Esc). `keys.rs`: `ctrl+x` prefix
  (both modes) → `m` opens picker; `F2`/`shift+F2` cycle recent. Tests: picker unit tests
  (filter, sections, selection), key resolution tests.
- **T6 `--model` on `honk chat --new` and `honk launch`.** `main.rs` flags; driver sends
  `session/set_config_option` after `session/new` before building `ChatCore` (error →
  notice); `launch.rs` `step!` after `rename`, failure aborts before prompt. Tests:
  `launch.rs` (order of calls, abort), `chat_driver.rs`.
- **T7 docs + live.** `docs/chat.md` (`/model`, keys, `--model`), README/flags if listed;
  live StubGoose test: real `set_config_option` through honkd, `_honk/model/list` reports
  the new current.

## Workstation (after merge, author's OK)

`--models-file` written by Nix (Vertex Claude + Gemini ids from opencode's config,
`@default` stripped) on cloudbox and devbox; bump the honk flake input; home-manager
switch + `systemctl --user restart honkd`.
