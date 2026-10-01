# Budgets

Dollar budgets per user, per agent, and platform-wide, enforced by the built-in LLM router
before tokens are spent. The router sits in the call path of every agent and coding-agent
LLM request, so it can act on spend instead of only observing it.

## Overview

- A budget caps spend for a UTC period. When the cap is reached the router either blocks
  the call (`block`) or serves it on the cheapest model until a hard ceiling (`downgrade`).
- The check runs on every call using Redis counters, never a `SUM` over `token_usage` on
  the request path.
- The check fails closed: if a budget applies and its counter cannot be read, the call is
  refused.
- Crossing the soft threshold or the limit writes exactly one durable event per budget per
  period (`budget_events`), which Phase 3 alerting consumes.

## Budget model

| Field | Meaning |
|---|---|
| `scope` / `target_id` | `user` (target = user id), `agent` (target = agent id), or `platform` (no target). |
| `period` | `daily`, `weekly` (starts Monday) or `monthly`; all boundaries are UTC. |
| `limit_usd` | The spend that triggers the action. |
| `soft_threshold_pct` | Percent of the limit (default 80). An event level only: it never blocks or downgrades. |
| `action` | `block` or `downgrade`. |
| `downgrade_ceiling_pct` | For `downgrade`: percent of the limit (default 125, min 100) at which the call is blocked outright. |
| `enabled` | Disabled budgets are ignored by enforcement. |

A call is checked against every enabled budget that applies to it: the billed user's, the
calling agent's, and the platform's. Any block beats a downgrade beats allow.

## REST API

All routes are under `/api`.

- `GET/POST /budgets`, `GET/PUT/DELETE /budgets/{id}`: admin only (`require_user_manager`
  plus a role check in each handler). Responses include live status: `spend_usd`,
  `pct_used`, `projected_usd` (linear projection over the period), `period_start`,
  `resets_at`, and `state`.
- `GET /budgets/me`: any authenticated user; returns only the budgets that apply to the
  caller: own user budgets, platform budgets, budgets of agents the caller owns (full
  amounts), and budgets of agents the caller can access through public visibility or
  grants. Members see redacted rows (`pct_used`, `state`, `resets_at` and `period` only;
  the dollar fields `limit_usd`, `spend_usd`, `projected_usd` are absent) for platform
  budgets and for budgets on agents that are public or granted to them rather than
  owned. Superusers see owned agents only, as before.
- `state` is one of `ok`, `soft`, `downgrading`, `blocked`, `disabled`, `unknown`.
  `unknown` means the counter store was unavailable; spend is never reported as zero then.

## Enforcement

- Surfaces: chat completions, Anthropic messages, Gemini, Responses, and embeddings,
  including the coding-agent path (`is_coding_agent`) and deployed agents. No change to the
  agent-side protocol.
- The check runs after routing resolves the destination and before any upstream call, so a
  blocked call never reaches a provider.
- With no applicable budget the router makes no Redis call. Otherwise it issues one `MGET`
  for all applicable counters (plus a rebuild for any missing key).
- The billed user is the flow's user (from `traceparent`) or, for coding agents, the JWT
  owner. This is the same identity the usage row is attributed to.

## Errors

A block returns 429 with `Retry-After` (seconds until the period resets, minimum 1) in the
caller's own error dialect, plus a machine-readable `nasiko_budget` block.

OpenAI-compatible (`code` is `budget_exceeded`):

```json
{
  "error": {
    "message": "LLM budget exceeded for user budget 7d0c...; resets at 2026-11-01T00:00:00+00:00",
    "type": "insufficient_quota",
    "code": "budget_exceeded",
    "param": null
  },
  "nasiko_budget": {
    "budget_id": "7d0c...", "scope": "user", "period": "monthly",
    "limit_usd": 50.0, "spend_usd": 50.01, "resets_at": "2026-11-01T00:00:00+00:00"
  }
}
```

Anthropic uses `{"type":"error","error":{"type":"rate_limit_error",...},"code":"budget_exceeded"}`.
Gemini uses `{"error":{"code":429,"status":"RESOURCE_EXHAUSTED",...}}`. All three carry the
same `nasiko_budget` block.

A counter-store outage returns 503 with code `budget_store_unavailable`, no spend details
and no internal error text:

```json
{
  "error": {
    "message": "LLM budget store unavailable; request refused",
    "type": "server_error",
    "code": "budget_store_unavailable",
    "param": null
  }
}
```

## Fail-closed

Counter-store calls on the pre-call path are bounded to 50 ms. If any enabled applicable
budget exists and its spend cannot be read in that time (Redis down, rebuild failed), the
call is refused with 503 `budget_store_unavailable`. Calls that no enabled budget applies to
are never affected. Post-call work (increment, rebuild, events) has a 1 s bound and never
fails the caller.

## Downgrade

When a `downgrade` budget's spend is at or above its limit but below
`limit x downgrade_ceiling_pct / 100`:

- The call is served on the cheapest configured model: the agent config's `tier3_model`,
  else the Tier3 model of the tier registry (operator `model_registry` override first, then
  the price-ranked catalog) for the call's provider.
- Fallback models are cleared, so a failure cannot escape to a pricier model.
- The response carries `x-nasiko-budget-downgraded: <budget_id>` and
  `x-nasiko-original-model: <requested model>`, on streaming and non-streaming responses
  and on the Responses surface.
- The `token_usage` row records `metadata.budget_downgrade = {budget_id, from_model,
  to_model}` and the cheaper model as `model`.
- At or above the ceiling the budget blocks exactly like a `block` budget.
- Pinned (compliance-locked) agents are never downgraded: they are blocked at the limit.
- Embeddings have no cheaper model; they are served unchanged until the ceiling.
- If no cheaper model is configured, or the call is already on the cheapest model, it is
  served as-is with no downgrade header (the ceiling still applies).
- The swap happens after routing, so the sticky routing decision cache never stores the
  downgraded model.

## Counters

- Key: `nasiko:budget:spend:{budget_id}:{period_start_unix}`, value an integer of micro-USD
  (1e-6 USD), TTL = period + 2 days.
- The post-call path increments with a Lua script that only increments an existing key.
- A missing key (new budget or period, Redis flush or eviction) is rebuilt from
  `token_usage` with `operation_type IN ('direct_llm','embedding')` and stored with
  `SET NX`; BYOK calls (user-owned keys) are counted too.
- A failed post-call increment deletes the key (or marks it dirty in-process if even that
  fails), so the next check rebuilds it instead of trusting a stale-low value.
- Counters are incremented before the `token_usage` insert to shorten the window in which a
  follow-up call sees old spend.

## Events

`budget_events` holds one row per `(budget_id, period_start, kind)` with `kind` in
`soft_threshold` or `hard_limit`. The `UNIQUE` constraint with `ON CONFLICT DO NOTHING` is
the exactly-once mechanism, so concurrent calls and multiple replicas cannot duplicate an
event.

Events are written when:

- a post-call increment moves a counter across the soft threshold or the limit;
- a counter is (re)built already at or above a level (including a budget created after
  the spend happened);
- a call is blocked at the limit and no `hard_limit` row exists yet.

Insert failures are logged and never affect the call. Phase 3 alerting consumes these rows.

## Overshoot bound (ENF-06)

The true cost of a call is known only after the response. The router checks current spend
against the limit before the call and reconciles after it. Consequences, stated plainly:

- Concurrent in-flight calls that pass the check together can overshoot the limit by at
  most the sum of their costs.
- A sequential caller is blocked once the post-call increment lands, normally well under
  1 s after the response.
- There is no reservation or escrow in this version.

Related bounded error, an UNDERCOUNT: when a counter key is missing, a rebuild `SUM`
snapshot can be taken before a concurrent row commits and still win the `SET NX`; that
row's cost is then not in the counter until the key next expires. This affects only the
flush or new-key window and is at most the cost of the calls committing inside it.

## Known limitations

- Budgets (and spend-spike detection) count only router-metered rows with
  `operation_type IN ('direct_llm','embedding')`. Not counted or gated: `'orchestrator'`
  (the orchestrator's own chat turns, written by `server/src/router/usage_meta.rs`),
  `'router_selection'` (the agent-selector LLM call in `orchestrator/src/engine.rs`) and
  `'mcp_tool_call'` (tool-call metering rows, not LLM spend).
- TokenOps charts read `trace_usage` (materialized from Tempo traces) while budgets and
  spend-spike alerts read `token_usage`, so TokenOps totals can differ from budget spend
  (missing or late traces, excluded operation types). Budget enforcement always uses
  `token_usage`.
- Budget definitions are cached for 5 s per replica; edits converge across replicas within
  that time (the replica that handled the edit sees it immediately).
- SDKs commonly retry 429 and 5xx responses. Budget refusals happen before any provider
  call, so retries are cheap.
- The standalone `llm-router` binary needs `REDIS_URL` and migration 0048; without them,
  calls that a budget applies to fail closed.
