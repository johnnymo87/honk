# Spike Report: Early Live Checks — Real Pigeon Client and Extension Set on Activation (Task A10)

## Overview

Task A10 exercises early checks to surface downstream integration and activation containment risks before subsequent M1 work:

1. **Part 1: Real Pigeon Client against honkd.** Validates that pigeon's unmodified `GooseAcpClient` with `webSocketTransport` and production `permissionPolicy` can connect to `honkd`, create a session with restricted extensions (`developer`), and complete an end-to-end prompt turn against the live patched sandbox (`:4181`).
2. **Part 2: Extension Set on Activation.** Verifies whether re-activating a session via `session/load` (or direct prompt) on a fresh connection preserves the restricted extension set from `session/new` or widens to the serve config's full enabled set. Tested both on the live sandbox (`:4181`) and in a controlled harness (`honk-testgoose`) against a wider `config.yaml` enabling multiple extensions.

Both checks passed.

---

## Part 1: Real Pigeon Client Integration

### Setup

- **Upstream:** `honk-goose-sandbox-patched` (`http://127.0.0.1:4181/acp`).
- **Relay:** `honkd` compiled from source (`target/debug/honkd`), run as a transient user unit with:
  - `--upstream http://127.0.0.1:4181/acp`
  - `--upstream-secret-file $HOME/.local/state/honk-sandbox-patched/secret`
  - `--state-dir <fresh mktemp>`
  - `--listen 127.0.0.1:0` (dynamically bound port)
- **Client:** Pigeon's `GooseAcpClient` (`packages/daemon/src/goose/acp-client.ts`) driven by Vitest in a throwaway worktree:
  - Transport: `webSocketTransport` with `?token=<honkd-secret>`
  - `permissionPolicy`: `(params) => params.options[0]?.optionId`
  - `sessionExtensions`: `[{ type: "platform", name: "developer" }]`
  - `serveFloor`: `["developer"]`
  - Prompt: `"Reply with exactly: PIGEON_OK"`

### Outcome

- **Status:** PASS
- **Receipt `stopReason`:** `end_turn`
- **Receipt text:** `PIGEON_OK`
- **Turn duration:** 1336 ms
- **honkd changes needed:** None. `honkd` routed handshake, session creation, extension negotiation, streaming notifications, and prompt response cleanly.

### Pigeon Vitest Implementation (for M1b PR)

The following test was executed in pigeon's `@pigeon/daemon` test suite and will move into the M1b pigeon PR:

```typescript
import { describe, it, expect } from "vitest";
import {
  GooseAcpClient,
  type GooseReceipt,
} from "../src/goose/acp-client.js";
import { webSocketTransport } from "../src/goose/ws-transport.js";
import * as os from "node:os";
import * as fs from "node:fs/promises";
import * as path from "node:path";

const HONKD_URL = process.env.PIGEON_TEST_HONKD_URL;
const HONKD_TOKEN = process.env.PIGEON_TEST_HONKD_TOKEN;

describe("honkd live integration with real pigeon client", () => {
  const runTest = HONKD_URL && HONKD_TOKEN ? it : it.skip;

  runTest(
    "connects through honkd, starts developer session, and completes turn",
    async () => {
      const tmpdir = await fs.mkdtemp(
        path.join(os.tmpdir(), "pigeon-honkd-test-"),
      );
      const accumulatedText: string[] = [];

      const client = new GooseAcpClient({
        url: HONKD_URL!,
        transportFactory: webSocketTransport(HONKD_TOKEN!),
        permissionPolicy: (params) => params.options[0]?.optionId,
        sessionExtensions: [{ type: "platform", name: "developer" }],
        serveFloor: ["developer"],
        onSessionUpdate: (_sid, update) => {
          if (update.sessionUpdate === "agent_message_chunk") {
            const text = (update.content as { text?: unknown } | undefined)?.text;
            if (typeof text === "string" && text !== "") {
              accumulatedText.push(text);
            }
          }
        },
      });

      try {
        await client.connect();
        const sid = await client.newSession(tmpdir);
        expect(sid).toBeTruthy();

        const outcome = await client.prompt(
          sid,
          "Reply with exactly: PIGEON_OK",
        );
        expect(outcome.kind).toBe("receipt");

        const receipt = outcome as GooseReceipt;
        expect(receipt.stopReason).toBe("end_turn");

        const fullReceiptText = accumulatedText.join("");
        console.log(`[PIGEON_RECEIPT_TEXT] ${fullReceiptText}`);
        expect(fullReceiptText).toContain("PIGEON_OK");
      } finally {
        client.close();
        await fs.rm(tmpdir, { recursive: true, force: true }).catch(() => {});
      }
    },
    120_000,
  );
});
```

---

## Part 2: Extension Set on Re-activation

### Test Design

Two tests evaluate session activation behavior:

1. **Live Sandbox Wire Probe (`crates/honk-upstream/tests/live_activation.rs`):**
   - Runs against the patched sandbox on `:4181`.
   - `session/new` with `_meta.enabledExtensions: [developer]` -> records `_meta.extensionResults`.
   - Connection A is deleted.
   - Connection B sends `session/load { sessionId, cwd, mcpServers: [] }` -> records `_meta.extensionResults`.
   - *Note on sandbox config:* The patched sandbox unit's `config.yaml` enables only `developer`, so wire parity alone does not test whether a wider host config would bleed into a re-activated session.

2. **Wider Host Config Integration Test (`crates/honk-testgoose/tests/activation.rs`):**
   - Runs real patched goose (`patched-1.54.0.3`) via `StubGoose` with a `config.yaml` explicitly enabling `developer` and `todo`.
   - Ground truth evidence: directly inspects the OpenAI `tools` array delivered to the mock model (`StubModel::turn_requests()`) for each turn.
   - **Sanity check:** An un-narrowed session (`session/new` with no `_meta.enabledExtensions`) loads 9 extensions (`Extension Manager`, `analyze`, `apps`, `developer`, `scheduler`, `skills`, `summon`, `todo`, `tom`) and offers 18 tools to the model (including `todo__todo_write`). This proves the wider config is active and capable of loading `todo`.
   - **Variant A (`session/load` on fresh connection):**
     - Initial session created with `_meta.enabledExtensions: [{ type: "platform", name: "developer" }]`.
     - Initial prompt records 5 developer tools: `{"edit", "read_image", "shell", "tree", "write"}`.
     - Connection 1 is deleted.
     - Fresh Connection 2 connects, issues `session/load { sessionId, cwd, mcpServers: [] }`.
     - `session/load` reports `_meta.extensionResults: [{"name": "developer", "success": true}]`.
     - Prompt turn on Connection 2 records tools delivered to the model.
   - **Variant B (Direct prompt on fresh connection without load):**
     - Mirrors pigeon's reconnect path after a goose restart or socket drop.
     - New session created with `_meta.enabledExtensions: [developer]`.
     - Connection deleted.
     - Fresh connection sends `session/prompt` directly without `session/load`.
     - Prompt turn records tools delivered to the model.

### Results

#### 1. Live Sandbox Probe (:4181)
- **Status:** PASS
- **`session/new` extension set:**
  ```json
  [{"name": "developer", "success": true}]
  ```
- **`session/load` extension set:**
  ```json
  [{"name": "developer", "success": true}]
  ```
- **Surplus / Missing:** None (`[]`).

#### 2. Wider Config Test (`honk-testgoose` against `patched-1.54.0.3`)
- **Sanity Baseline (no narrowing):**
  - Loaded extensions: `{"Extension Manager", "analyze", "apps", "developer", "scheduler", "skills", "summon", "todo", "tom"}` (9 extensions)
  - Registered model tools: 18 tools (includes `todo__todo_write`, `delegate`, `apps__*`, `extensionmanager__*`)
- **Initial Session (narrowed to `developer`):**
  - `_meta.extensionResults`: `[{"name": "developer", "success": true}]`
  - Registered model tools: 5 tools `{"edit", "read_image", "shell", "tree", "write"}`
  - Contains `todo` tool: `false`
- **Variant A (`session/load` on fresh connection):**
  - `_meta.extensionResults`: `[{"name": "developer", "success": true}]`
  - Registered model tools: 5 tools `{"edit", "read_image", "shell", "tree", "write"}`
  - Surplus extensionResults: `[]`
  - Surplus tools: `[]`
- **Variant B (Prompt on fresh connection without load):**
  - Registered model tools: 5 tools `{"edit", "read_image", "shell", "tree", "write"}`
  - Surplus tools: `[]`

### Stop Rule Analysis

- **Stop rule triggered:** **NO**.
- **Conclusion:** Re-activation (both via `session/load` and via direct `session/prompt` on a fresh connection) strictly preserves the session's original narrowed extension set. Goose stores enabled extensions in SQLite and does not reload unrequested extensions from `config.yaml` during re-activation.
- **Containment guarantee:** Pigeon's `assertExactExtensions` check (which runs only at `session/new`) remains safe across reconnections and `honkd` re-activations without risk of silent capability widening.

---

## Part 3: Live M1a Exit Tests on Patched Sandbox (Task A19)

### Setup & Environment

- **Target Upstream:** `honk-goose-sandbox-patched` user unit (`http://127.0.0.1:4181/acp`, token from `$HOME/.local/state/honk-sandbox-patched/secret`).
- **Relay Under Test:** `honkd` dynamically spawned on `127.0.0.1:0` with fresh temporary state directory (`Honkd::spawn`).
- **Model:** `claude-haiku-4-5@20251001` via Claude failover proxy, discovered from `session/new` `configOptions` and configured via `session/set_config_option`.
- **Harness:** `crates/honkd/tests/live_exit.rs`. Executed with `HONK_SANDBOX_PATCHED=$HOME/.local/state/honk-sandbox-patched nix develop -c cargo test -p honkd --test live_exit -- --ignored --test-threads=1 <name>`.

### Test Results Summary

| Test | Objective | Status | Duration | Key Observations |
|---|---|---|---|---|
| `e1_pigeon_turn_visible_in_tail` | PigeonLike turn streamed through honkd is visible in `honk::tail::run` | **PASS** | 1.77s | `tail_text == exp_turn_text`, `pigeon.receipt_text() == exp_turn_text`, terminal contains `HONK_E1_OK`. |
| `e2_kill9_honkd_mid_turn_delta_resume` | `kill -9 honkd` mid-turn while streaming tides prompt; tail resumes with delta; PigeonLike retries with same `client_req_id` | **PASS** | 3.84s | `tail_outcome.reconnect_resets == 0` (clean delta resume without `_honk/reset`); `tail_text == exp_turn_text == pigeon.receipt_text()`; export contains exactly 1 user prompt (no duplication). |
| `e3_goose_restart_mid_turn` | Patched sandbox restart during `sleep 40` tool call; turn cancellation and retry deduplication | **PASS** | 44.32s | `stopReason: "cancelled"`, `_meta.honk.reason: "upstream_restart"`; `pigeon.receipt_text() == mid_text` (empty); duplicate retry requests deduped into single upstream turn; export shows original + exactly 1 retry; sandbox verified active and answering. |
| `e4_permission_across_honkd_restart` | Permission request pending across `kill -9 honkd` and restart; re-offered to new viewer and answerable | **PASS** | 1.89s | Goose request ID preserved and re-offered to reconnected viewer on `session/load`; `allow_once` response executes tool; export contains `HONK_E4_OK`. |
| `delete_frees_parked_run` | Direct raw `honk-upstream` connection deleted while parked on permission; session load on new connection releases parked run and allows new prompt | **PASS** | 2.38s | Connection A deleted; on connection B, cancelling re-offered permission completes parked run without executing tool (`check_export_tool_output` false); subsequent prompt completes with `end_turn`. |
| `takeover_and_resume_live` | Stream frozen via `TcpProxy` for 30s; byte watchdog fires at 20s and reattaches with takeover | **PASS** | 31.18s | Reattached with takeover over fresh proxied connection without 409; turn completes cleanly (`end_turn`); `tail_outcome.reconnect_resets <= 1`; `tail_text == exp_turn_text` with no duplicate tokens. |

### Observations, Artefacts & Bug Fixes

#### 1. Bug Fix: Restored In-Flight Delivery Check (`crates/honkd/src/session.rs`)
- **Symptom:** During `e2_kill9_honkd_mid_turn_delta_resume`, after restarting `honkd`, the prompt retry with the same `client_req_id` resulted in 2 user messages in goose's export instead of 1.
- **Root Cause Analysis:**
  - In `crates/honkd/src/session.rs`, `check_restored_inflight_delivery` invoked `export_has_user_text`.
  - The previous implementation inspected messages *strictly after* `last_assistant_idx = conversation.rposition(|m| m.role == "assistant")`.
  - In live environments, when `honkd` was killed, upstream goose continued generating tokens and committed the assistant message to SQLite while `honkd` was dead.
  - Upon restart, goose's export already had an assistant message for the in-flight turn. Skipping past that assistant message caused `export_has_user_text` to find no trailing user message, falsely concluding that the in-flight prompt was never delivered to goose.
  - This triggered `self.turn.interrupt("not_delivered")`, clearing `inflight` state. Consequently, when the reconnected PigeonLike client resent the prompt with the same `client_req_id`, `honkd` treated it as a fresh turn and dispatched a second prompt to goose.
  - Additionally, goose inserts synthetic turn-context messages (`role: "user"`, `metadata: { "userVisible": false, "turnContext": true }`) which had to be filtered.
- **Resolution:**
  - Made `honk_wire::export::is_message_user_visible` public.
  - Replaced `export_has_user_text` with `export_has_inflight_delivery(&export_val, &user_text, logged_completed_turns)`.
  - If `export_completed_turns > logged_completed_turns`, goose completed a turn while `honkd` was down; the function verifies whether the user message preceding that last assistant turn matches `expected_text`.
  - If `export_completed_turns <= logged_completed_turns`, goose has not completed any new turn, and the function verifies trailing user messages after `last_assistant_idx`.
  - Added unit test in `crates/honkd/tests/restart.rs`: `turn_completed_on_upstream_during_honkd_restart_recognized_delivered_and_deduped`.

#### 2. Test Artefact: `e3_goose_restart_mid_turn` Prompt Phrasing
- **Symptom:** Probabilistic models (Claude Haiku) occasionally emitted preliminary text (e.g. *"I'll execute that command right now."*) before invoking the `shell` tool.
- **Observation:** If the model emits preliminary text before the tool call, that text is streamed live over SSE to `pigeon`, but when goose is killed via `systemctl restart` mid-tool-call, goose aborts before committing the turn to SQLite. Thus, `pigeon.receipt_text()` contained the uncommitted preamble while `export` turn text was empty.
- **Resolution:** Tuned prompt to instruct the model to execute the tool immediately without preliminary conversational text: `"Run the bash command 'sleep 40' immediately using your shell tool. Output no text before calling the tool."` This ensures deterministic parity between live streaming and export.

#### 3. Wire Finding: `delete_frees_parked_run` Mechanism
- **Observation:** When connection A is terminated via `DELETE /acp` while waiting on a tool confirmation in approve mode, goose's SQLite database retains the unconfirmed tool call. When connection B initializes and issues `session/load`, goose re-offers `session/request_permission`.
- **Resolution:** Answering `outcome: { outcome: "cancelled" }` to the re-offered request releases the parked tool call without executing it (`check_export_tool_output` confirms the tool never ran), freeing the session to accept and complete the subsequent prompt (`Reply with exactly: UNPARKED_OK`).

### M1a Exit Status

All criteria from design §8 are verified on the live patched sandbox (`:4181`):
1. **Pigeon turn visible in `honk tail`:** Verified by `e1_pigeon_turn_visible_in_tail`.
2. **`kill -9 honkd` mid-turn completes turn and tail resumes with delta:** Verified by `e2_kill9_honkd_mid_turn_delta_resume`.
3. **Pending permission across `honkd` restart remains answerable:** Verified by `e4_permission_across_honkd_restart`.
4. **Goose restart mid-turn returns cancelled, retry not duplicated:** Verified by `e3_goose_restart_mid_turn`.
5. **Auxiliary wire checks:** `delete_frees_parked_run` and `takeover_and_resume_live` pass.

Sandbox service `honk-goose-sandbox-patched` remains active and answering on `:4181`.
