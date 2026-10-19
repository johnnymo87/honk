# Per-session MCP servers in goose (`/mcp` in honk chat) — design and plan

Bead: `honk-9qf.15.8` (children `.1` goose fork, `.2` honkd, `.3` honk chat, `.4` workstation).

**Goal.** The author can turn the MCP servers they use in opencode (slack, slack-ro,
atlassian, pagerduty, rollbar, figma, devcycle, chrome-devtools-*, …) on and off **per goose
session** from honk chat (goose-serve :4080 → honkd :4280 → honk), and start a session
with one on (`honk chat --new --mcp`). Same definitions as opencode (one source of
truth), off by default, slack write granted deliberately, no secret ever printed or
persisted. Must not break pigeon or the stall-watch coordinator.

## Facts (verified 2026-10-19; goose fork `patched/v1.54.0` @ `7c535c9e8`)

- **opencode's MCP list is the generated `~/.config/opencode/opencode.json` `.mcp`**
  (workstation `assets/opencode/opencode.base.json:140-166` + per-host jq blocks in
  `users/dev/opencode-config.nix`, e.g. slack `:1608-1823`, datadog `:3011-3081`). There is
  no shared Nix attrset. Local entries: `{type:"local", command:[…], environment:{K: literal
  | "{file:/run/secrets/x}"}}`; secrets are only `{file:}` refs (`secretRef`, `:495-498`;
  macOS `{file:~/.config/opencode/secrets/x}`). Remote: datadog (Bearer `{file:}` header),
  notion (opencode-native OAuth). All `enabled:false`. slack-ro = the slack binary without
  the `SLACK_MCP_ADD_MESSAGE_TOOL`/`FILE_UPLOAD_TOOL` gates (`:1635-1643`). Stale runtime
  keys (`playwright*`, `atlassian-alt`) survive `mergeOpencode` (`:1192`).
- **goose session-scoped methods** (upstream v1.54.0, unpatched in the fork):
  `_goose/unstable/session/extensions/{add,remove,list}`
  (`goose-sdk-types/src/custom_requests.rs:32-48,530-541`; handlers
  `goose/src/acp/server/extensions.rs:8-37,101-120`). `add.extension` is a
  `GooseExtension` `{type:"mcp", server:{name,command,args,env:[]}, description?, timeout?,
  envKeys?}`. **Inline stdio env is rejected** on session paths (`extensions.rs:354-364`);
  `envKeys` resolve from goose-serve's own env or `secrets.yaml`, so they cannot express
  "read this file".
- add/remove **persist the agent's whole running set** to `session.extension_data`
  (`agents/agent.rs:1243-1274,1369-1388,1530-1555`), unresolved (key names only). `list`
  reads the DB. `session/load` starts only the stored set and drops servers that fail
  (`agent.rs:1279-1367`). So a set survives honkd restart (same goose connection) and
  goose-serve restart (honkd's `session/load`, `hub.rs:1454-1470`).
- Tools are rebuilt before every model call (`agent.rs:2727-2738`): a change lands at the
  next model call, even mid-turn. But `add` holds the agent's container mutex across
  spawn + initialize (`agent.rs:1476-1479`) and **MCP initialize has no timeout**
  (`agents/mcp_client.rs:507-560`): a server that never answers (mcp-remote waiting for an
  OAuth browser) stalls the running turn's tool calls and makes every later `session/load`
  of that session hit honkd's 30 s load timeout.
- No client notification on extension change. Each ACP connection has its own agent;
  honkd holds one per session, so all honkd viewers share one agent.
- honkd forwards any unhandled method carrying `sessionId` (`honkd/src/viewer.rs:767-801`,
  `session.rs:2547-2598`); the reply goes only to the asker. stall-watch
  (`eng-agent-platform/stallwatch/honk_transport.py:803`) uses raw `extensions/remove`
  through honkd, so raw forwarding must keep working.
- honk chat `--new --ext` sends `_meta.enabledExtensions` (platform names,
  `honk/src/chat/driver.rs:487-508`), which **replaces** goose's whole default set.
- pigeon `/launch --backend goose` goes through honkd on cloudbox, always names its
  extensions, and its ACP allowlist excludes extension RPCs (`acp-client.ts:1452`).

## Design (decided; oracle-reviewed)

**honkd owns the catalog, the translation and the policy**; viewers send server names.
One translation for honk chat, pigeon and anything else; one place for policy.

Rejected: client-side translation (pigeon would need a TypeScript copy, no policy point);
a generated goose `config.yaml` catalog (goose's Extension Manager lets the agent enable
anything listed there without asking); goose `envKeys` (cannot map a file to a variable);
blocking raw `extensions/add|remove` in honkd (breaks stall-watch).

1. **Catalog = opencode.json, read fresh on every request** from `honkd --mcp-catalog
   <path>` (no default; feature off without it). Parsing of opencode's format lives in one
   module (`honkd/src/mcp/catalog.rs`). v1 supports `type:"local"` only; remote entries
   are listed as unsupported with a reason. An entry is unsupported (listed, with reason)
   when: its name is not already a goose extension key (`[A-Za-z0-9_-]`); `{file:`/`{env:`
   appear outside `environment` values or embedded in a value; an env key is not
   `^[A-Za-z_][A-Za-z0-9_]*$` or is on goose's disallowed list (`agents/extension.rs:89-125`);
   a **literal** value's key looks secret (`TOKEN|SECRET|PASSWORD|PASSWD|API_KEY|_KEY$|AUTH`);
   an absolute command path does not exist.
2. **Translation.** No `environment` → `{type:"mcp", server:{name, command, args, env:[]}}`
   (keeps goose's npx/uvx OSV malware check working). With environment → command
   `/bin/sh`, args `["-c", WRAPPER, "honk-mcp-<name>", K1, kind1, v1, …, "--", cmd…]` where
   kind is `lit|file|env` and WRAPPER is one fixed, reviewed POSIX script:
   ```sh
   while [ "$#" -gt 0 ] && [ "$1" != -- ]; do
     case $2 in
       lit) v=$3 ;;
       file) v=; { IFS= read -r v || [ -n "$v" ]; } <"$3" || { echo "honk-mcp: cannot read $3" >&2; exit 78; } ;;
       env) eval "v=\${$3-}" ;;
       *) echo "honk-mcp: bad kind $2" >&2; exit 64 ;;
     esac
     export "$1=$v"; shift 3
   done
   [ "$1" = -- ] || { echo "honk-mcp: missing --" >&2; exit 64; }
   shift; exec "$@"
   ```
   Secrets are read by the child at spawn: never in honkd, the goose DB, argv, or logs.
   `~/` in a `{file:}` path is expanded by honkd from `$HOME`. Leading/trailing blanks of
   the one-line secret are trimmed (opencode trims). `exec` keeps goose's process group and
   parent-death signal. Every added extension carries `description:"honk-mcp:<name>"` (the
   **marker**).
3. **honkd methods** (viewer-facing, honk-aware or not):
   - `_honk/mcp/list {sessionId}` → `{servers:[{name, enabled, supported, reason?,
     confirm, sibling?}], other:[{key}]}`. `enabled` = the goose list has the key **and** the
     marker; a key present without the marker is reported as `conflict:true`. `confirm` is true
     for any server not in `--mcp-no-confirm`.
   - `_honk/mcp/enable {sessionId, names, confirm?}` and `_honk/mcp/disable {sessionId,
     names}` → `{results:[{name, ok, error?}], servers:[…]}`. `confirm` is an array of names
     to confirm (e.g. `["slack"]`); non-array `confirm` is rejected with `-32602`. Whole-request
     refusals are JSON-RPC errors: catalog missing/unreadable, unknown or unsupported name, a confirm
     name not listed in `confirm` (message suggests the `-ro` sibling when one exists),
     **session busy** (a run is active; `-32002`, "try after this turn"), another MCP op
     already in flight. Per-name failures (goose add errors) are results, redacted with
     honkd's `{file:}` secret redaction and capped to 2000 chars.
   - enable skips names already on; refuses a name whose key is held by an extension
     without the marker; **siblings are exclusive**: enabling `X` first removes a
     marker-carrying `X-ro` and vice versa (no duplicate slack tools).
   - disable only removes marker-carrying entries. Builtins/platform extensions are never
     touched by `/mcp`, so a viewer cannot strip what pigeon or a coordinator relies on.
   - **No-confirm list (fail-safe):** `honkd --mcp-no-confirm <name>` (repeatable/comma list).
     Servers in this list enable without confirmation; EVERY other catalog server requires
     confirmation. Workstation sets `--mcp-no-confirm slack-ro,figma`. The `-ro` sibling rule
     only drives the hint.
4. **Actor plumbing.** A new `SessionMsg::McpOp` is handled like `Forward` (queued while
   Reattaching/Rebuilding). The actor refuses when `turn.run`/`turn.inflight` is set or an
   op is running, else marks `mcp_op_running` and spawns a task. The task talks to goose
   through a new `SessionMsg::Internal {method, params, reply: oneshot}` (a pending request
   whose response fulfils the oneshot instead of going to a viewer; `fail_pending_requests`
   fails it), with an overall 60 s budget. On completion it replies to the asking viewer and
   sends `SessionMsg::McpOpDone`, which clears the flag and fans out.
5. **Visibility.** `_honk/mcp_changed {sessionId, enabled:[names], change, origin}` goes to
   every honk-aware viewer (not logged; `origin` is `"you"` for the asker, else `"other"`).
   A successful **raw** forwarded `_goose/unstable/session/extensions/add|remove` also fans
   out `_honk/mcp_changed {sessionId, origin}` without `enabled` (viewers re-list). pigeon
   and stall-watch ignore unknown notifications.
6. **Launch.** `session/new` with `_meta.honk.mcp {names, confirm?}`: honkd creates the
   session, runs the enable **before** replying, and puts the result in the reply's
   `_meta.honk.mcp`. A failed enable never fails the session/new. The default extension set
   is untouched (no `enabledExtensions`).
7. **goose fork patch (G1):** MCP initialize gets its own timeout,
   `GOOSE_EXTENSION_STARTUP_TIMEOUT` seconds, default 25 (below honkd's 30 s load timeout),
   separate from the per-request `timeout`. A hung server then fails the add/load cleanly,
   and goose drops it from the stored set.
8. **honk chat.** `/mcp` (table: name, on/off, `!` confirm marker, unsupported reason,
   other extensions), `/mcp on <names>`, `/mcp on! <names>` (confirm), `/mcp off <names>`.
   While a run is active the command is queued locally and runs after the turn (like
   `/compact`). Status bar shows `mcp:slack-ro,atlassian` (falls back to `mcp:2` when
   narrow; hidden when none). Refresh on attach/new, on `_honk/mcp_changed`, and on run end
   (catches the agent's own Extension Manager changes). `--new --mcp <name>` (repeatable;
   `name!` confirms). A change by another viewer prints a notice.
9. **Skill.** A harness-neutral `enabling-mcp-servers` skill (workstation, deployed to
   `~/.agents/skills`, read by both opencode and goose): in goose/honk, ask the human to type
   `/mcp on slack-ro`; prefer `-ro`; agents do not grant themselves write.

**Known trade-offs.** Stored server commands are Nix store paths: after a workstation
update plus GC, a reload fails to start the server and goose drops it from the set (the
user re-enables; the catalog is read fresh). Changes are refused mid-turn (opencode allows
them). honkd listens on loopback without auth, so any local process can call these
methods or goose's directly; `confirm` prevents accidents, not misuse — same stance as
`oc-mcp-enable`. Subagents (summon) may inherit the parent's extensions. Remote servers
(datadog, notion), pigeon `/launch --mcp` and Telegram `/mcp` for goose are follow-ups.

## Tasks

### G — goose fork (`johnnymo87/goose`, PR into `patched/v1.54.0`; fork rules in the patches plan)

- **G1** `mcp_client.rs`: wrap `serve_with_lifecycle` in `tokio::time::timeout`
  (`GOOSE_EXTENSION_STARTUP_TIMEOUT`, default 25 s) → `ClientInitializeError`-shaped error
  naming the timeout. Test: a stdio server that never answers initialize fails within the
  timeout and its process is gone. Modification notice. After merge: tag `patched-1.54.0.7`.

### H — honk (this branch)

- **H1** `honkd/src/mcp/catalog.rs` + `wrapper.rs`: parse, classify, translate (pure; unit
  tests incl. a shell test that runs the wrapper with lit/file/env and checks env + argv,
  missing file → exit 78, no secret in args).
- **H2** `honkd/src/mcp/ops.rs`: list/enable/disable orchestration over a `GooseRequester`
  trait (unit tests with a fake: marker, conflict, siblings, confirm, unknown, idempotence).
- **H3** actor + viewer + hub: `SessionMsg::{McpOp, Internal, McpOpDone}`, busy refusal,
  fan-out, raw-forward notification, `session/new` `_meta.honk.mcp`, `--mcp-catalog` /
  `--mcp-no-confirm` flags. e2e (`honkd/tests/mcp.rs`, StubGoose + a tiny stdio MCP fixture
  server): enable → `tools/list` shows its tool → second viewer gets `_honk/mcp_changed` →
  disable; busy refusal; persistence across goose restart; raw remove still forwarded.
- **H4** honk chat: `/mcp` commands, queueing, status bar, notices, `--new --mcp`;
  `chat_core` unit tests + a chat-driver e2e. `docs/chat.md`.

### W — workstation

- **W1** honkd `ExecStart` gains `--mcp-catalog %h/.config/opencode/opencode.json
  --mcp-no-confirm slack-ro,figma`; bump `flake.lock` honk (after the honk
  PR merges — honkd rejects unknown flags); skill `enabling-mcp-servers`.
- **W2** after the fork release: `pkgs/goose/default.nix` → `patched-1.54.0.7`.

## Deploy and verify (author runs)

1. Merge honk PR, then workstation PR (W1). `nix run home-manager -- switch --flake .#cloudbox`;
   `systemctl --user restart honkd` (honkd restart keeps goose sessions; honk chat reattaches).
2. In honk chat session `20261016_222`: `/mcp` lists servers; `/mcp on slack-ro` → status bar
   shows `mcp:slack-ro`; ask the agent to call `slack-ro__channels_list` (or similar);
   `/mcp on slack` is refused with the slack-ro hint; `/mcp off slack-ro` → indicator clears.
3. `honk chat --new --mcp atlassian` → the notice reports it on.
4. After the goose release (W2): `sqlite3 ~/.local/share/goose/sessions/sessions.db ".backup
   '$HOME/.local/share/goose/sessions/sessions.db.bak-$(date +%F)'"`, `sudo nixos-rebuild switch
   --flake .#cloudbox`, in a quiet window `sudo systemctl restart goose-serve`, then
   `systemctl --user restart honkd`; a session with `slack-ro` on still has it (`/mcp`).
