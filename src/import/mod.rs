//! Extensible external-session import adapters (`ClaudeCodeAdapter`, `CodexAdapter`,
//! `AntigravityAdapter`) converting foreign transcripts into standard `anix-proto` Tree WAL files
//! verified by `anix fsck` — zero changes required in Layer 1 `anix-core`.

mod antigravity;
mod claude;
mod codex;

use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::UNIX_EPOCH;

use anix_proto::IR_VERSION;
use anix_proto::ir::{
    HeaderTag, IrHeader, IrMessage, IrMeta, IrStream, OutputMode, StderrMode, ToolDef,
};
use anix_proto::wal::{WalConfigSnapshot, WalEvent, WalRole};

use crate::session::now_rfc3339;

pub use antigravity::AntigravityAdapter;
pub use claude::ClaudeCodeAdapter;
pub use codex::CodexAdapter;

/// Max UTF-8 bytes preserved per individual imported tool output (keeps full conversation turns
/// crisp while avoiding massive single-tool dumps; `anix` handles overall context compaction).
pub(crate) const TOOL_OUTPUT_MAX_BYTES: usize = 2048;

/// Supported external agent ecosystems.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ImportSource {
    Claude,
    Codex,
    Antigravity,
}

impl ImportSource {
    pub const fn id(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
            Self::Antigravity => "antigravity",
        }
    }

    pub const fn label(self) -> &'static str {
        match self {
            Self::Claude => "Claude Code",
            Self::Codex => "Codex",
            Self::Antigravity => "Antigravity",
        }
    }

    pub const fn badge(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
            Self::Antigravity => "agy",
        }
    }

    pub fn parse_filter(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "claude" | "claude-code" | "cc" => Some(Self::Claude),
            "codex" | "codex-cli" => Some(Self::Codex),
            "antigravity" | "agy" | "gemini" => Some(Self::Antigravity),
            _ => None,
        }
    }
}

/// Summary row surfaced in the interactive `/import` picker.
#[derive(Debug, Clone)]
pub struct ExternalSessionSummary {
    pub source: ImportSource,
    pub id: String,
    pub title: String,
    pub cwd: Option<String>,
    pub updated_at: String,
    pub updated_epoch: i64,
    pub turn_count: usize,
    pub path: PathBuf,
    pub matches_cwd: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImportedRole {
    User,
    Assistant,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportedTurn {
    pub role: ImportedRole,
    pub ts: String,
    pub text: String,
    pub reasoning: Option<String>,
}

/// Normalized, provider-agnostic conversation produced by any [`ImportAdapter`].
#[derive(Debug, Clone)]
pub struct ImportedConversation {
    pub source: ImportSource,
    pub original_id: String,
    pub title: String,
    pub original_cwd: Option<String>,
    pub model: Option<String>,
    pub turns: Vec<ImportedTurn>,
}

/// Adapter trait implemented for each external coding-agent transcript format.
pub trait ImportAdapter: Send + Sync {
    fn source(&self) -> ImportSource;
    fn can_parse(&self, path: &Path) -> bool;
    fn discover(&self, home: &Path, current_cwd: &Path) -> Vec<ExternalSessionSummary>;
    fn load(&self, path: &Path) -> Result<ImportedConversation, String>;
}

/// Built-in adapter registry (add new sources here without touching UI or WAL code).
pub fn builtin_adapters() -> Vec<Box<dyn ImportAdapter>> {
    vec![
        Box::new(ClaudeCodeAdapter),
        Box::new(CodexAdapter),
        Box::new(AntigravityAdapter),
    ]
}

/// Discover sessions across all (or a single filtered) built-in adapters, sorted with matching
/// `cwd` first and most-recently-updated first.
pub fn discover_all_sessions(
    home: &Path,
    current_cwd: &Path,
    filter: Option<ImportSource>,
) -> Vec<ExternalSessionSummary> {
    let mut out = Vec::new();
    for adapter in builtin_adapters() {
        if let Some(f) = filter
            && adapter.source() != f
        {
            continue;
        }
        out.extend(adapter.discover(home, current_cwd));
    }
    out.sort_by(|a, b| {
        b.matches_cwd
            .cmp(&a.matches_cwd)
            .then_with(|| b.updated_epoch.cmp(&a.updated_epoch))
            .then_with(|| a.id.cmp(&b.id))
    });
    out
}

/// Parse an external session file using an explicit source hint or automatic adapter detection.
pub fn load_external_session(
    path: &Path,
    hint: Option<ImportSource>,
) -> Result<ImportedConversation, String> {
    if !path.is_file() {
        return Err(format!("file does not exist: `{}`", path.display()));
    }
    let adapters = builtin_adapters();
    if let Some(src) = hint
        && let Some(adapter) = adapters.iter().find(|a| a.source() == src)
    {
        return adapter.load(path);
    }
    for adapter in &adapters {
        if adapter.can_parse(path) {
            return adapter.load(path);
        }
    }
    Err(format!(
        "could not recognize transcript format for `{}` (supported: claude, codex, antigravity)",
        path.display()
    ))
}

/// Serialize a normalized [`ImportedConversation`] into a valid `anix-proto` Tree WAL (`0600`),
/// running active `pre-context.d/` hooks on the seed system prompt and validating via `anix fsck`.
/// Returns the final leaf node ID.
#[allow(clippy::too_many_arguments)]
pub fn write_imported_wal(
    wal_path: &Path,
    session_id: &str,
    base_system_prompt: &str,
    current_cwd: &Path,
    hooks_dir: &Path,
    shell_name: &str,
    conv: &ImportedConversation,
    anix_bin: &Path,
) -> Result<u64, String> {
    if conv.turns.is_empty() {
        return Err("imported session contains no user/assistant turns".into());
    }

    let now = now_rfc3339();
    let system_text =
        build_imported_system_prompt(base_system_prompt, current_cwd, hooks_dir, conv);

    let mut events: Vec<WalEvent> = Vec::with_capacity(conv.turns.len() + 2);
    events.push(WalEvent::SessionInit {
        id: Some(1),
        ir_version: IR_VERSION,
        ts: now.clone(),
        session_id: Some(session_id.to_owned()),
        config: WalConfigSnapshot {
            stderr: Some(StderrMode::Auto),
            output: Some(OutputMode::Reply),
            shell: Some(shell_name.to_owned()),
            tool_name: Some("shell".into()),
        },
    });
    events.push(WalEvent::Message {
        id: Some(2),
        parent_id: Some(1),
        turn_id: 0,
        role: WalRole::System,
        ts: now.clone(),
        text: Some(system_text),
        reasoning: None,
        tool_calls: Vec::new(),
        tool_call_id: None,
        is_error: false,
        media: Vec::new(),
        provider_opaque: None,
        usage: None,
        message_ids: Vec::new(),
    });

    let mut parent_id = 2u64;
    for (idx, turn) in conv.turns.iter().enumerate() {
        let turn_id = (idx as u64) + 1;
        let node_id = turn_id + 2;
        let role = match turn.role {
            ImportedRole::User => WalRole::User,
            ImportedRole::Assistant => WalRole::Assistant,
        };
        let ts = if turn.ts.trim().is_empty() {
            now.clone()
        } else {
            turn.ts.clone()
        };
        events.push(WalEvent::Message {
            id: Some(node_id),
            parent_id: Some(parent_id),
            turn_id,
            role,
            ts,
            text: Some(turn.text.clone()),
            reasoning: turn.reasoning.clone(),
            tool_calls: Vec::new(),
            tool_call_id: None,
            is_error: false,
            media: Vec::new(),
            provider_opaque: None,
            usage: None,
            message_ids: Vec::new(),
        });
        parent_id = node_id;
    }

    if let Some(parent) = wal_path.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("create wal dir failed: {e}"))?;
    }
    let tmp_path = wal_path.with_extension(format!("wal.tmp.{}", uuid::Uuid::new_v4().simple()));
    {
        let mut f = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&tmp_path)
            .map_err(|e| format!("open temp wal failed: {e}"))?;
        for ev in &events {
            let line =
                serde_json::to_string(ev).map_err(|e| format!("serialize WalEvent failed: {e}"))?;
            f.write_all(line.as_bytes())
                .and_then(|()| f.write_all(b"\n"))
                .map_err(|e| format!("write temp wal failed: {e}"))?;
        }
        f.sync_data()
            .map_err(|e| format!("fsync temp wal failed: {e}"))?;
    }

    // Validate with Layer 1 `anix fsck` before promoting into place.
    let fsck = Command::new(anix_bin)
        .args(["fsck"])
        .arg(&tmp_path)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output();
    match fsck {
        Ok(out) if out.status.success() => {}
        Ok(out) => {
            let _ = fs::remove_file(&tmp_path);
            let detail = String::from_utf8_lossy(&out.stderr);
            let stdout = String::from_utf8_lossy(&out.stdout);
            return Err(format!(
                "anix fsck rejected imported WAL: {} {}",
                detail.trim(),
                stdout.trim()
            ));
        }
        Err(err) => {
            let _ = fs::remove_file(&tmp_path);
            return Err(format!("failed running `anix fsck`: {err}"));
        }
    }

    fs::rename(&tmp_path, wal_path).map_err(|e| {
        let _ = fs::remove_file(&tmp_path);
        format!("commit imported wal failed: {e}")
    })?;

    Ok(parent_id)
}

/// Assemble the frozen seed system prompt for an imported session: `<posix_runtime>` + agent base
/// prompt + active `hooks/pre-context.d/` (fail-open) + concise `<imported_session>` banner.
fn build_imported_system_prompt(
    base_system_prompt: &str,
    current_cwd: &Path,
    hooks_dir: &Path,
    conv: &ImportedConversation,
) -> String {
    let posix_contract = "\
<posix_runtime>
You are running inside the Anix POSIX runtime (`output=reply`, `stderr=auto`, tool=`shell`).
- Your FINAL text reply (after all `shell` tool calls finish) is delivered directly to `stdout` (`fd 1`). Intermediate commentary during tool-calling turns goes to `stderr` (`fd 2`).
- `$ANIX_OUT` is NOT mounted in `output=reply` mode; do NOT write to `$ANIX_OUT`.
- You may write progress notes or warnings during shell execution to `$ANIX_ERR` (`fd 2`, write-only pipe: redirect into it, never read it).
- `$ANIX_CTL` is a write-only JSONL side-channel pipe for runtime control frames (e.g. `compact`, `fold`, `media`, `hold`, `release`).
- You are running in persistent `--inbox` mode: `$ANIX_INBOX` points to your Maildir inbox; background jobs or sub-agents can send updates back via `anix send \"$ANIX_INBOX\"`.
</posix_runtime>";

    let orig_cwd = conv.original_cwd.as_deref().unwrap_or("(unknown)");
    let import_note = format!(
        "<imported_session source=\"{}\" original_id=\"{}\" original_cwd=\"{}\" current_cwd=\"{}\">\n\
This conversation was imported from {}. Earlier tool calls from {} are summarized inline as `<imported_tool>` blocks for context; from now on use the native Anix `shell` tool.\n\
</imported_session>",
        conv.source.id(),
        conv.original_id.replace('"', "'"),
        orig_cwd.replace('"', "'"),
        current_cwd.display().to_string().replace('"', "'"),
        conv.source.label(),
        conv.source.label(),
    );

    let mut seed_text = String::from(posix_contract);
    if !base_system_prompt.trim().is_empty() {
        seed_text.push_str("\n\n");
        seed_text.push_str(base_system_prompt.trim());
    }
    seed_text.push_str("\n\n");
    seed_text.push_str(&import_note);

    let mut stream = IrStream {
        header: IrHeader {
            ir_version: IR_VERSION,
            record_type: HeaderTag::Header,
            tools: vec![ToolDef {
                name: "shell".into(),
                description: "Execute POSIX shell command".into(),
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": { "cmd": { "type": "string" } },
                    "required": ["cmd"]
                }),
            }],
            max_output_tokens: None,
            params: BTreeMap::new(),
            meta: IrMeta {
                stderr_mode: StderrMode::Auto,
                output_mode: Some(OutputMode::Reply),
                cwd: current_cwd.display().to_string(),
                turn_budget: None,
                extra: BTreeMap::new(),
            },
            extra: BTreeMap::new(),
        },
        messages: vec![IrMessage::system(0, &seed_text)],
    };

    // Best-effort run active Layer 2 `pre-context.d/` hooks (`10-agents-md`, `15-memory`, etc.)
    // so imported sessions immediately get project rules, auto-memory, skills, and tips.
    if let Ok(rd) = fs::read_dir(hooks_dir) {
        let mut entries: Vec<(String, PathBuf)> = rd
            .flatten()
            .filter_map(|e| {
                let name = e.file_name().to_string_lossy().into_owned();
                if name.starts_with('.') {
                    return None;
                }
                let p = e.path();
                if is_hook_mask_or_non_exec(&p) {
                    return None;
                }
                Some((name, p))
            })
            .collect();
        entries.sort_by(|a, b| a.0.cmp(&b.0));

        for (_name, path) in entries {
            if let Ok(input) = stream.to_jsonl()
                && let Ok(stdout) = run_hook_fail_open(&path, &input, current_cwd)
                && let Ok(updated) = IrStream::from_jsonl(&stdout)
            {
                stream = updated;
            }
        }
    }

    stream
        .messages
        .into_iter()
        .find_map(|m| match m {
            IrMessage::System { text, .. } => Some(text),
            _ => None,
        })
        .unwrap_or(seed_text)
}

fn is_hook_mask_or_non_exec(path: &Path) -> bool {
    if let Ok(target) = fs::read_link(path)
        && target == Path::new("/dev/null")
    {
        return true;
    }
    let Ok(meta) = fs::metadata(path) else {
        return true;
    };
    if !meta.is_file() || meta.len() == 0 {
        return true;
    }
    (meta.permissions().mode() & 0o111) == 0
}

fn run_hook_fail_open(path: &Path, stdin_data: &str, cwd: &Path) -> io::Result<String> {
    let mut child = Command::new(path)
        .current_dir(cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()?;
    if let Some(mut stdin) = child.stdin.take() {
        let bytes = stdin_data.as_bytes().to_vec();
        thread::spawn(move || {
            let _ = stdin.write_all(&bytes);
        });
    }
    let mut out = String::new();
    if let Some(mut stdout) = child.stdout.take() {
        let _ = stdout.read_to_string(&mut out);
    }
    let status = child.wait()?;
    if status.success() {
        Ok(out)
    } else {
        Err(io::Error::other("hook failed"))
    }
}

pub(crate) fn truncate_utf8_bytes(s: &str, max_bytes: usize) -> (&str, bool) {
    if s.len() <= max_bytes {
        return (s, false);
    }
    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    (&s[..end], true)
}

pub(crate) fn format_imported_tool(
    name: &str,
    args_preview: &str,
    output: Option<&str>,
    is_error: bool,
) -> String {
    let clean_name = name.replace('"', "'");
    let clean_args = single_line_truncate(args_preview, 140).replace('"', "'");
    let err_attr = if is_error { " error=\"true\"" } else { "" };
    match output.map(str::trim).filter(|s| !s.is_empty()) {
        Some(raw) => {
            let (body, truncated) = truncate_utf8_bytes(raw, TOOL_OUTPUT_MAX_BYTES);
            let suffix = if truncated { "\n… (truncated)" } else { "" };
            format!(
                "<imported_tool name=\"{clean_name}\" args=\"{clean_args}\"{err_attr}>\n{body}{suffix}\n</imported_tool>"
            )
        }
        None => {
            format!("<imported_tool name=\"{clean_name}\" args=\"{clean_args}\"{err_attr} />")
        }
    }
}

/// Normalize a sequence of turns for universal multi-provider replay:
/// - Drops empty turns
/// - Drops any leading Assistant turns before the first User prompt
/// - Coalesces adjacent same-role turns cleanly with `\n\n`
pub(crate) fn coalesce_turns(raw: Vec<ImportedTurn>) -> Vec<ImportedTurn> {
    let mut out: Vec<ImportedTurn> = Vec::new();
    for mut turn in raw {
        turn.text = turn.text.trim().to_owned();
        if let Some(ref mut r) = turn.reasoning {
            *r = r.trim().to_owned();
            if r.is_empty() {
                turn.reasoning = None;
            }
        }
        if turn.text.is_empty() && turn.reasoning.is_none() {
            continue;
        }
        if out.is_empty() && turn.role != ImportedRole::User {
            continue;
        }
        if let Some(last) = out.last_mut()
            && last.role == turn.role
        {
            if !turn.text.is_empty() {
                if !last.text.is_empty() {
                    last.text.push_str("\n\n");
                }
                last.text.push_str(&turn.text);
            }
            if let Some(r) = turn.reasoning {
                match last.reasoning {
                    Some(ref mut prev) => {
                        prev.push_str("\n\n");
                        prev.push_str(&r);
                    }
                    None => last.reasoning = Some(r),
                }
            }
            if !turn.ts.is_empty() {
                last.ts = turn.ts;
            }
        } else {
            out.push(turn);
        }
    }
    out
}

pub(crate) fn single_line_truncate(raw: &str, max_chars: usize) -> String {
    let single = raw.replace(['\r', '\n'], " ");
    let trimmed = single.trim();
    let mut out = String::new();
    for (idx, ch) in trimmed.chars().enumerate() {
        if idx >= max_chars {
            out.push('…');
            break;
        }
        out.push(ch);
    }
    out
}

pub(crate) fn normalize_title(raw: &str, fallback: &str) -> String {
    let clean = single_line_truncate(raw, 72);
    if clean.is_empty() {
        fallback.to_owned()
    } else {
        clean
    }
}

pub(crate) fn cwd_matches(session_cwd: Option<&str>, current_cwd: &Path) -> bool {
    let Some(s) = session_cwd else {
        return false;
    };
    let sp = fs::canonicalize(Path::new(s)).unwrap_or_else(|_| PathBuf::from(s));
    let cp = fs::canonicalize(current_cwd).unwrap_or_else(|_| current_cwd.to_path_buf());
    sp == cp
}

pub(crate) fn file_mtime_epoch_and_rfc3339(path: &Path) -> (i64, String) {
    let secs = fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    (secs, epoch_to_rfc3339(secs))
}

pub(crate) fn parse_rfc3339_epoch(ts: &str) -> Option<i64> {
    let s = ts.trim();
    if s.len() < 19 {
        return None;
    }
    let y: i64 = s.get(0..4)?.parse().ok()?;
    let m: i64 = s.get(5..7)?.parse().ok()?;
    let d: i64 = s.get(8..10)?.parse().ok()?;
    let hh: i64 = s.get(11..13)?.parse().ok()?;
    let mm: i64 = s.get(14..16)?.parse().ok()?;
    let ss: i64 = s.get(17..19)?.parse().ok()?;
    let days = days_from_civil(y, m, d);
    Some(days * 86_400 + hh * 3600 + mm * 60 + ss)
}

fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = y - i64::from(m <= 2);
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = m + if m > 2 { -3 } else { 9 };
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

fn epoch_to_rfc3339(secs: i64) -> String {
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let hh = rem / 3600;
    let mm = (rem % 3600) / 60;
    let ss = rem % 60;
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = mp + if mp < 10 { 3 } else { -9 };
    let year = y + i64::from(m <= 2);
    format!("{year:04}-{m:02}-{d:02}T{hh:02}:{mm:02}:{ss:02}Z")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::resolve_anix_bin;

    #[test]
    fn all_three_adapters_parse_and_produce_fsck_valid_tree_wal() {
        let tmp = std::env::temp_dir().join(format!("anix-import-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(&tmp).unwrap();

        // 1. Claude Code sample (tree parentUuid + sidechain ignored + tool_use/tool_result paired)
        let claude_path = tmp.join("claude-demo.jsonl");
        let claude_jsonl = [
            r#"{"type":"ai-title","aiTitle":"Fix auth bug","sessionId":"cc-1"}"#,
            r#"{"type":"user","uuid":"u1","parentUuid":null,"isSidechain":false,"cwd":"/tmp/repo","timestamp":"2026-11-27T10:00:00Z","sessionId":"cc-1","message":{"role":"user","content":"Check auth.rs"}}"#,
            r#"{"type":"assistant","uuid":"a1","parentUuid":"u1","isSidechain":false,"timestamp":"2026-11-27T10:00:02Z","sessionId":"cc-1","message":{"role":"assistant","content":[{"type":"thinking","thinking":"Inspect auth.rs first","signature":"sig123"},{"type":"tool_use","id":"tu1","name":"Read","input":{"file_path":"src/auth.rs"}}]}}"#,
            r#"{"type":"user","uuid":"u2","parentUuid":"a1","isSidechain":false,"timestamp":"2026-11-27T10:00:03Z","sessionId":"cc-1","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"tu1","content":"fn verify() -> bool { true }"}]}}"#,
            r#"{"type":"assistant","uuid":"a2","parentUuid":"u2","isSidechain":false,"timestamp":"2026-11-27T10:00:05Z","sessionId":"cc-1","message":{"role":"assistant","content":[{"type":"text","text":"`verify()` always returns true."}]}}"#,
            r#"{"type":"assistant","uuid":"side1","parentUuid":"u1","isSidechain":true,"timestamp":"2026-11-27T10:00:06Z","sessionId":"cc-1","message":{"role":"assistant","content":[{"type":"text","text":"SIDECHAIN MUST BE IGNORED"}]}}"#,
        ]
        .join("\n");
        fs::write(&claude_path, claude_jsonl).unwrap();
        let cc = load_external_session(&claude_path, None).expect("claude parses");
        assert_eq!(cc.source, ImportSource::Claude);
        assert_eq!(cc.title, "Fix auth bug");
        assert_eq!(cc.turns.len(), 2);
        assert!(cc.turns[1].text.contains("<imported_tool name=\"Read\""));
        assert!(cc.turns[1].text.contains("verify()"));
        assert!(!cc.turns[1].text.contains("SIDECHAIN"));

        // 2. Codex CLI sample
        let codex_path = tmp.join("rollout-2026-11-27-codex.jsonl");
        let codex_jsonl = [
            r#"{"timestamp":"2026-11-27T11:00:00Z","type":"session_meta","payload":{"id":"cx-1","cwd":"/tmp/repo","timestamp":"2026-11-27T11:00:00Z"}}"#,
            r#"{"timestamp":"2026-11-27T11:00:01Z","type":"turn_context","payload":{"model":"gpt-5-codex","cwd":"/tmp/repo"}}"#,
            r#"{"timestamp":"2026-11-27T11:00:02Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"Run tests"}]}}"#,
            r#"{"timestamp":"2026-11-27T11:00:03Z","type":"response_item","payload":{"type":"function_call","call_id":"c1","name":"exec_command","arguments":"{\"cmd\":\"cargo test\"}"}}"#,
            r#"{"timestamp":"2026-11-27T11:00:04Z","type":"response_item","payload":{"type":"function_call_output","call_id":"c1","output":"test result: ok."}}"#,
            r#"{"timestamp":"2026-11-27T11:00:05Z","type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"All tests passed."}]}}"#,
        ]
        .join("\n");
        fs::write(&codex_path, codex_jsonl).unwrap();
        let cx = load_external_session(&codex_path, None).expect("codex parses");
        assert_eq!(cx.source, ImportSource::Codex);
        assert_eq!(cx.model.as_deref(), Some("gpt-5-codex"));
        assert_eq!(cx.turns.len(), 2);
        assert!(
            cx.turns[1]
                .text
                .contains("<imported_tool name=\"exec_command\"")
        );
        assert!(cx.turns[1].text.contains("All tests passed."));

        // 3. Antigravity sample
        let agy_dir = tmp.join("brain-abc/.system_generated/logs");
        fs::create_dir_all(&agy_dir).unwrap();
        let agy_path = agy_dir.join("transcript.jsonl");
        let agy_jsonl = [
            r#"{"step_index":0,"type":"USER_INPUT","created_at":"2026-11-27T12:00:00Z","content":"<USER_REQUEST>\nSummarize Cargo.toml\n</USER_REQUEST>"}"#,
            r#"{"step_index":1,"type":"PLANNER_RESPONSE","created_at":"2026-11-27T12:00:02Z","model":"gemini-pro","thinking":"Read Cargo.toml","tool_calls":[{"name":"read_file","args":{"Path":"/tmp/Cargo.toml"}}]}"#,
            r#"{"step_index":2,"type":"GENERIC","created_at":"2026-11-27T12:00:03Z","content":"[package]\nname = \"demo\""}"#,
            r#"{"step_index":3,"type":"PLANNER_RESPONSE","created_at":"2026-11-27T12:00:04Z","model":"gemini-pro","content":"The package is named `demo`."}"#,
        ]
        .join("\n");
        fs::write(&agy_path, agy_jsonl).unwrap();
        let agy = load_external_session(&agy_path, None).expect("antigravity parses");
        assert_eq!(agy.source, ImportSource::Antigravity);
        assert_eq!(agy.turns.len(), 2);
        assert_eq!(agy.turns[0].text, "Summarize Cargo.toml");
        assert!(
            agy.turns[1]
                .text
                .contains("<imported_tool name=\"read_file\"")
        );
        assert!(agy.turns[1].text.contains("demo"));

        // 4. Write Tree WAL and verify against real `anix fsck` if `anix` is available.
        if let Ok(anix_bin) = resolve_anix_bin(None) {
            let wal_out = tmp.join("imported.wal");
            let empty_hooks = tmp.join("empty-hooks");
            fs::create_dir_all(&empty_hooks).unwrap();
            let leaf = write_imported_wal(
                &wal_out,
                "s-import-test",
                "Base test prompt",
                &tmp,
                &empty_hooks,
                "msh",
                &cc,
                &anix_bin,
            )
            .expect("write_imported_wal + anix fsck must succeed");
            assert_eq!(leaf, 4); // 1=SessionInit, 2=System, 3=User, 4=Assistant
        }

        let _ = fs::remove_dir_all(&tmp);
    }
}
