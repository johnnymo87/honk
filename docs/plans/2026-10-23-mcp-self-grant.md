# goose agents turn MCP servers on and off for themselves

Bead: `honk-9qf.15.22`. Related: `honk-9qf.15.18` (pigeon/Telegram `/mcp` for goose).
Earlier work: `docs/plans/2026-10-19-mcp-toggle.md` (honk#18, honk#20), honk#23 (swarm MCP).

## Ask

Jonathan, 2026-10-23, relayed by stall-watch:

1. goose agents can turn MCP servers on and off for **themselves**, the way opencode
   agents use `oc-mcp-enable`. This includes the `!` servers ("also the ! ones").
2. Scope: "all all all, i want this to be a general capability". Every goose
   session: honk chat, pigeon/Telegram `/launch`, stall-watch coordinators.

## Facts this design rests on (verified 2026-10-23 at honk `7388394`)

- **Where an agent could get the tools.** honk#23's `honk swarm-mcp` is a per-session
  stdio MCP server. honkd builds its catalog entry with `--session <sid>` baked in
  (`catalog.rs:241-277`), and the server checks goose's `AGENT_SESSION_ID` /
  `_meta["agent-session-id"]` against it, which refuses subagents
  (`swarm_mcp.rs:46-62`). It already receives `--honk-url` and `--honk-secret-file`.
- **Who has it today.**
  - `honk chat --new` enables `swarm` right after `session/new` (`chat/driver.rs:645-713`).
  - stall-watch enables it on every coordinator wake (all 24 programs have
    `swarm_tools = true`; eng-agent-platform `stallwatch/honk_transport.py:987-1035`).
  - pigeon `/launch --backend goose` does **not**: a plain `session/new` through honkd
    (pigeon `goose/acp-client.ts:830-843`).
  - honkd itself never enables it.
- **pigeon's exact-extensions check** (`acp-client.ts:859-890`) looks only at goose's
  own `_meta.extensionResults` in the `session/new` reply. honkd's `_meta.honk.mcp`
  launch path runs after goose replies and does not touch that array, so a server
  honkd adds at `session/new` does not trip it.
- **The busy guard.** honkd refuses `_honk/mcp/enable|disable` while
  `turn.run`/`turn.inflight` is set (`session.rs:3065-3088`). A tool call always runs
  inside a turn, so a self-grant through the existing route would always be refused.
  The 10-19 plan's reason: goose's `add_extension_inner` holds the agent's container
  mutex across spawn and initialize (`agent.rs:1628-1631`), and every tool dispatch
  briefly takes that lock (`agent.rs:1365`). So an `add` mid-turn delays that
  turn's next tool dispatch by up to `GOOSE_EXTENSION_STARTUP_TIMEOUT` (25 s, patch G1).
  It does not deadlock: dispatch clones the container and drops the lock before
  calling the tool. Tool calls hold an `ExtensionLease` snapshot, and the lease is
  rebuilt before each model call (`agent.rs:2972-2983`), so a new server's tools are
  usable **later in the same turn**.
- **Auth.** honkd's `/acp` needs the shared secret (`downstream.rs:49-94`). Any holder
  can change any session's servers, and honkd cannot tell callers apart. This is
  the same as opencode's loopback front door, which `oc-mcp-enable` uses with no
  auth at all. The 10-19 plan already accepted it: `confirm` prevents accidents,
  not misuse.
- **A viewer is the wrong client for this.** `_honk/mcp/*` on `/acp` auto-attaches
  the caller as a live viewer of the session, which streams frames and replays the
  transcript to it. A tool call doesn't need either.
- **stall-watch's extension guard** (eng-agent-platform `honk_transport.py:915-969`
  live, `:1063-1094` DB) runs at the start of each coordinator wake. `allowed_keys` is
  `{developer, skills}` plus `swarm` (with the `honk-mcp:swarm` marker). Anything
  else is **removed silently**, or fails the wake if removal fails (3 failures in a
  row alert). So a coordinator's self-granted `slack-ro` would survive its current
  turn and then disappear at the next wake. The allowlist is hardcoded; no config
  knob.

## Design

Reviewed by the oracle on 2026-10-23 before implementation. goose line numbers are at
tag `patched-1.54.0.10`.

### 1. honkd: a self-grant route outside the viewer protocol

New flag `--mcp-self-grant` (needs `--mcp-catalog` or swarm). Without it the routes
answer 404, so turning the capability off is a flag removal and a restart, with no
client change.

`POST /mcp/list`, `POST /mcp/enable`, `POST /mcp/disable` on honkd's listener.
- Auth: header `x-secret-key` only (no `?token=`), constant-time compare shared with
  `/acp` through one helper. Missing or wrong secret → 401.
- Body `{sessionId, names?, confirm?, reason?}`. Response: the `_honk/mcp/*` result
  shape (`results`, `servers`, `other`, `enabled`, `summary`), or
  `{error:{code,message}}` with the same codes and messages.
- **`reason` is required when any name needs confirmation** (not on
  `--mcp-no-confirm`): `-32602 "'X' can write; give a reason ..."`. An agent typing
  `!` proves nothing, so the useful part is a record: the reason goes to the honkd log
  (info: session, op, names, reason), to the `_honk/mcp_changed` notice, and to the
  tool result.
- **Deadline in the handler: 60 s total**, covering queue time. On timeout:
  `-32603 "timed out; the change may still land; call mcp_list"`.

It sends `SessionMsg::McpSelfOp` to the session actor (activating the session if
needed; queued like `McpLaunch` while `Reattaching`/`Rebuilding`). It differs from
`McpOp` in exactly these ways:
- **No busy refusal**: the session's own turn is running by definition. It still
  refuses while another `/mcp` change is in progress (`mcp_op_running`).
- **`origin: "agent"`** on `_honk/mcp_changed` to every honk-aware viewer, plus
  `reason` when given. New honk chat shows `mcp: enabled slack (by the agent: <reason>)`.
  Old honk chat only prints notices for `origin=="other"`, so it shows nothing; the
  tool call in the transcript is the visibility that always works.
- **No viewer**: the reply goes to the HTTP handler.

Everything else is shared with `McpOp`.

Mid-turn safety (oracle-checked): no deadlock. goose runs `extensions/add` as a
spawned custom request (`dispatch.rs:479`), the in-flight swarm tool call holds no
lock, and `add_extension_inner`'s container lock (`agent.rs:1628`) only delays other
tool calls in the same step for up to 25 s. Enabling `swarm` itself short-circuits
as "already on" (`ops.rs:371`), so the server never restarts itself. Known and
accepted: if a turn is cancelled mid-add, goose can finish the add after honkd has
already timed out; and `extension_data` read-modify-write can race with
rolling-memory/todo writes in that window. Each grant costs one prompt-cache miss.

### 2. honk swarm-mcp: three new tools

On the same per-session server, so goose names them `swarm__mcp_list`,
`swarm__mcp_enable`, `swarm__mcp_disable`.

- `mcp_list {}`.
- `mcp_enable {names: ["slack-ro", "atlassian!"], reason?}`: trailing `!` confirms
  that name (same grammar as `/mcp enable`); `reason` is required for `!` names.
- `mcp_disable {names}`: trailing `!` stripped.

- Target is always `--session` from argv, after the existing caller-identity check:
  a session can only change its own servers, a subagent none.
- Refused in the tool: `mcp_disable swarm`, and enabling anything whose exclusive
  sibling is `swarm`.
- URL: replace the path of `--honk-url` (`http://127.0.0.1:P/acp`) with `/mcp/<op>`.
- Per-request timeout 70 s (the client default is 15 s). swarm-mcp serves one
  request at a time, so a parallel `swarm_read` waits; acceptable.
- 404 → "this honkd has no self-grant (too old, or --mcp-self-grant is off)".
- Results: what changed, that the new tools are usable from the next step, the
  reason. Errors: honkd's message verbatim.
- Tool descriptions carry the discipline: prefer `slack-ro`; give a reason for a `!`
  server that says what you will do with it; on failure stop and report; never call
  the service's API with environment tokens, and never honkd/goose on loopback.

### 3. Every new session gets the tools: `--swarm-default`

New honkd flag (needs `--swarm-mcp-bin`). On `session/new`, honkd runs a **separate**
swarm launch (not merged into the client's `_meta.honk.mcp` names, so a client's
refused `--mcp atlassian` cannot fail swarm and vice versa), unless the client sends
`_meta.honk.swarm: false` (a sibling key: an old honkd ignores it). The outcome is
reported in `_meta.honk.swarm` (`{enabled:true}` / `{error}` / `{pending:true}`); no
`_meta.honk.mcp` is created for clients that didn't send one. The swarm launch is
capped at 10 s on the reply path (stall-watch's `session/new` timeout is 30 s);
past that the reply says pending and the enable finishes in the background. It never
fails `session/new`.

| Client | After |
|---|---|
| `honk chat --new` | honkd turns swarm on; honk chat keeps its own enable (a no-op, covers deploy skew). `--no-swarm` sends `_meta.honk.swarm:false`. |
| pigeon `/launch --backend goose` | swarm on. pigeon's exact-extensions check reads goose's `extensionResults`, which honkd's launch does not touch; this exemption should be made explicit on the pigeon side (note on `honk-9qf.15.18`). |
| stall-watch | sends `_meta.honk.swarm = tools and swarm_tools`. |

Existing sessions are unchanged on activation: a resumed session keeps its set.
Sessions not created through honkd (bare `goose run`, devbox/macOS) don't get the
tools; the skill says so.

### 4. stall-watch coordinators (eng-agent-platform PR)

- Send `_meta.honk.swarm` at `session/new` as above, so recipe-mode and
  `swarm_tools=false` coordinators stay swarm-free.
- In **tools mode only**, the allowed set becomes `{developer, skills}` ∪ the names
  `_honk/mcp/list` reports `enabled` (honkd-managed, marker checked by honkd), minus
  `swarm` unless `swarm_tools`. Both the live and the DB layer use it. If the list
  call fails, fall back to today's strict set.
- A self-granted server is kept across wakes (parity with opencode and honk chat),
  and each wake logs the honkd-managed servers it kept.

**Decision for Jonathan:** keep across wakes (default), or strip at every wake.

### 5. Skill and host config (workstation PR)

`enabling-mcp-servers`: replace "In goose, never grant yourself a server" with the
self-grant section (tools, `slack-ro` first, reason for `!`, late and narrow, off when
done, stop and report, no REST/loopback route-arounds; tools missing → human
`/mcp enable swarm` on cloudbox, otherwise say it can't be done). The human `/mcp`
table stays. `users/dev/home.cloudbox.nix`: add `--mcp-self-grant --swarm-default`,
update the comment; flake bump.

### Out of scope

- Telegram `/mcp` for goose, showing agent changes on Telegram, the pigeon
  exact-extensions comment: `honk-9qf.15.18`.
- Per-session authorization in honkd. The capability isn't new (an agent can read the
  secret and call `_honk/mcp/*`); the new tools make it an affordance, which is why
  the reason log, the notice and the kill switch exist.

## Rollout and deploy skew

Order: honk PR → eng-agent-platform PR → workstation PR (flags, skill, flake bump) →
deploy (home-manager switch, `git pull` in eng-agent-platform, honkd restart).

- The flags are off until the workstation PR, so the honk PR deploys inertly.
- New honk with old honkd: tools return the 404 message; `_meta.honk.swarm` ignored.
- Running swarm-mcp processes keep the old tool list until their session reloads
  (goose re-spawns from the stable `~/.nix-profile/bin/honk`). Sessions reload on
  honkd restart only if goose restarted; otherwise `/mcp disable swarm` +
  `/mcp enable swarm`.
- stall-watch before `--swarm-default`, otherwise a coordinator's self-grant is
  stripped at its next wake.

## Tests

- honkd `/mcp/*`: 404 without the flag; 401 without/with wrong secret, `?token=`
  rejected; shapes; `reason` required for confirm-class; succeeds while a turn runs;
  refuses while another op runs; `origin:"agent"` + reason notice; queued while
  rebuilding with the deadline covering queue time; cancel a turn mid-op then prompt
  works.
- `--swarm-default`: swarm on at `session/new` and reported in `_meta.honk.swarm`;
  `_meta.honk.swarm:false` opts out; the client's own `_meta.honk.mcp` result
  unaffected; a slow swarm launch replies pending within the cap; failure doesn't
  fail `session/new`.
- Live goose: self-enable a stub server mid-turn and call its tool in the same turn.
- swarm-mcp: tool list, `!` parsing, reason required, `disable swarm` and swarm
  sibling refused, target is the argv session, URL path replacement, 70 s timeout,
  error passthrough, 404 message.
- honk chat: `--no-swarm` sends `_meta.honk.swarm:false`; `origin:"agent"` notice.
- stall-watch: tools-mode extras from `_honk/mcp/list` kept in both layers; unmarked
  or forged-key extras stripped; list failure → strict; recipe-mode and
  `swarm_tools=false` sessions request no swarm.

## Tasks (honk PR)

1. honkd: shared secret check, `--mcp-self-grant`, `POST /mcp/*`, `McpSelfOp`, reason,
   `origin:"agent"`, deadline. Unit + integration tests.
2. honkd: `--swarm-default`, `_meta.honk.swarm`, 10 s cap. Tests.
3. honk swarm-mcp: `mcp_list/enable/disable`. Tests.
4. honk chat: `--no-swarm` → `_meta.honk.swarm:false`; render `origin:"agent"` notices.
   Docs (`docs/chat.md`, README flag list). Live goose test for mid-turn self-enable.

## Changes made during implementation review

- **The tools are hidden while self-grant is off.** honkd answers
  `GET /mcp/self-grant` (secret header; 204 when on, 404 when off). swarm-mcp probes it
  once at `tools/list` (2 s; any failure means off) and lists the `mcp_*` tools only when
  it is on. So the honk PR deploys inertly even though production honkd already runs
  swarm-mcp, and removing `--mcp-self-grant` hides the tools from every swarm server
  spawned afterwards.
- **Old self-ops are dropped.** A self-op that is still queued after its HTTP caller
  timed out is dropped rather than run late.
- **`reason` is capped** at 500 characters.
- **Agents get their own refusal hint.** An agent asking for a `!` server without
  confirming gets `call mcp_enable with "X!" and a reason ...`. Humans still get
  `/mcp enable X!`.
- **Timeouts are staggered.** Agent ops get a 55 s worker budget, inside the HTTP
  handler's 60 s deadline.
- **Replies arrive after `mcp_op_running` clears.** honkd now sends `/mcp` replies only
  after clearing the flag, so a client's immediate follow-up `/mcp` call is never
  refused as "in progress".
- **swarm-mcp is pointed at the address honkd actually bound**, not the one in
  `--listen`.
- **honk chat no longer double-enables swarm.** It skips its own enable when the
  `session/new` reply carries `_meta.honk.swarm`.
- **The production goose must stay at patched-1.54.0.8 or later.** That release added
  patch G1, the extension startup timeout. Below it, a self-granted server that hangs at
  startup blocks the session's tool calls indefinitely, not for 25 s. CI pins .3, which
  is fine for the stub servers in the tests.
