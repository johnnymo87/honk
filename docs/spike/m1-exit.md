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
