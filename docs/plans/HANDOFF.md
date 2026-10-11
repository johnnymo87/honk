# honk M0 handoff (living document)

Executing `docs/plans/2026-10-10-honk-m0-plan.md` with subagent-driven development
(implementer subagent per task; then spec + code-quality review; fixes re-reviewed).
Branch `m0` (pushed to origin). No PR yet; open one at the end of M0 (load the
shepherding-pull-requests skill first). CI = `.github/workflows/ci.yml`.

## Author decisions (2026-10-10)
- Author is busy; agent uses best judgement, author reviews afterwards.
- P1 rubric written by agent (`docs/spike/m0-findings.md` §P1); author runs the goose day later. Probe/build work does not wait on it.
- Sandbox model traffic goes through claude-failover-proxy like production, with a distinct session header (`x-honk-sandbox-session`) so cfp does not pin/collide.
- Build honkd AND carry goose patches in the fork (johnnymo87/goose, branch `patched/v1.54.0`, local `/home/dev/projects/goose-patched`). Upstream proposal only after practice. Goose *deploy* (workstation pkgs bump, restart goose-serve) is author-gated.

## Status
- [x] T1+T2 nix shell, workspace, CI (c7cb527, 5d01f88, 3827fe0)
- [x] T3 hardened sandbox (d43ecc0, 36f8fdf). Unit `honk-goose-sandbox` on :4180, `GOOSE_PATH_ROOT=~/.local/state/honk-sandbox`, allowlisted config (developer extension only). `scripts/goose-sandbox-check.sh` must print `sandbox OK`.
- [x] T4 honk-upstream HTTP+SSE client (5984d8e), live smoke passes.
- [x] T5 WS manual check
- [x] T6 P2: Turn survives disconnects; clean reattach 409 is 0-1ms, half-open persists; frame loss in dead socket requires post-reattach reconciliation.
- [ ] T7 goose patch notes
- [x] T8 P3: Same-conn reload mid-run safe, agent preserved, no replay duplicates; CWD rewritten (honkd must pass stored CWD); cross-conn reload does not redirect live stream.
- [x] T9 P4: No SSE redelivery or load re-send of pending permissions; same-conn survives 90s listener drop and finishes; lost frames unrecoverable; teardown answers execute nothing.
- [x] T10 P5: cancel works from any conn (54-79ms, leaks orphan shell descendants); $/cancel_request ignored; cancel while parked requires answering cancelled; late steer consumed via exit_chat reset; restart returns 404, export works before load.
- [x] T11 P6: Message IDs and toolCallIds completely stable (block id = messageId); replayTail:1 covers last turn; goose ~0.5-1.7 MB/session RSS, 0 child procs; honkd TTL 15m, max 16 conns.
- [ ] T12 hydra (optional)  - [ ] T13 findings + M1 plan + goose-patch plan

## Carry-forward from reviews
- Wire learnings: modes `auto|approve|smart_approve|chat` (default auto); `configOptions` array with `options:[{name,value}]`, `currentValue`; cheap model `claude-haiku-4-5@20251001` via `session/set_config_option {configId:"model"}`; run start/end visible as `session_info_update` `_meta.goose.activeRunId` set/null; prompt reply has `stopReason` + usage.

## Gotchas
- Production goose (:4080, system unit `goose-serve`) is off-limits; never run goose without absolute `GOOSE_PATH_ROOT`.
- Public repo: fixtures must be grep-checked for secret, `/home/dev`, project id (compare to /run/secrets/google_cloud_project without printing), org names.
- Bash tool 120s default timeout; long probe waits via longer timeouts or systemd-run.
- Pigeon likely bug (unverified at runtime): its goose runner assumes turns survive disconnect; on WS they are cancelled. Report to author; not fixed.
