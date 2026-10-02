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

## Subagent capture (Claude Code)

When the destination advertises both `agent_scope` and `adapter_version` (cached by
`nasiko agents sync`, never probed from the hook), the Claude Code hook reports the work
of subagents as well as the main agent. Without both features, nothing changes: no scoped
events are sent and Claude receipts carry no marker.

**What is captured.** Claude Code writes each subagent's transcript next to the session,
at `<session>/subagents/agent-<agentId>.jsonl`, with `agent-<agentId>.meta.json`
alongside it. Each run segment becomes its own event in the parent session. A run segment
is the first prompt or a later coordinator resume. The event has
`turn.id = subagent:<agentId>:<segment uuid>` and `turn.agent_scope` set, and it carries
the subagent's own LLM calls (tokens by class), tool calls and timestamps. Linkage:

- `parent_tool_call_id` is the `toolUseId` of the `Agent` call that spawned the run
  (`Task` in older versions).
- `parent_agent_id` is set when that call is in another subagent's transcript (nested
  subagents); `spawn_depth` comes from the meta.

**Marker.** Claude receipts carry `source.adapter_version = 1`
(`CLAUDE_ADAPTER_VERSION_SUBAGENTS`) only while subagent capture is active. The server
uses it to say whether subagents were captured for a session.

**Intent and content policy.**

| Field | Metadata-only (`--no-content`) | Content capture |
|---|---|---|
| `agent_scope.kind`, `agent_type` | sent | sent |
| `parent_tool_call_id`, `parent_agent_id`, `spawn_depth` | sent | sent |
| `agent_scope.description` (task text), `agent_scope.name` | not sent | sent |
| `turn.prompt`, `turn.response` (handback report or last text) | not sent | sent |
| tool arguments, output, raw, error | not sent | sent |
| `session.title` | never sent on a scoped event | never sent on a scoped event |

**Output tokens are a lower bound.** Subagent transcripts often keep only the
start-of-stream usage snapshot for a call (`stop_reason: null` on every record; Claude
Code issues #97763 and #84223). Input and cache classes are correct, but output and
therefore cost are undercounted. Such calls carry
`accounting.output_tokens_final = false`, and views show them as minimums. Output is never
estimated from text length.

**No double counting.**

- Calls already present in the main transcript are excluded.
- A call that appears in several subagent files (forks, nested copies) is reported once.
- Parent-side summaries are never read as usage: `toolUseResult.usage`, `totalTokens`
  and `<subagent_tokens>` describe the final call's context, not the run.

**When a run is reported.** Receipts are immutable, so a run segment is sent only after
a terminal signal:

- a later segment exists, or
- the main transcript has a `<task-notification>` for the agent with status
  `completed`, `failed`, `killed` or `stopped`, or
- the spawning `Agent` call's result is `completed` or an error.

A run that is still going is deferred and retried on the next Stop. A fully captured
transcript whose size has not changed is not re-read (watermark `subagent_files`).
Discovery reads regular files with `agent-<id>` names only, skips symlinks, and is capped
at 500 agents and 64 MiB per transcript.

**Known gaps.**

- A subagent that is still running when the user quits is not captured until a later
  Stop in the same session sees its terminal signal. Without one, it is never captured
  (SessionEnd handling is Phase 5).
- Agent teams: teammates are not captured yet. Metas without `toolUseId`, or with
  `taskKind: "in_process_teammate"`, are skipped. A named subagent that has a `toolUseId`
  is still captured as a subagent. Split-pane teammates appear as separate sessions.
