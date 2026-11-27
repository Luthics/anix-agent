//! Layer 3 supervisor driving `anix` v0.4.0 worker processes strictly through external POSIX
//! protocols: `--inbox`, `--trace-fd`, `-c`/`--leaf`, `anix fork`, `--notify`, and Maildir envelopes.

use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, BufReader, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::io::{FromRawFd, RawFd};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, mpsc};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

static NEXT_WORKER_INSTANCE: AtomicU64 = AtomicU64::new(1);

use anix_proto::maildir::{MaildirEnvelope, MaildirHeaders};
use anix_proto::provider::TokenUsage;
use anix_proto::trace::{ContextWatermark, TraceEnvelope};
use serde::Deserialize;
use serde_json::Value;
use uuid::Uuid;

use crate::session::ensure_dir_0700;

/// Status of an ephemeral live progress checklist step published via `aplan` / `plan_update`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlanStepStatus {
    Pending,
    InProgress,
    Done,
    Skipped,
    Failed,
}

/// Single checklist item emitted by `anix-core` on `--trace-fd` as a `plan_update` event.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct PlanStep {
    pub title: String,
    pub status: PlanStepStatus,
}

/// Configuration used to launch or respawn a single persistent `anix --inbox` worker.
#[derive(Debug, Clone)]
pub struct WorkerSpec {
    pub id: String,
    pub anix_bin: PathBuf,
    pub agent_path: PathBuf,
    pub cwd: PathBuf,
    pub wal_path: PathBuf,
    pub inbox_dir: PathBuf,
    pub leaf: Option<u64>,
    pub config_file: Option<PathBuf>,
    pub provider: Option<String>,
    pub model: Option<String>,
    /// Logical reasoning effort tier shown in UI/status bar (e.g. `"off"`, `"high"`, `"xhigh"`).
    pub reasoning_effort: Option<String>,
    /// Resolved wire `--param KEY=VALUE` pairs for the active reasoning effort mapping.
    /// `Some(vec)` emits exactly those pairs (empty = explicit `null`/omit); `None` falls back to
    /// `--param reasoning_effort=<reasoning_effort>`.
    pub effort_params: Option<Vec<(String, Value)>>,
    /// Underlying shell binary wrapped by `approve-wrap` (`None` -> `msh`, `Some("none")` -> pure-text).
    pub base_shell: Option<String>,
    pub approve_sock: PathBuf,
    pub sandbox: Option<String>,
    /// Optional parent Maildir notified when a spawned background worker exits (`--notify`).
    pub notify_dir: Option<PathBuf>,
    /// Idle timeout in seconds (`0` = stay alive indefinitely; `>0` = exit & fire `--notify` when idle).
    pub idle_timeout_secs: u64,
    /// Optional explicit path to `anix-agent` binary for `approve-wrap` (defaults to `current_exe()`).
    pub self_exe: Option<PathBuf>,
}

/// Typed events streamed asynchronously from a running `anix` worker via `--trace-fd` & stderr.
#[derive(Debug, Clone)]
pub enum WorkerEvent {
    RunStart {
        worker_id: String,
        model: String,
        provider: String,
        shell: String,
        leaf_id: Option<u64>,
        session_usage: Option<TokenUsage>,
        context: Option<ContextWatermark>,
    },
    CycleStart {
        worker_id: String,
        cycle_id: u64,
        source: String,
    },
    InboxSteered {
        worker_id: String,
        count: u64,
    },
    ReasoningDelta {
        worker_id: String,
        text: String,
    },
    TextDelta {
        worker_id: String,
        text: String,
    },
    ToolStart {
        worker_id: String,
        tool_call_id: String,
        tool_name: String,
        cmd: String,
    },
    ToolOutput {
        worker_id: String,
        tool_call_id: String,
        raw_output: String,
    },
    ToolEnd {
        worker_id: String,
        tool_call_id: String,
        turn_id: u64,
        exit_code: i32,
        media_count: u64,
    },
    HoldStarted {
        worker_id: String,
        id: String,
        deadline_sec: u64,
        label: Option<String>,
    },
    HoldReleased {
        worker_id: String,
        id: String,
        reason: String,
    },
    HoldExpired {
        worker_id: String,
        id: String,
        label: Option<String>,
    },
    Compact {
        worker_id: String,
        reason: String,
        checkpoint_turn: u64,
        leaf_id: Option<u64>,
    },
    Fold {
        worker_id: String,
        from_turn: u64,
        to_turn: u64,
        summary: String,
    },
    PlanUpdate {
        worker_id: String,
        steps: Vec<PlanStep>,
    },
    CycleEnd {
        worker_id: String,
        cycle_id: u64,
        source: String,
        reason: String,
        exit_code: i32,
        turns: u32,
        cycle_usage: Option<TokenUsage>,
        session_usage: Option<TokenUsage>,
        context: Option<ContextWatermark>,
        leaf_id: Option<u64>,
    },
    Idle {
        worker_id: String,
        active_holds: u64,
    },
    ProviderError {
        worker_id: String,
        code: String,
        message: String,
    },
    Warning {
        worker_id: String,
        message: String,
    },
    StderrLine {
        worker_id: String,
        line: String,
    },
    Exited {
        worker_id: String,
        exit_code: i32,
    },
}

impl WorkerEvent {
    pub fn worker_id(&self) -> &str {
        match self {
            Self::RunStart { worker_id, .. }
            | Self::CycleStart { worker_id, .. }
            | Self::InboxSteered { worker_id, .. }
            | Self::ReasoningDelta { worker_id, .. }
            | Self::TextDelta { worker_id, .. }
            | Self::ToolStart { worker_id, .. }
            | Self::ToolOutput { worker_id, .. }
            | Self::ToolEnd { worker_id, .. }
            | Self::HoldStarted { worker_id, .. }
            | Self::HoldReleased { worker_id, .. }
            | Self::HoldExpired { worker_id, .. }
            | Self::Compact { worker_id, .. }
            | Self::Fold { worker_id, .. }
            | Self::PlanUpdate { worker_id, .. }
            | Self::CycleEnd { worker_id, .. }
            | Self::Idle { worker_id, .. }
            | Self::ProviderError { worker_id, .. }
            | Self::Warning { worker_id, .. }
            | Self::StderrLine { worker_id, .. }
            | Self::Exited { worker_id, .. } => worker_id,
        }
    }
}

/// Worker event tagged with the monotonic `instance_id` of the spawned `anix` process so the UI
/// can safely drop stale trailing events from replaced or intentionally stopped workers.
#[derive(Debug, Clone)]
pub struct WorkerMessage {
    pub instance_id: u64,
    pub event: WorkerEvent,
}

/// Active hold lease tracked by the UI for display in status bar and `/workers`.
#[derive(Debug, Clone)]
pub struct TrackedHold {
    pub id: String,
    pub label: Option<String>,
    pub deadline: Instant,
}

/// Running `anix --inbox` process handle owned by `anix-agent`.
#[derive(Debug)]
pub struct WorkerHandle {
    pub instance_id: u64,
    pub spec: WorkerSpec,
    child: Option<Child>,
    silenced: Arc<AtomicBool>,
    pub pid: u32,
    pub alive: bool,
    pub busy: bool,
    pub current_model: String,
    pub current_provider: String,
    pub current_leaf: Option<u64>,
    pub context: Option<ContextWatermark>,
    pub session_usage: TokenUsage,
    pub holds: Vec<TrackedHold>,
}

impl WorkerHandle {
    /// Spawn a persistent `anix --inbox` child attached to a private `--trace-fd 9` pipe.
    pub fn spawn(spec: WorkerSpec, tx: mpsc::Sender<WorkerMessage>) -> io::Result<Self> {
        ensure_maildir(&spec.inbox_dir)?;

        let (trace_r, trace_w) = create_pipe_cloexec()?;

        let mut cmd = Command::new(&spec.anix_bin);
        let idle_timeout_s = spec.idle_timeout_secs.to_string();
        cmd.current_dir(&spec.cwd)
            .args([
                "--inbox",
                &spec.inbox_dir.display().to_string(),
                "--idle-timeout",
                &idle_timeout_s,
                "--inbox-debounce-ms",
                "50",
                "--trace-fd",
                "9",
                "--stderr",
                "explicit",
                "-c",
                &spec.wal_path.display().to_string(),
            ])
            .env("ANIX_TRACE_LEVEL", "wire")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped());

        if let Some(leaf_id) = spec.leaf {
            cmd.args(["--leaf", &leaf_id.to_string()]);
        }
        if let Some(ref cfg) = spec.config_file {
            cmd.args(["--config", &cfg.display().to_string()]);
        }
        if let Some(ref prov) = spec.provider {
            cmd.args(["--provider", prov]);
        }
        if let Some(ref model) = spec.model {
            cmd.args(["--model", model]);
        }
        match spec.effort_params {
            Some(ref params) => {
                for (k, v) in params {
                    cmd.args(["--param", &format_cli_param(k, v)]);
                }
            }
            None => {
                if let Some(ref effort) = spec.reasoning_effort {
                    let eff = effort.trim();
                    if !eff.is_empty() && !eff.eq_ignore_ascii_case("default") {
                        cmd.args(["--param", &format!("reasoning_effort={eff}")]);
                    }
                }
            }
        }
        if let Some(ref sb) = spec.sandbox {
            cmd.arg(format!("--sandbox={sb}"));
        }
        if let Some(ref notify) = spec.notify_dir {
            cmd.args([
                "--notify",
                &notify.display().to_string(),
                "--notify-id",
                &spec.id,
            ]);
        }

        // Compose Layer 3 shell wrapper chain unless user requested `shell=none`.
        let base_shell = spec.base_shell.as_deref().unwrap_or("msh");
        if base_shell.eq_ignore_ascii_case("none") {
            cmd.args(["--shell", "none"]);
        } else {
            let self_exe = spec.self_exe.clone().unwrap_or_else(env_self_exe);
            let shell_chain = format!(
                "{} approve-wrap --ipc {} --worker-id {} -- {}",
                shell_quote(&self_exe.display().to_string()),
                shell_quote(&spec.approve_sock.display().to_string()),
                shell_quote(&spec.id),
                base_shell
            );
            cmd.args(["--shell", &shell_chain]);
        }

        cmd.arg(&spec.agent_path);

        // SAFETY: `pre_exec` invokes only async-signal-safe `dup2` / `fcntl` / `close`.
        // Isolates FDs `3..=8` so inherited host descriptors are never misread as `anix` extra input FDs.
        unsafe {
            cmd.pre_exec(move || {
                if trace_w != 9 {
                    if libc::dup2(trace_w, 9) < 0 {
                        return Err(io::Error::last_os_error());
                    }
                    if trace_w > 9 {
                        let _ = libc::close(trace_w);
                    }
                } else {
                    let flags = libc::fcntl(9, libc::F_GETFD);
                    if flags < 0 || libc::fcntl(9, libc::F_SETFD, flags & !libc::FD_CLOEXEC) < 0 {
                        return Err(io::Error::last_os_error());
                    }
                }
                if trace_r > 9 {
                    let _ = libc::close(trace_r);
                }
                for fd in 3..=8 {
                    let _ = libc::close(fd);
                }
                Ok(())
            });
        }

        let spawn_res = cmd.spawn();
        // Parent MUST close write end of trace pipe immediately so reader sees EOF on child exit.
        // SAFETY: `trace_w` is owned by parent.
        unsafe {
            libc::close(trace_w);
        }

        let mut child = match spawn_res {
            Ok(c) => c,
            Err(err) => {
                // SAFETY: `trace_r` owned by parent on error path.
                unsafe {
                    libc::close(trace_r);
                }
                return Err(err);
            }
        };
        let pid = child.id();
        let instance_id = NEXT_WORKER_INSTANCE.fetch_add(1, Ordering::Relaxed);
        let silenced = Arc::new(AtomicBool::new(false));

        // Spawn trace JSONL reader thread.
        let worker_id_trace = spec.id.clone();
        let tx_trace = tx.clone();
        let silenced_trace = Arc::clone(&silenced);
        // SAFETY: `trace_r` is exclusively owned by this thread.
        let trace_file = unsafe { File::from_raw_fd(trace_r) };
        thread::spawn(move || {
            let reader = BufReader::new(trace_file);
            for line_res in reader.lines() {
                if silenced_trace.load(Ordering::Acquire) {
                    break;
                }
                let Ok(line) = line_res else { break };
                if silenced_trace.load(Ordering::Acquire) {
                    break;
                }
                let trimmed = line.trim();
                if trimmed.is_empty() {
                    continue;
                }
                if let Some(event) = parse_trace_line(&worker_id_trace, trimmed)
                    && tx_trace.send(WorkerMessage { instance_id, event }).is_err()
                {
                    break;
                }
            }
        });

        // Spawn stderr reader thread for diagnostics & `$ANIX_ERR` output.
        if let Some(stderr_pipe) = child.stderr.take() {
            let worker_id_err = spec.id.clone();
            let tx_err = tx;
            let silenced_err = Arc::clone(&silenced);
            thread::spawn(move || {
                let reader = BufReader::new(stderr_pipe);
                for line_res in reader.lines() {
                    if silenced_err.load(Ordering::Acquire) {
                        break;
                    }
                    let Ok(line) = line_res else { break };
                    if silenced_err.load(Ordering::Acquire) {
                        break;
                    }
                    if !line.trim().is_empty()
                        && tx_err
                            .send(WorkerMessage {
                                instance_id,
                                event: WorkerEvent::StderrLine {
                                    worker_id: worker_id_err.clone(),
                                    line,
                                },
                            })
                            .is_err()
                    {
                        break;
                    }
                }
            });
        }

        Ok(Self {
            instance_id,
            current_model: spec.model.clone().unwrap_or_default(),
            current_provider: spec.provider.clone().unwrap_or_default(),
            current_leaf: spec.leaf,
            spec,
            child: Some(child),
            silenced,
            pid,
            alive: true,
            busy: false,
            context: None,
            session_usage: TokenUsage::default(),
            holds: Vec::new(),
        })
    }

    /// Non-blocking check whether the child process has exited unexpectedly.
    pub fn poll_exit(&mut self) -> Option<i32> {
        let child = self.child.as_mut()?;
        match child.try_wait() {
            Ok(Some(st)) => {
                let was_alive = self.alive;
                let was_silenced = self.silenced.load(Ordering::Acquire);
                self.alive = false;
                self.busy = false;
                self.child = None;
                if was_alive && !was_silenced {
                    Some(st.code().unwrap_or(1))
                } else {
                    None
                }
            }
            _ => None,
        }
    }

    /// Deliver a user prompt atomically to this worker's Maildir inbox (`tmp -> fsync -> new`).
    pub fn send_prompt(&mut self, text: &str, from: &str) -> io::Result<String> {
        self.send_prompt_with_media(text, from, &[])
    }

    /// Deliver a user prompt with optional local image attachments (`Anix-Media: <mime> <path>`).
    pub fn send_prompt_with_media(
        &mut self,
        text: &str,
        from: &str,
        media: &[(String, PathBuf)],
    ) -> io::Result<String> {
        self.send_prompt_with_reply_and_media(text, from, None, media)
    }

    /// Deliver a prompt with an optional `Anix-Reply-To` Maildir path.
    pub fn send_prompt_with_reply_to(
        &mut self,
        text: &str,
        from: &str,
        reply_to: Option<&Path>,
    ) -> io::Result<String> {
        self.send_prompt_with_reply_and_media(text, from, reply_to, &[])
    }

    fn send_prompt_with_reply_and_media(
        &mut self,
        text: &str,
        from: &str,
        reply_to: Option<&Path>,
        media: &[(String, PathBuf)],
    ) -> io::Result<String> {
        let msg_id = format!("ui_{}", Uuid::new_v4().simple());
        let env = MaildirEnvelope {
            headers: MaildirHeaders {
                message_id: Some(msg_id.clone()),
                reply_to: reply_to.map(|p| p.display().to_string()),
                in_reply_to: None,
                from: Some(from.to_owned()),
                control: None,
                exit_code: None,
            },
            body: text.to_owned(),
        };
        let encoded = if media.is_empty() {
            env.format()
        } else {
            let mut extra = String::new();
            for (mime, path) in media {
                extra.push_str("Anix-Media: ");
                extra.push_str(mime.trim());
                extra.push(' ');
                extra.push_str(&path.display().to_string().replace(['\r', '\n'], " "));
                extra.push('\n');
            }
            let base = env.format();
            if let Some((hdrs, body)) = base.split_once("\n\n") {
                format!("{hdrs}\n{extra}\n{body}")
            } else {
                format!("{extra}\n{base}")
            }
        };
        deliver_maildir_raw(&self.spec.inbox_dir, &encoded)?;
        self.busy = true;
        Ok(msg_id)
    }

    /// Trigger online context compaction via Maildir control header (`Anix-Control: compact`).
    pub fn send_compact(&mut self) -> io::Result<()> {
        let env = MaildirEnvelope {
            headers: MaildirHeaders {
                message_id: Some(format!("ctl_{}", Uuid::new_v4().simple())),
                reply_to: None,
                in_reply_to: None,
                from: Some("anix-agent".into()),
                control: Some("compact".into()),
                exit_code: None,
            },
            body: String::new(),
        };
        deliver_maildir_atomic(&self.spec.inbox_dir, &env)?;
        self.busy = true;
        Ok(())
    }

    /// Gracefully stop the worker (`SIGINT` or `SIGTERM` -> up to 3.5s grace -> `SIGKILL`).
    pub fn stop(&mut self, sig: libc::c_int) {
        // Silence trace/stderr forwarding BEFORE signaling so intentional restarts never leak a
        // trailing `run_end` (`exit=143`) into the UI or corrupt a newly spawned replacement.
        self.silenced.store(true, Ordering::SeqCst);
        let Some(mut child) = self.child.take() else {
            self.alive = false;
            self.busy = false;
            return;
        };
        let pid = child.id() as libc::pid_t;
        if pid > 0 {
            // SAFETY: Delivering signal to owned direct child PID.
            unsafe {
                libc::kill(pid, sig);
            }
        }
        let deadline = Instant::now() + Duration::from_millis(3500);
        loop {
            match child.try_wait() {
                Ok(Some(_)) => break,
                Ok(None) if Instant::now() < deadline => {
                    thread::sleep(Duration::from_millis(20));
                }
                _ => {
                    let _ = child.kill();
                    let _ = child.wait();
                    break;
                }
            }
        }
        self.alive = false;
        self.busy = false;
    }
}

impl Drop for WorkerHandle {
    fn drop(&mut self) {
        self.stop(libc::SIGTERM);
    }
}

/// Invoke `anix fork <src_wal> [--leaf <id>] -o <dst_wal>` via Layer 1 CLI.
pub fn run_anix_fork(
    anix_bin: &Path,
    src_wal: &Path,
    leaf: Option<u64>,
    dst_wal: &Path,
) -> io::Result<String> {
    let mut cmd = Command::new(anix_bin);
    cmd.arg("fork").arg(src_wal);
    if let Some(lid) = leaf {
        cmd.args(["--leaf", &lid.to_string()]);
    }
    cmd.arg("-o").arg(dst_wal);
    let out = cmd.output()?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_owned())
    } else {
        Err(io::Error::other(
            String::from_utf8_lossy(&out.stderr).trim().to_owned(),
        ))
    }
}

/// Atomically deliver a [`MaildirEnvelope`] into `<dir>/new/`.
pub fn deliver_maildir_atomic(dir: &Path, envelope: &MaildirEnvelope) -> io::Result<String> {
    deliver_maildir_raw(dir, &envelope.format())
}

fn deliver_maildir_raw(dir: &Path, encoded: &str) -> io::Result<String> {
    ensure_maildir(dir)?;
    let ts_ns = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let pid = std::process::id();
    let rand = &Uuid::new_v4().simple().to_string()[..12];
    let filename = format!("{ts_ns:020}-{pid}-{rand}.msg");
    let tmp_path = dir.join("tmp").join(&filename);
    let new_path = dir.join("new").join(&filename);

    let mut f = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&tmp_path)?;
    f.write_all(encoded.as_bytes())?;
    f.sync_data()?;
    drop(f);
    fs::rename(&tmp_path, &new_path).inspect_err(|_| {
        let _ = fs::remove_file(&tmp_path);
    })?;
    Ok(filename)
}

pub fn ensure_maildir(dir: &Path) -> io::Result<()> {
    ensure_dir_0700(dir)?;
    for sub in ["tmp", "new", "cur", "done"] {
        ensure_dir_0700(&dir.join(sub))?;
    }
    Ok(())
}

fn parse_trace_line(worker_id: &str, line: &str) -> Option<WorkerEvent> {
    let env: TraceEnvelope = serde_json::from_str(line).ok()?;
    let wid = worker_id.to_owned();
    let f = &env.fields;
    match env.event_type.as_str() {
        "run_start" => Some(WorkerEvent::RunStart {
            worker_id: wid,
            model: str_field(f.get("model")),
            provider: str_field(f.get("provider")),
            shell: str_field(f.get("shell")),
            leaf_id: f.get("leaf_id").and_then(Value::as_u64),
            session_usage: f
                .get("session_usage")
                .and_then(|v| serde_json::from_value::<TokenUsage>(v.clone()).ok()),
            context: f
                .get("context")
                .and_then(|v| serde_json::from_value::<ContextWatermark>(v.clone()).ok()),
        }),
        "cycle_start" => Some(WorkerEvent::CycleStart {
            worker_id: wid,
            cycle_id: f.get("cycle_id").and_then(Value::as_u64).unwrap_or(0),
            source: str_field(f.get("source")),
        }),
        "inbox_steered" => Some(WorkerEvent::InboxSteered {
            worker_id: wid,
            count: f.get("count").and_then(Value::as_u64).unwrap_or(1),
        }),
        "reasoning_delta" => Some(WorkerEvent::ReasoningDelta {
            worker_id: wid,
            text: str_field(f.get("text")),
        }),
        "text_delta" => Some(WorkerEvent::TextDelta {
            worker_id: wid,
            text: str_field(f.get("text")),
        }),
        "tool_start" => Some(WorkerEvent::ToolStart {
            worker_id: wid,
            tool_call_id: str_field(f.get("tool_call_id")),
            tool_name: str_field(f.get("tool_name")),
            cmd: str_field(f.get("cmd")),
        }),
        "wire_shell_output" => Some(WorkerEvent::ToolOutput {
            worker_id: wid,
            tool_call_id: str_field(f.get("tool_call_id")),
            raw_output: str_field(f.get("raw_output")),
        }),
        "tool_end" => Some(WorkerEvent::ToolEnd {
            worker_id: wid,
            tool_call_id: str_field(f.get("tool_call_id")),
            turn_id: f.get("turn_id").and_then(Value::as_u64).unwrap_or(0),
            exit_code: f.get("exit_code").and_then(Value::as_i64).unwrap_or(0) as i32,
            media_count: f.get("media_count").and_then(Value::as_u64).unwrap_or(0),
        }),
        "hold_started" => Some(WorkerEvent::HoldStarted {
            worker_id: wid,
            id: str_field(f.get("id")),
            deadline_sec: f.get("deadline_sec").and_then(Value::as_u64).unwrap_or(600),
            label: f
                .get("label")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned),
        }),
        "hold_released" => Some(WorkerEvent::HoldReleased {
            worker_id: wid,
            id: str_field(f.get("id")),
            reason: str_field(f.get("reason")),
        }),
        "hold_expired" => Some(WorkerEvent::HoldExpired {
            worker_id: wid,
            id: str_field(f.get("id")),
            label: f
                .get("label")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned),
        }),
        "compact" => Some(WorkerEvent::Compact {
            worker_id: wid,
            reason: str_field(f.get("reason")),
            checkpoint_turn: f
                .get("checkpoint_turn")
                .and_then(Value::as_u64)
                .unwrap_or(0),
            leaf_id: f.get("leaf_id").and_then(Value::as_u64),
        }),
        "fold" => Some(WorkerEvent::Fold {
            worker_id: wid,
            from_turn: f.get("from_turn").and_then(Value::as_u64).unwrap_or(0),
            to_turn: f.get("to_turn").and_then(Value::as_u64).unwrap_or(0),
            summary: str_field(f.get("summary")),
        }),
        "plan_update" => {
            let steps = f
                .get("steps")
                .and_then(|v| serde_json::from_value::<Vec<PlanStep>>(v.clone()).ok())
                .unwrap_or_default();
            Some(WorkerEvent::PlanUpdate {
                worker_id: wid,
                steps,
            })
        }
        "cycle_end" => Some(WorkerEvent::CycleEnd {
            worker_id: wid,
            cycle_id: f.get("cycle_id").and_then(Value::as_u64).unwrap_or(0),
            source: str_field(f.get("source")),
            reason: str_field(f.get("reason")),
            exit_code: f.get("exit_code").and_then(Value::as_i64).unwrap_or(0) as i32,
            turns: f.get("turns").and_then(Value::as_u64).unwrap_or(0) as u32,
            cycle_usage: f
                .get("cycle_usage")
                .and_then(|v| serde_json::from_value::<TokenUsage>(v.clone()).ok()),
            session_usage: f
                .get("session_usage")
                .and_then(|v| serde_json::from_value::<TokenUsage>(v.clone()).ok()),
            context: f
                .get("context")
                .and_then(|v| serde_json::from_value::<ContextWatermark>(v.clone()).ok()),
            leaf_id: f.get("leaf_id").and_then(Value::as_u64),
        }),
        "idle" => Some(WorkerEvent::Idle {
            worker_id: wid,
            active_holds: f.get("active_holds").and_then(Value::as_u64).unwrap_or(0),
        }),
        "provider_error" => Some(WorkerEvent::ProviderError {
            worker_id: wid,
            code: str_field(f.get("code")),
            message: str_field(f.get("message")),
        }),
        "warning" => Some(WorkerEvent::Warning {
            worker_id: wid,
            message: str_field(f.get("message")),
        }),
        "run_end" => Some(WorkerEvent::Exited {
            worker_id: wid,
            exit_code: f.get("exit_code").and_then(Value::as_i64).unwrap_or(0) as i32,
        }),
        _ => None,
    }
}

fn str_field(v: Option<&Value>) -> String {
    v.and_then(Value::as_str).unwrap_or("").to_owned()
}

fn create_pipe_cloexec() -> io::Result<(RawFd, RawFd)> {
    let mut fds = [0 as libc::c_int; 2];
    // SAFETY: `fds` is a valid 2-int buffer for `pipe(2)`.
    if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    for &fd in &fds {
        // SAFETY: Standard `fcntl(F_GETFD/F_SETFD)` on newly created pipe FDs.
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
        if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC) } < 0 {
            let err = io::Error::last_os_error();
            unsafe {
                libc::close(fds[0]);
                libc::close(fds[1]);
            }
            return Err(err);
        }
    }
    Ok((fds[0], fds[1]))
}

fn env_self_exe() -> PathBuf {
    std::env::current_exe().unwrap_or_else(|_| PathBuf::from("anix-agent"))
}

fn format_cli_param(key: &str, val: &Value) -> String {
    if let Value::String(s) = val
        && !s.is_empty()
        && !matches!(s.as_str(), "true" | "false" | "null")
        && !s.as_bytes()[0].is_ascii_digit()
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.'))
    {
        return format!("{key}={s}");
    }
    let encoded = serde_json::to_string(val).unwrap_or_else(|_| "null".into());
    format!("{key}={encoded}")
}

fn shell_quote(s: &str) -> String {
    if !s.is_empty()
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'/' | b'.' | b'_' | b'-' | b':'))
    {
        return s.to_owned();
    }
    format!("'{}'", s.replace('\'', "'\\''"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_ephemeral_plan_update_trace_event() {
        let raw = r#"{"v":1,"ts":"2026-11-28T00:00:00Z","pid":123,"ppid":100,"agent":"anix-agent","type":"plan_update","steps":[{"title":"Inspect","status":"done"},{"title":"Implement","status":"in_progress"},{"title":"Verify","status":"pending"}]}"#;
        let ev = parse_trace_line("main", raw).expect("parse plan_update");
        match ev {
            WorkerEvent::PlanUpdate { worker_id, steps } => {
                assert_eq!(worker_id, "main");
                assert_eq!(steps.len(), 3);
                assert_eq!(steps[0].status, PlanStepStatus::Done);
                assert_eq!(steps[1].title, "Implement");
                assert_eq!(steps[1].status, PlanStepStatus::InProgress);
                assert_eq!(steps[2].status, PlanStepStatus::Pending);
            }
            other => panic!("unexpected event: {other:?}"),
        }
    }
}
