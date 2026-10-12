# M1b Operator Runbook: pigeon Goose Traffic Cutover to honkd

> **AUTHOR-GATED**: This runbook must ONLY be executed by an operator/human after
> explicit author approval. Do NOT execute this document automatically.

---

## 1. Context and Verified Topology

### 1.1 Architecture
Before cutover:
```
pigeon-daemon (systemd, User=$USER)
  │
  ├─ [ws://127.0.0.1:4080/acp + /run/secrets/pigeon_goose_acp_token]
  ▼
goose-serve (systemd system service, 127.0.0.1:4080)
```

After cutover:
```
pigeon-daemon (systemd, User=$USER)
  │
  ├─ [ws://127.0.0.1:4280/acp + <honkd downstream secret>]
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
- **honkd service**:
  - Systemd user service running as `$USER` (the user that runs pigeon and goose).
  - B4 (workstation PR) provides this user unit (`systemd.user.services.honkd`).
  - Service unit requirements:
    - User unit (`honkd.service`).
    - ExecStart: `honkd --listen 127.0.0.1:4280 --upstream http://127.0.0.1:4080/acp --allow-production-upstream --upstream-secret-file /run/secrets/pigeon_goose_acp_token`
    - Restart policy: `Restart=always` with `RestartSec=2s`.
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
    RestartSec=2s

    [Install]
    WantedBy=default.target
    ```
  - State dir: honkd defaults to `$HOME/.local/state/honk` (or `$XDG_STATE_HOME/honk`), generating the downstream secret at `$HOME/.local/state/honk/secret` (mode `0600`, 64 hex characters). Do not pass a divergent `--state-dir`.
  - CLI `honk`:
    - Defaults to `--url http://127.0.0.1:4280/acp` (or `$HONK_URL`).
    - Resolves secret automatically from `$HOME/.local/state/honk/secret` (or `$XDG_STATE_HOME/honk/secret`), `$HONK_SECRET`, or explicit `--secret`.
    - With honkd running with its default state dir, CLI secret resolution is zero-config.
- **pigeon daemon**:
  - System unit: `pigeon-daemon` (User `$USER`, the user that runs pigeon and goose).
  - Namespace journal: logs are in the `pigeon` namespace, accessed via `journalctl --namespace=pigeon -u pigeon-daemon`.
  - Database: `$HOME/projects/pigeon/packages/daemon/data/pigeon-daemon.db`.
  - Storage table: `sessions` (columns: `session_id`, `backend_kind`, `backend_endpoint`, `backend_auth_token`).
  - ACP goose sessions have `backend_kind = 'goose-acp'`.
  - Precedence in `index.ts`: a session row's `backend_endpoint` and `backend_auth_token` take precedence over environment variables `PIGEON_GOOSE_ACP_URL` and `PIGEON_GOOSE_ACP_TOKEN`.
  - Today there are 0 `goose-acp` rows in `pigeon-daemon.db` (count verified = 0).
  - Workstation repo: `PIGEON_GOOSE_ACP_URL` and daemon start script are managed via NixOS configuration in `$HOME/projects/workstation`.
  - SQLite CLI: `sqlite3` is not on standard PATH; run via `nix shell nixpkgs#sqlite -c sqlite3`.
- **pigeon branch requirement**:
  - Pigeon must run the B1 branch (honk-aware runner: transmits `_meta.honk.client_req_id`, reconnects and resends once on honk disconnect) before or at cutover.
- **honkd operational limits**:
  - `--max-sessions 16`: M1a has no LRU eviction. Pigeon runner connections stay attached for the daemon lifetime, keeping sessions held. Any session creation or activation attempt beyond 16 returns JSON-RPC error `-32000` (`honkd: held-session limit reached`).
  - `--idle-ttl-secs 900`: 15-minute idle TTL for unheld/unviewed sessions.

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
3. Verify honk flake package builds cleanly from the repository checkout and binaries respond to `--help`:
   ```bash
   nix build .#honkd --out-link /tmp/honkd-result
   /tmp/honkd-result/bin/honkd --help
   /tmp/honkd-result/bin/honk --help
   ```
   *(Note: keep `/tmp/honkd-result` available during cutover, or use `honk` installed on PATH via B4).*
4. Verify pigeon daemon is running the B1 branch code (with honk client request id and reconnect support).
5. **Abort criteria**:
   - `goose-serve` is inactive or lacks stream features.
   - Secret file is missing, unreadable, or shorter than 32 characters.
   - `nix build` fails.
   - Pigeon B1 branch is not deployed.
   **Action**: Do not proceed. No services have been started or altered. Fix the prerequisite before re-attempting.

---

### Step 2: Start honkd and Verify Feature Gate

1. Start the honkd user service (managed via workstation user service unit `honkd` or spawned via user systemd):
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
3. Check the generated downstream secret in the default state directory:
   ```bash
   test -f "$HOME/.local/state/honk/secret" && [ $(wc -c < "$HOME/.local/state/honk/secret") -ge 32 ] && echo "Secret OK"
   # Expected: Secret OK
   ```
4. Verify `honk list` can talk to honkd and read upstream sessions:
   Prefer `honk` from the installed `honkd` package (provided by B4 on PATH, or via `nix run .#honkd` or `/tmp/honkd-result/bin/honk`). Secret resolution is automatic from default `$HOME/.local/state/honk/secret`:
   ```bash
   honk list --url http://127.0.0.1:4280/acp
   # Or using the nix build result if honk is not yet on PATH:
   /tmp/honkd-result/bin/honk list --url http://127.0.0.1:4280/acp
   ```
   **Expected output**:
   - List of current goose sessions (or exit code 0 with empty output if no sessions exist).
5. **Abort criteria**:
   - honkd exits with code 1 and stderr `honkd: upstream goose lacks Acp-Stream-Features takeover+replay; run patched-1.54.0.3 or pass --allow-unpatched-upstream`.
   - `honk list` returns connection refused or 401 unauthorized.
   **Action**: Stop honkd (`systemctl --user stop honkd`). Traffic has not been cut over yet; goose and pigeon remain untouched. Investigate logs (`journalctl --user -u honkd -n 50`).

---

### Step 3: Quiet Window Check

1. Verify no pigeon goose turn is in flight by checking pigeon daemon journal logs:
   ```bash
   journalctl --namespace=pigeon -u pigeon-daemon --since "10 min ago" --no-pager
   ```
   Inspect for recent goose turn lifecycle markers:
   - Turn dispatch: `[command-ingest] delivered commandId=... adapter=goose-acp`
   - Turn runner: `[goose] goose steer found no active run`, `[goose] goose rejected the prompt as busy`, `[goose] goose acp connected`
   - Turn completion: calls to `/stop` (e.g. `[stop] quieted sessionId=...` or outbox queuing)
   - Abnormal termination: `[goose] goose runner: could not report finished turn`, `[goose] goose runner: turn continuation threw`, `[goose] goose connection closed underneath us`
   Confirm that every started turn has a corresponding completion and no turn is currently outstanding.
2. Verify pigeon DB inbox has no pending commands and outbox has no queued messages:
   ```bash
   nix shell nixpkgs#sqlite -c sqlite3 "$HOME/projects/pigeon/packages/daemon/data/pigeon-daemon.db" \
     "SELECT count(*) FROM inbox WHERE status != 'done';"
   # Expected: 0
   nix shell nixpkgs#sqlite -c sqlite3 "$HOME/projects/pigeon/packages/daemon/data/pigeon-daemon.db" \
     "SELECT count(*) FROM outbox WHERE state = 'queued';"
   # Expected: 0
   ```
3. **Note on `honk list` limitation**:
   Do **not** rely on `honk list` to detect in-flight turns:
   - `honkd`'s `hub.rs::is_session_running` is a stub that always returns `false` today (the `running` column in `honk list` is always absent/false).
   - Prior to cutover, `honkd` is not in the traffic path between pigeon and goose, so honkd has zero visibility into active turns anyway.
   *(Note: pigeon has ~0 ambient goose ACP traffic, so finding an idle quiet window is immediate).*
4. **Abort criteria**:
   - An active turn is in progress in pigeon or goose, or pending inbox/outbox items exist.
   **Action**: Do not proceed with cutover. Wait until the in-flight turn finishes and all journals/inboxes are clear. If honkd was started in Step 2, keep it running or stop it (`systemctl --user stop honkd`) while waiting.

---

### Step 4: Backup Pigeon DB, Check Count, and Repoint Rows

1. Create a point-in-time SQLite backup of `pigeon-daemon.db` (for catastrophic disaster recovery only):
   ```bash
   BACKUP_PATH="$HOME/projects/pigeon/packages/daemon/data/pigeon-daemon.db.bak-$(date +%Y%m%d%H%M%S)"
   nix shell nixpkgs#sqlite -c sqlite3 "$HOME/projects/pigeon/packages/daemon/data/pigeon-daemon.db" ".backup '$BACKUP_PATH'"
   test -f "$BACKUP_PATH" && echo "Backup created: $BACKUP_PATH"
   ```
2. Query and record the number of existing `goose-acp` rows pointing to production goose:
   ```bash
   nix shell nixpkgs#sqlite -c sqlite3 "$HOME/projects/pigeon/packages/daemon/data/pigeon-daemon.db" \
     "SELECT count(*) FROM sessions WHERE backend_kind='goose-acp' AND backend_endpoint='ws://127.0.0.1:4080/acp';"
   ```
   *Note*: The count is expected to be `0` because pigeon currently has virtually no persisted goose ACP sessions.
3. Repoint existing `goose-acp` session rows to honkd:
   ```bash
   HONKD_SECRET="$(cat "$HOME/.local/state/honk/secret")"
   [ -n "$HONKD_SECRET" ] || { echo "ERROR: HONKD_SECRET is empty or unreadable from $HOME/.local/state/honk/secret"; exit 1; }
   nix shell nixpkgs#sqlite -c sqlite3 "$HOME/projects/pigeon/packages/daemon/data/pigeon-daemon.db" \
     "UPDATE sessions SET backend_endpoint='ws://127.0.0.1:4280/acp', backend_auth_token='$HONKD_SECRET' WHERE backend_kind='goose-acp' AND backend_endpoint='ws://127.0.0.1:4080/acp';"
   ```
4. Confirm no session rows remain pointing directly to `:4080`:
   ```bash
   nix shell nixpkgs#sqlite -c sqlite3 "$HOME/projects/pigeon/packages/daemon/data/pigeon-daemon.db" \
     "SELECT count(*) FROM sessions WHERE backend_kind='goose-acp' AND backend_endpoint='ws://127.0.0.1:4080/acp';"
   # Expected: 0
   ```
5. **Abort criteria**:
   - Database backup fails.
   - SQLite UPDATE fails with syntax error or database locked.
   **Action**: Stop honkd (`systemctl --user stop honkd`). If any rows were updated, execute Section 3 (Rollback Procedure) Step 1 to restore rows back to `:4080`.

---

### Step 5: Switch Pigeon Environment and Restart Pigeon

1. Apply the workstation change that repoints pigeon's default goose endpoint and token:
   - `PIGEON_GOOSE_ACP_URL=ws://127.0.0.1:4280/acp`
   - `PIGEON_GOOSE_ACP_TOKEN` loaded from `$HOME/.local/state/honk/secret`
   - Rebuild system / activate via NixOS / workstation deployment workflow (`nixos-rebuild switch`).
2. Restart the pigeon daemon:
   ```bash
   sudo systemctl restart pigeon-daemon
   systemctl is-active pigeon-daemon
   # Expected: active
   ```
3. Inspect pigeon daemon logs in its journal namespace:
   ```bash
   journalctl --namespace=pigeon -u pigeon-daemon -n 50 --no-pager
   ```
   Verify pigeon started cleanly and connected or registered the goose capability.
4. **Abort criteria**:
   - `pigeon-daemon` fails to start or continuously crash loops.
   - Pigeon logs report `PIGEON_GOOSE_ACP_TOKEN missing or unreadable`.
   **Action**: Go immediately to Section 3 (Rollback Procedure) to revert pigeon environment variables, deploy workstation configuration, restart pigeon daemon, and stop honkd.

---

### Step 6: Live Verification

Because pigeon receives little natural goose traffic, verification requires deliberately driven turns.

#### 6A: Standard Turn and Receipt Delivery
1. Trigger a goose turn via pigeon (e.g., via Telegram or test prompt to a pigeon goose session).
2. Monitor turn progress in real time via `honk tail`:
   ```bash
   honk tail <session-id> --url http://127.0.0.1:4280/acp
   ```
3. **Verify**:
   - Live stream chunks appear in `honk tail`.
   - The turn completes cleanly.
   - The final receipt arrives in Telegram / pigeon client with the expected output text.

#### 6B: Honkd Mid-Turn Restart Resilience
1. Start a longer goose turn via pigeon.
2. While the turn is actively running, restart honkd:
   ```bash
   systemctl --user restart honkd
   ```
3. **Verify**:
   - Pigeon B1 client detects disconnect, reconnects to `127.0.0.1:4280`, and re-attaches with the same `_meta.honk.client_req_id`.
   - honkd restores the session, rebuilds turn progress, and continues relaying to goose.
   - The turn finishes successfully.
   - Exactly **one** full receipt arrives in Telegram / pigeon client with no duplicate text or dropped responses.

---

## 3. Rollback Procedure

If issues occur during or after cutover, execute the following steps to revert traffic directly back to production goose (`:4080`):

1. **Repoint stored database rows back to goose-serve**:
   ```bash
   GOOSE_TOKEN_FILE="/run/secrets/pigeon_goose_acp_token"
   GOOSE_SECRET="$(cat "$GOOSE_TOKEN_FILE")"
   [ -n "$GOOSE_SECRET" ] || { echo "ERROR: GOOSE_SECRET is empty or unreadable from $GOOSE_TOKEN_FILE"; exit 1; }
   nix shell nixpkgs#sqlite -c sqlite3 "$HOME/projects/pigeon/packages/daemon/data/pigeon-daemon.db" \
     "UPDATE sessions SET backend_endpoint='ws://127.0.0.1:4080/acp', backend_auth_token='$GOOSE_SECRET' WHERE backend_kind='goose-acp' AND backend_endpoint='ws://127.0.0.1:4280/acp';"
   ```
   Verify repoint count:
   ```bash
   nix shell nixpkgs#sqlite -c sqlite3 "$HOME/projects/pigeon/packages/daemon/data/pigeon-daemon.db" \
     "SELECT count(*) FROM sessions WHERE backend_kind='goose-acp' AND backend_endpoint='ws://127.0.0.1:4280/acp';"
   # Expected: 0
   ```
2. **Revert pigeon environment variables in workstation configuration**:
   - Restore `PIGEON_GOOSE_ACP_URL=ws://127.0.0.1:4080/acp`.
   - Restore `PIGEON_GOOSE_ACP_TOKEN` pointing to `/run/secrets/pigeon_goose_acp_token` (`$GOOSE_TOKEN_FILE`).
   - Deploy workstation configuration.
3. **Restart pigeon daemon**:
   ```bash
   sudo systemctl restart pigeon-daemon
   systemctl is-active pigeon-daemon
   ```
4. **Stop honkd user service**:
   ```bash
   systemctl --user stop honkd
   ```
5. **DISASTER RECOVERY WARNING**:
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
- `error_kind: goose-turn-disconnected`: Indicates an unhandled socket drop without recovery.
- `DisconnectedDuringTurn`: Indicates turn failure on connection loss.
- `steer-unconsumed`: Indicates steering message was ignored or lost.
- `not_delivered`: Indicates prompt was rejected or never reached execution.

### 4.2 honkd Logs to Watch
Command:
```bash
journalctl --user -u honkd -f
```
Watch for:
- Any `ERROR` level logs.
- `4001` close codes: Indicates slow-viewer eviction when downstream cannot keep up.
- `_honk/gap`: Indicates stream sequence gap requiring export rebuild.
- Synthesized replies: Indicates goose response was lost and had to be reconstructed.
- `honkd: held-session limit reached`: Indicates session count reached `--max-sessions 16` cap without LRU cleanup.

### 4.3 M1b Exit Verdict
M1b is declared complete when pigeon runs on honkd in production for several days with zero lost receipts across all driven turns.
