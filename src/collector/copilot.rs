use super::process::{self, ProcInfo};
use crate::model::{AgentSession, ChildProcess, SessionStatus};
use std::collections::HashMap;
use std::fs;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

/// Collector for GitHub Copilot CLI sessions.
///
/// Discovery strategy:
/// 1. `ps` to find running `copilot` processes (path contains `copilot-cli`)
/// 2. Scan `~/.copilot/logs/process-{ts}-{pid}.log` for a log file matching each PID
/// 3. Parse log file for session ID, version, session name, git repository
/// 4. Given the session ID, incrementally parse
///    `~/.copilot/session-state/{sessionId}/events.jsonl` for live turn count,
///    output tokens, model, and context usage — see `EventsResult` below. This
///    is the real runtime telemetry source on current Copilot CLI builds; the
///    older `CompactionProcessor`/`Sending request` log-line patterns this
///    collector also still recognizes (for forward/backward compatibility)
///    have not been observed in any Copilot CLI build from 1.0.26 through
///    1.0.80 and are effectively dead in practice.
/// 5. Get CWD from lsof/proc for the process
/// 6. Read model/effort from `~/.copilot/settings.json` as a last-resort
///    fallback when neither has been observed live yet in `events.jsonl`
///
/// Key log patterns (from `process-{ts}-{pid}.log`):
/// - `Workspace initialized: {uuid}` — session ID
/// - `Starting Copilot CLI: {version}` — CLI version
/// - `Session named: "{name}"` — session title after first AI response
/// - `Session indexing debug: ..., repository={owner}/{repo}` — git remote
///
/// Key event types (from `events.jsonl`), see `parse_events_line`:
/// - `assistant.turn_start` — one per AI turn
/// - `assistant.message` — carries `outputTokens` and the live `model`;
///   accumulated into `token_history` for the Tokens panel sparkline
/// - `session.start` / `session.resume` — carries `selectedModel` and
///   `reasoningEffort` (initial values, don't clobber live-observed ones)
/// - `session.model_change` — carries `newModel` / `reasoningEffort` (live
///   updates when the user switches model/effort mid-session)
/// - `session.compaction_start` — carries `currentTokens` / `tokenLimit`
///
/// Optional richer telemetry via OpenTelemetry (opt-in, see
/// `copilot help monitoring`): `events.jsonl` has no live per-turn input or
/// cache-token counts — those only exist in Copilot CLI's OTel file exporter,
/// disabled by default. If a Copilot process was launched with
/// `COPILOT_OTEL_ENABLED=true` and `COPILOT_OTEL_FILE_EXPORTER_PATH=<path>`
/// set, `find_otel_export_path` discovers that path from the process's own
/// environment (`ps eww`) and `parse_otel_line` extracts full
/// input/output/cache-read/cache-write tokens plus a live context-window
/// snapshot from each "chat {model}" span — see `OtelResult`. When present,
/// this takes priority over the `events.jsonl`-derived numbers above.
pub struct CopilotCollector {
    logs_dir: PathBuf,
    settings_path: PathBuf,
    /// `~/.copilot/session-state` — parent of each session's `events.jsonl`.
    session_state_dir: PathBuf,
    /// Cached model name read from settings.json (refreshed on slow ticks).
    cached_model: String,
    /// Cached reasoning effort level read from settings.json (refreshed on slow ticks).
    cached_effort: String,
    /// Incremental log parse state, keyed by PID.
    log_cache: HashMap<u32, LogCache>,
    /// Incremental `events.jsonl` parse state, keyed by PID.
    events_cache: HashMap<u32, EventsCache>,
    /// OTel file-exporter path discovered per PID (`None` if not opted in),
    /// cached so `ps eww` only runs once per process rather than every tick.
    otel_paths: HashMap<u32, Option<PathBuf>>,
    /// Incremental OTel file-exporter parse state, keyed by PID.
    otel_cache: HashMap<u32, OtelCache>,
}

/// Incremental parse state for a single Copilot CLI log file.
struct LogCache {
    path: PathBuf,
    /// Byte offset read so far.
    offset: u64,
    /// Buffer for an incomplete trailing line.
    partial: String,
    /// Cumulative parse result.
    result: LogResult,
}

/// Data extracted from a Copilot CLI log file.
#[derive(Default, Clone)]
struct LogResult {
    session_id: String,
    version: String,
    session_name: String,
    repository: String,
    /// Context utilization 0–100.
    context_pct: f64,
    /// Tokens currently used in context window.
    context_tokens: u64,
    /// Total context window size in tokens.
    context_window: u64,
    /// Number of AI turns (= "Sending request" events).
    turn_count: u32,
    /// True when the last event was "Sending request" (model is thinking).
    model_generating: bool,
    /// Unix-epoch ms of when the most recent "Sending request" was seen.
    thinking_since_ms: u64,
    /// Epoch-ms of the first log line (= session start time).
    started_at_ms: u64,
    /// Last log line timestamp (epoch ms) — used for status detection.
    last_event_ms: u64,
}

/// Incremental parse state for a single Copilot CLI `events.jsonl` file.
struct EventsCache {
    path: PathBuf,
    /// Byte offset read so far.
    offset: u64,
    /// Buffer for an incomplete trailing line.
    partial: String,
    /// Cumulative parse result.
    result: EventsResult,
}

/// Incremental parse state for an OTel file-exporter stream. The file may be
/// shared across multiple concurrent Copilot CLI processes, so lines are
/// filtered to `session_id` on every read.
struct OtelCache {
    path: PathBuf,
    session_id: String,
    /// Byte offset read so far.
    offset: u64,
    /// Buffer for an incomplete trailing line.
    partial: String,
    /// Cumulative parse result.
    result: OtelResult,
}

/// Cumulative parse result from a Copilot CLI `events.jsonl` runtime telemetry
/// file (`~/.copilot/session-state/{sessionId}/events.jsonl`). Unlike the
/// human-readable log, this is a real per-session JSONL event stream and is
/// where turn/token/context data actually lives in current Copilot CLI builds
/// (the log-file patterns above no longer carry it).
#[derive(Default, Clone)]
struct EventsResult {
    /// Number of `assistant.turn_start` events seen.
    turn_count: u32,
    /// Cumulative `outputTokens` summed across `assistant.message` events.
    total_output_tokens: u64,
    /// Most recently observed model name (from `assistant.message` or
    /// `session.start`/`session.resume`).
    model: String,
    /// Context tokens in use as of the last `session.compaction_start` event.
    context_tokens: u64,
    /// Context window size (tokenLimit) as of the last `session.compaction_start` event.
    context_window: u64,
    /// Most recently observed reasoning effort level (from `session.model_change`
    /// or `session.start`/`session.resume`).
    effort: String,
    /// Per-turn `outputTokens` history, capped like Claude/Codex's equivalent
    /// (`claude.rs`/`codex.rs`), for the Tokens panel sparkline.
    token_history: Vec<u64>,
}

/// Same history cap used by `ClaudeCollector`/`CodexCollector`.
const MAX_TOKEN_HISTORY: usize = 10_000;

/// Cumulative parse result from a Copilot CLI OpenTelemetry file-exporter
/// stream (opt-in via `COPILOT_OTEL_FILE_EXPORTER_PATH`, see
/// `find_otel_export_path`). Each "chat {model}" span carries the full
/// input/output/cache-read/cache-write token breakdown for one LLM call plus
/// a live context-window snapshot — richer than anything `events.jsonl`
/// exposes, but only present for sessions the user has explicitly opted in.
#[derive(Default, Clone)]
struct OtelResult {
    total_input_tokens: u64,
    total_output_tokens: u64,
    total_cache_read: u64,
    total_cache_create: u64,
    /// Per-call (input+output) history, capped like `EventsResult::token_history`.
    token_history: Vec<u64>,
    model: String,
    effort: String,
    /// Context tokens in use as of the most recent chat span's embedded
    /// `github.copilot.session.usage_info` event.
    context_tokens: u64,
    /// Context window size as of the same event.
    context_window: u64,
    /// Number of matching "chat" spans folded in so far. Zero means this
    /// session has no OTel data yet (or none at all) — callers should fall
    /// back to the `events.jsonl`-based tracking in that case.
    calls_seen: u32,
}

/// Parse one line of an OTel file-exporter JSONL stream and fold it into
/// `result`, but only for spans belonging to `session_id` (the file may be
/// shared across multiple concurrent Copilot CLI processes, disambiguated by
/// `gen_ai.conversation.id`). Non-span records (metrics) and spans for other
/// sessions are ignored.
fn parse_otel_line(line: &str, session_id: &str, result: &mut OtelResult) {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
        return;
    };
    if value.get("type").and_then(|t| t.as_str()) != Some("span") {
        return;
    }
    let Some(attrs) = value.get("attributes") else {
        return;
    };
    let Some(cid) = attrs.get("gen_ai.conversation.id").and_then(|c| c.as_str()) else {
        return;
    };
    if cid != session_id {
        return;
    }
    let Some(input) = attrs
        .get("gen_ai.usage.input_tokens")
        .and_then(|v| v.as_u64())
    else {
        return;
    };
    let output = attrs
        .get("gen_ai.usage.output_tokens")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let cache_read = attrs
        .get("gen_ai.usage.cache_read.input_tokens")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let cache_create = attrs
        .get("gen_ai.usage.cache_creation.input_tokens")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);

    result.total_input_tokens += input;
    result.total_output_tokens += output;
    result.total_cache_read += cache_read;
    result.total_cache_create += cache_create;
    result.calls_seen += 1;
    if result.token_history.len() < MAX_TOKEN_HISTORY {
        result.token_history.push(input + output);
    }
    if let Some(model) = attrs.get("gen_ai.response.model").and_then(|m| m.as_str()) {
        if !model.is_empty() {
            result.model = model.to_string();
        }
    }
    if let Some(effort) = attrs
        .get("gen_ai.request.reasoning.level")
        .and_then(|e| e.as_str())
    {
        if !effort.is_empty() {
            result.effort = effort.to_string();
        }
    }

    if let Some(events) = value.get("events").and_then(|e| e.as_array()) {
        for event in events {
            if event.get("name").and_then(|n| n.as_str())
                != Some("github.copilot.session.usage_info")
            {
                continue;
            }
            let Some(a) = event.get("attributes") else {
                continue;
            };
            if let Some(cur) = a
                .get("github.copilot.current_tokens")
                .and_then(|v| v.as_u64())
            {
                result.context_tokens = cur;
            }
            if let Some(limit) = a.get("github.copilot.token_limit").and_then(|v| v.as_u64()) {
                result.context_window = limit;
            }
        }
    }
}

/// Parse one line of `events.jsonl` and fold it into `result`. Unknown event
/// types and malformed lines are silently ignored.
fn parse_events_line(line: &str, result: &mut EventsResult) {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
        return;
    };
    let Some(event_type) = value.get("type").and_then(|t| t.as_str()) else {
        return;
    };

    if event_type == "assistant.turn_start" {
        result.turn_count += 1;
        return;
    }

    let data = value.get("data");

    if event_type == "assistant.message" {
        if let Some(tokens) = data
            .and_then(|d| d.get("outputTokens"))
            .and_then(|t| t.as_u64())
        {
            result.total_output_tokens += tokens;
            if result.token_history.len() < MAX_TOKEN_HISTORY {
                result.token_history.push(tokens);
            }
        }
        if let Some(model) = data.and_then(|d| d.get("model")).and_then(|m| m.as_str()) {
            if !model.is_empty() {
                result.model = model.to_string();
            }
        }
        return;
    }

    if event_type == "session.start" || event_type == "session.resume" {
        if result.model.is_empty() {
            if let Some(model) = data
                .and_then(|d| d.get("selectedModel"))
                .and_then(|m| m.as_str())
            {
                result.model = model.to_string();
            }
        }
        if result.effort.is_empty() {
            if let Some(effort) = data
                .and_then(|d| d.get("reasoningEffort"))
                .and_then(|e| e.as_str())
            {
                result.effort = effort.to_string();
            }
        }
        return;
    }

    if event_type == "session.model_change" {
        if let Some(model) = data
            .and_then(|d| d.get("newModel"))
            .and_then(|m| m.as_str())
        {
            if !model.is_empty() {
                result.model = model.to_string();
            }
        }
        if let Some(effort) = data
            .and_then(|d| d.get("reasoningEffort"))
            .and_then(|e| e.as_str())
        {
            result.effort = effort.to_string();
        }
        return;
    }

    if event_type == "session.compaction_start" {
        if let Some(cur) = data
            .and_then(|d| d.get("currentTokens"))
            .and_then(|t| t.as_u64())
        {
            result.context_tokens = cur;
        }
        if let Some(limit) = data
            .and_then(|d| d.get("tokenLimit"))
            .and_then(|t| t.as_u64())
        {
            result.context_window = limit;
        }
    }
}

impl CopilotCollector {
    pub fn new() -> Self {
        let home = dirs::home_dir().unwrap_or_default();
        Self {
            logs_dir: home.join(".copilot").join("logs"),
            settings_path: home.join(".copilot").join("settings.json"),
            session_state_dir: home.join(".copilot").join("session-state"),
            cached_model: String::new(),
            cached_effort: String::new(),
            log_cache: HashMap::new(),
            events_cache: HashMap::new(),
            otel_paths: HashMap::new(),
            otel_cache: HashMap::new(),
        }
    }

    /// Config root directory for this session's agent (home-abbreviated, e.g. "~/.copilot").
    fn config_root(&self) -> String {
        super::abbrev_path(self.logs_dir.parent().unwrap_or(std::path::Path::new(".")))
    }

    fn collect_sessions(&mut self, shared: &super::SharedProcessData) -> Vec<AgentSession> {
        if !self.logs_dir.exists() {
            return vec![];
        }

        if shared.slow_tick {
            self.cached_model = read_model_from_settings(&self.settings_path);
            self.cached_effort = read_effort_from_settings(&self.settings_path);
        }
        if self.cached_model.is_empty() {
            self.cached_model = read_model_from_settings(&self.settings_path);
        }
        if self.cached_effort.is_empty() {
            self.cached_effort = read_effort_from_settings(&self.settings_path);
        }

        // Step 1: find running copilot PIDs
        let copilot_pids = find_copilot_pids(&shared.process_info);

        // Step 2: find log file for each PID
        let pid_to_log = map_pid_to_log(&copilot_pids, &self.logs_dir);

        // Step 3: parse/update each log file
        let mut sessions = Vec::new();
        let mut seen_pids: std::collections::HashSet<u32> = std::collections::HashSet::new();

        for (pid, log_path) in &pid_to_log {
            let pid = *pid;
            seen_pids.insert(pid);

            let cache = self.log_cache.entry(pid).or_insert_with(|| LogCache {
                path: log_path.clone(),
                offset: 0,
                partial: String::new(),
                result: LogResult::default(),
            });

            // If the path changed (shouldn't normally happen), reset
            if cache.path != *log_path {
                *cache = LogCache {
                    path: log_path.clone(),
                    offset: 0,
                    partial: String::new(),
                    result: LogResult::default(),
                };
            }

            update_log_cache(cache);

            let result = &cache.result;
            if result.session_id.is_empty() {
                continue;
            }

            // Runtime telemetry (turns/tokens/context/live model) lives in
            // events.jsonl, not the log file, on current Copilot CLI builds.
            let events_path = self
                .session_state_dir
                .join(&result.session_id)
                .join("events.jsonl");
            let events_cache = self.events_cache.entry(pid).or_insert_with(|| EventsCache {
                path: events_path.clone(),
                offset: 0,
                partial: String::new(),
                result: EventsResult::default(),
            });
            if events_cache.path != events_path {
                *events_cache = EventsCache {
                    path: events_path.clone(),
                    offset: 0,
                    partial: String::new(),
                    result: EventsResult::default(),
                };
            }
            update_events_cache(events_cache);
            let events_result = events_cache.result.clone();

            // Optional richer telemetry: if this process was launched with
            // COPILOT_OTEL_FILE_EXPORTER_PATH set (see `copilot help
            // monitoring`), it carries full input/cache-token accounting and
            // a live context-window snapshot per LLM call — everything
            // events.jsonl can't provide. Off by default; gracefully absent
            // for the common case.
            let otel_path = self
                .otel_paths
                .entry(pid)
                .or_insert_with(|| find_otel_export_path(pid))
                .clone();
            if let Some(path) = &otel_path {
                let needs_reset = self
                    .otel_cache
                    .get(&pid)
                    .map(|c| c.path != *path || c.session_id != result.session_id)
                    .unwrap_or(true);
                if needs_reset {
                    self.otel_cache.insert(
                        pid,
                        OtelCache {
                            path: path.clone(),
                            session_id: result.session_id.clone(),
                            offset: 0,
                            partial: String::new(),
                            result: OtelResult::default(),
                        },
                    );
                }
                if let Some(cache) = self.otel_cache.get_mut(&pid) {
                    update_otel_cache(cache);
                }
            }
            let otel = self
                .otel_cache
                .get(&pid)
                .filter(|c| c.result.calls_seen > 0)
                .map(|c| c.result.clone());

            let proc = shared.process_info.get(&pid);
            let mem_mb = proc.map(|p| p.rss_kb / 1024).unwrap_or(0);

            // CWD: get from lsof or /proc
            let cwd = get_process_cwd(pid).unwrap_or_default();
            let project_name = process::last_path_segment(&cwd).unwrap_or("?").to_string();

            // Git branch from git command (MultiCollector fills git_added/modified later)
            let git_branch = get_git_branch(&cwd);

            // Status detection
            let now_ms = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis() as u64;
            let pid_alive = proc.is_some();
            let log_age_secs = if result.last_event_ms > 0 {
                (now_ms.saturating_sub(result.last_event_ms)) / 1000
            } else {
                9999
            };

            let status = if !pid_alive {
                SessionStatus::Done
            } else if result.model_generating {
                SessionStatus::Thinking
            } else if log_age_secs < 30 {
                // Recent activity but model not actively generating — executing tool or just finished
                let has_active_child = process::has_active_descendant(
                    pid,
                    &shared.children_map,
                    &shared.process_info,
                    5.0,
                );
                if has_active_child {
                    SessionStatus::Executing
                } else {
                    SessionStatus::Waiting
                }
            } else {
                SessionStatus::Waiting
            };

            // Current task description
            let current_tasks = tasks_for_status(&status);

            // Children: collect all descendants recursively
            let mut children = Vec::new();
            {
                let mut stack: Vec<u32> =
                    shared.children_map.get(&pid).cloned().unwrap_or_default();
                let mut visited = std::collections::HashSet::new();
                while let Some(cpid) = stack.pop() {
                    if !visited.insert(cpid) {
                        continue;
                    }
                    if let Some(cproc) = shared.process_info.get(&cpid) {
                        let port = shared.ports.get(&cpid).and_then(|v| v.first().copied());
                        children.push(ChildProcess {
                            pid: cpid,
                            command: cproc.command.clone(),
                            mem_kb: cproc.rss_kb,
                            port,
                        });
                    }
                    if let Some(grandchildren) = shared.children_map.get(&cpid) {
                        stack.extend(grandchildren);
                    }
                }
            }

            let events_model = resolved_model(&events_result.model, &self.cached_model);
            let model = resolved_model(
                otel.as_ref().map(|o| o.model.as_str()).unwrap_or(""),
                &events_model,
            );
            let events_effort = resolved_effort(&events_result.effort, &self.cached_effort);
            let effort = resolved_effort(
                otel.as_ref().map(|o| o.effort.as_str()).unwrap_or(""),
                &events_effort,
            );
            let turn_count = resolved_turn_count(events_result.turn_count, result.turn_count);

            let mut context_tokens =
                resolved_context_tokens(events_result.context_tokens, result.context_tokens);
            let mut context_window = resolved_context_window(
                events_result.context_window,
                result.context_window,
                &model,
            );
            // OTel's context snapshot updates on every LLM call (vs. only at
            // compaction for events.jsonl), so prefer it when present.
            if let Some(o) = otel.as_ref() {
                if o.context_window > 0 {
                    context_tokens = o.context_tokens;
                    context_window = o.context_window;
                }
            }
            let context_percent = if context_tokens > 0 && context_window > 0 {
                (context_tokens as f64 / context_window as f64) * 100.0
            } else {
                0.0
            };

            // OTel gives the full input/cache breakdown per call; without it,
            // input/cache stay best-effort (only the compaction-derived
            // context_tokens approximates "input", cache stays unknown).
            let total_input_tokens = otel
                .as_ref()
                .map(|o| o.total_input_tokens)
                .unwrap_or(context_tokens);
            let total_output_tokens = otel
                .as_ref()
                .map(|o| o.total_output_tokens)
                .unwrap_or(events_result.total_output_tokens);
            let total_cache_read = otel.as_ref().map(|o| o.total_cache_read).unwrap_or(0);
            let total_cache_create = otel.as_ref().map(|o| o.total_cache_create).unwrap_or(0);
            let token_history = otel
                .as_ref()
                .map(|o| o.token_history.clone())
                .unwrap_or_else(|| events_result.token_history.clone());

            let session_name = if !result.session_name.is_empty() {
                result.session_name.clone()
            } else if !result.repository.is_empty() {
                result.repository.clone()
            } else {
                project_name.clone()
            };

            sessions.push(AgentSession {
                agent_cli: "copilot",
                pid,
                session_id: result.session_id.clone(),
                cwd,
                project_name,
                started_at: result.started_at_ms,
                status,
                model,
                effort,
                context_percent,
                total_input_tokens,
                total_output_tokens,
                total_cache_read,
                total_cache_create,
                turn_count,
                current_tasks,
                mem_mb,
                version: result.version.clone(),
                git_branch,
                git_added: 0,
                git_modified: 0,
                token_history,
                context_history: vec![],
                compaction_count: 0,
                context_window,
                subagents: vec![],
                mem_file_count: 0,
                mem_line_count: 0,
                children,
                initial_prompt: session_name,
                first_assistant_text: String::new(),
                chat_messages: vec![],
                tool_calls: vec![],
                pending_since_ms: 0,
                thinking_since_ms: result.thinking_since_ms,
                file_accesses: vec![],
                config_root: self.config_root(),
            });
        }

        // Evict stale cache entries (PIDs no longer running)
        self.log_cache.retain(|pid, _| seen_pids.contains(pid));
        self.events_cache.retain(|pid, _| seen_pids.contains(pid));

        sessions.sort_by_key(|s| std::cmp::Reverse(s.started_at));
        sessions
    }
}

impl super::AgentCollector for CopilotCollector {
    fn collect(&mut self, shared: &super::SharedProcessData) -> Vec<AgentSession> {
        self.collect_sessions(shared)
    }
}

impl Default for CopilotCollector {
    fn default() -> Self {
        Self::new()
    }
}

/// Find PIDs of running GitHub Copilot CLI processes.
/// Matches binaries named exactly `copilot` whose path contains `copilot-cli`.
/// Excludes `copilot-language-server` and IDE plugin processes.
fn find_copilot_pids(process_info: &HashMap<u32, ProcInfo>) -> Vec<u32> {
    let mut pids = Vec::new();
    for (pid, info) in process_info {
        let cmd = &info.command;
        // The copilot-cli binary path: e.g. /opt/homebrew/Caskroom/copilot-cli/1.0.26/copilot
        // Or just: copilot
        // Exclude: copilot-language-server, JetBrains copilot-agent
        if cmd.contains("copilot-language-server")
            || cmd.contains("copilot-agent")
            || cmd.contains("copilot-intellij")
        {
            continue;
        }
        if is_copilot_cli(cmd) {
            pids.push(*pid);
        }
    }
    pids
}

/// Returns true if the command string represents a GitHub Copilot CLI process.
/// The binary must be named `copilot` (exact match) with a `copilot-cli` ancestor
/// in the path, OR a bare `copilot` command (no path separators).
fn is_copilot_cli(cmd: &str) -> bool {
    let binary = cmd.split_whitespace().next().unwrap_or("");
    // Path contains copilot-cli directory (Homebrew Cask layout)
    if binary.contains("copilot-cli") {
        let base = binary.rsplit('/').next().unwrap_or(binary);
        return base == "copilot";
    }
    // Bare `copilot` command (no path separators, e.g. in PATH)
    if !binary.contains('/') && !binary.contains('\\') {
        return binary == "copilot";
    }
    false
}

/// Scan `~/.copilot/logs/` for log files whose PID component matches a running PID.
/// Log file name format: `process-{timestamp_ms}-{pid}.log`
fn map_pid_to_log(pids: &[u32], logs_dir: &Path) -> HashMap<u32, PathBuf> {
    let mut map = HashMap::new();
    if pids.is_empty() || !logs_dir.exists() {
        return map;
    }

    let pid_set: std::collections::HashSet<u32> = pids.iter().copied().collect();

    let entries = match fs::read_dir(logs_dir) {
        Ok(e) => e,
        Err(_) => return map,
    };

    for entry in entries.flatten() {
        // Skip symlinks
        if entry.file_type().map(|ft| ft.is_symlink()).unwrap_or(true) {
            continue;
        }
        let path = entry.path();
        let name = match path.file_name().and_then(|n| n.to_str()) {
            Some(n) => n,
            None => continue,
        };
        // Format: process-{ts}-{pid}.log
        if !name.starts_with("process-") || !name.ends_with(".log") {
            continue;
        }
        let inner = &name[8..name.len() - 4]; // strip "process-" and ".log"
                                              // inner = "{ts}-{pid}" — split at last '-'
        if let Some(dash_pos) = inner.rfind('-') {
            let pid_str = &inner[dash_pos + 1..];
            if let Ok(pid) = pid_str.parse::<u32>() {
                if pid_set.contains(&pid) {
                    // If we already have a log for this PID, keep the newer one
                    let existing = map.get(&pid);
                    let should_insert = existing.is_none() || {
                        let existing_mtime = fs::metadata(existing.unwrap())
                            .and_then(|m| m.modified())
                            .unwrap_or(SystemTime::UNIX_EPOCH);
                        let new_mtime = fs::metadata(&path)
                            .and_then(|m| m.modified())
                            .unwrap_or(SystemTime::UNIX_EPOCH);
                        new_mtime > existing_mtime
                    };
                    if should_insert {
                        map.insert(pid, path);
                    }
                }
            }
        }
    }
    map
}

/// Parse/update a log file cache incrementally.
/// On first call, reads the entire file. On subsequent calls, reads only new bytes.
fn update_log_cache(cache: &mut LogCache) {
    let mut file = match fs::OpenOptions::new().read(true).open(&cache.path) {
        Ok(f) => f,
        Err(_) => return,
    };

    let file_len = match file.metadata() {
        Ok(m) => m.len(),
        Err(_) => return,
    };

    // Detect file rotation (file got shorter — shouldn't happen for logs, but be safe)
    if file_len < cache.offset {
        cache.offset = 0;
        cache.partial.clear();
        cache.result = LogResult::default();
    }

    if file_len == cache.offset {
        return; // No new data
    }

    if file.seek(SeekFrom::Start(cache.offset)).is_err() {
        return;
    }

    let mut new_bytes = Vec::with_capacity((file_len - cache.offset).min(1024 * 1024) as usize);
    if file.read_to_end(&mut new_bytes).is_err() {
        return;
    }

    cache.offset = file_len;

    let text = String::from_utf8_lossy(&new_bytes);
    let combined = format!("{}{}", cache.partial, text);
    cache.partial.clear();

    // Re-process: split on '\n' and handle partial last line
    let full_text = combined;
    let (complete, partial) = if let Some(last_nl) = full_text.rfind('\n') {
        (&full_text[..last_nl], &full_text[last_nl + 1..])
    } else {
        ("", full_text.as_str())
    };

    cache.partial = partial.to_string();

    for line in complete.lines() {
        parse_log_line(line, &mut cache.result);
    }
}

/// Parse/update an `events.jsonl` cache incrementally. Same incremental-read
/// shape as `update_log_cache`, but each complete line is a JSON object parsed
/// by `parse_events_line` rather than a human-readable log line.
fn update_events_cache(cache: &mut EventsCache) {
    let mut file = match fs::OpenOptions::new().read(true).open(&cache.path) {
        Ok(f) => f,
        Err(_) => return,
    };

    let file_len = match file.metadata() {
        Ok(m) => m.len(),
        Err(_) => return,
    };

    if file_len < cache.offset {
        cache.offset = 0;
        cache.partial.clear();
        cache.result = EventsResult::default();
    }

    if file_len == cache.offset {
        return;
    }

    if file.seek(SeekFrom::Start(cache.offset)).is_err() {
        return;
    }

    let mut new_bytes = Vec::with_capacity((file_len - cache.offset).min(1024 * 1024) as usize);
    if file.read_to_end(&mut new_bytes).is_err() {
        return;
    }

    cache.offset = file_len;

    let text = String::from_utf8_lossy(&new_bytes);
    let combined = format!("{}{}", cache.partial, text);
    cache.partial.clear();

    let (complete, partial) = if let Some(last_nl) = combined.rfind('\n') {
        (
            combined[..last_nl].to_string(),
            combined[last_nl + 1..].to_string(),
        )
    } else {
        (String::new(), combined)
    };

    cache.partial = partial;

    for line in complete.lines() {
        parse_events_line(line, &mut cache.result);
    }
}

/// Parse/update an OTel file-exporter cache incrementally. Same
/// incremental-read shape as `update_events_cache`/`update_log_cache`, but
/// each complete line is filtered to `cache.session_id` by `parse_otel_line`
/// since the file may be shared across concurrent Copilot CLI processes.
fn update_otel_cache(cache: &mut OtelCache) {
    let mut file = match fs::OpenOptions::new().read(true).open(&cache.path) {
        Ok(f) => f,
        Err(_) => return,
    };

    let file_len = match file.metadata() {
        Ok(m) => m.len(),
        Err(_) => return,
    };

    if file_len < cache.offset {
        cache.offset = 0;
        cache.partial.clear();
        cache.result = OtelResult::default();
    }

    if file_len == cache.offset {
        return;
    }

    if file.seek(SeekFrom::Start(cache.offset)).is_err() {
        return;
    }

    let mut new_bytes = Vec::with_capacity((file_len - cache.offset).min(1024 * 1024) as usize);
    if file.read_to_end(&mut new_bytes).is_err() {
        return;
    }

    cache.offset = file_len;

    let text = String::from_utf8_lossy(&new_bytes);
    let combined = format!("{}{}", cache.partial, text);
    cache.partial.clear();

    let (complete, partial) = if let Some(last_nl) = combined.rfind('\n') {
        (
            combined[..last_nl].to_string(),
            combined[last_nl + 1..].to_string(),
        )
    } else {
        (String::new(), combined)
    };

    cache.partial = partial;

    for line in complete.lines() {
        parse_otel_line(line, &cache.session_id, &mut cache.result);
    }
}

/// Parse a single log line and update the result.
fn parse_log_line(line: &str, result: &mut LogResult) {
    // Extract timestamp from line start: "2026-05-07T07:41:48.151Z [INFO] ..."
    let ts_ms = parse_log_timestamp(line);
    if ts_ms > 0 {
        if result.started_at_ms == 0 {
            result.started_at_ms = ts_ms;
        }
        result.last_event_ms = ts_ms;
    }

    // Skip the timestamp+level prefix to get the message
    let msg = strip_log_prefix(line);

    if let Some(uuid) = msg.strip_prefix("Workspace initialized: ") {
        // "e840638a-9964-44a8-b41e-4ca8afe82103 (checkpoints: 0)"
        let uuid = uuid.split_whitespace().next().unwrap_or("").trim();
        if is_uuid(uuid) && result.session_id.is_empty() {
            result.session_id = uuid.to_string();
        }
        return;
    }

    if let Some(ver) = msg.strip_prefix("Starting Copilot CLI: ") {
        result.version = ver.trim().to_string();
        return;
    }

    if let Some(rest) = msg.strip_prefix("Session named: ") {
        // 'Session named: "Add GitHub Copilot CLI Support"'
        let name = rest.trim().trim_matches('"');
        result.session_name = name.to_string();
        return;
    }

    if msg.starts_with("Session indexing debug:") {
        // "Session indexing debug: SESSION_INDEXING=false, repository=lifejwang11/abtop, ..."
        if let Some(repo_part) = msg.find("repository=") {
            let after = &msg[repo_part + "repository=".len()..];
            let repo = after.split(',').next().unwrap_or("").trim();
            if !repo.is_empty() && repo != "undefined" {
                result.repository = repo.to_string();
            }
        }
        return;
    }

    if let Some(rest) = msg.strip_prefix("CompactionProcessor: Utilization ") {
        // "22.3% (28516/128000 tokens) below threshold 80%"
        parse_compaction_line(rest, result);
        return;
    }

    if msg.contains("--- Start of group: Sending request to the AI model ---") {
        result.turn_count += 1;
        result.model_generating = true;
        result.thinking_since_ms = ts_ms;
        return;
    }

    if msg.contains("--- End of group ---") {
        result.model_generating = false;
    }
}

/// Parse the CompactionProcessor utilization line.
/// Input: "22.3% (28516/128000 tokens) below threshold 80%"
fn parse_compaction_line(rest: &str, result: &mut LogResult) {
    // Extract percentage
    if let Some(pct_end) = rest.find('%') {
        if let Ok(pct) = rest[..pct_end].trim().parse::<f64>() {
            result.context_pct = pct;
        }
    }
    // Extract used/total from "(28516/128000 tokens)"
    if let (Some(open), Some(close)) = (rest.find('('), rest.find(')')) {
        let inner = &rest[open + 1..close];
        let parts: Vec<&str> = inner.split('/').collect();
        if parts.len() == 2 {
            let used_str = parts[0].trim();
            let total_part = parts[1].split_whitespace().next().unwrap_or("");
            if let (Ok(used), Ok(total)) = (used_str.parse::<u64>(), total_part.parse::<u64>()) {
                result.context_tokens = used;
                result.context_window = total;
            }
        }
    }
}

/// Extract timestamp from a log line and return it as Unix epoch milliseconds.
/// Log format: "2026-05-07T07:41:48.151Z [INFO] ..."
fn parse_log_timestamp(line: &str) -> u64 {
    let ts_end = line.find(' ').unwrap_or(0);
    if ts_end == 0 {
        return 0;
    }
    let ts_str = &line[..ts_end];
    // Parse ISO 8601 timestamp
    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(ts_str) {
        return dt.timestamp_millis() as u64;
    }
    0
}

/// Strip the "2026-05-07T07:41:48.151Z [INFO] " prefix from a log line.
fn strip_log_prefix(line: &str) -> &str {
    // Format: "{ts} [{LEVEL}] {message}"
    let after_ts = line.find(' ').map(|i| &line[i + 1..]).unwrap_or(line);
    // Skip "[INFO] " or "[ERROR] " etc.
    if after_ts.starts_with('[') {
        if let Some(bracket_close) = after_ts.find("] ") {
            return &after_ts[bracket_close + 2..];
        }
    }
    after_ts
}

/// Return true if the string looks like a UUID (8-4-4-4-12 hex).
fn is_uuid(s: &str) -> bool {
    let parts: Vec<&str> = s.split('-').collect();
    if parts.len() != 5 {
        return false;
    }
    let lengths = [8, 4, 4, 4, 12];
    for (p, &l) in parts.iter().zip(lengths.iter()) {
        if p.len() != l || !p.chars().all(|c| c.is_ascii_hexdigit()) {
            return false;
        }
    }
    true
}

/// Read the configured model name from `~/.copilot/settings.json`.
fn read_model_from_settings(path: &Path) -> String {
    let content = match fs::read_to_string(path) {
        Ok(c) => c,
        Err(_) => return String::new(),
    };
    let json: serde_json::Value = match serde_json::from_str(&content) {
        Ok(v) => v,
        Err(_) => return String::new(),
    };
    json["model"].as_str().unwrap_or("").to_string()
}

/// Read the configured reasoning effort level from `~/.copilot/settings.json`.
/// Last-resort fallback for when no live effort has been observed yet in
/// `events.jsonl`.
fn read_effort_from_settings(path: &Path) -> String {
    let content = match fs::read_to_string(path) {
        Ok(c) => c,
        Err(_) => return String::new(),
    };
    let json: serde_json::Value = match serde_json::from_str(&content) {
        Ok(v) => v,
        Err(_) => return String::new(),
    };
    json["effortLevel"].as_str().unwrap_or("").to_string()
}

/// Get the current working directory of a process.
/// On Linux, reads `/proc/{pid}/cwd` symlink.
/// On macOS/other, uses `lsof -p {pid} -a -d cwd -F n`.
fn get_process_cwd(pid: u32) -> Option<String> {
    #[cfg(target_os = "linux")]
    {
        let cwd_link = format!("/proc/{}/cwd", pid);
        return std::fs::read_link(&cwd_link)
            .ok()
            .and_then(|p| p.to_str().map(|s| s.to_string()));
    }

    #[cfg(not(target_os = "linux"))]
    {
        // lsof -p {pid} -a -d cwd -F n
        // Output:
        //   p{pid}
        //   fcwd
        //   n{/path}
        let output = Command::new("lsof")
            .args(["-p", &pid.to_string(), "-a", "-d", "cwd", "-F", "n", "--"])
            .output()
            .ok()?;
        let stdout = String::from_utf8_lossy(&output.stdout);
        for line in stdout.lines() {
            if let Some(path) = line.strip_prefix('n') {
                if !path.is_empty() && path.starts_with('/') {
                    return Some(path.to_string());
                }
            }
        }
        None
    }
}

/// Human-readable current-task description for a session status.
/// Exhaustive over all `SessionStatus` variants so a future variant fails to
/// compile here rather than silently falling through.
fn tasks_for_status(status: &SessionStatus) -> Vec<String> {
    match status {
        SessionStatus::Thinking => vec!["thinking...".to_string()],
        SessionStatus::Executing => vec!["executing...".to_string()],
        SessionStatus::Waiting => vec!["waiting for input".to_string()],
        SessionStatus::Done => vec!["finished".to_string()],
        SessionStatus::RateLimited => vec!["rate limited".to_string()],
        SessionStatus::Unknown => vec![],
    }
}

/// Extract a single `KEY=value` token from `ps eww`-style output (env vars
/// appended after COMMAND, space-separated). Used only to pull out one
/// specific, non-secret variable name — never call this expecting to inspect
/// or log the surrounding output, which may contain other processes'
/// credentials/API keys.
fn extract_ps_env_value(ps_output: &str, key: &str) -> Option<String> {
    let prefix = format!("{key}=");
    ps_output
        .split_whitespace()
        .find_map(|tok| tok.strip_prefix(&prefix))
        .map(|v| v.to_string())
}

/// Discover the OpenTelemetry file-exporter path a Copilot CLI process was
/// launched with, if any (`COPILOT_OTEL_FILE_EXPORTER_PATH`). This is how a
/// user opts a session into full input/cache token + live context telemetry
/// (see the `copilot help monitoring` topic) — disabled by default, so
/// `None` is the common case. Reads the process's own environment via
/// `ps eww`, which only works for processes owned by the current user; the
/// full env blob (which may contain other processes' secrets) is discarded
/// immediately after extracting this one value.
#[cfg(not(windows))]
fn find_otel_export_path(pid: u32) -> Option<PathBuf> {
    let output = Command::new("ps")
        .args(["eww", "-p", &pid.to_string()])
        .output()
        .ok()?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    extract_ps_env_value(&stdout, "COPILOT_OTEL_FILE_EXPORTER_PATH").map(PathBuf::from)
}

#[cfg(windows)]
fn find_otel_export_path(_pid: u32) -> Option<PathBuf> {
    None
}

/// Get the current git branch for a directory.
fn get_git_branch(cwd: &str) -> String {
    if cwd.is_empty() {
        return String::new();
    }
    let output = Command::new("git")
        .args(["-C", cwd, "rev-parse", "--abbrev-ref", "HEAD"])
        .output()
        .ok();
    if let Some(out) = output {
        if out.status.success() {
            return String::from_utf8_lossy(&out.stdout).trim().to_string();
        }
    }
    String::new()
}

/// Prefer the live model observed in `events.jsonl`; fall back to the
/// globally configured model from `settings.json` when the session hasn't
/// emitted one yet (e.g. very first tick).
fn resolved_model(events_model: &str, fallback: &str) -> String {
    if events_model.is_empty() {
        fallback.to_string()
    } else {
        events_model.to_string()
    }
}

/// Prefer the live reasoning effort observed in `events.jsonl`; fall back to
/// the static `effortLevel` configured in `settings.json`.
fn resolved_effort(events_effort: &str, fallback: &str) -> String {
    if events_effort.is_empty() {
        fallback.to_string()
    } else {
        events_effort.to_string()
    }
}

/// Prefer the live turn count from `events.jsonl`; fall back to the log-based
/// count (dead in current Copilot CLI builds, kept for forward/backward
/// compatibility in case some build still emits the old log markers).
fn resolved_turn_count(events_turns: u32, log_turns: u32) -> u32 {
    if events_turns > 0 {
        events_turns
    } else {
        log_turns
    }
}

/// Prefer the live context-tokens-in-use figure from the last
/// `session.compaction_start` event; fall back to the log-based figure.
fn resolved_context_tokens(events_tokens: u64, log_tokens: u64) -> u64 {
    if events_tokens > 0 {
        events_tokens
    } else {
        log_tokens
    }
}

/// Prefer the live context window (tokenLimit) from the last
/// `session.compaction_start` event; fall back to the log-based figure; and
/// failing both (no compaction has occurred yet this session), fall back to
/// the same model-name heuristic used by the other collectors.
fn resolved_context_window(events_window: u64, log_window: u64, model: &str) -> u64 {
    if events_window > 0 {
        events_window
    } else if log_window > 0 {
        log_window
    } else {
        crate::collector::context_window_for_model(model, "", 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_ps_env_value_finds_the_key() {
        let ps_output = "  PID   TT  STAT      TIME COMMAND\n95457   ??  SN     0:00.42 /opt/homebrew/Caskroom/copilot-cli/1.0.75/copilot -p hi HOME=/Users/malcolm COPILOT_OTEL_FILE_EXPORTER_PATH=/tmp/otel.jsonl COPILOT_OTEL_ENABLED=true PATH=/usr/bin";
        assert_eq!(
            extract_ps_env_value(ps_output, "COPILOT_OTEL_FILE_EXPORTER_PATH"),
            Some("/tmp/otel.jsonl".to_string())
        );
    }

    #[test]
    fn extract_ps_env_value_missing_key_is_none() {
        let ps_output = "  PID   TT  STAT      TIME COMMAND\n95457   ??  SN     0:00.42 /opt/homebrew/Caskroom/copilot-cli/1.0.75/copilot -p hi HOME=/Users/malcolm PATH=/usr/bin";
        assert_eq!(
            extract_ps_env_value(ps_output, "COPILOT_OTEL_FILE_EXPORTER_PATH"),
            None
        );
    }

    // `find_otel_export_path` itself is a thin `ps eww` wrapper around
    // `extract_ps_env_value` (tested above) — same convention as
    // `get_process_cwd`/`get_git_branch` in this file, which shell out and
    // aren't unit tested directly. A process-spawn test was tried and
    // dropped: macOS SIP hides environ from `ps eww` for system-protected
    // binaries like `/bin/sh`/`/bin/sleep` even for their owning user, which
    // made every reliable, dependency-free test fixture a false negative.
    // The real `copilot` binary is an unprotected user-installed executable
    // and was verified manually to work correctly.

    #[test]
    fn is_copilot_cli_matches_cask_path() {
        assert!(is_copilot_cli(
            "/opt/homebrew/Caskroom/copilot-cli/1.0.26/copilot"
        ));
        assert!(is_copilot_cli(
            "/opt/homebrew/Caskroom/copilot-cli/1.0.43/copilot --flag"
        ));
    }

    #[test]
    fn is_copilot_cli_matches_bare_command() {
        assert!(is_copilot_cli("copilot"));
        assert!(is_copilot_cli("copilot --arg"));
    }

    #[test]
    fn is_copilot_cli_excludes_language_server() {
        assert!(!is_copilot_cli("/path/copilot-language-server --stdio"));
        // language server check happens in find_copilot_pids, not is_copilot_cli
        // is_copilot_cli itself doesn't see "language-server" in this path
        // but the copilot-language-server binary name contains a dash so it won't match
        assert!(!is_copilot_cli("copilot-language-server"));
    }

    #[test]
    fn is_copilot_cli_excludes_unrelated() {
        assert!(!is_copilot_cli("node"));
        assert!(!is_copilot_cli("claude"));
        assert!(!is_copilot_cli("/usr/bin/git"));
    }

    #[test]
    fn is_uuid_valid() {
        assert!(is_uuid("e840638a-9964-44a8-b41e-4ca8afe82103"));
        assert!(!is_uuid("not-a-uuid"));
        assert!(!is_uuid("e840638a-9964-44a8-b41e")); // too short
    }

    #[test]
    fn parse_log_timestamp_valid() {
        let line = "2026-05-07T07:41:48.151Z [INFO] Starting Copilot CLI: 1.0.43";
        let ts = parse_log_timestamp(line);
        assert!(ts > 0);
    }

    #[test]
    fn parse_log_timestamp_invalid() {
        let ts = parse_log_timestamp("not a log line");
        assert_eq!(ts, 0);
    }

    #[test]
    fn strip_log_prefix_extracts_message() {
        let line = "2026-05-07T07:41:48.191Z [INFO] Starting Copilot CLI: 1.0.43";
        assert_eq!(
            strip_log_prefix("2026-05-07T07:41:48.191Z [INFO] Starting Copilot CLI: 1.0.43"),
            "Starting Copilot CLI: 1.0.43"
        );
        let _ = line;
    }

    #[test]
    fn parse_compaction_line_extracts_values() {
        let mut result = LogResult::default();
        parse_compaction_line(
            "22.3% (28516/128000 tokens) below threshold 80%",
            &mut result,
        );
        assert!((result.context_pct - 22.3).abs() < 0.01);
        assert_eq!(result.context_tokens, 28516);
        assert_eq!(result.context_window, 128000);
    }

    #[test]
    fn parse_log_line_session_id() {
        let mut result = LogResult::default();
        parse_log_line(
            "2026-05-07T07:41:48.190Z [INFO] Workspace initialized: e840638a-9964-44a8-b41e-4ca8afe82103 (checkpoints: 0)",
            &mut result,
        );
        assert_eq!(result.session_id, "e840638a-9964-44a8-b41e-4ca8afe82103");
    }

    #[test]
    fn parse_log_line_session_name() {
        let mut result = LogResult::default();
        parse_log_line(
            r#"2026-05-07T07:42:18.879Z [INFO] Session named: "Add GitHub Copilot CLI Support""#,
            &mut result,
        );
        assert_eq!(result.session_name, "Add GitHub Copilot CLI Support");
    }

    #[test]
    fn parse_log_line_repository() {
        let mut result = LogResult::default();
        parse_log_line(
            "2026-05-07T07:41:48.168Z [INFO] Session indexing debug: SESSION_INDEXING=false, repository=lifejwang11/abtop, savedIndexingLevel=undefined",
            &mut result,
        );
        assert_eq!(result.repository, "lifejwang11/abtop");
    }

    #[test]
    fn parse_log_line_turn_count() {
        let mut result = LogResult::default();
        parse_log_line(
            "2026-05-07T07:42:16.581Z [INFO] --- Start of group: Sending request to the AI model ---",
            &mut result,
        );
        assert_eq!(result.turn_count, 1);
        assert!(result.model_generating);
        parse_log_line(
            "2026-05-07T07:42:18.868Z [INFO] --- End of group ---",
            &mut result,
        );
        assert!(!result.model_generating);
    }

    #[test]
    fn map_pid_to_log_empty_dir() {
        let dir = tempfile::tempdir().unwrap();
        let result = map_pid_to_log(&[12345], dir.path());
        assert!(result.is_empty());
    }

    #[test]
    fn map_pid_to_log_matches_pid() {
        let dir = tempfile::tempdir().unwrap();
        let log_path = dir.path().join("process-1778139708150-12345.log");
        fs::write(&log_path, "test").unwrap();
        let result = map_pid_to_log(&[12345], dir.path());
        assert_eq!(result.get(&12345), Some(&log_path));
    }

    #[test]
    fn map_pid_to_log_ignores_wrong_pid() {
        let dir = tempfile::tempdir().unwrap();
        let log_path = dir.path().join("process-1778139708150-99999.log");
        fs::write(&log_path, "test").unwrap();
        let result = map_pid_to_log(&[12345], dir.path());
        assert!(result.is_empty());
    }

    #[test]
    fn config_root_is_copilot_home() {
        let c = CopilotCollector::new();
        assert!(c.config_root().ends_with("/.copilot"));
        assert!(c.config_root().starts_with('~'));
    }

    #[test]
    fn tasks_for_status_covers_every_variant() {
        assert_eq!(
            tasks_for_status(&SessionStatus::Thinking),
            vec!["thinking...".to_string()]
        );
        assert_eq!(
            tasks_for_status(&SessionStatus::Executing),
            vec!["executing...".to_string()]
        );
        assert_eq!(
            tasks_for_status(&SessionStatus::Waiting),
            vec!["waiting for input".to_string()]
        );
        assert_eq!(
            tasks_for_status(&SessionStatus::Done),
            vec!["finished".to_string()]
        );
        assert_eq!(
            tasks_for_status(&SessionStatus::RateLimited),
            vec!["rate limited".to_string()]
        );
        assert!(tasks_for_status(&SessionStatus::Unknown).is_empty());
    }

    #[test]
    fn parse_otel_line_accumulates_input_and_output_tokens() {
        let mut result = OtelResult::default();
        let line = r#"{"type":"span","name":"chat gpt-5.5","attributes":{"gen_ai.conversation.id":"s1","gen_ai.response.model":"gpt-5.5","gen_ai.request.reasoning.level":"xhigh","gen_ai.usage.input_tokens":36409,"gen_ai.usage.output_tokens":523}}"#;
        parse_otel_line(line, "s1", &mut result);
        assert_eq!(result.total_input_tokens, 36409);
        assert_eq!(result.total_output_tokens, 523);
        assert_eq!(result.calls_seen, 1);

        let line2 = r#"{"type":"span","name":"chat gpt-5.5","attributes":{"gen_ai.conversation.id":"s1","gen_ai.response.model":"gpt-5.5","gen_ai.usage.input_tokens":100,"gen_ai.usage.output_tokens":10}}"#;
        parse_otel_line(line2, "s1", &mut result);
        assert_eq!(result.total_input_tokens, 36509);
        assert_eq!(result.total_output_tokens, 533);
        assert_eq!(result.calls_seen, 2);
    }

    #[test]
    fn parse_otel_line_accumulates_cache_read_and_cache_creation() {
        let mut result = OtelResult::default();
        let read_line = r#"{"type":"span","name":"chat gpt-5.5","attributes":{"gen_ai.conversation.id":"s1","gen_ai.usage.input_tokens":36409,"gen_ai.usage.output_tokens":523,"gen_ai.usage.cache_read.input_tokens":24064}}"#;
        parse_otel_line(read_line, "s1", &mut result);
        assert_eq!(result.total_cache_read, 24064);
        assert_eq!(result.total_cache_create, 0);

        let write_line = r#"{"type":"span","name":"chat claude-opus-4.8","attributes":{"gen_ai.conversation.id":"s1","gen_ai.usage.input_tokens":66375,"gen_ai.usage.output_tokens":4,"gen_ai.usage.cache_creation.input_tokens":66373}}"#;
        parse_otel_line(write_line, "s1", &mut result);
        assert_eq!(result.total_cache_read, 24064);
        assert_eq!(result.total_cache_create, 66373);
    }

    #[test]
    fn parse_otel_line_accumulates_token_history_as_input_plus_output() {
        let mut result = OtelResult::default();
        let line = r#"{"type":"span","name":"chat gpt-5.5","attributes":{"gen_ai.conversation.id":"s1","gen_ai.usage.input_tokens":100,"gen_ai.usage.output_tokens":25}}"#;
        parse_otel_line(line, "s1", &mut result);
        assert_eq!(result.token_history, vec![125]);
    }

    #[test]
    fn parse_otel_line_extracts_live_model_and_effort() {
        let mut result = OtelResult::default();
        let line = r#"{"type":"span","name":"chat claude-opus-5","attributes":{"gen_ai.conversation.id":"s1","gen_ai.response.model":"claude-opus-5","gen_ai.request.reasoning.level":"high","gen_ai.usage.input_tokens":1,"gen_ai.usage.output_tokens":1}}"#;
        parse_otel_line(line, "s1", &mut result);
        assert_eq!(result.model, "claude-opus-5");
        assert_eq!(result.effort, "high");
    }

    #[test]
    fn parse_otel_line_extracts_context_from_embedded_usage_info_event() {
        let mut result = OtelResult::default();
        let line = r#"{"type":"span","name":"chat gpt-5.5","attributes":{"gen_ai.conversation.id":"s1","gen_ai.usage.input_tokens":1,"gen_ai.usage.output_tokens":1},"events":[{"name":"github.copilot.session.usage_info","attributes":{"github.copilot.token_limit":272000,"github.copilot.current_tokens":41276,"github.copilot.messages_length":3}}]}"#;
        parse_otel_line(line, "s1", &mut result);
        assert_eq!(result.context_tokens, 41276);
        assert_eq!(result.context_window, 272000);
    }

    #[test]
    fn parse_otel_line_ignores_spans_for_other_sessions() {
        let mut result = OtelResult::default();
        let line = r#"{"type":"span","name":"chat gpt-5.5","attributes":{"gen_ai.conversation.id":"other-session","gen_ai.usage.input_tokens":100,"gen_ai.usage.output_tokens":10}}"#;
        parse_otel_line(line, "s1", &mut result);
        assert_eq!(result.calls_seen, 0);
        assert_eq!(result.total_input_tokens, 0);
    }

    #[test]
    fn parse_otel_line_token_history_is_capped_at_ten_thousand() {
        let mut result = OtelResult::default();
        let line = r#"{"type":"span","name":"chat gpt-5.5","attributes":{"gen_ai.conversation.id":"s1","gen_ai.usage.input_tokens":1,"gen_ai.usage.output_tokens":1}}"#;
        for _ in 0..10_005 {
            parse_otel_line(line, "s1", &mut result);
        }
        assert_eq!(result.token_history.len(), 10_000);
        assert_eq!(result.calls_seen, 10_005);
    }

    #[test]
    fn parse_otel_line_ignores_non_chat_spans_and_metrics() {
        let mut result = OtelResult::default();
        parse_otel_line(
            r#"{"type":"span","name":"invoke_agent","attributes":{"gen_ai.conversation.id":"s1"}}"#,
            "s1",
            &mut result,
        );
        parse_otel_line(
            r#"{"type":"metric","name":"gen_ai.client.token.usage","dataPoints":[]}"#,
            "s1",
            &mut result,
        );
        assert_eq!(result.calls_seen, 0);
    }

    #[test]
    fn parse_events_line_counts_turn_start() {
        let mut result = EventsResult::default();
        parse_events_line(
            r#"{"type":"assistant.turn_start","data":{"turnId":"1"},"id":"a","timestamp":"2026-08-16T01:01:44.619Z"}"#,
            &mut result,
        );
        assert_eq!(result.turn_count, 1);
        parse_events_line(
            r#"{"type":"assistant.turn_start","data":{"turnId":"2"},"id":"b","timestamp":"2026-08-16T01:02:44.619Z"}"#,
            &mut result,
        );
        assert_eq!(result.turn_count, 2);
    }

    #[test]
    fn parse_events_line_accumulates_output_tokens() {
        let mut result = EventsResult::default();
        parse_events_line(
            r#"{"type":"assistant.message","data":{"messageId":"m1","model":"claude-opus-4.8","outputTokens":517}}"#,
            &mut result,
        );
        assert_eq!(result.total_output_tokens, 517);
        parse_events_line(
            r#"{"type":"assistant.message","data":{"messageId":"m2","model":"claude-opus-4.8","outputTokens":93}}"#,
            &mut result,
        );
        assert_eq!(result.total_output_tokens, 610);
    }

    #[test]
    fn parse_events_line_accumulates_token_history() {
        let mut result = EventsResult::default();
        parse_events_line(
            r#"{"type":"assistant.message","data":{"messageId":"m1","model":"claude-opus-4.8","outputTokens":517}}"#,
            &mut result,
        );
        parse_events_line(
            r#"{"type":"assistant.message","data":{"messageId":"m2","model":"claude-opus-4.8","outputTokens":93}}"#,
            &mut result,
        );
        assert_eq!(result.token_history, vec![517, 93]);
    }

    #[test]
    fn parse_events_line_token_history_is_capped_at_ten_thousand() {
        let mut result = EventsResult::default();
        for _ in 0..10_005 {
            parse_events_line(
                r#"{"type":"assistant.message","data":{"messageId":"m","model":"x","outputTokens":1}}"#,
                &mut result,
            );
        }
        assert_eq!(result.token_history.len(), 10_000);
    }

    #[test]
    fn parse_events_line_model_prefers_assistant_message_over_session_start() {
        let mut result = EventsResult::default();
        parse_events_line(
            r#"{"type":"session.start","data":{"sessionId":"s1","selectedModel":"gpt-5.5"}}"#,
            &mut result,
        );
        assert_eq!(result.model, "gpt-5.5");
        parse_events_line(
            r#"{"type":"assistant.message","data":{"messageId":"m1","model":"claude-opus-4.8","outputTokens":10}}"#,
            &mut result,
        );
        assert_eq!(result.model, "claude-opus-4.8");
    }

    #[test]
    fn parse_events_line_session_resume_sets_model_only_when_unset() {
        let mut result = EventsResult::default();
        parse_events_line(
            r#"{"type":"assistant.message","data":{"messageId":"m1","model":"claude-opus-4.8","outputTokens":10}}"#,
            &mut result,
        );
        parse_events_line(
            r#"{"type":"session.resume","data":{"sessionId":"s1","selectedModel":"gpt-5.5"}}"#,
            &mut result,
        );
        assert_eq!(
            result.model, "claude-opus-4.8",
            "session.resume must not clobber a model already observed live"
        );
    }

    #[test]
    fn parse_events_line_session_model_change_updates_effort() {
        let mut result = EventsResult::default();
        parse_events_line(
            r#"{"type":"session.model_change","data":{"contextTier":"default","newModel":"claude-opus-5","previousModel":"claude-opus-4.8","previousReasoningEffort":"xhigh","reasoningEffort":"medium"}}"#,
            &mut result,
        );
        assert_eq!(result.effort, "medium");
        assert_eq!(result.model, "claude-opus-5");
    }

    #[test]
    fn parse_events_line_session_start_seeds_initial_effort() {
        let mut result = EventsResult::default();
        parse_events_line(
            r#"{"type":"session.start","data":{"sessionId":"s1","selectedModel":"claude-opus-4.8","reasoningEffort":"xhigh"}}"#,
            &mut result,
        );
        assert_eq!(result.effort, "xhigh");
    }

    #[test]
    fn parse_events_line_session_resume_does_not_clobber_live_effort() {
        let mut result = EventsResult::default();
        parse_events_line(
            r#"{"type":"session.model_change","data":{"newModel":"claude-opus-5","reasoningEffort":"medium"}}"#,
            &mut result,
        );
        parse_events_line(
            r#"{"type":"session.resume","data":{"sessionId":"s1","selectedModel":"gpt-5.5","reasoningEffort":"xhigh"}}"#,
            &mut result,
        );
        assert_eq!(
            result.effort, "medium",
            "session.resume must not clobber an effort already observed live"
        );
    }

    #[test]
    fn parse_events_line_captures_context_from_compaction_start() {
        let mut result = EventsResult::default();
        parse_events_line(
            r#"{"type":"session.compaction_start","data":{"systemTokens":15570,"conversationTokens":136711,"toolDefinitionsTokens":9551,"currentTokens":161832,"tokenLimit":200000,"trigger":"threshold"}}"#,
            &mut result,
        );
        assert_eq!(result.context_tokens, 161832);
        assert_eq!(result.context_window, 200000);
    }

    #[test]
    fn parse_events_line_ignores_malformed_and_unknown_lines() {
        let mut result = EventsResult::default();
        parse_events_line("not json at all", &mut result);
        parse_events_line("", &mut result);
        parse_events_line(r#"{"type":"tool.execution_start","data":{}}"#, &mut result);
        parse_events_line(r#"{"no_type_field":true}"#, &mut result);
        assert_eq!(result.turn_count, 0);
        assert_eq!(result.total_output_tokens, 0);
        assert!(result.model.is_empty());
        assert_eq!(result.context_tokens, 0);
        assert_eq!(result.context_window, 0);
    }

    #[test]
    fn update_events_cache_reads_new_lines_incrementally() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("events.jsonl");
        fs::write(
            &path,
            "{\"type\":\"assistant.turn_start\",\"data\":{\"turnId\":\"1\"}}\n",
        )
        .unwrap();

        let mut cache = EventsCache {
            path: path.clone(),
            offset: 0,
            partial: String::new(),
            result: EventsResult::default(),
        };
        update_events_cache(&mut cache);
        assert_eq!(cache.result.turn_count, 1);

        // Append a second line; only the new bytes should be parsed.
        let mut file = fs::OpenOptions::new().append(true).open(&path).unwrap();
        use std::io::Write;
        writeln!(
            file,
            "{{\"type\":\"assistant.turn_start\",\"data\":{{\"turnId\":\"2\"}}}}"
        )
        .unwrap();
        update_events_cache(&mut cache);
        assert_eq!(cache.result.turn_count, 2);
    }

    #[test]
    fn update_events_cache_missing_file_is_a_noop() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("does-not-exist.jsonl");
        let mut cache = EventsCache {
            path,
            offset: 0,
            partial: String::new(),
            result: EventsResult::default(),
        };
        update_events_cache(&mut cache);
        assert_eq!(cache.result.turn_count, 0);
    }

    #[test]
    fn update_otel_cache_reads_new_lines_incrementally_for_matching_session() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("otel.jsonl");
        fs::write(
            &path,
            "{\"type\":\"span\",\"name\":\"chat gpt-5.5\",\"attributes\":{\"gen_ai.conversation.id\":\"s1\",\"gen_ai.usage.input_tokens\":100,\"gen_ai.usage.output_tokens\":10}}\n",
        )
        .unwrap();

        let mut cache = OtelCache {
            path: path.clone(),
            session_id: "s1".to_string(),
            offset: 0,
            partial: String::new(),
            result: OtelResult::default(),
        };
        update_otel_cache(&mut cache);
        assert_eq!(cache.result.calls_seen, 1);
        assert_eq!(cache.result.total_input_tokens, 100);

        // Append a second line, from a DIFFERENT session — must not accumulate.
        let mut file = fs::OpenOptions::new().append(true).open(&path).unwrap();
        use std::io::Write;
        writeln!(
            file,
            "{{\"type\":\"span\",\"name\":\"chat gpt-5.5\",\"attributes\":{{\"gen_ai.conversation.id\":\"other\",\"gen_ai.usage.input_tokens\":999,\"gen_ai.usage.output_tokens\":1}}}}"
        )
        .unwrap();
        update_otel_cache(&mut cache);
        assert_eq!(
            cache.result.calls_seen, 1,
            "line from another session must be ignored"
        );
        assert_eq!(cache.result.total_input_tokens, 100);
    }

    #[test]
    fn update_otel_cache_missing_file_is_a_noop() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("does-not-exist.jsonl");
        let mut cache = OtelCache {
            path,
            session_id: "s1".to_string(),
            offset: 0,
            partial: String::new(),
            result: OtelResult::default(),
        };
        update_otel_cache(&mut cache);
        assert_eq!(cache.result.calls_seen, 0);
    }

    #[test]
    fn read_effort_from_settings_reads_effort_level_key() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        fs::write(&path, r#"{"model":"gpt-5.5","effortLevel":"xhigh"}"#).unwrap();
        assert_eq!(read_effort_from_settings(&path), "xhigh");
    }

    #[test]
    fn read_effort_from_settings_missing_file_is_empty() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("does-not-exist.json");
        assert_eq!(read_effort_from_settings(&path), "");
    }

    #[test]
    fn resolved_model_prefers_events_over_fallback() {
        assert_eq!(
            resolved_model("claude-opus-4.8", "gpt-5.5"),
            "claude-opus-4.8"
        );
        assert_eq!(resolved_model("", "gpt-5.5"), "gpt-5.5");
    }

    #[test]
    fn resolved_effort_prefers_events_over_fallback() {
        assert_eq!(resolved_effort("xhigh", "medium"), "xhigh");
        assert_eq!(resolved_effort("", "medium"), "medium");
    }

    #[test]
    fn resolved_turn_count_prefers_events_over_log() {
        assert_eq!(resolved_turn_count(7, 0), 7);
        assert_eq!(resolved_turn_count(0, 3), 3);
        assert_eq!(resolved_turn_count(0, 0), 0);
    }

    #[test]
    fn resolved_context_tokens_prefers_events_over_log() {
        assert_eq!(resolved_context_tokens(161832, 0), 161832);
        assert_eq!(resolved_context_tokens(0, 5000), 5000);
    }

    #[test]
    fn resolved_context_window_falls_back_to_model_heuristic() {
        assert_eq!(
            resolved_context_window(200_000, 0, "claude-opus-4.8"),
            200_000
        );
        assert_eq!(
            resolved_context_window(0, 128_000, "claude-opus-4.8"),
            128_000
        );
        assert_eq!(
            resolved_context_window(0, 0, "claude-opus-4.8[1m]"),
            1_000_000
        );
        assert_eq!(resolved_context_window(0, 0, "claude-opus-4.8"), 200_000);
    }
}
