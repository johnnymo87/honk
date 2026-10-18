# Context-window usage readout (honk chat + pigeon Telegram)

Bead: `honk-9qf.15.9` (children `.1` honk, `.2` pigeon, `.3` workstation deploy).

**Goal.** Show how full a goose session's context window is (tokens used, percent of the
model's limit, cumulative cost when known) in the honk chat status bar and at the end of
pigeon's Telegram turn messages, in the format the author already reads in opencode.

## Reference behaviour (opencode v1.18.18, pigeon opencode plugin)

- opencode TUI, row under the prompt, right side, muted:
  `45.2K (23%) · $1.84`. Tokens are the last assistant step's prompt + completion,
  `<1000` raw, else one decimal `K`/`M`; percent `round(used/limit*100)`, omitted when the
  limit is unknown; cost is the session total in `Intl` USD, omitted when 0. Hidden until
  there is a step with tokens.
- pigeon's opencode plugin appends to the Telegram body, after the assistant text and
  before the daemon's `📂`/`🆔` footer: `📊 45.2K tokens · 23% · 🧠 <model>` (no cost).

## Data source (goose-patched 1.54.0, verified in source and honk fixtures)

`session/update` `{"sessionUpdate":"usage_update","used":N,"size":L,"cost":{"amount":A,"currency":"USD"}}`

- `used` = total tokens of the last provider request (= current context). Overwritten on
  every call; compaction sets it to the retained size; `/clear` sets it to 0.
- `size` = model context limit. `cost` = cumulative session cost incl. subagents; absent
  when no pricing is known.
- Emitted after every provider call, again at end of turn just before the `session/prompt`
  reply, and after `session/new|load|fork` (after the reply, on the session stream). Not
  emitted at the instant of compaction: the next provider call (auto and self-compaction,
  `honk-9qf.16`) or the end-of-turn update (manual `/compact`) carries the drop.
- The prompt reply's `usage` has no `size`; not used.

No goose patch is needed.

## Design

1. **honk-wire**: `Usage { used, size, cost: Option<Cost { amount, currency }> }` and
   `usage(update) -> Option<Usage>`; shared by honkd and honk.
2. **honk chat** (`crates/honk/src/tui`): the transcript tracks the latest `usage_update`
   through the same path for live and replayed frames; on `_honk/reset` the rebuilt
   transcript's value wins, falling back to the old one if the new generation had none.
   The status bar gets a right-aligned segment `45.2K (23%) · $1.84` (opencode format;
   non-USD cost as `1.84 EUR`). Hidden when `used == 0`; percent omitted when `size == 0`;
   percent may exceed 100%. When narrow, the title truncates first, then the cost drops,
   then the percent, then the whole segment.
3. **honkd**:
   - Every new log generation (activation `hub.rs`, goose-restart reactivation and gap
     rebuild in `session.rs`) is built from export, whose text/tool frames carry no
     `usage_update`. Carry one usage frame into it, ordered export frames → carried usage →
     buffered live frames: gap rebuild carries the last `usage_update` of the in-memory log;
     activation/reactivation keep the last `usage_update` the post-`session/load` drain
     would otherwise discard (a safety net: real goose sends it after the load reply, so it
     normally arrives later and is logged the ordinary way). The carried frame's `used` is
     replaced by the export's `usage.total_tokens` (the same value goose sends as `used`),
     so a gap that swallowed a compaction or `/clear` does not resurrect the old figure;
     `size` and `cost` stay from the frame (export cost excludes subagents). A viewer
     attaching to an idle session therefore sees the current value immediately.
   - The session actor keeps `last_usage`, updated from every session-stream
     `usage_update` (including ones buffered during a rebuild). Turn hook v1 gains an
     optional `usage: {used, size, cost?}` on `turn.ended` from it. Additive; omitted when
     unknown. Only `session/update` frames count (goose's custom
     `_goose/unstable/session/update` variant is ignored).
4. **pigeon** (`packages/daemon`): one formatter, `📊 45.2K tokens · 23% · $1.84`
   (percent omitted when size is 0; cost omitted when absent or 0; nothing when used is 0),
   appended as `\n\n<footer>` to the turn message by `renderHookTurn` (honkd hook turns)
   and by the goose session runner (pigeon-started turns; it records the last
   `usage_update` seen during the turn, which it currently drops).

Model name in the footer is deferred (needs configOptions tracking in honkd and the
runner); see follow-up bead.

## Deploy (author-gated)

Independent order: old pigeon ignores the new hook field; new pigeon renders nothing when
it is absent.

- **pigeon** runs from the `~/projects/pigeon` checkout (`tsx`): after the PR merges,
  `git -C ~/projects/pigeon pull --ff-only` on `main`, then `sudo systemctl restart
  pigeon-daemon`. Verify: `systemctl status pigeon-daemon`, then a turn in a registered
  goose session's topic ends with `📊 … tokens · …%`.
- **honk** (workstation PR bumping the `honk` flake input): `home-manager switch` updates
  the `honk` CLI immediately; honkd is `X-SwitchMethod=keep-old`, so run `systemctl --user
  restart honkd` in a quiet window (no goose turn running). Verify: `systemctl --user status
  honkd`; open `honk chat <sid>` on an idle session and the status bar shows
  `NN.NK (NN%) · $N.NN` on attach, before any new turn; after a honk-chat turn the topic's
  mirrored reply ends with the footer.
