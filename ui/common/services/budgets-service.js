/**
 * Budget data functions: admin CRUD over `/api/budgets` and the caller's own
 * read-only status from `/api/budgets/me`. The server enforces the admin gate;
 * these functions are plain transports.
 */

import { fetchApi, postJson, putJson, deleteJson } from '/common/services/api.js';
import { registerAll } from '/common/core/data-sources.js';

const fetchBudgets = async () => fetchApi('/budgets');

const fetchMyBudgets = async () => fetchApi('/budgets/me');

const createBudget = async (body) => postJson('/budgets', body);

const updateBudget = async ({ id, ...body }) =>
  putJson(`/budgets/${encodeURIComponent(id)}`, body);

const deleteBudget = async ({ id }) => deleteJson(`/budgets/${encodeURIComponent(id)}`);

// Target picker for user-scoped budgets. GET /api/users is admin-only, which
// matches the only caller (the admin dialog).
const USER_PICKER_LIMIT = 200;
const fetchBudgetUsers = async () => fetchApi(`/users?limit=${USER_PICKER_LIMIT}`);

registerAll(
  { fetchBudgets, fetchMyBudgets, createBudget, updateBudget, deleteBudget, fetchBudgetUsers },
  { replace: true },
);
