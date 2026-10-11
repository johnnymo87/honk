# Spike Report: Real Goose against a Stub Model Provider (Task A1)

## Verdict

**Decision rule met:** All six checks pass against real patched goose (`patched-1.54.0.3`).
**Verdict:** `StubGoose` (real patched goose child + in-test `StubModel` OpenAI provider) is confirmed as the **main test upstream** for proxy and relay tests.

## Summary of Results

| Check | Description | Result | Details |
|---|---|---|---|
| 1 | Text turn streaming | PASS | Streams `agent_message_chunk`s matching scripted chunks and finishes turn cleanly |
| 2 | Tool turn (`shell` / `echo hi` in auto mode) | PASS | Emits `tool_call` then `tool_call_update completed`, runs second completion text turn; `export()` contains both tool call and assistant text |
| 3 | Approve mode | PASS | Emits `session/request_permission`; answering `allow_once` executes the tool and finishes the turn |
| 4 | Gated completion, busy prompt, steer, cancel | PASS | Gate holds second completion; concurrent prompt receives busy error `-32602` (`already has active run`); steer with `expectedRunId` is accepted; `session/cancel` aborts run with `stopReason: "cancelled"` |
| 5 | Takeover and replay | PASS | `GET /acp` advertises `Acp-Stream-Features: takeover, replay`; resume with `takeover: true` and `resume_after` yields `ReplayOutcome::Complete` with all subsequent frames replayed in sequence |
| 6 | Process kill and respawn | PASS | Child kill drops connection; after `restart()`, old connection returns 404 / fails, and `export()` on respawned goose reads persisted session from SQLite |

## Auxiliary Model Calls & Classification Log

`StubModel` routes completions by request shape rather than strict FIFO queue order:
- **Turn requests:** Request body carries a non-empty `tools` array and its last message role is `"user"` or `"tool"`. These requests pop the next scripted `Turn` (or return `Text(["ok"])` if queue is empty).
- **Auxiliary requests:** Requests without tools or with non-user/tool final message (e.g. session naming summarization, compaction). These receive `Text(["ok"])` without popping scripted turns.

**Observed calls during spike:**
With `GOOSE_DISABLE_SESSION_NAMING=true`, goose skips background session naming completions entirely. All model requests made during the test suite were genuine turn requests (user prompts and tool output completions). When session naming or compaction is triggered, the classifier handles them as auxiliary without desynchronizing scripted turns.

## Exact Tool Names Seen

When goose configured with the builtin `developer` extension connects to the model provider, it passes the following function names in the `tools` array:

- **Developer extension tools:**
  - `shell` (invokes bash/sh command)
  - `write` (writes file contents)
  - `edit` (edits file contents)
  - `tree` (lists directory tree)
  - `analyze` (token / codebase analysis)
  - `load` (loads file / context)
  - `read_image` (reads and renders image)
- **Builtin runtime tools:**
  - `apps__create_app`, `apps__delete_app`, `apps__iterate_app`, `apps__list_apps`
  - `delegate`
  - `extensionmanager__list_resources`, `extensionmanager__manage_extensions`, `extensionmanager__read_resource`, `extensionmanager__search_available_extensions`
  - `load_skill`

Tool call matching in `StubModel` matches exact tool names as well as `__<tool>` suffix (e.g. matching `shell` to either `shell` or `developer__shell`), ensuring portability across configurations.

## Gap Notes & Wire Observations

1. **Steer requires `expectedRunId`:** When invoking `_goose/unstable/session/steer`, goose requires `expectedRunId` matching `session_info_update._meta.goose.activeRunId`. Calling steer without `expectedRunId` returns error `-32602` ("missing field `expectedRunId`").
2. **Replay ring is connection-scoped:** Replay ring buffers are maintained per connection mailbox. To replay missed frames using `Last-Event-ID` (`resume_after`), the subscriber must resume the same connection (`Upstream::resume`) rather than initializing a fresh connection.
3. **No fake fallback required for primary tests:** Because all 6 checks succeeded on real goose with the stub model, the primary test harness does not need to emulate core session semantics via `FakeGoose`. `FakeGoose` will be used in Task A2 only for synthetic fault injection (unpatched-style permanent 409, missing ring/gap emulation, and CI environments without a goose binary).
