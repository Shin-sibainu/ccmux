//! Claude Code session monitoring via JSONL transcript files.
//!
//! Watches ~/.claude/projects/<project>/*.jsonl for real-time events:
//! tool uses, sub-agent spawns (isSidechain), thinking state.

use std::collections::HashMap;
use std::fs::File;
use std::io::{BufRead, BufReader, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

/// A single todo item from TodoWrite tool.
#[derive(Debug, Clone)]
pub struct TodoItem {
    pub content: String,
    pub status: String, // "pending", "in_progress", "completed"
}

/// Current state of a Claude session inferred from JSONL events.
#[derive(Debug, Clone, Default)]
pub struct ClaudeState {
    /// Last tool used (Bash, Read, Edit, Task, etc.)
    pub current_tool: Option<String>,
    /// Active sub-agent count (isSidechain sessions currently running)
    pub subagent_count: usize,
    /// Names of active sub-agent types (e.g. "evaluator", "generator")
    pub subagent_types: Vec<String>,
    /// True if Claude is currently thinking/processing
    pub is_working: bool,
    /// Total tool uses in this session
    pub tool_use_count: usize,
    /// Current model (e.g. "claude-opus-4-6")
    pub model: Option<String>,
    /// Cumulative token usage
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_creation_tokens: u64,
    /// Current todo list (from TodoWrite)
    pub todos: Vec<TodoItem>,
    /// Current context window size (last message's total input tokens)
    pub context_tokens: u64,
    /// Git branch of the last assistant message
    pub git_branch: Option<String>,
}

impl ClaudeState {
    /// Total tokens used.
    pub fn total_tokens(&self) -> u64 {
        self.input_tokens + self.output_tokens + self.cache_read_tokens + self.cache_creation_tokens
    }

    /// Cache hit rate (0.0..1.0).
    #[allow(dead_code)]
    pub fn cache_hit_rate(&self) -> f64 {
        let total_input = self.input_tokens + self.cache_read_tokens + self.cache_creation_tokens;
        if total_input == 0 {
            0.0
        } else {
            self.cache_read_tokens as f64 / total_input as f64
        }
    }

    /// Todo completion stats: (completed, total).
    pub fn todo_progress(&self) -> (usize, usize) {
        let completed = self.todos.iter().filter(|t| t.status == "completed").count();
        (completed, self.todos.len())
    }

    /// Context window limit for the current model (in tokens).
    ///
    /// Claude Code writes the plain model id (e.g. `claude-opus-4-6`)
    /// into the JSONL, **without** the `[1m]` suffix even when the
    /// session is running the 1M variant. Opus 4.6 ships with a 1M
    /// context by default for Pro / Max users, so it's treated as 1M
    /// here. Sonnet and Haiku still default to 200K; the explicit
    /// `[1m]` / `-1m` suffix path catches any future model that does
    /// spell it out.
    pub fn context_limit(&self) -> u64 {
        match self.model.as_deref() {
            Some(m) if m.contains("[1m]") || m.contains("-1m") => 1_000_000,
            // Opus 4.6+: 1M context is default.
            Some(m) if m.contains("opus-4-6") => 1_000_000,
            Some(m) if m.contains("haiku") => 200_000,
            Some(m) if m.contains("sonnet") => 200_000,
            Some(m) if m.contains("opus") => 200_000,
            _ => 200_000,
        }
    }

    /// Context usage ratio (0.0..1.0).
    pub fn context_usage(&self) -> f64 {
        let limit = self.context_limit();
        if limit == 0 {
            0.0
        } else {
            (self.context_tokens as f64 / limit as f64).min(1.0)
        }
    }

    /// Short model name for display (e.g. "opus-4-6" → "opus").
    pub fn short_model(&self) -> Option<&str> {
        let full = self.model.as_deref()?;
        if full.contains("opus") {
            Some("opus")
        } else if full.contains("sonnet") {
            Some("sonnet")
        } else if full.contains("haiku") {
            Some("haiku")
        } else {
            Some(full)
        }
    }
}

/// Per-pane monitor state.
struct PaneMonitor {
    jsonl_path: Option<PathBuf>,
    file_position: u64,
    last_mtime: Option<SystemTime>,
    /// Last time we did a metadata check (for throttling).
    last_check: Instant,
    /// Last time we ran a full directory scan for new JSONL files.
    last_rescan: Instant,
    state: ClaudeState,
    /// Active sub-agents: tool_use_id → subagent_type (or "general-purpose")
    active_task_ids: std::collections::HashMap<String, String>,
    /// Request IDs already counted for token usage (avoid double-counting).
    counted_request_ids: std::collections::HashSet<String>,
}

impl PaneMonitor {
    fn new() -> Self {
        Self {
            jsonl_path: None,
            file_position: 0,
            last_mtime: None,
            last_check: Instant::now() - Duration::from_secs(10),
            last_rescan: Instant::now() - Duration::from_secs(60),
            state: ClaudeState::default(),
            active_task_ids: std::collections::HashMap::new(),
            counted_request_ids: std::collections::HashSet::new(),
        }
    }
}

/// Shared state across all panes being monitored.
#[derive(Clone, Default)]
pub struct ClaudeMonitor {
    inner: Arc<Mutex<HashMap<usize, PaneMonitor>>>,
}

/// Throttle interval for file metadata checks (to avoid per-frame syscalls).
const CHECK_INTERVAL: Duration = Duration::from_millis(500);

/// Maximum cached request IDs for token dedup. JSONL is read sequentially
/// and we never re-read old lines, so clearing the set is safe — the only
/// cost is a potential double-count of the very last request if it spans
/// two read batches (negligible).
const MAX_REQUEST_ID_CACHE: usize = 10_000;

impl ClaudeMonitor {
    pub fn new() -> Self {
        Self::default()
    }

    /// Get the current state for a pane.
    pub fn state(&self, pane_id: usize) -> ClaudeState {
        self.inner
            .lock()
            .ok()
            .and_then(|m| m.get(&pane_id).map(|p| p.state.clone()))
            .unwrap_or_default()
    }

    /// Update monitoring for a pane with its current cwd.
    /// Throttled to CHECK_INTERVAL to avoid per-frame syscalls.
    pub fn update(&self, pane_id: usize, cwd: &Path) {
        // Phase 1: check if we should run at all (short lock)
        let (path_to_read, read_from) = {
            let mut map = match self.inner.lock() {
                Ok(m) => m,
                Err(_) => return,
            };

            let monitor = map.entry(pane_id).or_insert_with(PaneMonitor::new);

            // Throttle: skip if checked recently
            if monitor.last_check.elapsed() < CHECK_INTERVAL {
                return;
            }
            monitor.last_check = Instant::now();

            // Locate or re-locate the JSONL file.
            // Full directory scan every 5s or when our path disappears,
            // to detect new sessions (new JSONL files).
            let path_missing = monitor
                .jsonl_path
                .as_ref()
                .map_or(true, |p| !p.exists());
            let stale_scan = monitor.last_rescan.elapsed() > Duration::from_secs(5);
            if path_missing || stale_scan {
                monitor.last_rescan = Instant::now();
                let expected_path = find_jsonl_path(cwd);
                if monitor.jsonl_path != expected_path {
                    monitor.jsonl_path = expected_path;
                    monitor.file_position = 0;
                    monitor.state = ClaudeState::default();
                    monitor.active_task_ids.clear();
                    monitor.counted_request_ids.clear();
                }
            }

            let path = match &monitor.jsonl_path {
                Some(p) => p.clone(),
                None => return,
            };

            // Check file metadata — skip if unchanged, detect truncation/rotation
            let meta = match std::fs::metadata(&path) {
                Ok(m) => m,
                Err(_) => return,
            };
            let mtime = meta.modified().ok();
            if mtime == monitor.last_mtime {
                return;
            }
            monitor.last_mtime = mtime;

            // File truncation/rotation detection: if file shrank, reset state
            if meta.len() < monitor.file_position {
                monitor.file_position = 0;
                monitor.state = ClaudeState::default();
                monitor.active_task_ids.clear();
                monitor.counted_request_ids.clear();
            }

            (path, monitor.file_position)
        };

        // Phase 2: read file without holding the lock
        let file = match File::open(&path_to_read) {
            Ok(f) => f,
            Err(_) => return,
        };
        let mut reader = BufReader::new(file);
        if reader.seek(SeekFrom::Start(read_from)).is_err() {
            return;
        }

        let mut new_lines = Vec::new();
        let mut new_position = read_from;
        let mut buf = String::new();
        loop {
            buf.clear();
            let bytes = match reader.read_line(&mut buf) {
                Ok(0) => break,
                Ok(n) => n,
                Err(_) => break,
            };
            if !buf.ends_with('\n') {
                break;
            }
            new_position += bytes as u64;
            new_lines.push(buf.clone());
        }

        // Phase 3: apply parsed events (short lock)
        if new_lines.is_empty() {
            return;
        }
        if let Ok(mut map) = self.inner.lock() {
            if let Some(monitor) = map.get_mut(&pane_id) {
                monitor.file_position = new_position;
                for line in &new_lines {
                    process_event(monitor, line);
                }
            }
        }
    }

    pub fn remove(&self, pane_id: usize) {
        if let Ok(mut map) = self.inner.lock() {
            map.remove(&pane_id);
        }
    }
}

/// Process a single JSONL line and update the monitor state.
fn process_event(monitor: &mut PaneMonitor, line: &str) {
    let json: serde_json::Value = match serde_json::from_str(line.trim()) {
        Ok(v) => v,
        Err(_) => return,
    };

    let event_type = json.get("type").and_then(|v| v.as_str()).unwrap_or("");

    match event_type {
        "assistant" => {
            let message = json.get("message");

            let stop_reason = message
                .and_then(|m| m.get("stop_reason"))
                .and_then(|v| v.as_str());

            // Any non-tool_use stop_reason means Claude finished this turn.
            // tool_use or null means still working.
            match stop_reason {
                Some("tool_use") | None => {
                    monitor.state.is_working = true;
                }
                Some(_) => {
                    monitor.state.is_working = false;
                    monitor.state.current_tool = None;
                }
            }

            // Model name
            if let Some(model) = message.and_then(|m| m.get("model")).and_then(|v| v.as_str()) {
                monitor.state.model = Some(model.to_string());
            }

            // Token usage — count once per requestId (avoid double-counting
            // when the same request is split across multiple JSONL lines)
            let request_id = json
                .get("requestId")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());

            // Only count tokens when requestId is present (to dedupe).
            // Missing requestId means we can't safely deduplicate, so skip counting.
            let should_count = match &request_id {
                Some(id) => {
                    if monitor.counted_request_ids.len() >= MAX_REQUEST_ID_CACHE {
                        monitor.counted_request_ids.clear();
                    }
                    monitor.counted_request_ids.insert(id.clone())
                }
                None => false,
            };

            if should_count {
                if let Some(usage) = message.and_then(|m| m.get("usage")) {
                    let input = usage.get("input_tokens").and_then(|v| v.as_u64()).unwrap_or(0);
                    let output = usage.get("output_tokens").and_then(|v| v.as_u64()).unwrap_or(0);
                    let cache_read = usage
                        .get("cache_read_input_tokens")
                        .and_then(|v| v.as_u64())
                        .unwrap_or(0);
                    let cache_create = usage
                        .get("cache_creation_input_tokens")
                        .and_then(|v| v.as_u64())
                        .unwrap_or(0);

                    monitor.state.input_tokens += input;
                    monitor.state.output_tokens += output;
                    monitor.state.cache_read_tokens += cache_read;
                    monitor.state.cache_creation_tokens += cache_create;

                    // Current context = input + cache (this is how much is sent each turn)
                    monitor.state.context_tokens = input + cache_read + cache_create;
                }
            }

            // Git branch (stored on every event; update if present)
            if let Some(branch) = json.get("gitBranch").and_then(|v| v.as_str()) {
                if !branch.is_empty() && branch != "HEAD" {
                    monitor.state.git_branch = Some(branch.to_string());
                }
            }

            let content = message
                .and_then(|m| m.get("content"))
                .and_then(|c| c.as_array());

            if let Some(content) = content {
                for block in content {
                    let block_type = block.get("type").and_then(|v| v.as_str()).unwrap_or("");
                    if block_type == "tool_use" {
                        if let Some(name) = block.get("name").and_then(|v| v.as_str()) {
                            monitor.state.current_tool = Some(name.to_string());
                            monitor.state.tool_use_count += 1;
                            monitor.state.is_working = true;

                            // Sub-agent tools (real name in JSONL is "Agent", "Task" was old name)
                            if name == "Agent" || name == "Task" {
                                if let Some(task_id) =
                                    block.get("id").and_then(|v| v.as_str())
                                {
                                    let subagent_type = block
                                        .get("input")
                                        .and_then(|i| i.get("subagent_type"))
                                        .and_then(|v| v.as_str())
                                        .unwrap_or("general-purpose")
                                        .to_string();
                                    monitor
                                        .active_task_ids
                                        .insert(task_id.to_string(), subagent_type);
                                    monitor.state.subagent_count =
                                        monitor.active_task_ids.len();
                                    monitor.state.subagent_types =
                                        monitor.active_task_ids.values().cloned().collect();
                                }
                            }

                            // TodoWrite — parse the todos
                            if name == "TodoWrite" {
                                if let Some(todos_arr) = block
                                    .get("input")
                                    .and_then(|v| v.get("todos"))
                                    .and_then(|v| v.as_array())
                                {
                                    monitor.state.todos = todos_arr
                                        .iter()
                                        .filter_map(|t| {
                                            Some(TodoItem {
                                                content: t
                                                    .get("content")?
                                                    .as_str()?
                                                    .to_string(),
                                                status: t
                                                    .get("status")?
                                                    .as_str()?
                                                    .to_string(),
                                            })
                                        })
                                        .collect();
                                }
                            }
                        }
                    }
                }
            }
        }
        "user" => {
            // User message indicates either a new prompt OR a tool_result
            let content = json
                .get("message")
                .and_then(|m| m.get("content"))
                .and_then(|c| c.as_array());

            let mut has_tool_result = false;
            if let Some(content) = content {
                for block in content {
                    if block.get("type").and_then(|v| v.as_str()) == Some("tool_result") {
                        has_tool_result = true;
                        // If this tool_result is for a Task, decrement the active set
                        if let Some(tool_use_id) =
                            block.get("tool_use_id").and_then(|v| v.as_str())
                        {
                            if monitor.active_task_ids.remove(tool_use_id).is_some() {
                                monitor.state.subagent_count = monitor.active_task_ids.len();
                                monitor.state.subagent_types =
                                    monitor.active_task_ids.values().cloned().collect();
                            }
                        }
                    }
                }
            }

            if !has_tool_result {
                // New user prompt — reset working state
                monitor.state.is_working = false;
                monitor.state.current_tool = None;
            }
        }
        _ => {}
    }
}

/// Convert a cwd path to Claude's project directory name and find the most recent JSONL.
fn find_jsonl_path(cwd: &Path) -> Option<PathBuf> {
    let home = dirs::home_dir()?;
    let projects_dir = home.join(".claude").join("projects");

    if !projects_dir.exists() {
        return None;
    }

    let encoded = encode_cwd_to_project_name(cwd);
    let project_dir = projects_dir.join(&encoded);

    if !project_dir.exists() {
        return None;
    }

    let mut latest: Option<(PathBuf, SystemTime)> = None;
    let entries = std::fs::read_dir(&project_dir).ok()?;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().map_or(false, |e| e == "jsonl") {
            if let Ok(meta) = entry.metadata() {
                if let Ok(mtime) = meta.modified() {
                    match &latest {
                        Some((_, old_mtime)) if *old_mtime >= mtime => {}
                        _ => latest = Some((path, mtime)),
                    }
                }
            }
        }
    }
    latest.map(|(p, _)| p)
}

/// Encode a path to Claude's project name format.
/// Claude Code replaces any character that is not ASCII alphanumeric or `.` with `-`.
/// E.g.,  `C:\Users\じゅぶ\dev` → `C--Users-----dev`
fn encode_cwd_to_project_name(cwd: &Path) -> String {
    let s = cwd.to_string_lossy();
    let mut result = String::with_capacity(s.len());
    for ch in s.chars() {
        if ch.is_ascii_alphanumeric() || ch == '.' {
            result.push(ch);
        } else {
            result.push('-');
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_encode_cwd() {
        let path = PathBuf::from(r"C:\Users\foo\bar");
        let encoded = encode_cwd_to_project_name(&path);
        assert_eq!(encoded, "C--Users-foo-bar");
    }

    #[test]
    fn test_encode_cwd_japanese() {
        // Claude encodes non-ASCII chars as dashes too
        let path = PathBuf::from("C:\\Users\\じゅぶ\\dev\\ccmux");
        let encoded = encode_cwd_to_project_name(&path);
        // C : \ U s e r s \ じ ゅ ぶ \ d e v \ c c m u x
        // C - - Users    - - - - dev - ccmux
        assert_eq!(encoded, "C--Users-----dev-ccmux");
    }

    #[test]
    fn test_process_tool_use() {
        let mut monitor = PaneMonitor::new();
        let line = r#"{"type":"assistant","message":{"content":[{"type":"tool_use","name":"Bash","id":"toolu_001","input":{}}],"stop_reason":"tool_use"}}"#;
        process_event(&mut monitor, line);
        assert_eq!(monitor.state.current_tool.as_deref(), Some("Bash"));
        assert!(monitor.state.is_working);
    }

    #[test]
    fn test_process_agent_spawn_and_complete() {
        let mut monitor = PaneMonitor::new();

        // Agent (sub-agent) spawn
        let spawn = r#"{"type":"assistant","message":{"content":[{"type":"tool_use","name":"Agent","id":"toolu_agent1","input":{}}],"stop_reason":"tool_use"}}"#;
        process_event(&mut monitor, spawn);
        assert_eq!(monitor.state.subagent_count, 1);

        // Sub-agent complete via tool_result
        let complete = r#"{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"toolu_agent1","content":"done"}]}}"#;
        process_event(&mut monitor, complete);
        assert_eq!(monitor.state.subagent_count, 0);
    }

    #[test]
    fn test_token_usage_no_double_count() {
        let mut monitor = PaneMonitor::new();
        // Same requestId appears 3 times (typical Claude JSONL pattern)
        let line = r#"{"type":"assistant","requestId":"req_123","message":{"model":"claude-opus-4-6","content":[{"type":"tool_use","name":"Bash","id":"t1","input":{}}],"usage":{"input_tokens":100,"output_tokens":50,"cache_read_input_tokens":1000}}}"#;
        process_event(&mut monitor, line);
        process_event(&mut monitor, line);
        process_event(&mut monitor, line);

        // Should be counted only once
        assert_eq!(monitor.state.input_tokens, 100);
        assert_eq!(monitor.state.output_tokens, 50);
        assert_eq!(monitor.state.cache_read_tokens, 1000);
    }

    #[test]
    fn test_todo_parsing() {
        let mut monitor = PaneMonitor::new();
        let line = r#"{"type":"assistant","message":{"content":[{"type":"tool_use","name":"TodoWrite","id":"t1","input":{"todos":[{"content":"Task A","status":"completed","activeForm":"Doing A"},{"content":"Task B","status":"in_progress","activeForm":"Doing B"},{"content":"Task C","status":"pending","activeForm":"Doing C"}]}}]}}"#;
        process_event(&mut monitor, line);
        assert_eq!(monitor.state.todos.len(), 3);
        assert_eq!(monitor.state.todo_progress(), (1, 3));
    }

    #[test]
    fn test_stop_reason_end_turn_clears_working() {
        let mut monitor = PaneMonitor::new();
        monitor.state.is_working = true;
        monitor.state.current_tool = Some("Bash".to_string());

        let line = r#"{"type":"assistant","message":{"content":[{"type":"text","text":"done"}],"stop_reason":"end_turn"}}"#;
        process_event(&mut monitor, line);
        assert!(!monitor.state.is_working);
        assert!(monitor.state.current_tool.is_none());
    }

    #[test]
    fn test_context_limit_opus_4_6_is_1m() {
        // Claude Code logs the plain model id without the [1m] suffix
        // even though Opus 4.6 ships with 1M context by default.
        let mut state = ClaudeState::default();
        state.model = Some("claude-opus-4-6".to_string());
        assert_eq!(state.context_limit(), 1_000_000);

        // Explicit 1m variant suffix still works.
        state.model = Some("claude-opus-4-6[1m]".to_string());
        assert_eq!(state.context_limit(), 1_000_000);

        // Older Opus remains 200K.
        state.model = Some("claude-opus-4-5".to_string());
        assert_eq!(state.context_limit(), 200_000);

        // Sonnet / Haiku default to 200K.
        state.model = Some("claude-sonnet-4-6".to_string());
        assert_eq!(state.context_limit(), 200_000);
        state.model = Some("claude-haiku-4-5".to_string());
        assert_eq!(state.context_limit(), 200_000);
    }
}
