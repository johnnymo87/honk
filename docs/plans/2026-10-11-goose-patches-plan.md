# goose patch track Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Ship fork release `patched-1.54.0.3` of goose with three patches: a shell
process-group kill, SSE subscriber takeover, and an SSE replay ring with
`Last-Event-ID`. Then prove each patch with a live honk probe against the sandbox,
and teach `honk-upstream` to use them only when goose advertises them.

**Architecture:** Patch 1 is goose code (`developer/shell.rs`). Patches 2 and 3 live in
the external crate `agent-client-protocol-http` 2.2.0. The fork vendors that crate
under `vendor/` and swaps it in with `[patch.crates-io]`. The patched crate advertises
its abilities in a response header on `GET /acp` (`Acp-Stream-Features`).
`honk-upstream` reads that header, so one honkd works against both unpatched and
patched goose. The release is built by the fork's existing `patched-release.yml`. The
sandbox runs the release binary through a new `GOOSE_BIN` override. Production deploy
is a separate runbook that only the author runs.

**Tech Stack:**
- Rust 1.99.0. The fork's `rust-toolchain.toml` and honk's pin are the same.
- tokio, axum 0.8 SSE, libc `killpg`.
- reqwest and `eventsource-stream` in honk.
- Nix dev shells: `nix develop` works in both repos.
- GitHub Actions (`patched-release.yml`) and the `gh` CLI.

**Read first:**
- `docs/spike/goose-patches.md`: candidates, ranking and code sites.
- `docs/spike/m0-findings.md`: Summary; amendments A2, A3, A8, A11 and A16; P2d, P4b and P5a.
- `docs/plans/2026-10-10-honk-m0-plan.md`: Ground rules and "Goose patch track".

---

## Ground rules

**Fork safety (non-negotiable)**
- Tags are `patched-1.54.0.<n>` only. **Never create or push a `v*` tag.** Upstream's
  `release.yml` and `publish-docker.yml` fire on `v*`.
- **Never move, delete or re-push a tag.** If a release run fails, fix it and cut
  `.4`.
- **Never push to `origin`.** In the fork checkout, `origin` is upstream
  `block/goose`. The only push target is the remote `fork` (`johnnymo87/goose`). Every
  push names an explicit refspec, and nothing is ever force-pushed.
- **Work in a dedicated worktree, not the existing fork checkout.**
  - The existing checkout sits on a local branch named `headless-cache-ttl`. That
    branch is at the same commit as `fork/patched/v1.54.0` (`66279407d`), but there is
    no local branch named `patched/v1.54.0`.
  - Other sessions may use that checkout. Never run `git switch`, `checkout`,
    `stash`, `reset` or `clean` there.

**Sandbox only**
- Production goose (`goose-serve` on 127.0.0.1:4080) is off-limits to the agent.
- Every live probe talks to the sandbox (`honk-goose-sandbox`, port 4180).
  `honk-upstream::guard_url` already refuses 4080.
- The "Production deploy" section is **author only**.

**Long commands**
- The bash tool times out after 120 s.
- The first cargo build in a fresh worktree takes many minutes. Release runs take
  about 30 min. Probe runs take 1-4 min.
- Run long commands as a user unit that writes a log, then poll the log with short
  `tail` calls:

```bash
export XDG_RUNTIME_DIR=/run/user/$(id -u)
systemd-run --user --unit="gp-$RANDOM" --collect --working-directory="$FORK_WT" \
  --setenv=PATH="$PATH" --setenv=HOME="$HOME" --setenv=CARGO_BUILD_JOBS=8 \
  --expand-environment=no \
  bash -c 'nix develop -c <CARGO COMMAND> > /tmp/opencode/<LOG>.log 2>&1; echo "exit=$?" >> /tmp/opencode/<LOG>.log'
# then, as separate calls:  tail -n 40 /tmp/opencode/<LOG>.log
```

  "Run (long, log `X`)" below means: use this pattern with log name `X`.
- `--expand-environment=no` matters. Without it, systemd rewrites `$` in the payload.

**Variables used below**
- `$FORK` is the existing goose fork checkout.
- `$FORK_WT` is the new worktree (Task 0).
- `$HONK` is the honk repo root.
- Every path is relative to whichever repo the task names.

**honk is a public repo**
- No secrets, no org names, no absolute home paths in committed files or fixtures.
- Fixtures go under `fixtures/goose-1.54.0-patched.3/<probe>/<ts>.jsonl` and pass
  through the existing redactor. Review every fixture diff by eye before pushing.
- Other agents work in honk: stage only the files you changed (`git add <paths>`,
  never `git add -A`).

**Fork commit style**
- Subject is `<area>: <what the change does, as a sentence>`, lowercase area, no body
  needed. Examples from history:
  - `gcpauth: GCP_AUTH_SCOPES sets the scopes for authorized-user token refresh`
  - `ci: run the patched modules' unit tests before releasing`
- Tags are annotated: `goose v1.54.0 + local patches (<list>)`.

---

## Facts this plan relies on (verified 2026-10-11 against the source)

| Fact | Where |
|---|---|
| goose depends on `agent-client-protocol-http = { version = "2.2.0", default-features = false }` | fork `Cargo.toml:26` |
| goose enables the crate's `server` feature behind goose feature `acp-http` | `crates/goose/Cargo.toml:111, 211` |
| `Cargo.lock` pins 2.2.0 from crates.io, checksum `18e3d711…cc28f` | `Cargo.lock:101-104` |
| The fork **already** has a `[patch.crates-io]` table (`v8`, `cudaforge`) and a vendored `vendor/v8`, listed in workspace `members` | `Cargo.toml:1-7, 117-119` |
| The crate package ships **no LICENSE file** (Apache-2.0 per metadata). Its `.cargo_vcs_info.json` points at `agentclientprotocol/rust-sdk` commit `2a78849d3eb3dcb140dade3b8fc938cf1e2b9ce5`, path `src/agent-client-protocol-http` | crate tarball |
| `cargo test -p <pkg>` refuses a non-member that has dev-dependencies, so the vendored crate must be a workspace **member** | cargo behaviour |
| `patched-release.yml` triggers on `push: tags: patched-*` and runs three jobs: `build`, `test-patches`, `release` (detail below) | fork `.github/workflows/patched-release.yml` |
| `build-cli-linux.yml` builds with `cargo build --release -p goose-cli` and **no `--locked`**, so a stale `Cargo.lock` would be silently re-resolved in CI | `build-cli-linux.yml:144-170` |
| The fork's CI, cargo-deny and cargo-machete workflows do not gate tag releases. `ci.yml` runs only for `main`; the other two show as skipped on the fork | `ci.yml:3-13`, `gh run list` |
| `crates/goose` default features are `[]`. The developer shell module is unconditional | `crates/goose/Cargo.toml:12`, `platform_extensions/mod.rs:9` |
| `libc` is already a goose dependency on unix. `nix` is only transitive | `crates/goose/Cargo.toml:280-281` |
| goose already puts MCP subprocesses in their own process group with `command.process_group(0)`, which is precedent for patch 1 | `crates/goose/src/subprocess.rs:68-74` |
| goose builds `ServerOptions` with a struct literal, so new `ServerOptions` fields would break goose. Patches 2 and 3 use crate constants instead | `crates/goose/src/acp/transport/mod.rs:101-108` |
| `initialize` reports `agentInfo = ("goose", CARGO_PKG_VERSION)` = `"1.54.0"` for both upstream and the fork. **The version cannot tell patched from unpatched** | `crates/goose/src/acp/server.rs:1848` |
| Both NixOS hosts are aarch64, and neither has a `/lib64` ELF loader. The raw release binary will not exec until autoPatchelf runs; `pkgs/goose` in the workstation repo does this | workstation `pkgs/goose/default.nix` |

`patched-release.yml` jobs in detail:
- `build` calls `build-cli-linux.yml` at the tag's SHA. It produces gnu and musl, x86_64
  and aarch64.
- `test-patches` runs on `ubuntu-24.04-arm` with
  `cargo test --release -p goose --features "$F" --lib -- gcpvertexai gcpauth` and
  `cargo test --release -p goose-cli --lib -- cache_ttl`.
- `release` needs both. It uploads the `goose-*-unknown-linux-gnu` tarballs plus
  `SHA256SUMS`.

## Out of scope

- **`_goose/unstable/session/subscribe` (candidate 4).** honkd already fans out to
  viewers with its own cursor and backfill. The patch is large (about 25 call sites)
  and adds new protocol surface.
- **WS detach (candidate 1).** honkd uses HTTP+SSE, where turns already survive drops
  (A1). An honest version needs a resume protocol and acks on top of JSON-RPC.
- **`TCP_USER_TIMEOUT` stopgap.** It is superseded by takeover.
- **Browser CORS for the new headers.** `transport/mod.rs:150-170` doesn't list
  `Acp-Takeover` or `Last-Event-ID`. honkd is not a browser.

## Patch order

1. Shell process-group kill.
2. Vendor the crate, unmodified (its own commit, so patch diffs are reviewable).
3. Subscriber takeover.
4. Replay ring with `Last-Event-ID`.
5. CI.

The shell patch goes first because it is the smallest, it is independent, and its
first build warms the worktree's `target/`. Patch 3 builds on patch 2's lease rewrite,
so it must come after it. This is the requested order, with the vendoring step made
explicit.

## Deviation from `goose-patches.md` §2 (stated so a reviewer can veto it)

The spike sketched takeover as a per-subscriber pump task. The pump owns the lease,
forwards frames into a bounded channel, and takeover aborts it. This plan keeps the
spike's key insight: the stuck stream is never polled, so the lease must be **taken**,
not requested. It implements that differently, with **owner-token leases**:

- The mailbox's queue lives under a `std::sync::Mutex` together with `owner: Option<u64>`.
- A lease is just `(Arc<shared>, id)`. Every dequeue checks `owner == Some(id)` under the
  lock. Takeover sets `owner` to a new id, synchronously.
- The stuck generator's next poll, if it ever comes, sees it has been revoked and ends.
  It never holds the receiver, so nothing has to be aborted or awaited.

The reason for the change:
- A pump runs on its own, so it dequeues frames into its channel before hyper asks for
  them. A clean client drop would then lose those staged frames.
- That breaks the crate's existing test `sse_allows_one_active_subscriber_per_logical_stream`
  (`http_server.rs:1309-1383`). The test asserts that frames queued before a drop are
  delivered to the next subscriber.
- Owner tokens keep today's exact behaviour: a frame is dequeued only when hyper polls
  for it. They also need no task and no `JoinHandle` bookkeeping.

---

### Task 0: Pre-flight (read-only, plus one new worktree)

1. Confirm the fork checkout is clean and where it should be:
   ```bash
   git -C "$FORK" status --porcelain            # expect: empty
   git -C "$FORK" fetch fork
   git -C "$FORK" rev-parse fork/patched/v1.54.0 # expect: 66279407d…
   git -C "$FORK" log --oneline v1.54.0..fork/patched/v1.54.0   # expect: the 7 existing patch/ci commits (top: 66279407d)
   git -C "$FORK" tag -l 'patched-*'             # expect: patched-1.54.0.1, patched-1.54.0.2 (no .3)
   git -C "$FORK" ls-remote --tags fork 'patched-1.54.0.3'      # expect: empty
   ```
   If `.3` already exists anywhere, stop and ask.
2. Create the worktree on a new local branch tracking the fork branch:
   ```bash
   FORK_WT="$(dirname "$FORK")/goose-patched-track3"
   git -C "$FORK" worktree add -b patched/v1.54.0 "$FORK_WT" fork/patched/v1.54.0
   ```
3. Toolchain smoke test (short): `nix develop -c rustc --version` in `$FORK_WT`
   should print `1.99.0`.
4. Warm the build and record a baseline. Run (long, log `gp-baseline`):
   `cargo test -p goose --lib -- developer::shell`. Expected: all existing shell tests
   pass. Record the count. If anything fails here, stop: it is not ours.

---

### Task 1: honk — `GOOSE_BIN` override for the sandbox

**Files:**
- Modify: `scripts/goose-sandbox.sh` (launch block, lines 141-150)
- Modify: `scripts/goose-sandbox-check.sh` (after the cwd check)

**Step 1: Add the override** right before `cd "$SANDBOX"` (line 141):

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

Then change the last line of the `exec env` block from `goose serve …` to
`"$GOOSE_BIN" serve --host 127.0.0.1 --port "$PORT"`.

Do **not** run `goose --version` here. It would run before `GOOSE_PATH_ROOT` is set.

Also add this line to the header comment's launch example:
`#   add --setenv=GOOSE_BIN=<abs path> to run a specific goose build`

**Step 2: Make the check script report and optionally assert the binary.** Insert this
after the cwd check in `goose-sandbox-check.sh`:

```bash
# 2b. Report which binary is running; with HONK_EXPECT_GOOSE_BIN, require it.
actual_exe=$(readlink "/proc/$pid/exe" 2>/dev/null || true)
echo "goose exe: $actual_exe"
if [[ -n "${HONK_EXPECT_GOOSE_BIN:-}" ]]; then
    want_exe=$(readlink -f "$HONK_EXPECT_GOOSE_BIN")
    if [[ "$actual_exe" != "$want_exe" ]]; then
        echo "ERROR: sandbox runs $actual_exe, expected $want_exe" >&2
        exit 1
    fi
fi
```

**Step 3: Verify without touching the running sandbox.**
- Run `bash -n scripts/goose-sandbox.sh scripts/goose-sandbox-check.sh`. Expected: no
  output.
- Run `GOOSE_BIN=relative/goose HONK_SANDBOX=$(mktemp -d) bash scripts/goose-sandbox.sh`.
  Expected: it exits 1 with `GOOSE_BIN must be an absolute path`. That check comes
  after config generation, so it may need the config source. If it fails earlier on a
  missing secret or config, accept the earlier error and re-read the diff instead.

**Step 4: Commit.**
```bash
git add scripts/goose-sandbox.sh scripts/goose-sandbox-check.sh
git commit -m "scripts: GOOSE_BIN runs a specific goose build in the sandbox"
```

---

### Task 2: fork — patch 1, shell process-group kill

**Files (all in `$FORK_WT`):**
- Modify: `crates/goose/src/agents/platform_extensions/developer/shell.rs`
  - `run_command` `:557-668`: spawn at `:574-576`; kill sites `:605`, `:611`, `:622`
  - tests module `:949`

**Behaviour after the patch (unix):**
- The shell is spawned with `process_group(0)`, so it leads a new process group whose
  pgid is its pid.
- On cancel, on timeout, **or when the `run_command` future is dropped mid-run**,
  goose sends `SIGKILL` to the whole group (`libc::killpg`), then reaps the child as
  before.
- A command that exits on its own leaves what it backgrounded running. That is today's
  behaviour, and `shell_does_not_hang_on_backgrounded_process` relies on it.
- Non-unix behaviour is unchanged: still `start_kill()` only.

The drop case exists because the WS-close and DELETE paths (T5) abort the agent task.
That drops this future without ever reaching the `select!` cancel arm, so today it
orphans the shell too.

**Step 1: Write the failing tests.** Append them inside `mod tests` (after
`shell_kills_hanging_command_after_explicit_timeout`, before the closing `}`):

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

    #[cfg(unix)]
    #[tokio::test]
    async fn shell_cancellation_kills_the_whole_process_group() {
        let tool = ShellTool::new_for_test().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let token = CancellationToken::new();
        let run = tool.shell_with_cwd(
            ShellParams {
                command: process_tree_command(dir.path()),
                timeout_secs: None,
            },
            None,
            None,
            token.clone(),
        );
        tokio::pin!(run);

        let pids = tokio::select! {
            _ = &mut run => panic!("command finished before it was cancelled"),
            pids = read_pids(dir.path(), &["sh.pid", "a.pid", "b.pid"], Duration::from_secs(10)) => pids,
        };
        let (sh, a, b) = (pids[0], pids[1], pids[2]);
        assert_eq!(process_group_of(sh), Some(sh), "the shell must lead its own process group");
        assert_eq!(process_group_of(a), Some(sh), "descendants must be in the shell's group");

        token.cancel();
        let result = tokio::time::timeout(Duration::from_secs(5), run)
            .await
            .expect("cancellation must return promptly");
        assert!(extract_shell_output(&result).exit_code.is_none());
        for pid in [sh, a, b] {
            assert!(
                wait_until_dead(pid, Duration::from_secs(2)).await,
                "pid {pid} survived cancellation"
            );
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn shell_timeout_kills_the_whole_process_group() {
        let tool = ShellTool::new_for_test().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let result = tool
            .shell(ShellParams {
                command: process_tree_command(dir.path()),
                timeout_secs: Some(1),
            })
            .await;
        assert!(extract_shell_output(&result).timed_out);
        let pids = read_pids(dir.path(), &["sh.pid", "a.pid", "b.pid"], Duration::from_secs(1)).await;
        for pid in pids {
            assert!(
                wait_until_dead(pid, Duration::from_secs(2)).await,
                "pid {pid} survived the timeout"
            );
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn dropping_a_running_shell_call_kills_the_whole_process_group() {
        let tool = ShellTool::new_for_test().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let pids = {
            let run = tool.shell(ShellParams {
                command: process_tree_command(dir.path()),
                timeout_secs: None,
            });
            tokio::pin!(run);
            tokio::select! {
                _ = &mut run => panic!("command finished on its own"),
                pids = read_pids(dir.path(), &["sh.pid", "a.pid", "b.pid"], Duration::from_secs(10)) => pids,
            }
        }; // `run` is dropped here, as when an aborted agent task drops the tool future.
        for pid in pids {
            assert!(
                wait_until_dead(pid, Duration::from_secs(2)).await,
                "pid {pid} survived the dropped call"
            );
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn process_group_is_left_alone_when_the_shell_exits_normally() {
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
```

**Step 2: Run them and watch them fail.** Run (long, log `gp-shell-red`):
`cargo test -p goose --lib -- process_group`.

Expected: 4 tests run.
- `shell_cancellation_kills_the_whole_process_group` FAILS at "the shell must lead its
  own process group".
- `shell_timeout_…` and `dropping_…` FAIL with `pid … survived`.
- `process_group_is_left_alone_when_the_shell_exits_normally` PASSES. It guards
  existing behaviour.

**Step 3: Implement.**

(a) Add the guard type above `fn run_command` (around `:556`):

```rust
/// Kills a shell command's whole process group if the command is cancelled, times out,
/// or its future is dropped mid-run (e.g. the agent task is aborted on WS close or
/// DELETE). The shell is spawned with `process_group(0)`, so the group id is the
/// shell's pid and every descendant that does not call `setsid`/`setpgid` is in it.
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

    fn kill(&mut self) {
        if let Some(pgid) = self.pgid.take() {
            // SAFETY: killpg only sends a signal. The group id cannot have been recycled:
            // its leader is our child and stays unreaped until `child.wait()` returns,
            // and the caller disarms before acting on a normal exit.
            unsafe {
                libc::killpg(pgid, libc::SIGKILL);
            }
        }
    }
}

#[cfg(unix)]
impl Drop for ProcessGroupKiller {
    fn drop(&mut self) {
        self.kill();
    }
}
```

(b) In `run_command`:
- After `let mut command = build_shell_command(…);` (`:568`), add:
  ```rust
  #[cfg(unix)]
  command.process_group(0);
  ```
  It goes here and not in `build_shell_command`, so that function stays a pure
  description of the command line.
- After the spawn (`:576`), add:
  ```rust
  #[cfg(unix)]
  let mut group = ProcessGroupKiller::new(&child);
  ```
- At each of the three kill sites (`:605`, `:611`, `:622`), insert this line
  **before** `let _ = child.start_kill();`:
  ```rust
  #[cfg(unix)]
  group.kill();
  ```
  Keep `start_kill()`: it is the non-unix path, and on unix it is a harmless no-op on a
  dead leader.
- Directly after the `let exit_code = if … { … };` expression ends (`:627`), add:
  ```rust
  #[cfg(unix)]
  group.disarm();
  ```

Flatpak: `process_group(0)` applies to `flatpak-spawn`. `killpg` then reaches
`flatpak-spawn` only, which is the same reach as today. Host-side descendants are
outside our namespace either way.

**Step 4: Run the tests and watch them pass.** Run (long, log `gp-shell-green`):
`cargo test -p goose --lib -- developer::shell`.
Expected: all shell tests pass, the baseline count from Task 0 plus 4.

Then run clippy. Run (long, log `gp-shell-clippy`):
`cargo clippy -p goose --lib --tests -- -D warnings`. Expected: clean.

**Step 5: Commit.**
```bash
git -C "$FORK_WT" add crates/goose/src/agents/platform_extensions/developer/shell.rs
git -C "$FORK_WT" commit -m "developer/shell: kill the command's whole process group on cancel, timeout and drop"
```

**Known behaviour change, recorded for the upstream proposal.** In interactive
`goose` CLI use, the shell is no longer in the terminal's foreground process group:
- Terminal Ctrl-C no longer reaches shell children directly. It is goose's own Ctrl-C
  handling, which cancels, that now kills them.
- A child that opens `/dev/tty` (an ssh or sudo password prompt) gets `SIGTTIN` and
  stops until the timeout kills it.

`stdin` is already `/dev/null`, and goose already does the same for MCP subprocesses.
`goose serve` has no terminal, so none of this applies there.

---

### Task 3: fork — vendor `agent-client-protocol-http` 2.2.0, unmodified

**Files (in `$FORK_WT`):**
- Create: `vendor/agent-client-protocol-http/**`, the crate tarball contents minus its
  nested `Cargo.lock`
- Create: `vendor/agent-client-protocol-http/LICENSE`
- Create: `vendor/agent-client-protocol-http/VENDORED.md`
- Modify: `Cargo.toml` (`members` `:2-7`; `[patch.crates-io]` `:117-119`)
- Modify: `Cargo.lock`, regenerated by cargo and never hand-edited

**Step 1: Fetch and verify the exact published crate.**
```bash
mkdir -p /tmp/opencode/acp-vendor && cd /tmp/opencode/acp-vendor
curl -fsSL -o acp-http-2.2.0.crate \
  https://static.crates.io/crates/agent-client-protocol-http/agent-client-protocol-http-2.2.0.crate
echo "18e3d7117a48e9db3c4b5e29f7539bc14e390f1f782d10a99b1d9f5c3a8cc28f  acp-http-2.2.0.crate" | sha256sum -c
tar -xzf acp-http-2.2.0.crate
```
Expected: `acp-http-2.2.0.crate: OK`. The checksum is the one in the fork's
`Cargo.lock:104`. Any mismatch: stop.

**Step 2: Copy it into the worktree.**
```bash
cp -a /tmp/opencode/acp-vendor/agent-client-protocol-http-2.2.0 "$FORK_WT/vendor/agent-client-protocol-http"
rm "$FORK_WT/vendor/agent-client-protocol-http/Cargo.lock"   # members' lockfiles are ignored; avoid confusion
```
- Keep `Cargo.toml`. It is the normalized, self-contained manifest, so it needs no
  workspace inheritance.
- Keep `Cargo.toml.orig`, `.cargo_vcs_info.json`, `README.md` and `CHANGELOG.md`
  unchanged, as provenance.

**Step 3: Add the license.** The crate tarball has none.
1. List the upstream repo root at the recorded commit:
   ```bash
   SHA=2a78849d3eb3dcb140dade3b8fc938cf1e2b9ce5
   gh api "repos/agentclientprotocol/rust-sdk/contents?ref=$SHA" -q '.[].name'
   gh api "repos/agentclientprotocol/rust-sdk/contents/src/agent-client-protocol-http?ref=$SHA" -q '.[].name'
   ```
2. Fetch the license file you found (the crate directory's own, if it has one, else
   the repo root's). Replace `<NAME>` with its exact path from the listing:
   ```bash
   gh api -H 'Accept: application/vnd.github.raw' \
     "repos/agentclientprotocol/rust-sdk/contents/<NAME>?ref=$SHA" \
     > "$FORK_WT/vendor/agent-client-protocol-http/LICENSE"
   head -3 "$FORK_WT/vendor/agent-client-protocol-http/LICENSE"   # expect "Apache License" / "Version 2.0"
   ```
   If the repo has no license file at that commit, copy goose's root `LICENSE`
   (also Apache-2.0). Say so in `VENDORED.md`.

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

## Local modifications (each is one commit on patched/v1.54.0)

- `acp-http: Acp-Takeover: 1 …` — subscriber takeover (owner-token leases).
- `acp-http: SSE event ids, … Last-Event-ID resume` — replay ring.

## When to delete this directory

When an upstream crate release contains equivalent takeover + replay and goose depends
on it. On every rebase onto a newer goose: if goose's `agent-client-protocol-http`
requirement leaves 2.x, the `[patch]` silently stops applying (cargo only warns
"patch was not used"); the release workflow's graph check fails in that case.
```

Leave the "Local modifications" bullets in now. Tasks 4 and 5 make them true before
the release.

**Step 5: Wire it into the workspace.** In the root `Cargo.toml`:

```toml
[workspace]
members = [
    "crates/*",
    # Mainly for cargo-machete to not error out during inspection.
    "vendor/v8",
    # Vendored + patched ACP HTTP transport; a member so `cargo test -p` can run its tests.
    "vendor/agent-client-protocol-http",
]
```

and in the existing table (`:117`):

```toml
[patch.crates-io]
v8 = { path = "vendor/v8" }
agent-client-protocol-http = { path = "vendor/agent-client-protocol-http" }
cudaforge = { git = "https://github.com/jbg/cudaforge", rev = "e7c1967340e40673db98dc9e17da0f04834a456f" }
```

**Step 6: Regenerate the lockfile minimally and check the graph.** Use these short
commands (resolution only):
```bash
nix develop -c cargo metadata --format-version 1 > /dev/null     # rewrites Cargo.lock
git -C "$FORK_WT" diff --stat Cargo.lock
git -C "$FORK_WT" diff Cargo.lock | grep '^[-+]version' || true
nix develop -c cargo tree --locked -p goose-cli --features acp-http -i agent-client-protocol-http --depth 0
```

Expected:
- In the `agent-client-protocol-http` lock entry, the `source` and `checksum` lines
  disappear. New `[[package]]` entries may appear for the crate's dev-dependencies.
- **No `-version` lines.** No existing package changes version.
- `cargo tree` prints `agent-client-protocol-http v2.2.0 (…/vendor/agent-client-protocol-http)`.
- Never run `cargo update`.

**Step 7: Prove the vendored copy is unmodified and its own tests pass.**
```bash
diff -r /tmp/opencode/acp-vendor/agent-client-protocol-http-2.2.0 "$FORK_WT/vendor/agent-client-protocol-http"
```
Expected: only `Only in …-2.2.0: Cargo.lock`, `Only in vendor/…: LICENSE` and
`Only in vendor/…: VENDORED.md`.

Run (long, log `gp-acp-baseline`):
`cargo test -p agent-client-protocol-http --features server --lib`.
Expected: all crate tests pass. Record the count.

Also run (long, log `gp-acp-clippy-baseline`):
`cargo clippy -p agent-client-protocol-http --features server --all-targets`.
Record the warning count as the baseline. The crate enables `clippy::pedantic` as
warn, and our later patches must add no new warnings.

**Step 8: Commit.**
```bash
git -C "$FORK_WT" add Cargo.toml Cargo.lock vendor/agent-client-protocol-http
git -C "$FORK_WT" commit -m "vendor: agent-client-protocol-http 2.2.0 from crates.io, unmodified, via [patch.crates-io]"
```

---

### Task 4: fork — patch 2, subscriber takeover

**Wire contract (new):**
- **Request header `Acp-Takeover: 1` on `GET /acp`.** If the requested stream
  (connection stream, or the session stream named by `Acp-Session-Id`) is held, the new
  subscriber takes it: `200`, not `409`.
- Without the header, nothing changes: a held stream still answers `409`.
- **Response header `Acp-Stream-Features: takeover`** goes on every `200` **and**
  `409` from `GET /acp`. Patch 3 extends the value to `takeover, replay`.
- Takeover is same-connection-id only. The connection id already authorizes POST and
  DELETE, so no new privilege is added.
- Takeover cannot move a live run to another connection. Live frames are bound to the
  connection that prompted (P3c).

**Files (in `$FORK_WT/vendor/agent-client-protocol-http/src/`):**
- Modify: `connection.rs`
  - `OutboundMailbox`/`OutboundLease` `:37-104`: rewrite as owner-token leases
  - `Connection` `:132-144`: add take-over methods
  - `OutboundTransport` `:232-251`: add take-over methods
  - tests `:607+`
- Modify: `http_server.rs`: `handle_get` `:327-415`; tests `:478+`
- Modify: `protocol.rs`: new header constants next to `:3-8`

**Step 1: Write the failing mailbox tests.** Add them to `connection.rs` `mod tests`,
after `outbound_mailbox_has_one_active_owner_and_preserves_queued_frames`:

```rust
    #[tokio::test]
    async fn outbound_mailbox_take_over_revokes_the_old_lease_without_polling_it() {
        let mailbox = OutboundMailbox::new();
        let mut old = mailbox.try_acquire().unwrap();
        mailbox.push("first".to_string()).unwrap();
        assert!(
            mailbox.try_acquire().is_none(),
            "plain acquire still refuses a held stream"
        );

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
        let mailbox = OutboundMailbox::new();
        let old = mailbox.try_acquire().unwrap();
        let _new = mailbox.take_over();
        drop(old);
        assert!(mailbox.try_acquire().is_none());
    }

    #[tokio::test]
    async fn take_over_wakes_a_revoked_lease_parked_in_recv() {
        let mailbox = OutboundMailbox::new();
        let mut old = mailbox.try_acquire().unwrap();
        let parked = tokio::spawn(async move { old.recv().await });
        sleep(Duration::from_millis(20)).await;
        let _new = mailbox.take_over();
        assert_eq!(
            timeout(Duration::from_secs(1), parked).await.unwrap().unwrap(),
            None
        );
    }
```

**Step 2: Write the failing HTTP tests.** Add them to `http_server.rs` `mod tests`.

Helpers go near the factories. Tagged frames are routed as `session/update`
notifications, so `route_outbound` picks the stream from `sessionId`, exactly as in
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
```

The tests:

```rust
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
                handle_get(
                    registry.clone(),
                    sse_get(&connection_id, session_id, &[(HEADER_TAKEOVER, "1")]),
                ),
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
            let ping = timeout(Duration::from_secs(1), forwarded_rx.recv())
                .await
                .unwrap()
                .unwrap();
            assert_eq!(method_for_message(&ping), Some("test/ping"));

            // Nothing the stuck stream never dequeued is lost.
            connection.shutdown().await;
            let body = timeout(
                Duration::from_secs(1),
                axum::body::to_bytes(taken.into_body(), 64 * 1024),
            )
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
        let _new = handle_get(
            registry.clone(),
            sse_get(&connection_id, None, &[(HEADER_TAKEOVER, "1")]),
        )
        .await;
        let body = timeout(
            Duration::from_secs(1),
            axum::body::to_bytes(old.into_body(), 1024),
        )
        .await
        .expect("a revoked stream must end, not idle on keepalives")
        .unwrap();
        assert!(body.is_empty());
    }
```

**Step 3: Run them and watch them fail.** Run (long, log `gp-takeover-red`):
`cargo test -p agent-client-protocol-http --features server --lib`.
Expected: a compile error naming `take_over`, `HEADER_TAKEOVER`,
`HEADER_STREAM_FEATURES` or `STREAM_FEATURES`.

**Step 4: Implement.**

(a) In `protocol.rs`, next to the existing constants:

```rust
#[cfg(feature = "server")]
pub(crate) const HEADER_TAKEOVER: &str = "acp-takeover";
#[cfg(feature = "server")]
pub(crate) const HEADER_STREAM_FEATURES: &str = "acp-stream-features";
```

(b) In `connection.rs`, replace `OutboundMailbox`, `OutboundLease`, their `impl`s and
`Drop` (`:37-104`) with the code below. Add `Notify` to the `tokio::sync` import. `mpsc`
stays imported, because it is still used for `TryRecvError` and the inbound/outbound
channels.

```rust
struct MailboxState {
    queue: VecDeque<String>,
    /// Lease that currently owns the stream. Takeover replaces it; a revoked lease's
    /// next dequeue sees the mismatch and ends, so it never has to be polled.
    owner: Option<u64>,
    next_lease_id: u64,
}

struct MailboxShared {
    state: StdMutex<MailboxState>,
    notify: Notify,
}

impl MailboxShared {
    fn lock(&self) -> std::sync::MutexGuard<'_, MailboxState> {
        self.state.lock().expect("outbound mailbox lock poisoned")
    }
}

struct OutboundMailbox {
    shared: Arc<MailboxShared>,
}

pub(crate) struct OutboundLease {
    shared: Arc<MailboxShared>,
    id: u64,
}

impl OutboundMailbox {
    fn new() -> Self {
        Self {
            shared: Arc::new(MailboxShared {
                state: StdMutex::new(MailboxState {
                    queue: VecDeque::new(),
                    owner: None,
                    next_lease_id: 0,
                }),
                notify: Notify::new(),
            }),
        }
    }

    #[allow(clippy::unnecessary_wraps)] // keeps the call sites' existing Result contract
    fn push(&self, msg: String) -> Result<(), &'static str> {
        self.shared.lock().queue.push_back(msg);
        self.shared.notify.notify_waiters();
        Ok(())
    }

    fn try_acquire(&self) -> Option<OutboundLease> {
        let mut state = self.shared.lock();
        if state.owner.is_some() {
            return None;
        }
        Some(self.lease(&mut state))
    }

    /// Takes the stream from its current owner, if any. Works even if the old
    /// subscriber's SSE body is never polled again (a half-open socket).
    fn take_over(&self) -> OutboundLease {
        let lease = {
            let mut state = self.shared.lock();
            self.lease(&mut state)
        };
        // Wake a revoked lease parked in `recv` so a still-polled old stream ends.
        self.shared.notify.notify_waiters();
        lease
    }

    fn lease(&self, state: &mut MailboxState) -> OutboundLease {
        let id = state.next_lease_id;
        state.next_lease_id += 1;
        state.owner = Some(id);
        OutboundLease {
            shared: self.shared.clone(),
            id,
        }
    }
}

fn take_next(shared: &MailboxShared, id: u64) -> Result<String, mpsc::error::TryRecvError> {
    let mut state = shared.lock();
    if state.owner != Some(id) {
        return Err(mpsc::error::TryRecvError::Disconnected);
    }
    state
        .queue
        .pop_front()
        .ok_or(mpsc::error::TryRecvError::Empty)
}

impl OutboundLease {
    /// Next frame, or `None` once this lease has been taken over.
    pub(crate) async fn recv(&mut self) -> Option<String> {
        let shared = self.shared.clone();
        loop {
            // Register for a wakeup before checking, so a push between the check and
            // the await is not missed.
            let mut notified = std::pin::pin!(shared.notify.notified());
            notified.as_mut().enable();
            match take_next(&shared, self.id) {
                Ok(msg) => return Some(msg),
                Err(mpsc::error::TryRecvError::Disconnected) => return None,
                Err(mpsc::error::TryRecvError::Empty) => notified.await,
            }
        }
    }

    pub(crate) fn try_recv(&mut self) -> Result<String, mpsc::error::TryRecvError> {
        take_next(&self.shared, self.id)
    }
}

impl Drop for OutboundLease {
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

(c) Add take-over methods beside the subscribe methods. On `Connection`, after
`subscribe_session_stream` (`:140`):

```rust
    pub(crate) fn take_over_connection_stream(&self) -> Option<OutboundLease> {
        self.outbound_transport.take_over_connection_stream()
    }

    pub(crate) async fn take_over_session_stream(&self, session_id: &str) -> Option<OutboundLease> {
        self.outbound_transport.take_over_session_stream(session_id).await
    }
```

On `OutboundTransport`, after `subscribe_session_stream` (`:244`):

```rust
    fn take_over_connection_stream(&self) -> Option<OutboundLease> {
        match self {
            Self::Http(http) => Some(http.connection_stream.take_over()),
            Self::WebSocket(_) => None,
        }
    }

    async fn take_over_session_stream(&self, session_id: &str) -> Option<OutboundLease> {
        match self {
            Self::Http(http) => Some(http.session_stream(session_id).await.take_over()),
            Self::WebSocket(_) => None,
        }
    }
```

(d) In `http_server.rs`:
- Import `HEADER_TAKEOVER` and `HEADER_STREAM_FEATURES` from `crate::protocol`.
- Add these items:

```rust
/// Advertised on every GET /acp response so clients can feature-detect.
pub(crate) const STREAM_FEATURES: &str = "takeover";

fn with_stream_features(mut response: Response) -> Response {
    response.headers_mut().insert(
        HEADER_STREAM_FEATURES,
        HeaderValue::from_static(STREAM_FEATURES),
    );
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
            (
                StatusCode::CONFLICT,
                "outbound stream already has a subscriber",
            )
                .into_response(),
        );
    };
```

- Wrap the success response. Change `let mut response = with_connection_header(…)` at
  `:399` to `let mut response = with_stream_features(with_connection_header(…));`.

The SSE generator body (`:364-397`) is **unchanged**. A revoked lease makes
`try_recv()` return `Disconnected`, which ends the `while let Ok` loop, and makes
`recv()` return `None`, which hits the existing `None => break`.

**Step 5: Run the tests and watch them pass.** Run (long, log `gp-takeover-green`):
`cargo test -p agent-client-protocol-http --features server --lib`.

Expected: every test passes, the Task 3 count plus 5.
- `sse_allows_one_active_subscriber_per_logical_stream` must still pass unchanged. It
  is the regression guard for the design choice.
- So must all `websocket_server` tests, whose path uses `try_acquire`/`recv`/`try_recv`
  unchanged.

Then run clippy (long, log `gp-takeover-clippy`), the same command as the Task 3
baseline. Expected: warning count ≤ baseline.

Then build goose against it (long, log `gp-takeover-goose`):
`cargo check -p goose --features acp-http`. Expected: clean. Goose uses only
`AcpHttpServer`, `ServerOptions` and `CorsOptions`, and none of them changed.

**Step 6: Commit.**
```bash
git -C "$FORK_WT" add vendor/agent-client-protocol-http/src
git -C "$FORK_WT" commit -m "acp-http: Acp-Takeover: 1 on GET /acp takes a held SSE stream instead of answering 409"
```

---

### Task 5: fork — patch 3, replay ring and `Last-Event-ID`

**Wire contract (new):**
- **Every SSE frame on an HTTP stream carries `id: <seq>`.** `seq` is a per-stream
  (per-mailbox) monotonic `u64` that starts at 1 and is stamped at enqueue. Each
  connection stream and each session stream has its own sequence.
- Keepalives carry no id.
- **Ring.** When a frame is dequeued for writing, it is also appended to a per-stream
  ring, atomically under the same lock. The ring keeps at most
  `REPLAY_RING_MAX_FRAMES = 4096` frames and `REPLAY_RING_MAX_BYTES = 1 MiB`, evicting
  oldest first.
- WebSocket mailboxes keep no ring.
- **Request header `Last-Event-ID: k` on `GET /acp`.**
  1. The server checks for a gap.
  2. It drops ring entries `≤ k`. That is an implicit ack.
  3. It sends the retained frames `> k` first, then live frames.
  4. The response carries **`Acp-Replay: complete`** if nothing after `k` was lost, or
     **`Acp-Replay: gap`** if some frame after `k` was evicted or already acked.
     `k` greater than the last issued seq is also a gap, and so is an unparseable `k`.
- Without `Last-Event-ID`, nothing changes except the added `id:` lines.
- `Acp-Stream-Features` becomes `takeover, replay`.
- Combined with `Acp-Takeover: 1`, this recovers the frames a half-open stream had
  dequeued but never delivered. That is P2d's loss and P4b's lost permission.

**Files:** `vendor/agent-client-protocol-http/src/{connection.rs,http_server.rs,protocol.rs}`.

**Step 1: Write the failing mailbox tests** in `connection.rs` `mod tests`:

```rust
    #[tokio::test]
    async fn outbound_mailbox_resume_replays_dequeued_frames_after_last_seen() {
        let mailbox = OutboundMailbox::new();
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
        assert!(matches!(
            resumed.try_recv_frame(),
            Err(mpsc::error::TryRecvError::Empty)
        ));
    }

    #[tokio::test]
    async fn outbound_mailbox_resume_below_the_ring_floor_is_a_gap() {
        let mailbox = OutboundMailbox::with_ring_limits(2, usize::MAX);
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
    async fn outbound_mailbox_ring_respects_the_byte_budget() {
        let mailbox = OutboundMailbox::with_ring_limits(usize::MAX, 10);
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
        let mailbox = OutboundMailbox::new();
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
        let mailbox = OutboundMailbox::new();
        mailbox.push("m1".to_string()).unwrap();
        let mut lease = mailbox.try_acquire().unwrap();
        assert_eq!(lease.resume_after(9), Replay::Gap);
    }

    #[tokio::test]
    async fn websocket_mailbox_keeps_no_ring() {
        let mailbox = OutboundMailbox::without_replay();
        mailbox.push("m1".to_string()).unwrap();
        let mut lease = mailbox.try_acquire().unwrap();
        lease.recv().await.unwrap();
        drop(lease);
        let mut resumed = mailbox.try_acquire().unwrap();
        assert_eq!(resumed.resume_after(0), Replay::Gap);
        assert!(matches!(
            resumed.try_recv_frame(),
            Err(mpsc::error::TryRecvError::Empty)
        ));
    }

    #[tokio::test]
    async fn take_over_then_resume_recovers_what_the_revoked_lease_dequeued() {
        let mailbox = OutboundMailbox::new();
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

**Step 2: Write the failing HTTP tests** in `http_server.rs` `mod tests`. Reuse the
Task 4 helpers.

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

        let resumed = handle_get(
            registry.clone(),
            sse_get(&connection_id, None, &[(HEADER_LAST_EVENT_ID, "1")]),
        )
        .await;
        assert_eq!(resumed.status(), StatusCode::OK);
        assert_eq!(resumed.headers()[HEADER_REPLAY], "complete");
        connection.shutdown().await;
        let body = String::from_utf8(
            axum::body::to_bytes(resumed.into_body(), 64 * 1024).await.unwrap().to_vec(),
        )
        .unwrap();
        assert_eq!(event_ids(&body), [2, 3, 4]);
        assert_eq!(data_tags(&body), ["two", "three", "four"]);
    }

    #[tokio::test]
    async fn sse_last_event_id_older_than_the_ring_flags_a_gap() {
        let (registry, _forwarded_rx) = capturing_registry();
        let (connection_id, connection) = registry.create_connection().await;
        let total = crate::connection::REPLAY_RING_MAX_FRAMES + 8;
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

        let resumed = handle_get(
            registry.clone(),
            sse_get(&connection_id, None, &[(HEADER_LAST_EVENT_ID, "0")]),
        )
        .await;
        assert_eq!(resumed.headers()[HEADER_REPLAY], "gap");
        connection.shutdown().await;
        let body = String::from_utf8(
            axum::body::to_bytes(resumed.into_body(), 4 * 1024 * 1024).await.unwrap().to_vec(),
        )
        .unwrap();
        let ids = event_ids(&body);
        assert_eq!(ids.len(), crate::connection::REPLAY_RING_MAX_FRAMES);
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
                sse_get(
                    &connection_id,
                    session_id,
                    &[(HEADER_TAKEOVER, "1"), (HEADER_LAST_EVENT_ID, "0")],
                ),
            )
            .await;
            assert_eq!(taken.status(), StatusCode::OK);
            assert_eq!(taken.headers()[HEADER_REPLAY], "complete");
            connection.shutdown().await;
            let body = String::from_utf8(
                axum::body::to_bytes(taken.into_body(), 64 * 1024).await.unwrap().to_vec(),
            )
            .unwrap();
            assert_eq!(event_ids(&body), [1, 2, 3]);
            assert_eq!(data_tags(&body), ["one", "two", "three"]);
            drop(stuck_body); // the half-open socket lingers; harmless
        }
    }
```

**Step 3: Run them and watch them fail.** Run (long, log `gp-replay-red`), the same
crate test command. Expected: a compile error naming `recv_frame`, `resume_after`,
`Replay`, `with_ring_limits`, `without_replay`, `REPLAY_RING_MAX_FRAMES` or
`HEADER_LAST_EVENT_ID`/`HEADER_REPLAY`.

**Step 4: Implement.**

(a) In `protocol.rs`:

```rust
#[cfg(feature = "server")]
pub(crate) const HEADER_LAST_EVENT_ID: &str = "last-event-id";
#[cfg(feature = "server")]
pub(crate) const HEADER_REPLAY: &str = "acp-replay";
```

(b) In `connection.rs`, extend the Task 4 mailbox:

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

struct MailboxState {
    queue: VecDeque<(u64, String)>,
    ring: VecDeque<(u64, String)>,
    ring_bytes: usize,
    max_ring_frames: usize,
    max_ring_bytes: usize,
    /// Highest seq removed from the ring (evicted or acknowledged). Resuming from
    /// below it, or past the last issued seq, is a gap.
    trimmed_through: u64,
    next_seq: u64,
    owner: Option<u64>,
    next_lease_id: u64,
}

impl MailboxState {
    /// Called under the lock at the moment a frame is dequeued for writing.
    fn record_dequeued(&mut self, seq: u64, msg: &str) {
        if self.max_ring_frames == 0 {
            self.trimmed_through = seq;
            return;
        }
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

Mailbox constructors:
- `OutboundMailbox::new()` calls `Self::with_ring_limits(REPLAY_RING_MAX_FRAMES, REPLAY_RING_MAX_BYTES)`.
- `fn without_replay() -> Self { Self::with_ring_limits(0, 0) }`.
- `fn with_ring_limits(max_frames, max_bytes)` builds the state with `next_seq: 1`. It
  is non-test code, because `new` uses it.

Use `without_replay()` in `WebSocketOutbound::new` (`:412-418`).

`push` stamps the sequence number:

```rust
    fn push(&self, msg: String) -> Result<(), &'static str> {
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

The lease gains a `replay: VecDeque<(u64, String)>` field. It is empty in `lease()`.
Replace `take_next`, and extend the lease methods:

```rust
fn take_next(
    shared: &MailboxShared,
    id: u64,
    replay: &mut VecDeque<(u64, String)>,
) -> Result<(u64, String), mpsc::error::TryRecvError> {
    let mut state = shared.lock();
    if state.owner != Some(id) {
        return Err(mpsc::error::TryRecvError::Disconnected);
    }
    if let Some(frame) = replay.pop_front() {
        return Ok(frame); // already in the ring; do not record twice
    }
    let (seq, msg) = state.queue.pop_front().ok_or(mpsc::error::TryRecvError::Empty)?;
    state.record_dequeued(seq, &msg);
    Ok((seq, msg))
}

impl OutboundLease {
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
                Err(mpsc::error::TryRecvError::Disconnected) => return None,
                Err(mpsc::error::TryRecvError::Empty) => notified.await,
            }
        }
    }

    pub(crate) fn try_recv_frame(&mut self) -> Result<(u64, String), mpsc::error::TryRecvError> {
        take_next(&self.shared, self.id, &mut self.replay)
    }

    pub(crate) async fn recv(&mut self) -> Option<String> {
        self.recv_frame().await.map(|(_, msg)| msg)
    }

    pub(crate) fn try_recv(&mut self) -> Result<String, mpsc::error::TryRecvError> {
        self.try_recv_frame().map(|(_, msg)| msg)
    }
}
```

`last_seen >= next_seq` means `last_seen > last issued`, since seqs start at 1.

(c) In `http_server.rs`:
- Change `STREAM_FEATURES` to `"takeover, replay"`. Import `HEADER_LAST_EVENT_ID`,
  `HEADER_REPLAY` and `crate::connection::Replay`.
- In `handle_get`, after the receiver is acquired and before `subscribe_closed`:

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

- In the generator, replace every `receiver.try_recv()` with `receiver.try_recv_frame()`
  and `receiver.recv()` with `receiver.recv_frame()`. Bind `(seq, msg)`, and build
  every event with a helper:

```rust
fn sse_event(seq: u64, msg: String) -> Event {
    Event::default().id(seq.to_string()).data(msg)
}
```

  There are four yield sites, at `:368`, `:373`, `:382` and `:390`.
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

**Step 5: Update the three exact-byte assertions** that `id:` lines legitimately
change. axum writes fields in call order, so the format is `id: N\ndata: …\n\n`.
- `:1355-1358`: `"id: 1\ndata: already-read\n\n"`.
- `:1375-1382`: `"id: 2\ndata: queued-before-drop-1\n\n"`,
  `"id: 3\ndata: queued-before-drop-2\n\n"`, `"id: 4\ndata: queued-after-drop\n\n"`.
- `:1447-1450`: `"id: 1\ndata: queued-before-subscribe\n\n"`.

The burst test (`:1261`) filters `data: ` lines and is unaffected.

**Step 6: Run the tests and watch them pass.** Run (long, log `gp-replay-green`), the
crate test command. Expected: all pass, the Task 4 count plus 10. Any failure other
than the three assertions listed in Step 5 is a real regression: fix it, do not edit
the assertion.

Then run clippy (≤ baseline), plus `cargo check -p goose --features acp-http`.

**Step 7: Commit.**
```bash
git -C "$FORK_WT" add vendor/agent-client-protocol-http/src
git -C "$FORK_WT" commit -m "acp-http: SSE event ids, a bounded per-stream replay ring, and Last-Event-ID resume"
```

Memory note for the findings: the ring holds up to 1 MiB per HTTP stream until a
`Last-Event-ID` acks it. At honkd's 16 held sessions that is about 17 MiB worst case.

---

### Task 6: fork — release workflow runs the new tests and checks the vendoring

**Files:** `$FORK_WT/.github/workflows/patched-release.yml` (`test-patches` job).

**Step 1: Edit.** The workflow file is read from the **tagged commit**, so this must
be committed before tagging. Insert this step before `Test patched modules`:

```yaml
      - name: Vendored acp-http is the one in the build graph
        # An unused [patch] is only a cargo warning; fail loudly instead. --locked also
        # fails if Cargo.lock is stale (build-cli-linux.yml does not pass --locked).
        run: |
          cargo tree --locked -p goose-cli --features acp-http -i agent-client-protocol-http --depth 0 \
            | tee /dev/stderr | grep -F 'vendor/agent-client-protocol-http'
```

Change `Test patched modules` to:

```yaml
      - name: Test patched modules
        run: |
          F=code-mode,tree-sitter,aws-providers,telemetry,otel,rustls-tls,live-voice,system-keyring,platform-apps,chat-recall,scheduler,acp-http,online-model-meta
          cargo test --release -p goose --features "$F" --lib -- gcpvertexai gcpauth process_group
          cargo test --release -p goose-cli --lib -- cache_ttl
          cargo test --release -p agent-client-protocol-http --features server --lib
```

Also update the comment above `test-patches` to:
`# Unit tests for the patched code paths (including the vendored acp-http crate), on the same toolchain.`

**Step 2: Verify locally.**
- Run the `cargo tree` command from the step above (short). Expected: one line ending
  in `vendor/agent-client-protocol-http)`.
- Run (long, log `gp-ci-parity`) the exact `F=…` goose line from the workflow, with
  `--release`.
  - Expected: the `process_group` tests pass under the release feature set.
  - If this local run fails on missing system libraries (protobuf, cmake or clang for
    `code-mode`/`live-voice`), that is the devShell, not the patch. Record it, rely on
    the CI job, and say so in the report.

**Step 3: Commit.**
```bash
git -C "$FORK_WT" add .github/workflows/patched-release.yml
git -C "$FORK_WT" commit -m "ci: run the process-group and vendored acp-http tests, and check the vendored crate is in the graph, before releasing"
```

---

### Task 7: honk — `honk-upstream` learns takeover, event ids and replay, behind feature detection

**Files:**
- Modify: `crates/honk-upstream/src/sse.rs`
- Modify: `crates/honk-upstream/src/conn.rs`
  - struct `:73-89`; constructors `:96-204`; `subscribe` `:394-499`; tests
- Modify: `crates/honk-upstream/src/lib.rs` (re-exports)

Behaviour against **unpatched** goose is unchanged by construction:
- `subscribe()` keeps its signature and sends no new headers.
- `subscribe_with` sends headers that unpatched goose ignores. It reports
  `ReplayOutcome::Unsupported` unless the server explicitly answers
  `Acp-Replay: complete`.

**Step 1: Write the failing tests.**

`sse.rs` `mod tests`:

```rust
    #[tokio::test]
    async fn frame_ids_are_parsed_and_absent_ids_are_none() {
        let chunks: Vec<Result<Bytes, std::io::Error>> = vec![
            Ok(Bytes::from("data: {\"a\":1}\n\n")),
            Ok(Bytes::from("id: 7\ndata: {\"b\":2}\n\n")),
            Ok(Bytes::from("data:\n\n")),
        ];
        let frames: Vec<SseFrame> = sse_to_frames(futures::stream::iter(chunks))
            .map(|r| r.unwrap())
            .collect()
            .await;
        assert_eq!(
            frames,
            vec![
                SseFrame { id: None, data: json!({"a": 1}) },
                SseFrame { id: Some(7), data: json!({"b": 2}) },
            ]
        );
    }
```

`conn.rs` `mod tests`:

```rust
    use reqwest::header::{HeaderMap, HeaderValue};

    #[test]
    fn stream_features_parse_from_header() {
        let mut h = HeaderMap::new();
        assert_eq!(StreamFeatures::from_headers(&h), StreamFeatures::default());
        h.insert("acp-stream-features", HeaderValue::from_static("takeover"));
        assert_eq!(
            StreamFeatures::from_headers(&h),
            StreamFeatures { takeover: true, replay: false }
        );
        h.insert("acp-stream-features", HeaderValue::from_static("takeover, replay, later-thing"));
        assert_eq!(
            StreamFeatures::from_headers(&h),
            StreamFeatures { takeover: true, replay: true }
        );
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
    async fn subscribe_with_sends_takeover_and_last_event_id_and_drops_duplicate_ids() {
        let (url, server) = one_shot_sse_server(
            "acp-stream-features: takeover, replay\r\nacp-replay: complete\r\n",
            "id: 42\ndata: {\"n\":42}\n\nid: 43\ndata: {\"n\":43}\n\n",
        )
        .await;
        let mut up = Upstream::resume(&url, "s".into(), "c".into(), Recorder::disabled()).unwrap();
        up.set_last_event_id(StreamKey::Conn, 42);
        let out = up
            .subscribe_with(None, SubscribeOpts { takeover: true, resume: true })
            .await
            .unwrap();
        let head = server.await.unwrap();
        assert!(head.contains("acp-takeover: 1"), "{head}");
        assert!(head.contains("last-event-id: 42"), "{head}");
        assert!(out.took_over);
        assert_eq!(out.replay, ReplayOutcome::Complete);
        assert_eq!(
            up.stream_features(),
            Some(StreamFeatures { takeover: true, replay: true })
        );
        let first = up.next(Duration::from_secs(1)).await.unwrap().unwrap();
        assert_eq!(
            first.frame(),
            Some(&serde_json::json!({"n": 43})),
            "id 42 was already seen and must be dropped"
        );
        assert_eq!(up.last_event_id(&StreamKey::Conn), Some(43));
    }

    #[tokio::test]
    async fn plain_subscribe_against_unpatched_server_reports_no_features() {
        let (url, server) = one_shot_sse_server("", "data: {\"n\":1}\n\n").await;
        let mut up = Upstream::resume(&url, "s".into(), "c".into(), Recorder::disabled()).unwrap();
        let out = up
            .subscribe_with(None, SubscribeOpts { takeover: true, resume: true })
            .await
            .unwrap();
        let _ = server.await.unwrap();
        assert!(!out.took_over);
        assert_eq!(out.replay, ReplayOutcome::Unsupported);
        assert_eq!(up.stream_features(), Some(StreamFeatures::default()));
    }
```

Run `nix develop -c cargo test -p honk-upstream --lib` (short). Expected: compile
errors for the missing items.

**Step 2: Implement.**

`sse.rs`:

```rust
/// One SSE event: its JSON payload and its `id:` field, if the server sent one.
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
    byte_stream
        .eventsource()
        .filter_map(|event_res| async move {
            match event_res {
                Ok(event) => {
                    let data = event.data.trim();
                    if data.is_empty() {
                        return None;
                    }
                    Some(
                        serde_json::from_str::<Value>(data)
                            .with_context(|| format!("failed to parse SSE data as JSON: {data}"))
                            .map(|data| SseFrame { id: event.id.trim().parse().ok(), data }),
                    )
                }
                Err(err) => Some(Err(anyhow::anyhow!("SSE stream error: {err}"))),
            }
        })
        .boxed()
}
```

Then make `sse_to_json_stream` a one-liner:
`sse_to_frames(byte_stream).map(|r| r.map(|f| f.data)).boxed()`.

`conn.rs`, new public types (after `rpc_id_matches`):

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
}

/// Per-call subscribe options. Safe to send to unpatched goose: it ignores both headers.
#[derive(Debug, Clone, Copy, Default)]
pub struct SubscribeOpts {
    /// Send `Acp-Takeover: 1`: evict a stuck subscriber instead of retrying 409.
    pub takeover: bool,
    /// Send `Last-Event-ID` (last id seen on this stream, or 0).
    pub resume: bool,
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
    /// Only an explicit `complete` from the server lets it skip.
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

`Upstream`:
- Add the fields `features: Option<StreamFeatures>` and
  `last_event_ids: Arc<std::sync::Mutex<HashMap<StreamKey, u64>>>`. Initialize them
  to `None` and an empty map in both constructors.
- Add these accessors:

```rust
    /// Features advertised by the last GET /acp response (None before the first GET).
    pub fn stream_features(&self) -> Option<StreamFeatures> {
        self.features
    }

    /// Highest SSE event id the reader task received on `key`.
    pub fn last_event_id(&self, key: &StreamKey) -> Option<u64> {
        self.last_event_ids.lock().expect("last_event_ids poisoned").get(key).copied()
    }

    /// Carry an id across `resume()` (or from honkd's persisted state).
    pub fn set_last_event_id(&mut self, key: StreamKey, id: u64) {
        self.last_event_ids.lock().expect("last_event_ids poisoned").insert(key, id);
    }
```

- Turn the body of `subscribe` (`:394-499`) into `pub async fn subscribe_with(&mut self,
  session: Option<&str>, opts: SubscribeOpts) -> anyhow::Result<SubscribeOutcome>`.
  Then: `pub async fn subscribe(&mut self, session) -> Result<Duration> {
  Ok(self.subscribe_with(session, SubscribeOpts::default()).await?.conflict_window) }`.
- Changes inside `subscribe_with`:
  - After the `acp-session-id` header, add:
    ```rust
    if opts.takeover { req = req.header("acp-takeover", "1"); }
    if opts.resume {
        let last = self.last_event_id(&stream_key).unwrap_or(0);
        req = req.header("last-event-id", last.to_string());
    }
    ```
  - Right after `send()`, set
    `let features = StreamFeatures::from_headers(res.headers()); self.features = Some(features);`.
    This happens on 409 too.
  - On success, compute `replay = ReplayOutcome::from_headers(opts.resume, res.headers())`
    and `took_over = opts.takeover && features.takeover`.
  - Keep the existing `subscribed to … (409 window: …)` marker text byte-identical,
    because old probes and fixtures read it. Add a **second** marker only when
    `opts.takeover || opts.resume`:
    `format!("stream options: took_over={took_over} replay={replay:?} features={features:?}")`.
  - Use `crate::sse::sse_to_frames`. In the spawned reader, before
    `rec.record_in`, add:
    ```rust
    if let Some(id) = id {
        let mut ids = last_ids.lock().expect("last_event_ids poisoned");
        if ids.get(&key).is_some_and(|last| id <= *last) {
            drop(ids);
            rec.record_marker(&s_name, format!("dropped duplicate event id {id}"));
            continue;
        }
        ids.insert(key.clone(), id);
    }
    ```
    `last_ids` is `self.last_event_ids.clone()`, moved into the task.
  - Return `Ok(SubscribeOutcome { conflict_window, took_over, replay, features })`.

`lib.rs` re-exports:
- `pub use conn::{Event, ReplayOutcome, StreamFeatures, StreamKey, SubscribeOpts, SubscribeOutcome, Upstream, rpc_id_matches};`
- `pub use sse::{SseFrame, sse_to_frames, sse_to_json_stream};`

**Step 3: Verify.**
```bash
nix develop -c cargo fmt --all --check
nix develop -c cargo clippy --workspace --all-targets -- -D warnings
nix develop -c cargo test --workspace
```
Expected: green, with the probes still `#[ignore]`d. `p2_disconnect` and friends
compile unchanged.

**Step 4: Commit.**
```bash
git add crates/honk-upstream/src/sse.rs crates/honk-upstream/src/conn.rs crates/honk-upstream/src/lib.rs
git commit -m "upstream: stream takeover, SSE event ids and Last-Event-ID resume, gated on Acp-Stream-Features"
```

---

### Task 8: Release `patched-1.54.0.3` (agent stops after this task)

**Step 1: Pre-push checks.**
```bash
git -C "$FORK_WT" status --porcelain                                   # expect: empty
git -C "$FORK_WT" fetch fork
git -C "$FORK_WT" merge-base --is-ancestor fork/patched/v1.54.0 HEAD && echo fast-forward
git -C "$FORK_WT" log --oneline fork/patched/v1.54.0..HEAD
```

The log should show exactly 5 commits:
1. `developer/shell: …`
2. `vendor: …`
3. `acp-http: Acp-Takeover …`
4. `acp-http: SSE event ids …`
5. `ci: …`

Then check the diff stat:
```bash
git -C "$FORK_WT" diff --stat fork/patched/v1.54.0..HEAD
```
Expected: only `shell.rs`, `Cargo.toml`, `Cargo.lock`, `vendor/agent-client-protocol-http/**`
and `patched-release.yml`.

**Step 2: Push the branch** with an explicit refspec, fast-forward only, no force, and
never to `origin`:
```bash
git -C "$FORK_WT" push fork HEAD:refs/heads/patched/v1.54.0
```

**Step 3: Tag and push the tag.**
```bash
git -C "$FORK_WT" tag -a patched-1.54.0.3 -m "goose v1.54.0 + local patches (headless cache TTL, Vertex host override, GCP auth scopes, shell process-group kill, ACP SSE takeover and replay)"
git -C "$FORK_WT" push fork refs/tags/patched-1.54.0.3
```

**Step 4: Watch the run** (about 30 min).
```bash
RUN=$(gh run list --repo johnnymo87/goose --workflow patched-release.yml --branch patched-1.54.0.3 -L1 --json databaseId -q '.[0].databaseId')
gh run view "$RUN" --repo johnnymo87/goose --json status,conclusion,jobs -q '.status, .conclusion, (.jobs[] | .name + ": " + .status + " " + .conclusion)'
```

- Poll that `gh run view` every few minutes. Keep each sleep as its own tool call.
- Alternatively, run `gh run watch "$RUN" --repo johnnymo87/goose --exit-status` with
  the long-command pattern (log `gp-release-watch`).
- If the turn will end before the run finishes, schedule a self-wake first. Use
  `swarm_schedule`, after 35m, with a self-contained message naming run `$RUN`, tag
  `patched-1.54.0.3` and this plan's path.

Expected: `build`, `test-patches` and `release` all `success`.

If `test-patches` fails:
1. Read the log: `gh run view "$RUN" --repo johnnymo87/goose --log-failed`.
2. Fix on the branch.
3. Push, then cut **`patched-1.54.0.4`**. Never re-tag `.3`.

**Step 5: Verify the assets.**
```bash
gh release view patched-1.54.0.3 --repo johnnymo87/goose --json assets,targetCommitish -q '.targetCommitish, .assets[].name'
```
Expected:
- The target is the pushed `HEAD` SHA.
- The assets are `goose-aarch64-unknown-linux-gnu.tar.{gz,bz2}`,
  `goose-x86_64-unknown-linux-gnu.tar.{gz,bz2}` and `SHA256SUMS`.

**Step 6: Fetch and package the binary for the sandbox.** This host is aarch64 NixOS
with no `/lib64` loader, so the tarball's binary needs autoPatchelf. The derivation
below mirrors the workstation repo's `pkgs/goose`:

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

**STOP.** Report the release URL and the run id. Production deploy is the author's
section below. The agent continues with Task 9, which uses only the sandbox.

---

### Task 9: honk — live probes P2e, P2f and P5a′ against the sandbox on the fork binary

**Relaunch the sandbox on the patched binary** (sandbox unit only). The sandbox DB is
shared across binaries; the schema is the same because both are 1.54.0.
```bash
systemctl --user stop honk-goose-sandbox 2>/dev/null
systemctl --user reset-failed honk-goose-sandbox 2>/dev/null
XDG_RUNTIME_DIR=/run/user/$(id -u) systemd-run --user --unit=honk-goose-sandbox --collect \
  --setenv=PATH="$PATH" --setenv=HOME="$HOME" \
  --setenv=GOOSE_BIN="$HOME/.local/state/honk-goose-bin/patched-1.54.0.3/pkg/bin/goose" \
  "$PWD/scripts/goose-sandbox.sh"
HONK_EXPECT_GOOSE_BIN="$HOME/.local/state/honk-goose-bin/patched-1.54.0.3/pkg/bin/goose" scripts/goose-sandbox-check.sh
```
Expected: `sandbox OK`, and `goose exe:` shows the `…-goose-1.54.0-patched.3/bin/goose`
store path.

**Files:**
- Create: `crates/honk-upstream/tests/common/proxy.rs`. Move `TcpProxy` and
  `wait_for_freeze` here from `tests/p2_disconnect.rs:12-116`. The only change is
  visibility: make `TcpProxy` and its `port` field `pub`, since they now cross a
  module boundary.
- Modify: `crates/honk-upstream/tests/common/mod.rs`. Add `pub mod proxy;`, the
  fixture-tag parameter, `require_patched`, and the frame predicates.
- Modify: `crates/honk-upstream/tests/p2_disconnect.rs`. Delete the moved code and add
  `use common::proxy::TcpProxy;`.
- Modify: `crates/honk-upstream/tests/p5_control.rs`. Delete the local `is_run_end` and
  `is_agent_chunk` (`:32-43`, `:64-69`); they now come from `common`.
- Create: `crates/honk-upstream/tests/p2_patched.rs` (P2e, P2f).
- Create: `crates/honk-upstream/tests/p5_patched.rs` (P5a′).
- Fixtures: `fixtures/goose-1.54.0-patched.3/{p2e,p2f,p5a_prime}/<ts>.jsonl`.

**Step 1: Refactor `common/mod.rs`. Behaviour is unchanged for old probes.**
- Add the constants `pub const FIXTURES_PATCHED_2: &str = "goose-1.54.0-patched.2";` and
  `pub const FIXTURES_PATCHED_3: &str = "goose-1.54.0-patched.3";`.
- Rename the body of `setup_session_with_mode` to
  `pub async fn setup_session_at(url: &str, fixtures: &str, probe_name: &str, mode: &str)`.
  - It uses `format!("fixtures/{fixtures}/{probe_name}/{ts}.jsonl")`.
  - It takes `url` instead of reading `HONK_GOOSE_URL`.
  - It still returns `Ok(None)` when `HONK_GOOSE_SECRET` is unset.
- Add `pub fn goose_url() -> String`, the existing env-or-default logic.
- `setup_session_with_mode(probe, mode)` becomes
  `setup_session_at(&goose_url(), FIXTURES_PATCHED_2, probe, mode)`.
- Move `is_run_end` and `is_agent_chunk` in as `pub fn`, and add:

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
        features.takeover && features.replay,
        "sandbox goose does not advertise `Acp-Stream-Features: takeover, replay` (got {features:?}); \
         relaunch honk-goose-sandbox with GOOSE_BIN pointing at the patched-1.54.0.3 build"
    );
    Ok(())
}
```

Run `nix develop -c cargo test -p honk-upstream --tests` (short). Expected: it
compiles; the probes are ignored.

**Step 2: Write `tests/p2_patched.rs`.** Shared harness first:

```rust
mod common;
use common::proxy::TcpProxy;
use common::*;
use honk_upstream::{ReplayOutcome, SubscribeOpts};

const STREAMING_PROMPT: &str =
    "Write a 60-line markdown document about tides, including a 5-row table, then reply FINISHED.";

struct HalfOpen {
    ts: TestSession,
    proxy: TcpProxy,
    prompt_id: Value,
    pre_frames: Vec<Value>,
    last_ids: Vec<(StreamKey, u64)>,
}

/// P2d's setup: everything through a proxy, streaming turn, freeze after 20 chunks.
async fn freeze_mid_stream(probe: &str) -> anyhow::Result<Option<HalfOpen>> {
    let proxy = TcpProxy::start(4180).await?;
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
    let mut chunks = 0;
    while chunks < 20 {
        match ts.upstream.next(Duration::from_secs(90)).await? {
            Some(Event::Frame(_, frame)) => {
                if is_agent_chunk(&frame) {
                    chunks += 1;
                }
                pre_frames.push(frame);
            }
            other => anyhow::bail!("stream ended before 20 chunks: {other:?}"),
        }
    }
    proxy.freeze();
    ts.upstream.hard_drop();
    // Frames the reader task already parsed are still ours; count them.
    while let Some(Event::Frame(_, frame)) = ts.upstream.next(Duration::ZERO).await? {
        pre_frames.push(frame);
    }
    let last_ids = [StreamKey::Conn, StreamKey::Session(ts.sid.clone())]
        .into_iter()
        .filter_map(|key| ts.upstream.last_event_id(&key).map(|id| (key, id)))
        .collect();
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
        let Some(Event::Frame(_, frame)) = up.next(deadline - Instant::now()).await? else { break };
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
```

Then the two probes:

```rust
/// P2e: half-open subscriber + takeover. Question: is the 409 window gone?
#[tokio::test]
#[ignore]
async fn p2e_half_open_takeover() -> anyhow::Result<()> {
    let Some(h) = freeze_mid_stream("p2e").await? else { return Ok(()) };
    let mut resumed = Upstream::resume(&goose_url(), h.ts.secret.clone(), h.ts.conn_id.clone(), h.ts.rec.clone())?;
    resumed.set_subscribe_deadline(Duration::from_secs(5));
    let opts = SubscribeOpts { takeover: true, resume: false };
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

/// P2f: half-open + takeover + Last-Event-ID. Question: is the at-most-once loss gone?
#[tokio::test]
#[ignore]
async fn p2f_half_open_takeover_with_replay() -> anyhow::Result<()> {
    let Some(h) = freeze_mid_stream("p2f").await? else { return Ok(()) };
    let mut resumed = Upstream::resume(&goose_url(), h.ts.secret.clone(), h.ts.conn_id.clone(), h.ts.rec.clone())?;
    for (key, id) in &h.last_ids {
        resumed.set_last_event_id(key.clone(), *id);
    }
    let opts = SubscribeOpts { takeover: true, resume: true };
    let conn = resumed.subscribe_with(None, opts).await?;
    let sess = resumed.subscribe_with(Some(&h.ts.sid), opts).await?;
    let tail = read_to_turn_end(&mut resumed, &h.prompt_id, Duration::from_secs(180)).await?;
    let export = export_session(&mut resumed, &h.ts.sid).await?;
    let streamed = format!("{}{}", streamed_text(&h.pre_frames), streamed_text(&tail.frames));
    let exported = extract_all_assistant_text(&export);
    let missing = exported.chars().count().saturating_sub(streamed.chars().count());
    h.ts.rec.record_marker("probe", json!({
        "probe": "p2f", "last_ids": h.last_ids.iter().map(|(k, id)| (k.stream_record_name(), *id)).collect::<Vec<_>>(),
        "conn_replay": format!("{:?}", conn.replay), "session_replay": format!("{:?}", sess.replay),
        "prompt_reply": tail.prompt_reply, "run_end": tail.run_end,
        "streamed_chars": streamed.chars().count(), "exported_chars": exported.chars().count(),
        "chars_missing": missing,
    }).to_string());
    assert_eq!(conn.replay, ReplayOutcome::Complete);
    assert_eq!(sess.replay, ReplayOutcome::Complete);
    assert!(tail.prompt_reply, "the session/prompt reply must be delivered");
    assert!(tail.run_end, "the run-end session_info_update must be delivered");
    assert_eq!(missing, 0, "streamed text must cover the exported assistant text");
    assert!(is_subsequence(&streamed, &exported));
    drop(h.proxy);
    Ok(())
}
```

**Step 3: Write `tests/p5_patched.rs` (P5a′).** It reuses P5a's shape
(`p5_control.rs:211-400`). The command has two backgrounded descendants:

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

    // Wait until both sleeps exist.
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
        if let Some(Event::Frame(_, f)) = ts.upstream.next(Duration::from_millis(200)).await?
            && f.get("id").is_some_and(|id| rpc_id_matches(&prompt_id, id))
        {
            stop_reason = f.pointer("/result/stopReason").and_then(|v| v.as_str()).map(str::to_owned);
            break;
        }
    }
    let cancel_ms = t_cancel.elapsed().as_millis();

    // Allow init to reap; then nothing may remain.
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

**Step 4: Run the probes.** Each takes 1-4 minutes. Run each as a user unit. The
secret is read inside the payload so it never lands in unit properties:
```bash
export XDG_RUNTIME_DIR=/run/user/$(id -u)
systemd-run --user --unit="honk-probe-$RANDOM" --collect --working-directory="$PWD" \
  --setenv=PATH="$PATH" --setenv=HOME="$HOME" --expand-environment=no \
  bash -c 'HONK_GOOSE_SECRET=$(cat "$HOME/.local/state/honk-sandbox/secret") nix develop -c cargo test -p honk-upstream --test p2_patched -- --ignored --nocapture --test-threads=1 > /tmp/opencode/honk-p2-patched.log 2>&1; echo "exit=$?" >> /tmp/opencode/honk-p2-patched.log'
```
Then do the same with `--test p5_patched` and log `honk-p5-patched`. Run each probe
twice (the M0 convention). Expected:
- **P2e:** 409 window 0 on both streams, reattach under 100 ms, `took_over` true.
  Frames may be lost; record them.
- **P2f:** `Complete` on both streams, prompt reply and run-end delivered,
  `chars_missing == 0`.
- **P5a′:** `stopReason: cancelled` and no survivors. Compare `fixtures/goose-1.54.0-patched.2/p5a_conn_a`,
  where the orphan survived.

If P2f shows `Gap`, the ring was too small for the frozen window, or `last_ids` was
empty. Record the numbers before changing anything.

**Step 5: Restore the sandbox to the production binary** so other agents' probes
measure what they expect: relaunch without `--setenv=GOOSE_BIN`, then run
`scripts/goose-sandbox-check.sh`.

**Step 6: Commit.** Read every new fixture diff by eye for secrets, home paths, the
GCP project id and org names. Then:
```bash
git add crates/honk-upstream/tests/common/mod.rs crates/honk-upstream/tests/common/proxy.rs \
  crates/honk-upstream/tests/p2_disconnect.rs crates/honk-upstream/tests/p5_control.rs \
  crates/honk-upstream/tests/p2_patched.rs crates/honk-upstream/tests/p5_patched.rs \
  fixtures/goose-1.54.0-patched.3
git commit -m "probe(P2e,P2f,P5a'): takeover, replay and process-group kill on patched-1.54.0.3"
```
Push, then confirm CI is green: `gh run watch <id> --exit-status`, through the
long-command pattern if needed.

---

### Task 10: honk — record results and the capability rule

**Files:**
- Modify: `docs/spike/goose-patches.md`
  - Status line `:3`: "Built: patches 2, 3 and 5 ship in fork release `patched-1.54.0.3`;
    see Results".
  - Append a section `## Results: patched-1.54.0.3` with one row per probe (P2e, P2f,
    P5a′): question, verdict, numbers, fixture path, matching the m0-findings table
    style.
  - Note the owner-token deviation from §2's pump sketch, and its reason.
- Modify: `docs/plans/2026-10-10-honk-design.md` §6, which holds the reattach rules.
  Add the capability table from the section below, verbatim.

**Commit:** `docs: goose patch results and stream-feature gating`.

---

## Capability and feature detection (how honkd runs against both builds)

**The signal.** Patched goose sends `Acp-Stream-Features: takeover, replay` on every
`GET /acp` response, `200` and `409` alike. Unpatched goose sends nothing. The signal
comes from the crate, which is where the behaviour lives:
- It costs no extra request. honkd learns it from the GET it makes anyway.
- It does not depend on version strings. `agentInfo.version` is `1.54.0` in both
  builds and is useless here; log it for diagnostics only.

**When honkd learns it.**
- `Upstream::stream_features()` is filled in by the first subscribe on each goose
  process.
- On any `404` (goose restarted, A13), honkd re-initializes and **forgets** the
  features. A restart may have swapped the binary in either direction, so honkd
  re-learns on the next GET.

**Sending the headers is always safe. Trusting the result is not.**
- Unpatched goose ignores `Acp-Takeover` (it answers 409 as today) and
  `Last-Event-ID` (it sends no replay).
- So honkd may always send both. It must gate behaviour **only** on positive
  signals:
  - `took_over` (the server advertised takeover and was asked).
  - `ReplayOutcome::Complete` (the server answered `Acp-Replay: complete`).
- The absence of a gap signal never means "no loss".

| goose advertises | On a dead stream (no bytes for > 20 s; goose keeps alive every 15 s) | After reattach |
|---|---|---|
| nothing (unpatched, today's production) | 409 retry with backoff, never DELETE (A2) | Always reconcile (A3, A5) |
| `takeover` | `subscribe_with{takeover}`: immediate, no backoff | Always reconcile |
| `takeover, replay` | `subscribe_with{takeover, resume}` with the persisted last event id per stream | Reconcile **only** if `replay.needs_reconcile()` (`Gap`/`Unsupported`), or after a 404 |

**honkd persistence that M1 needs:**
- Store the last processed event id per `(connection id, stream)` next to the
  connection ids (A15). Advance it after processing a frame, not when the frame is
  received.
- Persisted pending permissions (A8) stay required. Replay only helps while the
  connection lives; a goose restart still loses everything in memory.

**Takeover is for honkd's own dead streams only.** honkd owns its connection ids. Two
honkd instances must never share one, because takeover makes the last subscriber win
silently.

---

## Production deploy (author only — the agent must not run any step here)

Prerequisite: Tasks 8 and 9 are done, and P2e, P2f and P5a′ match their expectations.

1. **Bump the pin.** In the workstation repo, edit `pkgs/goose/default.nix`:
   - `version = "1.54.0-patched.3";` and `tag = "patched-1.54.0.3";`.
   - Replace both `hash` values. For each asset run
     `nix store prefetch-file --json https://github.com/johnnymo87/goose/releases/download/patched-1.54.0.3/<asset> | jq -r .hash`.
   - Extend the "THE PATCHES" comment:
     - 4. developer shell kills the whole process group on cancel, timeout or drop.
     - 5. Vendored `agent-client-protocol-http`: `Acp-Takeover`.
     - 6. Vendored `agent-client-protocol-http`: SSE ids, a replay ring and
       `Last-Event-ID`.
   - Note that 5 and 6 are inert unless a client sends the headers, apart from the
     added `id:` lines.
2. **Rebuild** with the cloudbox command from the workstation repo's `AGENTS.md` host
   table (the NixOS rebuild plus the home-manager switch). `goose-serve` sets
   `X-RestartIfChanged=false`, so the rebuild does **not** restart it.
3. **Pick a quiet window.** No pigeon goose run should be in flight.
4. **Back up the DB.** The backup is consistent even while goose is running:
   ```bash
   sqlite3 ~/.local/share/goose/sessions/sessions.db \
     ".backup '$HOME/.local/share/goose/sessions/sessions.db.pre-patched-1.54.0.3'"
   ```
5. **Restart:** `sudo systemctl restart goose-serve`.
   - Confirm with `systemctl show goose-serve -p ExecStart`: it must reference the
     `goose-1.54.0-patched.3` store path.
   - Restart `pigeon-daemon` only if the configuration comment says the token changed.
6. **Re-run pigeon's goose probes** (goose-launch, goose-runner, goose-id-split) as
   pigeon's docs describe. All must pass.
7. **Rollback:** revert the pin commit, rebuild, and restart `goose-serve`. The schema
   is unchanged (both builds are 1.54.0), so the DB backup is insurance and not part
   of rollback.

---

## Upstreaming later (author decision: only after living with the patches)

| Patch | Upstream | Shape | Notes |
|---|---|---|---|
| 1. shell process-group kill | goose (`block/goose`), `crates/goose/src/agents/platform_extensions/developer/shell.rs` | One small PR with the 4 tests | There is precedent: `subprocess.rs` already uses `process_group(0)` for MCP children. Call out the interactive-CLI TTY caveat (Task 2) and the drop-guard rationale (T5 abort path). |
| 2. takeover | `agent-client-protocol-http` (`agentclientprotocol/rust-sdk`, `src/agent-client-protocol-http`) | PR 1 | It is generic transport, nothing goose-specific. The header names need the ACP maintainers' blessing. The owner-token lease is self-contained in `connection.rs`. |
| 3. replay ring + `Last-Event-ID` | same crate | PR 2, depends on PR 1 | `Last-Event-ID` is standard SSE, so browser `EventSource` clients benefit for free. Propose making the ring caps `ServerOptions` fields upstream; the fork hard-codes them because goose builds `ServerOptions` with a struct literal. |

Once the crate releases equivalents and goose depends on that release, delete
`vendor/agent-client-protocol-http`, its `members` entry and its `[patch]` line, and
drop the CI graph check. The honk side does not change, because it keys on
`Acp-Stream-Features`. If upstream renames the headers, update `StreamFeatures` and
the probe expectations together.

**Rebase checklist** (moving the fork to goose > 1.54.0):
- `grep agent-client-protocol-http Cargo.toml`. If the requirement left `2.x`, the
  patch no longer applies.
- Re-vendor the new upstream version and replay the two `acp-http` commits onto it.
- The CI graph check fails loudly if this is forgotten.
