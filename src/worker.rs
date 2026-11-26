//! Layer 3 supervisor driving `anix` v0.4.0 worker processes strictly through external POSIX
//! protocols: `--inbox`, `--trace-fd`, `-c`/`--leaf`, `anix fork`, `--notify`, and Maildir envelopes.

use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, BufReader, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::io::{FromRawFd, RawFd};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anix_proto::maildir::{MaildirEnvelope, MaildirHeaders};
use anix_proto::provider::TokenUsage;
use anix_proto::trace::{ContextWatermark, TraceEnvelope};
use serde_json::Value;
use uuid::Uuid;

use crate::session::ensure_dir_0700;

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
    },
    CycleStart {
        worker_id: String,
        cycle_id: u64,
        source: String,
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
    pub spec: WorkerSpec,
    child: Option<Child>,
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
    pub fn spawn(spec: WorkerSpec, tx: mpsc::Sender<WorkerEvent>) -> io::Result<Self> {
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

        // Spawn trace JSONL reader thread.
        let worker_id_trace = spec.id.clone();
        let tx_trace = tx.clone();
        // SAFETY: `trace_r` is exclusively owned by this thread.
        let trace_file = unsafe { File::from_raw_fd(trace_r) };
        thread::spawn(move || {
            let reader = BufReader::new(trace_file);
            for line_res in reader.lines() {
                let Ok(line) = line_res else { break };
                let trimmed = line.trim();
                if trimmed.is_empty() {
                    continue;
                }
                if let Some(ev) = parse_trace_line(&worker_id_trace, trimmed)
                    && tx_trace.send(ev).is_err()
                {
                    break;
                }
            }
        });

        // Spawn stderr reader thread for diagnostics & `$ANIX_ERR` output.
        if let Some(stderr_pipe) = child.stderr.take() {
            let worker_id_err = spec.id.clone();
            let tx_err = tx;
            thread::spawn(move || {
                let reader = BufReader::new(stderr_pipe);
                for line_res in reader.lines() {
                    let Ok(line) = line_res else { break };
                    if !line.trim().is_empty()
                        && tx_err
                            .send(WorkerEvent::StderrLine {
                                worker_id: worker_id_err.clone(),
                                line,
                            })
                            .is_err()
                    {
                        break;
                    }
                }
            });
        }

        Ok(Self {
            current_model: spec.model.clone().unwrap_or_default(),
            current_provider: spec.provider.clone().unwrap_or_default(),
            current_leaf: spec.leaf,
            spec,
            child: Some(child),
            pid,
            alive: true,
            busy: false,
            context: None,
            session_usage: TokenUsage::default(),
            holds: Vec::new(),
        })
    }

    /// Non-blocking check whether the child process has exited.
    pub fn poll_exit(&mut self) -> Option<i32> {
        let child = self.child.as_mut()?;
        match child.try_wait() {
            Ok(Some(st)) => {
                self.alive = false;
                self.busy = false;
                self.child = None;
                Some(st.code().unwrap_or(1))
            }
            _ => None,
        }
    }

    /// Deliver a user prompt atomically to this worker's Maildir inbox (`tmp -> fsync -> new`).
    pub fn send_prompt(&mut self, text: &str, from: &str) -> io::Result<String> {
        self.send_prompt_with_reply_to(text, from, None)
    }

    /// Deliver a prompt with an optional `Anix-Reply-To` Maildir path.
    pub fn send_prompt_with_reply_to(
        &mut self,
        text: &str,
        from: &str,
        reply_to: Option<&Path>,
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
        deliver_maildir_atomic(&self.spec.inbox_dir, &env)?;
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

    let encoded = envelope.format();
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
        }),
        "cycle_start" => Some(WorkerEvent::CycleStart {
            worker_id: wid,
            cycle_id: f.get("cycle_id").and_then(Value::as_u64).unwrap_or(0),
            source: str_field(f.get("source")),
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

fn shell_quote(s: &str) -> String {
    if !s.is_empty()
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'/' | b'.' | b'_' | b'-' | b':'))
    {
        return s.to_owned();
    }
    format!("'{}'", s.replace('\'', "'\\''"))
}
