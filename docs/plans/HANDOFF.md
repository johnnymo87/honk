# honk handoff (living document)

Executing `docs/plans/2026-10-10-honk-m0-plan.md` with subagent-driven development
(implementer subagent per task; then spec + code-quality review; fixes re-reviewed).
M0 merged into main via PR #1 (squash) on 2026-10-12. CI = `.github/workflows/ci.yml`.

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
- Pigeon WS disconnect bug: confirmed at runtime and fixed by honest reporting in pigeon PR #168 (merged + deployed 2026-10-12): turns still die on WS close; pigeon now says so (`error_kind: goose-turn-disconnected`). The HTTP+SSE transport switch (pigeon bead `pigeon-82sa`) is DEFERRED in favour of routing pigeon through honkd.

## Post-M0 (2026-10-11)
- M0 probes done; findings + amendments A1-A16 in docs/spike/m0-findings.md; design revised (86ff5a3).
- Plans written and reviewed (adversarial x2 + oracle): docs/plans/2026-10-11-goose-patches-plan.md (verdict: execute) and docs/plans/2026-10-11-honk-m1-plan.md (split M1a/M1b/M1c; honkd REQUIRES patched goose; round-2 fixes applied).
- Order: execute the goose patch plan first (Tasks 0-11; stop at published release patched-1.54.0.3 + author-only deploy runbook), then M1a (A0 depends on the patch release).
- Org-name leak: commits 133bf83..ee1781b (pushed, public) contained the org name in redact.rs; fixed forward in b2443a5 (needles now in ~/.config/honk/redact.txt). History NOT rewritten — author decides.
- goose bug found (P5a): cancel orphans shell descendants — affects production goose; patch 1 of the patch plan fixes it.
- Open questions — all resolved 2026-10-12: GOOSE_STATE_MACHINE is OFF in production (env-only switch, `state_machine/mod.rs` `enabled()`, unset in goose-serve); extension set does not widen on re-activation (A10); Apache §4(b) modification notices are being added in the fork by a separate session (see "Current state").

## Goose patch track (2026-10-11) — branch `goose-patch-track` (worktree ../honk-patch-track)
- Fork worktree: ~/projects/goose-patched-track3 (local branch patched/v1.54.0). Pushed fork/patched/v1.54.0 = e41c1ec30; tag patched-1.54.0.3 pushed; release run 38116818991.
- Fork build env: the fork flake's rust-overlay lacks 1.99.0 -> `nix develop --no-write-lock-file --override-input rust-overlay github:oxalica/rust-overlay/<honk flake.lock rev>`; goose-cli default features need LIBCLANG_PATH/BINDGEN_EXTRA_CLANG_ARGS/cmake from nixpkgs (llama-cpp). goose lib tests need the release feature set (see patched-release.yml $F).
- Tasks 0-8 done (honk commits: Task 1 scripts, Task 7 honk-upstream ids/resume/features, Task 8 probes + fixes). Pre-tag gate PASS: P2e x2 (after control fix), P2f x3, P4b' x2, P5a' x2, pigeon WS probes x3 on the patched sandbox unit `honk-goose-sandbox-patched` (:4181, ~/.local/state/honk-sandbox-patched).
- Tasks 9-11 DONE: release run 38116818991 green; assets verified; release binary packaged at ~/.local/state/honk-goose-bin/patched-1.54.0.3/pkg (GC root); all probes + pigeon WS qualification PASS on it (logs /tmp/opencode/pigeon-qual-release-*.log); fixtures committed. `honk-goose-sandbox-patched` left running on the release binary for M1.
- Production deploy of patched-1.54.0.3 is AUTHOR-ONLY (plan "Production deploy" section); gate evidence = Task 10 pigeon-qual-release logs.

## Open PR status / next (2026-10-12)
- M0 PR #1 merged (squash) on 2026-10-12.
- `goose-patch-track` merged as PR #2 on 2026-10-12.
- Branch `m1a` now based on `main`, executes in worktree ../honk-m1a per docs/plans/2026-10-11-honk-m1-plan.md; patched sandbox `honk-goose-sandbox-patched` on :4181 runs the release binary.

## M1a progress (branch m1a, worktree ../honk-m1a)
- A0 preflight PASS (patched :4181 advertises `takeover, replay`; upstream tests green). Branch is `m1a`, not the plan's `m1`.
- A0.5 DONE: 40a4dd8 + review fixes c1d53ba, 15e1c25 (abort-on-drop, generation-guarded resubscribe, no URLs in errors).
- A1 DONE: stub-model spike, all 6 checks pass on patched-1.54.0.3 → stub-goose is the main test upstream (39dde5a, ece7451; docs/spike/m1-stub-model.md).
- A2 DONE: StubGoose API, FakeGoose (fidelity-checked vs vendored acp-http), TcpProxy, CI live-stub job (63fc931, 72c9107). Live job unverified until CI runs on a pushed PR/branch.
- A3 DONE: honk-wire (block keys, cursors, reducer, export mapping w/ goose visibility filter) + honkd::rpc (3c34fd7).
- A4/A5 DONE: locked atomic state file; durable per-session log (4b06ade, 35947e4, fixes 33b0644: 0600 at create, corrupt middle line refuses open, transactional mutate).
- A6 DONE: pure turn core (7970a10, fixes 6ce9ae2, e5b1ceb: no cancel_pending leak across turns; steer reply/consume ordering races; consumed-steer ids kept until next run start).
- A7 DONE: honkd binary, WS downstream (token/header auth, 401/406), control conn, patched-upstream gate (daa5783; fixes 8a15413 out-of-band 4001 eviction, harness binds :0).
- A8 DONE: per-session upstream connections persisted before use; session/list with _meta.honk (4510a35; fix 0ab3fb0 routing: session/close local, $/cancel_request dropped).
- A9 DONE: session actor + native prompt passthrough + receipt text from side-connection export (53db39c; fixes 9acef0b, c4fc3fd: stamped fan-out after record, inline ordered POSTs, multi-session viewers, steer/forward id remap, prompt-send failure via turn core, persisted inflight mirrors turn core).
- A10 DONE: real pigeon GooseAcpClient (throwaway worktree, not pushed) → honkd → :4181: end_turn, receipt "PIGEON_OK". Extension set after re-activation does NOT widen (StubGoose config developer+todo: session/load and prompt-on-fresh-conn both keep the 5 developer tools) — resolves that open question (500625a, 76ddb4e; docs/spike/m1-exit.md holds the vitest file for the M1b pigeon PR).
- A11 DONE: session/load cursor attach (reset/delta), legacy vs honk-aware classes, slow-viewer 4001 eviction, init_result persisted in HeldSession (a493f92). Carry-over nits for A12: is_duplicate fast path when no replayed heads; two_viewers_converge should assert text == "done"; log test should assert exact line count; legacy test assert no `_honk/` methods on raw frames (17713e0).
- A12 DONE: takeover+Last-Event-ID resume, export rebuild on gap, restart recovery, degraded mode (1c1d37f; review fixes abbc295 and follow-up: backpressured ReplayBatch on rebuild, response processing in Rebuilding, ledger race finalize parking, trailing user text export check, non-destructive steer probe active-run rule [note: goose dispatches requests concurrently via cx.spawn, so idle probe can overtake a late-arriving prompt reply; resolved by grace window awaiting stream reply before synthesizing end_turn], and -32000 rejection for inconsistent inflight crids).
- Process: implementer subagent per task with /tmp/opencode/m1a-ground-rules.md (recreate from the HANDOFF ground rules if /tmp was wiped), then code-reviewer; fixes as new commits.
- A12 DONE: watchdog/takeover+Last-Event-ID reattach, gap → _honk/gap + rebuild at turn end (idle → now), export rebuild as new epoch via backpressured replay, restart recovery, replies sidecar, not_delivered, degraded 409 backoff (17713e0 nits, 1c1d37f; fixes abbc295, d9cc015, 95cc2ea). Gap-with-active-turn rule: non-destructive steer probe with a bogus expectedRunId; idle probe indicates run ended, but because goose dispatches concurrently via cx.spawn an idle probe response can overtake a late prompt reply delayed by usage updates; therefore, idle probe triggers rebuild export followed by a grace window (default 2s) awaiting the stream reply before synthesizing end_turn. Carry-over to A13: probe POST failure is ignored (retry with backoff; 404 → Gone).
- A13 DONE: durable pending requests, re-offered on every attach, steer-probe retry/404 handling (da21169). Review fixes:
  1. Failed `up.respond` on non-gone (e.g. 500) keeps request pending in memory and state, does not broadcast `request_resolved`, and re-offers request to answering viewer with 1 bounded retry.
  2. In-memory pending list retains request until `up.respond` succeeds; on 404/Gone `mark_pending_stale` broadcasts `stale` for that request too.
  3. Cancel handler broadcasts `reason: "cancelled"` (was "answered").
  4. Handled `s.mutate` failures on new `PendingRequest` (persist-before-offer: on error, log and return early without adding to memory or offering), cancel clear, and stale clear.
  5. Respond-then-remove crash window documentation: crash after respond but before removal → re-offer after restart → second answer sent to goose. Verified in goose source (`vendor/agent-client-protocol-http/src/http_server.rs:183-186`, `crates/goose/src/acp/server.rs:1606-1632`, `vendor/agent-client-protocol-http/src/connection.rs:373-383`): POST /acp returns 202 Accepted, and ACP drops unmatched response IDs silently without error. Respond-then-remove is strictly safer than remove-then-respond (which could park goose forever if honkd crashed).
- Author decision 2026-10-11: m0 history rewrite NOT needed (org name in history accepted) — M0 PR no longer blocked on it.
- A13 DONE: durable pending server→client requests, first-answer-wins, re-offer on every attach, stale hook; failed respond keeps it pending; respond-then-remove crash window documented (da21169, d79a96a).
- A14/A15 DONE: ledger cross-session check, Done on every reply, busy rejection uncached, boot prune (24h/1000 Done; orphan InFlight resolved), Gone resolves InFlight; prompt POST failures classified (status/connect → retry-safe; ambiguous → stay InFlight, never not_delivered while live); raw steer passthrough; cancel answers pending + early-cancel (2baf988, b96e21f, 898aeb8, dced49a, 74c934e, c0264c9, 2fa61ee).
- CORRECTION to A12: goose dispatches prompt and custom requests concurrently (dispatch.rs cx.spawn), so the steer probe is NOT a strict barrier: idle ⇒ run ended, but the reply can trail the probe answer. Synthesis now waits for the rebuild + a grace window (--gap-synthesis-grace-ms, default 2000); a later real reply is logged, not double-sent (a0f2a1c).
- A16 DONE: lazy activation (stored cwd from session/list, conn persisted first, C1 replay discard keeping requests/conn events, export → new epoch), coalesced via watch receiver + guard, --max-sessions counts held+activating+creating, idle TTL eviction (DELETE, retired_epochs), undelivered msgs re-dispatched in order after eviction (7ef2b36; fixes ffa3bb6, bdc3c5a). Note: an unheld session with pending requests is unreachable in M1a (pending ⇒ not idle).
- A17 DONE: goose restart (404) → interrupt "upstream_restart" (ledger Done cancelled), pending stale, synthesized activeRunId:null to all viewers, no resend, reactivation in a spawned task (token-guarded, re-entrancy-safe, wins over an in-flight gap rebuild), staggered 500 ms for viewed sessions, lazy otherwise; permanent failure → _honk/evicted + cleanup; control conn backoff reconnect + feature re-check (unpatched → -32000 refusal) + byte watchdog (4b2427a; fixes 3911ac9, e8f0526).
- A18 DONE: crates/honk — renderer (printed/shadow, suffix, rebuilt marker once), `honk tail` (cursor resume, backoff reset on load, fatal errors exit, exit_on_turn_end per-connection), `honk list`; CI live-stub sets HONKD_BIN (1bcd4f4; fixes 56eb473). Workspace: 282 tests pass with HONK_GOOSE_BIN.
- A19 DONE: live exit tests on :4181 all pass — e1 tail, e2 kill9 delta resume, e3 goose restart (the one sanctioned sandbox restart), e4 permission across honkd restart, delete_frees_parked_run (DELETE alone frees a parked run: hypothesis holds), takeover_and_resume_live (ca3df92, 2b8f451, 15b00d8; docs 6e3a950, 7adec19, a897221).
- Live-found bug fixed: restored-inflight delivery check now uses prompt-sequence indexing (persisted client-prompt count N; export's Nth client prompt decides delivered/not_delivered/delivery_unknown; delivered wait bounded by steer probe + grace) (6e611b6, c2c7760; replaces 369e983).
- Finding for M1b: after a goose restart mid-turn, pigeon's receipt can contain streamed text goose never committed (cancelled reply has no _meta.honk.text, pigeon falls back to live chunks).
- M1a COMPLETE. Workspace 288 passed / 0 failed / 46 ignored (live) with HONK_GOOSE_BIN. Next is M1b (author-gated).
- Pre-PR review fixes:
  1. Sidecar collision prevention: Upstream IDs formatted as `hk{boot}_{actor}-{seq}` using hub-wide atomic actor ID counter; sidecar rotated/truncated on new generation / activation boundaries (90ba91c).
  2. Client prompts and turn text skip steer messages (`metadata.steer = true`); turn text stops at the next non-steer client user prompt (6e611b6, 955c79b).
  3. ReplayBatch sent to `replay_tx` before registering viewer for live fan-out, preventing live frames from preceding replay on attach (4a8321f).
  4. In-flight pending requests (steer/forward) failed with -32000 on connection replacement / handle_gone; on_rebuilt sets actor Live before draining session events (8d09f3b).
  5. Ledger prune (24h TTL, 1000 max entries) enforced on Done insert, not only at boot; sidecar consulted before declaring InFlight entries lost at boot (09741f0).
  6. Ambiguous prompt POST failures bounded: conservative bounded rule polls steer probe and export at 2s, 5s, 10s, 20s, 30s (scaled via --ambiguous-inflight-timeout-secs, default 30s); run announced, prompt reply, probe actualRunId, or export containing prompt N confirms delivered; whole window expiring with every probe idle and prompt absent marks not_delivered and dispatches parked.
  7. Secret file validated >= 32 chars and fsync'd on creation; downstream auth rejects empty tokens (3217f9f).
  8. Steer probe timeout and retry added for degraded mode (1e57c3d).

## Current state (2026-10-12, after M1a merged) — READ THIS FIRST
- main = 1d6843c: PRs #1 (M0), #2 (goose patch track), #3 (M1a) squash-merged. Old branches m0, goose-patch-track, m1a and worktrees ../honk-patch-track, ../honk-m1a are historical (m1a's pre-rebase history is in local branch `m1a-pre-rebase-backup`). M1b work happens in worktree ../honk-m1b (branch `m1b`, off main).
- Task tracking moved to beads: `bd` in this repo (stealth, `.beads/` git-excluded, DoltHub `jmohrbacher/honk`). M1b epic `honk-znj` (runbook in its design field), M1c epic `honk-oi2`; `bd ready --parent=<epic>` is the only "what next". This file keeps history and decisions.
- Production goose (:4080, `goose-serve`) runs patched-1.54.0.3 since 2026-10-12 10:46 EDT (workstation PR #665; sessions.db backup in ~/.local/state/goose-db-backups/pre-patched.3-20261012-104623). Verified by real stall-watch traffic. Plan Task B2 is DONE.
- goose fork hygiene (Apache §4(b) notices + Nix dev shell fix) merged as johnnymo87/goose PR #1 into patched/v1.54.0; no new release needed (binary unchanged).
- pigeon PR #168 (honest WS-disconnect reporting) merged and deployed. B1 must build on it: on a honk connection, reconnect once and resend with the same client_req_id; on bare goose keep #168's behaviour.
- stall-watch coordinators (eng-agent-platform/stallwatch/coordinator.py) do NOT use ACP: each tick runs the `goose run` CLI (`--session-id … --resume` or `--name stallwatch-…`) as a subprocess, sharing ~/.local/share/goose/sessions/sessions.db with goose-serve. The author wants to use honk to talk to those coordinators; that is a separate design question (CLI writer and serve writer on one session), tracked as a bead, not part of the cutover.
- Process the author asked for on M1b: compact first; optional oracle-opus consult; subagent-driven development where there are code tasks; adversarial-reviewer-opus on designs and diffs; PRs via shepherding-pull-requests; keep beads current. Author-gated steps (anything that changes live pigeon/honkd/goose service config) wait for an explicit go.
- Known limits carried into M1b: --max-sessions 16 with no LRU (M1c) — check how many goose sessions pigeon holds open before cutover; logs not compacted (M1c); state.json is rewritten+fsynced several times per turn under one lock; boot-time restored-inflight check awaits export inline.
