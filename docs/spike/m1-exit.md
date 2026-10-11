# Spike Report: Early Live Checks — Real Pigeon Client and Extension Set on Activation (Task A10)

## Overview

Task A10 exercises two early live checks against the patched sandbox (`honk-goose-sandbox-patched` on `:4181`) to surface downstream integration and activation containment risks before subsequent M1 work:

1. **Part 1: Real Pigeon Client against honkd.** Validates that pigeon's unmodified `GooseAcpClient` with `webSocketTransport` and production `permissionPolicy` can connect to `honkd`, create a session with restricted extensions (`developer`), and complete an end-to-end prompt turn.
2. **Part 2: Extension Set on Activation.** Verifies whether re-activating a session via `session/load` on a fresh connection preserves the restricted extension set from `session/new` or widens to the serve's default extensions.

Both checks passed against the live patched sandbox.

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

Probe test `crates/honk-upstream/tests/live_activation.rs`:
1. Connection A initializes and subscribes to connection stream.
2. Calls `session/new` specifying `_meta.enabledExtensions: [{ type: "platform", name: "developer" }]`.
3. Records `_meta.extensionResults` returned by `session/new`.
4. DELETES Connection A.
5. Connection B initializes on a fresh connection, subscribes to connection and session streams.
6. Calls `session/load { sessionId, cwd, mcpServers: [] }`.
7. Records `_meta.extensionResults` returned by `session/load` and compares against the `session/new` set.

### Results

- **Status:** PASS
- **`session/new` extension set:**
  ```json
  [{"name": "developer", "success": true}]
  ```
- **`session/load` extension set:**
  ```json
  [{"name": "developer", "success": true}]
  ```
- **Comparison:**
  - Surplus (widened): `[]`
  - Missing: `[]`
  - Exact match: `true`

### Stop Rule Analysis

- **Stop rule triggered:** **NO**.
- **Observation:** `session/load` on a fresh connection did not revert to host default extensions or widen beyond what was explicitly configured during `session/new`. Goose preserved the single `developer` extension across connection teardown and reload.
- **Risk evaluation:** Pigeon's `assertExactExtensions` check only executes at `session/new`. Because `session/load` on the patched sandbox preserves the extension set without widening, `honkd` re-activation will not silently broaden permissions or inject unwanted extensions into pigeon sessions.
