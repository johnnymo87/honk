# Driving a timer-driven goose coordinator through honkd

Status: design, 2026-10-13, revised after adversarial review. Verified facts are
marked **[V]** (read in goose `patched-1.54.0.3` source, or measured on the
patched sandbox through a throwaway honkd); inferences are marked **[I]**.

## 1. Problem

The author runs a set of **timer-driven goose coordinators**. A private Python
runner wakes every 15 minutes. For each of about 20 programs of work it sends a
JSON payload to that program's standing goose session and parses a JSON answer.
Today each wake runs the goose CLI as a subprocess:

```
goose run --recipe <recipe.yaml> --params payload=<json>
          (--name <n> | --session-id <id> --resume)
          --output-format json --max-turns 4
env: GOOSE_MODE=auto GOOSE_MAX_TOKENS=16000 GOOSE_HEADLESS_CACHE_TTL=1h
cwd: a per-session working dir (also used to attribute spend)
```

The CLI writes goose's `sessions.db` directly, so honkd never sees these turns.
`honk list` shows only the most recent page of sessions (the CLI reads page 1),
but nobody can watch a wake live or talk to a coordinator from honk. The goal is
that every wake goes through honkd, so the author can watch, and then chat with,
a coordinator from a honk client while the timer keeps running.

Non-goals: the chat client itself (separate work), and changing what the
coordinator decides.

## 2. What the CLI gives today, and the ACP equivalent

| CLI behaviour | Through honkd | Status |
|---|---|---|
| Recipe `instructions` in the system prompt | `session/new` with `_meta.recipeDeeplink` (from `_goose/unstable/recipes/encode`) applies them. `session/load`, which is what honkd activation does, re-applies the stored snapshot | [V] spike |
| System-prompt bytes | Extras are rendered by value in insertion order. Both paths give final_output then instructions, and key names are not rendered, so adoption need not cost a cold write | [V] review, `prompt_manager.rs:147-177` |
| Recipe edits reach the next wake (the CLI rewrites the stored recipe every run) | **Lost.** Under ACP only `session/new` writes `session.recipe`, so instruction, schema and settings edits never reach a standing session, and every re-activation re-applies the stale snapshot | [V] `new_session.rs:235`; fix in §4 |
| `response.json_schema` → `final_output` tool, plus the continuation loop that forces it | Same code. It is registered at `session/new` and on every activation | [V] spike |
| `extensions: []` → no tools | With a recipe, goose adds the `developer` builtin anyway. Fix: the ACP recipe copy has no `extensions` key, and `session/new` sends `_meta.enabledExtensions: []`. Fallback: `extensions/remove developer` persists `extension_data = []`. Re-activation loads only what the row stores | [V] remove path; [I] the `enabledExtensions` path is source-only, verify |
| `GOOSE_MODE=auto` | The serve's mode, stored per session. No tools means no permission requests | [V] |
| `GOOSE_MAX_TOKENS=16000` | Adopted sessions keep their stored 16000. New sessions get the serve's value (unset → model default, 64000 measured). It is a ceiling, so this is safe | [V] |
| `--max-turns 4` | ACP passes `max_turns: None` → the serve-global `GOOSE_MAX_TURNS` (default 1000). The runaway case is the continuation loop, where the model never calls `final_output`, so counting `final_output` calls does not bound it | [V]; fix in §4 |
| Subprocess `timeout=600` | Client deadline, then `session/cancel` through honkd | new |
| **`GOOSE_HEADLESS_CACHE_TTL=1h`** | **No equivalent.** On every restore the serve strips the stored `cache_ttl`, re-derives it from global `GOOSE_CACHE_TTL` (unset → 5-minute cache), and writes the stripped config back to the row | [V] `model_config.rs`, `agent.rs:3899, ~3783`; §4 |
| Envelope `metadata` (lifetime tokens, `cost_usd`) and context size | The session row's `accumulated_*` columns and `total_tokens`, read read-only (the spend tool already reads this DB). The prompt reply's `usage.totalTokens` is the context size | [V] |
| Final JSON | The prompt reply's `_meta.honk.text`. goose repeats the `final_output` JSON as the closing assistant text. honkd bounds the text at the next non-steer prompt. Prose before the tool call is concatenated in, so the parse keeps today's fallback | [V] spike; honk-wire `export.rs:270-285` |
| Spend attribution by working dir | `session/new cwd` is stored as `working_dir`. The spend tool reads by `working_dir` and does not filter on session type | [V] |
| One run per session | goose's in-process run registry. honkd surfaces it as busy `-32602` | [V] |

The payload construction, delta payloads on resume, the inbound-message mailbox
(keyed by program, not by goose session) and spend tagging do not depend on the
transport and do not change.

## 3. Design

### 3.1 Runner (private repo)

A honk transport sits beside the CLI one. Each wake picks one from a config
constant, with a file override for instant rollback. `run_goose` keeps its
return contract `(output_dict, metadata)`. The client is a small synchronous
WebSocket JSON-RPC client to honkd, using honkd's downstream secret and a
`max_size` of at least 64 MiB.

Per wake:

1. `initialize` with `_meta.honk = {}`.
2. **New session** (first run or rotation):
   - Build the ACP recipe copy: same `instructions`, `response` and `settings`; no `prompt`, `parameters` or `extensions`; `title` set to the session name.
   - `recipes/encode`, then `session/new {cwd, mcpServers: [], _meta: {recipeDeeplink, enabledExtensions: []}}`.
3. **Every wake: re-pin the session's settings.** Call `session/settings/set` (the goose patch, §4) with the current recipe and `cacheTtl: "1h"`. It is idempotent and self-healing. It also activates a cold session.
4. **Every wake: guard tools before prompting.** `tools/list` must be exactly `[recipe__final_output]`, and the row's `extension_data` must be empty. Otherwise remove the extras and re-check; if that fails, fail the wake (fail closed).
5. **Prompt.** Send `session/prompt {prompt: [text payload], _meta: {honk: {client_req_id}, goose: {maxTurns: 4}}}` and enforce the deadline. On a socket drop, reconnect and resend the same id with `resume_only`.
6. **Parse.** Take `output_dict` from `_meta.honk.text`. If that is not the JSON object, fall back to the DB conversation slice from the payload up to the next non-steer user message. Scan the truncation and tool errors on that slice, as today.
7. **Build `metadata`** from the row's `accumulated_*` in the CLI's field names, so per-session base/delta accounting is unchanged. Alert if the row's `model_config.request_params.cache_ttl` is not `1h`.
8. `session/close` (detaches only).

The runner does **not** call `session/export` through honkd. A forwarded export
runs on the held connection, and a multi-MB reply would sit in that
connection's replay ring (honkd rule I1).

**Failure classes**, which decide whether the runner rotates:

| Outcome | Examples | Effect |
|---|---|---|
| `CoordinatorBusy` (exists) | Lock busy; honkd busy `-32602` (a human turn is running) | Skip; retried next tick; not a failure |
| Activation "Session not found" | Activation `-32602 "Session not found: <sid>"` (same code as busy; client must match on message) | If session exists in goose DB, treat as `TransportUnavailable` and alert, not rotate |
| **`TransportUnavailable`** (new) | honkd down or refusing; `held-session limit reached`; goose restarting (`-32004`, 404); activation or load timeout; deadline hit while honkd is unreachable | Skip; not counted toward rotation; separate alert after N consecutive |
| `CoordinatorError` (exists) | Output parse failure; schema-invalid output; recipe render error; tool guard failed | Counted; rotation after 2 as today |

Rotation discards a history the author keeps on purpose, so only deterministic
failures may lead to it.

### 3.2 Concurrency with a human in honk

- **A human message while a wake runs.** A steer is drained into the
  conversation at the top of the agent loop. The loop then sees the stored
  `final_output` and ends the turn without another model call. A steer during the
  wake's single model call is therefore persisted but never answered, and is not
  reported as unconsumed ([V] review, `agent.rs:2761-2810`). So the chat client
  **must not steer into a run it did not start**. It holds the text and prompts
  once `activeRunId` is null. (Agreed with the chat-client work.)
- **The race this creates.** A human prompt can now start right after the
  wake's reply, before the runner parses. The runner never parses "the last
  assistant message" from the DB. It uses `_meta.honk.text`, which is bounded to
  its own turn, and the bounded fallback slice.
- **A wake while a human turn runs.** The runner gets busy, raises
  `CoordinatorBusy`, and retries next tick.
- **Human turns are not applied.** A human-started turn also ends in
  `final_output`, because the tool is session-wide. The runner never applies
  those outputs. The recipe gains one sentence: a turn that starts with a message
  from the author rather than a runner payload is conversation; answer in
  `notes`, and make state changes on the next runner payload. It reaches standing
  sessions through the per-wake recipe re-pin.
- **Usage deltas** per wake include any chat turns since the last wake. That is
  acceptable: it is still the coordinator's spend.
- **Held-session cap.** honkd holds at most `--max-sessions` sessions (default
  64 since the chat-client release; it was 16 in M1a; no LRU yet). Each wake
  holds a session for the 15-minute idle TTL, and about 20 coordinators plus
  pigeon and chat share the cap, which the default covers. A goose session costs
  about 0.5–1.7 MB and 9–12 threads. Hitting the cap is `TransportUnavailable`,
  not a failure.

### 3.3 Migration: adopt, don't rotate

The ~20 standing sessions are adopted in place. The first honk wake re-pins and
prompts the existing id, and honkd activates it with `session/load`.

- **Stored CLI rows.** They have `extension_data = []`, `max_tokens 16000` and
  `cache_ttl 1h`; the TTL is dropped on restore, and the §4 patch keeps it. The
  recipe snapshot's `prompt` holds the last payload, and `session/load`
  re-renders the snapshot as a template, so a `{{`, `{%` or `{#` would fail the
  load. All 20 are clean today [V]. The check runs before every roll-forward,
  because CLI wakes rewrite the snapshot. A render failure is a deterministic
  error; it is reported, never rotated silently.
- **Two writers.** goose's run guard is in-process only. CLI then serve on one
  session is safe, because the serve re-reads the conversation on every reply.
  Running them concurrently is not safe. The switch is read once per wake and the
  per-session `flock` serializes wakes, so this cannot happen at cutover. It can
  happen on **rollback**, when a human chats through honk while a CLI wake runs.
  Rollback guidance: don't chat while rolled back.
- **Runner killed mid-wake.** goose keeps running the turn. The next wake gets
  busy, or finds a completed turn that was never applied. Delta payloads advance
  only when a wake's output is applied, so the next payload re-sends what was
  missed.
- **Dry run before switching.** Run a throwaway patched `goose serve` on a copy
  of the sessions DB with a stub provider, behind a throwaway honkd. Activate
  every adopted session and re-pin it. Check the tools, the system prompt, the
  persisted TTL, and **time** the activation lookup and load against honkd's 30 s
  load timeout, using real ~400K histories (the load replay is the full history).
  No model calls; no production process touched.
- **Environment parity.** Diff the serve's `GOOSE_*` environment and config
  against the runner's: compaction threshold, context limit, effort.

### 3.4 Rollback

Create the override file. The next wake uses the CLI on the same session ids.
The CLI rewrites the stored recipe and re-applies its settings on resume, so
nothing needs undoing.

## 4. goose patch: per-session settings that survive restore

The serve cannot hold a per-session cache TTL, turn cap or updated recipe, and
those are exactly what the CLI re-applies on every run. Each coordinator wakes
every 15+ minutes into up to ~400K tokens. With a 5-minute cache every wake
rewrites the whole context, a several-fold increase in input cost.

The patch (fork, release `patched-1.54.0.4`) has three parts:

1. **Restore keeps a stored `cache_ttl`.** `with_rederived_cache_ttl` keeps the
   session's stored value unless `GOOSE_CACHE_TTL` is set globally. Adopted rows
   already store `1h`.
2. **`_goose/unstable/session/settings/set {sessionId, cacheTtl?, recipe?}`.**
   It updates the live agent (provider model config; recipe instructions and
   `final_output`) and persists both to the row. Idempotent. It is sent every
   wake, so it heals any path that strips the TTL (a model change from a chat
   client, provider fallback, a restore by an older serve) and propagates recipe
   edits.
3. **`session/prompt _meta.goose.maxTurns`.** It bounds the agent loop for that
   run. honkd forwards `_meta` other than `_meta.honk`.

Alternatives considered:

- **A. Global `GOOSE_CACHE_TTL=1h` on the production serve.** Two lines of host
  config. Every interactive goose session also gets 1-hour cache writes (2× base
  input instead of 1.25×) and becomes eligible for keep-warm. It does not fix
  recipe drift or the turn cap. Usable as a stopgap if speed matters more.
- **C. A second serve for coordinators.** Two honk endpoints, and two writers on
  one DB, or a separate DB that loses the adopted histories.

## 5. Plan

1. **goose:** the §4 patch, with unit tests and a sandbox test, then release
   `patched-1.54.0.4`. The author deploys it; a goose-serve restart is
   author-gated.
2. **Runner:** the honk transport behind the switch, default off. Unit tests
   against a fake honkd. Live test against a throwaway honkd on the patched
   sandbox with a cheap model.
3. **Adoption dry run** (§3.3).
4. **Production:** deploy goose and the honkd with the lookup fix (restarts
   author-gated), then the switch. Watch two ticks: `honk tail` on one
   coordinator, cache reads and cost in the spend report.
5. **Chat:** the chat client's queue-when-not-mine rule must be in place before
   the author chats into coordinator sessions. Until then, watch only.

Two honkd changes support this:
- Activation looks sessions up by id (`_goose/unstable/session/info`) instead of the first `session/list` page, allowing older sessions to activate directly.
- `session/list`'s `_meta.honk.running` now reports in-flight and active runs, advisory only — a check-then-act race remains and is harmless because goose's `settings/set` is idempotent and keeps a collected final output.

## 6. Open questions

- **Free-form chat turns.** Drop `final_output` and validate JSON client-side so
  the author gets prose? This changes the coordinator's output guarantees.
  Deferred until chat is in use.
- **Applying human-turn output.** Should the runner apply a human-started
  turn's output? Deferred.
