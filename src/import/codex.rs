//! Import adapter for OpenAI Codex CLI rollout transcripts
//! (`~/.codex/sessions/YYYY/MM/DD/rollout-*.jsonl` & `~/.codex/archived_sessions/*.jsonl`).

use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

use serde_json::Value;

use super::{
    ExternalSessionSummary, ImportAdapter, ImportSource, ImportedConversation, ImportedRole,
    ImportedTurn, coalesce_turns, cwd_matches, file_mtime_epoch_and_rfc3339, format_imported_tool,
    normalize_title, parse_rfc3339_epoch, single_line_truncate,
};

#[derive(Debug, Default)]
pub struct CodexAdapter;

impl ImportAdapter for CodexAdapter {
    fn source(&self) -> ImportSource {
        ImportSource::Codex
    }

    fn can_parse(&self, path: &Path) -> bool {
        let Ok(f) = File::open(path) else {
            return false;
        };
        let reader = BufReader::new(f);
        for line in reader.lines().take(20).flatten() {
            let Ok(v) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            if matches!(
                v.get("type").and_then(Value::as_str),
                Some("session_meta" | "response_item" | "turn_context")
            ) && v.get("payload").is_some()
            {
                return true;
            }
        }
        false
    }

    fn discover(&self, home: &Path, current_cwd: &Path) -> Vec<ExternalSessionSummary> {
        let mut files = Vec::new();
        collect_jsonl_recursive(&home.join(".codex/sessions"), 0, &mut files);
        collect_jsonl_recursive(&home.join(".codex/archived_sessions"), 0, &mut files);

        let mut out = Vec::new();
        for path in files {
            if let Some(summary) = scan_codex_summary(&path, current_cwd) {
                out.push(summary);
            }
        }
        out
    }

    fn load(&self, path: &Path) -> Result<ImportedConversation, String> {
        let f = File::open(path).map_err(|e| format!("open codex rollout failed: {e}"))?;
        let reader = BufReader::new(f);

        let mut session_id = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("codex-session")
            .to_owned();
        let mut original_cwd: Option<String> = None;
        let mut model: Option<String> = None;

        let mut items: Vec<(String, Value)> = Vec::new();
        for line in reader.lines().map_while(Result::ok) {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            let Ok(v) = serde_json::from_str::<Value>(trimmed) else {
                continue;
            };
            let ts = v
                .get("timestamp")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_owned();
            let Some(payload) = v.get("payload") else {
                continue;
            };
            match v.get("type").and_then(Value::as_str) {
                Some("session_meta") => {
                    if let Some(sid) = payload
                        .get("id")
                        .or_else(|| payload.get("session_id"))
                        .and_then(Value::as_str)
                        && !sid.is_empty()
                    {
                        session_id = sid.to_owned();
                    }
                    if original_cwd.is_none()
                        && let Some(c) = payload.get("cwd").and_then(Value::as_str)
                        && !c.is_empty()
                    {
                        original_cwd = Some(c.to_owned());
                    }
                }
                Some("turn_context") => {
                    if let Some(m) = payload.get("model").and_then(Value::as_str)
                        && !m.is_empty()
                    {
                        model = Some(m.to_owned());
                    }
                    if original_cwd.is_none()
                        && let Some(c) = payload.get("cwd").and_then(Value::as_str)
                        && !c.is_empty()
                    {
                        original_cwd = Some(c.to_owned());
                    }
                }
                Some("response_item") => {
                    items.push((ts, payload.clone()));
                }
                _ => {}
            }
        }

        // Pre-index tool outputs by `call_id` so function_call / custom_tool_call get outputs inline.
        let mut call_outputs: HashMap<String, String> = HashMap::new();
        for (_, pl) in &items {
            if matches!(
                pl.get("type").and_then(Value::as_str),
                Some("function_call_output" | "custom_tool_call_output")
            ) && let Some(cid) = pl.get("call_id").and_then(Value::as_str)
            {
                let out = match pl.get("output") {
                    Some(Value::String(s)) => s.clone(),
                    Some(other) => other.to_string(),
                    None => String::new(),
                };
                call_outputs.insert(cid.to_owned(), out);
            }
        }

        let mut raw_turns: Vec<ImportedTurn> = Vec::new();
        let mut first_user_prompt: Option<String> = None;

        for (ts, pl) in &items {
            match pl.get("type").and_then(Value::as_str) {
                Some("message") => {
                    let role_str = pl.get("role").and_then(Value::as_str).unwrap_or("");
                    let text = extract_codex_content_text(pl.get("content"));
                    let trimmed = text.trim();
                    if trimmed.is_empty() {
                        continue;
                    }
                    match role_str {
                        "user" => {
                            if is_codex_injected_context(trimmed) {
                                continue;
                            }
                            if first_user_prompt.is_none() {
                                first_user_prompt = Some(trimmed.to_owned());
                            }
                            raw_turns.push(ImportedTurn {
                                role: ImportedRole::User,
                                ts: ts.clone(),
                                text: trimmed.to_owned(),
                                reasoning: None,
                            });
                        }
                        "assistant" => {
                            raw_turns.push(ImportedTurn {
                                role: ImportedRole::Assistant,
                                ts: ts.clone(),
                                text: trimmed.to_owned(),
                                reasoning: None,
                            });
                        }
                        _ => {}
                    }
                }
                Some("reasoning") => {
                    let summary = extract_codex_reasoning_summary(pl.get("summary"));
                    if !summary.is_empty() {
                        raw_turns.push(ImportedTurn {
                            role: ImportedRole::Assistant,
                            ts: ts.clone(),
                            text: String::new(),
                            reasoning: Some(summary),
                        });
                    }
                }
                Some("function_call" | "custom_tool_call") => {
                    let name = pl.get("name").and_then(Value::as_str).unwrap_or("tool");
                    let args_raw = pl
                        .get("arguments")
                        .or_else(|| pl.get("input"))
                        .map(|v| match v {
                            Value::String(s) => s.clone(),
                            other => other.to_string(),
                        })
                        .unwrap_or_default();
                    let args_preview = summarize_codex_args(&args_raw);
                    let out = pl
                        .get("call_id")
                        .and_then(Value::as_str)
                        .and_then(|cid| call_outputs.get(cid))
                        .map(String::as_str);
                    raw_turns.push(ImportedTurn {
                        role: ImportedRole::Assistant,
                        ts: ts.clone(),
                        text: format_imported_tool(name, &args_preview, out, false),
                        reasoning: None,
                    });
                }
                _ => {}
            }
        }

        let turns = coalesce_turns(raw_turns);
        if turns.is_empty() {
            return Err("no user/assistant messages found in Codex session".into());
        }
        let title = first_user_prompt
            .as_deref()
            .map(|t| normalize_title(t, &session_id))
            .unwrap_or_else(|| session_id.clone());

        Ok(ImportedConversation {
            source: ImportSource::Codex,
            original_id: session_id,
            title,
            original_cwd,
            model,
            turns,
        })
    }
}

fn collect_jsonl_recursive(dir: &Path, depth: usize, out: &mut Vec<PathBuf>) {
    if depth > 5 || !dir.is_dir() {
        return;
    }
    let Ok(rd) = fs::read_dir(dir) else {
        return;
    };
    for entry in rd.flatten() {
        let p = entry.path();
        if p.is_dir() {
            collect_jsonl_recursive(&p, depth + 1, out);
        } else if p.is_file() && p.extension().and_then(|s| s.to_str()) == Some("jsonl") {
            out.push(p);
        }
    }
}

fn scan_codex_summary(path: &Path, current_cwd: &Path) -> Option<ExternalSessionSummary> {
    let f = File::open(path).ok()?;
    let reader = BufReader::new(f);
    let mut id = path.file_stem()?.to_str()?.to_owned();
    let mut cwd: Option<String> = None;
    let mut first_prompt: Option<String> = None;
    let mut last_ts: Option<String> = None;
    let mut turns = 0usize;

    for line in reader.lines().map_while(Result::ok) {
        if line.trim().is_empty() {
            continue;
        }
        let Ok(v) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        if let Some(ts) = v.get("timestamp").and_then(Value::as_str) {
            last_ts = Some(ts.to_owned());
        }
        let Some(pl) = v.get("payload") else {
            continue;
        };
        match v.get("type").and_then(Value::as_str) {
            Some("session_meta") => {
                if let Some(sid) = pl
                    .get("id")
                    .or_else(|| pl.get("session_id"))
                    .and_then(Value::as_str)
                    && !sid.is_empty()
                {
                    id = sid.to_owned();
                }
                if cwd.is_none()
                    && let Some(c) = pl.get("cwd").and_then(Value::as_str)
                    && !c.is_empty()
                {
                    cwd = Some(c.to_owned());
                }
            }
            Some("turn_context") => {
                if cwd.is_none()
                    && let Some(c) = pl.get("cwd").and_then(Value::as_str)
                    && !c.is_empty()
                {
                    cwd = Some(c.to_owned());
                }
            }
            Some("response_item") if pl.get("type").and_then(Value::as_str) == Some("message") => {
                match pl.get("role").and_then(Value::as_str) {
                    Some("user") => {
                        let text = extract_codex_content_text(pl.get("content"));
                        let trimmed = text.trim();
                        if !trimmed.is_empty() && !is_codex_injected_context(trimmed) {
                            turns += 1;
                            if first_prompt.is_none() {
                                first_prompt = Some(trimmed.to_owned());
                            }
                        }
                    }
                    Some("assistant") => {
                        let text = extract_codex_content_text(pl.get("content"));
                        if !text.trim().is_empty() {
                            turns += 1;
                        }
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }

    if turns == 0 {
        return None;
    }

    let (mtime_epoch, mtime_rfc) = file_mtime_epoch_and_rfc3339(path);
    let (updated_epoch, updated_at) = match last_ts {
        Some(ts) => {
            let ep = parse_rfc3339_epoch(&ts).unwrap_or(mtime_epoch);
            (ep, ts)
        }
        None => (mtime_epoch, mtime_rfc),
    };
    let title = normalize_title(first_prompt.as_deref().unwrap_or(&id), &id);
    let matches = cwd_matches(cwd.as_deref(), current_cwd);
    Some(ExternalSessionSummary {
        source: ImportSource::Codex,
        id,
        title,
        cwd,
        updated_at,
        updated_epoch,
        turn_count: turns,
        path: path.to_path_buf(),
        matches_cwd: matches,
    })
}

fn extract_codex_content_text(content: Option<&Value>) -> String {
    match content {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(blocks)) => blocks
            .iter()
            .filter_map(|b| b.get("text").and_then(Value::as_str))
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

fn extract_codex_reasoning_summary(summary: Option<&Value>) -> String {
    match summary {
        Some(Value::String(s)) => s.trim().to_owned(),
        Some(Value::Array(items)) => items
            .iter()
            .filter_map(|item| {
                item.get("text")
                    .and_then(Value::as_str)
                    .or_else(|| item.as_str())
            })
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

fn summarize_codex_args(raw: &str) -> String {
    if let Ok(v) = serde_json::from_str::<Value>(raw) {
        for key in ["cmd", "command", "path", "file_path", "query", "input"] {
            if let Some(s) = v.get(key).and_then(Value::as_str) {
                return single_line_truncate(s, 140);
            }
        }
    }
    single_line_truncate(raw, 140)
}

fn is_codex_injected_context(trimmed: &str) -> bool {
    trimmed.starts_with("<environment_context>")
        || trimmed.starts_with("<permissions instructions>")
        || trimmed.starts_with("# AGENTS.md instructions for ")
        || trimmed.starts_with("<INSTRUCTIONS>")
}
