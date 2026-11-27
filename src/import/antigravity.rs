//! Import adapter for Antigravity CLI transcripts
//! (`~/.gemini/antigravity-cli/brain/<conversation-id>/.system_generated/logs/transcript.jsonl`).

use std::collections::VecDeque;
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
pub struct AntigravityAdapter;

impl ImportAdapter for AntigravityAdapter {
    fn source(&self) -> ImportSource {
        ImportSource::Antigravity
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
            if v.get("step_index").is_some()
                && matches!(
                    v.get("type").and_then(Value::as_str),
                    Some("USER_INPUT" | "PLANNER_RESPONSE" | "GENERIC" | "CHECKPOINT")
                )
            {
                return true;
            }
        }
        false
    }

    fn discover(&self, home: &Path, current_cwd: &Path) -> Vec<ExternalSessionSummary> {
        let brain_dir = home.join(".gemini/antigravity-cli/brain");
        let Ok(entries) = fs::read_dir(&brain_dir) else {
            return Vec::new();
        };
        let mut out = Vec::new();
        for entry in entries.flatten() {
            let conv_dir = entry.path();
            if !conv_dir.is_dir() {
                continue;
            }
            let transcript = conv_dir.join(".system_generated/logs/transcript.jsonl");
            if !transcript.is_file() {
                continue;
            }
            let conv_id = entry.file_name().to_string_lossy().into_owned();
            if let Some(summary) = scan_agy_summary(&transcript, &conv_id, current_cwd) {
                out.push(summary);
            }
        }
        out
    }

    fn load(&self, path: &Path) -> Result<ImportedConversation, String> {
        let f = File::open(path).map_err(|e| format!("open antigravity transcript failed: {e}"))?;
        let reader = BufReader::new(f);

        let conv_id = infer_agy_conv_id(path);
        let mut model: Option<String> = None;
        let mut original_cwd: Option<String> = None;
        let mut first_user_prompt: Option<String> = None;
        let mut raw_turns: Vec<ImportedTurn> = Vec::new();
        // Queue of `(tool_name, args_preview, ts)` awaiting subsequent `GENERIC` tool outputs.
        let mut pending_tools: VecDeque<(String, String, String)> = VecDeque::new();

        for line in reader.lines().map_while(Result::ok) {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            let Ok(v) = serde_json::from_str::<Value>(trimmed) else {
                continue;
            };
            let ts = v
                .get("created_at")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_owned();

            match v.get("type").and_then(Value::as_str) {
                Some("USER_INPUT") => {
                    flush_unmatched_tools(&mut pending_tools, &mut raw_turns);
                    let raw = v.get("content").and_then(Value::as_str).unwrap_or("");
                    if original_cwd.is_none() {
                        original_cwd = extract_agy_cwd_hint(raw);
                    }
                    let clean = unwrap_agy_user_request(raw);
                    if clean.is_empty() {
                        continue;
                    }
                    if first_user_prompt.is_none() {
                        first_user_prompt = Some(clean.clone());
                    }
                    raw_turns.push(ImportedTurn {
                        role: ImportedRole::User,
                        ts,
                        text: clean,
                        reasoning: None,
                    });
                }
                Some("PLANNER_RESPONSE") => {
                    flush_unmatched_tools(&mut pending_tools, &mut raw_turns);
                    if model.is_none()
                        && let Some(m) = v.get("model").and_then(Value::as_str)
                        && !m.is_empty()
                    {
                        model = Some(m.to_owned());
                    }
                    let text = v
                        .get("content")
                        .and_then(Value::as_str)
                        .map(str::trim)
                        .unwrap_or("")
                        .to_owned();
                    let reasoning = v
                        .get("thinking")
                        .and_then(Value::as_str)
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                        .map(ToOwned::to_owned);

                    if !text.is_empty() || reasoning.is_some() {
                        raw_turns.push(ImportedTurn {
                            role: ImportedRole::Assistant,
                            ts: ts.clone(),
                            text,
                            reasoning,
                        });
                    }
                    if let Some(calls) = v.get("tool_calls").and_then(Value::as_array) {
                        for tc in calls {
                            let name = tc.get("name").and_then(Value::as_str).unwrap_or("tool");
                            // Skip internal UI checklist updates to keep imported history clean.
                            if name == "update_steps" {
                                continue;
                            }
                            let args = summarize_agy_tool_args(tc.get("args"));
                            pending_tools.push_back((name.to_owned(), args, ts.clone()));
                        }
                    }
                }
                Some("GENERIC") => {
                    let out_raw = v.get("content").and_then(Value::as_str).unwrap_or("");
                    let is_err = v.get("error").is_some_and(|e| !e.is_null());
                    if let Some((name, args, call_ts)) = pending_tools.pop_front() {
                        raw_turns.push(ImportedTurn {
                            role: ImportedRole::Assistant,
                            ts: if ts.is_empty() { call_ts } else { ts },
                            text: format_imported_tool(&name, &args, Some(out_raw), is_err),
                            reasoning: None,
                        });
                    }
                }
                _ => {}
            }
        }
        flush_unmatched_tools(&mut pending_tools, &mut raw_turns);

        let turns = coalesce_turns(raw_turns);
        if turns.is_empty() {
            return Err("no user/assistant messages found in Antigravity transcript".into());
        }
        let title = first_user_prompt
            .as_deref()
            .map(|t| normalize_title(t, &conv_id))
            .unwrap_or_else(|| conv_id.clone());

        Ok(ImportedConversation {
            source: ImportSource::Antigravity,
            original_id: conv_id,
            title,
            original_cwd,
            model,
            turns,
        })
    }
}

fn flush_unmatched_tools(
    pending: &mut VecDeque<(String, String, String)>,
    out: &mut Vec<ImportedTurn>,
) {
    while let Some((name, args, ts)) = pending.pop_front() {
        out.push(ImportedTurn {
            role: ImportedRole::Assistant,
            ts,
            text: format_imported_tool(&name, &args, None, false),
            reasoning: None,
        });
    }
}

fn infer_agy_conv_id(path: &Path) -> String {
    // Expected: `.../<conv_id>/.system_generated/logs/transcript.jsonl`
    path.parent()
        .and_then(Path::parent)
        .and_then(Path::parent)
        .and_then(Path::file_name)
        .and_then(|s| s.to_str())
        .or_else(|| path.file_stem().and_then(|s| s.to_str()))
        .unwrap_or("antigravity-session")
        .to_owned()
}

fn scan_agy_summary(
    path: &Path,
    conv_id: &str,
    current_cwd: &Path,
) -> Option<ExternalSessionSummary> {
    let f = File::open(path).ok()?;
    let reader = BufReader::new(f);
    let mut first_prompt: Option<String> = None;
    let mut last_ts: Option<String> = None;
    let mut cwd: Option<String> = None;
    let mut turns = 0usize;

    for line in reader.lines().map_while(Result::ok) {
        if line.trim().is_empty() {
            continue;
        }
        let Ok(v) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        if let Some(ts) = v.get("created_at").and_then(Value::as_str) {
            last_ts = Some(ts.to_owned());
        }
        match v.get("type").and_then(Value::as_str) {
            Some("USER_INPUT") => {
                let raw = v.get("content").and_then(Value::as_str).unwrap_or("");
                if cwd.is_none() {
                    cwd = extract_agy_cwd_hint(raw);
                }
                let clean = unwrap_agy_user_request(raw);
                if !clean.is_empty() {
                    turns += 1;
                    if first_prompt.is_none() {
                        first_prompt = Some(clean);
                    }
                }
            }
            Some("PLANNER_RESPONSE")
                if v.get("content")
                    .and_then(Value::as_str)
                    .is_some_and(|s| !s.trim().is_empty()) =>
            {
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
    let short_id = if conv_id.len() > 12 {
        conv_id[..12].to_owned()
    } else {
        conv_id.to_owned()
    };
    let title = normalize_title(first_prompt.as_deref().unwrap_or(&short_id), &short_id);
    let matches = cwd_matches(cwd.as_deref(), current_cwd);
    Some(ExternalSessionSummary {
        source: ImportSource::Antigravity,
        id: short_id,
        title,
        cwd,
        updated_at,
        updated_epoch,
        turn_count: turns,
        path: path.to_path_buf(),
        matches_cwd: matches,
    })
}

fn unwrap_agy_user_request(raw: &str) -> String {
    if let Some(start) = raw.find("<USER_REQUEST>") {
        let after = &raw[start + "<USER_REQUEST>".len()..];
        let body = match after.find("</USER_REQUEST>") {
            Some(end) => &after[..end],
            None => after,
        };
        return body.trim().to_owned();
    }
    raw.trim().to_owned()
}

fn extract_agy_cwd_hint(raw: &str) -> Option<String> {
    for marker in ["cwd:", "Active Workspaces:\n- "] {
        if let Some(pos) = raw.find(marker) {
            let after = &raw[pos + marker.len()..];
            let line = after.lines().next()?.trim();
            if line.starts_with('/') {
                return Some(line.to_owned());
            }
        }
    }
    None
}

fn summarize_agy_tool_args(args: Option<&Value>) -> String {
    let Some(v) = args else {
        return String::new();
    };
    for key in ["CommandLine", "Path", "query", "Target", "Pattern"] {
        if let Some(s) = v.get(key).and_then(Value::as_str) {
            return single_line_truncate(s, 140);
        }
    }
    single_line_truncate(&v.to_string(), 140)
}
