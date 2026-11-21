# Model-trimmed `/quote` (honk-9qf.31.4)

Jonathan (2026-11-21): "a smarter version of /quote that uses our default model (right now
Opus 5.5) to go through all the content that /quote offers, and boils it down into the stuff
that I likely need to respond to (or just to see), all quoted so that it's ready to go for me
to quote back with comments".

## Names

- `/quote` (and `:quote`): the smart one. Input is what plain bare `/quote` gave: every reply
  since his last prompt (cap 50).
- `/quote all`: the old bare `/quote`, every reply since his last prompt, untrimmed.
- `/quote N`: unchanged, the last N replies untrimmed.

## Verbatim by construction

honk numbers the replies R1..Rk and the lines of each reply L1..Ln and sends them with his
last prompt (context, not quotable). The model answers JSON only:

```json
{"keep":[{"reply":1,"all":true,"tag":"ask"},{"reply":2,"lines":[[3,5],[9,9]],"tag":"fyi"}]}
```

honk validates, clamps and merges ranges, widens them to whole fenced code blocks, and copies
the kept lines from the source. The model writes no text that reaches the composer. Lines honk
adds are `#` lines, as plain `/quote` headers already are:

- first line `# honk: trimmed by the model; kept X of Y lines from A of B replies (/quote all for everything)`
- per reply `# <time> (<trigger>) · <tag>` (`tag` is an enum: ask, decision, blocker, fyi)
- `# …` between non-adjacent kept ranges of one reply

## When the model is skipped

- Input ≤ 4000 bytes: quoted in full at once, with a notice.
- The running reply is never sent; it is appended untrimmed as in plain `/quote`.
- Call error, timeout (90 s), unparseable or invalid answer: plain `/quote all` plus a warning.
- Model keeps nothing: info notice, composer untouched.
- One-entry cache keyed by the input, so re-running after a cancel or a cleared composer is free.

## Model call: `_honk/oneshot` in honkd

`{oneshotId, system, prompt, effort?}` → `{text}`. A spawned task opens a side upstream
connection (orphan-recorded, always DELETEd), `session/new` with
`_meta {hidden:true, sessionTitle, enabledExtensions:[]}` in an empty temp cwd,
`system-prompt/set` (mode set), `thinking_effort`, `session/prompt`, collects
`agent_message_chunk` text, then `session/delete`. No model is set, so goose's configured
default model is used. The session id is persisted in honkd state before the prompt and swept
(deleted) at startup if honkd died. `_honk/oneshot/cancel {oneshotId}` and viewer disconnect
send `session/cancel` and clean up. No goose fork change.

## Client

`UiCommand::Oneshot` / `CancelOneshot` → driver → ChatCore request → `UiEvent::OneshotDone`.
While pending: notice, status bar `[trimming Ns]` (render tick redraws), first Ctrl-C or Esc
cancels the trim and is consumed (does not interrupt the run). The result loads the composer
like plain `/quote`, ahead of any draft present on arrival.

## Out of scope

Sentence-level selection of very long lines; spend-tag attribution of the side session.
