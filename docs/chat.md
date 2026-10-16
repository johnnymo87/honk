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

# Restrict loaded platform extensions (repeatable)
honk chat --new --cwd /path/to/project --ext developer --ext summon
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
   - **Thinking blocks**: internal model reasoning, displayed dim and collapsed to a single line by default (`zt` toggles full visibility).
   - **Tool calls**: compact status row displaying the glyph, title (formatted with recognizable command/path details such as `shell · ls` or `write · notes.md`), and live tool notification output (`za` toggles full input/output view). Tools ending in `final_output` (e.g. `recipe__final_output`) render as a dim one-liner `◆ final output` without dumping duplicate inputs.
   - **Honk notices**: informational and warning notices (e.g. queue state, extension warnings).

2. **Status Bar** (single row):
   - Displays session title (or session ID), working directory (`cwd`), run state (`idle`, `running`, `running (other client)`, or `queued: N`), and connection status (`live`, `reconnecting…`, or `stopped`).

3. **Composer / Command Bar** (bottom):
   - Multiline editor with modal vim navigation.
   - Long lines wrap at the screen edge. The composer grows with the number of wrapped rows,
     up to 8; taller input scrolls to keep the cursor visible.
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
| `z t` | Toggle thoughts expansion (expand/collapse model reasoning blocks). |
| `z j` | Toggle JSON payload expansion (expand/collapse structured user messages). |
| `:` | Open command line bar. |
| `Ctrl-P` / `Ctrl-N` | Recall previous / next prompt history. |
| `Ctrl-C` | If idle and composer has text: clears composer. Otherwise: interrupts active turn (`Action::Interrupt`). |
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

## Extension Handling on `--new`

When creating a new session with `honk chat --new`:
- By default, `honk chat` does **not** restrict extensions via `enabledExtensions`. Goose loads every extension enabled in the host configuration (`$HOME/.config/goose/config.yaml`), providing full developer tools out of the box.
- Specifying `--ext <NAME>` (e.g. `--ext developer --ext summon`) restricts loaded extensions strictly to those specified.
- Loaded extension summaries and any failure reports are presented as notices in the transcript.

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
- **Model Switching**: Mid-session model switching is not supported from the TUI.
- **Per-Session Extension Toggles**: Extensions are selected at creation time and cannot be dynamically toggled during a session.
