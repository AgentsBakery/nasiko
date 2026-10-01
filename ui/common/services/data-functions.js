/**
 * Shared data functions for every control-plane page, in both editions.
 *
 * Phase 3B: this file is now a barrel — each domain has its own module under
 * `services/`. Every module calls `registerAll` on import, so importing this
 * barrel once still registers the full set, exactly as the monolith did.
 *
 * Domain modules:
 *   agents-service.js      — agent registry, containers, builds
 *   sessions-service.js    — chat sessions
 *   flows-service.js       — flow listing and detail
 *   workflows-service.js   — MAF workflow CRUD, executions, generation
 *   observability-service.js — traces, spans, session history, resources
 *   llm-service.js         — LLM router configs, providers, secrets
 *   usage-service.js       — usage summary, TokenOps, by-agent/model
 *   settings-service.js    — workspace settings, user search
 *   budgets-service.js — budget CRUD and caller budget status
 *   mcp-service.js         — MCP gateway (connectors, credentials, OAuth,
 *                            toolkits, connections, per-agent access)
 */

import '/common/services/agents-service.js';
import '/common/services/sessions-service.js';
import '/common/services/flows-service.js';
import '/common/services/workflows-service.js';
import '/common/services/observability-service.js';
import '/common/services/llm-service.js';
import '/common/services/usage-service.js';
import '/common/services/settings-service.js';
import '/common/services/budgets-service.js';
import '/common/services/mcp-service.js';
