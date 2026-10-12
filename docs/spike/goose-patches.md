# Goose fork patches: candidates for the ACP transport

Status: Built: patches 2, 3 and 5 ship in fork release `patched-1.54.0.3`; see
Results.
Baseline: goose `v1.54.0` (fork branch `patched/v1.54.0`; its existing local
patches don't touch the ACP transport) and `agent-client-protocol-http` 2.2.0.
Evidence: `m0-findings.md` §T5 and §P2.

## Where the code lives

The ACP HTTP/WS transport is **not goose code**. It comes from the external
crate `agent-client-protocol-http` (workspace dep, goose `Cargo.toml:26`).
Goose only builds the router (`crates/goose/src/acp/transport/mod.rs:188-196`,
`AcpHttpServer::new(|| GooseAgentConnection::new(..))`) and supplies the
per-connection agent (`crates/goose/src/acp/server.rs:2699-2713`). The ACP
server lives in `crates/goose/src/acp/`. There is no `goose-acp` crate.

| Behaviour | Site | Owner |
|---|---|---|
| A new UUID per WS upgrade, with no resume | `websocket_server.rs:21` | crate |
| WS close → `registry.remove` + `shutdown()` | `websocket_server.rs:73-76` (loop exits at `:127-139`) | crate |
| `shutdown()` aborts the agent task | `connection.rs:190-200` | crate |
| Single-subscriber mailbox and lease | `connection.rs:37-105` (`try_acquire` `:62`, lease `Drop` `:92`) | crate |
| GET on a held stream → 409 | `http_server.rs:350-361` | crate |
| SSE frames are dequeued, then yielded (no `id:`) | `http_server.rs:363-395` | crate |
| Keepalive comment every 15 s | `http_server.rs:400-404` | crate |
| DELETE → `shutdown()` | `http_server.rs:417-429` | crate |
| Dropping the run future cancels the run | `ActiveRunDropGuard`, `server.rs:258-279`; armed in `on_prompt` `:2297` | goose |
| Run registry shared across connections | `crates/goose/src/execution/active_run.rs:27` | goose |
| Every connection gets its own `GooseAcpAgent` | `server.rs:2704` (`create_agent()`) | goose |
| Plain-HTTP listener | `crates/goose-cli/src/cli.rs:1902` | goose |

**T5 mechanism:** a WS close hits `shutdown()`, which aborts the agent task.
That drops the in-flight `on_prompt` future, and `ActiveRunDropGuard::drop`
cancels the run's token. HTTP has no close hook, so its runs survive
(P2a/b/c). DELETE takes the same `shutdown()` path, which is why it cancels.

**Patching the crate means changing a dependency.** Vendor it into the fork
(for example `vendor/agent-client-protocol-http/`) and add this to the root
`Cargo.toml`:

```toml
[patch.crates-io]
agent-client-protocol-http = { path = "vendor/agent-client-protocol-http" }
```

The costs:
- `Cargo.lock` churn.
- The patch silently stops applying if upstream goose moves to a
  semver-incompatible crate version (3.x). Pin and check on every rebase.
- The fork's release CI must build with the vendored copy.

Patches 2 and 3 are generic transport fixes, so they are good candidates to
upstream to the crate. That would retire the vendoring.

## 1. Detach-tolerant WS turns (opt-in)

**Problem.** Closing the WS cancels the turn (T5). Runner-style clients can't
survive a reconnect over WS.

**Sketch.** Add `ServerOptions.ws_detach_grace: Option<Duration>`, defaulting
to `None` (today's behaviour). When it is set, `run_ws` stops calling
`remove`+`shutdown()` on socket close. The connection stays in the registry,
and a reaper calls `shutdown()` once the grace period expires.

**Resume.** WS can't resume today. `handle_ws_upgrade` always mints a new id
(`websocket_server.rs:21`) and never reads `Acp-Connection-Id` from the
request. A resume needs a new upgrade path that looks up an existing WS
connection by id. Browsers can't set headers on a WS upgrade, so the id would
go in a header *or* a query param. The upgrade then reacquires
`all_outbound`. The lease returns its receiver on `Drop` (`connection.rs:92`),
so reacquiring works mechanically.

**The honest version is bigger.**
- WS has the same at-most-once loss as P2d: frames sent into a dead socket
  are gone.
- WS has no `Last-Event-ID`, so lossless resume needs an invented sequence
  and ack scheme on top of JSON-RPC.
- It also needs the reaper, which HTTP doesn't have either: HTTP connections
  leak until DELETE.

**Test idea.** Start a fake long-running agent, close the WS, reconnect with
the id, and assert that the agent future was not aborted and that queued
frames arrive.

**What honkd could drop:** nothing. honkd uses HTTP+SSE, where turns already
survive.

**Risk:** medium. It adds new resume semantics and leaked connections, and the
connection id becomes a long-lived bearer capability.

**Size:** about 150 LOC plus a resume protocol. **Not recommended** for honk.

## 2. Subscriber takeover (fixes P2d's indefinite 409)

**Problem.** P2d: a half-open subscriber holds the stream's lease. Every
reattach then gets 409 for more than 180 s, effectively forever under
zero-window probing. DELETE would clear it, but DELETE also cancels the run.

**Site.** `http_server.rs:350-361`, plus the mailbox in `connection.rs:37-105`.

**Why "signal the old stream to stop" fails.** hyper polls the SSE body only
when it can write. The stuck stream's generator is therefore never polled, so
a cancel token inside its `select!` is never seen. The old stream can't be
asked to give the lease up. It has to be taken away.

**Sketch.**
- Move the lease out of the SSE generator into a per-subscriber pump task
  (`tokio::spawn`). The pump owns the `OutboundLease` and forwards frames into
  a small bounded channel, and that channel is the SSE body.
- Each mailbox records the current pump's `JoinHandle` or `AbortHandle`.
- On a GET that finds the lease held and asks for takeover:
  1. Abort the old pump. tokio polls the task independently of hyper, so the
     abort lands.
  2. Await the old pump's exit. The lease `Drop` returns the receiver to the
     slot.
  3. `try_acquire` again.
- The old response body sees its channel close the next time hyper polls it.
  If the socket stays wedged, hyper never polls, and that half-open socket
  lingers until TCP kills it. That is harmless.

**Opt-in and compatibility.**
- Takeover happens only with a request header `Acp-Takeover: 1`, so the
  existing 409 contract and the crate test that asserts it
  (`http_server.rs:1310`) stay unchanged.
- Takeover needs only the connection id, and the connection id already
  authorizes POST and DELETE. No new privilege.

**Remaining loss.** The old pump's channel, hyper's buffer and the kernel
buffer still lose frames. Takeover fixes availability, not delivery. Pair it
with patch 3.

**Cheaper partial alternative, goose-only, unverified.** Set `TCP_USER_TIMEOUT`
on accepted sockets at `cli.rs:1902`, through axum 0.8 `ListenerExt::tap_io`
or a custom accept loop.
- A true blackhole (unACKed data) would die within N seconds.
- In P2d's frozen-proxy case the peer keeps ACKing zero-window probes. Whether
  the timeout fires there depends on the kernel and must be probed.
- It bounds the 409 window but still loses frames.

**Test idea.**
1. GET #1, hold the `Response` and never poll its body (this stands in for
   hyper's stalled writer). Push frames.
2. GET #2 without the header → expect 409.
3. GET #2 with `Acp-Takeover: 1` → expect 200, and frames pushed afterwards
   arrive on #2.
4. Assert that the agent task was never aborted.

**What honkd could drop:** the 409 exponential-backoff loop and the
"reattach latency is gated on goose's socket detection" caveat. honkd decides
its own stream is dead (no bytes for more than about 20 s, given goose's 15 s
keepalive) and takes it over immediately. The full reconcile after reattach
stays, because frames are still lost.

**Risk:** low to medium. The pump task changes the SSE ordering and draining
paths that the crate tests carefully (`sse_buffers_burst_*`, final-frame
drain on close).

**Size:** about 80-120 LOC plus 2 tests.

**Connection binding note (per P3c):**
Live frames are bound to the `ConnectionTo<Client>` that sent `session/prompt` (`crates/goose/src/acp/server.rs:on_prompt` → `forward_agent_stream`). Takeover is strictly same-connection-id and cannot move a live run to a new connection; cross-connection would need the `session/subscribe` patch.

## 3. Redelivery: per-stream replay ring plus `Last-Event-ID`

**Problem.** Delivery is at-most-once. The frame is dequeued at
`http_server.rs:365-391` before it is written. P2d lost 213-649 chars, the
run-end `session_info_update` and the `session/prompt` reply. The prompt reply
is routed to the session stream (pending route), so it is on the same mailbox.

**Sketch.** All changes are in `connection.rs` and `http_server.rs`.

- **Sequencing.** `OutboundMailbox::push` (`:56`) stamps a per-mailbox
  monotonic `u64` and sends `(seq, String)`.
- **Ring.** On dequeue, the lease holder appends the pair to an
  `Arc<StdMutex<VecDeque>>` on the mailbox. The cap is a byte budget (about
  1 MiB) plus a frame count. The lease holder is the only writer, so there is
  no contention.
- **Emit.** SSE emits `Event::default().id(seq).data(msg)`.
- **Replay.** On GET with `Last-Event-ID: k`, first trim entries `<= k` (an
  implicit ack), then yield ring entries `> k`, then go live.
- **Gaps.** If `k` is below the ring's floor, flag the gap. Use a response
  header `Acp-Replay-Gap: 1` and/or a first SSE comment, so the client knows
  to reconcile.

**Compatibility.**
- Default-on is safe: clients ignore `id:` unless they send `Last-Event-ID`.
  Browser `EventSource` sends it automatically, and replay is the right
  behaviour for those clients.
- The connection-scoped stream gets the same treatment.
- An explicit-ack design (POST an ack, dequeue on ack) works, but it is more
  protocol for no gain over the implicit ack in `Last-Event-ID`.

**Dependency on patch 2.** Without takeover, a half-open subscriber still
blocks reattach. Replay then helps only once TCP finally kills the stale
socket, so build 3 together with 2 or after it.

**Test idea.**
1. Read events 1-3 and drop the stream. Push 4-5.
2. GET with `Last-Event-ID: 1` → expect 2, 3, 4, 5 exactly once and in order.
3. With a cap of 2 and `Last-Event-ID: 0`, expect the gap flag.
4. With 2 and 3 combined: take over a stuck GET and get lossless replay.

**What honkd could drop:**
- The "must reconcile after every reattach" rule. Reconcile runs only on a
  gap flag or after a goose restart. The registry lives in memory, so a
  restart still drops every connection and the fallback code stays.
- honkd can trust the stream's run-end and prompt reply to drive turn
  completion, and stop polling export to infer it.

**Risk:** low to medium. Memory is bounded by the cap. The subtle part is the
ordering between ring replay and live dequeue: both happen in the same
generator or pump, so do replay first, then live.

**Size:** about 120-180 LOC plus 3 tests.

## 4. `_goose/unstable/session/subscribe` (multi-viewer broadcast)

**Problem.** Session updates go only to the `ConnectionTo<Client>` of the
connection that is serving the run. Each connection has its own
`GooseAcpAgent` and `client_cx` (`server.rs:337`, `:2704`). A second viewer
sees nothing until the next export.

**Sketch.** This is goose code, with no crate patch.
- Add a server-wide `SessionBroadcast` on `AcpServer`
  (`server_factory.rs`): `session_id → Vec<ConnectionTo<Client>>`.
- Add a new `_goose/unstable/session/subscribe` / `unsubscribe` dispatch.
- Add a helper that tees every session-scoped `send_notification` to the
  subscribers. There are about 25 call sites across `server.rs`,
  `server/{dispatch,load_session,live_voice,providers}.rs` and
  `tool_call_notifier.rs`.
- Agent→client *requests* (for example permission) must still go to exactly
  one connection.

**Limits.**
- Every viewer stream is still a crate mailbox, with P2d's 409 and loss.
- Late joiners get no backfill.

**Test idea.** Use two in-process connections. A subscribes to B's session,
B prompts, and A receives the same `session/update` sequence.

**What honkd could drop:** its own fan-out, but honkd fans out already, and
does so with its own cursor and backfill semantics. Low value for honk.

**Risk:** medium. There are many call sites, the send-error semantics change
(one dead viewer must not fail the run), and the method is new protocol
surface in an `unstable` namespace.

**Size:** about 300-500 LOC plus tests.

## 5. Process group termination for developer shell (fixes orphaned child processes on cancel)

**Problem.** When an in-flight prompt is cancelled (`session/cancel`), goose triggers the agent turn's cancellation token. In `crates/goose/src/agents/platform_extensions/developer/shell.rs:611,622`, the cancellation handler calls `child.start_kill()`. On Unix, `child` is the direct `bash` process (`build_shell_command`, line 732). Calling `start_kill()` sends `SIGKILL` only to `bash`. Any descendant processes spawned by `bash` (such as `sleep`, compiler invocations, build scripts, background tasks) are NOT killed; they are orphaned, reparented to PID 1 / user systemd (P5a), and continue running in the background until completion. This bug applies equally to production goose.

**Fix.** Spawn the shell process in its own process group on Unix (using `tokio::process::Command::process_group(0)` or `command.pre_exec(|| { nix::unistd::setpgid(nix::unistd::Pid::from_raw(0), nix::unistd::Pid::from_raw(0)).map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e)) })`), and on cancellation send `SIGKILL` to the process group (`-pgid`) rather than only the parent process before awaiting child exit.

**Test idea.** Run a shell command with a subshell or command chain (e.g. `bash -c "sleep 40"`), cancel the session, and verify using `pgrep -u $UID -f "sleep 40"` that no descendant processes survive.

**What honkd could drop:** Nothing directly, but prevents resource leaks and runaway background processes on the goose server host when user turns are cancelled.

**Risk:** Low. Standard Unix process group management.

**Size:** ≈20-40 LOC in `crates/goose/src/agents/platform_extensions/developer/shell.rs`.

## Ranking (value for honkd ÷ cost)

| Rank | Patch | Value for honkd | Cost | Where |
|---|---|---|---|---|
| 1 | 2: takeover | High: removes the indefinite 409 | S (≈100 LOC) | crate (vendored) |
| 2 | 3: replay ring | High: removes per-reattach loss; reconcile becomes rare | M (≈150 LOC) | crate (vendored) |
| 3 | 5: shell process group kill | High (host hygiene): stops runaway orphaned shell processes on cancel | S (≈30 LOC) | goose |
| 4 | 4: session/subscribe | Low: honkd already fans out | L | goose |
| 5 | 1: WS detach | ~None: honkd uses HTTP+SSE; honest version needs resume and acks | M+ | crate |

**Recommendation.** Build **2 then 3** in one vendored-crate patch series.
Write them so they can be upstreamed to `agent-client-protocol-http`, and
upstream them. Until both ship, honkd keeps its M0 design: backoff on 409 and
a full reconcile after every reattach. After 2 alone, honkd can drop the
backoff. After 2+3, honkd can make reconcile a gap- or restart-only fallback.
Optionally probe the goose-only `TCP_USER_TIMEOUT` change first. If it turns
out to clear the zero-window case too, it is a one-file stopgap that bounds
the 409 window without vendoring anything.

## Results: patched-1.54.0.3

Built per `docs/plans/2026-10-11-goose-patches-plan.md`. The release ships
patches 2 (takeover), 3 (replay ring plus `Last-Event-ID`) and 5 (shell
process-group termination). The vendored crate advertises them as
`Acp-Stream-Features: takeover, replay` on every SSE `GET /acp`. Patches 1 and
4 were not built (out of scope).

The numbers below are from the pre-release runs, on a build of fork commit
`e41c1ec30` (the commit tagged `patched-1.54.0.3`), against the isolated
`honk-goose-sandbox-patched` unit on port 4181. The release-binary re-run
(plan Task 10, the autoPatchelf'd `goose-aarch64-unknown-linux-gnu` asset from
release run 38116818991) passed every probe once; only that run's fixtures are
committed.

| Probe | Question | Verdict (pre-release) | Numbers (pre-release) | Release-binary re-run | Fixture |
|---|---|---|---|---|---|
| P2e (control) | Without `Last-Event-ID`, does a takeover after the turn committed behind a frozen socket still lose frames? | PASS: takeover works and loss is still there, as expected for a control | 409 window 0 on both streams; reattach 0-1 ms; `took_over` on both; prompt reply and run end **lost**; `chars_missing > 0` (streamed 231 chars of the turn's text) | PASS (release): streamed 215 chars, prompt reply + run end lost, 409 0, reattach 0 ms | `fixtures/goose-1.54.0-patched.3/p2e/` |
| P2f | Does takeover plus `Last-Event-ID` after commit recover everything? | PASS 3× | `Acp-Replay: complete` on both streams; `replayed_frames` 191 and 350 in the two recorded runs; prompt reply and run end replayed; `chars_missing` 0 | PASS (release): `complete`, 21 frames replayed, reply + run end, 0 missing | `fixtures/goose-1.54.0-patched.3/p2f/` |
| P4b′ | Is a permission request lost between `tool_call` and the permission frame recovered by resume? | PASS 2× | Drop right after `tool_call`, resume from id 9: permission replayed, answered `allow_once`, tool ran, run finished, `Acp-Replay: complete` | PASS (release): permission replayed from id 9, tool ran | `fixtures/goose-1.54.0-patched.3/p4b_prime/` |
| P5a′ | Does cancel kill the shell's whole process group, including backgrounded children? | PASS 2× | Two backgrounded `sleep`s; `stopReason: cancelled`; zero survivors in the unit's cgroup after 3 s | PASS (release): cancelled, 0 survivors | `fixtures/goose-1.54.0-patched.3/p5a_prime/` |
| Pigeon WS qualification | Does pigeon's WS path still work on the patched binary? | PASS | `goose-launch`, `goose-runner` and `goose-id-split` probes over `ws://…:4181/acp`: all exit 0, "ALL LIVE CHECKS PASSED" | PASS (release): all three exit 0; every check PASS (deploy gate evidence) | none (pigeon probe logs only) |

**P2e had to wait for commit.** Its first attempt took over 1 ms after the
freeze, before goose had written anything into the dead socket, so nothing was
lost and the control proved nothing. The probe now waits until the turn has
committed (polling session state until the turn commits) before taking over, which guarantees frames
went into the dead socket.

**Deviation from §2: owner-token leases instead of a pump task.** §2 sketched
takeover as a per-subscriber pump task that owns the lease and is aborted on
takeover. The build keeps the key insight (a stuck stream is never polled, so
the lease must be *taken*, not handed back) but uses owner tokens instead:
- The HTTP mailbox's queue sits under a `std::sync::Mutex` together with
  `owner: Option<u64>`. A lease is `(Arc<shared>, id)`, and every dequeue
  checks `owner == Some(id)` under the lock.
- Takeover installs a new owner id synchronously. If the stuck generator is
  ever polled again, it sees the mismatch and ends. Nothing is aborted or
  awaited.

The reason: a pump dequeues ahead of hyper, so a clean client drop would lose
whatever the pump had staged. That breaks the crate's existing test
`sse_allows_one_active_subscriber_per_logical_stream`, which asserts that frames
queued before a drop reach the next subscriber. Owner tokens keep today's
dequeue-on-poll behaviour exactly.

**The WS mailbox is unchanged.** The owner-token and ring mailbox is a new
module, `http_mailbox.rs` (`HttpMailbox`/`HttpLease`), used only by
`HttpOutbound`. The original `OutboundMailbox`/`OutboundLease` in
`connection.rs` and all of `websocket_server.rs` are byte-identical to the
crate, so pigeon's WS path runs the same source. The pigeon qualification
above checks that in practice.

**Patch 5 is TERM→KILL, not a bare SIGKILL.** On cancel, timeout and drop,
goose sends SIGTERM to the command's whole process group, waits up to 2 s for
the group to empty, then SIGKILLs the group. The grace lets cleanup handlers
run (a SIGTERMed git removes `.git/index.lock`; a SIGKILLed one leaves it). The
shell patch is 2 fork commits: the second keeps the pgid across terminate's
grace period, so an abort that lands mid-grace still escalates to SIGKILL
instead of dropping the group.

**Ring memory against A14.**
- Each HTTP stream's ring holds up to `REPLAY_RING_MAX_FRAMES = 4096` frames and
  `REPLAY_RING_MAX_BYTES = 1 MiB` until a `Last-Event-ID` acks it, and a
  healthy client acks only on reattach.
- Session mailboxes, and so their rings, are never removed until the
  connection ends (`connection.rs:322-333`).
- honkd's worst case, 16 held sessions with one connection each, is about
  16 MiB plus up to 1 MiB per connection stream. A14 measured 0.5-1.7 MB RSS
  per active session, so the rings could dominate goose's per-session cost by
  up to about 10×.
- A long-lived connection that subscribes many session streams grows without
  bound. honkd therefore runs every export on a short-lived side connection
  (initialize, subscribe, export, DELETE), which frees the mailbox and ring.

**Fork-flake gaps (worth fixing in the fork later).** Building the fork
worktree needed two workarounds that the fork's `flake.nix` should carry
itself:
- The fork's `rust-overlay` input is too old for Rust 1.99.0. The build used
  `--override-input rust-overlay` pinned to honk's locked rev, without writing
  the fork's lock.
- `goose-cli`'s default features pull in llama-cpp, whose build script needs
  libclang and cmake. Both came from nixpkgs, with `LIBCLANG_PATH` and
  `BINDGEN_EXTRA_CLANG_ARGS` set by hand.
