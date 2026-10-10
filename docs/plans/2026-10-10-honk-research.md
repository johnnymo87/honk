Building a goose ACP TUI: feasibility, reuse, and architecture

Research snapshot: October 10, 2026 · Target: Rust, ratatui, goose v1.54, SSH/tmux, single power user

Executive assessment

A custom ratatui client is feasible, but I would not start by writing a complete client. I would prototype the session ownership layer first and evaluate Nori CLI and Hydra ACP before choosing the UI codebase.

Your proposed 5–15k lines of code is plausible for the interactive frontend. It is optimistic for a polished system that also independently implements robust detach/reattach, session multiplexing, replay, permissions, terminal interoperability, and recovery.

The most important findings so far are:

1. Nori CLI is a stronger fork candidate than your initial shortlist suggests. It explicitly separates the ratatui frontend, ACP host, and session harness, and documents incremental rendering and scrollback management. 
GitHub
+1

2. Hydra ACP already implements much of your hardest architectural requirement. Its daemon owns live agent connections, broadcasts updates to multiple clients, serializes prompts, and arbitrates permissions. Its transport also defines session/attach and session/detach. This is unusually close to your existing OpenCode serve-pool workflow. 
smagnuso/hydra-acp · GitHub
+1

3. ACP session resume is progressing, but it does not automatically solve live turn reattachment. The specification now documents session/resume, including a v2 draft with a replay cursor, but reconnecting to an existing conversation is not the same as preserving an in-flight prompt whose underlying request is owned by a disconnected client. 
GitHub
+1

4. The composer is less risky than it would have been a year ago. The maintained ratatui-textarea fork gained Unicode-aware soft wrapping and visual-row cursor movement in April 2026. This eliminates two particularly irritating classes of multiline input bugs, although it does not provide your Vim command grammar. 
GitHub
+1

5. There is upstream momentum toward ACP clients, not clear evidence of a durable official Rust TUI. Goose publicly announced a TypeScript ACP TUI beta in April, but the roadmap and subsequent architectural changes need to be distinguished from an actively supported terminal product as of October. 
Your open source AI agent
+1

Effort estimate

These are engineering estimates, not observed completion times from comparable projects.

Proof of concept

20–40 h
ACP connection, prompt, streamed text, minimal editor

Usable personal TUI

80–160 h
Vim subset, tools, forms, scrollback, practical daily use

Full workflow replacement

160–350 h
Relay, multiple viewers, reconnection, robust recovery
Assumes an experienced Rust engineer, existing widgets and ACP SDKs, and deliberate omission of general-purpose product features. These ranges include integration/debugging time, not simply code generation.

My provisional preferred architecture is:

Not necessarily a separate TCP service: for one user on one machine, a Unix socket and one small resident process may be enough.

This keeps the highest-risk property—continuity of an active goose turn—independent of terminal lifetime. It also allows the frontend to be replaced without migrating sessions again.

The detailed comparison below separates what existing projects claim to support from what still needs to be tested with your goose deployment.

1. Current ACP TUI landscape

The useful distinction is between a frontend, a session harness, and a multiplexer. Most projects solve only one or two of these problems.

The candidates worth investigating

Project

	

Implementation / role

	

Reuse value

	

Main gap for you




Nori CLI

	

Rust, ratatui, ACP harness

	

High — frontend candidate

	

Goose-over-WS and live reattachment unverified




edtui

	

Rust Vim editor widget

	

High — composer

	

Not an ACP client




Hydra ACP

	

TypeScript daemon, mux, TUI

	

Very high — architecture

	

Non-Rust; goose integration needs validation




acp-mux

	

Rust ACP multiplexer

	

High — relay candidate

	

Primarily stdio subprocess-oriented




Toad

	

Multi-agent terminal frontend

	

Medium — ready-made alternative

	

UI and lifecycle fit unverified




Martty

	

Hybrid Node/Rust ratatui painter

	

Medium — rendering/reference

	

DeepSeek Harness/Cordis architecture




ed-acp-tui

	

Minimal Rust ratatui ACP client

	

Medium — protocol bootstrap

	

Single-session, subprocess-oriented




acpx

	

TypeScript headless ACP CLI

	

Medium — scripting

	

Not an interactive TUI




acpx Rust

	

Thin ACP subprocess client

	

Low–medium

	

Not a TUI or WS session manager




ACP Rust SDK

	

Official protocol and transport libraries

	

High — protocol foundation

	

No application session ownership

This is a codebase-capability shortlist, not a verified maintenance ranking. Several projects have current documentation, but I did not establish their issue-resolution rates, contributor retention, or release cadence.

Nori CLI: inspect first for a Rust frontend

Nori's documentation describes a separated architecture:

Plain text
nori-tui
    |
nori-harness
    |
nori-acp-host
    |
ACP agent

It explicitly identifies ratatui incremental rendering, double-buffered scrollback, and its own application-level lifecycle behavior rather than expecting ACP to provide everything. This is exactly the separation I'd want before adapting a client to your long-lived goose serve process. 
GitHub
+1

The decisive inspection questions are whether its ACP host assumes it launches a subprocess, whether transcript state can be fed through an externally supplied event stream, and whether its session model is genuinely independent of terminal lifetime.

If those boundaries are clean, forking Nori may save more time than extracting components from Codex. If they are tightly coupled, its architectural value remains high but its fork value decreases sharply.

Status: credible Rust foundation; goose compatibility is still a test requirement, not an established fact.

Hydra ACP: closest match to your actual workflow

Hydra is especially interesting because its README already promises functionality that would otherwise constitute most of your custom daemon project:

A persistent daemon owning agent sessions.

Multiple live subscribers to the same session.

Per-session prompt serialization.

Permission arbitration, with the first response winning.

Event broadcast, session resurrection, attach/detach, and a companion TUI.

Its documented ACP endpoint extends the standard protocol with session attachment and detachment. 
smagnuso/hydra-acp · GitHub
+1

I would test Hydra even if you are committed to Rust, because there are three possible outcomes:

Best outcome: Hydra already supports goose serve, permits your required session topology, and has acceptable persistence semantics. You implement only a bespoke ratatui frontend against it.

Middle outcome: Its daemon is useful with small goose-specific fixes. You run Node for the relay and Rust for the UI, avoiding an expensive rewrite of working infrastructure.

Worst outcome: Its assumptions about spawning agents or closing prompt connections conflict with goose's server behavior. In that case its protocol, arbitration, and event-handling design still serve as unusually relevant prior art.

Don't assume that supporting arbitrary stdio ACP agents proves compatibility with an existing shared, authenticated goose serve WebSocket. That is the first integration test.

acp-mux: the Rust alternative to Hydra

A newly discoverable project, 
acp-mux
, makes the relay decision more interesting.

Its core multiplexes a stdio ACP agent behind a WebSocket endpoint. Multiple subscribers identified by the same mux ID share an upstream agent subprocess. It also has a separate Rooms layer for peer coordination, queued turns, and shaped history replay. The project is MIT-licensed and its README describes a session/attach-style flow. 
GitHub

This is worth examining before writing a Rust daemon.

However, its documented lifetime rule—keeping an upstream agent until the last subscriber disconnects and a TTL expires—is not yet your desired lifetime rule. Your active turn must survive zero attached viewers, for as long as the daemon itself remains alive.

An existing goose serve deployment also changes the ownership model: the mux should own a persistent session/prompt connection, not necessarily spawn and kill goose.

I would borrow or fork its JSON-RPC routing machinery if the internals are straightforward, but I would make session ownership independent of subscriber count from the start.

Martty, ed-acp-tui, Toad, and acpx

Martty has real value as a renderer reference. Its architecture intentionally separates a Node-hosted agent/plugin runtime from an independently running Rust/ratatui painter; its documented primary environment is DeepSeek Harness, with generic ACP interoperability. This makes it more of a hybrid system than a straightforward Rust ACP client you could strip down. 
GitHub

ed-acp-tui is much closer to a minimal example of a typed Rust ACP client. Its published documentation describes a single session, local subprocess, file handlers, permissions, and ratatui rendering. It is appealing for learning the client API, but there is no indication that it already solves your session ownership problem. 
Rust

Toad deserves a brief binary-level test. Its stated goal is to support many ACP agents through one terminal interface, and its shell integration is substantial. But that shell capability is largely irrelevant to your needs; more important is whether you can obtain its desired editor behavior and external session attachment without bending the program's architecture. 
GitHub

There are also two distinct projects named acpx, which are easy to conflate. openclaw/acpx is a headless Node CLI for stateful agent sessions; imumesh18/acpx is a Rust library for stdio subprocess ACP connections. Neither is a ready-made ratatui frontend. 
GitHub
+1

Goose-specific adoption

I found evidence of goose's ACP endpoint being deliberately presented as a general integration surface, as well as an official VS Code integration built around goose ACP. That supports the basic interoperability premise. 
Your open source AI agent
+1

What I did not establish is a reliable collection of first-hand reports from people using goose predominantly through Nori, Hydra, Martty, or Toad in large daily session workloads.

That distinction matters. A client successfully calling initialize, session/new, and session/prompt proves far less than successfully handling permission callbacks, goose-specific configuration, history replay, long tool calls, and connection loss.

I would therefore treat a real goose compatibility smoke test as a mandatory selection gate.

2. What really drives the effort

Your LOC intuition makes sense if you separate the TUI from the application around it.

Work breakdown for a custom implementation

Subsystem

	

Estimated hours

	

What makes it difficult




ACP integration, transport, goose adaptations

	

16–32

	

Connection lifecycle, extensions, callback routing




Session/event model and persistence

	

16–35

	

Replay, event ordering, deduplication




Transcript rendering and scrolling

	

25–50

	

Wrapping, incremental rendering, scroll anchoring




Vim composer

	

8–20

	

Key handling, paste, wrapped cursor behavior




Tools, diffs, markdown, thinking

	

16–35

	

Heterogeneous content and streaming




Questions, permissions, model/settings

	

12–25

	

Async interaction state




Terminal/tmux integration

	

8–20

	

Input conflicts, resize, title, clipboard




Persistent relay and multi-client ownership

	

30–75

	

Pending RPCs, reconnect, prompt ownership




Hardening and regression testing

	

25–60

	

Long sessions, crashes, odd terminal states




Total

	

156–318

	

With substantial reuse

These estimates are deliberately not fully additive to the earlier frontend range: scope can shrink when a fork supplies multiple subsystems, and integration work can overlap with hardening.

My LOC expectation
Estimated production Rust LOC by scope

Engineering estimate, not measured codebase sizes

Lower estimate
Upper estimate
0
5K
10K
15K
20K
Basic client
Daily-driver UI
UI + relay
Basic client: Lower estimate 2K, Upper estimate 4K
Daily-driver UI: Lower estimate 5K, Upper estimate 10K
UI + relay: Lower estimate 9K, Upper estimate 18K

A tight personal project can absolutely remain below 15k LOC if it does not reproduce Codex's product architecture. The constraint that matters more is how many independent state machines you own.

For example, a 500-line JSON-RPC proxy can forward messages. A reliable ACP multiplexing service also needs to know:

Which request future owns a turn.

Which viewer is authorized to answer a permission.

Whether a reconnecting viewer has missed events.

Whether a tool invocation has reached a terminal state.

Whether the upstream connection is actually still alive.

What to do if the frontend crashes halfway through a user interaction.

Whether any operation can be safely retried.

Those are product semantics, not widget code.

War stories and evidence

I did not find a credible published set of personal-agent-TUI postmortems that would justify claiming something like "most ratatui agent TUIs take six weeks."

The strongest technical evidence is source-level evidence from mature projects.

Codex: streaming rendering is more than an incremental Markdown parser.

Codex's MarkdownStreamCollector commits newline-terminated source and keeps an incomplete tail separate. Its streaming controller also includes special treatment for tables, preventing incomplete Markdown table rows from temporarily rendering as malformed output. This is a concrete example of the sort of detail that accumulates after basic rendering already works. 
GitHub
+1

Ratatui text editing: visual-line navigation is a real subsystem.

The maintained textarea fork had to add grapheme-aware wrap modes and cursor movement along displayed rather than logical lines. That is a compelling reason not to implement the compositor's text-editing logic yourself. 
GitHub

Goose: frontend replacement and backend migration can compound each other's complexity.

Goose's May 2026 desktop discussion explicitly describes abandoning an effort that combined an ACP migration with a complete UI refresh because the combined transition made shipping quality software harder. That is not a ratatui postmortem, but it is directly relevant to the architecture risk you face. 
GitHub

The generalizable failure mode is not that ratatui takes enormous effort. It is that terminal rendering, transport lifetime, and agent session semantics are often developed together until they become difficult to change independently.

For your project, I'd insist on building the event model before any sophisticated UI.

3. Detach/reattach: the architecture I would use

This is the most important decision in the project.

Why session/load is insufficient

Based on your goose v1.54 source reading, this sequence is unsafe:

Plain text
TUI A -> goose: session/prompt
goose -> TUI A: streamed updates
TUI A disconnects
goose: drops prompting future, cancels turn
TUI B -> goose: session/load
goose -> TUI B: persisted history only

The problem is not primarily that TUI B lacks the missing streamed tokens. The problem is that disconnecting TUI A changes agent execution.

Adding local transcript persistence or a ring buffer does not fix this.

An active goose turn therefore needs a connection owner whose lifetime is independent of the TUI.

Compare the three approaches

Approach

	

Effort

	

Detach

	

Multiple live viewers

	

Judgment




Persistent TUI in tmux

	

Lowest

	

Via tmux detach

	

Only shared terminal view

	

Good initial workaround




ACP relay with persistent upstream ownership

	

Moderate

	

Native

	

Yes

	

Preferred long term




Patch goose for independent turn execution

	

Potentially highest

	

Potentially native

	

Still needs event subscription

	

Not first choice

Option A: tmux as the process supervisor

This is a serious alternative, not merely a hack.

Create one long-lived tmux window or session per goose conversation and let all access go through tmux attach, switch-client, or a dedicated control interface. Goose never sees a dropped ACP connection when the human changes tabs.

Advantages: virtually no new daemon, terminal title and scrollback already exist, and connection lifetime is naturally preserved.

But it has important limitations.

A tmux session is a persistent terminal, not a general-purpose session broadcaster. Two attached clients are viewing a shared rendering context, and independent scroll positions, focus, and input routing become awkward. External navigation to a specific message also requires UI-side control. Moving a single view between different pane sizes can cause resize and reflow surprises.

For one user who mostly views one session at a time, this may be enough. For your existing many-tab workflow, it is a reasonable fallback but not a full substitute for a relay.

Option B: a minimal resident ACP relay

This is my recommendation.

The relay should own the active prompt futures, with one logical session actor per active conversation. All TUI clients become replaceable viewers/controllers.

Plain text
       Client A         Client B         nvim integration
          |                |                    |
          +----------------+--------------------+
                           |
                       Unix socket
                           |
                      ACP relay
                  session actor registry
                           |
          +----------------+----------------+
          |                |                |
       Session A        Session B        Session C
       event log        event log        event log
       turn future      turn future      turn future
          |                |                |
          +----------------+----------------+
                           |
                  goose serve /acp

The distinction between a logical connection and a physical connection matters here. You may ultimately use one persistent physical WS connection for multiple session IDs, but you should not assume goose's callback and prompt-future ownership works cleanly with that topology until tested.

Start with one relay-owned WS connection per actively running goose session. This is simpler to reason about than one globally multiplexed connection, and it isolates connection-level failures. Once verified, connection pooling is an optimization.

At your scale, keeping perhaps several dozen idle connections is unlikely to be the dominant cost compared with active inference and tools.

Minimal relay contract

I would avoid creating a general-purpose transparent ACP proxy as the first internal abstraction.

Instead, define a small local protocol that exposes exactly what the UI needs.

Session control

Attach, detach, submit prompt, cancel, list, status, configuration

Event stream

Snapshot, sequenced deltas, turn state, tool lifecycle, pending requests

UI-specific commands

Focus message, reveal tool, follow tail, externally select session

For example:

Rust
enum ClientCommand {
    Attach {
        session_id: SessionId,
        after_seq: Option<u64>,
    },
    Detach {
        session_id: SessionId,
    },
    Prompt {
        session_id: SessionId,
        client_request_id: Uuid,
        content: String,
    },
    Cancel {
        session_id: SessionId,
    },
    AnswerPermission {
        request_id: RequestId,
        option_id: String,
    },
    ScrollToMessage {
        session_id: SessionId,
        message_id: MessageId,
    },
}

struct EventEnvelope {
    session_id: SessionId,
    seq: u64,
    payload: SessionEvent,
}

This is illustrative, not the upstream ACP schema. The important part is that the UI's protocol is stable even if goose changes an ACP type or _goose/unstable/* payload.

A ring buffer alone is not enough

I would preserve two representations.

Canonical events: goose notifications and local turn transitions, with monotonic sequence numbers, held in a bounded in-memory log and optionally appended to a journal.

Materialized transcript: messages, tool calls, thinking segments, permissions, and turn status, produced by applying canonical events.

On attach, the viewer receives a snapshot with a sequence number followed by events after that number. Snapshot publication and subsequent subscription must be atomic relative to the event stream, or you risk silently dropping a notification during attachment.

A reconnecting viewer sends its last acknowledged sequence number and receives all available missed events. If the cursor has fallen out of retention, it receives a fresh snapshot.

This is much simpler than attempting to reconstruct a live transcript from an ACP session/load replay every time a tab opens.

I would also keep an explicit idempotency key on UI prompt submission. ACP's underlying session/prompt is not inherently safe to retry just because the local socket was interrupted.

Permission handling is another ownership problem

The relay must not simply forward session/request_permission to whichever viewer happened to connect first.

You need policy for no-viewer scenarios and duplicate viewers:

With multiple active viewers, send the same pending request to each authorized viewer and accept one valid answer.

On viewer disconnect, leave the upstream request pending rather than destroying the request future.

With no viewer, retain the request and mark the session waiting_for_user.

Never automatically approve a sensitive tool action just because the user detached.

Preserve the original goose request/response association even after the UI reattaches.

Hydra's documented first-response-wins rule is a good starting point, although you should constrain it to authorized local clients. 
smagnuso/hydra-acp · GitHub

I'd distinguish waiting_for_permission from waiting_for_elicitation. They have different UI behavior and may require different response schemas.

The terminal callback trap

Your observation that advertising terminal/* causes goose to route shell execution to the client is consequential.

Do not advertise terminal capabilities in the initial version.

Otherwise you accidentally turn the TUI or relay into a shell execution backend and must handle subprocess lifetime, output streams, cancellation, PTYs, and process ownership.

Likewise, advertise filesystem operations only if you want your client to be the filesystem authority. Since goose already has access to its local environment, this might be unnecessary. If you do implement them, enforce the same root/cwd boundaries across the relay rather than trusting arbitrary client file requests.

This keeps two major categories of lifecycle complexity out of the project.

What ACP itself is moving toward

There are three relevant developments.

session/resume: Documented ACP v1 drafts and the v2 draft include resume capabilities. The v2 draft describes an optional replayFrom cursor. That is useful for reducing replay overhead, but neither feature should be mistaken for a standard many-viewer subscription model. 
GitHub
+1

ACP Conductor: The official Rust SDK includes a conductor for composing ACP proxy chains. It preserves request routing and JSON-RPC framing across intermediaries. This is useful infrastructure if you later build transparent ACP middleware, but a proxy chain does not inherently make a single active prompt survive its originating client's disappearance. 
GitHub
+1

Community multiplexers: Hydra and acp-mux already define additional attachment and multi-client behavior above ACP. Their existence suggests that the problem is sufficiently general to warrant dedicated implementations, but their extension contracts are not yet interchangeable standards. 
GitHub
+1

I would not build a general ACP conductor or v2 polyfill into your application just to prepare for the future.

When to patch goose instead

A goose patch becomes attractive if inspection reveals a small, stable internal boundary where an active turn can be owned by the server rather than by the invoking WebSocket future.

The architectural change would be:

Plain text
session/prompt
    |
spawn server-owned turn
    |
publish updates to session event bus
    |
request future waits for turn result

Then disconnecting a client would drop only its waiter, not the agent's actual work.

But this requires deliberate semantics for cancellation, persistence, recovering pending questions, and shutdown. A simple tokio::spawn that detaches the task is not sufficient.

Even after such a patch, you would still need a way to subscribe to updates and distinguish in-flight turns from idle sessions.

For your maintenance goals, it is better to own a small relay than to carry a permanent fork of goose's execution engine unless upstream wants the change.

4. Recommended ratatui component stack

Here is what I would actually choose for a Rust implementation.

Responsibility

	

Primary choice

	

Notes




Terminal UI

	

ratatui + crossterm

	

Standard foundation




Vim composer

	

edtui

	

Vim behavior already implemented




Alternative composer

	

ratatui-textarea

	

Better choice if you want custom modes




Text editing with atomic spans

	

tui-textarea-2

	

Useful for protected tokens/mentions




Markdown

	

tui-markdown

	

Styled ratatui text, optional highlighting




Syntax highlighting

	

syntect

	

Cache parsed/highlighted lines




Diff rendering

	

Custom unified-diff widget

	

Start with text, hunk headers, line styles




Transcript viewport

	

Custom message-block virtualizer

	

Required for reliable anchors




Forms/popups

	

ratatui primitives

	

Small enough to implement




Terminal interaction

	

crossterm

	

Resize, paste, keyboard events




Clipboard

	

OSC 52 plus tmux integration

	

Needs real terminal testing




ACP client

	

agent-client-protocol

	

Pin exact version




Transport

	

Official ACP HTTP/WS crate or thin WS adapter

	

Match goose's actual transport




Relay IPC

	

Unix socket, framed JSON or MessagePack

	

Avoid HTTP unless needed

Composer: edtui versus ratatui-textarea

First choice: edtui.

Its documented Vim implementation already includes much of your required grammar: i, a, A, o, O, hjkl, w, b, e, 0, $, gg, G, x, dd, dw, cw, along with mode-aware editing, undo, and configurable key handlers.

It also documents line wrapping, bracketed paste, and integration with an external $VISUAL editor. 
GitHub
+1

That's much closer to your goal than beginning with an Emacs-style textarea and recreating a Vim operator-pending state machine.

I would explicitly test these keys before committing to it:

Required behavior

	

Assessment




i a A o O

	

Documented




I S

	

Verify or add




hjkl, 0, $, w b e

	

Documented




^, W B E

	

Verify exact behavior




x dd dw cw

	

Documented




cc

	

Verify or add




Normal-mode transcript Ctrl-e/y/d/u/f/b

	

Implement outside editor




gg/G

	

Documented for editor; separate transcript handling

Do not route every normal-mode key event through edtui. Your application has two navigation contexts: composer and transcript. The editor can own the Vim editing grammar, but the shell around it must own pane focus and transcript movement.

For a chat composer, I would define focus policy explicitly: normal mode with an empty composer can use your preferred keys to scroll the transcript, while insert mode always preserves text editing semantics.

Alternative: ratatui-textarea.

The maintained ratatui fork now supports Unicode-aware wrapping and visual cursor navigation. Its normal editing defaults are Emacs-like, but it gives you an excellent text buffer/rendering basis if you want a deliberately tiny custom Vim subset. 
GitHub
+1

If edtui's wrapping or paste semantics fail your tests, I'd use textarea plus a small modal-command parser. Avoid writing the grapheme buffer and display-width calculations yourself.

tui-textarea-2 adds atomic editing ranges that are useful for mentions, tokens, and special input spans. Given that you've explicitly deprioritized @file completion, that is probably not worth making your initial dependency choice. 
Rust

Streaming Markdown without reflow jank

I would implement a simplified version of Codex's approach.

Maintain raw streaming source for the current assistant message, but distinguish between:

Completed, stable Markdown blocks.

A currently incomplete source tail.

Finished message blocks whose rendered layout is cached.

The main rule is: don't repeatedly reparse and relayout the entire transcript as new tokens arrive.

A practical pipeline:

Plain text
ACP agent_message_chunk
        |
append to raw message source
        |
parse completed newlines / blocks
        |
render stable prefix to ratatui Lines
        |
cache layout by message + width
        |
render only visible viewport
        |
render incomplete tail separately

Use tui-markdown for the initial renderer. It already supports ratatui Text and syntax highlighting through syntect. 
Rust

The tricky cases to test are incomplete fenced code blocks, inline backticks arriving across chunks, long lines without newlines, tables that become tables only after the separator row arrives, and reflow when the terminal width changes.

Codex explicitly uses newline-gated source commits and a table holdback scanner. Those are better pieces of code to study than its general widget architecture. 
GitHub
+1

I'd accept a limited amount of temporary plain-text rendering for incomplete lines. Perfectly formatting each partial token buys little, while eliminating layout oscillation is noticeable every day.

Transcript virtualization

This is the one UI subsystem I would expect to write yourself.

Ratatui can efficiently render a small viewport, but you still need a representation capable of locating message boundaries within a very large, variably wrapped transcript.

Use message blocks, not one global string:

Rust
struct TranscriptBlock {
    id: MessageId,
    kind: BlockKind,
    raw: String,
    expanded: bool,
}

struct LayoutCache {
    width: u16,
    lines: Vec<Line<'static>>,
    height: usize,
}

Only maintain rendered lines for the visible region and nearby blocks. Cache widths and invalidate on resize or presentation changes.

For efficient arbitrary jumps, index each block's rendered height using prefix sums; a Fenwick tree is justified only if measurements change frequently enough to make naive prefix recomputation expensive.

Most importantly, the scroll position should have a semantic anchor, such as (message_id, visual_line_offset), rather than only a global row offset.

That directly enables your external scroll-to-message behavior:

Plain text
nvim integration
  -> local relay: focus(message_id)
  -> TUI: locate message block
  -> TUI: scroll to anchor

When new content arrives, preserve that anchor unless the user is intentionally following the tail. Don't force-scroll someone reviewing an earlier tool diff.

Diff and tool-call rendering

You don't need a custom rich diff engine to begin with.

For tool calls, preserve the semantic structure rather than flattening everything into Markdown:

Plain text
ToolCall
  id
  name
  status
  title
  input
  output
  locations
  diff (when present)

Render a compact collapsed row by default, with expandable content and distinct styling for additions, deletions, and hunk headers.

I would use unified diffs initially, not side-by-side. Side-by-side becomes substantially worse when a tmux pane is narrow, and word wrapping makes old/new line alignment difficult.

Store the raw tool payload as well as the rendered interpretation. You will encounter formats that deserve specialized rendering later, and you don't want parsing assumptions to destroy original information.

Questions and forms

Given only roughly three form interactions daily, I would implement a simple modal from ratatui primitives, driven by the actual elicitation schema.

It needs focusable options, multiple questions, keyboard selection, submit/cancel, and preservation of partially completed answers when streaming notifications continue in the background.

Keep permission approval and arbitrary elicitation as separate widget types. Their response and safety semantics differ.

Terminal and tmux integration

A few specific details matter more than the component library.

Bracketed paste: Enable it and treat a paste event as an atomic insertion of literal text. Pasting a prompt containing newlines must not trigger submission or be interpreted as a sequence of normal-mode Vim commands.

Unicode: Test combining marks, wide CJK characters, emoji sequences, tabs, and mixed-width lines. Rendering and cursor movement must agree about cell widths.

OSC 52: The tmux wiki explains that clipboard integration requires cooperation among set-clipboard, the terminal's Ms capability, and the outer terminal. The default external setting allows tmux itself to copy outward but restricts applications inside tmux from setting the clipboard; application-originated OSC 52 may require on, which has security implications. 
GitHub

For a power-user personal application, a tmux-aware clipboard command may be more dependable than assuming raw OSC 52 passthrough.

Terminal title: Set the title using OSC 0/2, restore it on exit where practical, and test tmux's allow-rename, automatic-rename, and window naming settings. You want the session name to be authoritative, not overwritten by the next shell command.

Input conflicts: Choose whether Ctrl keys are handled by tmux, the TUI, or neovim before adding keybindings. Relying on terminal key distinctions that SSH/tmux configurations don't preserve is an easy source of inconsistent behavior.

5. Inline viewport or alternate screen?

For your workflow, I would choose alternate screen initially, but with architecture that permits an inline renderer later.

Comparison

Characteristic

	

Inline/native scrollback

	

Alternate screen




tmux pane-local scrollback

	

Natural, when correctly integrated

	

Application owns history




Independent viewport positions

	

Harder to control

	

Straightforward




External jump to message

	

Awkward once output is committed

	

Straightforward




Reflow after resize

	

Terminal-dependent

	

Application-controlled




Live updating earlier blocks

	

Difficult

	

Straightforward




Forms and overlays

	

Requires careful terminal layout

	

Straightforward




Restoring previous shell content

	

Natural

	

Automatic on exit




Many simultaneous tmux panes

	

Attractive

	

Also works well

The decisive issue for you is external navigation and independently scrollable transcripts, not aesthetic preference.

Codex-style inline rendering is appealing when an agent behaves like a command embedded within a shell session. Your interface behaves more like a long-running application attached to a mutable session document.

Those are different models.

Why alternate screen fits better

You want to navigate to specific historical messages, inspect expanding tool calls, keep scroll position while an agent streams, and switch among many ACP sessions.

An alternate-screen application owns the complete viewport and therefore can make these operations deterministic. A terminal-scrollback-oriented renderer must deal with content that has already been emitted into the terminal's history and cannot simply be redrawn at arbitrary locations.

The latter is possible with careful viewport design, as Codex demonstrates, but it is more specialized.

A compromise worth considering

Keep the alternate-screen TUI but add a command to export or pipe the visible conversation to $PAGER, nvim, or a temporary Markdown file.

This preserves a shell-native escape hatch without committing your live rendering implementation to an inline viewport.

I would not make inline mode a v1 requirement.

6. ACP stability and SDK churn

The evidence supports two separate conclusions:

Stable ACP v1 wire compatibility is becoming useful as an interoperability target. The Rust SDK's internal APIs are still changing quickly.

The 2.x to 3.x transition is not simply a protocol break

The official Rust SDK changelog gives concrete dates:

Version

	

Release

	

Relevant changes




2.0.0

	

July 23, 2026

	

Transport frames, batch preservation, connection APIs




2.2.0

	

September 18, 2026

	

Schema and coordination improvements




3.0.0

	

October 6, 2026

	

Breaking configuration, transport-feature, MCP binding changes




3.1.0

	

October 7, 2026

	

Prepared requests




3.2.0

	

October 8, 2026

	

Retained request cancellation handles




3.3.0

	

October 9, 2026

	

Schema 1.11 update

Docs.rs

The SDK's changelog explicitly distinguishes the v2 SDK API changes from the stable ACP v1 wire schema. The recent 3.0 release changes several low-level APIs and defaults, including making native transports opt-in. It does not mean a v3 SDK client must negotiate ACP protocol v2 with a goose server. 
Docs.rs
+1

This is a good reason to put the SDK behind a narrow adapter rather than to avoid it.

What I'd pin

For an initial implementation, I'd pin a compatible SDK set rather than mix arbitrary majors:

toml
# Illustrative — confirm features against selected SDK version.
agent-client-protocol = "=3.3.0"
agent-client-protocol-schema = "=1.11.0"
agent-client-protocol-http = "=3.3.0"

Your goose v1.54 server can continue using its 2.2.0-era SDK if both parties exchange compatible ACP v1 JSON-RPC. Rust crate compatibility across processes is not required.

However, don't assume that matching the latest SDK makes every extension automatically work. Goose's advertised capabilities and actual serialized responses are the authority.

Before adopting the latest HTTP transport, verify authentication, WebSocket upgrade, reconnection, and lifecycle behavior against goose. The v3 HTTP changes include stricter WebSocket builder configuration and changed proxy/TLS defaults. 
GitHub

Handling goose extensions

I would implement a two-layer adapter.

Layer 1: portable ACP

Use the official typed schema for initialization, sessions, standard updates, prompts, modes, configuration, permissions, and other stable functionality.

Layer 2: goose-specific extensions

Represent _goose/unstable/* requests in a small module using method-name constants and serde_json::Value for fields that are not represented by the standard SDK.

Don't try to generate exhaustive Rust types for all ~110 methods. You only need a few initially.

For example, define local operations such as:

Rust
trait AgentCapabilities {
    async fn set_model(&self, model: &str) -> Result<()>;

    async fn list_extensions(&self) -> Result<Vec<Extension>>;

    async fn set_extension_enabled(
        &self,
        id: &str,
        enabled: bool,
    ) -> Result<()>;
}

Then implement them through standard session/set_config_option where possible and goose-specific methods where necessary.

One caution: advertised standard session configuration is not equivalent to session-local extension control. Verify whether toggling a goose MCP extension is actually per-session, persisted, or process-global. You do not want toggling an extension in one tmux tab to unexpectedly change another active session.

Compatibility testing

I would build a tiny protocol fixture suite independently of the UI. Every captured exchange should include the actual goose version, advertised capabilities, and transport.

The useful tests are: create/load a session, stream a tool call, cancel mid-turn, trigger permission, trigger elicitation, change model, modify an extension, disconnect during an active turn, and reconnect after completion.

Preserve representative redacted wire traces as fixtures. Test unknown notification variants without panicking, and ensure the relay can preserve extension metadata even if the frontend ignores it.

For long-term maintenance, that suite is a better investment than targeting every new ACP draft.

7. Is goose likely to ship a TUI worth using?

There is clear ACP-first momentum, but I would not make your project dependent on an imminent official full-screen TUI.

The timeline matters:

February 18, 2026 — ACP becomes the declared direction. Goose's announcement proposed ACP as the primary interface for its clients, with a new TypeScript TUI as a migration phase. 
GitHub

April 8, 2026 — TypeScript TUI beta announced. Goose published an article introducing npx @aaif/goose, with streamed messages, tools, code highlighting, and Markdown. Session management and MCP controls were still described as forthcoming. 
Your open source AI agent

May 22, 2026 — desktop architecture reset. The team publicly described removing an experimental Goose 2 desktop implementation after combining ACP migration and UI redesign proved difficult. 
GitHub

September 2026 — users still discuss ACP client boundaries. A goose community discussion included a user explicitly wanting a general-purpose ACP TUI client rather than switching UIs for different agents. That is evidence of demand, but not evidence of an official Rust terminal-client initiative. 
GitHub

I did not verify an October 2026 announcement restoring the removed TypeScript client or blessing a particular third-party TUI. The April beta announcement by itself should not be treated as proof that an actively maintained full-screen client exists today.

The right interpretation is that upstream has made goose easier to build clients for, not that a custom client will necessarily become redundant.

I would look for upstream support for decoupled session lifetimes, subscription semantics, and reliable transport compatibility before asking for an official UI.

Those capabilities would benefit everyone building ACP clients and reduce your maintenance burden more than a new themeable TUI.

8. What I would build, fork, or reuse

My recommendation depends on how aggressively you want to eliminate future maintenance.

For the frontend: evaluate Nori, then fall back to a new small ratatui app.

Reuse edtui, tui-markdown, ACP SDK, and Codex's stable-source streaming strategy. Only fork a larger client if the existing architecture lets you replace its ACP connection and session model cleanly.

For session ownership: trial Hydra first, inspect acp-mux second.

Both are more relevant than Codex's enormous frontend. If neither works with an existing goose WS server, implement your minimal Rust relay with persistent prompt ownership.

For transcript semantics: own a small domain model.

Do not make another project's rendered widget tree, raw JSON-RPC log, or terminal screen buffer your authoritative conversation state. Use typed semantic blocks and stable IDs.

An efficient implementation sequence

I'd structure the work as a set of gates with explicit acceptance conditions, rather than write a full TUI and hope the architecture holds.

Gate

	

Work

	

Success criterion




1 — 4–8 h

	

Test Hydra and Nori with goose

	

Prompt, tool calls, permissions, long-lived sessions




2 — 8–16 h

	

Implement or validate relay ownership

	

Disconnect all viewers during a turn; work continues




3 — 12–24 h

	

Build event model and attach snapshots

	

Concurrent viewers converge on same transcript




4 — 20–40 h

	

Ratatui daily-driver shell

	

Vim composer, scroll anchoring, streamed text




5 — 16–32 h

	

Rich details and controls

	

Diffs, questions, config, extension toggles




6 — 20–40 h

	

Hardening against real workloads

	

100+ sessions/day, SSH/tmux, replay and recovery

These are milestone budgets, not a promise that the whole system will fit their sum. Gate 2 in particular can expand if goose's WebSocket connection semantics resist persistent ownership.

The four tests I'd run before choosing a codebase

Compatibility acceptance tests

0 / 4 verified

Zero-viewer survival

Submit a long prompt, disconnect every terminal client, reconnect later, and verify the same turn completed rather than being cancelled.

Two-viewer consistency

Attach two clients to one session, stream tool updates and Markdown, scroll independently, and verify identical logical history.

Disconnected permission flow

Trigger a permission or elicitation request, detach, reattach, and answer it without restarting the turn.

Historical attach

Load a substantial existing goose session and verify message order, tool details, config state, and bounded replay.

Copy checklist
Two decisions that could save the most time

First, decide whether simultaneous independent viewing is a hard requirement, or whether sequential attachment to one persistent TUI per session is sufficient. If it is the latter, tmux may eliminate the entire relay implementation in the first iteration.

Second, decide whether the TUI must speak goose's WebSocket ACP transport directly, or may speak a local relay's narrower event/control protocol. I would strongly prefer the latter for the frontend while requiring the relay itself to remain an ACP client.

That gives you goose interoperability without dragging all of ACP's lifecycle semantics into terminal widget code.

9. Final ranking for your exact situation

Path

	

Time to usable

	

Long-term maintenance

	

Fit




Keep v1.18 OpenCode patched

	

Immediate

	

Rising, platform-dependent

	

Existing best fit




Hydra as daemon + custom Rust frontend

	

Potentially fastest

	

Two-language dependency

	

Excellent if goose integration passes




Nori fork + small Rust relay

	

Moderate

	

Reasonable if clean architecture

	

Best Rust-first candidate




New ratatui frontend + minimal Rust relay

	

Moderate–high

	

Controlled, mostly your code

	

Best predictable fallback




Full Martty fork

	

Uncertain

	

Potentially high

	

Less attractive




Patch goose + custom TUI

	

High

	

Upstream fork burden

	

Avoid initially




Direct TUI-to-goose, no persistent process

	

Fast

	

Low code, broken detach semantics

	

Does not meet requirements

I would budget approximately 100–180 focused engineering hours for a satisfactory personal daily driver if either the Nori frontend or an existing relay is significantly reusable. If both fail and you own all the moving parts, 200–300+ hours is a more defensible expectation.

Your 5–15k LOC forecast is within reason. I would aim for 6–10k LOC in the TUI and 2–5k in the relay, excluding tests, but treat that as a constraint on architecture rather than a productivity target.

The highest-value existing code is not the 274k-line Codex TUI. It's the relatively small pieces that already implement the failure-prone details: edtui's Vim editing, ratatui-textarea's Unicode wrapping, Codex's streaming collector, and an ACP multiplexer with explicit session ownership.

My final recommendation: spend no more than the initial 1–2 days evaluating Nori, Hydra, and acp-mux. If no existing integration preserves a goose turn through zero attached viewers, build the relay first. Once the relay works, the full-screen ratatui frontend becomes a tractable, scoped UI project rather than another agent-runtime migration.

The most consequential unresolved issue is not how to render Markdown or Vim motions. It is who owns a goose turn when nobody is looking at it. Solve that once, beneath the interface, and nearly every other requirement becomes replaceable.