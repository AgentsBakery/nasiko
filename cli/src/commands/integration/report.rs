//! Hook-time parsing and durable capture. Network delivery belongs to `sync`.

use anyhow::{Result, bail};
use chrono::Utc;
use nasiko_types::{
    CODING_AGENT_CONTENT_MAX_BYTES, CODING_AGENT_EVENT_VERSION, CapturePolicy, CodingAgentEventV1,
    CodingAgentLlmCall, CodingAgentSession, CodingAgentSource, CodingAgentToolCall,
    CodingAgentTurn, coding_agent_event_id, coding_agent_session_id,
};
use std::collections::{BTreeMap, HashSet};
use std::io::Read;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use super::agents::Agent;
use super::agents::claude;
use super::agents::claude_subagents::ScannedFile;
use super::capabilities;
use super::model::{ScopedTurn, SnapshotOptions, Turn};
use super::queue::{self, QueueDestination, QueueRecord};
use super::state::{self, IntegrationState, SessionLock, SessionProgress};

const REPORT_BUDGET: Duration = Duration::from_secs(9);

pub fn run(agent: Agent) -> Result<()> {
    let deadline = Instant::now() + REPORT_BUDGET;
    let raw = read_payload()?;
    run_in(
        agent,
        &raw,
        deadline,
        &state::integrations_dir(),
        &mut spawn_sync,
    )
}

/// The report transaction against the integrations root `dir`. `spawn` starts
/// a detached `nasiko agents sync`; tests inject a counter so they never fork
/// a process or touch the real `~/.nasiko`.
fn run_in(
    agent: Agent,
    raw: &str,
    deadline: Instant,
    dir: &Path,
    spawn: &mut dyn FnMut() -> Result<()>,
) -> Result<()> {
    let spec = agent.spec();
    // Local file reads only: the hook never probes the network.
    let settings = IntegrationState::load_in(dir)?;
    let Some(agent_state) = settings.get(spec.id) else {
        bail!(
            "{} is not installed — run: nasiko agents install {}",
            spec.display_name,
            spec.id
        );
    };
    let destination = destination_from_state(agent_state)?;
    if !capabilities::has_cache_at(dir, &destination) {
        // Before any early return: without a cache this and every later
        // report would send v1.0 shapes. Sync probes and fills the cache in
        // the background so the next Stop uses the server's features.
        log(&format!(
            "no telemetry capabilities cached for {}; starting background sync",
            destination.cluster_name
        ));
        if let Err(error) = spawn() {
            log(&format!("failed to start background sync: {error:#}"));
        }
    }
    let options =
        SnapshotOptions::from_capabilities(&capabilities::load_cached_at(dir, &destination));
    let mut snapshot = agent.snapshot(raw, deadline, options)?;
    let lock = state::lock_session_in(
        dir,
        spec.id,
        &snapshot.session_id,
        deadline.saturating_duration_since(Instant::now()),
    )?;
    let completed = complete_turns(&snapshot.turns);
    if completed.is_empty() {
        log(&format!(
            "session {} — no completed turns; deferred",
            snapshot.session_id
        ));
        // A subagent run can finish while the main turn is still open.
        if snapshot.subagent_transcript.is_none() {
            return Ok(());
        }
    }
    let progress = lock.progress()?;
    if progress.migrated_legacy_counts {
        log(&format!(
            "session {} — migrated legacy progress; replaying complete turns once",
            snapshot.session_id
        ));
    }
    let pending = pending_turns(&completed, &progress.captured_turn_ids);
    let mut queued = 0;
    let mut rejected = 0;
    for turn in &pending {
        let record = QueueRecord::new(
            destination.clone(),
            canonical_event(
                spec.id,
                &agent_state.agent_name,
                &snapshot.session_id,
                snapshot.title.as_deref(),
                turn,
                agent_state.capture_content,
                snapshot.adapter_version,
            ),
        );
        if let Err(error) = record.event.validate() {
            queue::reject_invalid_at(dir, &record, &error)?;
            lock.mark_captured(std::slice::from_ref(&record.event.turn.id))?;
            rejected += 1;
            log(&format!(
                "session {} turn {} — quarantined invalid event: {error}",
                snapshot.session_id, record.event.turn.id
            ));
            continue;
        }
        queue_then_mark(dir, &record, &lock)?;
        queued += 1;
    }

    let mut scanned_files = BTreeMap::new();
    if let Some(transcript) = &snapshot.subagent_transcript {
        match claude::subagent_scan(
            transcript,
            &snapshot.turns,
            deadline,
            &progress.subagent_files,
        ) {
            Ok(scan) => {
                snapshot.scoped_turns = scan.scoped_turns;
                scanned_files = scan.files;
            }
            Err(error) => log(&format!(
                "session {} — subagent scan skipped: {error:#}",
                snapshot.session_id
            )),
        }
    }
    let source = EventSource {
        agent_id: spec.id,
        agent_name: &agent_state.agent_name,
        session_id: &snapshot.session_id,
        capture_content: agent_state.capture_content,
        adapter_version: snapshot.adapter_version,
    };
    let scoped = queue_scoped_turns(
        ScopedQueue {
            dir,
            lock: &lock,
            destination: &destination,
            progress: &progress,
            deadline,
        },
        &source,
        &snapshot.scoped_turns,
        &scanned_files,
    )?;
    drop(lock);

    if queued + scoped.queued > 0 {
        spawn()?;
    }
    if !pending.is_empty() || scoped.queued + scoped.rejected > 0 {
        log(&format!(
            "session {} — queued {queued} completed turn(s) and {} subagent run(s) for {}; quarantined {}",
            snapshot.session_id,
            scoped.queued,
            destination.cluster_name,
            rejected + scoped.rejected
        ));
    }
    Ok(())
}

/// Identity and policy shared by every event of one report.
struct EventSource<'a> {
    agent_id: &'a str,
    agent_name: &'a str,
    session_id: &'a str,
    capture_content: bool,
    adapter_version: Option<u32>,
}

/// Where scoped events are committed during one report.
struct ScopedQueue<'a> {
    dir: &'a Path,
    lock: &'a SessionLock,
    destination: &'a QueueDestination,
    progress: &'a SessionProgress,
    deadline: Instant,
}

#[derive(Debug, Default)]
struct ScopedOutcome {
    queued: usize,
    rejected: usize,
}

/// Queue finished subagent runs not yet captured, exactly like main turns
/// (invalid events are quarantined and marked), then record per-file progress.
/// Runs left when the deadline passes stay unmarked for the next Stop.
fn queue_scoped_turns(
    target: ScopedQueue<'_>,
    source: &EventSource<'_>,
    scoped_turns: &[ScopedTurn],
    scanned_files: &BTreeMap<String, ScannedFile>,
) -> Result<ScopedOutcome> {
    let mut outcome = ScopedOutcome::default();
    let mut captured_now: HashSet<&str> = HashSet::new();
    for scoped in scoped_turns {
        if target
            .progress
            .captured_turn_ids
            .contains(&scoped.turn.uuid)
        {
            continue;
        }
        if Instant::now() >= target.deadline {
            break;
        }
        let record = QueueRecord::new(
            target.destination.clone(),
            scoped_canonical_event(source, scoped),
        );
        if let Err(error) = record.event.validate() {
            queue::reject_invalid_at(target.dir, &record, &error)?;
            target
                .lock
                .mark_captured(std::slice::from_ref(&record.event.turn.id))?;
            outcome.rejected += 1;
            log(&format!(
                "session {} turn {} — quarantined invalid subagent event: {error}",
                source.session_id, record.event.turn.id
            ));
        } else {
            queue_then_mark(target.dir, &record, target.lock)?;
            outcome.queued += 1;
        }
        captured_now.insert(&scoped.turn.uuid);
    }

    let captured =
        |id: &str| captured_now.contains(id) || target.progress.captured_turn_ids.contains(id);
    let mut updates = BTreeMap::new();
    for (agent_id, scanned) in scanned_files {
        let previous = target.progress.subagent_files.get(agent_id);
        let mut file = previous.cloned().unwrap_or_default();
        file.size = scanned.size;
        file.spawn_tool_use_ids = scanned.spawn_tool_use_ids.clone();
        let mut all_captured = true;
        for scoped in scoped_turns
            .iter()
            .filter(|s| &s.scope.agent_id == agent_id)
        {
            if !captured(&scoped.turn.uuid) {
                all_captured = false;
                continue;
            }
            for call in &scoped.turn.calls {
                if !file.call_ids.contains(&call.uuid) {
                    file.call_ids.push(call.uuid.clone());
                }
            }
        }
        file.complete = scanned.all_terminal && all_captured;
        if previous != Some(&file) {
            updates.insert(agent_id.clone(), file);
        }
    }
    target.lock.mark_subagent_files(&updates)?;
    Ok(outcome)
}

/// A subagent run as its own event in the parent session. Intent (description,
/// name) is content: sent only with content capture, like prompt and tools.
/// Scoped events never carry the session title.
fn scoped_canonical_event(source: &EventSource<'_>, scoped: &ScopedTurn) -> CodingAgentEventV1 {
    let mut event = canonical_event(
        source.agent_id,
        source.agent_name,
        source.session_id,
        None,
        &scoped.turn,
        source.capture_content,
        source.adapter_version,
    );
    // A resume record can be blank; present scoped content must be nonblank.
    event.turn.prompt = event.turn.prompt.filter(|prompt| !prompt.trim().is_empty());
    let mut scope = scoped.scope.clone();
    if !source.capture_content {
        scope.description = None;
        scope.name = None;
    }
    event.turn.agent_scope = Some(scope);
    event
}

fn destination_from_state(agent_state: &super::state::AgentState) -> Result<QueueDestination> {
    let binding = agent_state.binding.as_ref().ok_or_else(|| {
        anyhow::anyhow!(
            "installed integration has no cluster binding; reinstall it with: nasiko agents install <agent>"
        )
    })?;
    Ok(QueueDestination {
        cluster_name: binding.cluster_name.clone(),
        cluster_url: binding.cluster_url.clone(),
        principal_id: binding.principal_id,
    })
}

fn canonical_event(
    agent_id: &str,
    agent_name: &str,
    source_session_id: &str,
    title: Option<&str>,
    turn: &Turn,
    capture_content: bool,
    adapter_version: Option<u32>,
) -> CodingAgentEventV1 {
    CodingAgentEventV1 {
        version: CODING_AGENT_EVENT_VERSION,
        event_id: coding_agent_event_id(agent_id, source_session_id, &turn.uuid),
        captured_at: turn.ended_at,
        source: CodingAgentSource {
            agent_id: agent_id.to_string(),
            agent_name: agent_name.to_string(),
            adapter_version,
        },
        session: CodingAgentSession {
            id: coding_agent_session_id(agent_id, source_session_id),
            source_id: source_session_id.to_string(),
            title: title.filter(|_| capture_content).map(|title| {
                bounded_text_to(title, nasiko_types::CODING_AGENT_SESSION_TITLE_MAX_BYTES)
            }),
        },
        turn: CodingAgentTurn {
            id: turn.uuid.clone(),
            prompt: capture_content.then(|| turn.prompt.clone()),
            response: capture_content.then(|| turn.response.clone()).flatten(),
            started_at: turn.started_at,
            ended_at: turn.ended_at,
            llm_calls: turn
                .calls
                .iter()
                .map(|call| CodingAgentLlmCall {
                    id: call.uuid.clone(),
                    provider: call.provider.clone(),
                    model: call.model.clone(),
                    input_tokens: call.input_tokens,
                    output_tokens: call.output_tokens,
                    cache_read_tokens: call.cache_read_tokens,
                    cache_creation_tokens: call.cache_creation_tokens,
                    accounting: call.accounting.clone(),
                    started_at: call.started_at,
                    ended_at: call.ended_at,
                })
                .collect(),
            tool_calls: turn
                .tool_calls
                .iter()
                .map(|tool| CodingAgentToolCall {
                    id: tool.id.clone(),
                    name: tool.name.clone(),
                    kind: tool.kind.clone(),
                    model_call_id: tool.model_call_id.clone(),
                    status: tool.status,
                    arguments: capture_content
                        .then(|| tool.arguments.as_ref().map(bounded_value))
                        .flatten(),
                    output: capture_content
                        .then(|| tool.output.as_ref().map(bounded_value))
                        .flatten(),
                    raw: capture_content
                        .then(|| tool.raw.as_deref().map(bounded_text))
                        .flatten(),
                    error: capture_content
                        .then(|| tool.error.as_deref().map(bounded_text))
                        .flatten(),
                    started_at: tool.started_at,
                    ended_at: tool.ended_at,
                    duration_ms: tool.duration_ms,
                    association: tool.association,
                    timestamp_quality: tool.timestamp_quality,
                })
                .collect(),
            agent_scope: None,
        },
        capture_policy: if capture_content {
            CapturePolicy::Content
        } else {
            CapturePolicy::MetadataOnly
        },
    }
}

fn bounded_value(value: &serde_json::Value) -> serde_json::Value {
    let serialized = serde_json::to_string(value).unwrap_or_else(|_| "null".into());
    if serialized.len() <= CODING_AGENT_CONTENT_MAX_BYTES {
        return value.clone();
    }
    serde_json::Value::String(bounded_text_to(
        &serialized,
        CODING_AGENT_CONTENT_MAX_BYTES / 2,
    ))
}

fn bounded_text(value: &str) -> String {
    bounded_text_to(value, CODING_AGENT_CONTENT_MAX_BYTES)
}

fn bounded_text_to(value: &str, max: usize) -> String {
    if value.len() <= max {
        return value.to_string();
    }
    let suffix = "...";
    let mut end = max.saturating_sub(suffix.len());
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}{suffix}", &value[..end])
}

fn queue_then_mark(dir: &Path, record: &QueueRecord, lock: &SessionLock) -> Result<()> {
    ordered_commit(
        || queue::enqueue_at(dir, record).map(|_| ()),
        || lock.mark_captured(std::slice::from_ref(&record.event.turn.id)),
    )
}

fn ordered_commit(
    enqueue: impl FnOnce() -> Result<()>,
    mark: impl FnOnce() -> Result<()>,
) -> Result<()> {
    enqueue()?;
    mark()
}

fn complete_turns(turns: &[Turn]) -> Vec<Turn> {
    turns
        .iter()
        .filter(|turn| !turn.is_empty() && turn.response.is_some())
        .cloned()
        .collect()
}

fn pending_turns(turns: &[Turn], captured: &HashSet<String>) -> Vec<Turn> {
    turns
        .iter()
        .filter(|turn| !captured.contains(&turn.uuid))
        .cloned()
        .collect()
}

fn spawn_sync() -> Result<()> {
    Command::new(std::env::current_exe()?)
        .args(["agents", "sync"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    Ok(())
}

fn read_payload() -> Result<String> {
    let mut raw = String::new();
    std::io::stdin().read_to_string(&mut raw)?;
    Ok(raw)
}

fn log(message: &str) {
    println!("{} {message}", Utc::now().to_rfc3339());
}

#[cfg(test)]
mod tests {
    use super::super::model::{LlmCall, ToolCall};
    use super::*;
    use chrono::{DateTime, Utc};
    use std::cell::Cell;

    fn turn(id: &str, complete: bool) -> Turn {
        let at = "2026-01-01T00:00:00Z".parse::<DateTime<Utc>>().unwrap();
        Turn {
            uuid: id.into(),
            prompt: "prompt".into(),
            response: complete.then(|| "response".into()),
            started_at: at,
            ended_at: at,
            calls: complete
                .then(|| LlmCall {
                    uuid: format!("call-{id}"),
                    provider: "provider".into(),
                    model: "model".into(),
                    input_tokens: 1,
                    output_tokens: 1,
                    cache_read_tokens: 0,
                    cache_creation_tokens: 0,
                    accounting: None,
                    started_at: at,
                    ended_at: at,
                })
                .into_iter()
                .collect(),
            tool_calls: vec![],
        }
    }

    #[test]
    fn incomplete_history_does_not_block_later_complete_turns() {
        let turns = [turn("a", true), turn("b", false), turn("c", true)];
        assert_eq!(
            complete_turns(&turns)
                .iter()
                .map(|turn| turn.uuid.as_str())
                .collect::<Vec<_>>(),
            ["a", "c"]
        );
    }

    #[test]
    fn captured_progress_filters_completed_turns() {
        let turns = [turn("a", true), turn("b", true)];
        assert_eq!(
            pending_turns(&turns, &HashSet::from(["a".to_string()]))[0].uuid,
            "b"
        );
    }

    #[test]
    fn queue_failure_never_advances_the_watermark() {
        let marked = Cell::new(false);
        let result = ordered_commit(
            || Err(anyhow::anyhow!("disk full")),
            || {
                marked.set(true);
                Ok(())
            },
        );
        assert!(result.is_err());
        assert!(!marked.get());
    }

    #[test]
    fn reporting_uses_the_install_time_destination() {
        let principal_id = uuid::Uuid::new_v4();
        let state = super::super::state::AgentState {
            agent_name: "alice-claude-code".into(),
            capture_content: true,
            hook_version: 1,
            binding: Some(super::super::state::InstallationBinding {
                cluster_name: "cluster-a".into(),
                cluster_url: "https://a.example".into(),
                principal_id,
            }),
        };

        let destination = destination_from_state(&state).unwrap();
        assert_eq!(destination.cluster_name, "cluster-a");
        assert_eq!(destination.cluster_url, "https://a.example");
        assert_eq!(destination.principal_id, principal_id);
    }

    #[test]
    fn legacy_install_state_without_a_destination_fails_closed() {
        let state = super::super::state::AgentState {
            agent_name: "alice-claude-code".into(),
            capture_content: true,
            hook_version: 1,
            binding: None,
        };

        assert!(
            destination_from_state(&state)
                .unwrap_err()
                .to_string()
                .contains("reinstall")
        );
    }

    #[test]
    fn canonical_identity_is_stable_and_content_policy_is_enforced() {
        let mut turn = turn("same-turn", true);
        turn.tool_calls.push(ToolCall {
            id: "tool-1".into(),
            name: "Read".into(),
            kind: "tool".into(),
            model_call_id: Some("call-same-turn".into()),
            status: nasiko_types::CodingAgentToolCallStatus::Succeeded,
            arguments: Some(serde_json::json!({"path": "secret"})),
            output: Some(serde_json::json!("secret output")),
            raw: Some("raw".into()),
            error: Some("hidden".into()),
            started_at: Some(turn.started_at),
            ended_at: Some(turn.ended_at),
            duration_ms: Some(0),
            association: nasiko_types::CodingAgentToolAssociation::Exact,
            timestamp_quality: nasiko_types::CodingAgentTimestampQuality::Exact,
        });
        let first = canonical_event(
            "claude",
            "claude-code",
            "same",
            Some("Claude session title"),
            &turn,
            false,
            None,
        );
        let second = canonical_event(
            "claude",
            "claude-code",
            "same",
            Some("Claude session title"),
            &turn,
            false,
            None,
        );
        assert_eq!(first.event_id, second.event_id);
        assert_eq!(first, second);
        assert_eq!(first.session.id, "claude:same");
        assert!(first.session.title.is_none());
        assert!(first.turn.prompt.is_none());
        assert!(first.turn.response.is_none());
        assert_eq!(first.turn.tool_calls[0].name, "Read");
        assert!(first.turn.tool_calls[0].arguments.is_none());
        assert!(first.turn.tool_calls[0].output.is_none());
        assert!(first.turn.tool_calls[0].error.is_none());
        assert!(first.validate().is_ok());
        assert_ne!(
            first.event_id,
            canonical_event(
                "opencode",
                "opencode",
                "same",
                Some("Claude session title"),
                &turn,
                false,
                None
            )
            .event_id
        );
        let content = canonical_event(
            "claude",
            "claude-code",
            "same",
            Some("Claude session title"),
            &turn,
            true,
            None,
        );
        assert_eq!(
            content.turn.tool_calls[0].arguments,
            Some(serde_json::json!({"path": "secret"}))
        );
        assert_eq!(
            content.session.title.as_deref(),
            Some("Claude session title")
        );
        assert!(content.validate().is_ok());
    }

    #[test]
    fn canonical_event_carries_the_adapter_marker_only_when_set() {
        let turn = turn("marked", true);
        let marked = canonical_event(
            "codex",
            "codex",
            "s",
            None,
            &turn,
            false,
            Some(nasiko_types::CODEX_ADAPTER_VERSION_EXCLUSIVE_INPUT),
        );
        assert_eq!(
            marked.source.adapter_version,
            Some(nasiko_types::CODEX_ADAPTER_VERSION_EXCLUSIVE_INPUT)
        );
        assert_eq!(
            serde_json::to_value(&marked).unwrap()["source"]["adapter_version"],
            1
        );

        let legacy = canonical_event("codex", "codex", "s", None, &turn, false, None);
        let encoded = serde_json::to_value(&legacy).unwrap();
        assert!(encoded["source"].get("adapter_version").is_none());
    }

    // ─── run_in: capability cache priming ──────────────────────────────────

    const COMPLETE_TRANSCRIPT: &str = concat!(
        "{\"type\":\"user\",\"uuid\":\"u\",\"timestamp\":\"2026-01-01T00:00:00Z\",\"message\":{\"content\":\"hello\"}}\n",
        "{\"type\":\"assistant\",\"uuid\":\"a\",\"parentUuid\":\"u\",\"timestamp\":\"2026-01-01T00:00:01Z\",\"message\":{\"model\":\"test\",\"content\":\"done\",\"stop_reason\":\"end_turn\",\"usage\":{\"input_tokens\":1,\"output_tokens\":2}}}\n",
    );
    const INCOMPLETE_TRANSCRIPT: &str = "{\"type\":\"user\",\"uuid\":\"u\",\"timestamp\":\"2026-01-01T00:00:00Z\",\"message\":{\"content\":\"hello\"}}\n";
    const BOUND_URL: &str = "https://bound.example";

    /// An installed Claude integration whose state lives entirely in `dir`.
    fn installed(dir: &std::path::Path) -> QueueDestination {
        let principal_id = uuid::Uuid::new_v4();
        let state = IntegrationState {
            agents: std::collections::HashMap::from([(
                "claude".to_string(),
                super::super::state::AgentState {
                    agent_name: "claude-code".into(),
                    capture_content: false,
                    hook_version: 3,
                    binding: Some(super::super::state::InstallationBinding {
                        cluster_name: "bound".into(),
                        cluster_url: BOUND_URL.into(),
                        principal_id,
                    }),
                },
            )]),
        };
        std::fs::write(
            dir.join("config.json"),
            serde_json::to_string(&state).unwrap(),
        )
        .unwrap();
        QueueDestination {
            cluster_name: "bound".into(),
            cluster_url: BOUND_URL.into(),
            principal_id,
        }
    }

    fn cache_empty_features(dir: &std::path::Path, destination: &QueueDestination) {
        std::fs::write(
            dir.join("capabilities.json"),
            serde_json::json!({"destinations": {
                format!("{}|{}", destination.cluster_url, destination.principal_id):
                    {"features": [], "fetched_at": "2026-01-01T00:00:00Z"}
            }})
            .to_string(),
        )
        .unwrap();
    }

    fn report_once(dir: &std::path::Path, transcript: &str) -> usize {
        let path = dir.join("session.jsonl");
        std::fs::write(&path, transcript).unwrap();
        let raw = serde_json::json!({"session_id": "s", "transcript_path": path}).to_string();
        let spawned = Cell::new(0);
        run_in(Agent::Claude, &raw, Instant::now(), dir, &mut || {
            spawned.set(spawned.get() + 1);
            Ok(())
        })
        .unwrap();
        spawned.get()
    }

    #[test]
    fn report_without_cache_spawns_sync() {
        let dir = tempfile::tempdir().unwrap();
        installed(dir.path());
        // First report queues the turn: the early capability spawn plus the
        // usual post-queue spawn (sync's lock makes the second harmless).
        assert_eq!(report_once(dir.path(), COMPLETE_TRANSCRIPT), 2);
        // Nothing left to queue; only the missing cache triggers a sync.
        assert_eq!(report_once(dir.path(), COMPLETE_TRANSCRIPT), 1);
    }

    #[test]
    fn report_without_cache_and_no_completed_turns_spawns_sync() {
        let dir = tempfile::tempdir().unwrap();
        installed(dir.path());
        assert!(report_once(dir.path(), INCOMPLETE_TRANSCRIPT) >= 1);
    }

    #[test]
    fn report_with_cache_and_nothing_queued_does_not_spawn() {
        let dir = tempfile::tempdir().unwrap();
        let destination = installed(dir.path());
        cache_empty_features(dir.path(), &destination);
        assert_eq!(report_once(dir.path(), INCOMPLETE_TRANSCRIPT), 0);
    }

    // ─── run_in: Claude subagent capture ───────────────────────────────────

    const SUBAGENT_ID: &str = "a1";
    const SUBAGENT_TURN_ID: &str = "subagent:a1:sub-u1";

    fn json_line(value: serde_json::Value) -> String {
        format!("{value}\n")
    }

    /// An installed Claude integration with an explicit content policy.
    fn installed_with(dir: &std::path::Path, capture_content: bool) -> QueueDestination {
        let destination = installed(dir);
        let path = dir.join("config.json");
        let mut state: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        state["agents"]["claude"]["capture_content"] = capture_content.into();
        std::fs::write(&path, state.to_string()).unwrap();
        destination
    }

    fn cache_features(dir: &std::path::Path, destination: &QueueDestination, features: &[&str]) {
        std::fs::write(
            dir.join("capabilities.json"),
            serde_json::json!({"destinations": {
                format!("{}|{}", destination.cluster_url, destination.principal_id):
                    {"features": features, "fetched_at": "2026-01-01T00:00:00Z"}
            }})
            .to_string(),
        )
        .unwrap();
    }

    const BOTH_FEATURES: &[&str] = &[
        nasiko_types::CODING_AGENT_FEATURE_AGENT_SCOPE,
        nasiko_types::CODING_AGENT_FEATURE_ADAPTER_VERSION,
    ];

    /// A complete main turn that spawned `toolu_1`. `foreground` decides whether
    /// the spawn result says the run completed or was launched in the background.
    fn main_transcript(foreground: bool) -> String {
        let status = if foreground {
            "completed"
        } else {
            "async_launched"
        };
        [
            serde_json::json!({"type":"user","uuid":"u","timestamp":"2026-01-01T00:00:00Z","sessionId":"s","message":{"content":"hello"}}),
            serde_json::json!({"type":"assistant","uuid":"a","parentUuid":"u","requestId":"main-req-1","timestamp":"2026-01-01T00:00:01Z","sessionId":"s","message":{"id":"main-msg-1","model":"test","stop_reason":"tool_use","content":[{"type":"tool_use","id":"toolu_1","name":"Agent","input":{"subagent_type":"Explore","description":"find X","prompt":"find X please"}}],"usage":{"input_tokens":1,"output_tokens":2}}}),
            serde_json::json!({"type":"user","uuid":"r","parentUuid":"a","timestamp":"2026-01-01T00:00:30Z","sessionId":"s","toolUseResult":{"status":status,"agentId":SUBAGENT_ID,"totalTokens":99999},"message":{"content":[{"type":"tool_result","tool_use_id":"toolu_1","content":"ok"}]}}),
            serde_json::json!({"type":"assistant","uuid":"b","parentUuid":"r","requestId":"main-req-2","timestamp":"2026-01-01T00:00:31Z","sessionId":"s","message":{"id":"main-msg-2","model":"test","stop_reason":"end_turn","content":"done","usage":{"input_tokens":1,"output_tokens":2}}}),
            serde_json::json!({"type":"ai-title","aiTitle":"Session title","sessionId":"s"}),
        ]
        .into_iter()
        .map(json_line)
        .collect()
    }

    fn completed_notification() -> String {
        json_line(serde_json::json!({
            "type":"queue-operation","operation":"enqueue","timestamp":"2026-01-01T00:01:00Z","sessionId":"s",
            "content": format!("<task-notification><task-id>{SUBAGENT_ID}</task-id><status>completed</status><usage><subagent_tokens>99999</subagent_tokens></usage></task-notification>")
        }))
    }

    fn subagent_transcript(segment_uuid: &str) -> String {
        [
            serde_json::json!({"parentUuid":null,"isSidechain":true,"agentId":SUBAGENT_ID,"type":"user","message":{"role":"user","content":"find X please"},"uuid":segment_uuid,"timestamp":"2026-01-01T00:00:02Z","sessionId":"s"}),
            serde_json::json!({"parentUuid":segment_uuid,"isSidechain":true,"agentId":SUBAGENT_ID,"type":"assistant","requestId":"sub-req-1","message":{"model":"test","id":"sub-msg-1","content":[{"type":"tool_use","id":"toolu_read","name":"Read","input":{"path":"redacted"}}],"stop_reason":null,"usage":{"input_tokens":2,"cache_creation_input_tokens":10,"cache_read_input_tokens":100,"output_tokens":3}},"uuid":"sub-a1","timestamp":"2026-01-01T00:00:03Z","sessionId":"s"}),
            serde_json::json!({"parentUuid":"sub-a1","isSidechain":true,"agentId":SUBAGENT_ID,"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_read","content":"secret file text"}]},"uuid":"sub-r1","timestamp":"2026-01-01T00:00:04Z","sessionId":"s"}),
            serde_json::json!({"parentUuid":"sub-r1","isSidechain":true,"agentId":SUBAGENT_ID,"type":"assistant","requestId":"sub-req-2","message":{"model":"test","id":"sub-msg-2","content":[{"type":"tool_use","id":"toolu_hb","name":"SubagentHandback","input":{"message":"X is in redacted.rs"}}],"stop_reason":null,"usage":{"input_tokens":2,"cache_creation_input_tokens":0,"cache_read_input_tokens":110,"output_tokens":4}},"uuid":"sub-a2","timestamp":"2026-01-01T00:00:05Z","sessionId":"s"}),
        ]
        .into_iter()
        .map(json_line)
        .collect()
    }

    fn write_subagent(dir: &std::path::Path, meta: serde_json::Value, transcript: &str) {
        let subagents = dir.join("session").join("subagents");
        std::fs::create_dir_all(&subagents).unwrap();
        std::fs::write(
            subagents.join(format!("agent-{SUBAGENT_ID}.meta.json")),
            meta.to_string(),
        )
        .unwrap();
        std::fs::write(
            subagents.join(format!("agent-{SUBAGENT_ID}.jsonl")),
            transcript,
        )
        .unwrap();
    }

    fn subagent_meta() -> serde_json::Value {
        serde_json::json!({"agentType":"Explore","description":"find X","toolUseId":"toolu_1","spawnDepth":1,"requestShape":"foreground"})
    }

    /// Report with a live deadline (subagent scanning honours it).
    fn report_live(dir: &std::path::Path, transcript: &str) -> usize {
        let path = dir.join("session.jsonl");
        std::fs::write(&path, transcript).unwrap();
        let raw = serde_json::json!({"session_id": "s", "transcript_path": path}).to_string();
        let spawned = Cell::new(0);
        run_in(
            Agent::Claude,
            &raw,
            Instant::now() + Duration::from_secs(5),
            dir,
            &mut || {
                spawned.set(spawned.get() + 1);
                Ok(())
            },
        )
        .unwrap();
        spawned.get()
    }

    fn queued_events(dir: &std::path::Path) -> Vec<CodingAgentEventV1> {
        let mut events = Vec::new();
        let Ok(clusters) = std::fs::read_dir(dir.join("queue")) else {
            return events;
        };
        for cluster in clusters.flatten() {
            for entry in std::fs::read_dir(cluster.path()).unwrap().flatten() {
                events.push(queue::load(&entry.path()).unwrap().event);
            }
        }
        events.sort_by(|left, right| left.turn.id.cmp(&right.turn.id));
        events
    }

    fn scoped_events(dir: &std::path::Path) -> Vec<CodingAgentEventV1> {
        queued_events(dir)
            .into_iter()
            .filter(|event| event.turn.agent_scope.is_some())
            .collect()
    }

    fn watermark(dir: &std::path::Path) -> serde_json::Value {
        let path = dir.join("watermarks").join("claude").join("s.json");
        serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
    }

    #[test]
    fn report_emits_scoped_events_with_content() {
        let dir = tempfile::tempdir().unwrap();
        let destination = installed_with(dir.path(), true);
        cache_features(dir.path(), &destination, BOTH_FEATURES);
        write_subagent(dir.path(), subagent_meta(), &subagent_transcript("sub-u1"));

        report_live(dir.path(), &main_transcript(true));

        let events = queued_events(dir.path());
        assert_eq!(events.len(), 2);
        let main = events
            .iter()
            .find(|e| e.turn.agent_scope.is_none())
            .unwrap();
        assert_eq!(
            main.source.adapter_version,
            Some(nasiko_types::CLAUDE_ADAPTER_VERSION_SUBAGENTS)
        );
        assert_eq!(main.session.title.as_deref(), Some("Session title"));
        let scoped = &scoped_events(dir.path())[0];
        assert_eq!(scoped.turn.id, SUBAGENT_TURN_ID);
        assert_eq!(scoped.session.id, main.session.id);
        assert!(scoped.session.title.is_none());
        let scope = scoped.turn.agent_scope.as_ref().unwrap();
        assert_eq!(scope.agent_type.as_deref(), Some("Explore"));
        assert_eq!(scope.description.as_deref(), Some("find X"));
        assert_eq!(scope.parent_tool_call_id.as_deref(), Some("toolu_1"));
        assert_eq!(scoped.turn.prompt.as_deref(), Some("find X please"));
        assert_eq!(scoped.turn.response.as_deref(), Some("X is in redacted.rs"));
        assert_eq!(scoped.turn.llm_calls.len(), 2);
        assert!(
            scoped.turn.llm_calls.iter().all(|call| {
                call.accounting.as_ref().unwrap().output_tokens_final == Some(false)
            })
        );
        assert_eq!(
            scoped.source.adapter_version,
            Some(nasiko_types::CLAUDE_ADAPTER_VERSION_SUBAGENTS)
        );
        assert!(scoped.validate().is_ok());
        assert!(!dir.path().join("rejected").exists());
    }

    #[test]
    fn report_strips_intent_under_no_content() {
        let dir = tempfile::tempdir().unwrap();
        let destination = installed_with(dir.path(), false);
        cache_features(dir.path(), &destination, BOTH_FEATURES);
        let mut meta = subagent_meta();
        meta["name"] = "researcher".into();
        write_subagent(dir.path(), meta, &subagent_transcript("sub-u1"));

        report_live(dir.path(), &main_transcript(true));

        let scoped = scoped_events(dir.path());
        assert_eq!(scoped.len(), 1);
        let event = &scoped[0];
        let scope = event.turn.agent_scope.as_ref().unwrap();
        assert_eq!(scope.kind, nasiko_types::CodingAgentScopeKind::Subagent);
        assert_eq!(scope.agent_type.as_deref(), Some("Explore"));
        assert!(scope.description.is_none());
        assert!(scope.name.is_none());
        assert!(event.turn.prompt.is_none());
        assert!(event.turn.response.is_none());
        assert!(event.session.title.is_none());
        assert!(!event.turn.tool_calls.is_empty());
        assert!(
            event
                .turn
                .tool_calls
                .iter()
                .all(|tool| tool.arguments.is_none()
                    && tool.output.is_none()
                    && tool.raw.is_none()
                    && tool.error.is_none())
        );
        assert!(event.validate().is_ok());
    }

    #[test]
    fn report_without_capability_emits_no_scoped_events_and_no_marker() {
        let dir = tempfile::tempdir().unwrap();
        let destination = installed_with(dir.path(), true);
        cache_empty_features(dir.path(), &destination);
        write_subagent(dir.path(), subagent_meta(), &subagent_transcript("sub-u1"));

        report_live(dir.path(), &main_transcript(true));

        let events = queued_events(dir.path());
        assert_eq!(events.len(), 1);
        assert!(events[0].turn.agent_scope.is_none());
        let encoded = serde_json::to_value(&events[0]).unwrap();
        assert!(encoded["source"].get("adapter_version").is_none());
        assert!(watermark(dir.path()).get("subagent_files").is_none());
    }

    #[test]
    fn second_stop_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let destination = installed_with(dir.path(), true);
        cache_features(dir.path(), &destination, BOTH_FEATURES);
        write_subagent(dir.path(), subagent_meta(), &subagent_transcript("sub-u1"));
        let main = main_transcript(true);

        assert_eq!(report_live(dir.path(), &main), 1);
        assert_eq!(report_live(dir.path(), &main), 0);
        assert_eq!(queued_events(dir.path()).len(), 2);
        let file = &watermark(dir.path())["subagent_files"][SUBAGENT_ID];
        assert_eq!(file["complete"], true);
        assert_eq!(file["call_ids"].as_array().unwrap().len(), 2);

        // Same size, different segment id: a re-parse would queue a new event,
        // so none appearing proves the unchanged complete file was skipped.
        write_subagent(dir.path(), subagent_meta(), &subagent_transcript("sub-u9"));
        assert_eq!(report_live(dir.path(), &main), 0);
        let ids: Vec<_> = queued_events(dir.path())
            .into_iter()
            .map(|event| event.turn.id)
            .collect();
        assert_eq!(ids.len(), 2);
        assert!(ids.contains(&SUBAGENT_TURN_ID.to_string()));
    }

    #[test]
    fn scoped_only_stop_spawns_sync() {
        let dir = tempfile::tempdir().unwrap();
        let destination = installed_with(dir.path(), true);
        cache_features(dir.path(), &destination, BOTH_FEATURES);
        write_subagent(dir.path(), subagent_meta(), &subagent_transcript("sub-u1"));
        let main = main_transcript(false);
        assert_eq!(report_live(dir.path(), &main), 1);
        assert!(scoped_events(dir.path()).is_empty());

        // Every main turn is already captured; only the finished run is new.
        let finished = main + &completed_notification();
        assert_eq!(report_live(dir.path(), &finished), 1);
        assert_eq!(scoped_events(dir.path()).len(), 1);
        assert_eq!(report_live(dir.path(), &finished), 0);
    }

    #[test]
    fn deferred_run_not_marked() {
        let dir = tempfile::tempdir().unwrap();
        let destination = installed_with(dir.path(), false);
        cache_features(dir.path(), &destination, BOTH_FEATURES);
        write_subagent(dir.path(), subagent_meta(), &subagent_transcript("sub-u1"));
        let main = main_transcript(false);

        report_live(dir.path(), &main);
        let captured = watermark(dir.path())["captured_turn_ids"].clone();
        assert!(
            !captured
                .as_array()
                .unwrap()
                .iter()
                .any(|id| id == SUBAGENT_TURN_ID)
        );
        let file = &watermark(dir.path())["subagent_files"][SUBAGENT_ID];
        assert_ne!(file["complete"], true);

        report_live(dir.path(), &(main + &completed_notification()));
        let scoped = scoped_events(dir.path());
        assert_eq!(scoped.len(), 1);
        assert_eq!(scoped[0].turn.id, SUBAGENT_TURN_ID);
        assert!(
            watermark(dir.path())["captured_turn_ids"]
                .as_array()
                .unwrap()
                .iter()
                .any(|id| id == SUBAGENT_TURN_ID)
        );
    }

    #[test]
    fn tool_content_projection_bounds_large_json_and_unicode_text() {
        let value = serde_json::json!({"value": "x".repeat(CODING_AGENT_CONTENT_MAX_BYTES)});
        assert!(
            serde_json::to_vec(&bounded_value(&value)).unwrap().len()
                <= CODING_AGENT_CONTENT_MAX_BYTES
        );
        let text = "é".repeat(CODING_AGENT_CONTENT_MAX_BYTES);
        let bounded = bounded_text(&text);
        assert!(bounded.len() <= CODING_AGENT_CONTENT_MAX_BYTES);
        assert!(bounded.ends_with("..."));
    }
}
