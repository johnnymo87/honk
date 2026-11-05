# goose sessions get their project's MCP servers, remote ones included

Tracking bead: `eternal-machinery-r7gkd` (the consumer's tracker). Branch `project-mcp`.

## Ask

Jonathan, 2026-11-05: a goose worker started in a repo should get the MCP servers that
repo declares for opencode, so one config serves both harnesses. "let's do the cleanest fix".

The first consumer is a repo whose `opencode.json` declares a local game-control server:

```json
{"mcp":{"tec":{"type":"remote","url":"http://localhost:4006/mcp","enabled":true}}}
```

## Facts this design rests on (verified 2026-11-05 at honk `ac2e358`)

- **Where it is blocked today.**
  - `honk launch` sends `session/new` with `mcpServers: []` (`launch.rs:416`).
  - The catalog is user-level only (`--mcp-catalog`).
  - The catalog rejects `type:"remote"` (`catalog.rs:299-307`).
- **One choke point.** honk chat, `honk launch` and pigeon `/launch --backend goose` all
  create sessions through honkd's `session/new` (`viewer.rs:697`). `--swarm-default`
  already enables a server there, through the `McpLaunch` queue, and reports the result
  in `_meta.honk.swarm`.
- **goose takes http servers.** `_goose/unstable/session/extensions/add` accepts
  `{"type":"mcp","server":{"type":"http","name","url","headers":[]}}` and turns it into a
  streamable-HTTP extension (goose `acp/server/extensions.rs:276`). An added extension
  persists in the session only on success. A reload (`session/load`, same id) brings it
  back, the way `swarm` survives today.
- **OAuth stall.** When a streamable-HTTP server answers 401/403 with OAuth metadata,
  goose starts a browser OAuth flow. That flow waits up to 300 s
  (`GOOSE_OAUTH_CALLBACK_TIMEOUT_SECONDS`), outside the 25 s extension startup timeout.
  On a headless serve the add stalls. This was found by the goose-fork session, at goose
  `agents/extension_manager/streamable_http.rs` ~632-650.
- **How opencode finds project config** (sst/opencode v1.18.18, `config/paths.ts`).
  - It walks from the session directory up to the worktree (the git toplevel), taking
    `opencode.jsonc` and then `opencode.json` in each directory.
  - Files nearer the cwd win, and they are deep-merged.
  - `mcp.<name>.enabled` defaults to true: only `false` disables an entry.

## Design (after the oracle consult)

1. **Remote entries in the catalog.** `type:"remote"` with an `http`/`https` URL becomes
   supported. It translates to the http `extensions/add` shape above, with the usual
   `honk-mcp:<name>` marker. These remote entries stay unsupported, each with a reason:
   - entries with `headers`;
   - entries with an `oauth` object;
   - entries whose URL contains `{env:` or `{file:`.
2. **Probe before every remote add.** honkd sends one MCP `initialize` POST with a
   timeout of a few seconds.
   - Connection refused or a timeout: "not reachable".
   - 401 or 403: "needs authorization (OAuth), which honkd does not support".
   - Anything else: go ahead with the add.

   This keeps the OAuth stall and a down server out of the session. The user catalog's
   OAuth servers (notion and the like) then fail fast and say why.
3. **Project source.**
   - **Walk.** Canonicalize the session cwd and walk up to the first directory holding
     `.git`, file or directory, so worktrees and submodules stop at their own root. If
     there is no git root, read the cwd only. That is stricter than opencode on
     purpose: opencode walks to `/`, and `/tmp` is world-writable.
   - **Files.** In each directory, read `opencode.jsonc` and then `opencode.json`. Both
     accept comments and trailing commas.
   - **Merge.** Deep-merge the `mcp` objects; the nearest file wins, and arrays are
     replaced.
   - **Bad files.** An unreadable or bad file is skipped and reported, never fatal.
   - **Deferred.** `.opencode/opencode.json` is not read yet.
4. **Effective catalog.** Every MCP operation builds the session's catalog as: the user
   catalog, plus the project overlay for the session's stored cwd, plus `swarm`.
   - A user entry beats a project entry of the same name. The project entry is listed
     as shadowed. This matters because `--mcp-no-confirm` is keyed by name, so a repo
     must not be able to reuse a trusted name.
   - `swarm` beats both.
   - Every entry records its source: `user`, `project:<file>` or `builtin`.
5. **Trust.**
   - **Auto-eligible.** A project entry may be auto-enabled, and needs no confirm, only
     if all of these hold:
     - it is remote;
     - it is not `enabled:false`;
     - it has no headers or oauth;
     - its URL host, after parsing, is `localhost`, in 127.0.0.0/8, or `::1`.
   - **Other project remote entries** are listed and need the human's `name!`. An agent
     cannot self-grant them.
   - **Project stdio entries** are listed as unsupported in v1. A command chosen by the
     repo would run, and goose stores stdio env in its user-wide secret store.
6. **Auto-enable at `session/new`.**
   - It sits behind a new honkd flag, `--project-mcp`, off by default. The flag works
     without `--mcp-catalog`.
   - A session opts out with `_meta.honk.projectMcp: false`.
   - All eligible names are enabled in one `McpLaunch`, after swarm.
   - The outcome goes in the reply as `_meta.honk.projectMcp`. Its shape is
     `{"servers":[{"name","source","ok","error"?}], "skipped":[{"name","reason"}]}`.
   - A failure never fails `session/new`. `honk launch` prints the outcome.
7. **Disable by marker.** `/mcp disable <name>` works for a honk-marked extension even
   after its entry has left the catalog, for example when the project file changed.
8. **Not in this change.**
   - Re-adding failed project servers on reactivation.
   - Remote headers and auth.
   - `.opencode/` directories.
   - Detecting the same server loaded twice (honkd source plus a goose plugin
     `.mcp.json`, after the goose fork's plugin http work). Use one route per repo.
   - Setting `GOOSE_OAUTH_CALLBACK_TIMEOUT_SECONDS` low in goose-serve's environment.
     That belongs to the deploy step.

## Tasks

1. **catalog.rs.**
   - Remote parsing and translation, with the unsupported reasons above.
   - The source tag, and the loopback auto-eligibility check, which parses the URL.
   - Unit tests: `localhost.evil.com`, `127.0.0.1@evil.com`, `[::1]`, `127.2.3.4`, https,
     `{env:` in a URL, headers, oauth.
2. **New `mcp/project.rs`.**
   - Discovery: the walk, jsonc, and the deep merge.
   - `effective_catalog(user, project, swarm)`, with user-wins shadowing.
   - Unit tests: a worktree's `.git` file, a symlinked cwd, no git root, nearest file
     wins, `enabled:false` in a nearer file, a bad file skipped.
3. **ops.rs and session.rs.**
   - Remote probe through a trait (real version on reqwest; tests use a fake). A test
     against a local axum stub covers 200, 401, 403 and a closed port.
   - Trusted project entries skip confirm, but an agent still gives a reason.
   - Disable by marker.
   - `spawn_mcp_task` uses the effective catalog with the stored cwd.
4. **config, viewer and launch.**
   - `--project-mcp`, the opt-out, and `_meta.honk.projectMcp`.
   - `McpConfig` built without a catalog path.
   - `honk launch` prints the outcome.
   - Integration tests with `honk-testgoose`: an http add with the marker, the opt-out,
     a down server reported while `session/new` still succeeds, and disable after the
     entry is removed.
   - Docs: README, HANDOFF.

## Deploy (separate, needs the author's go)

- Bump honk in the workstation flake.
- Add `--project-mcp` to honkd's ExecStart.
- Optionally set `GOOSE_OAUTH_CALLBACK_TIMEOUT_SECONDS` for goose-serve.
- Restart honkd. Sessions keep running through a honkd restart; only new sessions get
  project servers.
