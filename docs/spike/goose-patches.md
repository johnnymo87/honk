# Goose fork patches: candidates for the ACP transport

Status: research only. Nothing below has been built.
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

## Ranking (value for honkd ÷ cost)

| Rank | Patch | Value for honkd | Cost | Where |
|---|---|---|---|---|
| 1 | 2: takeover | High: removes the indefinite 409 | S (≈100 LOC) | crate (vendored) |
| 2 | 3: replay ring | High: removes per-reattach loss; reconcile becomes rare | M (≈150 LOC) | crate (vendored) |
| 3 | 4: session/subscribe | Low: honkd already fans out | L | goose |
| 4 | 1: WS detach | ~None: honkd uses HTTP+SSE; honest version needs resume and acks | M+ | crate |

**Recommendation.** Build **2 then 3** in one vendored-crate patch series.
Write them so they can be upstreamed to `agent-client-protocol-http`, and
upstream them. Until both ship, honkd keeps its M0 design: backoff on 409 and
a full reconcile after every reattach. After 2 alone, honkd can drop the
backoff. After 2+3, honkd can make reconcile a gap- or restart-only fallback.
Optionally probe the goose-only `TCP_USER_TIMEOUT` change first. If it turns
out to clear the zero-window case too, it is a one-file stopgap that bounds
the 409 window without vendoring anything.
