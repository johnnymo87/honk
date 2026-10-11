# goose patch track Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Ship fork release `patched-1.54.0.3` of goose with three patches:
1. A shell process-group kill, with SIGTERM before SIGKILL.
2. SSE subscriber takeover.
3. An SSE replay ring with `Last-Event-ID`.

Each patch is proven by a live honk probe against an isolated **patched** sandbox,
before tagging and again on the release binary. `honk-upstream` learns to use the
patches and to report whether its upstream has them. M1's honkd requires patched goose.

**Architecture:**
- Patch 1 is goose code (`developer/shell.rs`).
- Patches 2 and 3 live in the external crate `agent-client-protocol-http` 2.2.0. The fork
  vendors the crate under `vendor/` and swaps it in with `[patch.crates-io]`.
- The patches touch **only the HTTP SSE path**. The new HTTP mailbox is a new module. The
  WebSocket path keeps the original mailbox source unchanged. That path is the one
  pigeon uses in production.
- The patched crate advertises what it supports in a response header on `GET /acp`
  (`Acp-Stream-Features`). `honk-upstream` exposes that header, so honkd can refuse
  unpatched goose unless told otherwise.
- The patched build runs as a **second** sandbox unit, `honk-goose-sandbox-patched` on
  port 4181. The shared sandbox on 4180 is never touched.
- Production deploy is a separate runbook that only the author runs. It is gated on
  pigeon's own goose probes passing against the patched binary.

**Tech Stack:** Rust 1.99.0 (the fork's `rust-toolchain.toml` and honk's pin agree),
tokio, axum 0.8 SSE, libc `killpg`; reqwest and `eventsource-stream` in honk; Nix dev
shells (`nix develop` in both repos); GitHub Actions (`patched-release.yml`); `gh`;
pigeon's `npx tsx` probe scripts.

**Read first:**
- `docs/spike/goose-patches.md`: candidates, ranking, code sites.
- `docs/spike/m0-findings.md`: Summary; amendments A2, A3, A8, A11, A14, A16; P2d, P4b, P5a.
- `docs/plans/2026-10-10-honk-m0-plan.md`: Ground rules and "Goose patch track".

**Review record:** revised 2026-10-11 after adversarial review, verdict "execute after
fixes, no rework". The review confirmed the core mechanisms as sound. The fixes
applied in this revision:
- A separate sandbox unit for the patched build.
- P2f actually exercises replay, and a new probe P4b′ covers a lost permission.
- Frames carry their SSE id, and resume is explicit.
- Kills escalate from SIGTERM to SIGKILL.
- The WS mailbox stays unchanged, with a pigeon qualification step.
- Probes run before tagging.
- Several minor fixes.

---

## Ground rules

**Fork safety (non-negotiable)**
- **Tags.** Only `patched-1.54.0.<n>`. **Never create or push a `v*` tag**: upstream's
  `release.yml` and `publish-docker.yml` fire on `v*`. **Never move, delete or re-push
  a tag.** If a release run fails, fix the cause and cut `.4`.
- **Pushes.** **Never push to `origin`**: in the fork checkout, `origin` is upstream
  `block/goose`. The only push target is the remote `fork` (`johnnymo87/goose`). Every
  push names an explicit refspec. Nothing is force-pushed.
- **Tag only after Task 8 passes.** Its probes run against a build of the fork
  worktree.
- **Work in a dedicated worktree.** The existing fork checkout is on local branch
  `headless-cache-ttl`, which is the same commit as `fork/patched/v1.54.0`
  (`66279407d`). No local `patched/v1.54.0` branch exists. Other sessions may use
  that checkout, so never `switch`, `checkout`, `stash`, `reset` or `clean` there.
- **Formatting.** Never run `cargo fmt --all` (or the Justfile's fmt recipe) in the
  fork. The vendored crate is a workspace member, and reformatting it would bury the
  patches in churn.
  - Format goose code with `cargo fmt -p goose`.
  - Format vendored files you touched with `rustfmt --edition 2024 <file>...`.
  - Before every commit, check that `git diff --stat` lists only files you meant to
    change.

**Sandbox only**
- **Production is off-limits to the agent.** That is the `goose-serve` service on
  127.0.0.1:4080. The agent never runs any step of "Production deploy".
- **Two sandbox units, never swapped:**

  | Unit | Port | `HONK_SANDBOX` | Binary |
  |---|---|---|---|
  | `honk-goose-sandbox` (shared; other agents' probes depend on it) | 4180 | `$HOME/.local/state/honk-sandbox` | `goose` on PATH. **Never restarted or relaunched by this plan.** |
  | `honk-goose-sandbox-patched` (this plan's) | 4181 | `$HOME/.local/state/honk-sandbox-patched` | `GOOSE_BIN`: the worktree build (Task 8), then the release build (Task 10) |

- **How probes find the patched sandbox.**
  - Probes take the URL from `goose_url()`, which reads `HONK_GOOSE_URL`. Set it to
    `http://127.0.0.1:4181/acp`.
  - The secret comes from `$HOME/.local/state/honk-sandbox-patched/secret`.
  - `TcpProxy` targets `goose_port()`, never a literal port.

**Long commands**
- The bash tool times out after 120 s. Run longer commands as a user unit that
  writes a log, and poll the log with short `tail` calls. This covers cargo builds in
  a fresh worktree, release runs (~30 min), and probe runs (1-4 min each).

```bash
export XDG_RUNTIME_DIR=/run/user/$(id -u)
systemd-run --user --unit="gp-$RANDOM" --collect --working-directory="<DIR>" \
  --setenv=PATH="$PATH" --setenv=HOME="$HOME" --setenv=CARGO_BUILD_JOBS=8 \
  --expand-environment=no \
  bash -c '<COMMAND> > /tmp/opencode/<LOG>.log 2>&1; echo "exit=$?" >> /tmp/opencode/<LOG>.log'
# then, as separate calls:  tail -n 40 /tmp/opencode/<LOG>.log
```

  "Run (long, log `X`) in `<DIR>`" below means this pattern. `<COMMAND>` is usually
  `nix develop -c cargo …`. Pass `--expand-environment=no`; without it systemd rewrites
  `$` inside the payload.

**Variables:** `$FORK` is the existing fork checkout, `$FORK_WT` the new worktree
(Task 0), `$HONK` the honk repo root, `$PIGEON` the pigeon checkout. All paths in this
plan are relative to whichever repo the task names.

**honk is a public repo**
- No secrets, org names or absolute home paths in committed files or fixtures.
- Fixtures land under `fixtures/goose-1.54.0-patched.3/<probe>/<ts>.jsonl`, through the
  existing redactor. Only the Task 10 run, on the release binary, is committed.
- Pre-release runs write to `HONK_FIXTURES_ROOT=/tmp/opencode/honk-prerelease-fixtures`.
- Review every fixture diff by eye before pushing.
- Other agents work in honk: stage only your own files (`git add <paths>`, never `-A`).

**Fork commit style:** `<area>: <what the change does, as a sentence>`, lowercase
area, no body needed. For example: `gcpauth: GCP_AUTH_SCOPES sets the scopes for
authorized-user token refresh`, `ci: run the patched modules' unit tests before
releasing`. Tags are annotated: `goose v1.54.0 + local patches (<list>)`.

---

## Facts this plan relies on (verified 2026-10-11 against the source)

| Fact | Where |
|---|---|
| goose depends on `agent-client-protocol-http = { version = "2.2.0", default-features = false }` | fork `Cargo.toml:26` |
| goose enables the crate's `server` feature behind goose feature `acp-http`, which `goose-cli` enables by default | `crates/goose/Cargo.toml:111, 211`; `crates/goose-cli/Cargo.toml:110, 131` |
| `Cargo.lock` pins 2.2.0 from crates.io, checksum `18e3d711…cc28f` | `Cargo.lock:101-104` |
| The fork **already** has a `[patch.crates-io]` table (`v8`, `cudaforge`) and a vendored `vendor/v8` in workspace `members` | `Cargo.toml:1-7, 117-119` |
| The crate package ships **no LICENSE file**. It is Apache-2.0 per its metadata. `.cargo_vcs_info.json` points at `agentclientprotocol/rust-sdk` commit `2a78849d3eb3dcb140dade3b8fc938cf1e2b9ce5`, path `src/agent-client-protocol-http` | crate tarball |
| `cargo test -p <pkg>` refuses a non-member that has dev-dependencies. The vendored crate must therefore be a workspace **member** | cargo behaviour |
| `GET /acp` dispatch: a WS upgrade goes to `websocket_server::handle_ws_upgrade`; everything else goes to `http_server::handle_get`. That function answers `406` before any header logic when `Accept` lacks `text/event-stream`, and pigeon's preflight relies on that `406` | crate `server.rs:177-194`, `http_server.rs:331-342`; pigeon `src/goose/preflight.ts:18-31` |
| `HttpOutbound.session_streams` mailboxes are created per session id and **never removed** until the connection ends | crate `connection.rs:322-333` |
| `patched-release.yml` triggers on `push: tags: patched-*` and runs three jobs, listed under this table | fork `.github/workflows/patched-release.yml` |
| `build-cli-linux.yml` builds `cargo build --release -p goose-cli` with default features and **no `--locked`**. A stale `Cargo.lock` would be silently re-resolved in CI | `build-cli-linux.yml:144-170` |
| The fork's CI, cargo-deny and cargo-machete workflows don't gate tag releases. `ci.yml` is `main`-only, and the other two show as skipped on the fork | `ci.yml:3-13`, `gh run list` |
| `crates/goose` default features are `[]`. The developer shell module is unconditional | `crates/goose/Cargo.toml:12`, `platform_extensions/mod.rs:9` |
| `libc` is already a goose dependency on unix. `nix` is only transitive | `crates/goose/Cargo.toml:280-281` |
| goose already puts MCP subprocesses in their own process group with `command.process_group(0)`. That is precedent for patch 1 | `crates/goose/src/subprocess.rs:68-74` |
| goose constructs `ServerOptions` with a struct literal, so a new field would break it. Patches 2 and 3 use crate constants instead | `crates/goose/src/acp/transport/mod.rs:101-108` |
| `initialize` reports `agentInfo = ("goose", CARGO_PKG_VERSION)` = `"1.54.0"` for both upstream and the fork. **The version cannot distinguish them** | `crates/goose/src/acp/server.rs:1848` |
| Both NixOS hosts are aarch64 and have no `/lib64` loader. A release binary needs autoPatchelf, and the workstation repo's `pkgs/goose` does that. A binary built inside the fork's `nix develop` already links against Nix glibc and runs as is | workstation `pkgs/goose/default.nix` |
| Pigeon talks to production goose over **WebSocket** (`ws://…/acp?token=`). Its goose probes are `packages/daemon/scripts/goose-{launch,runner,id-split}-probe.ts`, run with `npx tsx`, configured by `GOOSE_ACP_URL`, `PIGEON_GOOSE_PROBE_TOKEN` and `GOOSE_ACP_CWD`. They exit non-zero on any FAIL, and the launch probe uses an in-memory DB | pigeon checkout |

The three `patched-release.yml` jobs:
- `build` calls `build-cli-linux.yml` at the tag's SHA (gnu and musl, x86_64 and aarch64).
- `test-patches` runs on `ubuntu-24.04-arm`. It runs filtered `cargo test` for
  `gcpvertexai gcpauth` and `cache_ttl`.
- `release` needs both, and uploads the gnu tarballs plus `SHA256SUMS`.

## Out of scope

- **`_goose/unstable/session/subscribe` (candidate 4).** honkd already fans out to
  viewers with its own cursor and backfill. This patch is large (about 25 call sites)
  and adds new protocol surface.
- **WS detach (candidate 1).** honkd uses HTTP+SSE, where turns already survive drops
  (A1). An honest version needs a resume protocol plus acks on top of JSON-RPC.
- **Any change to the WebSocket path.** This plan deliberately leaves it unchanged
  (see "WebSocket stays byte-identical").
- **`TCP_USER_TIMEOUT` stopgap.** Superseded by takeover.
- **Browser CORS for the new headers** (`transport/mod.rs:150-170`). honkd is not a
  browser.

## Order of work

| # | Task | Gate |
|---|---|---|
| 0 | Pre-flight, worktree | clean tree, no `.3` anywhere |
| 1 | honk: `GOOSE_BIN`, `HONK_SANDBOX_UNIT` | scripts parse; guard rejects relative paths |
| 2 | fork: patch 1, shell process-group TERM→KILL | 6 new shell tests green |
| 3 | fork: vendor the crate unmodified | `diff -r` clean; crate tests green |
| 4 | fork: patch 2, takeover (new HTTP mailbox; WS untouched) | crate tests green; WS sources unchanged |
| 5 | fork: patch 3, replay ring and `Last-Event-ID` | crate tests green |
| 6 | fork: release workflow tests and vendoring check | `cargo tree --locked` check passes |
| 7 | honk: `honk-upstream` ids, explicit resume, features | workspace tests green |
| 8 | **Pre-tag gate:** build the worktree; P2e, P2f, P4b′, P5a′ and pigeon probes on `honk-goose-sandbox-patched` | all pass |
| 9 | Release `patched-1.54.0.3` | assets verified, binary packaged |
| 10 | Re-run every probe once on the release binary; commit fixtures | all pass |
| 11 | Docs | — |
| — | **Production deploy (author only)** | Task 10 pigeon qualification PASS |

The shell patch comes first because it is the smallest and independent, and its build
warms the worktree's `target/`. Patch 3 extends patch 2's mailbox, so it must follow it.

## Deviation from `goose-patches.md` §2 (stated so a reviewer can veto it)

The spike sketched takeover as a per-subscriber pump task. The pump would own the
lease, forward frames into a bounded channel, and be aborted on takeover. This plan
keeps the spike's key insight. The stuck stream is never polled again, so it cannot be
asked to give up the lease; the lease must be **taken**. The mechanism differs:
**owner-token leases**.

- The HTTP mailbox's queue sits under a `std::sync::Mutex` together with
  `owner: Option<u64>`.
- A lease is `(Arc<shared>, id)`. Every dequeue checks `owner == Some(id)` under the
  lock.
- Takeover installs a new owner id synchronously.
- If the stuck generator is ever polled again, it sees the mismatch and ends. Nothing
  is aborted or awaited.

The reason: a pump runs on its own, so it dequeues frames ahead of hyper. A clean
client drop would then lose whatever the pump had staged. That breaks the crate's
existing test `sse_allows_one_active_subscriber_per_logical_stream`
(`http_server.rs:1309-1383`), which asserts that frames queued before a drop reach the
next subscriber. Owner tokens keep today's exact dequeue-on-poll behaviour.

## WebSocket stays byte-identical (decision)

Pigeon drives production goose over WS. Patches 2 and 3 therefore leave the WS path's
code unchanged:
- The new owner-token/ring mailbox goes in a **new module** `http_mailbox.rs`
  (`HttpMailbox`/`HttpLease`), used only by `HttpOutbound`.
- The original `OutboundMailbox`/`OutboundLease` in `connection.rs:37-104` stay
  untouched and keep serving `WebSocketOutbound`.
- `websocket_server.rs` is not edited.
- Task 4 Step 6 checks this with `git diff --exit-code` over those line ranges and that
  file.

This is practical. The two transports already have separate mailboxes
(`HttpOutbound.connection_stream`/`session_streams` versus
`WebSocketOutbound.all_outbound`). Only the HTTP subscribe methods change return type.
The cost is two mailbox types in one crate, which the upstream PR could later unify.

What still changes for pigeon in production:
- **Patch 1.** Cancel and timeout of a developer-shell command now SIGTERM the whole
  process group, wait up to 2 s, then SIGKILL. Before, only `bash` was SIGKILLed, so
  pigeon-driven cancels left orphans too.
- **The vendored crate as a whole is compiled from the fork.** Its WS code is
  identical source.

Hence the pigeon qualification in Tasks 8 and 10, and the deploy gate.

---

### Task 0: Pre-flight (read-only, plus one new worktree)

1. Check the fork state:
   ```bash
   git -C "$FORK" status --porcelain            # expect: empty
   git -C "$FORK" fetch fork
   git -C "$FORK" rev-parse fork/patched/v1.54.0 # expect: 66279407d…
   git -C "$FORK" log --oneline v1.54.0..fork/patched/v1.54.0   # expect: the 7 existing patch/ci commits (top: 66279407d)
   git -C "$FORK" tag -l 'patched-*'             # expect: patched-1.54.0.1, patched-1.54.0.2 (no .3)
   git -C "$FORK" ls-remote --tags fork 'patched-1.54.0.3'      # expect: empty
   ```
   If `.3` exists anywhere, stop and ask.
2. Create the worktree:
   ```bash
   FORK_WT="$(dirname "$FORK")/goose-patched-track3"
   git -C "$FORK" worktree add -b patched/v1.54.0 "$FORK_WT" fork/patched/v1.54.0
   ```
3. In `$FORK_WT`, run `nix develop -c rustc --version`. Expected: `1.99.0`.
4. Run (long, log `gp-baseline`) in `$FORK_WT`: `nix develop -c cargo test -p goose
   --lib -- developer::shell`. Expected: all existing shell tests pass; record the
   count. A failure here is not ours: stop.
5. Check that pigeon's probes exist: `ls "$PIGEON"/packages/daemon/scripts/goose-*-probe.ts`.
   Expected: `goose-launch-probe.ts`, `goose-runner-probe.ts`, `goose-id-split-probe.ts`
   (plus `goose-acp-probe.ts`).

---

### Task 1: honk — `GOOSE_BIN` and a configurable unit name for the sandbox scripts

**Files:**
- Modify: `scripts/goose-sandbox.sh` (launch block `:141-150`; header comment)
- Modify: `scripts/goose-sandbox-check.sh` (unit name; exe report)

**Step 1: `GOOSE_BIN` in `goose-sandbox.sh`.** Insert right before `cd "$SANDBOX"`:

```bash
# Which goose to run. Default: whatever `goose` is on PATH (the production build).
# Set GOOSE_BIN to an absolute path to run a specific build, e.g. a fork release.
GOOSE_BIN="${GOOSE_BIN:-goose}"
if [[ "$GOOSE_BIN" != "goose" ]]; then
    case "$GOOSE_BIN" in
        /*) ;;
        *)
            echo "ERROR: GOOSE_BIN must be an absolute path: $GOOSE_BIN" >&2
            exit 1
            ;;
    esac
    if [[ ! -x "$GOOSE_BIN" ]]; then
        echo "ERROR: GOOSE_BIN is not executable: $GOOSE_BIN" >&2
        exit 1
    fi
fi
goose_resolved=$(command -v "$GOOSE_BIN") || { echo "ERROR: $GOOSE_BIN not found" >&2; exit 1; }
echo "goose binary: $(readlink -f "$goose_resolved")" >&2
```

Then change the final line of the `exec env` block to:
`"$GOOSE_BIN" serve --host 127.0.0.1 --port "$PORT"`.

Do not run `goose --version` there; it would run before `GOOSE_PATH_ROOT` is set.

Add to the header comment:

```bash
# A second, isolated sandbox for a specific goose build (e.g. the fork):
#   systemd-run --user --unit=honk-goose-sandbox-patched --collect --setenv=PATH="$PATH" --setenv=HOME="$HOME" \
#     --setenv=HONK_SANDBOX="$HOME/.local/state/honk-sandbox-patched" --setenv=HONK_SANDBOX_PORT=4181 \
#     --setenv=GOOSE_BIN=<abs path to goose> <abs path>/scripts/goose-sandbox.sh
```

**Step 2: `goose-sandbox-check.sh`.**
- After the `PORT` validation, add `UNIT="${HONK_SANDBOX_UNIT:-honk-goose-sandbox}"`.
- Replace both literal `honk-goose-sandbox` occurrences in the `systemctl` call and
  the error message with `"$UNIT"`.
- After the cwd check, add:

```bash
# 2b. Report which binary is running; with HONK_EXPECT_GOOSE_BIN, require it.
actual_exe=$(readlink "/proc/$pid/exe" 2>/dev/null || true)
echo "goose exe: $actual_exe"
if [[ -n "${HONK_EXPECT_GOOSE_BIN:-}" ]]; then
    want_exe=$(readlink -f "$HONK_EXPECT_GOOSE_BIN")
    if [[ "$actual_exe" != "$want_exe" ]]; then
        echo "ERROR: $UNIT runs $actual_exe, expected $want_exe" >&2
        exit 1
    fi
fi
```

**Step 3: Verify without touching any running unit.**
- Run `bash -n scripts/goose-sandbox.sh scripts/goose-sandbox-check.sh`. Expected: no
  output.
- Run `GOOSE_BIN=relative/goose HONK_SANDBOX_PORT=4199 HONK_SANDBOX=$(mktemp -d) bash
  scripts/goose-sandbox.sh`. Expected: exit 1 with `GOOSE_BIN must be an absolute
  path`. An earlier error (missing source config or secret) is acceptable; if you get
  one, re-read the diff instead.
- Run `scripts/goose-sandbox-check.sh` with no env. Expected: still `sandbox OK`
  against the shared unit, which is a read-only check.

**Step 4: Commit.**
```bash
git add scripts/goose-sandbox.sh scripts/goose-sandbox-check.sh
git commit -m "scripts: GOOSE_BIN and HONK_SANDBOX_UNIT run and check a second, isolated goose build"
```

---

### Task 2: fork — patch 1, shell process-group termination (SIGTERM, grace, SIGKILL)

**Files (in `$FORK_WT`):** `crates/goose/src/agents/platform_extensions/developer/shell.rs`.
The relevant spots:
- `run_command` `:557-668`: the spawn at `:574-576` and the kill sites at `:605`,
  `:611` and `:622`.
- The tests module at `:949`.

**Behaviour after the patch (unix):**
- **Spawn.** The shell is spawned with `process_group(0)`, so it leads a new process
  group whose pgid is its own pid.
- **Cancel or timeout.** goose sends **SIGTERM to the whole group** and waits up to
  `SHELL_TERM_GRACE` (2 s) for the group to empty. It reaps the leader as soon as it
  exits. Whatever is still in the group after the grace gets **SIGKILL**. The cancel
  and timeout paths await this, so a cooperative group is gone within milliseconds and
  a TERM-ignoring one within about 2 s.
- **Future dropped mid-run.** A dropped future cannot await. Escalation is handed to a
  detached `std::thread`: TERM, poll for up to 2 s, then KILL. The drop case exists
  because WS close and DELETE abort the agent task (T5). That drops this future without
  reaching the `select!` cancel arm, and today it orphans the shell.
- **Normal exit.** A command that exits on its own leaves what it backgrounded
  running, exactly as today. `shell_does_not_hang_on_backgrounded_process` relies on
  this.
- **Non-unix.** Unchanged: `start_kill()` only.

**Why TERM first.** SIGKILL gives no process a chance to clean up. The concrete
damage on this host is git: a SIGKILLed `git commit` or `git rebase` leaves
`.git/index.lock` behind. Every later git command in that repo then fails with
"Unable to create '…/index.lock': File exists" until someone deletes the lock. A
SIGTERMed git removes its lock in its signal handler.

**Accepted race (unbiased `select!`).** `run_command`'s `tokio::select!` is unbiased.
If the command exits at the same instant cancellation fires, the exit branch can win.
The guard is then disarmed and anything the command backgrounded survives. This is the
same outcome as a cancel arriving a moment later. It is accepted and left unchanged,
because adding `biased;` would change upstream select semantics for no measurable
gain.

**Accepted cost on this host (interactive CLI).**
- The shell no longer sits in the terminal's foreground process group.
- In interactive `goose` CLI use, terminal Ctrl-C reaches shell children only through
  goose's cancel, which is now TERM→KILL.
- A child that opens `/dev/tty` (an ssh or sudo password prompt) gets SIGTTIN/SIGTTOU
  and stops until the timeout terminates it.
- The interactive CLI on this host shares the patched binary, so the author accepts
  this. `stdin` is already `/dev/null`, so such prompts never worked. goose already
  does the same for MCP subprocesses. `goose serve` has no terminal.

**Modification notice.** `shell.rs` is goose's own Apache-2.0 file, inside goose's own
(forked) repository. It follows the precedent of the fork's three existing patches,
which carry no in-file notice; the fork's public git history is the record of change.
It is also the upstreaming target. **Author's call:** if you want an in-file §4(b)
notice here as well, add `// Modified in the goose fork (patched/v1.54.0): process-group
termination.` above `struct ProcessGroupKiller`.

**Step 1: Write the failing tests.** Append inside `mod tests`, after
`shell_kills_hanging_command_after_explicit_timeout`. Every test name contains
`process_group`, which is the CI filter.

```rust
    /// Command whose tree is: bash (sh.pid) -> two backgrounded sleeps (a.pid, b.pid).
    #[cfg(unix)]
    fn process_tree_command(dir: &std::path::Path) -> String {
        let d = dir.display();
        format!(
            "echo $$ > {d}/sh.pid; sleep 31 & echo $! > {d}/a.pid; sleep 32 & echo $! > {d}/b.pid; wait"
        )
    }

    #[cfg(unix)]
    async fn read_pids(dir: &std::path::Path, names: &[&str], within: Duration) -> Vec<i32> {
        let deadline = std::time::Instant::now() + within;
        loop {
            let pids: Option<Vec<i32>> = names
                .iter()
                .map(|name| std::fs::read_to_string(dir.join(name)).ok()?.trim().parse().ok())
                .collect();
            if let Some(pids) = pids {
                return pids;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "shell never wrote its pid files"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    /// Fields after the `)` that closes comm: state, ppid, pgrp, ...
    #[cfg(unix)]
    fn proc_stat_fields(pid: i32) -> Option<Vec<String>> {
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
        let (_, rest) = stat.rsplit_once(')')?;
        Some(rest.split_whitespace().map(str::to_owned).collect())
    }

    #[cfg(unix)]
    fn process_group_of(pid: i32) -> Option<i32> {
        proc_stat_fields(pid)?.get(2)?.parse().ok()
    }

    /// True once `pid` is gone or a zombie (killed, waiting to be reaped).
    #[cfg(unix)]
    async fn wait_until_dead(pid: i32, within: Duration) -> bool {
        let deadline = std::time::Instant::now() + within;
        loop {
            let dead = match proc_stat_fields(pid) {
                None => true,
                Some(fields) => fields.first().is_some_and(|state| state == "Z"),
            };
            if dead {
                return true;
            }
            if std::time::Instant::now() >= deadline {
                return false;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    /// Starts `command`, waits for `pid_files`, returns the pinned future and the pids.
    #[cfg(unix)]
    macro_rules! start_and_read_pids {
        ($run:ident, $dir:expr, $files:expr) => {{
            tokio::select! {
                _ = &mut $run => panic!("command finished before the test acted"),
                pids = read_pids($dir, $files, Duration::from_secs(10)) => pids,
            }
        }};
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn process_group_killed_on_cancellation() {
        let tool = ShellTool::new_for_test().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let token = CancellationToken::new();
        let run = tool.shell_with_cwd(
            ShellParams { command: process_tree_command(dir.path()), timeout_secs: None },
            None,
            None,
            token.clone(),
        );
        tokio::pin!(run);
        let pids = start_and_read_pids!(run, dir.path(), &["sh.pid", "a.pid", "b.pid"]);
        let (sh, a, b) = (pids[0], pids[1], pids[2]);
        assert_eq!(process_group_of(sh), Some(sh), "the shell must lead its own process group");
        assert_eq!(process_group_of(a), Some(sh), "descendants must be in the shell's group");

        token.cancel();
        let result = tokio::time::timeout(Duration::from_secs(5), run)
            .await
            .expect("cancellation must return promptly");
        assert!(extract_shell_output(&result).exit_code.is_none());
        for pid in [sh, a, b] {
            assert!(wait_until_dead(pid, Duration::from_secs(2)).await, "pid {pid} survived cancellation");
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn process_group_killed_on_timeout() {
        let tool = ShellTool::new_for_test().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let result = tool
            .shell(ShellParams { command: process_tree_command(dir.path()), timeout_secs: Some(1) })
            .await;
        assert!(extract_shell_output(&result).timed_out);
        let pids = read_pids(dir.path(), &["sh.pid", "a.pid", "b.pid"], Duration::from_secs(1)).await;
        for pid in pids {
            assert!(wait_until_dead(pid, Duration::from_secs(2)).await, "pid {pid} survived the timeout");
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn process_group_killed_when_the_call_is_dropped() {
        let tool = ShellTool::new_for_test().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let pids = {
            let run = tool.shell(ShellParams { command: process_tree_command(dir.path()), timeout_secs: None });
            tokio::pin!(run);
            start_and_read_pids!(run, dir.path(), &["sh.pid", "a.pid", "b.pid"])
        }; // `run` is dropped here, as when an aborted agent task drops the tool future.
        for pid in pids {
            // Escalation runs on a detached thread: TERM now, KILL after the grace if needed.
            assert!(wait_until_dead(pid, Duration::from_secs(4)).await, "pid {pid} survived the dropped call");
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn process_group_left_alone_when_the_shell_exits_normally() {
        let tool = ShellTool::new_for_test().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let result = tool
            .shell(ShellParams {
                command: format!("sleep 33 & echo $! > {}/bg.pid", dir.path().display()),
                timeout_secs: None,
            })
            .await;
        assert_eq!(result.is_error, Some(false));
        let bg = read_pids(dir.path(), &["bg.pid"], Duration::from_secs(1)).await[0];
        assert!(
            !wait_until_dead(bg, Duration::from_millis(300)).await,
            "a deliberately backgrounded process must outlive a normal exit"
        );
        // SAFETY: plain signal to a pid this test created.
        unsafe { libc::kill(bg, libc::SIGKILL) };
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn process_group_gets_sigterm_and_a_grace_period_before_sigkill() {
        let tool = ShellTool::new_for_test().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path().display();
        let token = CancellationToken::new();
        let run = tool.shell_with_cwd(
            ShellParams {
                command: format!(
                    "trap 'echo cleaned > {d}/trap; exit 0' TERM; echo $$ > {d}/sh.pid; sleep 30 & wait"
                ),
                timeout_secs: None,
            },
            None,
            None,
            token.clone(),
        );
        tokio::pin!(run);
        let _ = start_and_read_pids!(run, dir.path(), &["sh.pid"]);
        token.cancel();
        tokio::time::timeout(Duration::from_secs(5), run)
            .await
            .expect("cancellation must return");
        assert_eq!(
            std::fs::read_to_string(dir.path().join("trap")).unwrap_or_default().trim(),
            "cleaned",
            "the TERM handler must run before any SIGKILL (git removes index.lock here)"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn process_group_escalates_to_sigkill_when_sigterm_is_ignored() {
        let tool = ShellTool::new_for_test().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path().display();
        let token = CancellationToken::new();
        // `trap '' TERM` sets SIG_IGN, which the backgrounded sleep inherits too.
        let run = tool.shell_with_cwd(
            ShellParams {
                command: format!("trap '' TERM; echo $$ > {d}/sh.pid; sleep 30 & echo $! > {d}/a.pid; wait"),
                timeout_secs: None,
            },
            None,
            None,
            token.clone(),
        );
        tokio::pin!(run);
        let pids = start_and_read_pids!(run, dir.path(), &["sh.pid", "a.pid"]);
        let started = std::time::Instant::now();
        token.cancel();
        tokio::time::timeout(Duration::from_secs(10), run)
            .await
            .expect("escalation must finish");
        let elapsed = started.elapsed();
        assert!(elapsed >= Duration::from_millis(1500), "SIGKILL came before the grace period: {elapsed:?}");
        assert!(elapsed < Duration::from_secs(5), "escalation took too long: {elapsed:?}");
        for pid in pids {
            assert!(wait_until_dead(pid, Duration::from_secs(2)).await, "pid {pid} survived SIGKILL");
        }
    }
```

**Step 2: Run them and watch them fail.** Run (long, log `gp-shell-red`) in
`$FORK_WT`: `nix develop -c cargo test -p goose --lib -- process_group`.
Expected: 6 tests run.
- `…_killed_on_cancellation` fails at "the shell must lead its own process group".
- `…_killed_on_timeout` and `…_when_the_call_is_dropped` fail with `pid … survived`.
- `…_gets_sigterm_…` fails: no trap file, because the shell got SIGKILL.
- `…_escalates_…` fails, because the elapsed time is under 1500 ms.
- `…_left_alone_…` passes. It guards existing behaviour.

**Step 3: Implement.**

(a) Above `fn run_command` (around `:556`):

```rust
/// How long a cancelled or timed-out command's process group gets to exit after
/// SIGTERM before it is SIGKILLed. Long enough for cleanup handlers (git removes
/// `.git/index.lock` on SIGTERM; a SIGKILLed git leaves it and every later git command
/// in that repo fails), short enough to keep cancel responsive.
#[cfg(unix)]
const SHELL_TERM_GRACE: Duration = Duration::from_secs(2);

#[cfg(unix)]
fn signal_group(pgid: libc::pid_t, signal: libc::c_int) {
    // SAFETY: killpg only sends a signal; ESRCH (group already empty) is fine.
    unsafe {
        libc::killpg(pgid, signal);
    }
}

/// Whether any process (a not-yet-reaped zombie included) is still in the group.
#[cfg(unix)]
fn group_alive(pgid: libc::pid_t) -> bool {
    // SAFETY: signal 0 performs only the existence and permission check.
    let rc = unsafe { libc::killpg(pgid, 0) };
    rc == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

/// Terminates a shell command's whole process group when the command is cancelled,
/// times out, or its future is dropped mid-run (e.g. the agent task is aborted on WS
/// close or DELETE). The shell is spawned with `process_group(0)`, so the group id is
/// the shell's pid and every descendant that does not call `setsid`/`setpgid` is in it.
///
/// Group-id reuse: SIGKILL is only sent right after `group_alive` observed a member,
/// and a pgid with a live member cannot be recycled. The remaining window between
/// that check and the signal would need `pid_max` pid allocations to bite; accepted.
#[cfg(unix)]
struct ProcessGroupKiller {
    pgid: Option<libc::pid_t>,
}

#[cfg(unix)]
impl ProcessGroupKiller {
    fn new(child: &tokio::process::Child) -> Self {
        Self {
            pgid: child.id().and_then(|pid| libc::pid_t::try_from(pid).ok()),
        }
    }

    /// The shell exited on its own: leave anything it backgrounded running.
    fn disarm(&mut self) {
        self.pgid = None;
    }

    /// SIGTERM the group, give it `SHELL_TERM_GRACE` to empty (reaping the leader as
    /// soon as it exits), then SIGKILL whatever remains.
    async fn terminate(&mut self, child: &mut tokio::process::Child) {
        let Some(pgid) = self.pgid.take() else {
            return;
        };
        signal_group(pgid, libc::SIGTERM);
        let deadline = tokio::time::Instant::now() + SHELL_TERM_GRACE;
        let _ = tokio::time::timeout_at(deadline, child.wait()).await;
        while group_alive(pgid) && tokio::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        if group_alive(pgid) {
            signal_group(pgid, libc::SIGKILL);
        }
    }
}

#[cfg(unix)]
impl Drop for ProcessGroupKiller {
    fn drop(&mut self) {
        let Some(pgid) = self.pgid.take() else {
            return;
        };
        // No await is possible here: escalate on a detached thread (not a tokio task,
        // which could be dropped with a shutting-down runtime).
        let escalate = move || {
            signal_group(pgid, libc::SIGTERM);
            let deadline = std::time::Instant::now() + SHELL_TERM_GRACE;
            while group_alive(pgid) && std::time::Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(25));
            }
            if group_alive(pgid) {
                signal_group(pgid, libc::SIGKILL);
            }
        };
        if std::thread::Builder::new()
            .name("goose-shell-kill".to_owned())
            .spawn(escalate)
            .is_err()
        {
            signal_group(pgid, libc::SIGKILL);
        }
    }
}
```

The shell leader is unreaped in the drop path until tokio's orphan reaper collects it.
`group_alive` therefore stays true for the full grace and the thread always sends the
final SIGKILL. That is harmless, because its target is at worst a zombie.

(b) Changes in `run_command`:
- After `let mut command = build_shell_command(…);` (`:568`), add
  `#[cfg(unix)] command.process_group(0);`. It goes here and not in
  `build_shell_command`, which stays a pure description of the command line.
- After the spawn (`:576`), add
  `#[cfg(unix)] let mut group = ProcessGroupKiller::new(&child);`.
- In each of the three kill sites (`:605`, `:611`, `:622`), insert
  `#[cfg(unix)] group.terminate(&mut child).await;` **before**
  `let _ = child.start_kill();`.
  - Keep `start_kill()`. It is the non-unix path, and on unix it is a no-op on a
    reaped or dead leader.
  - Keep the existing `child.wait().await`, which returns the cached status.
  - Inside a `select!` arm the other branch's future has already been dropped, so
    `&mut child` is free.
- Directly after the `let exit_code = if … { … };` expression (`:627`), add
  `#[cfg(unix)] group.disarm();`.

Flatpak: `process_group(0)` applies to `flatpak-spawn`, and the signals reach it only.
That is the same reach as today. Host-side descendants are outside our namespace
either way.

**Step 4: Run the tests and watch them pass.**
- Run (long, log `gp-shell-green`): `nix develop -c cargo test -p goose --lib --
  developer::shell`. Expected: the baseline count plus 6, all passing.
- Run `nix develop -c cargo clippy -p goose --lib --tests -- -D warnings`. Expected:
  clean.
- Run `nix develop -c cargo fmt -p goose`, then `git diff --stat`. Expected: only
  `shell.rs`.

**Step 5: Commit.**
```bash
git -C "$FORK_WT" add crates/goose/src/agents/platform_extensions/developer/shell.rs
git -C "$FORK_WT" commit -m "developer/shell: terminate the command's whole process group on cancel, timeout and drop (SIGTERM, 2s grace, SIGKILL)"
```

---

### Task 3: fork — vendor `agent-client-protocol-http` 2.2.0, unmodified

**Files (in `$FORK_WT`):**
- Create: `vendor/agent-client-protocol-http/**`, the crate contents minus its nested
  `Cargo.lock`.
- Create: `vendor/agent-client-protocol-http/LICENSE`.
- Create: `vendor/agent-client-protocol-http/VENDORED.md`.
- Modify: `Cargo.toml` (`members` `:2-7`; `[patch.crates-io]` `:117-119`).
- Modify: `Cargo.lock`, regenerated by cargo only.

**Step 1: Fetch the exact published crate and verify it.**
```bash
mkdir -p /tmp/opencode/acp-vendor && cd /tmp/opencode/acp-vendor
curl -fsSL -o acp-http-2.2.0.crate \
  https://static.crates.io/crates/agent-client-protocol-http/agent-client-protocol-http-2.2.0.crate
echo "18e3d7117a48e9db3c4b5e29f7539bc14e390f1f782d10a99b1d9f5c3a8cc28f  acp-http-2.2.0.crate" | sha256sum -c
tar -xzf acp-http-2.2.0.crate
```
Expected: `OK`. This is the checksum in the fork's `Cargo.lock:104`. On a mismatch,
stop.

**Step 2: Copy it in.**
```bash
cp -a /tmp/opencode/acp-vendor/agent-client-protocol-http-2.2.0 "$FORK_WT/vendor/agent-client-protocol-http"
rm "$FORK_WT/vendor/agent-client-protocol-http/Cargo.lock"   # members' lockfiles are ignored; avoid confusion
```
Keep the normalized `Cargo.toml`, which is self-contained. Also keep
`Cargo.toml.orig`, `.cargo_vcs_info.json`, `README.md` and `CHANGELOG.md` as
provenance.

**Step 3: Add the license (the tarball has none).**
```bash
SHA=2a78849d3eb3dcb140dade3b8fc938cf1e2b9ce5
gh api "repos/agentclientprotocol/rust-sdk/contents?ref=$SHA" -q '.[].name'
gh api "repos/agentclientprotocol/rust-sdk/contents/src/agent-client-protocol-http?ref=$SHA" -q '.[].name'
# fetch the license file found (crate dir's own if present, else repo root's), exact path from the listing:
gh api -H 'Accept: application/vnd.github.raw' \
  "repos/agentclientprotocol/rust-sdk/contents/<NAME>?ref=$SHA" \
  > "$FORK_WT/vendor/agent-client-protocol-http/LICENSE"
head -3 "$FORK_WT/vendor/agent-client-protocol-http/LICENSE"   # expect "Apache License" / "Version 2.0"
```
If the repo has no license file at that commit, copy goose's root `LICENSE` instead
(also Apache-2.0) and say so in `VENDORED.md`.

**Step 4: Write `vendor/agent-client-protocol-http/VENDORED.md`:**

```markdown
# Vendored: agent-client-protocol-http 2.2.0

- Source: crates.io `agent-client-protocol-http` 2.2.0,
  sha256 `18e3d7117a48e9db3c4b5e29f7539bc14e390f1f782d10a99b1d9f5c3a8cc28f`
  (the checksum upstream goose v1.54.0's Cargo.lock pins).
- Upstream VCS: github.com/agentclientprotocol/rust-sdk @ 2a78849d3eb3dcb140dade3b8fc938cf1e2b9ce5,
  path `src/agent-client-protocol-http` (from `.cargo_vcs_info.json`).
- License: Apache-2.0 (`LICENSE`, fetched from the upstream repo at that commit; the
  crate package ships none).
- Removed from the package: its nested `Cargo.lock` (ignored for workspace members).
- Wired in by the root `Cargo.toml`: workspace `members` (so `cargo test -p` works)
  and `[patch.crates-io]`.
- Formatting: never `cargo fmt --all` in this repo; it would reformat this crate. Use
  `rustfmt --edition 2024` on files you change.

## Local modifications (Apache-2.0 §4(b): each modified file carries a notice)

All on the HTTP SSE path only; the WebSocket path (`OutboundMailbox`/`OutboundLease` in
`connection.rs`, and `websocket_server.rs`) is unchanged.

- New `src/http_mailbox.rs`: owner-token HTTP mailbox (takeover) and per-stream replay ring.
- `src/connection.rs`, `src/http_server.rs`, `src/protocol.rs`, `src/lib.rs`: wiring,
  `Acp-Takeover`, `Acp-Stream-Features`, SSE `id:`, `Last-Event-ID`, `Acp-Replay`.

## When to delete this directory

When an upstream crate release contains equivalent takeover + replay and goose depends
on it. On every rebase onto a newer goose: if goose's `agent-client-protocol-http`
requirement leaves 2.x, the `[patch]` silently stops applying (cargo only warns
"patch was not used"); the release workflow's graph check fails in that case.
```

**Step 5: Wire it into the workspace.** In the root `Cargo.toml`, `members` gains:

```toml
    # Vendored + patched ACP HTTP transport; a member so `cargo test -p` can run its tests.
    "vendor/agent-client-protocol-http",
```

The existing `[patch.crates-io]` table (`:117`) gains:

```toml
agent-client-protocol-http = { path = "vendor/agent-client-protocol-http" }
```

**Step 6: Regenerate the lockfile minimally and check the graph.** In `$FORK_WT`:
```bash
nix develop -c cargo metadata --format-version 1 > /dev/null     # rewrites Cargo.lock
git diff --stat Cargo.lock
git diff Cargo.lock | grep '^[-+]version' || true
nix develop -c cargo tree --locked -p goose-cli --features acp-http -i agent-client-protocol-http --depth 0
```
Expected:
- The crate's lock entry loses its `source` and `checksum` lines. New `[[package]]`
  entries for its dev-dependencies may appear.
- **There are no `-version` lines.**
- `cargo tree` prints `… (…/vendor/agent-client-protocol-http)`.
- Never run `cargo update`.

**Step 7: Prove the copy is unmodified and its tests pass.**
- Run `diff -r /tmp/opencode/acp-vendor/agent-client-protocol-http-2.2.0
  "$FORK_WT/vendor/agent-client-protocol-http"`. Expected: only the `Cargo.lock`,
  `LICENSE` and `VENDORED.md` differences.
- Run (long, log `gp-acp-baseline`): `nix develop -c cargo test -p
  agent-client-protocol-http --features server --lib`. Expected: all pass; record the
  count.
- Run (long, log `gp-acp-clippy-baseline`): `nix develop -c cargo clippy -p
  agent-client-protocol-http --features server --all-targets`. Record the warning
  count. The crate enables `clippy::pedantic` as warn, and our patches must add no
  warnings.

**Step 8: Commit.**
```bash
git -C "$FORK_WT" add Cargo.toml Cargo.lock vendor/agent-client-protocol-http
git -C "$FORK_WT" commit -m "vendor: agent-client-protocol-http 2.2.0 from crates.io, unmodified, via [patch.crates-io]"
```

---

### Task 4: fork — patch 2, subscriber takeover (HTTP mailbox only)

**Wire contract (new):**
- **Request header `Acp-Takeover: 1` on an SSE `GET /acp`.** If the requested stream
  is held, the new subscriber takes it: `200`, not `409`. The stream is the connection
  stream, or the session stream named by `Acp-Session-Id`. Without the header nothing
  changes.
- **Response header `Acp-Stream-Features: takeover`** goes on every SSE `GET /acp`
  `200` **and** `409`. Patch 3 makes it `takeover, replay`. The `406`/`400`/`404`
  answers are unchanged, so pigeon's preflight `406` is untouched.
- **Scope.** Takeover works within the same connection id only. The id already
  authorizes POST and DELETE, so this adds no privilege. It cannot move a live run to
  another connection (P3c).

**Files (in `$FORK_WT/vendor/agent-client-protocol-http/src/`):**
- Create: `http_mailbox.rs`.
- Modify: `lib.rs` (module declaration).
- Modify: `connection.rs`:
  - `HttpOutbound` `:27-31`, `:298-333`.
  - The HTTP subscribe methods on `Connection` `:132-140` and `OutboundTransport`
    `:232-244`.
  - Leave `OutboundMailbox`/`OutboundLease` `:37-104` and everything `WebSocket*` as
    they are.
- Modify: `http_server.rs` (`handle_get` `:327-415`; tests).
- Modify: `protocol.rs` (constants next to `:3-8`).

**Step 1: Write the failing mailbox tests.** Create `http_mailbox.rs` containing only
the notice and the tests. Ports of the three original mailbox tests come first, so the
new type is held to the old contract:

```rust
// Modified for the goose fork (github.com/johnnymo87/goose, branch patched/v1.54.0):
// new file, not part of agent-client-protocol-http 2.2.0. See ../VENDORED.md.
// Licensed Apache-2.0, like the rest of this crate.

//! Outbound mailbox for HTTP SSE streams. WebSocket connections keep using the
//! original `connection::OutboundMailbox`, unchanged.

#[cfg(test)]
mod tests {
    use tokio::time::{Duration, sleep, timeout};

    use super::*;

    const ISSUE_288_BURST: usize = 1_025;

    #[tokio::test]
    async fn http_mailbox_buffers_bursts_before_subscription() {
        let mailbox = HttpMailbox::new();
        for index in 0..ISSUE_288_BURST {
            mailbox.push(format!("message-{index}")).unwrap();
        }
        let mut lease = mailbox.try_acquire().unwrap();
        for index in 0..ISSUE_288_BURST {
            assert_eq!(lease.recv().await, Some(format!("message-{index}")));
        }
    }

    #[tokio::test]
    async fn http_mailbox_has_one_active_owner_and_preserves_queued_frames() {
        let mailbox = HttpMailbox::new();
        let lease = mailbox.try_acquire().unwrap();
        assert!(mailbox.try_acquire().is_none());
        mailbox.push("queued before disconnect".to_string()).unwrap();
        drop(lease);
        mailbox.push("queued after disconnect".to_string()).unwrap();
        let mut resumed = mailbox.try_acquire().unwrap();
        assert_eq!(resumed.recv().await.as_deref(), Some("queued before disconnect"));
        assert_eq!(resumed.recv().await.as_deref(), Some("queued after disconnect"));
    }

    #[tokio::test]
    async fn take_over_revokes_the_old_lease_without_polling_it() {
        let mailbox = HttpMailbox::new();
        let mut old = mailbox.try_acquire().unwrap();
        mailbox.push("first".to_string()).unwrap();
        assert!(mailbox.try_acquire().is_none(), "plain acquire still refuses a held stream");

        let mut new = mailbox.take_over();
        mailbox.push("second".to_string()).unwrap();

        assert_eq!(new.recv().await.as_deref(), Some("first"));
        assert_eq!(new.recv().await.as_deref(), Some("second"));
        assert_eq!(
            timeout(Duration::from_secs(1), old.recv()).await.unwrap(),
            None,
            "the revoked lease ends instead of competing for frames"
        );
    }

    #[tokio::test]
    async fn dropping_a_revoked_lease_keeps_the_new_owner() {
        let mailbox = HttpMailbox::new();
        let old = mailbox.try_acquire().unwrap();
        let _new = mailbox.take_over();
        drop(old);
        assert!(mailbox.try_acquire().is_none());
    }

    #[tokio::test]
    async fn take_over_wakes_a_revoked_lease_parked_in_recv() {
        let mailbox = HttpMailbox::new();
        let mut old = mailbox.try_acquire().unwrap();
        let parked = tokio::spawn(async move { old.recv().await });
        sleep(Duration::from_millis(20)).await;
        let _new = mailbox.take_over();
        assert_eq!(timeout(Duration::from_secs(1), parked).await.unwrap().unwrap(), None);
    }
}
```

In `lib.rs`, add `#[cfg(feature = "server")] mod http_mailbox;` after `mod http_server;`.

**Step 2: Write the failing HTTP tests.** Add to `http_server.rs` `mod tests`. The
helpers go near the factories. Tagged frames are routed as `session/update`
notifications, so `route_outbound` picks the stream by `sessionId` exactly as in
production:

```rust
    fn capturing_registry() -> (
        Arc<ConnectionRegistry>,
        mpsc::UnboundedReceiver<RawJsonRpcMessage>,
    ) {
        let (forwarded_tx, forwarded_rx) = mpsc::unbounded_channel();
        let registry = Arc::new(ConnectionRegistry::new(Arc::new(CapturingAgentFactory {
            forwarded: forwarded_tx,
        })));
        (registry, forwarded_rx)
    }

    fn sse_get(connection_id: &str, session_id: Option<&str>, extra: &[(&str, &str)]) -> Request<Body> {
        let mut request = Request::builder()
            .method("GET")
            .uri("/acp")
            .header(header::ACCEPT, EVENT_STREAM_MIME_TYPE)
            .header(HEADER_CONNECTION_ID, connection_id);
        if let Some(session_id) = session_id {
            request = request.header(HEADER_SESSION_ID, session_id);
        }
        for (name, value) in extra {
            request = request.header(*name, *value);
        }
        request.body(Body::empty()).unwrap()
    }

    async fn push_tagged(connection: &Connection, session_id: Option<&str>, tag: &str) {
        let mut params = json!({ "tag": tag });
        if let Some(session_id) = session_id {
            params["sessionId"] = json!(session_id);
        }
        connection
            .route_outbound(TransportFrame::Single(
                RawJsonRpcMessage::notification("session/update".to_string(), params).unwrap(),
            ))
            .await
            .unwrap();
    }

    fn data_tags(body: &str) -> Vec<String> {
        body.lines()
            .filter_map(|line| line.strip_prefix("data: "))
            .filter_map(|data| serde_json::from_str::<serde_json::Value>(data).ok())
            .filter_map(|value| value["params"]["tag"].as_str().map(str::to_owned))
            .collect()
    }

    #[tokio::test]
    async fn sse_takeover_evicts_an_unpolled_subscriber() {
        for session_id in [None, Some("session-1")] {
            let (registry, mut forwarded_rx) = capturing_registry();
            let (connection_id, connection) = registry.create_connection().await;

            // #1 subscribes and its body is never polled: hyper's writer is stuck.
            let stuck = handle_get(registry.clone(), sse_get(&connection_id, session_id, &[])).await;
            assert_eq!(stuck.status(), StatusCode::OK);
            assert_eq!(stuck.headers()[HEADER_STREAM_FEATURES], STREAM_FEATURES);
            push_tagged(&connection, session_id, "before-takeover").await;

            // Without the header the 409 contract is unchanged.
            let refused = handle_get(registry.clone(), sse_get(&connection_id, session_id, &[])).await;
            assert_eq!(refused.status(), StatusCode::CONFLICT);
            assert_eq!(refused.headers()[HEADER_STREAM_FEATURES], STREAM_FEATURES);

            // With it, the new subscriber wins at once.
            let taken = timeout(
                Duration::from_millis(100),
                handle_get(registry.clone(), sse_get(&connection_id, session_id, &[(HEADER_TAKEOVER, "1")])),
            )
            .await
            .expect("takeover must not wait for the stuck stream");
            assert_eq!(taken.status(), StatusCode::OK);
            push_tagged(&connection, session_id, "after-takeover").await;

            // The new owner keeps the stream, also after the stale body is dropped.
            let again = handle_get(registry.clone(), sse_get(&connection_id, session_id, &[])).await;
            assert_eq!(again.status(), StatusCode::CONFLICT);
            drop(stuck);
            let again = handle_get(registry.clone(), sse_get(&connection_id, session_id, &[])).await;
            assert_eq!(again.status(), StatusCode::CONFLICT);

            // The agent task was never aborted.
            connection
                .send_frame_to_agent(TransportFrame::Single(
                    RawJsonRpcMessage::notification("test/ping".to_string(), json!({})).unwrap(),
                ))
                .unwrap();
            let ping = timeout(Duration::from_secs(1), forwarded_rx.recv()).await.unwrap().unwrap();
            assert_eq!(method_for_message(&ping), Some("test/ping"));

            // Nothing the stuck stream never dequeued is lost.
            connection.shutdown().await;
            let body = timeout(Duration::from_secs(1), axum::body::to_bytes(taken.into_body(), 64 * 1024))
                .await
                .unwrap()
                .unwrap();
            assert_eq!(
                data_tags(&String::from_utf8(body.to_vec()).unwrap()),
                ["before-takeover", "after-takeover"]
            );
        }
    }

    #[tokio::test]
    async fn sse_takeover_ends_a_still_polled_subscriber() {
        let (registry, _forwarded_rx) = capturing_registry();
        let (connection_id, _connection) = registry.create_connection().await;
        let old = handle_get(registry.clone(), sse_get(&connection_id, None, &[])).await;
        let _new = handle_get(registry.clone(), sse_get(&connection_id, None, &[(HEADER_TAKEOVER, "1")])).await;
        let body = timeout(Duration::from_secs(1), axum::body::to_bytes(old.into_body(), 1024))
            .await
            .expect("a revoked stream must end, not idle on keepalives")
            .unwrap();
        assert!(body.is_empty());
    }
```

**Step 3: Run them and watch them fail.** Run (long, log `gp-takeover-red`):
`nix develop -c cargo test -p agent-client-protocol-http --features server --lib`.
Expected: compile errors naming `HttpMailbox`, `take_over`, `HEADER_TAKEOVER`,
`HEADER_STREAM_FEATURES` or `STREAM_FEATURES`.

**Step 4: Implement.**

(a) `protocol.rs`. Put a notice line at the top of the file:
`// Modified for the goose fork (patched/v1.54.0): SSE takeover/replay header names. See ../VENDORED.md.`
Then add:

```rust
#[cfg(feature = "server")]
pub(crate) const HEADER_TAKEOVER: &str = "acp-takeover";
#[cfg(feature = "server")]
pub(crate) const HEADER_STREAM_FEATURES: &str = "acp-stream-features";
```

(b) `http_mailbox.rs`. Put this above the tests:

```rust
use std::{
    collections::VecDeque,
    sync::{Arc, Mutex as StdMutex, MutexGuard},
};

use tokio::sync::{Notify, mpsc::error::TryRecvError};

struct MailboxState {
    queue: VecDeque<String>,
    /// Lease that currently owns the stream. Takeover replaces it; a revoked lease's
    /// next dequeue sees the mismatch and ends, so it never has to be polled.
    owner: Option<u64>,
    next_lease_id: u64,
}

struct Shared {
    state: StdMutex<MailboxState>,
    notify: Notify,
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, MailboxState> {
        self.state.lock().expect("http mailbox lock poisoned")
    }
}

pub(crate) struct HttpMailbox {
    shared: Arc<Shared>,
}

pub(crate) struct HttpLease {
    shared: Arc<Shared>,
    id: u64,
}

impl HttpMailbox {
    pub(crate) fn new() -> Self {
        Self {
            shared: Arc::new(Shared {
                state: StdMutex::new(MailboxState { queue: VecDeque::new(), owner: None, next_lease_id: 0 }),
                notify: Notify::new(),
            }),
        }
    }

    #[allow(clippy::unnecessary_wraps)] // same Result contract as OutboundMailbox::push
    pub(crate) fn push(&self, msg: String) -> Result<(), &'static str> {
        self.shared.lock().queue.push_back(msg);
        self.shared.notify.notify_waiters();
        Ok(())
    }

    pub(crate) fn try_acquire(&self) -> Option<HttpLease> {
        let mut state = self.shared.lock();
        if state.owner.is_some() {
            return None;
        }
        Some(self.lease(&mut state))
    }

    /// Takes the stream from its current owner, if any. Works even if the old
    /// subscriber's SSE body is never polled again (a half-open socket).
    pub(crate) fn take_over(&self) -> HttpLease {
        let lease = {
            let mut state = self.shared.lock();
            self.lease(&mut state)
        };
        // Wake a revoked lease parked in `recv` so a still-polled old stream ends.
        self.shared.notify.notify_waiters();
        lease
    }

    fn lease(&self, state: &mut MailboxState) -> HttpLease {
        let id = state.next_lease_id;
        state.next_lease_id += 1;
        state.owner = Some(id);
        HttpLease { shared: self.shared.clone(), id }
    }
}

fn take_next(shared: &Shared, id: u64) -> Result<String, TryRecvError> {
    let mut state = shared.lock();
    if state.owner != Some(id) {
        return Err(TryRecvError::Disconnected);
    }
    state.queue.pop_front().ok_or(TryRecvError::Empty)
}

impl HttpLease {
    /// Next frame, or `None` once this lease has been taken over.
    pub(crate) async fn recv(&mut self) -> Option<String> {
        let shared = self.shared.clone();
        loop {
            // Register for a wakeup before checking, so a push in between is not missed.
            let mut notified = std::pin::pin!(shared.notify.notified());
            notified.as_mut().enable();
            match take_next(&shared, self.id) {
                Ok(msg) => return Some(msg),
                Err(TryRecvError::Disconnected) => return None,
                Err(TryRecvError::Empty) => notified.await,
            }
        }
    }

    pub(crate) fn try_recv(&mut self) -> Result<String, TryRecvError> {
        take_next(&self.shared, self.id)
    }
}

impl Drop for HttpLease {
    fn drop(&mut self) {
        // A revoked lease must not release its successor's ownership.
        if let Ok(mut state) = self.shared.state.lock()
            && state.owner == Some(self.id)
        {
            state.owner = None;
        }
    }
}
```

(c) `connection.rs`.
- Put a notice line at the top:
  `// Modified for the goose fork (patched/v1.54.0): HTTP streams use http_mailbox (takeover/replay); WebSocket path unchanged. See ../VENDORED.md.`
- Add `use crate::http_mailbox::{HttpLease, HttpMailbox};`.
- In `HttpOutbound`, change `connection_stream: OutboundMailbox` to `HttpMailbox` and
  `session_streams: RwLock<HashMap<String, Arc<OutboundMailbox>>>` to
  `Arc<HttpMailbox>`.
- In `HttpOutbound::new` and `session_stream` (`:298-333`), use `HttpMailbox::new()`
  and return `Arc<HttpMailbox>`.
- `Connection::subscribe_connection_stream`/`subscribe_session_stream` and their
  `OutboundTransport` twins now return `Option<HttpLease>`. Their bodies are
  unchanged: `http.connection_stream.try_acquire()` and
  `http.session_stream(id).await.try_acquire()`.
- `subscribe_all_outbound` keeps returning `Option<OutboundLease>`.
- Add the take-over pair to both types. On `Connection`, after
  `subscribe_session_stream`:

```rust
    pub(crate) fn take_over_connection_stream(&self) -> Option<HttpLease> {
        self.outbound_transport.take_over_connection_stream()
    }

    pub(crate) async fn take_over_session_stream(&self, session_id: &str) -> Option<HttpLease> {
        self.outbound_transport.take_over_session_stream(session_id).await
    }
```

  On `OutboundTransport`:

```rust
    fn take_over_connection_stream(&self) -> Option<HttpLease> {
        match self {
            Self::Http(http) => Some(http.connection_stream.take_over()),
            Self::WebSocket(_) => None,
        }
    }

    async fn take_over_session_stream(&self, session_id: &str) -> Option<HttpLease> {
        match self {
            Self::Http(http) => Some(http.session_stream(session_id).await.take_over()),
            Self::WebSocket(_) => None,
        }
    }
```

The existing tests that subscribe HTTP streams (`:947`, `:1005`, `:1047`, `:1077`,
`:1145`, `:1186`, and `slow_session_mailbox_does_not_stall_other_routes` `:680`) call
only `recv`, `try_recv`, `is_none` and `is_some`. They compile unchanged against
`HttpLease`.

(d) `http_server.rs`.
- Put a notice line at the top:
  `// Modified for the goose fork (patched/v1.54.0): Acp-Takeover, Acp-Stream-Features (and in patch 3, SSE ids / Last-Event-ID). See ../VENDORED.md.`
- Import `HEADER_TAKEOVER` and `HEADER_STREAM_FEATURES`, and add:

```rust
/// Advertised on every SSE GET /acp response (200 and 409) so clients can feature-detect.
pub(crate) const STREAM_FEATURES: &str = "takeover";

fn with_stream_features(mut response: Response) -> Response {
    response
        .headers_mut()
        .insert(HEADER_STREAM_FEATURES, HeaderValue::from_static(STREAM_FEATURES));
    response
}
```

- In `handle_get`, replace `:351-362` with:

```rust
    let session_id = header_value(request.headers(), HEADER_SESSION_ID);
    let takeover = header_value(request.headers(), HEADER_TAKEOVER).is_some_and(|v| v.trim() == "1");
    let receiver = match (session_id.as_deref(), takeover) {
        (Some(session_id), false) => connection.subscribe_session_stream(session_id).await,
        (Some(session_id), true) => connection.take_over_session_stream(session_id).await,
        (None, false) => connection.subscribe_connection_stream(),
        (None, true) => connection.take_over_connection_stream(),
    };
    let Some(mut receiver) = receiver else {
        return with_stream_features(
            (StatusCode::CONFLICT, "outbound stream already has a subscriber").into_response(),
        );
    };
```

- At `:399`, wrap the success response:
  `let mut response = with_stream_features(with_connection_header(…));`.
- The generator (`:364-397`) is unchanged. A revoked lease makes `try_recv()` return
  `Disconnected`, which ends the `while let Ok` loop, and makes `recv()` return
  `None`, which hits the existing `None => break`.

`lib.rs` gets the notice line too:
`// Modified for the goose fork (patched/v1.54.0): adds mod http_mailbox. See ../VENDORED.md.`

**Step 5: Run the tests and watch them pass.** Run (long, log `gp-takeover-green`),
the crate test command. Expected: all pass, the Task 3 count plus 7.
`sse_allows_one_active_subscriber_per_logical_stream` must pass unchanged, as must all
`websocket_server` tests.
- Run clippy (long, log `gp-takeover-clippy`). Expected: warnings ≤ baseline.
- Run (long, log `gp-takeover-goose`): `nix develop -c cargo check -p goose --features
  acp-http`. Expected: clean.
- Format only the touched files: `nix develop -c rustfmt --edition 2024
  vendor/agent-client-protocol-http/src/{http_mailbox,connection,http_server,protocol,lib}.rs`.

**Step 6: Prove the WebSocket path is untouched.**
```bash
cd "$FORK_WT"
git diff --exit-code HEAD -- vendor/agent-client-protocol-http/src/websocket_server.rs vendor/agent-client-protocol-http/src/server.rs
git diff -U0 HEAD -- vendor/agent-client-protocol-http/src/connection.rs | grep -nE '^[-+].*(OutboundMailbox|OutboundLease|WebSocketOutbound|all_outbound)' || echo "WS mailbox untouched"
```
Expected:
- The first command exits 0.
- The second prints `WS mailbox untouched`, or only the `HttpOutbound` field-type
  lines that *replace* `OutboundMailbox` with `HttpMailbox`. Check each by eye: no line
  inside `impl OutboundMailbox`, `impl OutboundLease`, `impl Drop for OutboundLease` or
  `WebSocketOutbound` may change.

**Step 7: Commit.**
```bash
git -C "$FORK_WT" add vendor/agent-client-protocol-http/src
git -C "$FORK_WT" commit -m "acp-http: Acp-Takeover: 1 on GET /acp takes a held SSE stream instead of answering 409 (HTTP mailbox only)"
```

---

### Task 5: fork — patch 3, replay ring and `Last-Event-ID`

**Wire contract (new; HTTP SSE only):**
- **Event ids.** Every frame on an HTTP stream carries `id: <seq>`. `seq` is a
  per-stream monotonic `u64`, starting at 1 and stamped at enqueue. The connection
  stream and each session stream count separately. Keepalives carry no id.
- **Ring.** When a frame is dequeued for writing, it is also appended to that stream's
  ring, atomically under the same lock. The ring keeps at most `REPLAY_RING_MAX_FRAMES
  = 4096` frames and `REPLAY_RING_MAX_BYTES = 1 MiB`, evicting oldest first.
- **Resume.** A request header **`Last-Event-ID: k`** on `GET /acp` does three things:
  1. It decides whether there is a gap.
  2. It drops ring entries `≤ k`, which is an implicit ack.
  3. It re-sends the retained entries `> k`, then continues live.

  The response carries **`Acp-Replay: complete`** if nothing after `k` is lost, else
  **`Acp-Replay: gap`**. It is a gap when an entry after `k` was evicted or already
  acked, when `k` is past the last issued seq, or when `k` is unparseable.
- **Without `Last-Event-ID`:** only the added `id:` lines differ.
- **Features header:** `Acp-Stream-Features` becomes `takeover, replay`.
- **Takeover plus resume:** combined with `Acp-Takeover: 1`, this recovers what a
  half-open stream had dequeued but not delivered. That is P2d's loss and P4b's lost
  permission.

**Memory (record in the findings):**
- **Per stream.** A ring holds up to 1 MiB until a `Last-Event-ID` acks it, and a
  healthy client acks only on reattach.
- **Mailboxes are never removed.** Session mailboxes (and their rings) last until the
  connection ends (`connection.rs:322-333`). A connection that touches many sessions
  accumulates rings.
- **honkd's worst case.** With 16 held sessions, one connection each, that is about
  16 MiB, plus up to 1 MiB per connection stream. Compare A14's measured 0.5-1.7 MB RSS
  per active session: the rings could dominate goose's per-session cost by up to about
  10×.
- **Why it is acceptable.** It is bounded, and honkd's idle-TTL DELETE frees it.
- **Revisit after P2f** records actual turn sizes. A 256-512 KiB cap would likely
  suffice.

**Files:** `vendor/agent-client-protocol-http/src/{http_mailbox.rs,http_server.rs,protocol.rs}`.
`connection.rs` is untouched in this task.

**Step 1: Write the failing mailbox tests** in `http_mailbox.rs` `mod tests`:

```rust
    #[tokio::test]
    async fn resume_replays_dequeued_frames_after_last_seen() {
        let mailbox = HttpMailbox::new();
        for n in 1..=3 {
            mailbox.push(format!("m{n}")).unwrap();
        }
        let mut first = mailbox.try_acquire().unwrap();
        for n in 1..=3u64 {
            assert_eq!(first.recv_frame().await, Some((n, format!("m{n}"))));
        }
        drop(first); // written to a socket, but the client only processed m1
        mailbox.push("m4".to_string()).unwrap();

        let mut resumed = mailbox.try_acquire().unwrap();
        assert_eq!(resumed.resume_after(1), Replay::Complete);
        for n in 2..=4u64 {
            assert_eq!(resumed.recv_frame().await, Some((n, format!("m{n}"))));
        }
        assert!(matches!(resumed.try_recv_frame(), Err(TryRecvError::Empty)));
    }

    #[tokio::test]
    async fn resume_below_the_ring_floor_is_a_gap() {
        let mailbox = HttpMailbox::with_ring_limits(2, usize::MAX);
        for n in 1..=4 {
            mailbox.push(format!("m{n}")).unwrap();
        }
        let mut first = mailbox.try_acquire().unwrap();
        for _ in 0..4 {
            first.recv_frame().await.unwrap();
        }
        drop(first);

        let mut resumed = mailbox.try_acquire().unwrap();
        assert_eq!(resumed.resume_after(0), Replay::Gap);
        assert_eq!(resumed.recv_frame().await, Some((3, "m3".to_string())));
        assert_eq!(resumed.recv_frame().await, Some((4, "m4".to_string())));
    }

    #[tokio::test]
    async fn ring_respects_the_byte_budget() {
        let mailbox = HttpMailbox::with_ring_limits(usize::MAX, 10);
        mailbox.push("aaaaaa".to_string()).unwrap();
        mailbox.push("bbbbbb".to_string()).unwrap();
        let mut lease = mailbox.try_acquire().unwrap();
        lease.recv_frame().await.unwrap();
        lease.recv_frame().await.unwrap(); // 12 bytes > 10: seq 1 is evicted at this dequeue
        drop(lease);

        let mut resumed = mailbox.try_acquire().unwrap();
        assert_eq!(resumed.resume_after(0), Replay::Gap);
        assert_eq!(resumed.recv_frame().await, Some((2, "bbbbbb".to_string())));
    }

    #[tokio::test]
    async fn resume_acknowledges_so_an_older_resume_is_a_gap() {
        let mailbox = HttpMailbox::new();
        for n in 1..=3 {
            mailbox.push(format!("m{n}")).unwrap();
        }
        let mut lease = mailbox.try_acquire().unwrap();
        for _ in 0..3 {
            lease.recv_frame().await.unwrap();
        }
        drop(lease);
        let mut acked = mailbox.try_acquire().unwrap();
        assert_eq!(acked.resume_after(2), Replay::Complete);
        drop(acked);
        let mut stale = mailbox.try_acquire().unwrap();
        assert_eq!(stale.resume_after(1), Replay::Gap);
    }

    #[tokio::test]
    async fn resume_past_the_last_issued_seq_is_a_gap() {
        let mailbox = HttpMailbox::new();
        mailbox.push("m1".to_string()).unwrap();
        let mut lease = mailbox.try_acquire().unwrap();
        assert_eq!(lease.resume_after(9), Replay::Gap);
    }

    #[tokio::test]
    async fn take_over_then_resume_recovers_what_the_revoked_lease_dequeued() {
        let mailbox = HttpMailbox::new();
        mailbox.push("m1".to_string()).unwrap();
        mailbox.push("m2".to_string()).unwrap();
        let mut stuck = mailbox.try_acquire().unwrap();
        stuck.recv_frame().await.unwrap();
        stuck.recv_frame().await.unwrap(); // dequeued into a socket nobody reads
        let mut taken = mailbox.take_over();
        assert_eq!(taken.resume_after(0), Replay::Complete);
        assert_eq!(taken.recv_frame().await, Some((1, "m1".to_string())));
        assert_eq!(taken.recv_frame().await, Some((2, "m2".to_string())));
        drop(stuck);
    }
```

**Step 2: Write the failing HTTP tests** in `http_server.rs` `mod tests`. They reuse
the Task 4 helpers:

```rust
    fn event_ids(body: &str) -> Vec<u64> {
        body.lines()
            .filter_map(|line| line.strip_prefix("id: "))
            .map(|id| id.parse().unwrap())
            .collect()
    }

    #[tokio::test]
    async fn sse_frames_carry_ids_and_last_event_id_replays_the_rest() {
        let (registry, _forwarded_rx) = capturing_registry();
        let (connection_id, connection) = registry.create_connection().await;
        for tag in ["one", "two", "three"] {
            push_tagged(&connection, None, tag).await;
        }
        let first = handle_get(registry.clone(), sse_get(&connection_id, None, &[])).await;
        assert_eq!(first.headers()[HEADER_STREAM_FEATURES], "takeover, replay");
        assert!(first.headers().get(HEADER_REPLAY).is_none());
        let mut body = first.into_body().into_data_stream();
        for _ in 0..3 {
            timeout(Duration::from_secs(1), body.next()).await.unwrap().unwrap().unwrap();
        }
        drop(body); // goose wrote all three; the client only processed id 1
        push_tagged(&connection, None, "four").await;

        let resumed = handle_get(registry.clone(), sse_get(&connection_id, None, &[(HEADER_LAST_EVENT_ID, "1")])).await;
        assert_eq!(resumed.status(), StatusCode::OK);
        assert_eq!(resumed.headers()[HEADER_REPLAY], "complete");
        connection.shutdown().await;
        let body = String::from_utf8(axum::body::to_bytes(resumed.into_body(), 64 * 1024).await.unwrap().to_vec()).unwrap();
        assert_eq!(event_ids(&body), [2, 3, 4]);
        assert_eq!(data_tags(&body), ["two", "three", "four"]);
    }

    #[tokio::test]
    async fn sse_last_event_id_older_than_the_ring_flags_a_gap() {
        let (registry, _forwarded_rx) = capturing_registry();
        let (connection_id, connection) = registry.create_connection().await;
        let total = crate::http_mailbox::REPLAY_RING_MAX_FRAMES + 8;
        for n in 0..total {
            connection.push_connection_stream_for_test(format!("m{n}")).unwrap();
        }
        let first = handle_get(registry.clone(), sse_get(&connection_id, None, &[])).await;
        let mut body = first.into_body().into_data_stream();
        timeout(Duration::from_secs(5), async {
            for _ in 0..total {
                body.next().await.unwrap().unwrap();
            }
        })
        .await
        .unwrap();
        drop(body);

        let resumed = handle_get(registry.clone(), sse_get(&connection_id, None, &[(HEADER_LAST_EVENT_ID, "0")])).await;
        assert_eq!(resumed.headers()[HEADER_REPLAY], "gap");
        connection.shutdown().await;
        let body = String::from_utf8(axum::body::to_bytes(resumed.into_body(), 4 * 1024 * 1024).await.unwrap().to_vec()).unwrap();
        let ids = event_ids(&body);
        assert_eq!(ids.len(), crate::http_mailbox::REPLAY_RING_MAX_FRAMES);
        assert_eq!(ids.first().copied(), Some(9), "seqs 1..=8 were evicted");
    }

    #[tokio::test]
    async fn sse_takeover_with_last_event_id_recovers_frames_a_stuck_stream_dequeued() {
        for session_id in [None, Some("session-1")] {
            let (registry, _forwarded_rx) = capturing_registry();
            let (connection_id, connection) = registry.create_connection().await;
            push_tagged(&connection, session_id, "one").await;
            push_tagged(&connection, session_id, "two").await;

            // goose writes both frames into a socket the client never reads.
            let stuck = handle_get(registry.clone(), sse_get(&connection_id, session_id, &[])).await;
            let mut stuck_body = stuck.into_body().into_data_stream();
            for _ in 0..2 {
                timeout(Duration::from_secs(1), stuck_body.next()).await.unwrap().unwrap().unwrap();
            }
            push_tagged(&connection, session_id, "three").await;

            let taken = handle_get(
                registry.clone(),
                sse_get(&connection_id, session_id, &[(HEADER_TAKEOVER, "1"), (HEADER_LAST_EVENT_ID, "0")]),
            )
            .await;
            assert_eq!(taken.status(), StatusCode::OK);
            assert_eq!(taken.headers()[HEADER_REPLAY], "complete");
            connection.shutdown().await;
            let body = String::from_utf8(axum::body::to_bytes(taken.into_body(), 64 * 1024).await.unwrap().to_vec()).unwrap();
            assert_eq!(event_ids(&body), [1, 2, 3]);
            assert_eq!(data_tags(&body), ["one", "two", "three"]);
            drop(stuck_body); // the half-open socket lingers; harmless
        }
    }
```

**Step 3: Run them and watch them fail.** Run (long, log `gp-replay-red`). Expected:
compile errors naming `recv_frame`, `resume_after`, `Replay`, `with_ring_limits`,
`REPLAY_RING_MAX_FRAMES` or `HEADER_LAST_EVENT_ID`/`HEADER_REPLAY`.

**Step 4: Implement.**

(a) In `protocol.rs`:

```rust
#[cfg(feature = "server")]
pub(crate) const HEADER_LAST_EVENT_ID: &str = "last-event-id";
#[cfg(feature = "server")]
pub(crate) const HEADER_REPLAY: &str = "acp-replay";
```

(b) In `http_mailbox.rs`, extend the module:

```rust
/// Per-stream replay ring caps (frames dequeued for writing, kept for Last-Event-ID).
pub(crate) const REPLAY_RING_MAX_FRAMES: usize = 4096;
pub(crate) const REPLAY_RING_MAX_BYTES: usize = 1024 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Replay {
    /// Every frame after the client's last id is being re-sent or is still queued.
    Complete,
    /// Some frame after the client's last id is gone; the client must reconcile.
    Gap,
}
```

The new `MailboxState`:

```rust
struct MailboxState {
    queue: VecDeque<(u64, String)>,
    ring: VecDeque<(u64, String)>,
    ring_bytes: usize,
    max_ring_frames: usize,
    max_ring_bytes: usize,
    /// Highest seq removed from the ring (evicted or acknowledged).
    trimmed_through: u64,
    next_seq: u64,
    owner: Option<u64>,
    next_lease_id: u64,
}

impl MailboxState {
    /// Under the lock, at the moment a frame is dequeued for writing.
    fn record_dequeued(&mut self, seq: u64, msg: &str) {
        self.ring.push_back((seq, msg.to_owned()));
        self.ring_bytes += msg.len();
        while self.ring.len() > self.max_ring_frames || self.ring_bytes > self.max_ring_bytes {
            let Some((evicted, old)) = self.ring.pop_front() else { break };
            self.ring_bytes -= old.len();
            self.trimmed_through = evicted;
        }
    }
}
```

The constructors: `HttpMailbox::new()` calls `Self::with_ring_limits(REPLAY_RING_MAX_FRAMES,
REPLAY_RING_MAX_BYTES)`. `pub(crate) fn with_ring_limits(max_frames: usize, max_bytes:
usize) -> Self` builds the state with `next_seq: 1`; it is non-test code, because `new`
uses it.

`push` stamps the sequence number:

```rust
    pub(crate) fn push(&self, msg: String) -> Result<(), &'static str> {
        {
            let mut state = self.shared.lock();
            let seq = state.next_seq;
            state.next_seq += 1;
            state.queue.push_back((seq, msg));
        }
        self.shared.notify.notify_waiters();
        Ok(())
    }
```

`HttpLease` gains `replay: VecDeque<(u64, String)>`, which starts empty in `lease()`.
Then replace `take_next` and the lease methods:

```rust
fn take_next(
    shared: &Shared,
    id: u64,
    replay: &mut VecDeque<(u64, String)>,
) -> Result<(u64, String), TryRecvError> {
    let mut state = shared.lock();
    if state.owner != Some(id) {
        return Err(TryRecvError::Disconnected);
    }
    if let Some(frame) = replay.pop_front() {
        return Ok(frame); // already in the ring; do not record twice
    }
    let (seq, msg) = state.queue.pop_front().ok_or(TryRecvError::Empty)?;
    state.record_dequeued(seq, &msg);
    Ok((seq, msg))
}

impl HttpLease {
    /// Call once, before the first recv. Acknowledges everything up to `last_seen`
    /// and queues the retained frames after it ahead of live frames.
    pub(crate) fn resume_after(&mut self, last_seen: u64) -> Replay {
        let mut state = self.shared.lock();
        let gap = last_seen < state.trimmed_through || last_seen >= state.next_seq;
        while state.ring.front().is_some_and(|(seq, _)| *seq <= last_seen) {
            let (seq, msg) = state.ring.pop_front().expect("front checked");
            state.ring_bytes -= msg.len();
            state.trimmed_through = state.trimmed_through.max(seq);
        }
        self.replay = state.ring.iter().cloned().collect();
        if gap { Replay::Gap } else { Replay::Complete }
    }

    pub(crate) async fn recv_frame(&mut self) -> Option<(u64, String)> {
        let shared = self.shared.clone();
        loop {
            let mut notified = std::pin::pin!(shared.notify.notified());
            notified.as_mut().enable();
            match take_next(&shared, self.id, &mut self.replay) {
                Ok(frame) => return Some(frame),
                Err(TryRecvError::Disconnected) => return None,
                Err(TryRecvError::Empty) => notified.await,
            }
        }
    }

    pub(crate) fn try_recv_frame(&mut self) -> Result<(u64, String), TryRecvError> {
        take_next(&self.shared, self.id, &mut self.replay)
    }

    pub(crate) async fn recv(&mut self) -> Option<String> {
        self.recv_frame().await.map(|(_, msg)| msg)
    }

    pub(crate) fn try_recv(&mut self) -> Result<String, TryRecvError> {
        self.try_recv_frame().map(|(_, msg)| msg)
    }
}
```

`last_seen >= next_seq` means "past the last issued seq", because seqs start at 1.

(c) In `http_server.rs`:
- `STREAM_FEATURES` becomes `"takeover, replay"`.
- Import `HEADER_LAST_EVENT_ID`, `HEADER_REPLAY` and `crate::http_mailbox::Replay`.
- In `handle_get`, after the receiver is acquired and before `subscribe_closed`, add:

```rust
    let replay = request.headers().get(HEADER_LAST_EVENT_ID).map(|value| {
        match value.to_str().ok().and_then(|v| v.trim().parse::<u64>().ok()) {
            Some(last_seen) => receiver.resume_after(last_seen),
            None => {
                receiver.resume_after(0);
                Replay::Gap // unparseable id: send what is retained, but flag it
            }
        }
    });
```

- In the generator, `try_recv()` becomes `try_recv_frame()` and `recv()` becomes
  `recv_frame()`. Bind `(seq, msg)` and yield `sse_event(seq, msg)` at all four sites
  (`:368`, `:373`, `:382`, `:390`), using:

```rust
fn sse_event(seq: u64, msg: String) -> Event {
    Event::default().id(seq.to_string()).data(msg)
}
```

- After the session-id header insertion (`:409-413`), add:

```rust
    if let Some(replay) = replay {
        response.headers_mut().insert(
            HEADER_REPLAY,
            HeaderValue::from_static(match replay {
                Replay::Complete => "complete",
                Replay::Gap => "gap",
            }),
        );
    }
```

- Extend the notice line at the top of `http_mailbox.rs`/`http_server.rs` to mention
  the replay ring and `Last-Event-ID`.

**Step 5: Update the three exact-byte assertions** that the new `id:` lines change.
axum writes fields in call order: `id: N\ndata: …\n\n`.
- `:1355-1358` becomes `"id: 1\ndata: already-read\n\n"`.
- `:1375-1382` becomes `"id: 2\ndata: queued-before-drop-1\n\n"`, `"id: 3\ndata:
  queued-before-drop-2\n\n"`, `"id: 4\ndata: queued-after-drop\n\n"`.
- `:1447-1450` becomes `"id: 1\ndata: queued-before-subscribe\n\n"`.

The burst test (`:1261`) filters `data: ` lines and is unaffected. The WS tests are
unaffected: no WS code changed.

**Step 6: Run the tests and watch them pass.** Run (long, log `gp-replay-green`).
Expected: all pass, the Task 4 count plus 9. Any failure other than the three
assertions above is a regression: fix the code, not the assertion.
- Run clippy (≤ baseline) and `cargo check -p goose --features acp-http`.
- `rustfmt --edition 2024` the touched files.
- Repeat Task 4 Step 6. The WS path must still be untouched.

**Step 7: Commit.**
```bash
git -C "$FORK_WT" add vendor/agent-client-protocol-http/src
git -C "$FORK_WT" commit -m "acp-http: SSE event ids, a bounded per-stream replay ring, and Last-Event-ID resume (HTTP mailbox only)"
```

---

### Task 6: fork — release workflow runs the new tests and checks the vendoring

**Files:** `$FORK_WT/.github/workflows/patched-release.yml` (`test-patches` job). The
workflow is read from the tagged commit, so this must land before tagging.

**Step 1: Edit.** Insert this step before `Test patched modules`:

```yaml
      - name: Vendored acp-http is the one in the build graph
        # An unused [patch] is only a cargo warning; fail loudly instead. --locked also
        # fails if Cargo.lock is stale (build-cli-linux.yml does not pass --locked).
        run: |
          cargo tree --locked -p goose-cli --features acp-http -i agent-client-protocol-http --depth 0 \
            | tee /dev/stderr | grep -F 'vendor/agent-client-protocol-http'
```

Replace `Test patched modules` with:

```yaml
      - name: Test patched modules
        run: |
          F=code-mode,tree-sitter,aws-providers,telemetry,otel,rustls-tls,live-voice,system-keyring,platform-apps,chat-recall,scheduler,acp-http,online-model-meta
          cargo test --release -p goose --features "$F" --lib -- gcpvertexai gcpauth process_group
          cargo test --release -p goose-cli --lib -- cache_ttl
          cargo test --release -p agent-client-protocol-http --features server --lib
```

Update the job comment: `# Unit tests for the patched code paths (including the
vendored acp-http crate), on the same toolchain.`

**Step 2: Verify locally.**
- Run the `cargo tree` command above (short). Expected: one line ending in
  `vendor/agent-client-protocol-http)`.
- Run (long, log `gp-ci-parity`) the exact `F=…` goose line, with `--release`.
  Expected: the `process_group` tests pass under the release feature set. If it fails
  on missing system libraries (protobuf, cmake or clang for `code-mode`/`live-voice`),
  that is the dev shell, not the patch. Record it and rely on the CI job.

**Step 3: Commit.**
```bash
git -C "$FORK_WT" add .github/workflows/patched-release.yml
git -C "$FORK_WT" commit -m "ci: run the process-group and vendored acp-http tests, and check the vendored crate is in the graph, before releasing"
```

---

### Task 7: honk — `honk-upstream` carries event ids, resumes explicitly, exposes stream features

**Files:**
- Modify: `crates/honk-upstream/src/sse.rs`.
- Modify: `crates/honk-upstream/src/conn.rs`:
  - `Event` `:32-58`
  - struct `:73-89`
  - constructors `:96-204`
  - `buffered_frame_count` `:231-236`
  - `subscribe` `:394-499`
  - `call_full` `:543-580`
  - tests `:740-801`
- Modify: `crates/honk-upstream/src/lib.rs` (re-exports).
- Modify: `crates/honk-upstream/tests/*.rs`, a mechanical `Event::Frame` arity update.

**API (what honkd will build on):**
- **`Event::Frame(StreamKey, Value, Option<u64>)`.** The third field is the SSE event
  id, if goose sent one. `Event::event_id()` returns it. honkd advances its persisted
  "last processed id" only after it has *processed* a frame. That is why the id
  travels with the frame instead of living in a library-side map.
- **`SubscribeOpts { takeover: bool, resume_after: Option<u64> }`.** `Last-Event-ID` is
  sent **only** when `resume_after` is `Some`; there is no implicit 0. To resume a
  stream it has never seen an id on, the caller must pass `Some(0)` explicitly.
- **`ReplayOutcome`** (`NotRequested | Unsupported | Complete | Gap`), with
  `needs_reconcile()`. Only an explicit server `complete` skips reconciliation.
- **`StreamFeatures { takeover, replay }`**, with `is_patched()`, read from
  `Acp-Stream-Features` on every GET (200 or 409). `Upstream::stream_features()`
  returns the latest. M1's honkd uses it to refuse unpatched upstream (see
  "Capability and feature detection").
- **Defensive dedupe.** Within one subscription, the reader drops a frame whose id is
  `≤` the highest id seen so far, starting from `resume_after`.
  - eventsource-stream copies the last `id:` onto later events that carry none. An id
    therefore counts as present only when it **differs** from the previous event's.
  - This way an id-less event can never be mistaken for a duplicate.
- **`subscribe()` is unchanged in signature and sends no new headers.** Its recorded
  marker text is byte-identical, because old probes read it.

**Step 1: Write the failing tests.**

In `sse.rs` `mod tests`:

```rust
    #[tokio::test]
    async fn frame_ids_are_parsed_and_absent_ids_are_none() {
        let chunks: Vec<Result<Bytes, std::io::Error>> = vec![
            Ok(Bytes::from("data: {\"a\":1}\n\n")),
            Ok(Bytes::from("id: 7\ndata: {\"b\":2}\n\n")),
            Ok(Bytes::from("data:\n\n")),
        ];
        let frames: Vec<SseFrame> =
            sse_to_frames(futures::stream::iter(chunks)).map(|r| r.unwrap()).collect().await;
        assert_eq!(
            frames,
            vec![
                SseFrame { id: None, data: json!({"a": 1}) },
                SseFrame { id: Some(7), data: json!({"b": 2}) },
            ]
        );
    }

    #[tokio::test]
    async fn an_inherited_id_is_not_reported_as_new() {
        let chunks: Vec<Result<Bytes, std::io::Error>> = vec![
            Ok(Bytes::from("id: 5\ndata: {\"a\":1}\n\n")),
            Ok(Bytes::from("data: {\"b\":2}\n\n")),
            Ok(Bytes::from("id: 6\ndata: {\"c\":3}\n\n")),
        ];
        let ids: Vec<Option<u64>> = sse_to_frames(futures::stream::iter(chunks))
            .map(|r| r.unwrap().id)
            .collect()
            .await;
        assert_eq!(ids, vec![Some(5), None, Some(6)]);
    }
```

In `conn.rs` `mod tests`:

```rust
    use reqwest::header::{HeaderMap, HeaderValue};

    #[test]
    fn stream_features_parse_from_header() {
        let mut h = HeaderMap::new();
        assert_eq!(StreamFeatures::from_headers(&h), StreamFeatures::default());
        assert!(!StreamFeatures::from_headers(&h).is_patched());
        h.insert("acp-stream-features", HeaderValue::from_static("takeover"));
        assert_eq!(StreamFeatures::from_headers(&h), StreamFeatures { takeover: true, replay: false });
        h.insert("acp-stream-features", HeaderValue::from_static("takeover, replay, later-thing"));
        assert!(StreamFeatures::from_headers(&h).is_patched());
    }

    #[test]
    fn replay_outcome_trusts_only_an_explicit_complete() {
        let mut h = HeaderMap::new();
        assert_eq!(ReplayOutcome::from_headers(false, &h), ReplayOutcome::NotRequested);
        assert_eq!(ReplayOutcome::from_headers(true, &h), ReplayOutcome::Unsupported);
        h.insert("acp-replay", HeaderValue::from_static("gap"));
        assert_eq!(ReplayOutcome::from_headers(true, &h), ReplayOutcome::Gap);
        h.insert("acp-replay", HeaderValue::from_static("complete"));
        assert_eq!(ReplayOutcome::from_headers(true, &h), ReplayOutcome::Complete);
        assert!(!ReplayOutcome::Complete.needs_reconcile());
        for outcome in [ReplayOutcome::NotRequested, ReplayOutcome::Unsupported, ReplayOutcome::Gap] {
            assert!(outcome.needs_reconcile());
        }
    }

    /// Accepts one GET, returns its lowercased request head, answers with an SSE body.
    async fn one_shot_sse_server(
        response_headers: &'static str,
        body: &'static str,
    ) -> (String, tokio::task::JoinHandle<String>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/acp", listener.local_addr().unwrap());
        let handle = tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = vec![0u8; 8192];
            let mut head = String::new();
            while !head.contains("\r\n\r\n") {
                let n = sock.read(&mut buf).await.unwrap();
                if n == 0 {
                    break;
                }
                head.push_str(&String::from_utf8_lossy(&buf[..n]));
            }
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n{response_headers}\r\n{body}"
            );
            sock.write_all(response.as_bytes()).await.unwrap();
            let _ = sock.shutdown().await;
            head.to_ascii_lowercase()
        });
        (url, handle)
    }

    #[tokio::test]
    async fn subscribe_with_sends_takeover_and_resume_and_drops_replayed_duplicates() {
        let (url, server) = one_shot_sse_server(
            "acp-stream-features: takeover, replay\r\nacp-replay: complete\r\n",
            "id: 42\ndata: {\"n\":42}\n\nid: 43\ndata: {\"n\":43}\n\n",
        )
        .await;
        let mut up = Upstream::resume(&url, "s".into(), "c".into(), Recorder::disabled()).unwrap();
        let out = up
            .subscribe_with(None, SubscribeOpts { takeover: true, resume_after: Some(42) })
            .await
            .unwrap();
        let head = server.await.unwrap();
        assert!(head.contains("acp-takeover: 1"), "{head}");
        assert!(head.contains("last-event-id: 42"), "{head}");
        assert!(out.took_over);
        assert_eq!(out.replay, ReplayOutcome::Complete);
        assert_eq!(up.stream_features(), Some(StreamFeatures { takeover: true, replay: true }));
        let first = up.next(Duration::from_secs(1)).await.unwrap().unwrap();
        assert_eq!(first.frame(), Some(&serde_json::json!({"n": 43})), "id 42 was already processed");
        assert_eq!(first.event_id(), Some(43));
    }

    #[tokio::test]
    async fn subscribe_without_resume_sends_no_last_event_id() {
        let (url, server) = one_shot_sse_server("", "data: {\"n\":1}\n\n").await;
        let mut up = Upstream::resume(&url, "s".into(), "c".into(), Recorder::disabled()).unwrap();
        let out = up
            .subscribe_with(None, SubscribeOpts { takeover: true, resume_after: None })
            .await
            .unwrap();
        let head = server.await.unwrap();
        assert!(!head.contains("last-event-id"), "{head}");
        assert!(!out.took_over, "an unpatched server cannot honour takeover");
        assert_eq!(out.replay, ReplayOutcome::NotRequested);
        assert_eq!(up.stream_features(), Some(StreamFeatures::default()));
        let first = up.next(Duration::from_secs(1)).await.unwrap().unwrap();
        assert_eq!(first.event_id(), None);
    }
```

Run `nix develop -c cargo test -p honk-upstream --lib`. Expected: compile errors.

**Step 2: Implement.**

`sse.rs`:

```rust
/// One SSE event: its JSON payload and its `id:` field if this event set a new one.
#[derive(Debug, Clone, PartialEq)]
pub struct SseFrame {
    pub id: Option<u64>,
    pub data: Value,
}

/// Like [`sse_to_json_stream`], but keeps each event's id (patched goose stamps one).
pub fn sse_to_frames<S, E>(byte_stream: S) -> BoxStream<'static, anyhow::Result<SseFrame>>
where
    S: Stream<Item = Result<Bytes, E>> + Send + 'static,
    E: std::error::Error + Send + Sync + 'static,
{
    let mut previous_id = String::new();
    byte_stream
        .eventsource()
        .filter_map(move |event_res| {
            let item = match event_res {
                Ok(event) => {
                    // eventsource-stream carries the last `id:` onto later events that
                    // set none; only an id that differs from the previous one is new.
                    let id = if event.id != previous_id { event.id.trim().parse().ok() } else { None };
                    previous_id.clone_from(&event.id);
                    let data = event.data.trim();
                    if data.is_empty() {
                        None
                    } else {
                        Some(
                            serde_json::from_str::<Value>(data)
                                .with_context(|| format!("failed to parse SSE data as JSON: {data}"))
                                .map(|data| SseFrame { id, data }),
                        )
                    }
                }
                Err(err) => Some(Err(anyhow::anyhow!("SSE stream error: {err}"))),
            };
            futures::future::ready(item)
        })
        .boxed()
}
```

`sse_to_json_stream` becomes
`sse_to_frames(byte_stream).map(|r| r.map(|f| f.data)).boxed()`.

`conn.rs`. Change `Event`:

```rust
pub enum Event {
    /// A JSON-RPC frame received from an SSE stream, with its SSE event id if goose
    /// sent one (patched-1.54.0.3+). Persist the id only after processing the frame.
    Frame(StreamKey, Value, Option<u64>),
    StreamEnded { key: StreamKey, error: Option<String> },
}
```

Update the `key`/`frame` matches, and add:

```rust
    pub fn event_id(&self) -> Option<u64> {
        match self {
            Event::Frame(_, _, id) => *id,
            Event::StreamEnded { .. } => None,
        }
    }
```

Then update every `Event::Frame(a, b)` pattern and constructor in `conn.rs`
(`buffered_frame_count`, `call_full` `:564`/`:574`, the tests `:747-773`) to three
fields (`_` or `None`).

New public types, after `rpc_id_matches`:

```rust
/// What a goose `GET /acp` advertises in `Acp-Stream-Features` (patched-1.54.0.3+).
/// Unpatched goose sends no header: both false.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct StreamFeatures {
    pub takeover: bool,
    pub replay: bool,
}

impl StreamFeatures {
    pub fn from_headers(headers: &reqwest::header::HeaderMap) -> Self {
        let mut features = Self::default();
        if let Some(value) = headers.get("acp-stream-features").and_then(|v| v.to_str().ok()) {
            for token in value.split(',').map(str::trim) {
                match token {
                    "takeover" => features.takeover = true,
                    "replay" => features.replay = true,
                    _ => {}
                }
            }
        }
        features
    }

    /// Everything honkd's lossless-reattach design needs.
    pub fn is_patched(self) -> bool {
        self.takeover && self.replay
    }
}

/// Per-call subscribe options. Safe against unpatched goose: it ignores both headers.
#[derive(Debug, Clone, Copy, Default)]
pub struct SubscribeOpts {
    /// Send `Acp-Takeover: 1`: evict a stuck subscriber instead of retrying 409.
    pub takeover: bool,
    /// Send `Last-Event-ID: <n>`: replay what follows the last *processed* id.
    /// `None` sends nothing; use `Some(0)` to replay everything retained.
    pub resume_after: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReplayOutcome {
    NotRequested,
    /// Resume was requested but the server did not answer `Acp-Replay` (unpatched).
    Unsupported,
    Complete,
    Gap,
}

impl ReplayOutcome {
    pub fn from_headers(requested: bool, headers: &reqwest::header::HeaderMap) -> Self {
        if !requested {
            return Self::NotRequested;
        }
        match headers.get("acp-replay").and_then(|v| v.to_str().ok()).map(str::trim) {
            Some("complete") => Self::Complete,
            Some("gap") => Self::Gap,
            _ => Self::Unsupported,
        }
    }

    /// Whether the caller must reconcile (export / same-connection session/load, A3/A5).
    pub fn needs_reconcile(self) -> bool {
        !matches!(self, Self::Complete)
    }
}

#[derive(Debug, Clone, Copy)]
pub struct SubscribeOutcome {
    pub conflict_window: Duration,
    /// The server honoured `Acp-Takeover` (it advertises takeover and we asked).
    pub took_over: bool,
    pub replay: ReplayOutcome,
    pub features: StreamFeatures,
}
```

`Upstream` changes:
- Add a field `features: Option<StreamFeatures>`, initialized to `None` in both
  constructors.
- Add an accessor:
  `pub fn stream_features(&self) -> Option<StreamFeatures> { self.features }`. It
  returns the features from the last GET, or `None` before the first.

Turn the body of `subscribe` into `pub async fn subscribe_with(&mut self, session:
Option<&str>, opts: SubscribeOpts) -> anyhow::Result<SubscribeOutcome>`. Then
`subscribe(session)` becomes
`Ok(self.subscribe_with(session, SubscribeOpts::default()).await?.conflict_window)`.

Inside `subscribe_with`:
- **Request headers.** After the `acp-session-id` header, add:
  ```rust
  if opts.takeover { req = req.header("acp-takeover", "1"); }
  if let Some(last) = opts.resume_after { req = req.header("last-event-id", last.to_string()); }
  ```
- **Features.** Right after `send()`:
  `let features = StreamFeatures::from_headers(res.headers()); self.features = Some(features);`.
  This runs on 409 too.
- **Outcome.** On success:
  `let replay = ReplayOutcome::from_headers(opts.resume_after.is_some(), res.headers());`
  and `let took_over = opts.takeover && features.takeover;`.
- **Markers.** Keep the existing `subscribed to …` marker exactly. Add a second
  marker only when `opts.takeover || opts.resume_after.is_some()`:
  `format!("stream options: took_over={took_over} resume_after={:?} replay={replay:?} features={features:?}", opts.resume_after)`.
- **Reader task.** Use `crate::sse::sse_to_frames`. Move `let mut floor =
  opts.resume_after;` into the task, and replace the `Ok(val)` arm with:
  ```rust
  Ok(SseFrame { id, data }) => {
      if let Some(id) = id {
          if floor.is_some_and(|f| id <= f) {
              rec.record_marker(&s_name, format!("dropped duplicate event id {id}"));
              continue;
          }
          floor = Some(id);
      }
      rec.record(Direction::In, &s_name, None, Some(data.clone()), id.map(|id| format!("event id {id}")));
      if tx.send(Event::Frame(key.clone(), data, id)).is_err() {
          return;
      }
  }
  ```
- **Return** `Ok(SubscribeOutcome { conflict_window, took_over, replay, features })`.

`lib.rs`:
`pub use conn::{Event, ReplayOutcome, StreamFeatures, StreamKey, SubscribeOpts, SubscribeOutcome, Upstream, rpc_id_matches};`
and `pub use sse::{SseFrame, sse_to_frames, sse_to_json_stream};`.

**Step 3: Update the integration tests mechanically.** `cargo test --workspace
--no-run` lists every site; there are about 50 in `tests/*.rs`. Patterns gain a `_`:
`Event::Frame(_, ref frame)` becomes `Event::Frame(_, ref frame, _)`, and
`Event::Frame(key, val)` becomes `Event::Frame(key, val, _)`. Change nothing else.

**Step 4: Verify.**
```bash
nix develop -c cargo fmt --all --check
nix develop -c cargo clippy --workspace --all-targets -- -D warnings
nix develop -c cargo test --workspace
```
Expected: green, with the probes still `#[ignore]`d.

**Step 5: Commit.**
```bash
git add crates/honk-upstream/src crates/honk-upstream/tests
git commit -m "upstream: frames carry SSE event ids; explicit resume_after and takeover; expose Acp-Stream-Features"
```

---

### Task 8: Pre-tag gate — build the worktree, run every probe on the patched sandbox

Nothing is tagged until this task passes.

**Step 1: Build `goose` from the worktree.** Run (long, log `gp-build-goose`) in
`$FORK_WT`: `nix develop -c cargo build --release -p goose-cli --bin goose`. This uses
the same default features as the gnu release.

If the dev shell lacks a system library for a default feature (`code-mode`/`live-voice`),
fall back to the musl job's feature set:
`--no-default-features --features portable-default`. It has the same transport and
shell code. Record the fallback in the report.

Expected: `$FORK_WT/target/release/goose` exists, and `"$FORK_WT/target/release/goose"
--version` prints `1.54.0`. The binary links against Nix glibc and needs no patchelf.

**Step 2: Launch the patched sandbox unit.** This is the second unit; the shared one is
never touched.
```bash
PSB="$HOME/.local/state/honk-sandbox-patched"
systemctl --user stop honk-goose-sandbox-patched 2>/dev/null
systemctl --user reset-failed honk-goose-sandbox-patched 2>/dev/null
XDG_RUNTIME_DIR=/run/user/$(id -u) systemd-run --user --unit=honk-goose-sandbox-patched --collect \
  --setenv=PATH="$PATH" --setenv=HOME="$HOME" \
  --setenv=HONK_SANDBOX="$PSB" --setenv=HONK_SANDBOX_PORT=4181 \
  --setenv=GOOSE_BIN="$FORK_WT/target/release/goose" \
  "$HONK/scripts/goose-sandbox.sh"
HONK_SANDBOX="$PSB" HONK_SANDBOX_PORT=4181 HONK_SANDBOX_UNIT=honk-goose-sandbox-patched \
  HONK_EXPECT_GOOSE_BIN="$FORK_WT/target/release/goose" "$HONK/scripts/goose-sandbox-check.sh"
```
Expected: `sandbox OK`, and `goose exe:` is the worktree binary. The first launch
generates `$PSB/config/config.yaml` and `$PSB/secret`.

**Step 3: honk test-harness changes.** All of these are in `crates/honk-upstream/tests/`:

- **`common/proxy.rs` (new).** Move `TcpProxy` and `wait_for_freeze` here from
  `p2_disconnect.rs:12-116`. Make `TcpProxy` and its `port` field `pub`; change
  nothing else.
- **`common/mod.rs`:**
  - Add `pub mod proxy;`.
  - Add the constants `FIXTURES_PATCHED_2 = "goose-1.54.0-patched.2"` and
    `FIXTURES_PATCHED_3 = "goose-1.54.0-patched.3"`.
  - Add
    `pub fn goose_url() -> String { std::env::var("HONK_GOOSE_URL").unwrap_or_else(|_| "http://127.0.0.1:4180/acp".into()) }`.
  - Add
    `pub fn goose_port() -> u16 { reqwest::Url::parse(&goose_url()).ok().and_then(|u| u.port()).unwrap_or(4180) }`.
  - Add `fn fixtures_root() -> PathBuf`, which returns `HONK_FIXTURES_ROOT` if set,
    else the repo's `fixtures/`.
  - Rename the body of `setup_session_with_mode` to `pub async fn setup_session_at(url:
    &str, fixtures: &str, probe_name: &str, mode: &str)`. It writes
    `fixtures_root()/{fixtures}/{probe_name}/{ts}.jsonl`, uses `url`, and still returns
    `Ok(None)` without `HONK_GOOSE_SECRET`.
  - `setup_session_with_mode(probe, mode)` becomes
    `setup_session_at(&goose_url(), FIXTURES_PATCHED_2, probe, mode)`.
  - Move in, as `pub fn`: `is_run_end` and `is_agent_chunk` from `p5_control.rs`
    (`:32-43`, `:64-69`), and `is_permission_request`, `is_tool_call_chunk` and
    `allow_once_outcome` from `p4_pending.rs` (`:9-11`, `:30-35`, `:50-57`). Delete
    them, and `p4_pending.rs`'s duplicate `is_run_end` (`:37-48`), from those files.
  - Add:

```rust
pub fn chunk_text(frame: &Value) -> Option<&str> {
    if !is_agent_chunk(frame) {
        return None;
    }
    frame.pointer("/params/update/content/text").and_then(|v| v.as_str())
}

/// Fails loudly if the sandbox is not running the patched build.
pub fn require_patched(upstream: &Upstream) -> anyhow::Result<()> {
    let features = upstream
        .stream_features()
        .ok_or_else(|| anyhow::anyhow!("no GET /acp has been made yet"))?;
    anyhow::ensure!(
        features.is_patched(),
        "goose at {} does not advertise `Acp-Stream-Features: takeover, replay` (got {features:?}); \
         point HONK_GOOSE_URL at honk-goose-sandbox-patched (port 4181)",
        goose_url()
    );
    Ok(())
}

/// Polls export on a fresh connection until the turn's FINISHED reply is committed
/// (export reads sqlite and works on any connection, A6). Returns the wait.
pub async fn wait_until_turn_committed(ts: &TestSession, within: Duration) -> anyhow::Result<Duration> {
    let t0 = Instant::now();
    let mut probe = Upstream::initialize(&goose_url(), ts.secret.clone(), ts.rec.clone()).await?;
    probe.subscribe(None).await?;
    probe.subscribe(Some(&ts.sid)).await?;
    loop {
        let export = export_session(&mut probe, &ts.sid).await?;
        if extract_all_assistant_text(&export).contains("FINISHED") {
            break;
        }
        anyhow::ensure!(t0.elapsed() < within, "turn not committed within {within:?}");
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
    let _ = probe.delete().await;
    Ok(t0.elapsed())
}
```

- **`p2_disconnect.rs`.** Delete the moved code, add `use common::proxy::TcpProxy;`,
  and change `TcpProxy::start(4180)` (P2d) to `TcpProxy::start(goose_port())`. P2d's
  `direct_url` already reads `HONK_GOOSE_URL`.
- **`p4_pending.rs`, `p5_control.rs`.** Import the moved helpers from `common`.

Run `nix develop -c cargo test -p honk-upstream --tests`. Expected: it compiles, and the
probes are ignored.

**Step 4: Write `tests/p2_patched.rs` (P2e, P2f).**

```rust
mod common;
use common::proxy::TcpProxy;
use common::*;
use honk_upstream::{ReplayOutcome, SubscribeOpts};
use std::collections::HashMap;

const STREAMING_PROMPT: &str =
    "Write a 60-line markdown document about tides, including a 5-row table, then reply FINISHED.";

struct HalfOpen {
    ts: TestSession,
    proxy: TcpProxy,
    prompt_id: Value,
    pre_frames: Vec<Value>,
    /// Last PROCESSED event id per stream: what honkd would have persisted.
    last_ids: HashMap<StreamKey, u64>,
}

/// P2d's setup: all traffic through a proxy, streaming turn, freeze after 20 chunks.
async fn freeze_mid_stream(probe: &str) -> anyhow::Result<Option<HalfOpen>> {
    let proxy = TcpProxy::start(goose_port()).await?;
    let proxy_url = format!("http://127.0.0.1:{}/acp", proxy.port);
    let Some(mut ts) = setup_session_at(&proxy_url, FIXTURES_PATCHED_3, probe, "auto").await? else {
        return Ok(None);
    };
    require_patched(&ts.upstream)?;
    let prompt_id = ts
        .upstream
        .request(
            "session/prompt",
            json!({"sessionId": &ts.sid, "prompt": [{"type": "text", "text": STREAMING_PROMPT}]}),
            Some(&ts.sid),
        )
        .await?;
    let mut pre_frames = Vec::new();
    let mut last_ids = HashMap::new();
    let mut chunks = 0;
    while chunks < 20 {
        match ts.upstream.next(Duration::from_secs(90)).await? {
            Some(Event::Frame(key, frame, id)) => {
                if is_agent_chunk(&frame) {
                    chunks += 1;
                }
                if let Some(id) = id {
                    last_ids.insert(key, id);
                }
                pre_frames.push(frame);
            }
            other => anyhow::bail!("stream ended before 20 chunks: {other:?}"),
        }
    }
    proxy.freeze();
    ts.upstream.hard_drop();
    // Frames the reader task had already parsed were delivered; process them too.
    while let Some(Event::Frame(key, frame, id)) = ts.upstream.next(Duration::ZERO).await? {
        if let Some(id) = id {
            last_ids.insert(key, id);
        }
        pre_frames.push(frame);
    }
    Ok(Some(HalfOpen { ts, proxy, prompt_id, pre_frames, last_ids }))
}

struct Tail {
    frames: Vec<Value>,
    prompt_reply: bool,
    run_end: bool,
}

async fn read_to_turn_end(up: &mut Upstream, prompt_id: &Value, within: Duration) -> anyhow::Result<Tail> {
    let deadline = Instant::now() + within;
    let mut tail = Tail { frames: Vec::new(), prompt_reply: false, run_end: false };
    while !(tail.prompt_reply && tail.run_end) && Instant::now() < deadline {
        let Some(Event::Frame(_, frame, _)) = up.next(deadline - Instant::now()).await? else { break };
        if frame.get("id").is_some_and(|id| rpc_id_matches(prompt_id, id)) && frame.get("result").is_some() {
            tail.prompt_reply = true;
        }
        if is_run_end(&frame) {
            tail.run_end = true;
        }
        tail.frames.push(frame);
    }
    Ok(tail)
}

fn streamed_text(frames: &[Value]) -> String {
    frames.iter().filter_map(chunk_text).collect()
}

/// P2e: half-open subscriber + takeover. Question: is the 409 window gone?
#[tokio::test]
#[ignore]
async fn p2e_half_open_takeover() -> anyhow::Result<()> {
    let Some(h) = freeze_mid_stream("p2e").await? else { return Ok(()) };
    let mut resumed = Upstream::resume(&goose_url(), h.ts.secret.clone(), h.ts.conn_id.clone(), h.ts.rec.clone())?;
    resumed.set_subscribe_deadline(Duration::from_secs(5));
    let opts = SubscribeOpts { takeover: true, resume_after: None };
    let t0 = Instant::now();
    let conn = resumed.subscribe_with(None, opts).await?;
    let sess = resumed.subscribe_with(Some(&h.ts.sid), opts).await?;
    let reattach = t0.elapsed();
    let tail = read_to_turn_end(&mut resumed, &h.prompt_id, Duration::from_secs(180)).await?;
    let export = export_session(&mut resumed, &h.ts.sid).await?;
    let streamed = format!("{}{}", streamed_text(&h.pre_frames), streamed_text(&tail.frames));
    let exported = extract_all_assistant_text(&export);
    let missing = exported.chars().count().saturating_sub(streamed.chars().count());
    h.ts.rec.record_marker("probe", json!({
        "probe": "p2e", "reattach_ms": reattach.as_millis(),
        "conn_409_ms": conn.conflict_window.as_millis(), "session_409_ms": sess.conflict_window.as_millis(),
        "took_over": [conn.took_over, sess.took_over],
        "prompt_reply": tail.prompt_reply, "run_end": tail.run_end,
        "streamed_chars": streamed.chars().count(), "exported_chars": exported.chars().count(),
        "chars_missing": missing, "streamed_is_subsequence": is_subsequence(&streamed, &exported),
    }).to_string());
    assert_eq!(conn.conflict_window, Duration::ZERO);
    assert_eq!(sess.conflict_window, Duration::ZERO);
    assert!(conn.took_over && sess.took_over);
    assert!(reattach < Duration::from_millis(100), "takeover reattach took {reattach:?}");
    // Loss is expected and recorded, not asserted: takeover fixes availability, not delivery.
    drop(h.proxy);
    Ok(())
}

/// P2f: half-open + wait for the turn to finish behind the frozen socket, then
/// takeover + Last-Event-ID. Every post-reattach frame is then a replay of something
/// goose already wrote into the dead socket (P2d: those were all lost).
#[tokio::test]
#[ignore]
async fn p2f_half_open_takeover_with_replay() -> anyhow::Result<()> {
    let Some(h) = freeze_mid_stream("p2f").await? else { return Ok(()) };
    let waited = wait_until_turn_committed(&h.ts, Duration::from_secs(150)).await?;
    let mut resumed = Upstream::resume(&goose_url(), h.ts.secret.clone(), h.ts.conn_id.clone(), h.ts.rec.clone())?;
    let conn_last = h.last_ids.get(&StreamKey::Conn).copied();
    let sess_last = h.last_ids.get(&StreamKey::Session(h.ts.sid.clone())).copied();
    anyhow::ensure!(sess_last.is_some(), "no event id was processed on the session stream before the freeze");
    let conn = resumed
        .subscribe_with(None, SubscribeOpts { takeover: true, resume_after: Some(conn_last.unwrap_or(0)) })
        .await?;
    let sess = resumed
        .subscribe_with(Some(&h.ts.sid), SubscribeOpts { takeover: true, resume_after: sess_last })
        .await?;
    let tail = read_to_turn_end(&mut resumed, &h.prompt_id, Duration::from_secs(30)).await?;
    let export = export_session(&mut resumed, &h.ts.sid).await?;
    let streamed = format!("{}{}", streamed_text(&h.pre_frames), streamed_text(&tail.frames));
    let exported = extract_all_assistant_text(&export);
    let missing = exported.chars().count().saturating_sub(streamed.chars().count());
    h.ts.rec.record_marker("probe", json!({
        "probe": "p2f", "waited_for_commit_ms": waited.as_millis(),
        "last_ids": {"conn": conn_last, "session": sess_last},
        "conn_replay": format!("{:?}", conn.replay), "session_replay": format!("{:?}", sess.replay),
        "replayed_frames": tail.frames.len(),
        "prompt_reply": tail.prompt_reply, "run_end": tail.run_end,
        "streamed_chars": streamed.chars().count(), "exported_chars": exported.chars().count(),
        "chars_missing": missing,
    }).to_string());
    assert_eq!(conn.replay, ReplayOutcome::Complete);
    assert_eq!(sess.replay, ReplayOutcome::Complete);
    assert!(!tail.frames.is_empty(), "replay delivered nothing: the probe did not exercise the ring");
    assert!(tail.prompt_reply, "the session/prompt reply must be replayed");
    assert!(tail.run_end, "the run-end session_info_update must be replayed");
    assert_eq!(missing, 0, "streamed text must cover the exported assistant text");
    assert!(is_subsequence(&streamed, &exported));
    drop(h.proxy);
    Ok(())
}
```

Why P2f's post-reattach frames are replays:
- The turn has already finished, as confirmed by export, so goose generates nothing
  new after the reattach.
- About 10 KB of output is far below loopback socket buffers. hyper has therefore
  written, and so dequeued, every remaining frame into the frozen socket.
- Without the ring, P2d showed those frames are all lost: zero chunks after reattach.

**Step 5: Write `tests/p4_patched.rs` (P4b′).** It covers the lost-permission frame
that design amendment A8 relies on replay for.

```rust
mod common;
use common::*;
use honk_upstream::{ReplayOutcome, SubscribeOpts};
use std::collections::HashMap;

/// P4b′: drop right after the tool_call, before processing the permission request;
/// resume with Last-Event-ID; the permission must arrive and answering it must finish.
#[tokio::test]
#[ignore]
async fn p4b_prime_lost_permission_is_replayed() -> anyhow::Result<()> {
    let Some(mut ts) = setup_session_at(&goose_url(), FIXTURES_PATCHED_3, "p4b_prime", "approve").await? else {
        return Ok(());
    };
    require_patched(&ts.upstream)?;
    let prompt = "Run the shell command \"echo P4OK\", then reply with only the word FINISHED.";
    let prompt_id = ts.upstream.request(
        "session/prompt",
        json!({"sessionId": &ts.sid, "prompt": [{"type": "text", "text": prompt}]}),
        Some(&ts.sid),
    ).await?;

    // Process frames up to and including the tool_call; never look past it.
    let mut last_ids: HashMap<StreamKey, u64> = HashMap::new();
    let start = Instant::now();
    loop {
        anyhow::ensure!(start.elapsed() < Duration::from_secs(60), "no tool_call");
        let Some(Event::Frame(key, frame, id)) = ts.upstream.next(Duration::from_secs(60)).await? else {
            anyhow::bail!("stream ended before tool_call");
        };
        anyhow::ensure!(!is_permission_request(&frame), "permission arrived before tool_call; re-run");
        if let Some(id) = id {
            last_ids.insert(key, id);
        }
        if is_tool_call_chunk(&frame) {
            break;
        }
    }
    // Simulate a client crash: whatever the reader parsed after the tool_call is
    // discarded unprocessed, along with the old Upstream.
    ts.upstream.hard_drop();
    tokio::time::sleep(Duration::from_secs(2)).await;

    let mut resumed = Upstream::resume(&goose_url(), ts.secret.clone(), ts.conn_id.clone(), ts.rec.clone())?;
    let sess_last = last_ids.get(&StreamKey::Session(ts.sid.clone())).copied();
    anyhow::ensure!(sess_last.is_some(), "no session-stream id processed before the drop");
    resumed
        .subscribe_with(None, SubscribeOpts { takeover: true, resume_after: Some(last_ids.get(&StreamKey::Conn).copied().unwrap_or(0)) })
        .await?;
    let sess = resumed
        .subscribe_with(Some(&ts.sid), SubscribeOpts { takeover: true, resume_after: sess_last })
        .await?;

    let mut permission = None;
    let t_wait = Instant::now();
    while permission.is_none() && t_wait.elapsed() < Duration::from_secs(10) {
        if let Some(Event::Frame(_, frame, _)) = resumed.next(Duration::from_millis(500)).await?
            && is_permission_request(&frame)
        {
            permission = Some(frame);
        }
    }
    let permission = permission.ok_or_else(|| anyhow::anyhow!("permission request was not replayed"))?;
    let perm_id = permission.get("id").cloned().expect("request has an id");
    resumed.respond(perm_id.clone(), allow_once_outcome(), Some(&ts.sid)).await?;

    let mut prompt_reply = false;
    let t_end = Instant::now();
    while !prompt_reply && t_end.elapsed() < Duration::from_secs(60) {
        if let Some(Event::Frame(_, frame, _)) = resumed.next(Duration::from_secs(5)).await?
            && frame.get("id").is_some_and(|id| rpc_id_matches(&prompt_id, id))
            && frame.get("result").is_some()
        {
            prompt_reply = true;
        }
    }
    let export = export_session(&mut resumed, &ts.sid).await?;
    let tool_ran = export.to_string().contains("P4OK");
    let finished = extract_all_assistant_text(&export).contains("FINISHED");
    ts.rec.record_marker("probe", json!({
        "probe": "p4b_prime", "session_resume_after": sess_last,
        "session_replay": format!("{:?}", sess.replay), "permission_replayed": true,
        "prompt_reply": prompt_reply, "tool_ran": tool_ran, "finished": finished,
    }).to_string());
    assert_eq!(sess.replay, ReplayOutcome::Complete);
    assert!(prompt_reply && tool_ran && finished, "answering the replayed permission must finish the run");
    Ok(())
}
```

**Step 6: Write `tests/p5_patched.rs` (P5a′).** It has the same shape as P5a
(`p5_control.rs:211-400`):

```rust
mod common;
use common::*;

fn pgrep_user(pattern: &str) -> anyhow::Result<Vec<u32>> {
    let uid = String::from_utf8(std::process::Command::new("id").arg("-u").output()?.stdout)?;
    let out = std::process::Command::new("pgrep").args(["-u", uid.trim(), "-f", pattern]).output()?;
    Ok(String::from_utf8_lossy(&out.stdout).lines().filter_map(|l| l.trim().parse().ok()).collect())
}

/// P5a′: session/cancel must leave no shell descendant alive (patch 1).
#[tokio::test]
#[ignore]
async fn p5a_prime_cancel_leaves_no_orphans() -> anyhow::Result<()> {
    let Some(mut ts) = setup_session_at(&goose_url(), FIXTURES_PATCHED_3, "p5a_prime", "auto").await? else {
        return Ok(());
    };
    require_patched(&ts.upstream)?;
    let prompt = "Run the shell command \"sleep 61 & sleep 62; echo P5A_PRIME_DONE\", then reply with only the word FINISHED.";
    let prompt_id = ts.upstream.request(
        "session/prompt",
        json!({"sessionId": &ts.sid, "prompt": [{"type": "text", "text": prompt}]}),
        Some(&ts.sid),
    ).await?;

    let start = Instant::now();
    let (a, b) = loop {
        anyhow::ensure!(start.elapsed() < Duration::from_secs(60), "sleeps never started");
        let _ = ts.upstream.next(Duration::from_millis(200)).await?;
        let (a, b) = (pgrep_user("sleep 61")?, pgrep_user("sleep 62")?);
        if !a.is_empty() && !b.is_empty() {
            break (a, b);
        }
    };

    let t_cancel = Instant::now();
    ts.upstream.notify("session/cancel", json!({"sessionId": &ts.sid}), Some(&ts.sid)).await?;
    let mut stop_reason = None;
    while t_cancel.elapsed() < Duration::from_secs(10) {
        if let Some(Event::Frame(_, f, _)) = ts.upstream.next(Duration::from_millis(200)).await?
            && f.get("id").is_some_and(|id| rpc_id_matches(&prompt_id, id))
        {
            stop_reason = f.pointer("/result/stopReason").and_then(|v| v.as_str()).map(str::to_owned);
            break;
        }
    }
    let cancel_ms = t_cancel.elapsed().as_millis();

    let mut survivors = Vec::new();
    for _ in 0..15 {
        survivors = [pgrep_user("sleep 61")?, pgrep_user("sleep 62")?].concat();
        if survivors.is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    ts.rec.record_marker("probe", json!({
        "probe": "p5a_prime", "initial_pids": [a, b], "cancel_ms": cancel_ms,
        "stop_reason": stop_reason, "survivors_after_3s": survivors,
    }).to_string());
    for pid in &survivors {
        let _ = std::process::Command::new("kill").args(["-9", &pid.to_string()]).status();
    }
    assert_eq!(stop_reason.as_deref(), Some("cancelled"));
    assert!(survivors.is_empty(), "orphaned shell descendants survived cancel: {survivors:?}");
    Ok(())
}
```

Expected cancel latency: similar to P5a's 54-79 ms. `sleep` and `bash` die on SIGTERM,
so the group empties at once and no 2 s grace is spent. Record `cancel_ms`. A value
near 2000 ms means something ignored TERM.

Run `nix develop -c cargo test -p honk-upstream --tests` (it compiles; probes are
ignored), then clippy and fmt as in Task 7.

**Step 7: Run the four probes against the worktree build.** Pre-release fixtures go to
`/tmp`. Each probe gets its own unit:
```bash
export XDG_RUNTIME_DIR=/run/user/$(id -u)
for t in p2_patched p4_patched p5_patched; do
systemd-run --user --unit="honk-probe-$t-$RANDOM" --collect --working-directory="$HONK" \
  --setenv=PATH="$PATH" --setenv=HOME="$HOME" --expand-environment=no \
  bash -c "HONK_GOOSE_URL=http://127.0.0.1:4181/acp HONK_FIXTURES_ROOT=/tmp/opencode/honk-prerelease-fixtures HONK_GOOSE_SECRET=\$(cat \"\$HOME/.local/state/honk-sandbox-patched/secret\") nix develop -c cargo test -p honk-upstream --test $t -- --ignored --nocapture --test-threads=1 > /tmp/opencode/honk-prerelease-$t.log 2>&1; echo exit=\$? >> /tmp/opencode/honk-prerelease-$t.log"
done
```
The `$t` is expanded by your shell when the string is built. The escaped `\$`
references are left for the unit's bash, because of `--expand-environment=no`. Poll the
three logs. Each probe runs twice in total (the M0 convention), so repeat the loop once
after the first round passes.

Expected:
- **P2e:** 409 window 0 on both streams, reattach < 100 ms, `took_over` true. Loss is
  recorded, not asserted.
- **P2f:** `Complete` on both streams, `replayed_frames > 0`, the prompt reply and
  run-end replayed, `chars_missing == 0`.
- **P4b′:** the permission is replayed, and answering it finishes the run (`P4OK` in
  export).
- **P5a′:** `stopReason: cancelled`, no survivors.

If P2f shows `Gap`, or `replayed_frames == 0`, record the numbers before changing
anything. The ring may be too small, or the freeze may have landed after goose had
finished writing.

**Step 8: Qualify pigeon's WS path on the same build.** These probes use the isolated
unit only. The launch probe uses an in-memory DB; nothing touches pigeon's state.
```bash
export XDG_RUNTIME_DIR=/run/user/$(id -u)
for p in goose-launch-probe goose-runner-probe goose-id-split-probe; do
systemd-run --user --unit="pigeon-qual-$p-$RANDOM" --collect --working-directory="$PIGEON/packages/daemon" \
  --setenv=PATH="$PATH" --setenv=HOME="$HOME" --expand-environment=no \
  bash -c "GOOSE_ACP_URL=ws://127.0.0.1:4181/acp PIGEON_GOOSE_PROBE_TOKEN=\$(cat \"\$HOME/.local/state/honk-sandbox-patched/secret\") GOOSE_ACP_CWD=\$(mktemp -d) npx tsx scripts/$p.ts > /tmp/opencode/pigeon-qual-prerelease-$p.log 2>&1; echo exit=\$? >> /tmp/opencode/pigeon-qual-prerelease-$p.log"
done
```
Expected: each log ends in `exit=0` with no `FAIL` lines.

If one fails, run the same probe against the **shared unpatched** sandbox
(`ws://127.0.0.1:4180/acp`, secret `$HOME/.local/state/honk-sandbox/secret`). That
tells a regression apart from a pre-existing probe or environment issue. Only a
failure that appears on 4181 and not on 4180 blocks.

**Step 9: Commit the probe code only.** The pre-release fixtures in `/tmp` are not
committed.
```bash
git add crates/honk-upstream/tests/common/mod.rs crates/honk-upstream/tests/common/proxy.rs \
  crates/honk-upstream/tests/p2_disconnect.rs crates/honk-upstream/tests/p4_pending.rs \
  crates/honk-upstream/tests/p5_control.rs crates/honk-upstream/tests/p2_patched.rs \
  crates/honk-upstream/tests/p4_patched.rs crates/honk-upstream/tests/p5_patched.rs
git commit -m "probe(P2e,P2f,P4b',P5a'): takeover, replay, lost-permission replay and process-group kill on patched goose"
```

**Gate:** all four probes passed twice and all three pigeon probes passed. Only then
continue to Task 9.

---

### Task 9: Release `patched-1.54.0.3`

**Step 1: Pre-push checks.**
```bash
git -C "$FORK_WT" status --porcelain                                   # expect: empty
git -C "$FORK_WT" fetch fork
git -C "$FORK_WT" merge-base --is-ancestor fork/patched/v1.54.0 HEAD && echo fast-forward
git -C "$FORK_WT" log --oneline fork/patched/v1.54.0..HEAD
git -C "$FORK_WT" diff --stat fork/patched/v1.54.0..HEAD
```
Expected:
- The log shows exactly 5 commits: `developer/shell: …`, `vendor: …`, `acp-http:
  Acp-Takeover …`, `acp-http: SSE event ids …` and `ci: …`.
- The diff stat shows only `shell.rs`, `Cargo.toml`, `Cargo.lock`,
  `vendor/agent-client-protocol-http/**` and `patched-release.yml`.
- The `HEAD` being tagged is the commit Task 8 built. If anything was committed after
  that build, rebuild and re-run Task 8.

**Step 2: Push the branch** (explicit refspec, fast-forward only, never `origin`):
`git -C "$FORK_WT" push fork HEAD:refs/heads/patched/v1.54.0`.

**Step 3: Tag and push the tag.**
```bash
git -C "$FORK_WT" tag -a patched-1.54.0.3 -m "goose v1.54.0 + local patches (headless cache TTL, Vertex host override, GCP auth scopes, shell process-group termination, ACP HTTP SSE takeover and replay)"
git -C "$FORK_WT" push fork refs/tags/patched-1.54.0.3
```

**Step 4: Watch the run** (about 30 min).
```bash
RUN=$(gh run list --repo johnnymo87/goose --workflow patched-release.yml --branch patched-1.54.0.3 -L1 --json databaseId -q '.[0].databaseId')
gh run view "$RUN" --repo johnnymo87/goose --json status,conclusion,jobs -q '.status, .conclusion, (.jobs[] | .name + ": " + .status + " " + .conclusion)'
```
- Poll every few minutes, with each sleep as its own tool call. Alternatively, run
  `gh run watch "$RUN" --repo johnnymo87/goose --exit-status` with the long-command
  pattern (log `gp-release-watch`).
- If the turn will end first, schedule a self-wake with `swarm_schedule` (after 35m).
  The message must stand alone: run `$RUN`, tag `patched-1.54.0.3`, this plan's path.
- Expected: `build`, `test-patches` and `release` all succeed.
- If `test-patches` fails: read `gh run view "$RUN" --repo johnnymo87/goose
  --log-failed`, fix, push, re-run Task 8, and cut **`.4`**. Never re-tag.

**Step 5: Verify the assets.**
```bash
gh release view patched-1.54.0.3 --repo johnnymo87/goose --json assets,targetCommitish -q '.targetCommitish, .assets[].name'
```
Expected:
- The target is the pushed `HEAD` SHA.
- The assets are `goose-aarch64-unknown-linux-gnu.tar.{gz,bz2}`,
  `goose-x86_64-unknown-linux-gnu.tar.{gz,bz2}` and `SHA256SUMS`.

**Step 6: Fetch and package the release binary** (autoPatchelf; this mirrors the
workstation repo's `pkgs/goose`):
```bash
BIN_DIR="$HOME/.local/state/honk-goose-bin/patched-1.54.0.3"
mkdir -p "$BIN_DIR"
gh release download patched-1.54.0.3 --repo johnnymo87/goose --dir "$BIN_DIR" \
  --pattern 'goose-aarch64-unknown-linux-gnu.tar.gz' --pattern SHA256SUMS
(cd "$BIN_DIR" && sha256sum -c --ignore-missing SHA256SUMS)          # expect: OK
TARBALL="$BIN_DIR/goose-aarch64-unknown-linux-gnu.tar.gz" nix build --impure --out-link "$BIN_DIR/pkg" --expr '
  let pkgs = (builtins.getFlake "nixpkgs").legacyPackages.${builtins.currentSystem}; in
  pkgs.stdenv.mkDerivation {
    pname = "goose"; version = "1.54.0-patched.3";
    src = /. + builtins.getEnv "TARBALL";
    nativeBuildInputs = [ pkgs.autoPatchelfHook ];
    buildInputs = [ pkgs.stdenv.cc.cc.lib ];
    dontConfigure = true; dontBuild = true; dontStrip = true;
    unpackPhase = "runHook preUnpack; tar -xzf $src; runHook postUnpack";
    installPhase = "runHook preInstall; mkdir -p $out/bin; install -m755 goose $out/bin/goose; runHook postInstall";
    doInstallCheck = true;
    installCheckPhase = "$out/bin/goose --version";
  }'
ls -l "$BIN_DIR/pkg/bin/goose"
```
The out-link is a GC root.

---

### Task 10: Re-run on the release binary; commit fixtures

**Step 1: Switch the patched unit to the release binary.** This relaunches only
`honk-goose-sandbox-patched`.
```bash
PSB="$HOME/.local/state/honk-sandbox-patched"
REL="$HOME/.local/state/honk-goose-bin/patched-1.54.0.3/pkg/bin/goose"
systemctl --user stop honk-goose-sandbox-patched 2>/dev/null
systemctl --user reset-failed honk-goose-sandbox-patched 2>/dev/null
XDG_RUNTIME_DIR=/run/user/$(id -u) systemd-run --user --unit=honk-goose-sandbox-patched --collect \
  --setenv=PATH="$PATH" --setenv=HOME="$HOME" \
  --setenv=HONK_SANDBOX="$PSB" --setenv=HONK_SANDBOX_PORT=4181 --setenv=GOOSE_BIN="$REL" \
  "$HONK/scripts/goose-sandbox.sh"
HONK_SANDBOX="$PSB" HONK_SANDBOX_PORT=4181 HONK_SANDBOX_UNIT=honk-goose-sandbox-patched \
  HONK_EXPECT_GOOSE_BIN="$REL" "$HONK/scripts/goose-sandbox-check.sh"
```
Expected: `goose exe:` shows the `…-goose-1.54.0-patched.3/bin/goose` store path.

**Step 2: Run each probe once** with Task 8 Step 7's loop. This time **omit
`HONK_FIXTURES_ROOT`**, so fixtures land in the repo under
`fixtures/goose-1.54.0-patched.3/`. Expected: the same results as Task 8.

**Step 3: Run the pigeon qualification once** with Task 8 Step 8's loop, renaming the
logs to `pigeon-qual-release-*`. **This is the evidence the production deploy is gated
on.** Keep the three logs, and copy their `PASS`/`FAIL` summary lines into the report.

**Step 4: Commit the fixtures.** Read every new fixture diff by eye for secrets, home
paths, the GCP project id and org names.
```bash
git add fixtures/goose-1.54.0-patched.3
git commit -m "fixtures: P2e, P2f, P4b', P5a' on the patched-1.54.0.3 release binary"
```
Push, and confirm CI is green (`gh run watch <id> --exit-status`, using the
long-command pattern if needed).

Leave `honk-goose-sandbox-patched` running if M1 work starts next, since M1 targets
patched goose. Otherwise stop it (`systemctl --user stop honk-goose-sandbox-patched`);
it is this plan's own unit. Never touch `honk-goose-sandbox`.

---

### Task 11: honk — record results and the capability rule

**Files:**
- Modify: `docs/spike/goose-patches.md`.
  - Status line (`:3`): "Built: patches 2, 3 and 5 ship in fork release
    `patched-1.54.0.3`; see Results".
  - Append `## Results: patched-1.54.0.3`, with one row per probe (P2e, P2f, P4b′,
    P5a′, and the pigeon qualification): question, verdict, numbers, fixture path.
  - Note:
    - the owner-token deviation from §2, and its reason;
    - that the WS mailbox is unchanged;
    - TERM→KILL;
    - ring memory against A14.
- Modify: `docs/plans/2026-10-10-honk-design.md` §6 (reattach rules). Add the
  capability table and the persistence rule from the section below, verbatim.

**Commit:** `docs: goose patch results and stream-feature gating`.

---

## Capability and feature detection (how honkd treats patched and unpatched goose)

**The signal.** Patched goose sends `Acp-Stream-Features: takeover, replay` on every
SSE `GET /acp` response, `200` and `409`. Unpatched goose sends nothing.
- The signal comes from the crate, which is where the behaviour lives.
- It costs no extra request.
- It does not depend on version strings. `agentInfo.version` is `1.54.0` in both
  builds; log it for diagnostics only.
- `honk-upstream` exposes it as `Upstream::stream_features()` and
  `StreamFeatures::is_patched()`.

**M1 policy: honkd requires patched goose.**
- After its first subscribe on each goose process, honkd checks
  `stream_features().is_patched()`. If that is false, it **refuses to serve**, with an
  error naming the release to deploy.
- The escape hatch is **`--allow-unpatched-upstream`**. It makes honkd run in M0
  degraded mode instead: 409 backoff, reconcile after every reattach (A2/A3), and
  lost-permission detection with cancel (A8 fallback).
- honkd re-checks after every `404`, i.e. after a goose restart (A13). A restart may
  have swapped the binary in either direction.

**Sending the headers is always safe. Trusting the result is not.**
- Unpatched goose ignores `Acp-Takeover` (it answers 409) and `Last-Event-ID` (no
  replay).
- Gate behaviour **only** on positive signals: `took_over`, and
  `ReplayOutcome::Complete`. Never treat the absence of a gap signal as "no loss".

| goose advertises | On a dead stream (no bytes for > 20 s; goose keeps alive every 15 s) | After reattach |
|---|---|---|
| nothing (unpatched): only with `--allow-unpatched-upstream` | 409 retry with backoff, never DELETE (A2) | Always reconcile (A3, A5) |
| `takeover` only (not a released combination; treat as degraded) | `subscribe_with{takeover}` | Always reconcile |
| `takeover, replay` (`patched-1.54.0.3`+) | `subscribe_with{takeover, resume_after: Some(last processed id, or 0)}` | Reconcile **only** if `replay.needs_reconcile()` (`Gap`/`Unsupported`), or after a 404 |

**Where honkd persists the last processed event id.** Keep it in **the session's own
append-only log**, written in the same record as the effect of the frame it processed.
Do not keep it in the global state file.
- The id and the state it describes then commit together, and can never disagree after
  a crash.
- The state file is not rewritten on every frame.
- On restart, the last id in the session log is exactly what to pass as
  `resume_after`.
- The connection stream is low-rate (`session/new` replies and connection-level
  notifications). Its id can live next to the connection id in the state file (A15).

**Still required with replay:**
- Persisted pending permissions (A8). Replay only helps while the connection lives; a
  goose restart loses everything in memory.
- Takeover is for honkd's own dead streams only. honkd owns its connection ids, and two
  honkd instances must never share one: the last subscriber silently wins.

---

## Production deploy (author only — the agent must not run any step here)

**Prerequisites:**
- Task 10 passed: all four honk probes, and all three pigeon qualification probes on the
  **release binary** (logs `pigeon-qual-release-*`).
- If any pigeon probe failed there, do not deploy.

**What actually changes in production.** Pigeon talks to `goose-serve` over WS.
- **Patch 1 changes pigeon's turns.** A cancelled or timed-out shell command now gets
  SIGTERM to its whole process group, then SIGKILL after 2 s. Before, only `bash` was
  SIGKILLed and descendants were orphaned.
- **Patches 2 and 3 change only the HTTP SSE path.** The WS mailbox code is unchanged
  (Task 4 Step 6). The WS path is still compiled from the vendored crate, which is why
  it is qualified rather than assumed identical.
- **The interactive `goose` CLI on this host** (same binary) takes the TTY cost
  described in Task 2.

**Steps:**

1. **Bump the pin.** In the workstation repo, edit `pkgs/goose/default.nix`:
   - Set `version = "1.54.0-patched.3";` and `tag = "patched-1.54.0.3";`.
   - Replace both `hash` values. For each asset, run `nix store prefetch-file --json
     https://github.com/johnnymo87/goose/releases/download/patched-1.54.0.3/<asset> |
     jq -r .hash`.
   - Extend the "THE PATCHES" comment. Do **not** call these inert:
     - 4. developer shell: SIGTERM, 2 s grace, then SIGKILL to the command's whole
       process group on cancel, timeout or drop. This changes pigeon's cancels.
     - 5. Vendored `agent-client-protocol-http`, HTTP SSE only: `Acp-Takeover`.
     - 6. Same crate, HTTP SSE only: SSE `id:`, a per-stream replay ring and
       `Last-Event-ID`.
     - The WS mailbox is unchanged. 5 and 6 are qualified against pigeon's goose
       probes.
   - Update "WHY A PINNED RELEASE" to say 1.54.0-patched.3 was re-measured with
     pigeon's three probes on the date of Task 10.
2. **Rebuild** with the cloudbox command from the workstation repo's `AGENTS.md` host
   table (NixOS rebuild plus home-manager switch). `goose-serve` sets
   `X-RestartIfChanged=false`, so the rebuild does **not** restart it.
3. **Pick a quiet window.** No pigeon goose run may be in flight.
4. **Back up the DB.** The `.backup` command is consistent even while goose is running:
   ```bash
   sqlite3 ~/.local/share/goose/sessions/sessions.db \
     ".backup '$HOME/.local/share/goose/sessions/sessions.db.pre-patched-1.54.0.3'"
   ```
5. **Restart:** `sudo systemctl restart goose-serve`. Check `systemctl show goose-serve
   -p ExecStart`: it must reference the `goose-1.54.0-patched.3` store path. Restart
   `pigeon-daemon` only if the configuration comment says the token changed.
6. **Re-run pigeon's three goose probes against production,** as pigeon's docs
   describe. All must pass.
7. **Rollback:** revert the pin commit, rebuild, restart `goose-serve`. The schema is
   unchanged (both builds are 1.54.0), so the DB backup is insurance, not part of the
   rollback.

---

## Upstreaming later (author decision: only after living with the patches)

| Patch | Upstream | Shape | Notes |
|---|---|---|---|
| 1. shell process-group termination | goose (`block/goose`), `crates/goose/src/agents/platform_extensions/developer/shell.rs` | One PR with the 6 tests | Precedent: `subprocess.rs` already uses `process_group(0)`. Explain TERM→KILL with the git `index.lock` case, the drop guard (the T5 abort path), the interactive-CLI TTY caveat, and the unbiased-`select!` window. |
| 2. takeover | `agent-client-protocol-http` (`agentclientprotocol/rust-sdk`, `src/agent-client-protocol-http`) | PR 1 | Generic transport. The header names need the ACP maintainers' blessing. Upstream could fold `HttpMailbox` back into `OutboundMailbox` for both transports; the fork keeps them split only to leave WS unchanged. |
| 3. replay ring + `Last-Event-ID` | same crate | PR 2, depends on PR 1 | `Last-Event-ID` is standard SSE, so browser `EventSource` clients benefit for free. Propose the ring caps as `ServerOptions` fields; the fork hard-codes them because goose builds `ServerOptions` with a struct literal. Mention that session mailboxes are never removed, a memory note that predates the ring. |

When the crate releases equivalents and goose depends on that release:
- Delete `vendor/agent-client-protocol-http`, its `members` entry and its `[patch]`
  line, and drop the CI graph check.
- honk is unaffected, because it keys on `Acp-Stream-Features`. If upstream renames the
  headers, update `StreamFeatures` and the probe expectations together.

**Rebase checklist** (moving the fork to goose > 1.54.0):
- `grep agent-client-protocol-http Cargo.toml`. If the requirement left `2.x`, the patch
  no longer applies.
- Re-vendor the new upstream version, then replay the two `acp-http` commits.
- The CI graph check fails loudly if this is forgotten.
