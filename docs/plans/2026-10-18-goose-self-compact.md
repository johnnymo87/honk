# goose self-compaction (`self_compact_and_resume`) — design and plan

Bead: `honk-9qf.16` (children `.1` goose fork, `.2` honk, `.3` workstation).

**Goal.** A goose agent can compact its own context and continue with a resume
prompt it wrote, the way opencode agents use `self_compact_and_resume`. It must
work end to end through honk chat (goose-serve :4080 → honkd :4280 → honk), and
must not break pigeon or the stall-watch coordinator.

## Facts (verified 2026-10-18 against fork `patched/v1.54.0` @ `3eb4f657b`)

- Production `goose serve` runs the **legacy** agent loop (`Agent::reply_internal`,
  `crates/goose/src/agents/agent.rs`); `GOOSE_STATE_MACHINE` is unset and honkd never
  sends `_meta.goose.unrolledAgentLoop`.
- goose compacts at turn start past `GOOSE_AUTO_COMPACT_THRESHOLD` (0.8), reactively on
  `ContextLengthExceeded`, and on a user-typed `/compact` (`execute_commands.rs`). The
  agent cannot trigger compaction, and `/compact` never starts an LLM turn.
- `compact_messages` (`context_mgmt/mod.rs`) keeps every original row (now agent-
  invisible, still user-visible) and appends an agent-only summary + continuation.
  `replace_conversation` rewrites the session's rows in one transaction.
- ACP drops `AgentEvent::HistoryReplaced`; status notices reach only clients that opted
  into goose custom notifications (honkd does not).
- A second `session/prompt` during an active run is rejected ("use steer").
- Platform-extension tools get no handle on the running `Agent`; sessions persist their
  enabled-extension list, so a new platform extension never reaches existing sessions.
- honkd counts client prompts via `honk_wire::export::client_prompt_entries`: user-visible,
  **non-steer** user messages. Receipts (`_meta.honk.text`) and the coordinator's parse
  run to the next non-steer user message.
- The coordinator's guard requires `tools/list == [recipe__final_output]` and empty
  `extension_data` (docs/plans/2026-10-13-coordinator-on-honk.md).
- honk chat answers `/compact` with "unknown command" (`crates/honk/src/chat/cmd.rs`).

## Design (decided; oracle-reviewed)

**Compaction happens inside the same run.** The tool only arms; the legacy loop compacts
at the top of its next iteration and continues with the resume prompt as the next user
message. One `session/prompt`, one `activeRunId`: honkd sees an ordinary long turn, so
honkd, pigeon and honk-wire need no changes.

Rejected: a goose-initiated second run (a client prompt can land in the gap and be
refused; honk chat would mark the run Foreign; receipts stop early) and a honkd-side
mechanism (two runs with gaps, `/compact` counted as a client prompt, only works through
honkd).

1. **Agent-owned tool**, built like `final_output` (not a platform extension):
   `self_compact_and_resume(prompt: string)`.
   - Listed (in `prepare_tools_and_prompt` after `prepare_inference_tools`, and in
     `Agent::list_tools`) only when **all** hold: config `GOOSE_SELF_COMPACT` is not false
     (**default on** in the fork), no `final_output_tool` (excludes recipe/coordinator
     sessions, keeping the coordinator guard exact), no recipe `retry_config`, not a
     subagent, and `!provider.manages_own_context()`. The state-machine loop never lists
     it.
   - Dispatch intercepted by name (like `FINAL_OUTPUT_TOOL_NAME`): validate non-empty and
     ≤ 16 000 chars; result text: "Compaction runs right after this step; `prompt` will be
     your next user message. Do not call other tools in this step." Stores no state.
   - Annotated `readOnlyHint: true` so smart_approve never asks for permission.
2. **Arming is loop-local.** When tool results for a step are assembled, a successful
   (non-error) call sets `pending_self_compact = Some(prompt)`; last call wins. Failed or
   denied calls never arm.
3. **Compaction point:** top of the next loop iteration, after tools/turn context are
   rebuilt and before the steer drain. The tool pair was persisted at the end of the
   previous iteration, so the summary includes it.
   - If `turns_taken >= max_turns`: do not compact; persist a user+agent-visible notice
     containing the resume text; let the loop end normally.
   - Else: yield a notice; reload the conversation from the DB; `compact_messages(...,
     CompactionKind::SelfRequested)` raced against the cancel token; build compacted +
     user-only note "Context self-compacted" + resume message (role user, user- and
     agent-visible, `metadata.steer = true`, `created` ≥ newest row); write once with
     `replace_conversation`; update session metrics (retained); yield `HistoryReplaced`
     and the resume message; reset loop state (`conversation`, recomputed compaction info
     for the turn context, `initial_messages`, `compaction_attempts = 0`); clear pending.
     **Do not reset `turns_taken`.**
   - Steers queued meanwhile drain after the resume message, verbatim.
4. **Failure.** Compaction error or cancel → DB unchanged; persist a notice ("Self-
   compaction failed: …; the resume prompt was not delivered:" + prompt) and end the run.
   After the loop, any still-pending request (provider error, cancel, refusal) produces the
   same notice. Never "queued" followed by silence.
5. **`compact_messages(manual: bool)` → `CompactionKind { Auto, Manual, SelfRequested }`**
   with a new continuation text: "You compacted your own context. The previous message is
   a summary of the conversation so far; the next user message is the resumption prompt
   you wrote. Act on it."
6. **Turn context.** `turn_start_compaction_info` must be recomputed after self-compaction
   (otherwise the agent keeps seeing the old "tokens remaining" and may loop), and
   `persist_turn_context_if_changed` must compare only against agent-visible turn
   contexts.
7. **honk chat `/compact`.** Sends `/compact` as a prompt when idle; when a run is active
   it goes to the local queue (never a steer: steers bypass goose's command handling).
8. **Skill.** `preparing-for-compaction` becomes harness-neutral (opencode and goose share
   `~/.agents/skills`; the tool name is the same in both): persist first, restate the
   original request in the resume prompt (self-requested compaction does not preserve
   it), call the tool alone in its own step, read the result.

**Known trade-offs.** A goose restart between arming and compacting loses the request
(the run dies anyway, honkd reports the interruption). There is no per-session off
switch (it could not live in `extension_data`); `GOOSE_SELF_COMPACT=false` turns it off
server-wide. Summarization runs for up to minutes with only a progress notice.

## Tasks

### G — goose fork (`johnnymo87/goose`, PR into `patched/v1.54.0`)

Worktree `~/projects/goose-patched/.worktrees/self-compact`, branch `self-compact`.
Fork rules (docs/plans/2026-10-11-goose-patches-plan.md): push only to `fork` with an
explicit refspec; never `cargo fmt --all` (use `cargo fmt -p goose`); commit style
`<area>: <sentence>`; modified upstream files carry the `// Modified by Jonathan
Mohrbacher, 2026: …` notice (extend the existing line where present). Long cargo runs go
through `systemd-run --user … --expand-environment=no` with a log.

- **G1** `context_mgmt`: `CompactionKind` enum + self-requested continuation text; update
  all call sites. Tests: kind selects the continuation text; `SelfRequested` does not
  preserve the last user prompt.
- **G2** tool: definition, availability predicate, listing in both places, dispatch
  interception and validation, `readOnlyHint`. Tests: listed/unlisted per predicate
  (config off, final_output, retry config, manages_own_context); validation errors;
  smart_approve yields no ActionRequired.
- **G3** loop: arming, compaction point, failure/cancel/max-turns/leftover notices,
  turn-context fixes. Tests (scripted provider, `tests/compaction.rs` style): happy path
  (provider calls: turn → summary → turn; last request holds only summary, continuation,
  resume-as-steer, turn context); reload order and metadata; `turns_taken` not reset; last
  call wins; failed call does not arm; summary error and cancel leave DB unchanged with a
  notice; max-turns notice; steer queued during compaction lands after the resume; goal
  survives; no stale "tokens remaining"; rolling memory on → next turn does not fold
  immediately.
- **G4** ACP test: within one run id the resume arrives as `user_message_chunk` with
  `_meta.goose.steer = true`, with no `activeRunId: null` in between.
- **G5** `patched-release.yml` test-patches filter gains `self_compact`; modification
  notices; PR. After merge: tag `patched-1.54.0.6` (annotated, "goose v1.54.0 + local
  patches (…)"), which also ships the merged-but-unreleased rolling memory (off unless
  configured).

### H — honk

- **H1** `/compact` in honk chat (cmd.rs, help, docs/chat.md), unit + chat-driver tests.
- **H2** honk-wire unit test: export fixture with hidden originals, agent-only summary,
  user-only note and a steer resume → `client_prompt_entries` counts only the originals and
  `turn_text_at_index` spans the whole run.
- **H3** honkd e2e (StubGoose/StubModel, needs a goose with the patch via
  `HONK_GOOSE_BIN`): `ToolCall("self_compact_and_resume", {prompt:"R"})` then
  `Text("after")` → one run, receipt includes "after", a second prompt's receipt still
  matches; honk chat keeps ownership Own. Recipe with a response schema lists exactly
  `[recipe__final_output]`. CI's pinned goose moves to `patched-1.54.0.6`.

### W — workstation

- **W1** skill `preparing-for-compaction`: harness-neutral, goose section.
- **W2** after the fork release: bump `pkgs/goose/default.nix` to `patched-1.54.0.6` and
  `flake.lock` honk. **Deploy is author-gated** (see below).

## Deploy and verify (author runs; restarting goose-serve interrupts live goose turns)

1. `cp ~/.local/share/goose/sessions/sessions.db ~/.local/share/goose/sessions/sessions.db.bak-$(date +%F)`
2. `sudo nixos-rebuild switch --flake .#cloudbox` and `nix run home-manager -- switch --flake .#cloudbox`
3. In a quiet window: `sudo systemctl restart goose-serve`, then `systemctl --user restart honkd`.
4. Verify: in a honk chat session ask the agent to list its tools (expect
   `self_compact_and_resume`); ask it to self-compact with a resume prompt that names a
   trivial first action; expect one continuous run: tool call → "Context self-compacted"
   → the resume prompt as a steer block → the agent acting on it. `honk list` shows the
   session idle afterwards.
5. `/compact` typed in honk chat compacts and answers "Compaction complete".
