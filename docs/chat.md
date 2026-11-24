# honk chat: Interactive TUI Client

`honk chat` is a full-screen [ratatui](https://github.com/ratatui/ratatui) terminal client for goose sessions held by `honkd`. It lets you attach to any running or saved session, watch transcript output stream in real time—including turns driven by automated background agents—submit prompts, steer active turns, and navigate session history.

---

## Quick Start

### Attach to an Existing Session

```bash
# List available sessions held or tracked by honkd
honk list

# Attach to a session by ID
honk chat ses_0192384f-abcd-...
```

### Create and Attach to a New Session

```bash
# Start a new session in current working directory
honk chat --new

# Start a new session in a specific directory
honk chat --new --cwd /path/to/project

# Start a new session with a specific model
honk chat --new --model claude-opus-5-5

# Enable MCP servers from catalog on session creation (repeatable, '!' confirms)
honk chat --new --mcp slack-ro --mcp atlassian!

# Restrict loaded platform extensions (repeatable)
honk chat --new --cwd /path/to/project --ext developer --ext summon

# Tag the new session in oc-tags (as goose:<id>), like opencode-launch --tag
honk chat --new --tag honk
```

*(Note: `--cwd` is validated and canonicalized; it returns an error if the directory does not exist. If omitted, `--new` defaults to the canonicalized current working directory).*

### Connecting to honkd

By default, `honk chat` connects to `http://127.0.0.1:4280/acp`. You can override this using:

```bash
honk chat ses_123 --url http://127.0.0.1:4280/acp
# or via environment variable
export HONK_URL=http://127.0.0.1:4280/acp
```

### Limiting Turns per Prompt

You can cap the number of agent model calls per prompt turn:

```bash
# Cap model calls per turn at 10
honk chat ses_123 --max-turns 10
# or via environment variable
export HONK_MAX_TURNS=10
```

When set, `honk chat` includes `_meta.goose.maxTurns` on each prompt. You can dynamically adjust or clear this during a session using `/turns <N>` or `/turns off`.

The per-prompt value *replaces* goose's own `GOOSE_MAX_TURNS` limit for that prompt, in both directions: a large value raises the cap above the server's setting. It requires goose patched-1.54.0.4 or later; older goose ignores it. Recipe sessions that end every turn with `final_output` (such as stall-watch coordinators) can loop on chat-style prompts, so a small cap (around 10) is a good idea when chatting with them.

`honk list` walks every page of goose's `session/list` (50 per page, most recently updated first). A session updated while the walk is in progress can move to an already-fetched page and be missed; run it again if a busy session is absent. `honk list --cwd DIR` filters by exact working directory (no symlink or trailing-slash normalization).

`honk list --json` prints the same sessions as one JSON array, for scripts. Each object has `sessionId`, `title`, `cwd`, `held`, `running`, `updatedAt`, `lastMessageAt`, `archivedAt`, `sessionType`, `hasRecipe`, `pendingRequests` (permission prompts and other goose requests waiting on an answer, counted by honkd) and `lastError`; a value that was not sent is `null`.

`lastError` is `{kind, message, at}` when the session's last turn failed in the past 48 hours, else `null`. `at` is in Unix seconds. The `kind` comes from goose's `_meta.goose.turnError` on the prompt response (`network`, `provider`, `refusal`, `context_length`, `self_compact`, `retry_exhausted`, `empty_response`, `unsupported`; a goose without that patch never sends it). honkd adds two kinds of its own: `rpc`, when goose answered the prompt with a JSON-RPC error, and `interrupted`, when goose restarted mid-turn or honkd could not tell whether goose got the prompt. A cancelled turn is not an error. The error clears when a later turn on the session starts, or ends clean. honkd keeps it in its state file across restarts and drops it after 7 days. When honkd itself restarts mid-turn and never sees goose's reply, it ends the turn as finished, so a failure in that turn is not reported. workstation's `oc-session-list` reads it to put goose sessions in the nvim session pickers.

Authentication secret is resolved in the following priority order:
1. `--secret <token>` CLI argument
2. `$HONK_SECRET` environment variable
3. `$XDG_STATE_HOME/honk/secret`
4. `$HOME/.local/state/honk/secret`

---

## Screen Layout

The user interface consists of three distinct vertical areas:

1. **Transcript Pane** (top):
   - Renders session turns chronologically.
   - **User messages**: labeled user turns (`> text`). Consecutive identical user messages automatically collapse into a single line with a `×N` suffix. When a user message's full content parses as a JSON object or array (such as recipe or coordinator wake payloads), it renders collapsed by default to a single dim summary line (e.g. `▸ structured input · program=example-program · wake_reason=activity · 18 KB`); `zj` or `:json` toggles full structural expansion.
   - **Assistant messages**: markdown rendered with syntax highlighting and newline-gated streaming. When the response parses as a JSON object or array, it renders structurally: top-level keys appear as bold/dim labels, string values are unescaped and rendered as markdown, string arrays appear as bullet lists, arrays of objects appear as indented sub-blocks, inline scalars appear beside keys, and empty strings/arrays/objects are omitted. In-progress streaming JSON displays `◌ receiving structured reply (N KB)…` until the full object arrives.
   - **Thinking blocks**: the model's reasoning, shown in full by default under a dim yellow `- Thought` header, the reasoning itself dim italic. When the reasoning opens with a `**Title**` paragraph (as summarized thinking often does), the title moves into the header: `- Thought: Title`. `zt` collapses every thinking block to its header with a line count (`+ Thought: Title · 3 lines`) and expands them again. A block with no reasoning text shows `Thought (no text)`; that is what a provider sends when it returns only a thinking signature, as Claude does when thinking is requested without `display: "summarized"`.
   - **Tool calls**: compact status row displaying the glyph, title (formatted with recognizable command/path details such as `shell · ls` or `write · notes.md`), and live tool notification output (`za` toggles full input/output view). Tools ending in `final_output` (e.g. `recipe__final_output`) render as a dim one-liner `◆ final output` without dumping duplicate inputs.
   - **Honk notices**: informational and warning notices (e.g. queue state, extension warnings).

2. **Status Bar** (single row):
   - Displays session title (or session ID), working directory (`cwd`), run state, `queued: N`, and connection status (`live`, `reconnecting…`, or `stopped`).
   - **Activity indicator**: while a turn is busy, the run state is an animated spinner, what the session is doing, and how long the turn has run: `[⠹ thinking 12s]`, `[⠼ shell · cargo test 40s]`. The states are `needs approval` (another client's turn waits on `/approve`), `compacting`, `waiting` (prompt sent, turn not started), `thinking`, `replying`, the running tool's title, and `working` (between steps). A turn another client started adds `· other client`. Idle shows `[idle]`. Every attached view shows it; nothing is sent to honkd or pigeon. When you attach mid-turn, the timer counts from the turn's user message. The bar is redrawn only when the indicator changes (10 times a second while busy, never while idle); set `HONK_NO_ANIM=1` for a static `⋯` on slow links.

3. **Composer / Command Bar** (bottom):
   - Multiline editor with modal vim navigation.
   - Long lines wrap at the screen edge. The composer grows with the number of wrapped rows,
     up to 8; taller input scrolls to keep the cursor visible. `zm` maximizes it (all but the
     status bar and 3 transcript rows) until you toggle it back or send.
   - Pasted text keeps its line breaks (the CR or CRLF that terminals and tmux send become
     newlines). In insert mode it goes in at the cursor, as if typed.
   - Pressing `:` in normal mode switches to a single-line command bar for executing commands.

---

## Key Map

### Insert Mode

| Key | Action |
|-----|--------|
| `Enter` | Submit current prompt (when composer is not empty). |
| `Alt-Enter` | Insert a newline. |
| `Ctrl-J` | Insert a newline. |
| `Shift-Enter` | Insert a newline (requires terminal/tmux keyboard enhancement support; otherwise treated as `Enter`). |
| `Up` / `Down` | Scroll transcript up / down 3 lines (when composer is empty, e.g. mouse wheel via `?1007`); moves cursor in editor when composer has text. |
| `Ctrl-C` | If composer has text: exits to **Normal Mode** (vim habit). If composer is empty: interrupts active turn (`Action::Interrupt`). |
| `Ctrl-D` | If composer is empty: quits `honk chat` (`Action::Quit`). If composer has text: ignored (no-op). |
| `Ctrl-P` | Previous history entry. |
| `Ctrl-N` | Next history entry. |

### Normal Mode (Vim)

Press `Esc` or `Ctrl-C` from insert mode to enter normal mode. Press `i` or `a` to return to insert mode.

| Key | Action |
|-----|--------|
| `Ctrl-E` | Scroll transcript down 1 line. |
| `Ctrl-Y` | Scroll transcript up 1 line. |
| `Ctrl-D` | Scroll transcript down half a page. |
| `Ctrl-U` | Scroll transcript up half a page. |
| `Ctrl-F` | Scroll transcript down full page. |
| `Ctrl-B` | Scroll transcript up full page. |
| `Up` / `Down` | Scroll transcript up / down 3 lines (when composer is empty); moves cursor in editor when composer has text. |
| `g g` | Scroll to top of transcript (when composer is empty). |
| `G` (Shift-G) | Scroll to bottom of transcript (when composer is empty). |
| `z a` | Toggle tool call expansion (expand/collapse tool arguments and outputs). |
| `z t` | Toggle thoughts expansion (collapse/expand model reasoning blocks; expanded by default). |
| `z j` | Toggle JSON payload expansion (expand/collapse structured user messages). |
| `z m` | Toggle a maximized composer. `/quote` turns it on; sending or clearing the composer turns it off. |
| `Ctrl-X m` | Open model picker overlay (in both Normal and Insert modes). |
| `F2` | Cycle to next recent model without reordering recents (in both Normal and Insert modes). |
| `Shift-F2` | Cycle to previous recent model without reordering recents (in both Normal and Insert modes). |
| `:` | Open command line bar. |
| `Ctrl-P` / `Ctrl-N` | Recall previous / next prompt history. |
| `Ctrl-C` | If composer has text and honk is idle or the composer is maximized: clears composer (and unmaximizes). Otherwise: interrupts active turn (`Action::Interrupt`). |
| Standard Vim | `h`, `j`, `k`, `l`, `w`, `b`, `x`, `u`, `dd`, etc. for composer text manipulation. |

### Command Line Mode (`:`)

| Key | Action |
|-----|--------|
| `Enter` | Execute command. |
| `Esc` / `Ctrl-C` | Dismiss command bar without executing. |
| `Backspace` | Delete previous character. |

---

## Commands

Commands can be executed either from the `:` command line or by typing them directly as slash commands in the prompt composer:

| Command | Colon Form | Description |
|---------|------------|-------------|
| `/help` | `:help` | Show command help summary. |
| `/quit` | `:q`, `:quit` | Cleanly detach from session and exit. The upstream session and any running turns continue undisturbed. |
| `/cancel` | `:cancel` | Interrupt active turn if owned by this client. |
| `/cancel!` | `:cancel!` | Unconditionally cancel active turn upstream, regardless of who started it. |
| `/drop` | `:drop` | Discard all locally queued or held messages. |
| `/approve` | `:approve` | Approve the oldest pending permission request with `allow_once`. |
| `/turns` | `:turns` | Set or clear model calls cap per prompt (`/turns N` or `/turns off`). |
| `/compact` | `:compact` | Compact session context (queued if a turn is running; lines typed during compaction are queued; /cancel cannot interrupt a compaction already running). |
| `/model [id]` | `:model [id]` | Switch model (`/model <id>`), open model picker (`/model`), or cycle recent models with `F2`/`Shift-F2`. Switching to a model with a smaller context window mid-session may trigger goose auto-compaction on the next turn. `F2`/`Shift-F2` cycling does not reorder the recent models list. |
| `/undo` | `:undo` | Hide the last turn across all views and fill an empty composer with the undone prompt text (files not restored; `/redo` restores until next prompt). |
| `/redo` | `:redo` | Restore the last undone turn (until the next prompt arrives from anyone). |
| `/mcp` | `:mcp` | List catalog MCP servers, or configure per-session servers (`/mcp list`, `/mcp enable <names>`, `/mcp disable <names>`). |
| `/quote` | `:quote` | Load the replies since your own last prompt (as `/quote full` picks them) into the composer, trimmed by goose's default model to what you need to answer or see. The model only returns line numbers; honk copies those lines from the transcript, so every `> ` line is the agent's own words. Lines honk adds are `#` lines: a first line saying which model trimmed and how many lines it kept, each kept reply's header with a tag (`ask`, `blocker`, `done`, `fyi`), and `# …` between passages it cut apart. A reply still streaming is appended untrimmed. Replies under 4000 bytes in all are quoted in full without the model; when that happens for bare `/quote`, the notice points at `/quote N` and `/quote prev`, for when you already replied before reading. While the model runs (seconds to a minute) the status bar shows `[trimming Ns]`; Ctrl-C on an empty composer cancels the trim (not the agent's run), and sending a prompt or an untrimmed `/quote` cancels it too. Anything you type meanwhile stays below the quote when it arrives, and Ctrl-C with text in the composer keeps its usual meaning. If the call fails or the answer is unusable, it quotes all the picked replies, with a warning; if the model keeps nothing, it says so and leaves the composer alone. A span over about 100 KB of reply text is split into several calls of whole replies (about 100 KB each, four at a time), each told it sees a part, and the kept lines are merged in reply order; the notice says how many calls. If some calls fail, their replies are quoted in full, with a warning naming how many failed. The same replies again reuse the last answer, when every call succeeded. Limits that remain: each call has honkd's 90 s budget, so a very long span can take a few minutes (the status bar counts seconds, not calls); a single reply is never split, so one too long for the model is quoted whole after its call fails; spend grows with the span; and a reply still streaming is quoted as it was when you ran `/quote`. A very large untrimmed quote also makes the composer slower to redraw (about 30 ms per frame at 1 MB). honkd does the pick and the trim (`_honk/quote`, below; pigeon's Slack `/quote` makes the same call), each model call through `_honk/oneshot`, on a throwaway hidden goose session with no tools, archived afterwards and tagged so its spend counts under the right program: a stall-watch coordinator's program (its tag comes from the coordinator's working directory, since coordinators themselves are tagged `stall-watcher-runs`), otherwise the quoted session's own tag. |
| `/quote N` | `:quote N` | `/quote`'s trim over the last N replies instead, for when you already replied and bare `/quote` finds little. Any N works; asking for more replies than the session has quotes them all, saying how many. |
| `/quote prev` | `:quote prev` | `/quote`'s trim over every reply since your previous prompt (the one before your last), the replies to both prompts included. With only one prompt of yours in the transcript it counts from that one, saying so. |
| `/quote full` | `:quote full` | Any of the forms above untrimmed, with no model call: `/quote full` (since your last prompt), `/quote full N`, `/quote full prev`. `/quote all` is the same as `/quote full`. "Since your last prompt" starts with the reply to it: wakes, swarm messages, scheduled messages, prompts goose wrote and `/compact` (stall-watch sends those too) don't count as yours; a plain-text prompt another agent wrote, such as a launch prompt, does, since nothing marks it. There is no cap on how many replies any form picks: honkd reads the session's whole log, so every reply goose kept is there to quote. If you haven't spoken in the loaded transcript it falls back to the last 3 (the trimmed forms too), and if nothing has replied since your prompt it warns instead. |
| Quote format | | Every form loads replies into the composer, oldest first, each quoted with `> ` under a `# MM-DD HH:MM (trigger)` header, and maximizes the composer in normal mode. A turn counts only if the agent wrote reply text in it; thoughts, tool calls and the message that started the turn (a stall-watch wake, a swarm message, or your own prompt) are left out, and `/compact` turns are skipped. The trigger is `you`, `wake: <reason>`, `swarm: <sender>`, `scheduled`, `goose` (a prompt goose wrote itself), or `start`; `in progress` marks a reply still streaming. `/quote` replaces the composer; `:quote` keeps your draft below the quotes. No `/quote` form is sent upstream. |
| `/yank` | `:yank` | Copy the text of the latest assistant message to the system clipboard via ANSI OSC 52 (refused if >100 KB base64). |
| — | `:dump` | Write entire transcript to a temporary markdown file (`honk-dump-*.md`) and open in `$PAGER` or `$EDITOR` (defaults to `less -R`). |
| — | `:json` | Toggle JSON payload expansion (`zj`). |
| `//text` | — | Escape sequence: sends literal `/text` as a user message instead of parsing as a command. |

---

## Shared Sessions and Concurrency

Sessions in `honkd` are designed to be concurrently shared between humans and automated clients (e.g., pigeon workers, stall-watch coordinators).

### Turn Ownership
Every prompt submitted assigns a unique client request ID (`client_req_id`).
- **Own turn**: When `honkd` broadcasts a turn whose echo carries your client request ID, `honk chat` confirms ownership.
- **Pending turn**: Your prompt has been sent and is awaiting dispatch or confirmation.
- **Foreign turn**: A turn initiated by another client or agent.

### Queued Prompts vs. Steering
- **Foreign turns are never steered into**: In automated sessions (such as recipes or coordinators), steering into an ongoing foreign turn could corrupt workflow state. If you submit a prompt while a foreign turn is running, `honk chat` holds the message locally and queues it. When the foreign turn completes, your message is automatically dispatched.
- **Steering own turns**: While your own turn is running, submitting additional text issues a steer command (`_goose/unstable/session/steer`) into the active turn.

### Cancellation
- `Ctrl-C` or `/cancel`: Only cancels **own confirmed turns**. If another client's turn is active, `honk chat` refuses to cancel and alerts you that the turn belongs to someone else.
- `/cancel!`: Emergency escape hatch that forces upstream cancellation of whatever turn is active.

### Permissions and Elicitation
- **Auto-approval**: Permission requests occurring within **your confirmed turns** are automatically answered with `allow_once` (avoiding persistent policy writes in goose).
- **Foreign turns**: Permission requests belonging to other clients are ignored by your viewer (a notice is shown), letting the owning client respond.
- **/approve**: If an abandoned turn is stuck awaiting permission, `/approve` allows you to unblock it manually.
- **Elicitation**: Form elicitation requests in own turns are declined with a notice (interactive forms are planned for a subsequent release).

---

## Model Switching (`/model`)

`honk chat` tracks the current model and lets you switch models dynamically **per session**:

- **Status bar**: The current model (e.g. `claude-opus-5-5`) is displayed in the status bar, dropped gracefully on narrower terminal widths.
- **Commands & Shortcuts**:
  - `/model <id>` (or `:model <id>`): Switch the session to a specific model. Goose accepts any model ID within the session's provider. If entered while a turn is active, the command is queued (`queued: /model <id>`) and dispatched automatically when the turn ends.
  - `/model` (or `:model`): Opens the filterable **model picker overlay** above the composer. Type to filter by model ID or display name (subsequence match), use the arrow keys or `Ctrl-n`/`Ctrl-p` to navigate, `Enter` to switch, and `Esc` to close.
  - `Ctrl-X m`: Opens the model picker overlay directly from both Normal and Insert modes.
  - `F2` / `Shift-F2`: Cycle to the next or previous model in the recent models list from both Normal and Insert modes.
- **Session creation flags**:
  - `honk chat --new --model <id>`: Starts a new session directly on the specified model. If setting the model fails, an error notice is displayed in chat and the session remains open.
  - `honk launch --model <id>`: Headlessly launches a new session on the specified model. Failure aborts before prompting.
- **Configured models (`--models-file`)**: `honkd` reads an optional JSON configuration file specifying models per provider via `--models-file <path>` (e.g. `{"providers": {"gcp_vertex_ai": [{"id": "...", "name": "..."}]}}`). Configured models appear at the top of the picker in file order, followed by Goose's built-in options. The file is re-read on each `/model` request so edits take effect immediately without restarting `honkd`.
- **Recent models**: Successful model switches are recorded in `$XDG_STATE_HOME/honk/model-recent.json` (falling back to `~/.local/state/honk/model-recent.json`), maintaining the 10 most recently used models across all `honk chat` sessions on the host.

---

## Extension Handling on `--new`

When creating a new session with `honk chat --new`:
- By default, `honk chat` does **not** restrict extensions via `enabledExtensions`. Goose loads every extension enabled in the host configuration (`$HOME/.config/goose/config.yaml`), providing full developer tools out of the box.
- Specifying `--ext <NAME>` (e.g. `--ext developer --ext summon`) restricts loaded extensions strictly to those specified.
- Loaded extension summaries and any failure reports are presented as notices in the transcript.

---

## MCP Servers (`/mcp`)

`honk chat` allows turning Model Context Protocol (MCP) servers on and off dynamically **per session**:

- **Catalog source**: `honkd` reads OpenCode's generated MCP configuration (`~/.config/opencode/opencode.json`) via `--mcp-catalog <path>`. All catalog servers are off by default.
- **Session-scoped & persistent**: Server toggles are stored in goose's session extension database, so enabled servers persist across goose-serve and honkd restarts.
- **Commands**:
  - `/mcp` or `/mcp list` (or `:mcp`): Lists catalog servers (`on`/`off`), `!` markers for servers requiring confirmation, conflicts, and unsupported servers (`n/a <name> — <reason>`). Servers marked `!` need a trailing `!` to confirm (`/mcp enable <name>!`); honkd decides which via `--mcp-no-confirm`.
  - `/mcp enable <names...>`: Enables one or more catalog servers. Append `!` (e.g. `/mcp enable slack!`) to confirm servers marked with `!`. For servers like Slack, prefer read-only siblings (`slack-ro`) unless write permissions are explicitly needed. Sibling toggles are exclusive (enabling `slack` automatically replaces `slack-ro` and vice versa).
  - `/mcp disable <names...>`: Disables one or more servers previously enabled by honk. Built-in platform extensions (e.g. `developer`) are never removed by `/mcp`.
- **Queued during active turns**: MCP changes are refused by `honkd` while a turn is in flight (`-32002`). When typed during an active turn, `honk chat` queues `/mcp enable|disable` locally (`queued: /mcp enable <name>`) and dispatches them automatically when the turn ends.
- **Shared visibility**: Enabled MCP servers appear in the status bar (`mcp:slack-ro,atlassian`, falling back to `mcp:2` on narrower terminals and dropped on the narrowest). Changes made by another client connected to the same session trigger an informational notice (`mcp: <change> (by another client)`), while changes made by the agent itself trigger `mcp: <change> (by the agent: <reason>)` (or `(by the agent)` when no reason was given).
- **Swarm MCP default**: When `honkd` is started with `--swarm-default` (requiring `--swarm-mcp-bin` and `--swarm-pigeon-url`), the built-in `swarm` server is enabled by default on `session/new` and reported under `_meta.honk.swarm` in the reply. `honk chat --new --no-swarm` opts out by sending `_meta.honk.swarm: false` on `session/new` and skipping its own post-creation swarm enable. If swarm enablement fails on `session/new`, `honk chat` surfaces the error as a notice.
- **Agent self-grant tools (`mcp_list`, `mcp_enable`, `mcp_disable`)**: When `honkd` runs with `--mcp-self-grant` and the `swarm` MCP server is enabled, goose agents can turn MCP servers on and off for themselves via three tools:
  - `mcp_list` (`swarm__mcp_list`): Lists catalog servers, their on/off status, `!` confirmation markers, read-only siblings, and unsupported reasons.
  - `mcp_enable` (`swarm__mcp_enable`): Enables one or more catalog servers. For servers marked `!`, agents append `!` to confirm (e.g. `slack!`) and provide a `reason` explaining write access.
  - `mcp_disable` (`swarm__mcp_disable`): Disables one or more servers. Disabling `swarm` itself is refused before any request.
  The `swarm` MCP server probes `honkd` for self-grant support at startup (`GET /mcp/self-grant`); when `--mcp-self-grant` is off, these three tools are omitted from the tool list. Existing sessions see the tools after their swarm server is respawned (i.e. session reload or `/mcp disable swarm` followed by `/mcp enable swarm`). When enabled, new tools become available immediately in the same turn and are usable from the agent's next step.
- **Session creation flags**: `honk chat --new --mcp <name>` enables catalog servers at creation time. Append `!` (e.g. `--mcp slack!`) to confirm servers that require confirmation (honkd decides which via `--mcp-no-confirm`). Pass `--no-swarm` to disable swarm MCP on the new session.
- **Remote servers**: A catalog entry of `type: "remote"` with an `http` or `https` URL is supported and goes to goose as a streamable-HTTP server. Before every remote add, `honkd` sends one MCP `initialize` POST (3 s budget). A server that is not reachable fails that name with `not reachable at <url>: <cause>`; one that answers 401 or 403 fails with `needs authorization (OAuth), which honkd does not support`. This is deliberate: goose answers such a server with a browser OAuth flow that waits up to five minutes on a headless serve (`GOOSE_OAUTH_CALLBACK_TIMEOUT_SECONDS`), and the probe keeps that stall out of the session.
- **Project servers (`--project-mcp`)**: With `honkd --project-mcp`, the MCP servers a repo declares for OpenCode also serve its goose sessions, so one `opencode.json` covers both harnesses.
  - *Where it looks.* From the session's `cwd` up to the first directory holding `.git` (a file or a directory, so worktrees and submodules stop at their own root), reading `opencode.jsonc` then `opencode.json` in each directory; files nearer the `cwd` win and their `mcp` objects are deep-merged, as in OpenCode. With no `.git` above, only the `cwd` itself is read (OpenCode would walk to `/`; a shared parent such as `/tmp` must not be able to inject servers). An unreadable or invalid file is skipped and reported, never fatal. `.opencode/` directories are not read yet.
  - *What is enabled on `session/new`.* A project entry is enabled automatically, with no confirmation, only if it is a remote server, is not `"enabled": false`, has no `headers` or `oauth`, and its URL host is `localhost`, an address in `127.0.0.0/8`, or `::1` (checked on the parsed host, so `localhost.example.com` and `127.0.0.1@example.com` do not count). All such servers are enabled in one go after `swarm`, each behind the probe above, and the outcome is reported in the reply's `_meta.honk.projectMcp`: `servers` (`name`, `source`, `ok`, `error`, and `pending: true` if honkd stopped waiting; the add may still land, so run `/mcp`), `skipped` (`name`, `reason`) and `problems`. It is omitted when the project declares no MCP servers. A failure never fails `session/new`, and `honk launch` prints one stderr line about it. `honk chat --new` and pigeon's goose launches go through the same `session/new`.
  - *What is left to a person.* Any other project entry is listed in `/mcp` (marked `(project)`) and needs the trailing `!` (`/mcp enable <name>!`). A goose agent cannot enable one with `mcp_enable`, even with a reason. It can enable the loopback ones, but must give a `reason` for any project server (marked `(project)` in `mcp_list`), `!` or not. Project stdio (`type: "local"`) entries are listed as unsupported: a command chosen by a repo would run on the host, and goose keeps stdio environments in its user-wide secret store. Remote entries with `headers`, an `oauth` object, or `{env:`/`{file:` in the URL are unsupported too.
  - *User wins.* A project entry is dropped when the user catalog has an entry of the same name, the `-ro`/non-`-ro` sibling of that name (`slack` and `slack-ro`), or the same URL; `swarm` and `swarm-ro` are reserved and never taken from a project. Dropped entries are reported in `skipped`. This matters because `--mcp-no-confirm` is keyed by name: a repo cannot borrow a trusted name, and configured `--mcp-no-confirm` names never apply to project entries. Siblings only pair within one source, and when a project holds both `foo` and `foo-ro` as loopback servers, only `foo-ro` is enabled at `session/new`.
  - *Hostile files.* A repo picks these paths, so a config file must be a regular file of at most 1 MiB, owned by the user honkd runs as, and must resolve inside the project (a symlink to another directory's config is skipped). The git root must be owned by that user too, or only the session directory is read. A file that fails a check is skipped and listed in `problems`; discovery that takes more than 2 s is abandoned the same way. A remote entry whose URL has credentials (`user:pw@host`) is unsupported, and a probe that meets a redirect fails with the target URL: put the final URL in the config.
  - *Opting out and turning it off.* `_meta.honk.projectMcp: false` on `session/new` skips only the auto-enable for one session: the project's entries are still listed in `/mcp`, and a person (or an agent, with a reason) can enable them later. The project files are read again on every `/mcp` operation; without `--project-mcp` honkd reads no project files at all. The flag works with or without `--mcp-catalog`.
  - *After the fact.* `/mcp disable <name>` removes an extension that honk added even when its entry is gone from the catalog (the project file changed). Servers that failed at `session/new` are not retried when the session is reactivated; `/mcp enable <name>` does it by hand.
- **Tool allowlists (`--mcp-allow-tools`)**: `honkd --mcp-allow-tools SERVER=TOOL,TOOL,...` (repeatable, one server per flag) makes honkd pass goose's per-extension `available_tools` when it enables `SERVER`. goose then leaves every other tool of that server out of the session's tool list and refuses calls to it. A server with no entry exposes every tool. This is how a server that is only *mostly* read-only (e.g. `slack-ro`, whose upstream registers channel join/leave and usergroup edits unconditionally) is held to its read tools; the list is an allowlist, so a tool added by a server upgrade stays hidden until it is listed. Enabling a server that is already on under a different (or no) allowlist removes and re-adds it (`slack-ro (tool allowlist updated)`), which is how sessions that enabled it before the flag was set get narrowed. The reverse does not happen: removing a server's entry leaves sessions at their narrower list until `/mcp disable` and `/mcp enable`. Nothing narrows a session that nobody re-enables, since reactivation and honkd restarts do not touch extensions, so after adding an entry re-enable the server in sessions that already have it. A server name missing from the catalog restricts nothing; honkd logs a warning for it at startup. An empty or malformed value is a startup error, since goose reads an empty `available_tools` as "every tool".
- **Unsupported servers**: Remote MCP servers that need OAuth or headers (such as Datadog or Notion) are listed as `n/a` (or fail with the OAuth message above) and cannot be enabled in v1.

---

## Undo and Redo (`/undo`, `/redo`)

`honk chat` supports rewinding conversation turns via `/undo` and restoring them via `/redo` (also available via `:undo` and `:redo` in command mode):

- **Soft revert & multi-view hiding**: Running `/undo` hides the undone turn in every attached `honk chat` viewer (as well as tail views). `honkd` writes a new log generation and broadcasts `_honk/reset` followed by log replay, immediately updating all viewers without needing client-side transcript filtering.
- **Composer recall**: On a successful undo, the undone prompt text is placed back into the composer if the composer is currently empty, allowing you to easily edit and re-submit your prompt.
- **`/redo` restores until the next prompt**: An undone turn is preserved in a sidecar and can be brought back with `/redo`. However, the revert is soft: as soon as the next prompt reaches the session from **anyone** (a prompt typed in `honk chat`, an incoming reply typed in its pigeon channel, a stall-watch wake, or a swarm message), `honkd` commits the truncation upstream, deletes the redo sidecar, and permanently drops the undone turns.
- **Undo stops**: Only your own human prompts serve as undo stops. Automated prompts (such as stall-watch coordinator wakes, swarm messages, or prompts that goose writes itself) are skipped and are undone or restored together with the human prompt they follow.
- **Slash commands are not stops**: Slash commands (such as `/compact`, `/mcp`, `/turns`, etc.) run as client actions rather than session prompts and do not create undo stops.
- **Busy state refusal**: Both `/undo` and `/redo` require the session to be idle. They are refused locally (and by `honkd`) while a turn is actively running, a prompt is in flight, a permission request is awaiting answer, or lines are queued or held.
- **Compaction guard**: You cannot undo past a compaction. If a manual or automatic compaction occurred at or after the target prompt, undo is refused.
- **Files are not restored**: `/undo` and `/redo` rewrite session conversation history and model context only. Any files created, modified, or deleted by tools during the undone turns remain unchanged on the filesystem.
- **Patched goose requirement**: Undo requires `honkd` connected to a goose instance advertising the `truncateFromMessage` capability flag (goose `patched-1.54.0.10` or later).
- **Wire protocol**:
  - `_honk/undo {sessionId}`: Returns `{ "turns": <count>, "text": "<target prompt text>", "messageId": "<target id>" }`.
  - `_honk/redo {sessionId}`: Returns `{ "restored": <count>, "cleared": <bool> }` where `cleared` is true when all undone turns have been restored and no further redo state remains.
  - `_honk/notice {sessionId, level, message}`: Ephemeral notification sent to honk-aware viewers (not logged in the session log), such as warning when a revert commit could not be made permanent upstream before prompt post.

### `_honk/oneshot`

One model call outside any session's conversation, used by `/quote`'s trim. Request
`{system, prompt, effort?, timeoutMs?, oneshotId?, tagFrom?, tag?}`; reply `{text, model}`, or error `-32800`
when cancelled. honkd opens a side connection to goose, creates a hidden session with no
extensions in an empty directory under its state dir, replaces goose's base system prompt with
`system` (goose still appends its hints), sets `thinking_effort` if given, prompts, collects the
agent's text, and archives the session (`_goose/unstable/session/archive`; `session/delete` would
also delete its usage records, which session-tags counts spend from). With `--session-tags-bin`
and `tagFrom` (a goose session id), honkd copies that session's tag onto the new one
(`session-tags which goose:<tagFrom>`, then `set <tag> goose:<new>`); an `auto:` tag is not
copied. An explicit `tag` is set instead of `tagFrom`'s. No model is set, so goose's configured default runs it.
`timeoutMs` defaults to 90 s and is capped at 300 s. `_honk/oneshot/cancel {oneshotId}` cancels
a call made on the same connection, and so does closing the connection. A session left behind by
a honkd that died mid-call is archived at the next start.

### `_honk/quote`

`/quote`'s pick and trim, shared by honk chat and pigeon's Slack `/quote` (honk-9qf.31.7).
Request `{sessionId, pick: {kind: "since"} | {kind: "prev"} | {kind: "last", n}, full?, tag?,
quoteId?}`. honkd reads the session's log (`<state>/sessions/<sid>.jsonl`) line by line, without
loading an idle session into goose, rebuilds the turns as honk chat's transcript would, picks
the replies with the fallbacks above (only one prompt of yours: count from it; none: the last
3), and unless `full`, trims them with one `_honk/oneshot` call per batch of about 100 KB,
four at a time. A reply still streaming (the session is running) is marked in progress and
never sent to the model. The last 16 fully good trim answers are remembered, so the same
replies again cost nothing. `tag` names the session-tags tag the model calls are charged to;
without it, a stall-watch coordinator's program (read from the working directory of a session honkd holds; a coordinator honkd has let go of is charged to its own tag), else the quoted session's tag. `never_spoke` and `one_prompt` are not returned today, since honkd falls back instead (counting from your only prompt, or the last 3 replies); a session with nothing to quote is `no_replies`. Use a fresh `quoteId` per request: a second request with the same id on the same connection cancels the first.

Reply:

```jsonc
{ "summary": "# honk: trimmed by claude-opus-5-5, kept 12 of 80 lines from 3 of 5 replies",
                                       // null when untrimmed
  "replies": [{ "header": "# 11-24 13:05 (wake: mail) · ask",   // as the composer shows it
                "passages": [["line", "", "line"], ["line"]],    // verbatim, unquoted
                "inProgress": false }],
  "notices": ["trimmed 5 replies since your last prompt to what needs you"],
  "warnings": [],                      // e.g. trim failed, quoted them all, untrimmed
  "empty": null,                       // or no_replies | never_spoke | nothing_since
                                       //    | one_prompt | nothing_kept (reason in notices)
  "model": "claude-opus-5-5" }
```

honk chat's composer text is the summary, then each reply's header and its passages as `> `
lines with `# …` between passages, blocks separated by a blank line. An unknown session or bad
params is error -32602; `_honk/quote/cancel {quoteId}` (or closing the connection) cancels it
with -32800, and each model call still archives its session.

---

## Chat channels via pigeon

If you run a [pigeon](https://github.com/johnnymo87/pigeon) daemon, `honk chat --new` can register the new session with it so the session gets a pigeon chat channel (a Slack channel):

```bash
export HONK_PIGEON_URL=http://127.0.0.1:4731          # or --pigeon-url
export HONK_PIGEON_TOKEN_FILE=/path/to/pigeon-token   # or --pigeon-token-file; only if the daemon requires auth
honk chat --new                  # registers in the background
honk chat --new --no-pigeon      # skip it for this session

# Register a session that already exists (cwd and title come from honkd's session list)
honk pigeon 20261016_7
```

- Registration is off unless a pigeon URL is configured. It runs in the background; the outcome is shown as a notice, and a failure is only a warning, never a reason the chat stops.
- honk calls pigeon's `POST /goose/sessions` with the goose session id, the honkd URL (as `ws://` or `wss://`), the working directory, and the honkd secret honk connected with (pigeon stores it and presents it to honkd; it refuses messages typed in a session's channel when it stored no secret for it). pigeon creates a channel only when it sends a notification, so honk then posts one short announcement (`POST /stop`), which creates the channel.
- Replies typed in the channel prompt the session, and pigeon posts those turns' results there. Prompts and replies from every client (honk chat included) appear in the channel when honkd runs with `--turn-hook-url` pointing at pigeon's `/goose/turn-event` (see [docs/turn-hook.md](turn-hook.md)).
- Attaching with `honk chat <id>` never registers (it would re-announce on every attach); use `honk pigeon <id>`. `honk pigeon` refuses a session pigeon already has, because registering again overwrites pigeon's record of it (including the token of a session pigeon launched); `--force` overrides.
- The bearer token and the honkd secret travel in the request, so keep the pigeon URL on loopback or use `https://`.
- The honkd URL is sent as honk sees it, so pigeon must be able to reach honkd at the same address (normally both on localhost).

---

## Tagging with oc-tags

`honk chat --new` tags the new session in oc-tags as `goose:<id>`, mirroring `opencode-launch`.

- **Explicit tag**: `honk chat --new --tag <tag>` tags the session with that tag explicitly.
- **Inheritance**: Without `--tag`, `honk chat --new` automatically inherits the launching session's tag if one is active. The launching session is resolved from `$OPENCODE_SESSION_ID`, or `$AGENT_SESSION_ID` (as `goose:<id>`), where `$AGENT_SESSION_ID` comes from goose's developer shell and `goose term init` shells. When both are set, `$OPENCODE_SESSION_ID` takes precedence (opencode-launch reads only `$OPENCODE_SESSION_ID`; honk adds the goose fallback). If the launching session has an explicit session tag, the new session is tagged with it and the chat notice notes `(inherited from <root>)`. If no launcher variable is set, or if the launching session has no explicit session tag, nothing is tagged. A failed inheritance shows a one-line note, except when oc-tags simply isn't installed and `$HONK_OC_TAGS_BIN` is unset (which skips silently).
- **Opt-out**: `--tag auto` (case-insensitive) opts out of inheritance and tagging. `auto:` tags remain reserved and cannot be set explicitly.
- Tagging runs in the background after the session exists. For an explicit tag, `oc-tags set` is bounded to 10 seconds; for inheritance, `oc-tags which` is bounded to 4 seconds followed by 10 seconds for `oc-tags set` (up to 14 seconds total). The outcome is a `Tag:` notice; a failure is a warning that includes the command to apply the tag by hand, never a reason the chat stops.
- An explicit tag is checked before the session is created: a letter or digit, then up to 63 letters, digits or `._:/-`. `auto:...` prefixes are refused (reserved for derived tags). oc-tags stores tags lowercased.
- Quitting right after `--new` waits up to 3 seconds for tagging (and pigeon registration) to finish. An in-flight `oc-tags set` finishes on its own. An inheritance still looking up the launcher's tag is abandoned, and the warning says how to apply it. Warnings that arrive after the chat screen has closed are printed to stderr.
- `--tag` requires `--new`. Attaching to an existing session never tags. To tag an existing session, run `oc-tags set <tag> goose:<id>`.

---

## Headless Launch (`honk launch`)

`honk launch <dir> <prompt>` starts a goose session headlessly through honkd, tags it, registers it with pigeon (pigeon chat channel), sends the initial prompt, and exits while the turn runs in the background. It is the goose analogue of `opencode-launch`.

### Command-Line Flags

- `--title <title>`: Session title, set before the first prompt (also sets the pigeon channel title).
- `--model <id>`: Initial model for the session, set before the first prompt. Refuses IDs not present in honkd's model list (configured via honkd `--models-file`), aborting before prompting. If setting the model fails, the launch aborts before prompting with a partial exit code and reports the created session ID.
- `--effort <level>`: Thinking effort for the new session (`off`, `low`, `medium`, `high`, `max`), set after the model step before the first prompt. If setting the thinking effort fails, the launch aborts before prompting with a partial exit code and reports the created session ID. Default: goose-serve's `GOOSE_THINKING_EFFORT`.
- `--tag <tag>`: Tag the session in session-tags (`goose:<id>`). Defaults to inheriting the launching session's tag; `--tag auto` opts out.
- `--no-attach`: Do not auto-attach (do not open a tab running `honk chat`).
- `--tmux-session <name>`: Target tmux session for auto-attach [default: nvim's session].
- `--pigeon-url <url>`: pigeon daemon URL [default: `http://127.0.0.1:4731`].
- `--pigeon-token-file <path>`: File holding pigeon's bearer token.
- `--url <url>`: honkd WebSocket or HTTP endpoint [default: `http://127.0.0.1:4280/acp`].
- `--secret <token>`: honkd secret auth token.

### Auto-Open / Auto-Attach

After sending the prompt, `honk launch` auto-opens the new session in a new nvim/tmux tab running `honk chat <id>`:

- **Mechanism**: Spawns `oc-auto-attach [--tmux-session <name>] goose:<id>` in its own process group with stdin null and stdout/stderr appended to `/tmp/oc-auto-attach.log` (overridable via `HONK_AUTO_ATTACH_LOG`).
- **Target session**: Passes `--tmux-session <name>` if provided; otherwise the tab opens in tmux session `main`.
- **Timing**: `oc-auto-attach` waits up to `OC_AA_GOOSE_GRACE_SECS` (default 10) for goose to list the new session, which it does only once the first user message is persisted (honk-9qf.9). `started` means the spawn succeeded, not that a tab opened; why one did not is in the log.
- **Automated launchers should opt out.** An open tab is a viewer, and honkd never idle-evicts a session with a viewer, so every auto-opened session stays loaded in goose serve until its tab is closed. The stall-watch goose worker recipe sets `HONK_LAUNCH_NO_ATTACH=1`, as opencode workers pass `--no-attach`.
- **Environment scrubbing**: `OPENCODE_SESSION_ID` and `AGENT_SESSION_ID` are scrubbed from the spawned process environment so a new tmux server will not capture the launcher's session ID.
- **Binary resolution**: Runs `$HONK_AUTO_ATTACH_BIN` if set and non-empty, otherwise resolves `oc-auto-attach` from `$PATH`. If not found, auto-attach is skipped with a stderr message.
- **Opting out**: Pass `--no-attach` or set `HONK_LAUNCH_NO_ATTACH` to a truthy value (`1`, `true`, `TRUE`, `yes`, `YES`, `on`, `ON`). Other values like `0` do not disable auto-attach.
- **Status reporting**: Exactly one stderr line reports the outcome:
  ```
  honk launch: auto-attach: started
  honk launch: auto-attach: skipped (--no-attach)
  honk launch: auto-attach: skipped (HONK_LAUNCH_NO_ATTACH=1)
  honk launch: auto-attach: skipped (oc-auto-attach not found)
  honk launch: auto-attach: skipped (spawn failed: <err>)
  honk launch: auto-attach: skipped (prompt refused)
  ```
- **Robustness**: Auto-attach is best-effort; a spawn failure never fails the launch. If the launch fails before a session is created, auto-attach is not reported.

---

## Copying Text

- **Terminal Selection**: Mouse capture is intentionally disabled. Native terminal mouse selection (click and drag, Shift-drag in certain terminals) and standard copy shortcuts (`Ctrl-Shift-C`, `Cmd-C`) work out of the box.
- **Mouse Wheel**: Alternate-scroll mode (`\x1b[?1007h`) is enabled so mouse wheel gestures scroll the viewport smoothly.
- **OSC 52 Yank (`:yank`)**: Copies the complete markdown text of the most recent assistant message directly to your system clipboard using standard ANSI OSC 52 sequences (refused with a warning notice if base64 size exceeds 100 KB).
- **Dump (`:dump`)**: Exports the complete formatted session markdown to a temporary file and launches `$PAGER` or `$EDITOR` (e.g. `less -R`) with the terminal suspended and event reading paused.

---

## Tmux Configuration

If you run `honk chat` inside `tmux`, consider the following settings in `~/.tmux.conf`:

### Shift-Enter for Newlines
By default, tmux may intercept `Shift-Enter` and report it as a plain `Enter`. To allow `Shift-Enter` to insert newlines:
```tmux
# Enable extended keys (tmux 3.2+ with compatible terminal)
set -s extended-keys on
```
*(Note: If your terminal does not support extended keys, you can always use `Alt-Enter` or `Ctrl-J` to insert newlines).*

### OSC 52 Clipboard (`:yank`)
To enable OSC 52 clipboard passthrough from tmux to your outer terminal:
```tmux
set -g set-clipboard on
```

---

## Known Limitations (v1)

- **No Interactive Permission UI**: Permissions on own turns are auto-approved (`allow_once`); foreign turns require their owner or manual `/approve`.
- **No Elicitation Form UI**: Elicitation prompts are auto-declined.
- **Duplicate User Line after Log Rebuild**: Following a `honkd` state rebuild from export, a user message may appear duplicated due to echo-reconciliation differences.
- **Per-Session Extension Toggles**: Platform extensions (`--ext`) are selected at creation time; catalog MCP servers can be dynamically toggled per session via `/mcp`.
