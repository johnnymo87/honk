# honk handoff (living document)

Executing `docs/plans/2026-10-10-honk-m0-plan.md` with subagent-driven development
(implementer subagent per task; then spec + code-quality review; fixes re-reviewed).
Branch `m0` (pushed to origin). No PR yet; open one at the end of M0 (load the
shepherding-pull-requests skill first). CI = `.github/workflows/ci.yml`.

## Author decisions (2026-10-10)
- Author is busy; agent uses best judgement, author reviews afterwards.
- P1 rubric written by agent (`docs/spike/m0-findings.md` §P1); author runs the goose day later. Probe/build work does not wait on it.
- Sandbox model traffic goes through claude-failover-proxy like production, with a distinct session header (`x-honk-sandbox-session`) so cfp does not pin/collide.
- Build honkd AND carry goose patches in the fork (johnnymo87/goose, branch `patched/v1.54.0`, local `~/projects/goose-patched`). Upstream proposal only after practice. Goose *deploy* (workstation pkgs bump, restart goose-serve) is author-gated.

## Status
- [x] T1+T2 nix shell, workspace, CI (c7cb527, 5d01f88, 3827fe0)
- [x] T3 hardened sandbox (d43ecc0, 36f8fdf). Unit `honk-goose-sandbox` on :4180, `GOOSE_PATH_ROOT=~/.local/state/honk-sandbox`, allowlisted config (developer extension only). `scripts/goose-sandbox-check.sh` must print `sandbox OK`.
- [x] T4 honk-upstream HTTP+SSE client (5984d8e), live smoke passes.
- [x] T5 WS manual check
- [x] T6 P2: Turn survives disconnects; clean reattach 409 is 0-1ms, half-open persists; frame loss in dead socket requires post-reattach reconciliation.
- [x] T7 goose patch notes (d3ec06a + later additions in docs/spike/goose-patches.md)
- [x] T8 P3: Same-conn reload mid-run safe, agent preserved; replay re-sends committed steps of the in-flight turn (never the uncommitted message), so clients must dedupe/replace by messageId; CWD rewritten (honkd must pass stored CWD); cross-conn reload does not redirect live stream.
- [x] T9 P4: No SSE redelivery or load re-send of pending permissions; same-conn survives 90s listener drop and finishes; lost frames unrecoverable; teardown answers execute nothing.
- [x] T10 P5: cancel works from any conn (54-79ms, leaks orphan shell descendants); $/cancel_request ignored; cancel while parked requires answering cancelled; late steer consumed via exit_chat reset; restart returns 404, export works before load.
- [x] T11 P6: live text/thought messageIds and toolCallIds stable across live/replay/export (block id = messageId; tool blocks = toolCallId, since tool_call frame messageId is not stable); replayTail:1 covers last turn (source + unrecorded multi-turn run; fixtures single-turn); goose ~0.5-1.7 MB/session RSS, 0 child procs (unrecorded); honkd TTL 15m, max 16 conns.
- [x] T12 hydra (optional): skipped (isolation cost not worth it given P2-P6 settled the design)
- [x] T13 findings, design amendments, M1 plan + goose-patch plan (66dae62, 86ff5a3, a692b40, 84642ce, bd505ce)

## Carry-forward from reviews
- Wire learnings: modes `auto|approve|smart_approve|chat` (default auto); `configOptions` array with `options:[{name,value}]`, `currentValue`; cheap model `claude-haiku-4-5@20251001` via `session/set_config_option {configId:"model"}`; run start/end visible as `session_info_update` `_meta.goose.activeRunId` set/null; prompt reply has `stopReason` + usage.

## Gotchas
- Production goose (:4080, system unit `goose-serve`) is off-limits; never run goose without absolute `GOOSE_PATH_ROOT`.
- Public repo: fixtures must be grep-checked for secret, `/home/dev`, project id (compare to /run/secrets/google_cloud_project without printing), org names.
- Bash tool 120s default timeout; long probe waits via longer timeouts or systemd-run.
- Pigeon likely bug (unverified at runtime): its goose runner assumes turns survive disconnect; on WS they are cancelled. Report to author; not fixed.

## Post-M0 (2026-10-11)
- M0 probes done; findings + amendments A1-A16 in docs/spike/m0-findings.md; design revised (86ff5a3).
- Plans written and reviewed (adversarial x2 + oracle): docs/plans/2026-10-11-goose-patches-plan.md (verdict: execute) and docs/plans/2026-10-11-honk-m1-plan.md (split M1a/M1b/M1c; honkd REQUIRES patched goose; round-2 fixes applied).
- Order: execute the goose patch plan first (Tasks 0-11; stop at published release patched-1.54.0.3 + author-only deploy runbook), then M1a (A0 depends on the patch release).
- Org-name leak: commits 133bf83..ee1781b (pushed, public) contained the org name in redact.rs; fixed forward in b2443a5 (needles now in ~/.config/honk/redact.txt). History NOT rewritten — author decides.
- goose bug found (P5a): cancel orphans shell descendants — affects production goose; patch 1 of the patch plan fixes it.
- Open questions for author: GOOSE_STATE_MACHINE in production? widened extension set on re-activation acceptable? in-file Apache notice on shell.rs?
