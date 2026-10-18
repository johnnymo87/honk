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
     rebuild in `session.rs`) is built from export, which has no usage. Carry one usage
     frame into it, ordered export frames → carried usage → buffered live frames:
     activation/reactivation keep the last `usage_update` that the post-`session/load`
     drain would otherwise discard; gap rebuild carries the last `usage_update` in the
     in-memory log. A viewer attaching to an idle session therefore sees the current value
     immediately.
   - Turn hook v1 gains an optional `usage: {used, size, cost?}` on `turn.ended` (latest
     `usage_update` in the log at turn end). Additive; omitted when unknown.
4. **pigeon** (`packages/daemon`): one formatter, `📊 45.2K tokens · 23% · $1.84`
   (percent omitted when size is 0; cost omitted when absent or 0; nothing when used is 0),
   appended as `\n\n<footer>` to the turn message by `renderHookTurn` (honkd hook turns)
   and by the goose session runner (pigeon-started turns; it records the last
   `usage_update` seen during the turn, which it currently drops).

Model name in the footer is deferred (needs configOptions tracking in honkd and the
runner); see follow-up bead.

## Deploy (author-gated)

honk: rebuild/redeploy `honkd` and `honk` from the merged rev (workstation pin bump);
restarting honkd is safe for idle sessions and interrupts nothing that a reattach does not
recover, but is still the author's call. pigeon: redeploy the daemon from the merged rev.
Exact steps and verification live in the PRs and on the bead.
