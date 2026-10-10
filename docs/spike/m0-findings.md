# honk M0 findings

Status: in progress. Each probe section records the question, verdict, decision-rule
outcome, and fixture evidence.

## P1: Is goose's agent loop good enough? (human day)

Rubric drafted by the agent on the author's delegation (2026-10-10); **author to review
before running the day**. The comparison baseline is a typical OpenCode v1 day
(~48 root sessions, Opus-class model, allow-all permissions, frequent subagents,
self-compaction via a tool).

Run: one working day; route every root session that would have gone to OpenCode
through goose instead (`goose session` / pigeon launch), same model family. Keep a
one-line note per session in the table below.

| # | Criterion | How measured | Pass | Fail |
|---|---|---|---|---|
| C1 | Task completion | Sessions where goose finished the task without the author taking over in OpenCode | ≥ 80% of sessions | < 65% |
| C2 | Edit/shell/read tool reliability | Count of tool failures the agent could not self-recover (bad patch, wrong file, hung shell) | ≤ 1 per 10 sessions | ≥ 3 per 10 |
| C3 | Long-session survival | Any session past one compaction stays on-task (no lost goal, no repeated work) | All observed | Any session derails after compaction |
| C4 | Subagent delegation | Reviewer / explorer style subagents run and return usable results | Works for the 3 most-used roles (adversarial reviewer, explore, implementer) | Any of those three unusable |
| C5 | Cost per session | Day's spend ÷ sessions, vs. the OpenCode daily average | ≤ 1.3× | > 2× |
| C6 | Per-session MCP enablement | Enable Slack/Atlassian for one session without affecting others | Works | Not possible without global config edits |
| C7 | Skills & instructions | AGENTS.md / skills are discovered and followed | Followed in normal use | Ignored or need per-session pasting |

**Kill condition:** two or more criteria in the Fail column, or any single
criterion the author judges a blocker. Middle-band results are recorded as
follow-ups (patch, config, or recipe), not as failures.

| Session | Task | Outcome | Notes (criteria hit) |
|---|---|---|---|
| | | | |

**Verdict:** _pending_

## P2: HTTP disconnect, reattach window, frame loss
_pending_

## WS check (Task 5)
_pending_

## P3: Same-connection reload mid-run
_pending_

## P4: Pending permission with no listener
_pending_

## P5: Cancel, steer, restart
_pending_

## P6: Message ids, resource cost, fixtures
_pending_

## P7: hydra-acp (optional)
_pending_

## Design amendments
_pending_
