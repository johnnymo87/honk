# Project-scoped and remote MCP servers for goose sessions (draft)

Status: draft design. Not yet reviewed (oracle and adversarial review are pending).
Branch: `project-mcp`. Worktree: `~/projects/honk/.worktrees/project-mcp` (devbox).
Tracking bead: `eternal-machinery-r7gkd`, the consumer-side bead in eternal-machinery's
DoltHub tracker. Requested by Jonathan on 2026-11-05: "let's do the cleanest fix".

## Problem

A goose session started through honkd cannot get a repo's own MCP servers.

eternal-machinery declares its TEC game server in its repo `opencode.json`:

```json
{"mcp":{"tec":{"type":"remote","url":"http://localhost:4006/mcp","enabled":true}}}
```

opencode attaches it to every session in that repo. Usage over the last 90 days: 80
sessions and 2,792 calls. honkd blocks the same thing for goose at three points:

- `honk launch` sends `session/new` with `mcpServers: []`.
- honkd's catalog (`--mcp-catalog ~/.config/opencode/opencode.json`) is user-level only.
- The catalog rejects `type:"remote"` with "remote MCP servers are not supported yet"
  (`crates/honkd/src/mcp/catalog.rs:299-307`).

goose itself already supports what is needed:

- `_goose/unstable/session/extensions/add` accepts
  `{"type":"mcp","server":{"type":"http","name":..,"url":..,"headers":[]}}`. This is ACP
  `McpServer::Http`, converted to a streamable_http extension in goose
  `acp/server/extensions.rs:276`.
- The resulting extension is persisted in the session, so a reload keeps it.

goose's own per-directory route is project plugins at `.agents/plugins/<p>/.mcp.json`.
They are stdio only, auto-enabled without consent, and write into the global config, so
they are not a good fit.

## Goal

A goose session created through honkd whose `cwd` is inside a project gets that project's
`opencode.json` MCP servers, the same ones opencode gives an opencode session there. One
config file then serves both harnesses. Remote entries work, both from the project file and
from the user catalog through `/mcp enable`.

## Design sketch (to be refined after oracle review)

1. **Remote catalog entries.** `type:"remote"` with `url` (http or https) becomes a
   supported entry kind. `to_add_extension_json` emits
   `{"type":"mcp","description":"honk-mcp:<name>","server":{"type":"http","name":<name>,"url":<url>,"headers":[]}}`.
   `headers` are unsupported in v1: an entry with headers is reported as unsupported with
   a reason. This avoids secret handling until something needs it.
2. **Project source.** For a session's cwd, honkd reads the opencode project config: the
   `opencode.json` / `opencode.jsonc` files from cwd up to the git root (a worktree's
   `.git` file counts as the root). This matches opencode's project-config lookup;
   [verify against opencode source]. Project entries overlay user-catalog entries of the
   same name. The source is recorded per entry: `user` or `project:<path>`.
3. **Auto-enable at session/new.** A project entry that is not `enabled:false` (opencode
   treats a missing `enabled` as true; verify) is enabled right after `session/new`,
   through the same queued `extensions/add` path that `--swarm-default` uses
   (`viewer.rs:697-770`).
   - It is gated by a new honkd flag, `--project-mcp`, off by default. Turning it on in
     workstation is an operator decision.
   - The outcome is reported in the `session/new` reply under `_meta.honk.projectMcp`, so
     `honk launch` and `honk chat` can show it.
   - It is skipped when the client sends `_meta.honk.projectMcp: false`.
4. **Trust policy (open question for the oracle).** A project file is repo-controlled.
   Options:
   - (a) auto-enable remote loopback URLs only, and require the normal confirm flow for
     stdio and non-loopback;
   - (b) auto-enable all, on the grounds that the agent already has a shell in the repo;
   - (c) allowlist projects in honkd config.

   Leaning (a).
5. **Re-activation.** goose restores persisted extensions on `session/load`, so nothing is
   re-added. A project-file change does not reach existing sessions, which is acceptable.
   Self-grant `mcp_list`/`mcp_enable` should list project entries too, which needs a
   per-session cwd lookup in `session.rs:2608/2645`.
6. **Tests.** Unit tests in `catalog.rs` for remote parsing, translation, headers and
   invalid urls, plus project lookup and overlay. An integration test with the stub goose
   (`honk-testgoose`) that `session/new` with a project cwd results in an `extensions/add`
   carrying the http server.

## Out of scope

- Remote MCP headers and auth.
- goose fork changes such as plugin `.mcp.json` http support (the goose coordinator's
  item 7).
- Deploy: the workstation flake bump and turning on `--project-mcp` are a separate,
  author-gated step.
