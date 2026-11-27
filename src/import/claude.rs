//! Import adapter for Claude Code JSONL transcripts (`~/.claude/projects/<proj>/<uuid>.jsonl`).

use std::collections::{HashMap, HashSet};
use std::fs::{self, File};
use std::io::{BufRead, BufReader};
use std::path::Path;

use serde_json::Value;

use super::{
    ExternalSessionSummary, ImportAdapter, ImportSource, ImportedConversation, ImportedRole,
    ImportedTurn, coalesce_turns, cwd_matches, file_mtime_epoch_and_rfc3339, format_imported_tool,
    normalize_title, parse_rfc3339_epoch, single_line_truncate,
};

#[derive(Debug, Default)]
pub struct ClaudeCodeAdapter;

impl ImportAdapter for ClaudeCodeAdapter {
    fn source(&self) -> ImportSource {
        ImportSource::Claude
    }

    fn can_parse(&self, path: &Path) -> bool {
        let Ok(f) = File::open(path) else {
            return false;
        };
        let reader = BufReader::new(f);
        for line in reader.lines().take(25).flatten() {
            let Ok(v) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            if v.get("sessionId").is_some()
                && (v.get("parentUuid").is_some()
                    || matches!(
                        v.get("type").and_then(Value::as_str),
                        Some(
                            "permission-mode"
                                | "file-history-snapshot"
                                | "last-prompt"
                                | "ai-title"
                                | "cost-state"
                        )
                    ))
            {
                return true;
            }
        }
        false
    }

    fn discover(&self, home: &Path, current_cwd: &Path) -> Vec<ExternalSessionSummary> {
        let projects_dir = home.join(".claude/projects");
        let Ok(proj_entries) = fs::read_dir(&projects_dir) else {
            return Vec::new();
        };
        let mut out = Vec::new();
        for proj in proj_entries.flatten() {
            let pdir = proj.path();
            if !pdir.is_dir() {
                continue;
            }
            let Ok(files) = fs::read_dir(&pdir) else {
                continue;
            };
            for f in files.flatten() {
                let path = f.path();
                if path.extension().and_then(|s| s.to_str()) != Some("jsonl") || !path.is_file() {
                    continue;
                }
                if let Some(summary) = scan_claude_summary(&path, current_cwd) {
                    out.push(summary);
                }
            }
        }
        out
    }

    fn load(&self, path: &Path) -> Result<ImportedConversation, String> {
        let f = File::open(path).map_err(|e| format!("open claude transcript failed: {e}"))?;
        let reader = BufReader::new(f);

        let mut session_id = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("claude-session")
            .to_owned();
        let mut title_hint: Option<String> = None;
        let mut original_cwd: Option<String> = None;
        let mut model: Option<String> = None;

        // Collect all non-sidechain user/assistant records in file order.
        let mut records: Vec<Value> = Vec::new();
        for line in reader.lines().map_while(Result::ok) {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            let Ok(v) = serde_json::from_str::<Value>(trimmed) else {
                continue;
            };
            if let Some(sid) = v.get("sessionId").and_then(Value::as_str)
                && !sid.is_empty()
            {
                session_id = sid.to_owned();
            }
            if original_cwd.is_none()
                && let Some(c) = v.get("cwd").and_then(Value::as_str)
                && !c.is_empty()
            {
                original_cwd = Some(c.to_owned());
            }
            match v.get("type").and_then(Value::as_str) {
                Some("ai-title") => {
                    if let Some(t) = v.get("aiTitle").and_then(Value::as_str)
                        && !t.trim().is_empty()
                    {
                        title_hint = Some(t.trim().to_owned());
                    }
                }
                Some("user" | "assistant") => {
                    if v.get("isSidechain").and_then(Value::as_bool) == Some(true) {
                        continue;
                    }
                    if model.is_none()
                        && let Some(m) = v
                            .get("message")
                            .and_then(|m| m.get("model"))
                            .and_then(Value::as_str)
                    {
                        model = Some(m.to_owned());
                    }
                    records.push(v);
                }
                _ => {}
            }
        }

        // Reconstruct active branch lineage from the latest record via `uuid` -> `parentUuid`
        // (falls back to chronological order when records lack `uuid`).
        let lineage = select_claude_lineage(records);

        // Pre-index tool_result outputs by `tool_use_id` so every tool_use gets its result inline.
        let mut tool_results: HashMap<String, (String, bool)> = HashMap::new();
        for rec in &lineage {
            if rec.get("type").and_then(Value::as_str) != Some("user") {
                continue;
            }
            if let Some(blocks) = rec
                .get("message")
                .and_then(|m| m.get("content"))
                .and_then(Value::as_array)
            {
                for b in blocks {
                    if b.get("type").and_then(Value::as_str) == Some("tool_result")
                        && let Some(tid) = b.get("tool_use_id").and_then(Value::as_str)
                    {
                        let text = extract_claude_tool_result_text(b.get("content"));
                        let is_err = b.get("is_error").and_then(Value::as_bool).unwrap_or(false);
                        tool_results.insert(tid.to_owned(), (text, is_err));
                    }
                }
            }
        }

        let mut raw_turns: Vec<ImportedTurn> = Vec::new();
        let mut first_user_prompt: Option<String> = None;

        for rec in &lineage {
            let rec_type = rec.get("type").and_then(Value::as_str).unwrap_or("");
            let ts = rec
                .get("timestamp")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_owned();
            let Some(msg) = rec.get("message") else {
                continue;
            };

            if rec_type == "user" {
                if rec.get("isMeta").and_then(Value::as_bool) == Some(true) {
                    continue;
                }
                let text = extract_claude_user_text(msg.get("content"));
                let cleaned = text.trim();
                if cleaned.is_empty() || is_claude_internal_wrapper(cleaned) {
                    continue;
                }
                if first_user_prompt.is_none() {
                    first_user_prompt = Some(cleaned.to_owned());
                }
                raw_turns.push(ImportedTurn {
                    role: ImportedRole::User,
                    ts,
                    text: cleaned.to_owned(),
                    reasoning: None,
                });
            } else if rec_type == "assistant" {
                let mut text_parts: Vec<String> = Vec::new();
                let mut reasoning_parts: Vec<String> = Vec::new();

                match msg.get("content") {
                    Some(Value::String(s)) if !s.trim().is_empty() => {
                        text_parts.push(s.trim().to_owned());
                    }
                    Some(Value::Array(blocks)) => {
                        for b in blocks {
                            match b.get("type").and_then(Value::as_str) {
                                Some("text") => {
                                    if let Some(t) = b.get("text").and_then(Value::as_str)
                                        && !t.trim().is_empty()
                                    {
                                        text_parts.push(t.trim().to_owned());
                                    }
                                }
                                Some("thinking") => {
                                    if let Some(t) = b.get("thinking").and_then(Value::as_str)
                                        && !t.trim().is_empty()
                                    {
                                        reasoning_parts.push(t.trim().to_owned());
                                    }
                                }
                                Some("tool_use") => {
                                    let name =
                                        b.get("name").and_then(Value::as_str).unwrap_or("tool");
                                    let args = summarize_claude_tool_input(b.get("input"));
                                    let (out, is_err) = b
                                        .get("id")
                                        .and_then(Value::as_str)
                                        .and_then(|id| tool_results.get(id))
                                        .map(|(o, e)| (Some(o.as_str()), *e))
                                        .unwrap_or((None, false));
                                    text_parts.push(format_imported_tool(name, &args, out, is_err));
                                }
                                _ => {}
                            }
                        }
                    }
                    _ => {}
                }

                if !text_parts.is_empty() || !reasoning_parts.is_empty() {
                    raw_turns.push(ImportedTurn {
                        role: ImportedRole::Assistant,
                        ts,
                        text: text_parts.join("\n\n"),
                        reasoning: if reasoning_parts.is_empty() {
                            None
                        } else {
                            Some(reasoning_parts.join("\n\n"))
                        },
                    });
                }
            }
        }

        let turns = coalesce_turns(raw_turns);
        if turns.is_empty() {
            return Err("no user/assistant messages found in Claude Code session".into());
        }
        let title = title_hint
            .as_deref()
            .or(first_user_prompt.as_deref())
            .map(|t| normalize_title(t, &session_id))
            .unwrap_or_else(|| session_id.clone());

        Ok(ImportedConversation {
            source: ImportSource::Claude,
            original_id: session_id,
            title,
            original_cwd,
            model,
            turns,
        })
    }
}

fn select_claude_lineage(records: Vec<Value>) -> Vec<Value> {
    if records.is_empty() {
        return records;
    }
    let has_uuids = records
        .iter()
        .any(|r| r.get("uuid").and_then(Value::as_str).is_some());
    if !has_uuids {
        return records;
    }

    let mut by_uuid: HashMap<String, usize> = HashMap::with_capacity(records.len());
    for (idx, r) in records.iter().enumerate() {
        if let Some(u) = r.get("uuid").and_then(Value::as_str) {
            by_uuid.insert(u.to_owned(), idx);
        }
    }

    // Walk backwards from the last record having a `uuid`.
    let Some(mut cur_idx) = records
        .iter()
        .rposition(|r| r.get("uuid").and_then(Value::as_str).is_some())
    else {
        return records;
    };

    let mut chain_indices: Vec<usize> = Vec::new();
    let mut visited = HashSet::new();
    while visited.insert(cur_idx) {
        chain_indices.push(cur_idx);
        let parent = records[cur_idx]
            .get("parentUuid")
            .and_then(Value::as_str)
            .and_then(|pu| by_uuid.get(pu).copied());
        match parent {
            Some(pidx) => cur_idx = pidx,
            None => break,
        }
    }
    chain_indices.reverse();
    // If parentUuid chain was broken by a non-message node (e.g. system/snapshot) and covered
    // < 50% of messages, fall back to chronological non-sidechain order.
    if chain_indices.len() * 2 < records.len() && records.len() > 2 {
        return records;
    }
    chain_indices
        .into_iter()
        .map(|i| records[i].clone())
        .collect()
}

fn scan_claude_summary(path: &Path, current_cwd: &Path) -> Option<ExternalSessionSummary> {
    let f = File::open(path).ok()?;
    let reader = BufReader::new(f);
    let mut id = path.file_stem()?.to_str()?.to_owned();
    let mut title: Option<String> = None;
    let mut first_prompt: Option<String> = None;
    let mut cwd: Option<String> = None;
    let mut last_ts: Option<String> = None;
    let mut turns = 0usize;

    for line in reader.lines().map_while(Result::ok) {
        if line.trim().is_empty() {
            continue;
        }
        let Ok(v) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        if let Some(sid) = v.get("sessionId").and_then(Value::as_str)
            && !sid.is_empty()
        {
            id = sid.to_owned();
        }
        if cwd.is_none()
            && let Some(c) = v.get("cwd").and_then(Value::as_str)
            && !c.is_empty()
        {
            cwd = Some(c.to_owned());
        }
        if let Some(ts) = v.get("timestamp").and_then(Value::as_str) {
            last_ts = Some(ts.to_owned());
        }
        match v.get("type").and_then(Value::as_str) {
            Some("ai-title") => {
                if let Some(t) = v.get("aiTitle").and_then(Value::as_str)
                    && !t.trim().is_empty()
                {
                    title = Some(t.trim().to_owned());
                }
            }
            Some("user") => {
                if v.get("isSidechain").and_then(Value::as_bool) == Some(true)
                    || v.get("isMeta").and_then(Value::as_bool) == Some(true)
                {
                    continue;
                }
                let text =
                    extract_claude_user_text(v.get("message").and_then(|m| m.get("content")));
                let trimmed = text.trim();
                if !trimmed.is_empty() && !is_claude_internal_wrapper(trimmed) {
                    turns += 1;
                    if first_prompt.is_none() {
                        first_prompt = Some(trimmed.to_owned());
                    }
                }
            }
            Some("assistant") if v.get("isSidechain").and_then(Value::as_bool) != Some(true) => {
                turns += 1;
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
    let final_title = normalize_title(
        title.as_deref().or(first_prompt.as_deref()).unwrap_or(&id),
        &id,
    );
    let matches = cwd_matches(cwd.as_deref(), current_cwd);
    Some(ExternalSessionSummary {
        source: ImportSource::Claude,
        id,
        title: final_title,
        cwd,
        updated_at,
        updated_epoch,
        turn_count: turns,
        path: path.to_path_buf(),
        matches_cwd: matches,
    })
}

fn extract_claude_user_text(content: Option<&Value>) -> String {
    match content {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(blocks)) => {
            let mut parts = Vec::new();
            for b in blocks {
                if b.get("type").and_then(Value::as_str) == Some("text")
                    && let Some(t) = b.get("text").and_then(Value::as_str)
                    && !t.trim().is_empty()
                {
                    parts.push(t.trim().to_owned());
                }
            }
            parts.join("\n")
        }
        _ => String::new(),
    }
}

fn extract_claude_tool_result_text(content: Option<&Value>) -> String {
    match content {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(blocks)) => blocks
            .iter()
            .filter_map(|b| {
                b.get("text")
                    .and_then(Value::as_str)
                    .or_else(|| b.as_str())
                    .map(ToOwned::to_owned)
            })
            .collect::<Vec<_>>()
            .join("\n"),
        Some(other) => other.to_string(),
        None => String::new(),
    }
}

fn summarize_claude_tool_input(input: Option<&Value>) -> String {
    let Some(v) = input else {
        return String::new();
    };
    for key in ["command", "cmd", "file_path", "path", "pattern", "query"] {
        if let Some(s) = v.get(key).and_then(Value::as_str) {
            return single_line_truncate(s, 140);
        }
    }
    single_line_truncate(&v.to_string(), 140)
}

fn is_claude_internal_wrapper(trimmed: &str) -> bool {
    trimmed.starts_with("<local-command-caveat>")
        || trimmed.starts_with("<command-name>")
        || trimmed.starts_with("<local-command-stdout>")
}
