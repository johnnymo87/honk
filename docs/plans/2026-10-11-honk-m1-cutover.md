# M1b Operator Runbook: pigeon Goose Traffic Cutover to honkd

> **AUTHOR-GATED**: This runbook must ONLY be executed by an operator/human after
> explicit author approval. Do NOT execute this document automatically.

---

## 1. Context and Verified Topology

### 1.1 Architecture
Before cutover:
```
pigeon-daemon (systemd system service, User=$USER)
  │
  ├─ [ws://127.0.0.1:4080/acp + /run/secrets/pigeon_goose_acp_token]
  ▼
goose-serve (systemd system service, 127.0.0.1:4080)
```

After cutover:
```
pigeon-daemon (systemd system service, User=$USER)
  │
  ├─ [ws://127.0.0.1:4280/acp + /run/secrets/pigeon_goose_acp_token (stable token via symlink)]
  ▼
honkd (systemd user service, 127.0.0.1:4280)
  │
  ├─ [http://127.0.0.1:4080/acp + /run/secrets/pigeon_goose_acp_token]
  ▼
goose-serve (systemd system service, 127.0.0.1:4080)
```

### 1.2 Component Facts (Verified 2026-10-12)
- **Production goose**:
  - System service: `goose-serve` bound to `127.0.0.1:4080`.
  - Binary version: `patched-1.54.0.3` (advertises `takeover, replay` in `Acp-Stream-Features`).
  - Auth token: `/run/secrets/pigeon_goose_acp_token` (mode `0400`, owner `$USER`, readable by user services running as `$USER`). Shell variable in runbook: `GOOSE_TOKEN_FILE=/run/secrets/pigeon_goose_acp_token`.
- **honkd service (B4 split: part a)**:
  - Systemd user service running as `$USER` (the user that runs pigeon and goose).
  - B4 is split into:
    - (a) `honkd` home-manager user unit (`systemd.user.services.honkd`), deployed BEFORE cutover Step 2 (inert until pigeon is repointed; nobody connects to `:4280`).
    - (b) pigeon environment change `PIGEON_GOOSE_ACP_URL` → `ws://127.0.0.1:4280/acp`, which IS the cutover step (`nixos-rebuild switch` restarts `pigeon-daemon`).
  - Service unit requirements:
    - User unit (`honkd.service`).
    - ExecStart: `honkd --listen 127.0.0.1:4280 --upstream http://127.0.0.1:4080/acp --allow-production-upstream --upstream-secret-file /run/secrets/pigeon_goose_acp_token`
    - Restart policy: `Restart=always`, `RestartSec=5`, `StartLimitIntervalSec=0` (honkd exits if goose isn't up yet at boot — automatic connection retry is tracked as an M1c bead).
    - Switch method: `X-SwitchMethod=keep-old` (home-manager switch must not restart honkd mid-turn).
    - Memory cap: `MemoryMax=1G` (enforces container slice resource bounds).
    - PATH: `honk` on PATH (provided by `packages.honkd` in the honk flake).
    - State dir: default state dir `$HOME/.local/state/honk` (do NOT pass `--state-dir`).
    - Install target: `WantedBy=default.target`.
    - Exclusion: must be excluded from the nightly `reset-workspace` restart pool (honkd is a persistent relay daemon, not an opencode serve instance).
  - Illustrative unit definition:
    ```ini
    [Unit]
    Description=honkd - durable ACP proxy for goose
    After=network.target

    [Service]
    ExecStart=%h/.nix-profile/bin/honkd --listen 127.0.0.1:4280 --upstream http://127.0.0.1:4080/acp --allow-production-upstream --upstream-secret-file /run/secrets/pigeon_goose_acp_token
    Restart=always
    RestartSec=5
    StartLimitIntervalSec=0
    X-SwitchMethod=keep-old
    MemoryMax=1G

    [Install]
    WantedBy=default.target
    ```
  - State dir & Stable Token:
    - Default state dir: `$HOME/.local/state/honk` (or `$XDG_STATE_HOME/honk`).
    - Stable downstream token: pre-seed `$HOME/.local/state/honk/secret` as a symlink to `/run/secrets/pigeon_goose_acp_token`.
    - Symlink verification: `crates/honkd/src/main.rs:46` checks `secret_path.exists()` and reads with `std::fs::read_to_string`, both of which transparently follow symlinks. It does not perform file permission or ownership checks on existing files or symlinks; it only validates `trimmed.len() >= 32` (`/run/secrets/pigeon_goose_acp_token` is 40 hex chars, 41 bytes, mode `0400`, owned by `$USER`).
    - Because honkd's downstream secret is identical to the goose token, pigeon's token remains completely unchanged! Only `PIGEON_GOOSE_ACP_URL` changes.
  - CLI `honk`:
    - Defaults to `--url http://127.0.0.1:4280/acp` (or `$HONK_URL`).
    - Resolves secret automatically from `$HOME/.local/state/honk/secret` (which points to the token).
    - Zero-config secret resolution when honkd uses the default state dir.
- **pigeon daemon**:
  - System unit: `pigeon-daemon` (User `$USER`).
  - Deploy model: pigeon runs via `tsx` directly against the checkout at `$HOME/projects/pigeon` (`exec node .../tsx/dist/cli.mjs $HOME/projects/pigeon/packages/daemon/src/index.ts`).
  - Namespace journal: logs in `pigeon` namespace (`journalctl --namespace=pigeon -u pigeon-daemon`).
  - Database: `$HOME/projects/pigeon/packages/daemon/data/pigeon-daemon.db`.
  - Storage table: `sessions` (columns: `session_id`, `backend_kind`, `backend_endpoint`, `backend_auth_token`, `backend_session_id`).
  - ACP goose sessions have `backend_kind = 'goose-acp'`.
  - Session IDs: `session_id` is pigeon's internal session ID, whereas `backend_session_id` is goose's ACP session ID. honkd indexes sessions by `backend_session_id`.
  - Precedence in `index.ts`: a session row's `backend_endpoint` takes precedence over `PIGEON_GOOSE_ACP_URL`.
  - Row count: today there are 0 `goose-acp` rows in `pigeon-daemon.db`.
  - Workstation repo: `PIGEON_GOOSE_ACP_URL` is set in systemd `Environment=` in `$HOME/projects/workstation/hosts/cloudbox/configuration.nix`.
  - SQLite CLI: run via `nix shell nixpkgs#sqlite -c sqlite3 -cmd '.timeout 5000'`.
- **pigeon branch requirement (B1)**:
  - Pigeon must run the B1 branch (honk-aware runner: transmits `_meta.honk.client_req_id`, on honk disconnect retries the reconnect with backoff (~2 min budget) and resends with the same id, receives authoritative turn text from `_meta.honk.text`).
- **Operational limits**:
  - `--max-sessions 16`: M1a has no LRU eviction.
  - Socket idle timeout: pigeon closes idle runner sockets after 10 minutes of inactivity (`DEFAULT_IDLE_SOCKET_TIMEOUT_MS = 10 * 60 * 1000`) for honk connections (`isHonk: true`). Therefore, held sessions in honkd are bounded by recent activity (sessions active within the last 10 minutes), not by the lifetime of the pigeon-daemon process.
  - `--idle-ttl-secs 900`: 15-minute idle TTL in honkd for unheld/unviewed sessions.
- **Cutover blockers**:
  - In addition to B1 and B4a, the cutover is blocked by bead `honk-znj.9` and bead `honk-znj.10`.

---

## 2. Cutover Procedure

### Step 1: Preflight Verification

1. Verify production goose is running patched-1.54.0.3 and advertising required features:
   ```bash
   systemctl is-active goose-serve
   # Expected: active
   ```
2. Verify production goose secret file is readable:
   ```bash
   GOOSE_TOKEN_FILE="/run/secrets/pigeon_goose_acp_token"
   test -r "$GOOSE_TOKEN_FILE" && [ $(wc -c < "$GOOSE_TOKEN_FILE") -ge 32 ] && echo "Token OK"
   # Expected: Token OK
   ```
3. Pre-seed the honkd downstream secret symlink to the goose token:
   ```bash
   mkdir -p "$HOME/.local/state/honk"
   if [ ! -e "$HOME/.local/state/honk/secret" ]; then
     ln -s /run/secrets/pigeon_goose_acp_token "$HOME/.local/state/honk/secret"
   fi
   test -r "$HOME/.local/state/honk/secret" && [ $(wc -c < "$HOME/.local/state/honk/secret") -ge 32 ] && echo "Symlink secret OK"
   # Expected: Symlink secret OK
   ```
4. Verify honk flake package builds cleanly from the repository checkout and binaries respond to `--help`:
   ```bash
   nix build .#honkd --out-link /tmp/honkd-result
   /tmp/honkd-result/bin/honkd --help
   /tmp/honkd-result/bin/honk --help
   ```
5. Verify pigeon daemon is running a build containing B1:
   Because pigeon runs directly from the working tree at `$HOME/projects/pigeon` via `tsx`:
   - Check that the B1 commit is in the checkout:
     ```bash
     git -C "$HOME/projects/pigeon" merge-base --is-ancestor "<B1_COMMIT_OR_BRANCH>" HEAD && echo "B1 commit ancestor OK"
     ```
   - Check that honk-aware session runner code is present:
     ```bash
     grep -q 'honk disconnect mid-turn' "$HOME/projects/pigeon/packages/daemon/src/goose/session-runner.ts" && echo "B1 runner code OK"
     ```
   - Check that `pigeon-daemon` process was started after the B1 code was deployed:
     ```bash
     PIGEON_START="$(systemctl show pigeon-daemon --property=ActiveEnterTimestamp --value)"
     CHECKOUT_MOD="$(stat -c %y "$HOME/projects/pigeon/packages/daemon/src/goose/session-runner.ts")"
     echo "pigeon-daemon active since: $PIGEON_START (checkout mod: $CHECKOUT_MOD)"
     ```
6. Verify cutover blockers `honk-znj.9` and `honk-znj.10` are resolved:
   ```bash
   bd show honk-znj.9 honk-znj.10
   ```
   Both beads must be closed before proceeding.
7. **Abort criteria**:
   - `goose-serve` is inactive or lacks stream features.
   - Secret file is missing, unreadable, or shorter than 32 characters.
   - Symlink creation failed.
   - `nix build` fails.
   - Pigeon checkout lacks B1, runner code is missing, or pigeon-daemon was not restarted with B1.
   - Blocker beads `honk-znj.9` or `honk-znj.10` remain open.
   **Action**: Do not proceed. No services have been started or altered. Fix the prerequisite before re-attempting.

---

### Step 2: Deploy & Start honkd (B4a) and Verify Feature Gate

1. Start the honkd user service (deployed via home-manager user unit `honkd.service` per B4a):
   ```bash
   systemctl --user start honkd
   systemctl --user status honkd
   # Expected: active (running)
   ```
2. Inspect honkd startup logs to verify the upstream feature gate passed:
   ```bash
   journalctl --user -u honkd -n 50 --no-pager
   ```
   **Expected log indicators**:
   - `honkd listening on 127.0.0.1:4280` (printed to stdout via `println!`, captured by journald).
   - Note: `Hub::start` does not emit an explicit log line for feature-gate success; absence of error and presence of `honkd listening on ...` confirms the gate passed.
   - No stderr error: `honkd: upstream goose lacks Acp-Stream-Features takeover+replay; run patched-1.54.0.3 or pass --allow-unpatched-upstream`.
3. Check the downstream secret:
   ```bash
   test -r "$HOME/.local/state/honk/secret" && [ $(wc -c < "$HOME/.local/state/honk/secret") -ge 32 ] && echo "Secret OK"
   # Expected: Secret OK
   ```
4. Verify `honk list` can talk to honkd and read upstream sessions:
   Use `honk` installed on PATH, or via `nix shell .#honkd -c honk list`:
   ```bash
   honk list --url http://127.0.0.1:4280/acp
   # Or using nix shell if honk is not yet on PATH:
   nix shell .#honkd -c honk list --url http://127.0.0.1:4280/acp
   ```
   **Expected output**:
   - List of current goose sessions (or exit code 0 with empty output if no sessions exist).
5. **Abort criteria**:
   - honkd exits with code 1 and stderr `honkd: upstream goose lacks Acp-Stream-Features takeover+replay; run patched-1.54.0.3 or pass --allow-unpatched-upstream`.
   - `honk list` returns connection refused or 401 unauthorized.
   **Action**: Stop honkd (`systemctl --user stop honkd`). Traffic has not been cut over yet; goose and pigeon remain untouched. Investigate logs (`journalctl --user -u honkd -n 50`).

---

### Step 3: Quiet Window Check

1. Verify no pigeon goose turn is in flight by inspecting established network sockets to goose:
   ```bash
   ss -tn state established '( dport = :4080 )' | tail -n +2 | wc -l
   # Expected: 0
   ```
   *Mechanical guarantee*: Pigeon maintains an established WebSocket connection to `:4080` whenever a goose turn is actively executing. If the count is 0, no goose turn is in flight. (After cutover, the equivalent check will inspect `:4280`).
2. **Abort criteria**:
   - Socket count is greater than 0 (an active turn is in progress).
   **Action**: Wait for in-flight turn(s) to finish until the established socket count reaches 0. honkd can safely remain running while waiting because it is completely inert until pigeon is repointed.

---

### Step 4: Backup Pigeon DB and Pre-Cutover Row Repoint

1. Create a point-in-time SQLite backup of `pigeon-daemon.db` (for catastrophic disaster recovery only):
   ```bash
   BACKUP_PATH="$HOME/projects/pigeon/packages/daemon/data/pigeon-daemon.db.bak-$(date +%Y%m%d%H%M%S)"
   nix shell nixpkgs#sqlite -c sqlite3 -cmd '.timeout 5000' "$HOME/projects/pigeon/packages/daemon/data/pigeon-daemon.db" ".backup '$BACKUP_PATH'"
   if [ -f "$BACKUP_PATH" ]; then
     echo "Backup created: $BACKUP_PATH"
   else
     echo "ERROR: Backup creation failed"
   fi
   ```
2. Query and record the number of existing `goose-acp` rows pointing to production goose:
   ```bash
   nix shell nixpkgs#sqlite -c sqlite3 -cmd '.timeout 5000' "$HOME/projects/pigeon/packages/daemon/data/pigeon-daemon.db" \
     "SELECT count(*) FROM sessions WHERE backend_kind='goose-acp' AND backend_endpoint='ws://127.0.0.1:4080/acp';"
   ```
   *Note*: The count is expected to be `0` because pigeon currently has virtually no persisted goose ACP sessions.
3. Repoint existing `goose-acp` session rows to honkd:
   Because honkd's downstream secret symlinks to `/run/secrets/pigeon_goose_acp_token`, the auth token is identical and does not need to be updated or passed on the command line:
   ```bash
   nix shell nixpkgs#sqlite -c sqlite3 -cmd '.timeout 5000' "$HOME/projects/pigeon/packages/daemon/data/pigeon-daemon.db" \
     "UPDATE sessions SET backend_endpoint='ws://127.0.0.1:4280/acp' WHERE backend_kind='goose-acp' AND backend_endpoint='ws://127.0.0.1:4080/acp';"
   ```
4. Confirm no session rows remain pointing directly to `:4080`:
   ```bash
   nix shell nixpkgs#sqlite -c sqlite3 -cmd '.timeout 5000' "$HOME/projects/pigeon/packages/daemon/data/pigeon-daemon.db" \
     "SELECT count(*) FROM sessions WHERE backend_kind='goose-acp' AND backend_endpoint='ws://127.0.0.1:4080/acp';"
   # Expected: 0
   ```
5. **Abort criteria**:
   - Database backup fails.
   - SQLite UPDATE fails with error or database locked.
   **Action**: Stop honkd (`systemctl --user stop honkd`). If any rows were updated, execute Section 3 (Rollback Procedure) Step 2 to restore rows back to `:4080`.

---

### Step 5: Cutover — Switch Pigeon Environment via NixOS Rebuild

1. Apply the workstation change (B4b) in `$HOME/projects/workstation`:
   - Set `PIGEON_GOOSE_ACP_URL=ws://127.0.0.1:4280/acp` in `hosts/cloudbox/configuration.nix`.
   - (Note: `PIGEON_GOOSE_ACP_TOKEN` remains unchanged, reading `/run/secrets/pigeon_goose_acp_token`).
   - Run system deployment:
     ```bash
     sudo nixos-rebuild switch
     ```
     *Note*: `nixos-rebuild switch` restarts `pigeon-daemon` automatically because its systemd configuration changed; no separate manual restart command is required.
2. Verify `pigeon-daemon` is active:
   ```bash
   systemctl is-active pigeon-daemon
   # Expected: active
   ```
3. Re-run the DB row UPDATE (idempotent, to catch any rows created during the restart window):
   ```bash
   nix shell nixpkgs#sqlite -c sqlite3 -cmd '.timeout 5000' "$HOME/projects/pigeon/packages/daemon/data/pigeon-daemon.db" \
     "UPDATE sessions SET backend_endpoint='ws://127.0.0.1:4280/acp' WHERE backend_kind='goose-acp' AND backend_endpoint='ws://127.0.0.1:4080/acp';"
   ```
   Confirm 0 rows point to `:4080`:
   ```bash
   nix shell nixpkgs#sqlite -c sqlite3 -cmd '.timeout 5000' "$HOME/projects/pigeon/packages/daemon/data/pigeon-daemon.db" \
     "SELECT count(*) FROM sessions WHERE backend_kind='goose-acp' AND backend_endpoint='ws://127.0.0.1:4080/acp';"
   # Expected: 0
   ```
4. Inspect pigeon daemon logs in its journal namespace:
   ```bash
   journalctl --namespace=pigeon -u pigeon-daemon -n 50 --no-pager
   ```
   Verify pigeon started cleanly and registered the goose capability.
5. **Abort criteria**:
   - `pigeon-daemon` fails to start or continuously crash loops.
   - Pigeon logs report errors reading configuration.
   **Action**: Go immediately to Section 3 (Rollback Procedure) to revert pigeon environment variables and repoint rows.

---

### Step 6: Live Verification

Because pigeon receives little natural goose traffic, verification requires deliberately driven turns.

#### 6A: Standard Turn, honk Detection, and Receipt Delivery
1. Trigger a goose turn via pigeon (e.g., via Telegram or test prompt to a pigeon goose session).
2. Verify pigeon detected honkd via the connection log line:
   ```bash
   journalctl --namespace=pigeon -u pigeon-daemon -n 100 --no-pager | grep "goose acp connected"
   ```
   **Expected log line**:
   `[goose] goose acp connected { url: 'ws://127.0.0.1:4280/acp', isHonk: true }`
   The presence of `isHonk: true` proves pigeon learned from `initialize` response `_meta.honk` that honkd is in the traffic path.
3. Query goose's backend session ID from pigeon DB for `honk tail`:
   ```bash
   BACKEND_SID="$(nix shell nixpkgs#sqlite -c sqlite3 -cmd '.timeout 5000' "$HOME/projects/pigeon/packages/daemon/data/pigeon-daemon.db" \
     "SELECT backend_session_id FROM sessions WHERE backend_kind = 'goose-acp' ORDER BY rowid DESC LIMIT 1;")"
   echo "Goose session ID: $BACKEND_SID"
   ```
   *(Note: pigeon's `session_id` differs from goose's `backend_session_id`; `honk tail` requires goose's `backend_session_id`).*
4. Monitor turn progress in real time via `honk tail`:
   ```bash
   honk tail "$BACKEND_SID" --url http://127.0.0.1:4280/acp
   # Or using nix shell if honk is not yet on PATH:
   nix shell .#honkd -c honk tail "$BACKEND_SID" --url http://127.0.0.1:4280/acp
   ```
5. **Verify**:
   - Live stream chunks appear in `honk tail`.
   - The turn completes cleanly.
   - The final receipt arrives in Telegram / pigeon client with the expected output text.

#### 6B: Honkd Mid-Turn Restart Resilience
*Note*: This resilience depends on pigeon's reconnect-with-backoff (implemented in B1) and `X-SwitchMethod=keep-old` on the honkd user unit.
1. Start a longer goose turn via pigeon.
2. While the turn is actively running, restart honkd:
   ```bash
   systemctl --user restart honkd
   ```
3. **Verify**:
   - Pigeon B1 client detects disconnect, logs `[goose] honk disconnect mid-turn, retrying reconnect and resend with backoff`, reconnects to `127.0.0.1:4280`, and re-attaches with the same `_meta.honk.client_req_id`.
   - honkd restores the session, rebuilds turn progress, and continues relaying to goose.
   - The turn finishes successfully.
   - Exactly **one** full receipt arrives in Telegram / pigeon client with no duplicate text or dropped responses.

#### 6C: Upstream goose-serve Restart Handling (Optional / Quiet Window Only)
*Note*: This verification tests honest cancellation reporting when upstream goose restarts. Because restarting `goose-serve` affects all users of production goose, this step is OPTIONAL and must ONLY be executed during an explicit quiet window.
1. Start a driven goose turn via pigeon.
2. While the turn is actively running, restart production goose:
   ```bash
   sudo systemctl restart goose-serve
   ```
3. **Verify**:
   - honkd detects upstream goose restart (HTTP 404 / connection closed), synthesizes `activeRunId: null`, and marks the turn cancelled (`upstream_restart`).
   - Pigeon reports an honest cancelled / upstream_restart notice to the client (does not hang indefinitely).

---

## 3. Rollback Procedure

If issues occur during or after cutover, execute the following steps to revert traffic directly back to production goose (`:4080`):

1. **Immediate pigeon URL rollback via runtime systemd drop-in**:
   In the workstation configuration, `PIGEON_GOOSE_ACP_URL` is set in systemd `Environment=` and is NOT overridden in the `pigeon-daemon-start` script. Therefore, a runtime systemd drop-in immediately overrides the endpoint without waiting for a workstation rebuild:
   ```bash
   sudo systemctl edit --runtime pigeon-daemon
   ```
   Add the following configuration:
   ```ini
   [Service]
   Environment="PIGEON_GOOSE_ACP_URL=ws://127.0.0.1:4080/acp"
   ```
   Restart pigeon-daemon:
   ```bash
   sudo systemctl restart pigeon-daemon
   systemctl is-active pigeon-daemon
   # Expected: active
   ```
2. **Repoint stored database rows back to goose-serve**:
   (Since the auth token is unchanged, only `backend_endpoint` is updated):
   ```bash
   nix shell nixpkgs#sqlite -c sqlite3 -cmd '.timeout 5000' "$HOME/projects/pigeon/packages/daemon/data/pigeon-daemon.db" \
     "UPDATE sessions SET backend_endpoint='ws://127.0.0.1:4080/acp' WHERE backend_kind='goose-acp' AND backend_endpoint='ws://127.0.0.1:4280/acp';"
   ```
   Re-run the UPDATE after restart (idempotent):
   ```bash
   nix shell nixpkgs#sqlite -c sqlite3 -cmd '.timeout 5000' "$HOME/projects/pigeon/packages/daemon/data/pigeon-daemon.db" \
     "UPDATE sessions SET backend_endpoint='ws://127.0.0.1:4080/acp' WHERE backend_kind='goose-acp' AND backend_endpoint='ws://127.0.0.1:4280/acp';"
   ```
   Verify 0 rows point to `:4280`:
   ```bash
   nix shell nixpkgs#sqlite -c sqlite3 -cmd '.timeout 5000' "$HOME/projects/pigeon/packages/daemon/data/pigeon-daemon.db" \
     "SELECT count(*) FROM sessions WHERE backend_kind='goose-acp' AND backend_endpoint='ws://127.0.0.1:4280/acp';"
   # Expected: 0
   ```
3. **Stop honkd user service**:
   ```bash
   systemctl --user stop honkd
   ```
4. **Clear upstream goose session memory (Optional / Quiet Window)**:
   Upstream sessions created or touched through honkd may remain loaded in goose's memory. In a quiet window, an optional restart of `goose-serve` clears them:
   ```bash
   # Optional:
   sudo systemctl restart goose-serve
   ```
5. **Workstation configuration revert (at leisure)**:
   Revert `PIGEON_GOOSE_ACP_URL=ws://127.0.0.1:4080/acp` in `$HOME/projects/workstation/hosts/cloudbox/configuration.nix` and deploy:
   ```bash
   sudo nixos-rebuild switch
   ```
   After `nixos-rebuild switch` applies the reverted configuration, clean up the runtime drop-in:
   ```bash
   sudo systemctl revert pigeon-daemon
   sudo systemctl restart pigeon-daemon
   ```
6. **DISASTER RECOVERY WARNING**:
   **Never** restore the SQLite DB file backup (`.backup` file) during a routine rollback! Restoring the backup file clobbers all other sessions (e.g. active opencode sessions, routing state) created after the backup was taken. Rollback must strictly be performed via the inverse SQL `UPDATE` statement above.

---

## 4. Post-Cutover Monitoring and M1b Exit Criteria

Monitor the services over the next several days. Note that because pigeon has almost no ambient goose traffic, the operator must periodically drive test turns to exercise the path.

### 4.1 Pigeon Logs to Watch
Command:
```bash
journalctl --namespace=pigeon -u pigeon-daemon -f
```
Watch for:
- `[goose] honk disconnect mid-turn, retrying reconnect and resend with backoff`: a connection drop was caught and recovery started (expected around honkd restarts).
- `[goose] honk reconnect attempt failed, backing off` / `honk resend disconnected, backing off and retrying reconnect`: recovery in progress; repeated lines are normal while honkd starts.
- `[goose] honk resend failed with non-disconnect error`: honkd answered the resend with an error; the human was told.
- `[goose] honk reconnect budget exhausted, reporting lost contact`: **a receipt was lost** (the turn may have finished in honkd). Investigate honkd's journal.
- `[goose] goose connection closed underneath us`: Indicates socket dropped during idle or between turns.
- `[goose] goose runner: could not report finished turn` / `turn continuation threw`: Indicates finish-turn reporting error.
- `error_kind: goose-turn-disconnected` or `DisconnectedDuringTurn`: Indicates unrecoverable turn failure on connection loss.

*(Note: Strings `steer-unconsumed` and `not_delivered` were removed as they do not appear in the codebase).*

### 4.2 honkd Logs to Watch
Command:
```bash
journalctl --user -u honkd -f
```
Watch for:
- Any log records with `"level":"ERROR"` or `"level":"WARN"` in honkd's JSON tracing output.

*(Note: Close code `4001`, `_honk/gap`, synthesized replies, and `held-session limit reached` are JSON-RPC wire-only protocol elements and do not emit distinct log strings in M1a; structured warning logs for these events are tracked as M1c bead `honk-oi2.6`).*

### 4.3 M1b Exit Verdict
M1b is declared complete when pigeon runs on honkd in production for several days with zero lost receipts across all driven turns.
