//! Layer 3 shell approval wrapper (`anix-agent approve-wrap`) and parent Unix domain socket
//! approval broker.
//!
//! By routing approval prompts over a private `0600` Unix domain socket instead of opening
//! `/dev/tty` directly, `anix-agent` preserves full ownership of the interactive terminal while
//! staying 100% compliant with `anix-core`'s `--shell` wrapper chain contract (RFC §6.1).

use std::collections::HashSet;
use std::env;
use std::fs;
use std::io::{self, BufRead, BufReader, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};
use std::sync::mpsc;
use std::thread;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::cli::ApprovalPolicy;
use crate::session::is_executable;

const DENY_TO_MODEL_EXIT: u8 = 126;
const ABORT_TEMPFAIL_EXIT: u8 = 75;

/// Default read-only / inspection commands allowed automatically in `Ask` and `Plan` modes.
pub const DEFAULT_SAFE_COMMANDS: &[&str] = &[
    "ls",
    "cat",
    "head",
    "tail",
    "rg",
    "grep",
    "find",
    "pwd",
    "wc",
    "which",
    "echo",
    "printf",
    "stat",
    "file",
    "tree",
    "git status",
    "git diff",
    "git log",
    "git show",
    "git branch",
    "anix log",
    "anix fsck",
    "aclr",
    "aview",
    "aplan",
];

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApprovalRequestWire {
    pub worker_id: String,
    pub cmd: String,
    pub cwd: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "decision", rename_all = "snake_case")]
pub enum ApprovalResponseWire {
    Allow { cmd: String },
    Deny { reason: String },
    Abort { reason: String },
}

/// Live approval request delivered from the UDS server thread to the `App` event loop.
#[derive(Debug)]
pub struct PendingApproval {
    pub worker_id: String,
    pub cmd: String,
    pub cwd: String,
    reply_tx: Option<mpsc::SyncSender<ApprovalResponseWire>>,
}

impl PendingApproval {
    pub fn allow(mut self, cmd: String) {
        if let Some(tx) = self.reply_tx.take() {
            let _ = tx.send(ApprovalResponseWire::Allow { cmd });
        }
    }

    pub fn deny(mut self, reason: impl Into<String>) {
        if let Some(tx) = self.reply_tx.take() {
            let _ = tx.send(ApprovalResponseWire::Deny {
                reason: reason.into(),
            });
        }
    }

    pub fn abort(mut self, reason: impl Into<String>) {
        if let Some(tx) = self.reply_tx.take() {
            let _ = tx.send(ApprovalResponseWire::Abort {
                reason: reason.into(),
            });
        }
    }
}

/// Session-scoped Unix domain socket server accepting approval queries from `approve-wrap`.
#[derive(Debug)]
pub struct ApprovalServer {
    socket_path: PathBuf,
}

impl ApprovalServer {
    pub fn start(
        runtime_dir: &Path,
        on_request: impl Fn(PendingApproval) + Send + 'static,
    ) -> io::Result<Self> {
        let short_name = format!(
            "ap-{}-{}.sock",
            std::process::id(),
            &Uuid::new_v4().simple().to_string()[..6]
        );
        // macOS `sun_path` limit is 104 bytes; fall back to `/tmp/anix-agent-<uid>` if needed.
        let base_dir = if runtime_dir.join(&short_name).as_os_str().len() < 96 {
            runtime_dir.to_path_buf()
        } else {
            // SAFETY: `geteuid(2)` cannot fail.
            let uid = unsafe { libc::geteuid() };
            PathBuf::from(format!("/tmp/anix-agent-{uid}"))
        };
        fs::create_dir_all(&base_dir)?;
        let _ = fs::set_permissions(&base_dir, fs::Permissions::from_mode(0o700));
        let socket_path = base_dir.join(&short_name);
        let _ = fs::remove_file(&socket_path);

        let listener = UnixListener::bind(&socket_path)?;
        fs::set_permissions(&socket_path, fs::Permissions::from_mode(0o600))?;

        thread::spawn(move || {
            for conn in listener.incoming() {
                let Ok(stream) = conn else {
                    break;
                };
                let (reply_tx, reply_rx) = mpsc::sync_channel::<ApprovalResponseWire>(1);
                let mut reader = BufReader::new(match stream.try_clone() {
                    Ok(s) => s,
                    Err(_) => continue,
                });
                let mut line = String::new();
                if reader.read_line(&mut line).ok().unwrap_or(0) == 0 {
                    continue;
                }
                let Ok(req) = serde_json::from_str::<ApprovalRequestWire>(line.trim()) else {
                    let _ = write_response(
                        stream,
                        &ApprovalResponseWire::Abort {
                            reason: "malformed approval request".into(),
                        },
                    );
                    continue;
                };

                on_request(PendingApproval {
                    worker_id: req.worker_id,
                    cmd: req.cmd,
                    cwd: req.cwd,
                    reply_tx: Some(reply_tx),
                });

                // Respond on a lightweight detached thread so concurrent workers never head-of-line block.
                thread::spawn(move || {
                    let resp = reply_rx.recv().unwrap_or(ApprovalResponseWire::Abort {
                        reason: "approval session closed".into(),
                    });
                    let _ = write_response(stream, &resp);
                });
            }
        });

        Ok(Self { socket_path })
    }

    pub fn socket_path(&self) -> &Path {
        &self.socket_path
    }
}

impl Drop for ApprovalServer {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.socket_path);
    }
}

fn write_response(mut stream: UnixStream, resp: &ApprovalResponseWire) -> io::Result<()> {
    let mut line =
        serde_json::to_string(resp).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    line.push('\n');
    stream.write_all(line.as_bytes())?;
    stream.flush()
}

/// Evaluate automatic approval rules before deciding whether to show an inline prompt.
/// Returns `Some(ApprovalResponseWire)` if decided automatically, or `None` if user input is needed.
pub fn try_auto_decide(
    policy: ApprovalPolicy,
    always_allowed: &HashSet<String>,
    cmd: &str,
) -> Option<ApprovalResponseWire> {
    match policy {
        ApprovalPolicy::Auto => Some(ApprovalResponseWire::Allow {
            cmd: cmd.to_owned(),
        }),
        ApprovalPolicy::Plan => {
            if matches_safe_commands(cmd) {
                Some(ApprovalResponseWire::Allow {
                    cmd: cmd.to_owned(),
                })
            } else {
                Some(ApprovalResponseWire::Deny {
                    reason: "Plan mode (read-only): mutating shell commands are blocked. Propose a plan or ask the user to switch mode (Shift+Tab).".into(),
                })
            }
        }
        ApprovalPolicy::Ask => {
            if matches_safe_commands(cmd) {
                return Some(ApprovalResponseWire::Allow {
                    cmd: cmd.to_owned(),
                });
            }
            if is_single_simple_command(cmd)
                && let Some(bin) = extract_argv0(cmd)
                && always_allowed.contains(&bin)
            {
                return Some(ApprovalResponseWire::Allow {
                    cmd: cmd.to_owned(),
                });
            }
            None
        }
    }
}

/// Entry point for `anix-agent approve-wrap --ipc <SOCK> --worker-id <ID> -- <SHELL> ... -c <CMD>`.
pub fn run_approve_wrap(ipc: &Path, worker_id: &str, wrapped: &[String]) -> ExitCode {
    if wrapped.is_empty() {
        eprintln!("anix-agent approve-wrap: missing wrapped shell command");
        return ExitCode::from(2);
    }
    let shell_spec = &wrapped[0];
    let shell_args = wrapped[1..].to_vec();
    let mut cmd_idx = None;
    for idx in 0..shell_args.len() {
        if shell_args[idx] == "-c" && idx + 1 < shell_args.len() {
            cmd_idx = Some(idx + 1);
        }
    }
    let Some(cmd_idx) = cmd_idx else {
        eprintln!("anix-agent approve-wrap: wrapped command must contain `-c <CMD>`");
        return ExitCode::from(2);
    };
    let cmd_str = shell_args[cmd_idx].clone();
    let cwd = env::current_dir()
        .map(|p| p.display().to_string())
        .unwrap_or_default();

    let mut stream = match UnixStream::connect(ipc) {
        Ok(s) => s,
        Err(err) => {
            eprintln!(
                "anix-agent approve-wrap: failed connecting to `{}`: {err}",
                ipc.display()
            );
            return ExitCode::from(ABORT_TEMPFAIL_EXIT);
        }
    };

    let req = ApprovalRequestWire {
        worker_id: worker_id.to_owned(),
        cmd: cmd_str,
        cwd,
    };
    let Ok(mut payload) = serde_json::to_string(&req) else {
        eprintln!("anix-agent approve-wrap: failed serializing approval request");
        return ExitCode::from(ABORT_TEMPFAIL_EXIT);
    };
    payload.push('\n');
    if let Err(err) = stream
        .write_all(payload.as_bytes())
        .and_then(|_| stream.flush())
    {
        eprintln!("anix-agent approve-wrap: failed writing approval request: {err}");
        return ExitCode::from(ABORT_TEMPFAIL_EXIT);
    }

    let mut reader = BufReader::new(stream);
    let mut resp_line = String::new();
    match reader.read_line(&mut resp_line) {
        Ok(0) | Err(_) => {
            eprintln!("anix-agent approve-wrap: approval broker closed connection (Fail-Closed)");
            return ExitCode::from(ABORT_TEMPFAIL_EXIT);
        }
        Ok(_) => {}
    }

    let resp: ApprovalResponseWire = match serde_json::from_str(resp_line.trim()) {
        Ok(r) => r,
        Err(err) => {
            eprintln!("anix-agent approve-wrap: invalid approval response: {err}");
            return ExitCode::from(ABORT_TEMPFAIL_EXIT);
        }
    };

    match resp {
        ApprovalResponseWire::Allow { cmd: final_cmd } => {
            let mut exec_args = shell_args;
            exec_args[cmd_idx] = final_cmd;
            let resolved_shell = resolve_shell_bin(shell_spec);
            let err = Command::new(&resolved_shell).args(&exec_args).exec();
            eprintln!(
                "anix-agent approve-wrap: failed to exec `{}`: {err}",
                resolved_shell.display()
            );
            ExitCode::FAILURE
        }
        ApprovalResponseWire::Deny { reason } => {
            println!("Error: {reason}");
            ExitCode::from(DENY_TO_MODEL_EXIT)
        }
        ApprovalResponseWire::Abort { reason } => {
            eprintln!("anix-agent approve-wrap: {reason}");
            ExitCode::from(ABORT_TEMPFAIL_EXIT)
        }
    }
}

fn resolve_shell_bin(spec: &str) -> PathBuf {
    if spec.contains('/') {
        if let Some(rest) = spec.strip_prefix("~/")
            && let Some(home) = env::var_os("HOME")
        {
            return PathBuf::from(home).join(rest);
        }
        return PathBuf::from(spec);
    }
    if let Some(path_var) = env::var_os("PATH") {
        for dir in env::split_paths(&path_var) {
            let cand = dir.join(spec);
            if is_executable(&cand) {
                return cand;
            }
        }
    }
    if let Some(home) = env::var_os("HOME") {
        let libexec = PathBuf::from(home).join(".local/libexec/anix").join(spec);
        if is_executable(&libexec) {
            return libexec;
        }
    }
    if let Ok(exe) = env::current_exe()
        && let Some(dir) = exe.parent()
    {
        let sibling = dir.join(spec);
        if is_executable(&sibling) {
            return sibling;
        }
    }
    PathBuf::from(spec)
}

/// Safe read-only pipeline matcher (rejects redirections, command substitutions, `;`, `&&`, `||`, `&`).
pub fn matches_safe_commands(cmd: &str) -> bool {
    let trimmed = cmd.trim();
    if trimmed.is_empty() {
        return false;
    }
    if is_safe_aplan_heredoc(trimmed) {
        return true;
    }
    let Some(segments) = split_safe_pipeline(trimmed) else {
        return false;
    };
    if segments.is_empty() {
        return false;
    }
    segments.iter().all(|seg| segment_is_safe(seg))
}

/// Strictly validate `aplan <<'EOF'\n...\nEOF` (and safe unquoted heredocs without shell
/// expansions) so the agent can update the ephemeral live progress checklist without triggering
/// approval prompts in `ask` mode or being blocked in `plan` mode.
fn is_safe_aplan_heredoc(cmd: &str) -> bool {
    let mut lines: Vec<&str> = cmd.lines().collect();
    while lines.last().is_some_and(|l| l.trim().is_empty()) {
        lines.pop();
    }
    if lines.len() < 2 {
        return false;
    }
    let first = lines[0].trim();
    let Some(after_cmd) = first.strip_prefix("aplan") else {
        return false;
    };
    if !after_cmd.starts_with(char::is_whitespace) {
        return false;
    }
    let rest = after_cmd.trim();
    let Some(after_redir) = rest.strip_prefix("<<") else {
        return false;
    };
    let (strip_tabs, raw_delim) = match after_redir.strip_prefix('-') {
        Some(r) => (true, r.trim()),
        None => (false, after_redir.trim()),
    };
    if raw_delim.is_empty() {
        return false;
    }
    let (quoted, delim) = if raw_delim.len() >= 2
        && ((raw_delim.starts_with('\'') && raw_delim.ends_with('\''))
            || (raw_delim.starts_with('"') && raw_delim.ends_with('"')))
    {
        (true, &raw_delim[1..raw_delim.len() - 1])
    } else {
        (false, raw_delim)
    };
    if delim.is_empty()
        || !delim
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-'))
    {
        return false;
    }
    let matches_delim = |line: &str| {
        if strip_tabs {
            line.trim_start_matches('\t') == delim
        } else {
            line == delim
        }
    };
    let last_idx = lines.len() - 1;
    if !matches_delim(lines[last_idx]) {
        return false;
    }
    for &body_line in &lines[1..last_idx] {
        // Reject early delimiter termination (prevents smuggled commands after the heredoc).
        if matches_delim(body_line) {
            return false;
        }
        // Unquoted heredocs perform `$()` / backtick / `$VAR` expansion in POSIX sh.
        if !quoted && body_line.contains(['$', '`', '\\']) {
            return false;
        }
    }
    true
}

pub fn is_single_simple_command(cmd: &str) -> bool {
    split_safe_pipeline(cmd.trim()).is_some_and(|segs| segs.len() == 1)
}

pub fn extract_argv0(cmd: &str) -> Option<String> {
    let words = split_shell_words(cmd);
    for w in words {
        if w.contains('=') && !w.starts_with('=') && !w.starts_with('-') {
            continue;
        }
        return Some(w);
    }
    None
}

fn segment_is_safe(segment: &str) -> bool {
    let tokens = split_shell_words(segment);
    if tokens.is_empty() {
        return false;
    }
    let first = &tokens[0];
    let first_two = if tokens.len() >= 2 {
        Some(format!("{} {}", tokens[0], tokens[1]))
    } else {
        None
    };

    DEFAULT_SAFE_COMMANDS.iter().any(|&allowed| {
        if allowed.contains(' ') {
            first_two.as_deref() == Some(allowed)
        } else {
            first == allowed
        }
    })
}

fn split_safe_pipeline(cmd: &str) -> Option<Vec<String>> {
    let mut segments = Vec::new();
    let mut cur = String::new();
    let mut chars = cmd.chars().peekable();
    let mut in_single = false;
    let mut in_double = false;

    while let Some(ch) = chars.next() {
        if in_single {
            if ch == '\'' {
                in_single = false;
            }
            cur.push(ch);
            continue;
        }
        if in_double {
            if ch == '\\' {
                cur.push(ch);
                if let Some(next) = chars.next() {
                    cur.push(next);
                }
                continue;
            }
            if ch == '`' || (ch == '$' && chars.peek() == Some(&'(')) {
                return None;
            }
            if ch == '"' {
                in_double = false;
            }
            cur.push(ch);
            continue;
        }
        match ch {
            '\'' => {
                in_single = true;
                cur.push(ch);
            }
            '"' => {
                in_double = true;
                cur.push(ch);
            }
            '\\' => {
                cur.push(ch);
                if let Some(next) = chars.next() {
                    cur.push(next);
                }
            }
            ';' | '&' | '>' | '<' | '`' | '\n' => return None,
            '$' if chars.peek() == Some(&'(') => return None,
            '|' => {
                if chars.peek() == Some(&'|') {
                    return None;
                }
                let seg = cur.trim().to_owned();
                if seg.is_empty() {
                    return None;
                }
                segments.push(seg);
                cur.clear();
            }
            _ => cur.push(ch),
        }
    }

    if in_single || in_double {
        return None;
    }
    let last = cur.trim().to_owned();
    if !last.is_empty() {
        segments.push(last);
    }
    Some(segments)
}

fn split_shell_words(input: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut cur = String::new();
    let mut in_single = false;
    let mut in_double = false;
    let mut chars = input.chars().peekable();

    while let Some(ch) = chars.next() {
        if in_single {
            if ch == '\'' {
                in_single = false;
            } else {
                cur.push(ch);
            }
        } else if in_double {
            if ch == '\\' {
                if let Some(next) = chars.next() {
                    cur.push(next);
                }
            } else if ch == '"' {
                in_double = false;
            } else {
                cur.push(ch);
            }
        } else {
            match ch {
                '\'' => in_single = true,
                '"' => in_double = true,
                '\\' => {
                    if let Some(next) = chars.next() {
                        cur.push(next);
                    }
                }
                c if c.is_whitespace() => {
                    if !cur.is_empty() {
                        words.push(std::mem::take(&mut cur));
                    }
                }
                _ => cur.push(ch),
            }
        }
    }

    if !cur.is_empty() {
        words.push(cur);
    }
    words
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn safe_command_matcher_allows_read_only_pipes_and_rejects_mutations() {
        assert!(matches_safe_commands("ls -la | grep Cargo | head -n 5"));
        assert!(matches_safe_commands("git status"));
        assert!(matches_safe_commands("git diff --stat"));
        assert!(matches_safe_commands("aplan -d 'Read code' -i 'Edit file'"));
        assert!(matches_safe_commands("aplan clear"));
        assert!(matches_safe_commands(
            "aplan <<'EOF'\n- [x] Inspect auth\n- [~] Fix refresh $(safe literal)\n- [ ] Verify\nEOF"
        ));
        assert!(matches_safe_commands(
            "aplan <<EOF\n- [x] Inspect auth\n- [ ] Verify\nEOF"
        ));
        assert!(!matches_safe_commands(
            "aplan <<EOF\n- [x] $(rm -rf /)\nEOF"
        ));
        assert!(!matches_safe_commands(
            "aplan <<'EOF'\n- [x] ok\nEOF\nrm -rf /tmp/x"
        ));
        assert!(!matches_safe_commands("aplan <<'EOF'; rm -rf /tmp/x\nEOF"));
        assert!(!matches_safe_commands("rm -rf /tmp/x"));
        assert!(!matches_safe_commands("ls > out.txt"));
        assert!(!matches_safe_commands("echo $(whoami)"));
        assert!(!matches_safe_commands("ls && rm foo"));
    }

    #[test]
    fn policy_auto_decisions_match_ask_auto_plan() {
        let mut always = HashSet::new();
        always.insert("cargo".to_owned());

        assert!(matches!(
            try_auto_decide(ApprovalPolicy::Auto, &always, "rm -rf x"),
            Some(ApprovalResponseWire::Allow { .. })
        ));
        assert!(matches!(
            try_auto_decide(ApprovalPolicy::Plan, &always, "rm -rf x"),
            Some(ApprovalResponseWire::Deny { .. })
        ));
        assert!(matches!(
            try_auto_decide(ApprovalPolicy::Plan, &always, "git status"),
            Some(ApprovalResponseWire::Allow { .. })
        ));
        assert!(matches!(
            try_auto_decide(ApprovalPolicy::Ask, &always, "cargo test"),
            Some(ApprovalResponseWire::Allow { .. })
        ));
        assert!(try_auto_decide(ApprovalPolicy::Ask, &always, "rm -rf x").is_none());
    }
}
