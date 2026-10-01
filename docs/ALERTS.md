# Alerts, monitors and notifications

Nasiko turns metering into alerts: budget events, spend spikes and error-rate or latency
monitors raise deduplicated alerts, and a durable outbox delivers them to signed webhooks
and Slack. Admins manage everything from the API (and the UI); spend spikes also appear as
markers on the TokenOps spend chart.

## Overview

- Alerts are rows in `alerts` with a `kind` (`budget_soft`, `budget_hard`, `spend_spike`,
  `monitor_breach`), a `severity` (`info`, `warning`, `critical`), a scope (`platform`,
  `agent`, `model`, `user`) and a `status` (`open`, `acknowledged`, `resolved`).
- Raising or resolving an alert writes the matching `notification_outbox` rows in the same
  transaction, so a notification is never lost between the alert and its delivery.
- Delivery is at-least-once with retries. Receivers dedupe on `X-Nasiko-Delivery`.
- Alert CRUD, monitors and channels are admin-only. Non-admins can read only spike markers
  for agents they can access.

## Alert sources

### Budget events

Budget soft and hard events (see [BUDGETS.md](BUDGETS.md)) become alerts: a soft threshold
crossing is a `warning`, reaching the limit is `critical`. Events for a period that has
already ended are skipped. The worker polls `budget_events` every `ALERTS_BUDGET_EVENTS_SECS`.

### Spend spikes

- Data source: `token_usage` router rows (`operation_type` `direct_llm` and `embedding`,
  the same rows budgets meter).
- For the latest complete hour, each scope (the platform, and every agent) is compared with
  the mean plus `ALERTS_SPIKE_SIGMA` sample standard deviations (n-1) of the previous 168
  hours. A spike also needs spend of at least `ALERTS_SPIKE_FLOOR_USD`.
- Cold start: a scope with fewer than 24 non-zero baseline hours is skipped.
- Severity is `critical` above twice the threshold, otherwise `warning`.
- Evaluated once per complete hour by one replica (a transaction-scoped advisory try-lock
  guards the tick; `ALERTS_SPIKE_SECS` is the poll interval, not the evaluation interval).
- The dedup key carries the hour, so a sustained spike raises one alert per hour.

### Monitors

Admins define monitors (`/api/monitors`) with a metric, a scope (`platform`, `agent` or
`model`), a window of 5 to 1440 minutes, a threshold, a `min_samples` gate (default 20) and
a severity (default `warning`). At most 100 monitors may be enabled.

- `error_rate` is `100 * failures / (failures + successes)`. A failure is one row in
  `llm_call_failures` per failed request: provider 4xx/5xx, rate limits, timeouts,
  transport and parse errors, and mid-stream errors. Successes are router-metered
  `token_usage` rows.
- `p95_latency_ms` is the 95th percentile of `latency_ms` over successful calls. For
  streaming calls this is end-to-end generation time, not time to first token.
- Responses failed-attempt rows (`failed:*` / `http:*` finish reasons) are zero-cost audit
  rows and count as neither successes nor latency samples.
- Windows with fewer than `min_samples` calls change nothing in either direction, so a
  quiet period never flaps an alert.
- Each monitor is evaluated under its own cross-replica advisory lock; a monitor that
  errors is logged and skipped so it cannot starve the others.

## Dedup and resolve

- Dedup is on `dedup_key` while an alert is `open` or `acknowledged` (a partial unique
  index, atomic across replicas). Raising it again only counts an occurrence.
- If the new severity is higher, the alert escalates and an `escalated` notification is
  sent. Same-or-lower severity sends nothing new.
- Acknowledging (`POST /api/alerts/{id}/acknowledge`) keeps the alert live for dedup.
- Resolution sends a `resolved` notification:
  - Budget alerts: when the budget period ends, or the budget is disabled or deleted.
  - Monitor alerts: after 3 consecutive clear evaluations (hysteresis) that also meet
    `min_samples`, or when the monitor is disabled or deleted.
  - Spike alerts: on the next quiet hour for that scope, or after 24 hours as a backstop.
- The resolve sweep runs every `ALERTS_RESOLVE_SWEEP_SECS` and also purges old data (below).

## Notification channels

Channels (`/api/notification-channels`) are `webhook` or `slack`. Routes decide which
alerts a channel receives: each route has an optional `alert_kind` (null matches all) and a
`min_severity`. A channel with several matching routes still gets one delivery per alert
event. A channel with no route receives nothing. A channel may have up to 20 routes.

Channel URLs are secrets: they are encrypted at rest, and the API returns only a hint
(host plus the last few path characters). HMAC secrets (webhook only, up to 256
characters) are encrypted at rest and never returned; the API exposes `has_hmac_secret`.

### Delivery and retries

- Transactional outbox: the dispatcher claims due rows (`FOR UPDATE SKIP LOCKED`), sends,
  and records the outcome with a guarded update.
- At-least-once. If a worker dies after sending but before recording, the row is reclaimed
  once the claim is stale (2 minutes) and sent again. A guarded completion means a
  reclaimed claim cannot be overwritten by the original worker. `X-Nasiko-Delivery` is the
  outbox row id and stays stable across retries, so receivers can dedupe on it.
- Batches of 20 rows, sent 5 at a time, each send capped at 10 seconds (worst case 40
  seconds per batch, inside the 2-minute stale-claim window).
- Retry schedule after a failed attempt: 30s, 2m, 10m, 30m, 2h. After the 6th attempt the
  row is marked `failed`. Terminal failures (channel disabled or deleted, undecryptable
  config) are failed immediately. A missing master key stays retryable.
- `last_error` holds a short slug, never the URL or a response body.
- Finished rows are purged after 30 days. `llm_call_failures` rows are also kept 30 days.
- `GET /api/notification-deliveries` lists history (filter by `alert_id` or `channel_id`).

### Webhook payload

```json
{
  "event": "opened",
  "alert": {
    "id": "7f0c1a4e-...",
    "kind": "spend_spike",
    "severity": "warning",
    "scope": "agent",
    "scope_ref": "3b9d...",
    "title": "Spend spike on agent-a",
    "message": "...",
    "link": "/tokenops?agent=3b9d...&range=24h",
    "first_seen_at": "2026-10-01T11:00:00.000000Z",
    "last_seen_at": "2026-10-01T11:00:00.000000Z",
    "occurrences": 1,
    "status": "open"
  },
  "sent_at": "2026-10-01T11:05:00Z"
}
```

`event` is `opened`, `escalated`, `resolved` or `test`. Headers on every webhook:

| Header | Value |
|---|---|
| `X-Nasiko-Event` | The event name. |
| `X-Nasiko-Delivery` | Outbox row id, stable across retries. Dedupe on this. |
| `X-Nasiko-Timestamp` | Unix seconds at send time. |
| `X-Nasiko-Signature` | `sha256=<hex>` when the channel has an HMAC secret. |

### Verifying the HMAC signature

The signature is `sha256=` plus the lowercase hex of
`HMAC_SHA256(secret, "{X-Nasiko-Timestamp}.{raw_body}")`. Verify over the exact raw bytes
received (do not re-serialize the JSON), compare in constant time, and reject timestamps
older than 5 minutes to stop replays.

```python
import hashlib, hmac, time

TOLERANCE_SECS = 300

def verify(secret: bytes, headers, raw_body: bytes) -> bool:
    ts = headers["X-Nasiko-Timestamp"]
    if abs(time.time() - int(ts)) > TOLERANCE_SECS:
        return False
    expected = "sha256=" + hmac.new(
        secret, ts.encode() + b"." + raw_body, hashlib.sha256
    ).hexdigest()
    return hmac.compare_digest(expected, headers["X-Nasiko-Signature"])
```

### Slack

Slack channels post a fallback `text` plus Block Kit `blocks` (a label such as CRITICAL or
RESOLVED with the title, the message, and an "Open in Nasiko" link). Every interpolated
field has `&`, `<` and `>` escaped so admin-chosen names cannot inject links or mentions.
Links are absolute when `ALERTS_PUBLIC_BASE_URL` (or `APP_BASE_URL`) is set; otherwise the
relative path is shown as text. Slack channels are not signed.

## SSRF policy

The server fetches channel URLs from inside the deployment network, so they are checked
when stored, right before every send (DNS can change), and at connect time by a custom
resolver.

- `https` only. Credentials in the URL (`user:pass@`) are rejected.
- No private, loopback, link-local or cloud-metadata addresses, including IP literals
  (exotic spellings such as `0x7f.1` or `2130706433` are normalized by the URL parser
  first). `localhost` and `*.localhost` are rejected. The address definition is shared with
  the MCP gateway (`is_blocked_ip`).
- A hostname that resolves to any blocked address is refused at send time.
- Slack channels must use host `hooks.slack.com`; an IP literal is never accepted.
- Redirects are never followed.
- `ALERTS_ALLOW_PRIVATE_URLS=true` relaxes the https requirement, the address checks and
  the Slack host pin together. It is for development and tests only; never enable it in
  production.

## Configuration

| Variable | Default | Meaning |
|---|---|---|
| `ALERTS_BUDGET_EVENTS_SECS` | `15` | Poll interval turning budget events into alerts. `0` disables the worker. |
| `ALERTS_RESOLVE_SWEEP_SECS` | `60` | Resolver and retention sweep interval. `0` disables. |
| `ALERTS_OUTBOX_SECS` | `5` | Outbox dispatcher poll interval. `0` disables. |
| `ALERTS_MONITORS_SECS` | `60` | Monitor evaluation interval. `0` disables. |
| `ALERTS_SPIKE_SECS` | `300` | Spike detector poll interval (evaluates once per complete hour). `0` disables. |
| `ALERTS_SPIKE_SIGMA` | `3.0` | Standard deviations above the 168h mean. |
| `ALERTS_SPIKE_FLOOR_USD` | `5.0` | Hourly spend below which a spike is never raised. |
| `ALERTS_PUBLIC_BASE_URL` | falls back to `APP_BASE_URL`, then empty | Origin prepended to alert links in Slack messages. |
| `ALERTS_ALLOW_PRIVATE_URLS` | `false` | Dev/test only: relax the SSRF policy. |

## API

All routes are under `/api`.

| Route | Who | Purpose |
|---|---|---|
| `GET /alerts` | admin | List alerts (cursor paged; filter by status, kind and more). |
| `POST /alerts/{id}/acknowledge` | admin | Acknowledge an alert. |
| `GET/POST /monitors`, `GET/PUT/DELETE /monitors/{id}` | admin | Monitor CRUD. |
| `GET/POST /notification-channels`, `GET/PUT/DELETE /notification-channels/{id}` | admin | Channel CRUD. |
| `GET/PUT /notification-channels/{id}/routes` | admin | Read or replace a channel's routes. |
| `POST /notification-channels/{id}/test` | admin | Send a test notification; rate limited to 10 per minute per caller (429 with `Retry-After`). |
| `GET /notification-deliveries` | admin | Delivery history. |
| `GET /alerts/spike-markers` | any authenticated user | Spike markers for the TokenOps chart, scoped like the FinOps endpoints. |

Admin routes check the admin role in each handler (the outer `require_user_manager` layer
is allow-all in OSS). Errors use `{ "error": "...", "code": "..." }`.

## TokenOps spike markers

`GET /alerts/spike-markers?range=|start_time=&end_time=&agent_id=` returns up to 500
markers, newest first, with `alert_id`, `hour_start`, `scope`, `scope_ref`, `agent_id`,
`severity`, `spend_usd`, `threshold_usd` and `title`.

- Admins see platform and agent markers. Other users see only markers for agents they can
  access and never platform markers. An `agent_id` the caller cannot access returns the
  same 404 as an unknown agent.
- The TokenOps spend chart draws markers as anomalies on the Spend series, bucketed in UTC:
  hour buckets for the 24h range, day buckets otherwise. Markers with no matching chart
  point are dropped and markers in the same bucket collapse into one with joined notes.
  A marker fetch failure never breaks the chart.
- The detector reads `token_usage` while the chart reads `trace_usage`, so the marker's
  spend and the plotted bar can differ.
- Alert links open TokenOps preselected: `/tokenops?agent=<uuid>&range=24h`. Only
  `24h`, `7d` and `30d` are accepted for `range`.
- Budget alerts link to `/tokenops?agent=<uuid>&range=<24h|7d|30d>` for agent budgets and
  `/tokenops?range=<r>` otherwise (range from the budget period: daily 24h, weekly 7d,
  monthly 30d). `details.budget_url = "/budgets"` is shown as a secondary Budget button on
  the Alerts page. Alerts raised before this change keep their stored `/budgets` link.

## Known limitations

- A sustained spike raises one alert per hour (the dedup key carries the hour), not a
  single long-lived alert.
- `error_rate` counts provider 4xx responses as failures. Filtering by `error_kind` is
  possible later.
- Delivery is at-least-once, never exactly-once: receivers must dedupe on
  `X-Nasiko-Delivery`.
- The chart (`trace_usage`) and the detector (`token_usage`) use different data sources,
  so values can differ.
- Failure rows record the primary resolved model, while successful calls that fell back
  record the effective model. A per-model `error_rate` monitor therefore attributes
  failures to the model that was asked for and successes to the model that answered.
- Timeout classification relies on the transport error text.
- SSRF residual gaps: IPv6 transition prefixes that embed an IPv4 address (6to4
  `2002::/16`, NAT64 `64:ff9b::/96`) are not blocked, and `0.0.0.0/8` beyond `0.0.0.0`
  itself is not blocked.
- Behind an egress proxy, DNS is resolved by the proxy, so the connect-time resolver and
  the pre-send DNS check do not see the address actually connected to. Enforce destination
  rules at the proxy.
