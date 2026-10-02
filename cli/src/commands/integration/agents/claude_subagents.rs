//! Claude Code subagent transcripts turned into scoped turns of the parent session.
//!
//! Layout: next to the main transcript `<session>.jsonl`, Claude Code writes
//! `<session>/subagents/agent-<agentId>.jsonl` (every record `isSidechain: true`,
//! parent `sessionId`) plus `agent-<agentId>.meta.json`
//! (`agentType`, `description`, `toolUseId`, `spawnDepth`, ...).
//!
//! Linkage: `meta.toolUseId` is the id of the `Agent` (older: `Task`) tool call that
//! spawned the run. Found in the main transcript, the parent is the main agent;
//! found in another subagent's transcript, that agent is the parent (nested).
//! The id is only a lookup key: no path is ever built from file contents.
//!
//! Each run segment (the first prompt, then each coordinator resume) is its own
//! immutable event, `turn.id = subagent:<agentId>:<segment start uuid>`. Receipts
//! cannot be amended, and a background run usually finishes long after the parent
//! turn was sent, so a segment is emitted only on a terminal signal (a later
//! segment, a terminal `<task-notification>`, or a completed/failed spawn result)
//! and never on elapsed time. A run still going when the user quits stays
//! uncaptured until a later Stop sees its signal.
//!
//! Usage comes from the subagent's own call records only. Parent-side summaries
//! (`toolUseResult.usage`/`totalTokens`, `<subagent_tokens>`) describe the last
//! call's context, not the run, and are never read as usage. Calls whose records
//! only carry the start-of-stream usage snapshot (`stop_reason: null` on every
//! observation, Claude Code issues #97763/#84223) report output as a lower bound
//! with `accounting.output_tokens_final = Some(false)`. A call with a final
//! observation leaves the flag absent, which already means final.
//!
//! Teammates are not captured here yet (Plan 08): a meta is teammate-shaped only
//! when `toolUseId` is absent or `taskKind == "in_process_teammate"`. A `name`
//! alone never excludes an ordinary subagent; it is kept as content.

use chrono::{DateTime, Utc};
use nasiko_types::{
    CODING_AGENT_ID_MAX_BYTES, CODING_AGENT_NAME_MAX_BYTES,
    CODING_AGENT_SCOPE_DESCRIPTION_MAX_BYTES, CodingAgentScope, CodingAgentScopeKind,
    CodingAgentToolAssociation,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::Instant;

use super::super::model::{ScopedTurn, Turn};
use super::claude::{AssistantIndexes, Entry, apply_tool_results, attach_assistant, parse_entry};

const SUBAGENTS_DIR: &str = "subagents";
const FILE_PREFIX: &str = "agent-";
const TRANSCRIPT_SUFFIX: &str = ".jsonl";
const META_SUFFIX: &str = ".meta.json";
/// Agent ids are short hex today; the cap keeps file names and event ids bounded.
const MAX_AGENT_ID_CHARS: usize = 64;
/// At most this many agents are read per report (sorted by agentId).
const MAX_SUBAGENT_FILES: usize = 500;
/// Larger transcripts are skipped rather than blowing the hook budget.
const MAX_SUBAGENT_FILE_BYTES: u64 = 64 * 1024 * 1024;
const MAX_SUBAGENT_META_BYTES: u64 = 64 * 1024;
/// Tool names that spawn a subagent (`Task` in older Claude Code versions).
const SPAWN_TOOL_NAMES: &[&str] = &["Agent", "Task"];
const HANDBACK_TOOL_NAME: &str = "SubagentHandback";
const TEAMMATE_TASK_KIND: &str = "in_process_teammate";
const COORDINATOR_ORIGIN: &str = "coordinator";
const TASK_NOTIFICATION_ORIGIN: &str = "task-notification";
const COMPLETED_SPAWN_STATUS: &str = "completed";
const TERMINAL_NOTIFICATION_STATUSES: &[&str] = &["completed", "failed", "killed", "stopped"];

/// Persisted per-agent progress (watermark `subagent_files`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubagentFileProgress {
    /// Transcript size when last read.
    pub size: u64,
    /// Call identities this agent's captured segments reported. They seed the
    /// cross-file dedupe even when the file itself is not re-read.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub call_ids: Vec<String>,
    /// `Agent`/`Task` tool-use ids in this transcript, for nested linkage.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub spawn_tool_use_ids: Vec<String>,
    /// Every segment was terminal and captured: an unchanged size skips the read.
    #[serde(default)]
    pub complete: bool,
}

/// What one subagent file contributed to a scan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScannedFile {
    pub size: u64,
    /// Every run segment had a terminal signal.
    pub all_terminal: bool,
    pub spawn_tool_use_ids: Vec<String>,
}

#[derive(Debug, Default)]
pub struct SubagentScan {
    /// Finished run segments, in agentId then segment order.
    pub scoped_turns: Vec<ScopedTurn>,
    /// Files read in this scan (skipped files are absent).
    pub files: BTreeMap<String, ScannedFile>,
}

/// Scan the subagents directory next to `transcript_path`.
///
/// `main_content` is the main transcript (spawn ids and terminal signals);
/// `main_turns` are its parsed turns (their call ids are never re-reported);
/// `progress` is the persisted per-agent state. Unreadable or unsafe entries are
/// skipped silently: a hook must never fail the coding agent over them.
pub fn scan_subagents(
    transcript_path: &Path,
    main_content: &str,
    main_turns: &[Turn],
    deadline: Instant,
    progress: &BTreeMap<String, SubagentFileProgress>,
) -> SubagentScan {
    let dir = transcript_path.with_extension("").join(SUBAGENTS_DIR);
    let Some(candidates) = discover(&dir) else {
        return SubagentScan::default();
    };

    let mut signals = Signals::default();
    signals.collect(main_content, None);
    for (agent_id, file) in progress {
        for id in &file.spawn_tool_use_ids {
            signals
                .spawn_parents
                .entry(id.clone())
                .or_insert_with(|| Some(agent_id.clone()));
        }
    }

    let mut agents = Vec::new();
    for (agent_id, candidate) in candidates {
        let (Some(transcript), Some(meta)) = (candidate.transcript, candidate.meta) else {
            continue;
        };
        if progress
            .get(&agent_id)
            .is_some_and(|known| known.complete && known.size == transcript.size)
        {
            continue;
        }
        if Instant::now() >= deadline {
            break;
        }
        let Some(meta) = read_meta(&meta.path) else {
            continue;
        };
        if meta.is_teammate() {
            continue;
        }
        let Some(content) = read_bounded(&transcript.path, MAX_SUBAGENT_FILE_BYTES) else {
            continue;
        };
        signals.collect(&content, Some(&agent_id));
        let parsed = parse_agent(&agent_id, &content);
        agents.push((agent_id, meta, content.len() as u64, parsed));
    }

    let mut seen: HashSet<String> = main_turns
        .iter()
        .flat_map(|turn| turn.calls.iter().map(|call| call.uuid.clone()))
        .collect();
    seen.extend(
        progress
            .values()
            .flat_map(|file| file.call_ids.iter().cloned()),
    );

    let mut scan = SubagentScan::default();
    for (agent_id, meta, size, parsed) in agents {
        let segment_count = parsed.segments.len();
        let mut all_terminal = true;
        for (index, segment) in parsed.segments.into_iter().enumerate() {
            let terminal = index + 1 < segment_count
                || signals.notified_after(&agent_id, segment.last_at)
                || (index == 0
                    && meta
                        .tool_use_id
                        .as_ref()
                        .is_some_and(|id| signals.finished_spawns.contains(id)));
            let turn = exclude_seen_calls(segment.turn, &mut seen);
            if !terminal {
                all_terminal = false;
                continue;
            }
            if turn.calls.is_empty() && turn.tool_calls.is_empty() {
                continue;
            }
            scan.scoped_turns.push(ScopedTurn {
                scope: scope_for(&agent_id, &meta, &signals),
                turn,
            });
        }
        scan.files.insert(
            agent_id,
            ScannedFile {
                size,
                all_terminal,
                spawn_tool_use_ids: parsed.spawn_tool_use_ids,
            },
        );
    }
    scan
}

// ─── discovery ───────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FileKind {
    Transcript,
    Meta,
}

struct FoundFile {
    path: PathBuf,
    size: u64,
}

#[derive(Default)]
struct Candidate {
    transcript: Option<FoundFile>,
    meta: Option<FoundFile>,
}

/// Regular files with allow-listed names directly inside `dir`, by agentId.
fn discover(dir: &Path) -> Option<BTreeMap<String, Candidate>> {
    // The directory itself must not redirect the read elsewhere.
    if !std::fs::symlink_metadata(dir).ok()?.is_dir() {
        return None;
    }
    let mut candidates: BTreeMap<String, Candidate> = BTreeMap::new();
    for entry in std::fs::read_dir(dir).ok()?.flatten() {
        let name = entry.file_name();
        let Some((agent_id, kind)) = name.to_str().and_then(classify_name) else {
            continue;
        };
        let path = dir.join(&name);
        let Ok(metadata) = std::fs::symlink_metadata(&path) else {
            continue;
        };
        let limit = match kind {
            FileKind::Transcript => MAX_SUBAGENT_FILE_BYTES,
            FileKind::Meta => MAX_SUBAGENT_META_BYTES,
        };
        if !metadata.file_type().is_file() || metadata.len() > limit {
            continue;
        }
        let found = FoundFile {
            path,
            size: metadata.len(),
        };
        let candidate = candidates.entry(agent_id.to_string()).or_default();
        match kind {
            FileKind::Transcript => candidate.transcript = Some(found),
            FileKind::Meta => candidate.meta = Some(found),
        }
    }
    Some(candidates.into_iter().take(MAX_SUBAGENT_FILES).collect())
}

/// `agent-<id>.jsonl` / `agent-<id>.meta.json`, id `[A-Za-z0-9_-]{1,64}`.
fn classify_name(name: &str) -> Option<(&str, FileKind)> {
    let rest = name.strip_prefix(FILE_PREFIX)?;
    let (agent_id, kind) = if let Some(id) = rest.strip_suffix(META_SUFFIX) {
        (id, FileKind::Meta)
    } else {
        (rest.strip_suffix(TRANSCRIPT_SUFFIX)?, FileKind::Transcript)
    };
    let valid = !agent_id.is_empty()
        && agent_id.len() <= MAX_AGENT_ID_CHARS
        && agent_id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-');
    valid.then_some((agent_id, kind))
}

/// Read at most `limit` bytes without following a symlink swapped in after
/// discovery. Oversized or unreadable files yield `None`.
fn read_bounded(path: &Path, limit: u64) -> Option<String> {
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    let file = options.open(path).ok()?;
    let mut bytes = Vec::new();
    file.take(limit + 1).read_to_end(&mut bytes).ok()?;
    if bytes.len() as u64 > limit {
        return None;
    }
    // A line still being written may end mid-character; it fails to parse anyway.
    Some(String::from_utf8_lossy(&bytes).into_owned())
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Meta {
    agent_type: Option<String>,
    description: Option<String>,
    tool_use_id: Option<String>,
    spawn_depth: Option<u32>,
    name: Option<String>,
    task_kind: Option<String>,
}

impl Meta {
    fn is_teammate(&self) -> bool {
        self.tool_use_id.is_none() || self.task_kind.as_deref() == Some(TEAMMATE_TASK_KIND)
    }
}

/// A meta without `agentType` belongs to an internal agent and is skipped.
fn read_meta(path: &Path) -> Option<Meta> {
    let content = read_bounded(path, MAX_SUBAGENT_META_BYTES)?;
    let meta: Meta = serde_json::from_str(&content).ok()?;
    meta.agent_type
        .as_deref()
        .is_some_and(|agent_type| !agent_type.trim().is_empty())
        .then_some(meta)
}

// ─── linkage and terminal signals ────────────────────────────────────────────

#[derive(Default)]
struct Signals {
    /// Spawn tool-use id -> spawning agent (`None` = main agent).
    spawn_parents: HashMap<String, Option<String>>,
    /// Spawn tool-use ids whose result says the run completed or failed.
    finished_spawns: HashSet<String>,
    /// agentId -> timestamps of terminal task notifications.
    notifications: HashMap<String, Vec<Option<DateTime<Utc>>>>,
}

impl Signals {
    /// Collect spawn ids, spawn results and notifications from one transcript.
    /// Only lines that can carry them are parsed (the main transcript is large).
    fn collect(&mut self, content: &str, owner: Option<&str>) {
        let spawn_names: Vec<String> = SPAWN_TOOL_NAMES
            .iter()
            .map(|name| format!("\"{name}\""))
            .collect();
        for line in content.lines() {
            let relevant = line.contains("<task-notification>")
                || line.contains("\"tool_result\"")
                || spawn_names.iter().any(|name| line.contains(name.as_str()));
            if !relevant {
                continue;
            }
            let Ok(record) = serde_json::from_str::<Value>(line) else {
                continue;
            };
            self.collect_record(&record, owner);
        }
    }

    fn collect_record(&mut self, record: &Value, owner: Option<&str>) {
        let at = record["timestamp"]
            .as_str()
            .and_then(|at| at.parse::<DateTime<Utc>>().ok());
        let content = &record["message"]["content"];
        match record["type"].as_str() {
            Some("queue-operation") => {
                if let Some(text) = record["content"].as_str() {
                    self.collect_notifications(text, at);
                }
            }
            Some("user") => {
                if record["origin"]["kind"] == TASK_NOTIFICATION_ORIGIN {
                    self.collect_notifications(&text_of(content), at);
                }
                let completed = record["toolUseResult"]["status"] == COMPLETED_SPAWN_STATUS;
                for block in blocks(content).filter(|block| block["type"] == "tool_result") {
                    let Some(id) = block["tool_use_id"].as_str() else {
                        continue;
                    };
                    if completed || block["is_error"] == true {
                        self.finished_spawns.insert(id.to_string());
                    }
                }
            }
            Some("assistant") => {
                for block in blocks(content).filter(|block| {
                    block["type"] == "tool_use"
                        && block["name"]
                            .as_str()
                            .is_some_and(|name| SPAWN_TOOL_NAMES.contains(&name))
                }) {
                    if let Some(id) = block["id"].as_str() {
                        self.spawn_parents
                            .entry(id.to_string())
                            .or_insert_with(|| owner.map(str::to_string));
                    }
                }
            }
            _ => {}
        }
    }

    fn collect_notifications(&mut self, text: &str, at: Option<DateTime<Utc>>) {
        const OPEN: &str = "<task-notification>";
        const CLOSE: &str = "</task-notification>";
        let mut rest = text;
        while let Some(start) = rest.find(OPEN) {
            let body = &rest[start + OPEN.len()..];
            let end = body.find(CLOSE).unwrap_or(body.len());
            let notification = &body[..end];
            rest = &body[end..];
            let (Some(task_id), Some(status)) = (
                tag_text(notification, "task-id"),
                tag_text(notification, "status"),
            ) else {
                continue;
            };
            if TERMINAL_NOTIFICATION_STATUSES.contains(&status) {
                self.notifications
                    .entry(task_id.to_string())
                    .or_default()
                    .push(at);
            }
        }
    }

    /// A terminal notification for `agent_id` at or after the segment's last record.
    fn notified_after(&self, agent_id: &str, last_at: Option<DateTime<Utc>>) -> bool {
        self.notifications.get(agent_id).is_some_and(|times| {
            times.iter().any(|at| match (at, last_at) {
                (Some(at), Some(last)) => *at >= last,
                (_, None) => true,
                (None, Some(_)) => false,
            })
        })
    }
}

fn tag_text<'a>(text: &'a str, tag: &str) -> Option<&'a str> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let start = text.find(&open)? + open.len();
    let end = text[start..].find(&close)? + start;
    Some(text[start..end].trim())
}

fn blocks(content: &Value) -> impl Iterator<Item = &Value> {
    content.as_array().into_iter().flatten()
}

fn text_of(content: &Value) -> String {
    content.as_str().map(str::to_string).unwrap_or_else(|| {
        blocks(content)
            .filter_map(|block| block["text"].as_str())
            .collect::<Vec<_>>()
            .join("\n")
    })
}

fn scope_for(agent_id: &str, meta: &Meta, signals: &Signals) -> CodingAgentScope {
    let parent_tool_call_id = meta
        .tool_use_id
        .clone()
        .filter(|id| id.len() <= CODING_AGENT_ID_MAX_BYTES);
    let parent_agent_id = parent_tool_call_id
        .as_ref()
        .and_then(|id| signals.spawn_parents.get(id).cloned().flatten())
        .filter(|parent| parent != agent_id);
    CodingAgentScope {
        kind: CodingAgentScopeKind::Subagent,
        agent_id: agent_id.to_string(),
        agent_type: bounded(meta.agent_type.as_deref(), CODING_AGENT_NAME_MAX_BYTES),
        parent_tool_call_id,
        parent_agent_id,
        spawn_depth: meta.spawn_depth,
        description: bounded(
            meta.description.as_deref(),
            CODING_AGENT_SCOPE_DESCRIPTION_MAX_BYTES,
        ),
        name: bounded(meta.name.as_deref(), CODING_AGENT_NAME_MAX_BYTES),
    }
}

/// Trimmed, nonblank, cut to `max` bytes on a char boundary.
fn bounded(value: Option<&str>, max: usize) -> Option<String> {
    let value = value?.trim();
    let mut end = value.len().min(max);
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    let value = value[..end].trim_end();
    (!value.is_empty()).then(|| value.to_string())
}

// ─── run segments ────────────────────────────────────────────────────────────

struct Segment {
    turn: Turn,
    /// Timestamp of the segment's last user/assistant record.
    last_at: Option<DateTime<Utc>>,
}

struct ParsedAgent {
    segments: Vec<Segment>,
    spawn_tool_use_ids: Vec<String>,
}

/// Split one subagent transcript into run segments with their calls and tools.
/// Call dedupe and final-observation selection are the main parser's.
fn parse_agent(agent_id: &str, content: &str) -> ParsedAgent {
    let mut turns: Vec<Turn> = Vec::new();
    let mut last_at: Vec<Option<DateTime<Utc>>> = Vec::new();
    let mut handbacks: Vec<Option<String>> = Vec::new();
    let mut last_texts: Vec<Option<String>> = Vec::new();
    let mut spawn_tool_use_ids = Vec::new();
    let mut owners: HashMap<String, usize> = HashMap::new();
    let mut calls: HashMap<String, (usize, usize, bool, DateTime<Utc>)> = HashMap::new();
    let mut tools: HashMap<String, (usize, usize)> = HashMap::new();
    let mut fallback_turns: HashSet<usize> = HashSet::new();
    let mut seen_records: HashSet<String> = HashSet::new();
    let mut previous_at: Option<DateTime<Utc>> = None;

    for (line_index, line) in content.lines().enumerate() {
        let Some(entry) = parse_entry(line) else {
            continue;
        };
        let duplicate = entry
            .uuid
            .as_ref()
            .is_some_and(|id| !seen_records.insert(id.clone()));
        if duplicate && entry.kind != "assistant" {
            continue;
        }
        if starts_segment(&entry, turns.is_empty()) {
            let at = entry
                .timestamp
                .or(previous_at)
                .unwrap_or(DateTime::<Utc>::UNIX_EPOCH);
            turns.push(Turn {
                uuid: format!("subagent:{agent_id}:{}", entry.stable_identity(line_index)),
                prompt: entry.user_prompt().unwrap_or_default(),
                response: None,
                started_at: at,
                ended_at: at,
                calls: Vec::new(),
                tool_calls: Vec::new(),
            });
            last_at.push(entry.timestamp);
            handbacks.push(None);
            last_texts.push(None);
        }
        let Some(current) = turns.len().checked_sub(1) else {
            previous_at = entry.timestamp.or(previous_at);
            continue;
        };
        match entry.kind.as_str() {
            "assistant" => {
                let Some(at) = entry.timestamp else {
                    continue;
                };
                for block in tool_use_blocks(&entry) {
                    let name = block["name"].as_str().unwrap_or_default();
                    if SPAWN_TOOL_NAMES.contains(&name)
                        && let Some(id) = block["id"].as_str()
                        && !spawn_tool_use_ids.iter().any(|known| known == id)
                    {
                        spawn_tool_use_ids.push(id.to_string());
                    }
                    if name == HANDBACK_TOOL_NAME
                        && let Some(message) = block["input"]["message"].as_str()
                        && !message.trim().is_empty()
                    {
                        handbacks[current] = Some(message.trim().to_string());
                    }
                }
                if let Some(text) = entry.assistant_text() {
                    last_texts[current] = Some(text);
                }
                attach_assistant(
                    &entry,
                    current,
                    previous_at.unwrap_or(at),
                    at,
                    &mut turns,
                    AssistantIndexes {
                        owners: &mut owners,
                        calls: &mut calls,
                        tools: &mut tools,
                        fallback_turns: &mut fallback_turns,
                    },
                );
            }
            "user" => {
                apply_tool_results(entry.tool_results(), entry.timestamp, &mut turns, &tools);
            }
            _ => {}
        }
        if matches!(entry.kind.as_str(), "assistant" | "user")
            && let Some(at) = entry.timestamp
        {
            last_at[current] = Some(last_at[current].map_or(at, |last| last.max(at)));
        }
        previous_at = entry.timestamp.or(previous_at);
    }

    let segments = turns
        .into_iter()
        .zip(last_at)
        .zip(handbacks.into_iter().zip(last_texts))
        .map(|((mut turn, last_at), (handback, last_text))| {
            for call in &mut turn.calls {
                let final_seen = calls.get(&call.uuid).is_some_and(|entry| entry.2);
                if !final_seen && let Some(accounting) = &mut call.accounting {
                    accounting.output_tokens_final = Some(false);
                }
            }
            turn.response = handback.or(last_text);
            if let Some(last) = last_at {
                turn.ended_at = turn.ended_at.max(last);
            }
            Segment { turn, last_at }
        })
        .collect();
    ParsedAgent {
        segments,
        spawn_tool_use_ids,
    }
}

/// Segment 1: the first user record with string content and no parent.
/// Later segments: coordinator resumes. Other meta records never split a run.
fn starts_segment(entry: &Entry, first: bool) -> bool {
    if entry.kind != "user" {
        return false;
    }
    if first {
        return !entry.is_meta
            && entry.parent_uuid.is_none()
            && entry
                .message
                .as_ref()
                .and_then(|message| message.content.as_ref())
                .is_some_and(Value::is_string);
    }
    entry.is_meta
        && entry
            .origin
            .as_ref()
            .is_some_and(|origin| origin["kind"] == COORDINATOR_ORIGIN)
}

fn tool_use_blocks(entry: &Entry) -> impl Iterator<Item = &Value> {
    entry
        .message
        .as_ref()
        .and_then(|message| message.content.as_ref())
        .into_iter()
        .flat_map(blocks)
        .filter(|block| block["type"] == "tool_use")
}

/// Drop calls already reported elsewhere (main transcript, another subagent
/// file, a captured segment) and claim the rest. Tools issued by a dropped call
/// go with it; tools whose call carried no usage keep turn-level association.
fn exclude_seen_calls(mut turn: Turn, seen: &mut HashSet<String>) -> Turn {
    let excluded: HashSet<String> = turn
        .calls
        .iter()
        .filter(|call| seen.contains(&call.uuid))
        .map(|call| call.uuid.clone())
        .collect();
    seen.extend(turn.calls.iter().map(|call| call.uuid.clone()));
    turn.calls.retain(|call| !excluded.contains(&call.uuid));
    let kept: HashSet<&str> = turn.calls.iter().map(|call| call.uuid.as_str()).collect();
    let mut tool_calls = Vec::with_capacity(turn.tool_calls.len());
    for mut tool in std::mem::take(&mut turn.tool_calls) {
        match tool.model_call_id.as_deref() {
            Some(id) if excluded.contains(id) => continue,
            Some(id) if !kept.contains(id) => {
                tool.model_call_id = None;
                tool.association = CodingAgentToolAssociation::Turn;
            }
            _ => {}
        }
        tool_calls.push(tool);
    }
    turn.tool_calls = tool_calls;
    turn
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::time::Duration;

    const SESSION: &str = "parent-session";

    // ─── fixture builders (synthetic, redacted shapes only) ─────────────────

    fn at(second: u32) -> String {
        format!("2026-01-01T00:{:02}:{:02}Z", second / 60, second % 60)
    }

    fn line(value: Value) -> String {
        format!("{value}\n")
    }

    fn prompt(uuid: &str, second: u32, text: &str) -> String {
        line(json!({
            "parentUuid": null, "isSidechain": true, "agentId": "x", "type": "user",
            "message": {"role": "user", "content": text},
            "uuid": uuid, "timestamp": at(second), "sessionId": SESSION
        }))
    }

    fn resume(uuid: &str, second: u32) -> String {
        line(json!({
            "type": "user", "isMeta": true, "isSidechain": true, "agentId": "x",
            "origin": {"kind": "coordinator"}, "parentUuid": "prev",
            "message": {"role": "user", "content": "resume text"},
            "uuid": uuid, "timestamp": at(second), "sessionId": SESSION
        }))
    }

    fn reminder(uuid: &str, second: u32) -> String {
        line(json!({
            "type": "user", "isMeta": true, "isSidechain": true, "parentUuid": "prev",
            "message": {"role": "user", "content": "system reminder"},
            "uuid": uuid, "timestamp": at(second), "sessionId": SESSION
        }))
    }

    /// One assistant record per content block, start-of-stream usage.
    fn assistant(
        uuid: &str,
        request: &str,
        second: u32,
        block: Value,
        stop: Option<&str>,
    ) -> String {
        line(json!({
            "parentUuid": "prev", "isSidechain": true, "agentId": "x", "type": "assistant",
            "requestId": request,
            "message": {
                "model": "claude-test", "id": format!("msg-{request}"), "role": "assistant",
                "content": [block], "stop_reason": stop,
                "usage": {"input_tokens": 2, "cache_creation_input_tokens": 10,
                          "cache_read_input_tokens": 100, "output_tokens": 3}
            },
            "uuid": uuid, "timestamp": at(second), "sessionId": SESSION
        }))
    }

    fn text(value: &str) -> Value {
        json!({"type": "text", "text": value})
    }

    fn tool_use(id: &str, name: &str, input: Value) -> Value {
        json!({"type": "tool_use", "id": id, "name": name, "input": input})
    }

    fn tool_result(uuid: &str, second: u32, tool_id: &str, extra: Value) -> String {
        let mut record = json!({
            "type": "user", "parentUuid": "prev",
            "message": {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": tool_id, "content": "ok"}
            ]},
            "uuid": uuid, "timestamp": at(second), "sessionId": SESSION
        });
        if let Value::Object(extra) = extra {
            for (key, value) in extra {
                record[key] = value;
            }
        }
        line(record)
    }

    fn notification_op(second: u32, agent_id: &str, status: &str) -> String {
        line(json!({
            "type": "queue-operation", "operation": "enqueue", "timestamp": at(second),
            "sessionId": SESSION,
            "content": format!("<task-notification>\n<task-id>{agent_id}</task-id>\n<tool-use-id>toolu_x</tool-use-id>\n<status>{status}</status>\n<summary>done</summary>\n<usage><subagent_tokens>999999</subagent_tokens></usage>\n</task-notification>")
        }))
    }

    fn notification_user(uuid: &str, second: u32, agent_id: &str, status: &str) -> String {
        line(json!({
            "type": "user", "uuid": uuid, "timestamp": at(second), "sessionId": SESSION,
            "origin": {"kind": "task-notification", "producer": "session-task"},
            "message": {"role": "user", "content": format!("<task-notification><task-id>{agent_id}</task-id><status>{status}</status></task-notification>")}
        }))
    }

    /// Main transcript: a user prompt and an assistant spawning `tool_id`.
    fn main_with_spawn(tool_name: &str, tool_id: &str) -> String {
        let mut main = line(json!({
            "type": "user", "uuid": "m-u1", "timestamp": at(0), "sessionId": SESSION,
            "message": {"content": "do the thing"}
        }));
        main.push_str(&line(json!({
            "type": "assistant", "uuid": "m-a1", "parentUuid": "m-u1", "requestId": "m-req-1",
            "timestamp": at(1), "sessionId": SESSION,
            "message": {"model": "claude-test", "id": "msg-m-req-1", "stop_reason": "tool_use",
                        "content": [tool_use(tool_id, tool_name, json!({"subagent_type": "Explore", "description": "find X", "prompt": "find X please"}))],
                        "usage": {"input_tokens": 1, "output_tokens": 1}}
        })));
        main
    }

    fn foreground_completed(tool_id: &str, second: u32) -> String {
        tool_result(
            "m-r1",
            second,
            tool_id,
            json!({"toolUseResult": {"status": "completed", "agentId": "a1", "totalTokens": 123456,
                                     "usage": {"input_tokens": 50000, "output_tokens": 70000}}}),
        )
    }

    fn async_launched(tool_id: &str, second: u32) -> String {
        tool_result(
            "m-r1",
            second,
            tool_id,
            json!({"toolUseResult": {"isAsync": true, "status": "async_launched", "agentId": "a1"}}),
        )
    }

    /// Prompt, a Read tool call (two block lines), its result, and a handback.
    fn simple_run(prompt_uuid: &str, base: u32, request_prefix: &str) -> String {
        let r1 = format!("{request_prefix}-1");
        let r2 = format!("{request_prefix}-2");
        let mut run = prompt(prompt_uuid, base, "find X please");
        run.push_str(&assistant(
            &format!("{prompt_uuid}-a1"),
            &r1,
            base + 1,
            text("looking"),
            None,
        ));
        run.push_str(&assistant(
            &format!("{prompt_uuid}-a2"),
            &r1,
            base + 2,
            tool_use(
                &format!("{prompt_uuid}-read"),
                "Read",
                json!({"path": "redacted"}),
            ),
            None,
        ));
        run.push_str(&tool_result(
            &format!("{prompt_uuid}-r"),
            base + 3,
            &format!("{prompt_uuid}-read"),
            json!({}),
        ));
        run.push_str(&reminder(&format!("{prompt_uuid}-meta"), base + 4));
        run.push_str(&assistant(
            &format!("{prompt_uuid}-a3"),
            &r2,
            base + 5,
            tool_use(
                &format!("{prompt_uuid}-hb"),
                HANDBACK_TOOL_NAME,
                json!({"message": "X is in redacted.rs"}),
            ),
            None,
        ));
        run
    }

    struct Fixture {
        _dir: tempfile::TempDir,
        transcript: PathBuf,
        subagents: PathBuf,
    }

    impl Fixture {
        fn new() -> Self {
            let dir = tempfile::tempdir().unwrap();
            let transcript = dir.path().join(format!("{SESSION}.jsonl"));
            let subagents = dir.path().join(SESSION).join(SUBAGENTS_DIR);
            std::fs::create_dir_all(&subagents).unwrap();
            Self {
                _dir: dir,
                transcript,
                subagents,
            }
        }

        fn agent(&self, agent_id: &str, meta: Value, transcript: &str) {
            std::fs::write(
                self.subagents.join(format!("agent-{agent_id}.meta.json")),
                meta.to_string(),
            )
            .unwrap();
            std::fs::write(
                self.subagents.join(format!("agent-{agent_id}.jsonl")),
                transcript,
            )
            .unwrap();
        }

        fn scan(&self, main: &str) -> SubagentScan {
            self.scan_with(main, &BTreeMap::new(), far_deadline())
        }

        fn scan_with(
            &self,
            main: &str,
            progress: &BTreeMap<String, SubagentFileProgress>,
            deadline: Instant,
        ) -> SubagentScan {
            let main_turns = super::super::claude::turns_from_lines(main);
            scan_subagents(&self.transcript, main, &main_turns, deadline, progress)
        }
    }

    fn far_deadline() -> Instant {
        Instant::now() + Duration::from_secs(30)
    }

    fn meta(tool_use_id: &str) -> Value {
        json!({"agentType": "Explore", "description": "find X", "toolUseId": tool_use_id,
               "spawnDepth": 1, "requestShape": "foreground", "model": "sonnet"})
    }

    fn call_ids(turn: &Turn) -> Vec<String> {
        turn.calls.iter().map(|call| call.uuid.clone()).collect()
    }

    // ─── behaviour ──────────────────────────────────────────────────────────

    #[test]
    fn discovers_and_links_foreground_subagent() {
        let fixture = Fixture::new();
        fixture.agent("a1", meta("toolu_1"), &simple_run("s1", 10, "req"));
        let main = main_with_spawn("Agent", "toolu_1") + &foreground_completed("toolu_1", 30);

        let scan = fixture.scan(&main);

        assert_eq!(scan.scoped_turns.len(), 1);
        let scoped = &scan.scoped_turns[0];
        assert_eq!(scoped.turn.uuid, "subagent:a1:s1");
        assert_eq!(
            scoped.scope,
            CodingAgentScope {
                kind: CodingAgentScopeKind::Subagent,
                agent_id: "a1".into(),
                agent_type: Some("Explore".into()),
                parent_tool_call_id: Some("toolu_1".into()),
                parent_agent_id: None,
                spawn_depth: Some(1),
                description: Some("find X".into()),
                name: None,
            }
        );
        // Two block lines of req-1 are one call; req-2 is the second.
        assert_eq!(scoped.turn.calls.len(), 2);
        let tool_names: Vec<_> = scoped
            .turn
            .tool_calls
            .iter()
            .map(|t| t.name.as_str())
            .collect();
        assert_eq!(tool_names, ["Read", HANDBACK_TOOL_NAME]);
        assert_eq!(
            scoped.turn.tool_calls[0].status,
            nasiko_types::CodingAgentToolCallStatus::Succeeded
        );
        assert_eq!(scoped.turn.prompt, "find X please");
        assert_eq!(scoped.turn.response.as_deref(), Some("X is in redacted.rs"));
        assert!(scoped.turn.started_at <= scoped.turn.ended_at);
        // Parent-side summaries (totalTokens, toolUseResult.usage) are never usage.
        let output: u64 = scoped.turn.calls.iter().map(|c| c.output_tokens).sum();
        assert_eq!(output, 6);
        assert!(scan.files["a1"].all_terminal);
    }

    #[test]
    fn response_falls_back_to_last_assistant_text() {
        let fixture = Fixture::new();
        let mut run = prompt("s1", 10, "p");
        run.push_str(&assistant(
            "s1-a1",
            "req-1",
            11,
            text("final words"),
            Some("end_turn"),
        ));
        fixture.agent("a1", meta("toolu_1"), &run);
        let main = main_with_spawn("Agent", "toolu_1") + &foreground_completed("toolu_1", 30);

        let scan = fixture.scan(&main);
        assert_eq!(
            scan.scoped_turns[0].turn.response.as_deref(),
            Some("final words")
        );
    }

    #[test]
    fn background_run_deferred_until_notification() {
        let fixture = Fixture::new();
        fixture.agent("a1", meta("toolu_1"), &simple_run("s1", 10, "req"));
        let main = main_with_spawn("Agent", "toolu_1") + &async_launched("toolu_1", 2);

        let deferred = fixture.scan(&main);
        assert!(deferred.scoped_turns.is_empty());
        assert!(!deferred.files["a1"].all_terminal);

        // A notification for another task does not finish this run.
        let other = main.clone() + &notification_op(40, "b-other", "completed");
        assert!(fixture.scan(&other).scoped_turns.is_empty());
        // Neither does a notification older than the run's last record.
        let stale = main.clone() + &notification_op(5, "a1", "completed");
        assert!(fixture.scan(&stale).scoped_turns.is_empty());

        let finished = main.clone() + &notification_op(40, "a1", "completed");
        assert_eq!(fixture.scan(&finished).scoped_turns.len(), 1);
        let delivered = main + &notification_user("m-n1", 40, "a1", "killed");
        assert_eq!(fixture.scan(&delivered).scoped_turns.len(), 1);
    }

    #[test]
    fn resumed_agent_yields_two_segments() {
        let fixture = Fixture::new();
        let mut run = simple_run("s1", 10, "req");
        run.push_str(&resume("s2", 50));
        run.push_str(&assistant(
            "s2-a1",
            "req-3",
            51,
            text("resumed answer"),
            None,
        ));
        fixture.agent("a1", meta("toolu_1"), &run);
        // The spawn result completed, but that only finishes the first run.
        let main = main_with_spawn("Agent", "toolu_1") + &foreground_completed("toolu_1", 30);

        let scan = fixture.scan(&main);
        let ids: Vec<_> = scan
            .scoped_turns
            .iter()
            .map(|s| s.turn.uuid.as_str())
            .collect();
        assert_eq!(ids, ["subagent:a1:s1"]);
        assert!(!scan.files["a1"].all_terminal);

        let finished = main + &notification_op(60, "a1", "completed");
        let scan = fixture.scan(&finished);
        let ids: Vec<_> = scan
            .scoped_turns
            .iter()
            .map(|s| s.turn.uuid.as_str())
            .collect();
        assert_eq!(ids, ["subagent:a1:s1", "subagent:a1:s2"]);
        assert_eq!(scan.scoped_turns[1].turn.prompt, "resume text");
        assert_eq!(scan.scoped_turns[1].turn.calls.len(), 1);
        assert_eq!(
            scan.scoped_turns[1].turn.response.as_deref(),
            Some("resumed answer")
        );
        assert!(scan.files["a1"].all_terminal);
    }

    #[test]
    fn nested_subagent_links_parent_agent() {
        let fixture = Fixture::new();
        let mut parent_run = simple_run("s1", 10, "req");
        parent_run.push_str(&assistant(
            "s1-spawn",
            "req-9",
            20,
            tool_use(
                "toolu_nested",
                "Agent",
                json!({"subagent_type": "general-purpose"}),
            ),
            Some("tool_use"),
        ));
        fixture.agent("a1", meta("toolu_1"), &parent_run);
        let mut child_meta = meta("toolu_nested");
        child_meta["spawnDepth"] = json!(2);
        fixture.agent("a2", child_meta, &simple_run("c1", 21, "child"));
        let main = main_with_spawn("Agent", "toolu_1")
            + &foreground_completed("toolu_1", 40)
            + &notification_op(41, "a2", "completed");

        let scan = fixture.scan(&main);
        let child = scan
            .scoped_turns
            .iter()
            .find(|s| s.scope.agent_id == "a2")
            .expect("nested run captured");
        assert_eq!(child.scope.parent_agent_id.as_deref(), Some("a1"));
        assert_eq!(
            child.scope.parent_tool_call_id.as_deref(),
            Some("toolu_nested")
        );
        assert_eq!(child.scope.spawn_depth, Some(2));
        assert_eq!(scan.files["a1"].spawn_tool_use_ids, ["toolu_nested"]);

        // With the parent file skipped, its persisted spawn ids still link.
        let progress = BTreeMap::from([(
            "a1".to_string(),
            SubagentFileProgress {
                size: std::fs::metadata(fixture.subagents.join("agent-a1.jsonl"))
                    .unwrap()
                    .len(),
                call_ids: Vec::new(),
                spawn_tool_use_ids: vec!["toolu_nested".into()],
                complete: true,
            },
        )]);
        let scan = fixture.scan_with(&main, &progress, far_deadline());
        assert_eq!(scan.scoped_turns.len(), 1);
        assert_eq!(
            scan.scoped_turns[0].scope.parent_agent_id.as_deref(),
            Some("a1")
        );
    }

    #[test]
    fn output_snapshot_flagged() {
        let fixture = Fixture::new();
        let mut run = prompt("s1", 10, "p");
        run.push_str(&assistant("s1-a1", "req-1", 11, text("partial"), None));
        run.push_str(&assistant("s1-a2", "req-2", 12, text("first"), None));
        run.push_str(&assistant(
            "s1-a3",
            "req-2",
            13,
            text("final"),
            Some("end_turn"),
        ));
        fixture.agent("a1", meta("toolu_1"), &run);
        let main = main_with_spawn("Agent", "toolu_1") + &foreground_completed("toolu_1", 30);

        let turn = &fixture.scan(&main).scoped_turns[0].turn;
        let finals: Vec<_> = turn
            .calls
            .iter()
            .map(|call| call.accounting.as_ref().unwrap().output_tokens_final)
            .collect();
        // Snapshot-only call is a lower bound; a final observation leaves the flag absent.
        assert_eq!(finals, [Some(false), None]);
    }

    #[test]
    fn main_call_ids_excluded() {
        let fixture = Fixture::new();
        // Old layouts wrote sidechain calls inline: the same request in main.
        fixture.agent("a1", meta("toolu_1"), &simple_run("s1", 10, "m-req"));
        let main = main_with_spawn("Agent", "toolu_1") + &foreground_completed("toolu_1", 30);

        let turn = &fixture.scan(&main).scoped_turns[0].turn;
        let main_call = super::super::claude::turns_from_lines(&main)[0].calls[0]
            .uuid
            .clone();
        assert!(!call_ids(turn).contains(&main_call));
        assert_eq!(turn.calls.len(), 1);
        // The Read tool came from the excluded call and leaves with it.
        assert!(turn.tool_calls.iter().all(|t| t.name != "Read"));
    }

    #[test]
    fn cross_file_duplicate_calls_counted_once() {
        let fixture = Fixture::new();
        fixture.agent("a1", meta("toolu_1"), &simple_run("s1", 10, "req"));
        // A fork copies a1's history and adds one call of its own.
        let mut fork = simple_run("s1", 10, "req");
        fork.push_str(&assistant(
            "f-a9",
            "fork-req",
            25,
            text("fork answer"),
            None,
        ));
        let fork = fork.replace("\"uuid\":\"s1\"", "\"uuid\":\"f1\"");
        fixture.agent("a2", meta("toolu_2"), &fork);
        let main = main_with_spawn("Agent", "toolu_1")
            + &foreground_completed("toolu_1", 30)
            + &notification_op(31, "a2", "completed");

        let scan = fixture.scan(&main);
        let total: usize = scan.scoped_turns.iter().map(|s| s.turn.calls.len()).sum();
        assert_eq!(total, 3);
        let a1 = scan
            .scoped_turns
            .iter()
            .find(|s| s.scope.agent_id == "a1")
            .unwrap();
        let a2 = scan
            .scoped_turns
            .iter()
            .find(|s| s.scope.agent_id == "a2")
            .unwrap();
        assert_eq!(a1.turn.calls.len(), 2);
        assert_eq!(a2.turn.calls.len(), 1);
    }

    #[test]
    fn skipped_file_identities_still_dedupe() {
        let fixture = Fixture::new();
        fixture.agent("a1", meta("toolu_1"), &simple_run("s1", 10, "req"));
        let main = main_with_spawn("Agent", "toolu_1") + &foreground_completed("toolu_1", 30);
        let first = fixture.scan(&main);
        let a1_ids = call_ids(&first.scoped_turns[0].turn);
        assert_eq!(a1_ids.len(), 2);

        // a1 is fully captured; a nested/fork copy a0 (sorted first) appears later.
        let mut copy = simple_run("s1", 10, "req");
        copy.push_str(&assistant(
            "c-a9",
            "copy-req",
            25,
            text("copy answer"),
            None,
        ));
        let copy = copy.replace("\"uuid\":\"s1\"", "\"uuid\":\"c1\"");
        fixture.agent("a0", meta("toolu_2"), &copy);
        let progress = BTreeMap::from([(
            "a1".to_string(),
            SubagentFileProgress {
                size: first.files["a1"].size,
                call_ids: a1_ids.clone(),
                spawn_tool_use_ids: Vec::new(),
                complete: true,
            },
        )]);
        let main = main + &notification_op(31, "a0", "completed");

        let scan = fixture.scan_with(&main, &progress, far_deadline());
        assert!(
            !scan.files.contains_key("a1"),
            "unchanged complete file is not re-read"
        );
        assert_eq!(scan.scoped_turns.len(), 1);
        let only = &scan.scoped_turns[0].turn;
        assert_eq!(only.calls.len(), 1);
        assert!(!a1_ids.contains(&only.calls[0].uuid));
    }

    #[test]
    fn skips_unsafe_or_foreign_files() {
        let fixture = Fixture::new();
        let run = simple_run("s1", 10, "req");
        let main = main_with_spawn("Agent", "toolu_1")
            + &foreground_completed("toolu_1", 30)
            + &[
                "ok",
                "named",
                "nometa",
                "noagenttype",
                "team1",
                "team2",
                "linked",
                "big",
            ]
            .iter()
            .map(|id| notification_op(40, id, "completed"))
            .collect::<String>();
        // A real subagent, for contrast.
        fixture.agent("ok", meta("toolu_1"), &run);
        // A named ordinary subagent (name AND toolUseId) is still a subagent.
        let mut named = meta("toolu_named");
        named["name"] = json!("researcher");
        fixture.agent("named", named, &run.replace("req-", "named-"));
        // Transcript without meta.
        std::fs::write(fixture.subagents.join("agent-nometa.jsonl"), &run).unwrap();
        // Meta without agentType (internal agent).
        fixture.agent(
            "noagenttype",
            json!({"toolUseId": "toolu_x"}),
            &run.replace("req-", "nat-"),
        );
        // Teammate-shaped metas.
        fixture.agent(
            "team1",
            json!({"agentType": "x", "name": "t"}),
            &run.replace("req-", "t1-"),
        );
        let mut teammate = meta("toolu_t");
        teammate["taskKind"] = json!(TEAMMATE_TASK_KIND);
        fixture.agent("team2", teammate, &run.replace("req-", "t2-"));
        // Foreign names.
        for name in [
            "agent-.jsonl",
            "agent-a.b.jsonl",
            "other-a1.jsonl",
            "agent-a1.json",
            "agent-ü.jsonl",
        ] {
            std::fs::write(fixture.subagents.join(name), &run).unwrap();
        }
        let long_id = "a".repeat(MAX_AGENT_ID_CHARS + 1);
        fixture.agent(&long_id, meta("toolu_1"), &run.replace("req-", "long-"));
        // Symlinked transcript and meta pointing outside the directory.
        #[cfg(unix)]
        {
            let outside = fixture._dir.path().join("outside.jsonl");
            std::fs::write(&outside, run.replace("req-", "out-")).unwrap();
            let outside_meta = fixture._dir.path().join("outside.meta.json");
            std::fs::write(&outside_meta, meta("toolu_1").to_string()).unwrap();
            std::os::unix::fs::symlink(&outside, fixture.subagents.join("agent-linked.jsonl"))
                .unwrap();
            std::fs::write(
                fixture.subagents.join("agent-linked.meta.json"),
                meta("toolu_1").to_string(),
            )
            .unwrap();
            std::os::unix::fs::symlink(
                &outside_meta,
                fixture.subagents.join("agent-linkedmeta.meta.json"),
            )
            .unwrap();
            std::fs::write(
                fixture.subagents.join("agent-linkedmeta.jsonl"),
                run.replace("req-", "lm-"),
            )
            .unwrap();
        }
        // Oversized transcript (sparse file, never actually read).
        let big = std::fs::File::create(fixture.subagents.join("agent-big.jsonl")).unwrap();
        big.set_len(MAX_SUBAGENT_FILE_BYTES + 1).unwrap();
        std::fs::write(
            fixture.subagents.join("agent-big.meta.json"),
            meta("toolu_1").to_string(),
        )
        .unwrap();

        let scan = fixture.scan(&main);
        let agents: Vec<_> = scan
            .scoped_turns
            .iter()
            .map(|s| s.scope.agent_id.as_str())
            .collect();
        assert_eq!(agents, ["named", "ok"]);
        let named = &scan.scoped_turns[0].scope;
        assert_eq!(named.kind, CodingAgentScopeKind::Subagent);
        assert_eq!(named.name.as_deref(), Some("researcher"));
        let read: Vec<_> = scan.files.keys().map(String::as_str).collect();
        assert_eq!(read, ["named", "ok"]);
    }

    #[test]
    fn symlinked_subagents_directory_is_not_followed() {
        let fixture = Fixture::new();
        let elsewhere = fixture._dir.path().join("elsewhere");
        std::fs::create_dir_all(&elsewhere).unwrap();
        std::fs::write(
            elsewhere.join("agent-a1.jsonl"),
            simple_run("s1", 10, "req"),
        )
        .unwrap();
        std::fs::write(
            elsewhere.join("agent-a1.meta.json"),
            meta("toolu_1").to_string(),
        )
        .unwrap();
        std::fs::remove_dir(&fixture.subagents).unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(&elsewhere, &fixture.subagents).unwrap();
        let main = main_with_spawn("Agent", "toolu_1") + &foreground_completed("toolu_1", 30);
        assert!(fixture.scan(&main).scoped_turns.is_empty());
    }

    #[test]
    fn task_tool_name_links_like_agent() {
        let fixture = Fixture::new();
        fixture.agent("a1", meta("toolu_1"), &simple_run("s1", 10, "req"));
        let main = main_with_spawn("Task", "toolu_1") + &foreground_completed("toolu_1", 30);

        let scan = fixture.scan(&main);
        assert_eq!(scan.scoped_turns.len(), 1);
        assert_eq!(
            scan.scoped_turns[0].scope.parent_tool_call_id.as_deref(),
            Some("toolu_1")
        );
        assert_eq!(scan.scoped_turns[0].scope.parent_agent_id, None);
    }

    #[test]
    fn failed_spawn_result_is_terminal() {
        let fixture = Fixture::new();
        fixture.agent("a1", meta("toolu_1"), &simple_run("s1", 10, "req"));
        let failed = line(json!({
            "type": "user", "uuid": "m-r1", "parentUuid": "m-a1", "timestamp": at(30),
            "message": {"content": [{"type": "tool_result", "tool_use_id": "toolu_1",
                                     "content": "interrupted", "is_error": true}]}
        }));
        let main = main_with_spawn("Agent", "toolu_1") + &failed;
        assert_eq!(fixture.scan(&main).scoped_turns.len(), 1);
    }

    #[test]
    fn deadline_honoured() {
        let fixture = Fixture::new();
        fixture.agent("a1", meta("toolu_1"), &simple_run("s1", 10, "req"));
        let main = main_with_spawn("Agent", "toolu_1") + &foreground_completed("toolu_1", 30);

        let scan = fixture.scan_with(&main, &BTreeMap::new(), Instant::now());
        assert!(scan.scoped_turns.is_empty());
        assert!(scan.files.is_empty());
    }

    #[test]
    fn description_is_bounded_on_a_char_boundary() {
        let long = "é".repeat(CODING_AGENT_SCOPE_DESCRIPTION_MAX_BYTES);
        let cut = bounded(Some(&long), CODING_AGENT_SCOPE_DESCRIPTION_MAX_BYTES).unwrap();
        assert!(cut.len() <= CODING_AGENT_SCOPE_DESCRIPTION_MAX_BYTES);
        assert!(cut.chars().all(|c| c == 'é'));
        assert_eq!(bounded(Some("   "), 10), None);
    }

    #[test]
    fn file_names_are_strictly_allow_listed() {
        assert_eq!(
            classify_name("agent-a1.jsonl"),
            Some(("a1", FileKind::Transcript))
        );
        assert_eq!(
            classify_name("agent-A_b-9.meta.json"),
            Some(("A_b-9", FileKind::Meta))
        );
        for bad in [
            "agent-.jsonl",
            "agent-../x.jsonl",
            "agent-a.b.jsonl",
            "agent-a1.json",
            "xagent-a1.jsonl",
            "agent-a1.jsonl.bak",
        ] {
            assert_eq!(classify_name(bad), None, "{bad}");
        }
    }
}
