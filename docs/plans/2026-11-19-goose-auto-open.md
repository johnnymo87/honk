# Auto-open launched goose sessions in honk chat (honk-9qf.30)

Jonathan, 2026-11-16 (honk Telegram topic): "making honk auto-open on cloudbox
the way that opencode does." Scope (a) approved 2026-11-19: on by default, like
`opencode-launch`.

## Where opencode auto-opens, and the goose counterpart

Verified 2026-11-19 against origin/main (workstation cca5a0f0, honk 0ecf5e6).

| opencode path | goose counterpart | in scope |
|---|---|---|
| `opencode-launch` (pkgs/opencode-launch ~L842-910): default on, `--no-attach`, `OPENCODE_LAUNCH_NO_ATTACH` (allowlist truthy), `--tmux-session` | `honk launch` (crates/honk/src/launch.rs): no auto-open | yes (honk) |
| pigeon Telegram `/launch` (daemon worker/launch-ingest.ts `autoAttach`) | `/launch --backend goose` (daemon goose/launch-ingest.ts): no auto-open | yes (pigeon) |
| pigeon remote-launch receiver (`swarm_launch` from another machine) | none: the receiver only creates opencode sessions | no |
| pigeon revive-and-deliver | none: honkd is durable | no |

Unchanged by design: lgtm and stall-watch workers pass `--no-attach`;
stall-watch coordinators are created over honk transport, not by a launcher.

The open itself already exists: `oc-auto-attach goose:<id>` resolves the cwd
from `honk list --json` and RPCs nvim's `user.oc_auto_attach.open` with
`harness="goose"`, which opens a tab running `honk chat <id>` (honk-9qf.15.11).
Only the session picker calls it today.

## Design

Each launcher fires `oc-auto-attach goose:<native>` best-effort and
fire-and-forget after the session exists and its first prompt was sent, the same
shape as the opencode launchers. Rejected: one hook in pigeon at goose session
registration. `honk launch` registers before prompting, which widens the
honk-9qf.9 race below, and `--no-attach` would have no clean route to it.

### 1. workstation: oc-auto-attach waits briefly for a brand-new goose session

honk-9qf.9: goose's session/list omits a session until its first user message is
persisted, and the goose branch gives up at once (exit 3) on not-found. Replace
the single `honk list --json` with a poll:

- Deadline `OC_AA_GOOSE_GRACE_SECS` (default 10), poll every 0.5 s.
- Listed, not archived, cwd exists: proceed.
- Listed but archived, or cwd gone: conclusive, exit 3 at once (as today).
- Not listed: keep polling; at the deadline exit 3.
- `honk list` failed or printed a non-array: keep polling; at the deadline exit
  2 (the last observation decides between 2 and 3).
- Tests (pkgs/oc-auto-attach/test-project-key.sh, fake honk via
  `OC_AA_HONK_BIN`): a session that appears on the Nth call is found; not-found
  still exits 3 (with a short grace so the test stays fast); archived exits 3
  without waiting; list failure still exits 2.
- Docs: script header comment, and the opencode-launch skill's auto-attach
  section gets a goose paragraph (honk launch, /launch --backend goose, opt-outs).

### 2. honk: `honk launch` auto-opens

- `LaunchArgs`: `--no-attach`, `--tmux-session <name>`. Env
  `HONK_LAUNCH_NO_ATTACH`: allowlist truthy (`1 true TRUE yes YES on ON`), as
  opencode-launch, so `=0` does not disable.
- `LaunchOpts.attach: Option<AttachSettings { bin, tmux_session }>`; `None` is
  off. Bin: `$HONK_AUTO_ATTACH_BIN`, else `oc-auto-attach` if on PATH, else skip.
- In `launch::run`, after the prompt, whenever `launched.prompt != No` (Ok or
  Unknown; also on a tag-only Partial): spawn
  `oc-auto-attach [--tmux-session X] goose:<native>` with stdin null,
  stdout+stderr appended to /tmp/oc-auto-attach.log, its own process group
  (`process_group(0)`), `OPENCODE_SESSION_ID` and `AGENT_SESSION_ID` removed
  (a tmux server it starts would otherwise capture the launcher's id into every
  later pane; see opencode-launch's `env -u OPENCODE_SESSION_ID`). Not waited on.
- One stderr line: `honk launch: auto-attach: started|skipped (<why>)`. The
  stdout line format is unchanged (machine-readable).
- Tests: unit tests for the env allowlist and arg building; tests/launch.rs with a
  fake oc-auto-attach script recording argv and env: happy path spawns it with
  `goose:<id>` (and `--tmux-session`), env scrubbed; `attach: None` spawns
  nothing; prompt refused (prompt=no) spawns nothing.
- Docs: `LAUNCH_LONG_ABOUT`, docs/chat.md.

### 3. pigeon: `/launch --backend goose` auto-opens

- `GooseLaunchInput.autoAttach?: (target: string) => void`; called with
  `goose:<backendSessionId>` after a successful `deliver` (no `failure`),
  wrapped in try/catch. `deliver` returns without waiting for the turn, so the
  tab opens while the turn runs.
- index.ts passes the existing `autoAttach` from worker/launch-ingest.ts
  (honors `OC_AUTO_ATTACH_BIN`, logs to /tmp/oc-auto-attach.log).
- Tests in packages/daemon/test/goose-launch-ingest.test.ts: called once with
  `goose:<id>` on success; not called when the prompt is refused or mint fails;
  a throwing autoAttach does not change the Telegram reply.
- pigeon-daemon's PATH is locked down; oc-auto-attach's `repair_path` restores
  the login PATH before `honk list`, and honk reads its secret from the state
  file (pigeon runs as dev). Verify on deploy.

## Delivery

Three PRs, independent (any order): workstation (1), honk (2), pigeon (3). Each
gets an adversarial review before opening. No merge, deploy, home-manager switch
or restart without Jonathan's OK. After deploy, verify end to end: `honk launch`
on a throwaway prompt opens a tab; `/launch --backend goose` from Telegram opens
one; `--no-attach` does not.
