# honkd Turn Hook (v1)

honkd features an optional, generic turn hook that reports the start and end of every turn across all active sessions to an HTTP receiver. This allows external services—such as the [pigeon](https://github.com/johnnymo87/pigeon) daemon—to mirror prompt submissions and assistant replies into external notification streams (like Telegram topics) regardless of which client initiated the turn.

## Configuration Flags

The turn hook is enabled on `honkd` via command-line flags:

- `--turn-hook-url <URL>`: Target URL for hook HTTP POST requests (e.g. `http://127.0.0.1:4731/goose/turn-event`). If omitted, the turn hook is disabled and no events are generated.
- `--turn-hook-token-file <PATH>`: Optional path to a file containing a Bearer authentication token. The file is read at startup, whitespace is trimmed, and its contents are sent in the `Authorization: Bearer <token>` header. An empty file or missing URL when the token file is provided is rejected on startup.
- `--turn-hook-retry-secs <SECS>`: (Optional, hidden; default 1200s / 20 min) Duration measured from an event's enqueue time for which transient delivery errors will be retried before giving up.

At startup, honkd logs `turn hook enabled url=<url>` (without exposing token contents). If a non-loopback URL is specified, a warning is logged.

## Delivery Semantics

- **Bounded Queue**: Hook events are dispatched via an in-memory bounded channel (capacity 256).
- **Non-blocking Emission**: Session actors emit events using `try_send`. If the channel is full, the event is dropped with a warning log; session actors never await network I/O or block on receiver latency.
- **Sequential In-Order Delivery**: A dedicated background sender loop delivers one event at a time in order.
- **Retry Window From Enqueue Time**: Each event records its enqueue timestamp (`enqueued`). The sender always makes at least one delivery attempt. If an attempt fails and `now >= enqueued + retry_window`, the event is dropped on that failure without further retries so stale backlogs do not delay fresh events after long outages.
- **Error Handling & Retries**:
  - `2xx`: Successful delivery. The sender advances immediately to the next queued event.
  - `401 Unauthorized` / `403 Forbidden`: Treated as auth errors. If `--turn-hook-token-file` is configured, honkd re-reads the token file from disk. If the token changed, it retries the event once immediately with the new token. If the token is unchanged, the file is unreadable, or the retry fails, the event is dropped with an error log (tokens are never logged).
  - Other `4xx`: Client errors (such as `404 Not Found` if the receiver route is missing) are treated as permanent. The event is dropped with a warning log and the sender advances to the next event.
  - `5xx` / Connection Refused / Timeout: Treated as transient. The sender retries the event with exponential backoff (starting at 1s, doubling up to a 60s cap) until the retry window (`--turn-hook-retry-secs`, 20 minutes default from enqueue time) has elapsed, after which the event is dropped with a warning.

## Wire Contract: `turn-hook v1`

Every request is a `POST` with `Content-Type: application/json` and an optional `Authorization: Bearer <token>` header. Each request contains a single event object.

### `turn.started`

Emitted when goose announces an active run for the turn (coinciding with honkd's user-message echo synthesis):

```json
{
  "v": 1,
  "type": "turn.started",
  "event_id": "<session_id>:<upstream_id>:started",
  "session_id": "<goose session id>",
  "upstream_id": "hk3_7-12",
  "run_id": "run_abc",
  "client_req_ids": ["crid-1"],
  "prompt_text": "user text of the prompt (capped)",
  "at_ms": 1792162675775
}
```

### `turn.ended`

Emitted once when the turn concludes (result received, error received, synthesized gap reply, or interruption of an in-flight prompt):

```json
{
  "v": 1,
  "type": "turn.ended",
  "event_id": "<session_id>:<upstream_id>:ended",
  "session_id": "<goose session id>",
  "upstream_id": "hk3_7-12",
  "run_id": "run_abc",
  "client_req_ids": ["crid-1"],
  "prompt_text": "user text of the prompt (capped)",
  "at_ms": 1792162677890,
  "stop_reason": "end_turn",
  "final_text": "assistant reply text",
  "final_text_truncated": false,
  "usage": {
    "used": 53000,
    "size": 200000,
    "cost": {
      "amount": 0.045,
      "currency": "USD"
    }
  }
}
```

Ended-only fields (`stop_reason`, `honk_reason`, `final_text`, `error`, `usage`) are omitted when empty/null in serialized JSON (`#[serde(skip_serializing_if = "Option::is_none")]`). Receivers must treat absent and `null` fields identically.

### Event Fields

- `v` (integer): Wire protocol version, currently `1`.
- `type` (string): Either `"turn.started"` or `"turn.ended"`.
- `event_id` (string): Stable identifier format `<session_id>:<upstream_id>:started` or `<session_id>:<upstream_id>:ended`.
- `session_id` (string): The goose session id.
- `upstream_id` (string): honkd's upstream prompt identifier (`hk{boot}_{actor}-{seq}`), unique across prompts and restarts. Acts as the turn correlation key.
- `run_id` (string or null): goose run identifier when known.
- `client_req_ids` (array of strings): Deduplicated `client_req_id` values associated with the prompt's owners.
- `prompt_text` (string): User prompt text, capped at 256 KiB on a UTF-8 character boundary.
- `at_ms` (integer): Event timestamp in Unix epoch milliseconds.
- `stop_reason` (string, `turn.ended` only, omitted if empty): goose stop reason string (e.g. `"end_turn"`, `"cancelled"`), or omitted on error.
- `honk_reason` (string, `turn.ended` only, omitted if empty): honkd reason when synthesized or interrupted (e.g. `"upstream_restart"`, `"delivery_unknown"`, `"cancelled"`, `"synthesized"`), else omitted.
- `final_text` (string, `turn.ended` only, omitted if empty): Final assistant response text from exported session history, capped at 256 KiB.
- `final_text_truncated` (boolean, `turn.ended` only): Always present on `turn.ended`. Indicates whether `final_text` was truncated at the 256 KiB boundary (`false` if within limits or if `final_text` is absent).
- `error` (string, `turn.ended` only, omitted if empty): Error message string when goose responded with a JSON-RPC error.
- `usage` (object, `turn.ended` only, omitted if empty/unknown): Context window usage at turn end, with `used` (integer), `size` (integer), and optional `cost` (`amount` (float) and `currency` (string)).

## Idempotency and Accepted Gaps

- **Receiver Idempotency**: After honkd restarts, replayed replies or reattached sessions may re-emit `turn.ended` with the same `event_id`. Receivers must be idempotent with respect to `event_id`.
- **Emission Conditions**:
  - `turn.started` is emitted at most once per `upstream_id` upon run announcement, or right before `turn.ended` if goose interrupts an unannounced turn (e.g. upstream restart).
  - `turn.ended` is emitted only for turns that ran or may have run (including gap synthesis, restart/delivery_unknown interrupts). Parked prompts that never ran, busy rejections, send failures, `not_delivered`, and unannounced cancellations do not emit `turn.ended`.
- **Accepted Gaps**:
  - **Steer messages**: Steer messages typed into a running turn are not mirrored as standalone turns.
  - **Direct-to-goose runs**: Runs initiated directly against goose (bypassing honkd) are not visible to honkd and will not be emitted.
  - **Parked prompts ordering**: With parked prompts from two clients, `turn.started` for turn N+1 can precede `turn.ended` for turn N if the second prompt is dispatched before the asynchronous export task for turn N completes.
  - **Loss before creation**: An ended event can be lost before creation if honkd crashes between upstream prompt reply and side-connection export, or if goose restarts during the reply-text export with no attached viewers.
  - **Missing final_text**: `turn.ended` may lack `final_text` if goose restarts during the export task.
  - **Crash / expiration loss**: Events in memory when honkd crashes or terminates, or events that exceed the retry window, are dropped.

## Receiver Implementation

pigeon implements a compatible receiver at `POST /goose/turn-event`. See pigeon's `docs/contracts/honkd-turn-hook-v1.md` for receiver-side handling, session mapping, and own-turn filtering.
