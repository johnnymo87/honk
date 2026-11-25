# Cross-provider model switch (honk-9qf.15.24 part B)

Beads: honk-9qf.15.24 (switchable provider), honk-9qf.26 (goose astra/fable twins).
Builds on docs/plans/2026-11-19-model-switch.md (in-session model switch, honk-9qf.15.7).

## Problem

goose switches a session's model only within its provider. workstation now deploys a
goose custom provider `codex_lb` (OpenAI models through codex-lb on 127.0.0.1:2455:
gpt-6-astra, gpt-6.1-sol, gpt-6-luna, gpt-5.6-sol/terra/luna), and honkd's
`--models-file` gains `providers.codex_lb`. A cloudbox session runs on `gcp_vertex_ai`,
so today honk never offers those models, and `/model gpt-6-astra` would set an OpenAI
id on the Vertex provider (accepted by goose, fails at the next turn).

## goose behaviour relied on (goose 1.60.2, crates/goose/src/acp/server/dispatch.rs)

- `session/set_config_option {sessionId, configId:"provider", value:<provider>}` switches
  the session's provider and resets the model to that provider's default model; the
  response carries `configOptions` (provider/model current values and the new provider's
  model options) and a `config_option_update` notification precedes it.
- A following `configId:"model"` call sets the model within the new provider.
- Unknown provider -> JSON-RPC error. Model ids are not validated.

## Design

### honkd: `_honk/model/list`

- New optional request param `allProviders: bool` (default false).
- Every returned entry gains `"provider": <provider id>` (configured entries: the
  models-file key; goose entries: the session's current provider).
- With `allProviders: true`, after the current provider's configured entries and goose's
  own options, append the configured entries of every OTHER provider in the models file
  (providers in sorted key order, entries in file order), `source: "configured"`, each with
  its own `provider`. Dedup is per (provider, id), not per id.
- Without the flag the reply is exactly today's plus the `provider` field, so an old honk
  client never sees an id it would switch with a model-only call.

### honk client (chat core, TUI picker, launch)

- `ModelItem` gains `provider: Option<String>` (serde default).
- Every `_honk/model/list` request honk sends carries `allProviders: true`.
- Switching to an item (picker selection, `/model <id>`, recent-model pick) resolves the
  target provider:
  - picker / recents: the item's `provider`;
  - `/model <id>`: the first listed entry with that id whose provider equals the session's
    current provider, else the first listed entry with that id; unknown id -> today's
    behaviour (warning + model-only set).
  - `/model <provider>/<id>` (only when `<provider>` is a provider key in the list) names
    the provider explicitly.
- If the target provider is None or equals the session's current provider: today's
  single `configId:"model"` set.
- Otherwise two steps: `configId:"provider"` with the provider, then on success
  `configId:"model"` with the id. Implement as a new pending variant (e.g.
  `PendingModelReq::SetProvider { provider, then_model, queued, record_recent }`) whose
  success dispatches the model set; the model step's existing success path records recents
  and emits the `model: a → b` notice. Busy (-32002 honk busy) on the provider step
  requeues exactly like the model step. Any error on either step: error notice naming the
  step (`provider switch to codex_lb failed: ...`) and a silent list refresh. While the two
  steps are in flight, `has_pending_model_mutation()` must be true so prompts queue.
- A provider change emits a notice `provider: gcp_vertex_ai → codex_lb` (from
  `update_model_state`, mirroring the model notice).
- Picker: entries whose provider differs from the session's show the provider as a
  suffix/label (e.g. `GPT-6 Astra  codex_lb`); grouping/ordering otherwise unchanged.
- `honk launch --model <id>` and `honk chat --new --model <id>`: resolve the provider from
  the list the same way and, when it differs, send the provider set before the model set.
  The "not in honkd's model list" check accepts ids of any provider.

## Out of scope

- Telegram `/model` (honk-9qf.15.23).
- Provider switching to providers absent from the models file.

## Tests

- honkd models.rs unit tests: provider field; allProviders appends other providers; no flag
  -> unchanged set; per-(provider,id) dedup.
- honk chat core tests: same-provider set unchanged (one request); cross-provider set emits
  provider request then, on success reply, model request; provider error -> notice + list
  refresh, no model request; busy on provider step requeues; `/model codex_lb/gpt-6-astra`;
  pending mutation blocks prompts until the model step completes.
- launch tests for the two-step path.
