# `/quote N`: load the coordinator's last N replies into the composer

Bead: honk-9qf.31.2 (under epic honk-9qf.31, message-history study:
`docs/plans/2026-11-19-message-history-study.md`).

## Why

Jonathan, 2026-11-19: "in coordinator sessions, i'd like the ability to load the last N turns
of transcript into the editor ... i look at all the messages i've missed, and then quote bits
of their transcript back at them and reply to it with what i want to happen. in counting
turns and capturing them for the editor, i want to exclude the stall watcher wake messages
and any messages from other swarm members, ie i just want the coordinator's replies."

Decisions he made: the text goes into honk's own vim composer (no `$EDITOR` round-trip); the
composer can be maximized with a `zm` toggle that `/quote` turns on; every turn in which the
coordinator wrote reply text counts, whoever started it; only the coordinator's words are
included.

## Behaviour

- `/quote [N]` typed in the composer, or `:quote [N]` on the command line. N defaults to 3,
  must be a positive integer.
- Turns are segmented by user blocks: a turn is a user block plus every block after it up to
  the next user block (blocks before the first user block form a turn with no trigger).
- A turn's reply is the trimmed text of its assistant (`Text`) blocks joined by a blank line.
  Thoughts, tools, notices and the user block itself are excluded. Turns with an empty reply
  do not count.
- The trigger label comes from the user block's text:
  - starts with `[sw wake ` -> `wake: <reason>` (the `reason=` token, `wake` if absent)
  - starts with `<swarm_message ` -> `scheduled` if it has a `scheduled_for=` attribute (a
    self-scheduled wake), else `swarm: <from>` (the `from="..."` attribute)
  - otherwise -> `you`
  - blocks before the first user block -> `start`
- Turns whose trigger is exactly `/compact` (goose answers "Compaction complete" as assistant
  text) are skipped.
- If a run is active, the last turn's header says `in progress`, since its reply may be partial.
- Output, oldest of the N first, turns separated by a blank line:

  ```
  # 11-19 14:32 (wake: heartbeat:settle)
  > reply line 1
  >
  > reply line 3
  ```

  The time is local, from `_meta.goose.created` on the turn's first assistant block; omitted if
  absent.
- Composer: `/quote` replaces the composer (its text was the command). `:quote` keeps an
  existing draft below the quotes, separated by a blank line. Cursor goes to (0,0), Normal
  mode, composer scroll 0, maximized on.
- Fewer than N replies: insert what exists plus an info notice ("only K replies"). None: a
  warning notice, composer untouched (for `/quote`, the command text is cleared).
- `/quote` is never sent upstream and not recorded in prompt history; history recall state
  (`history_index`, `history_stash`) is reset so Ctrl-N cannot overwrite the quotes.
- Intercept rule (matches `core.rs:241` + `cmd.rs:7`): `t = text.trim();
  !t.starts_with("//") && (t == "/quote" || t.starts_with("/quote "))`; same for `/yank`.

## Maximized composer

- `zm` in Normal mode toggles `App.composer_maximized`.
- When maximized the composer height is
  `area.height - 1 (status) - command line (0/1) - 3 (transcript rows)`, at least 1.
  When not, behaviour is unchanged (`Rendered::height()`, 1..=8), also clamped to the same
  available height.
- Reset (unmaximize) on Submit (prompt sent) and on Ctrl-C clearing the composer.

## Also

- `/yank` in the composer works like `:yank` (docs/chat.md already documents it; `cmd.rs`
  had no branch). Intercepted in `App` like `/quote` because both need the TUI transcript.
- `/help` lists `/quote` and `/yank`.

## Implementation

1. `Block.created: Option<i64>`: set on block creation from `update._meta.goose.created`
   (first chunk wins; also later if still None). Both `Block {..}` literals updated.
2. New `crates/honk/src/tui/quote.rs`: `classify(&str) -> Trigger`,
   `build(&[Block], n, running, fmt_time: impl Fn(i64) -> Option<String>) -> Option<Quoted>`
   (formatter injected so tests do not depend on the host TZ; CI is UTC), and
   `local_time(i64)` via `libc::localtime_r` with a null check (libc added as a direct dep;
   already in Cargo.lock).
3. `keys.rs`: `Action::ToggleMaximize` for `z m`.
4. `app.rs`: `composer_maximized`; layout in `draw()`; intercept `/quote`, `/yank` in
   Submit; `:quote` in `execute_app_command`; `insert_quote(n, keep_draft)`.
5. `cmd.rs` help text; `docs/chat.md` key map and command table.

## Tests

- quote.rs: classify (wake with reason, wake without, swarm with from, plain, leading
  whitespace); build over fixture blocks: wake/swarm/you turns, a silent turn not counted,
  multi-text-block turn joined, missing `created` header, N larger than available, pre-first-user
  assistant text, empty lines quoted as `>`.
- keys.rs: `zm` resolves to ToggleMaximize.
- app.rs: `/quote 2` submit fills composer, Normal mode, maximized, nothing sent on cmd_tx;
  `:quote` keeps the draft below; no replies -> notice, nothing sent; maximized height via
  TestBackend; Submit resets maximized; `/yank` sends nothing upstream.

## Review (adversarial-reviewer-opus, 2026-11-19)

Verified against all 155 honkd logs: `created` on every assistant chunk; Text chunks contiguous
per messageId; one user chunk per message; every live run preceded by a user block; Submit is
the only composer path upstream. Folded in: injectable time formatter, exact intercept rule,
history reset, `/compact` skip, `scheduled` label, `in progress` header. Not changed (UX he
picked): Enter in Insert still submits even when maximized; edtui undo can bring back the
pre-quote text (Ctrl-R recovers).
