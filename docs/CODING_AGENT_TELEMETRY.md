# Coding-agent telemetry

How coding-agent CLIs (Claude Code, Codex) report turns to Nasiko, how the contract
evolves without breaking older CLIs or servers, and how Nasiko corrects history that an
older CLI reported wrongly.

## Contract evolution

- The event contract is `CodingAgentEventV1` (`types/src/coding_agent.rs`), posted to
  `POST /api/telemetry/coding-agent/events/batch`. `CODING_AGENT_EVENT_VERSION` stays `1`.
- New fields are additive and optional (`serde(default, skip_serializing_if =
  "Option::is_none")`): `source.adapter_version`, `turn.agent_scope` and
  `llm_calls[].accounting.output_tokens_final`. A v1.0 payload round-trips byte-identical,
  and an older server ignores the fields it does not know. Unknown enum values map to
  `Unknown` (`serde(other)`). The event version is not bumped for additive fields.
- `GET /api/telemetry/coding-agent/capabilities` returns
  `{"data": {"event_version": 1, "features": [...]}}`. Under the contract, a CLI sends a new
  field only when the server lists its feature slug (`agent_scope`, `adapter_version`), and
  treats an older server without the endpoint as having no features. The CLI-side
  capability gate is added by a later Phase 4 plan.
- Receipts are immutable and keyed by `(user_id, event_id)`. A replay of the same
  identity with an identical payload is `duplicate`. Under the tolerated-replay rule, a
  stored receipt without `source.adapter_version`, replayed by a payload of the same
  identity that does carry the marker, is also `duplicate`: the first receipt is kept and
  token fields are not compared. Every other differing payload is `rejected`.

## Codex cached-input correction

**The bug.** Codex CLIs before adapter version 1 reported `input_tokens` inclusive of the
cache reads they also reported in `cache_read_tokens`. Every figure derived from those
receipts (Tempo spans, `trace_usage`, the chat transcript) therefore counted cached input
twice, once at the fresh input rate and once at the cache-read rate.

**The marker.** Fixed CLIs send `source.adapter_version` (Codex: `1` or later, constant
`CODEX_ADAPTER_VERSION_EXCLUSIVE_INPUT`). The server applies the correction only when
`source.agent_id = 'codex'` and `adapter_version` is absent. Receipts that carry the
marker, and every non-Codex receipt, are never corrected. Old CLIs keep sending
inclusive events after a server upgrade, so the rule is applied at ingest as well as by
the history backfill.

**The correction.** Labelled `codex_inclusive_input_v1`. Per LLM call,
`input := input - cache_read` (saturating at 0), and the turn is re-priced with the
corrected usage. The per-receipt rollup `coding_agent_turn_usage` (migration 0050)
stores both the reported and the corrected figures, plus `correction`.

**Nothing stored is rewritten.** Receipts are immutable (trigger, migration 0017), and
`trace_usage` is re-upserted from Tempo by the materializer, so a correction written into
either would be refused or lost. Corrections are applied at read time, as an overlay of
`reported - corrected` deltas taken from rows whose `correction` is set
(`server/src/observability/codex_correction.rs`). Deltas are clamped at 0, so a
correction can only ever lower a figure. Every corrected figure carries
`usage_corrected: true`. Uncorrected responses omit the flag, or report `false` on the
TokenOps dashboard.

**Where corrected figures appear:**

| View | Source | How |
|---|---|---|
| TokenOps dashboard, spend timeseries, spend calendar and day view, agent stats | `trace_usage` | Read through the `trace_usage_corrected` view. The dashboard returns `usage_corrected` when any figure in the window was corrected. |
| Session list (`/api/observability/session/list`) | Tempo | The page's overlay is loaded in one query. Token and cost totals drop by the summed deltas of the traces shown. |
| Session detail | Tempo | Session totals (total and prompt cost and tokens) and each per-turn root entry are corrected. |
| Trace and span views | Tempo | Trace totals drop by the trace delta. Each LLM span takes a share of it proportional to its cached input. |
| Chat transcript (`/api/chat/sessions/{id}` and `/messages`) | `chat_messages` | A per-trace delta on `chat_messages.trace_id` is applied to assistant messages. |
| Per-agent breakdown (`/api/coding-sessions/{id}/agents`) | `coding_agent_turn_usage` | Corrected when the rollup row is written. |

If the overlay cannot be loaded, the view is served uncorrected and without the flag,
and the failure is logged. The view never fails because of the overlay.

**UI label (neutral wording):** "Corrected: Codex cached input was counted twice by older
CLIs. Figures shown exclude the duplicate."

### Reach checked

| Consumer | Affected | Why |
|---|---|---|
| Budgets (router enforcement and status) | No | Budgets meter router-written `token_usage`. Hook receipts never write `token_usage`, and the LLM router normalizes OpenAI usage (`normalize_openai_details` lifts `prompt_tokens_details.cached_tokens` out of `prompt_tokens`) before pricing. |
| Spend-spike detector (`server/src/alerts/spike.rs`) | No | Reads the same router-metered `token_usage` as budgets. |
| Cache-savings KPI (`window_cache_savings`) | No | Uses only `cache_read_tokens`, which Codex always reported correctly. It reads through the corrected view only for consistency. |
| OTLP log attributes in Loki | Not corrected | Exported log records keep the figures that were reported. Logs are a raw record, not a spend view. |
| Rollup and Tempo pricing | Small differences possible | Deltas come from the rollup's own pricing (`reported - corrected`). The observability pricing of the same trace can differ slightly, and the result is labelled as corrected. |

### Residual risks

1. **Server downgrade.** If a CLI cached the `adapter_version` capability from an upgraded
   server and the server is then downgraded to a build that ignores the field, the CLI
   still sends exclusive figures, but the older server stores them without the marker.
   After the upgrade is restored, those receipts look legacy and are corrected a second
   time.
2. **The tolerated-replay rule is one-way.** An unmarked receipt followed by a marked
   replay of the same identity is a `duplicate`. The reverse is `rejected`, not
   `duplicate`: a stored marked receipt followed by an unmarked replay, for example after
   the capability flips during a downgrade.
