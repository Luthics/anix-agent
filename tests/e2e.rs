//! End-to-end integration tests for `anix-agent` (Layer 3):
//! 1. Always-on `approve-wrap` + 0600 UDS `ApprovalServer` IPC tests (Allow, Edit, Deny exit 126,
//!    Fail-Closed exit 75).
//! 2. Full Layer 1 `anix` v0.4.0 integration test (streaming Trace over `--trace-fd 9`,
//!    `wire_shell_output`, live `approve-wrap` allow/edit/deny, `Anix-Control: compact`,
//!    `anix fork` + cross-file `WalGraph` / `load_recent_replay`, and parallel worker `--notify`
//!    Maildir wakeup into `main`).

use std::collections::HashSet;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use anix_agent::approve::{ApprovalResponseWire, ApprovalServer, PendingApproval, try_auto_decide};
use anix_agent::cli::ApprovalPolicy;
use anix_agent::wal_view::{WalGraph, load_recent_replay};
use anix_agent::worker::{WorkerEvent, WorkerHandle, WorkerSpec, ensure_maildir, run_anix_fork};

fn anix_agent_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_anix-agent"))
}

fn find_real_anix_bin() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("ANIX_BIN") {
        let pb = PathBuf::from(p);
        if pb.is_file() {
            return Some(pb);
        }
    }
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let workspace_candidates = [
        manifest_dir.join("../anix-core/target/debug/anix"),
        manifest_dir.join("../anix-core/target/release/anix"),
        manifest_dir.join("../../os-agent/target/debug/anix"),
        manifest_dir.join("../../os-agent/target/release/anix"),
    ];
    for cand in workspace_candidates {
        if cand.is_file() {
            return Some(cand);
        }
    }
    anix_agent::session::resolve_anix_bin(None).ok()
}

fn make_temp_dir(tag: &str) -> PathBuf {
    let dir = PathBuf::from(format!(
        "/tmp/ax-{tag}-{}",
        &uuid::Uuid::new_v4().simple().to_string()[..8]
    ));
    fs::create_dir_all(&dir).expect("create temp test dir");
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).expect("chmod 0700");
    dir
}

fn write_exec(path: &Path, content: &str) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).expect("create parent dir");
    }
    fs::write(path, content).expect("write script");
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).expect("chmod 0755");
}

#[test]
fn approve_wrap_ipc_allow_edit_deny_and_fail_closed() {
    let root = make_temp_dir("approve-wrap");
    let (req_tx, req_rx) = mpsc::channel::<PendingApproval>();
    let server = ApprovalServer::start(&root, move |pending| {
        let _ = req_tx.send(pending);
    })
    .expect("start UDS approval server");

    let sock_meta = fs::metadata(server.socket_path()).expect("socket stat");
    assert_eq!(
        sock_meta.permissions().mode() & 0o777,
        0o600,
        "approval UDS must be chmod 0600"
    );

    let bin = anix_agent_bin();
    let sock = server.socket_path().to_path_buf();

    // 1. Allow unchanged
    let bin1 = bin.clone();
    let sock1 = sock.clone();
    let h1 = std::thread::spawn(move || {
        Command::new(bin1)
            .args([
                "approve-wrap",
                "--ipc",
                &sock1.display().to_string(),
                "--worker-id",
                "w1",
                "--",
                "/bin/sh",
                "-c",
                "printf allow-ok",
            ])
            .output()
            .expect("run approve-wrap allow")
    });
    let pending = req_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("receive allow request");
    assert_eq!(pending.worker_id, "w1");
    assert_eq!(pending.cmd, "printf allow-ok");
    pending.allow("printf allow-ok".into());
    let out1 = h1.join().unwrap();
    assert!(out1.status.success());
    assert_eq!(String::from_utf8_lossy(&out1.stdout), "allow-ok");

    // 2. Edit command before exec
    let bin2 = bin.clone();
    let sock2 = sock.clone();
    let h2 = std::thread::spawn(move || {
        Command::new(bin2)
            .args([
                "approve-wrap",
                "--ipc",
                &sock2.display().to_string(),
                "--worker-id",
                "w1",
                "--",
                "/bin/sh",
                "-c",
                "printf original",
            ])
            .output()
            .expect("run approve-wrap edit")
    });
    let pending2 = req_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("receive edit request");
    pending2.allow("printf edited-by-human".into());
    let out2 = h2.join().unwrap();
    assert!(out2.status.success());
    assert_eq!(String::from_utf8_lossy(&out2.stdout), "edited-by-human");

    // 3. Soft deny back to model (exit 126)
    let bin3 = bin.clone();
    let sock3 = sock.clone();
    let h3 = std::thread::spawn(move || {
        Command::new(bin3)
            .args([
                "approve-wrap",
                "--ipc",
                &sock3.display().to_string(),
                "--worker-id",
                "w1",
                "--",
                "/bin/sh",
                "-c",
                "rm -rf /tmp/nope",
            ])
            .output()
            .expect("run approve-wrap deny")
    });
    let pending3 = req_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("receive deny request");
    pending3.deny("Blocked in safety review");
    let out3 = h3.join().unwrap();
    assert_eq!(out3.status.code(), Some(126));
    assert!(
        String::from_utf8_lossy(&out3.stdout).contains("Error: Blocked in safety review"),
        "expected Error reason on stdout, got: {}",
        String::from_utf8_lossy(&out3.stdout)
    );

    // 4. Fail-Closed (exit 75) when socket is missing/closed
    drop(server);
    let out4 = Command::new(&bin)
        .args([
            "approve-wrap",
            "--ipc",
            &sock.display().to_string(),
            "--worker-id",
            "w1",
            "--",
            "/bin/sh",
            "-c",
            "printf should-not-run",
        ])
        .output()
        .expect("run approve-wrap fail-closed");
    assert_eq!(out4.status.code(), Some(75));
    assert!(out4.stdout.is_empty());

    let _ = fs::remove_dir_all(&root);
}

#[test]
fn e2e_real_anix_streaming_approve_compact_fork_and_notify() {
    let Some(anix_bin) = find_real_anix_bin() else {
        eprintln!("skipping real-anix E2E test: no `anix` v0.4.0 binary found");
        return;
    };

    let root = make_temp_dir("real-anix");
    let work = root.join("work");
    fs::create_dir_all(&work).unwrap();
    let main_inbox = root.join("main_inbox");
    let w1_inbox = root.join("w1_inbox");
    ensure_maildir(&main_inbox).unwrap();
    ensure_maildir(&w1_inbox).unwrap();

    // Mock coprocess provider speaking Anix v0.4.0 wire protocol:
    // - When prompt mentions "RUN_TOOLS" and no tool result yet: emits reasoning + 3 tool calls
    //   (safe read-only, mutating to be edited, dangerous to be denied).
    // - When prompt mentions "WORKER_TASK": replies with a concise worker report.
    // - When compaction request arrives (system mentions "compact" or "summar"): replies with summary.
    // - Otherwise replies cleanly acknowledging tool results or maildir notification.
    let mock_prov = root.join("mock-provider.py");
    write_exec(
        &mock_prov,
        r#"#!/usr/bin/env python3
import json, sys

for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    req = json.loads(line)
    msgs = req.get("messages", [])
    full_text = "\n".join(str(m.get("text") or "") for m in msgs)
    has_tool = any(m.get("role") == "tool" for m in msgs)
    last_user = ""
    for m in reversed(msgs):
        if m.get("role") == "user":
            last_user = str(m.get("text") or "")
            break

    if "RUN_TOOLS" in last_user and not has_tool:
        events = [
            {"type": "reasoning_delta", "text": "Checking safe, editable, and denied commands."},
            {"type": "tool_call", "id": "tc_safe", "name": "shell", "arguments": {"cmd": "echo safe-readonly"}},
            {"type": "tool_call", "id": "tc_edit", "name": "shell", "arguments": {"cmd": "printf original > marker.txt"}},
            {"type": "tool_call", "id": "tc_deny", "name": "shell", "arguments": {"cmd": "rm -rf /forbidden"}},
            {"type": "usage", "input_tokens": 120, "output_tokens": 45, "cached_tokens": 0},
            {"type": "done", "finish_reason": "tool_calls"},
        ]
    elif "WORKER_TASK" in last_user:
        events = [
            {"type": "text_delta", "text": "Worker analysis complete: all green."},
            {"type": "usage", "input_tokens": 60, "output_tokens": 15, "cached_tokens": 0},
            {"type": "done", "finish_reason": "stop"},
        ]
    elif "Anix-In-Reply-To: w1" in last_user or "Worker analysis complete" in last_user:
        events = [
            {"type": "text_delta", "text": "Main received w1 completion via Maildir notify."},
            {"type": "usage", "input_tokens": 80, "output_tokens": 20, "cached_tokens": 0},
            {"type": "done", "finish_reason": "stop"},
        ]
    elif "summar" in full_text.lower() and len(msgs) <= 3:
        events = [
            {"type": "text_delta", "text": "Compact summary of session state."},
            {"type": "usage", "input_tokens": 40, "output_tokens": 10, "cached_tokens": 0},
            {"type": "done", "finish_reason": "stop"},
        ]
    else:
        events = [
            {"type": "text_delta", "text": "Turn finished cleanly."},
            {"type": "usage", "input_tokens": 140, "output_tokens": 30, "cached_tokens": 20},
            {"type": "done", "finish_reason": "stop"},
        ]

    for ev in events:
        sys.stdout.write(json.dumps(ev) + "\n")
    sys.stdout.flush()
"#,
    );

    let cfg_path = root.join("config.toml");
    fs::write(
        &cfg_path,
        format!(
            "[defaults]\nprovider = \"mock\"\nmodel = \"mock-v1\"\n\n[providers.mock]\nbin = \"{}\"\nmode = \"coprocess\"\napi_key_env = \"\"\n",
            mock_prov.display()
        ),
    )
    .unwrap();

    let agent_path = root.join("test.agent");
    write_exec(
        &agent_path,
        "#!/usr/bin/env anix\n---\nname: e2e-agent\noutput: reply\nstderr: explicit\nmax_turns: 8\n---\nYou are an E2E test agent.\n",
    );

    // Start ApprovalServer routing decisions automatically for safe cmds, editing tc_edit, denying tc_deny!
    let (ev_tx, ev_rx) = mpsc::channel::<WorkerEvent>();
    let server = ApprovalServer::start(&root, move |pending| {
        let always = HashSet::new();
        if let Some(auto) = try_auto_decide(ApprovalPolicy::Ask, &always, &pending.cmd) {
            match auto {
                ApprovalResponseWire::Allow { cmd } => pending.allow(cmd),
                ApprovalResponseWire::Deny { reason } => pending.deny(reason),
                ApprovalResponseWire::Abort { reason } => pending.abort(reason),
            }
            return;
        }
        if pending.cmd.contains("marker.txt") {
            pending.allow("printf edited-by-approval > marker.txt".into());
        } else if pending.cmd.contains("/forbidden") {
            pending.deny("Blocked dangerous rm");
        } else {
            let cmd = pending.cmd.clone();
            pending.allow(cmd);
        }
    })
    .expect("start ApprovalServer");

    let main_wal = root.join("main.wal.jsonl");
    let mut main_worker = WorkerHandle::spawn(
        WorkerSpec {
            id: "main".into(),
            anix_bin: anix_bin.clone(),
            agent_path: agent_path.clone(),
            cwd: work.clone(),
            wal_path: main_wal.clone(),
            inbox_dir: main_inbox.clone(),
            leaf: None,
            config_file: Some(cfg_path.clone()),
            provider: Some("mock".into()),
            model: Some("mock-v1".into()),
            base_shell: Some("/bin/sh".into()),
            approve_sock: server.socket_path().to_path_buf(),
            sandbox: None,
            notify_dir: None,
            idle_timeout_secs: 0,
            self_exe: Some(anix_agent_bin()),
        },
        ev_tx.clone(),
    )
    .expect("spawn main worker");

    main_worker
        .send_prompt("RUN_TOOLS please", "user")
        .expect("send RUN_TOOLS prompt");

    let mut saw_reasoning = false;
    let mut saw_text = String::new();
    let mut saw_wire_output = false;
    let mut deny_exit_code = None;
    let mut main_leaf: Option<u64> = None;

    let deadline = Instant::now() + Duration::from_secs(20);
    while Instant::now() < deadline {
        if let Ok(ev) = ev_rx.recv_timeout(Duration::from_millis(200)) {
            match ev {
                WorkerEvent::ReasoningDelta { text, .. } if !text.is_empty() => {
                    saw_reasoning = true;
                }
                WorkerEvent::TextDelta { worker_id, text } if worker_id == "main" => {
                    saw_text.push_str(&text);
                }
                WorkerEvent::ToolOutput { raw_output, .. } => {
                    if raw_output.contains("safe-readonly") {
                        saw_wire_output = true;
                    }
                }
                WorkerEvent::ToolEnd {
                    tool_call_id,
                    exit_code,
                    ..
                } => {
                    if tool_call_id == "tc_deny" {
                        deny_exit_code = Some(exit_code);
                    }
                }
                WorkerEvent::CycleEnd {
                    worker_id, leaf_id, ..
                } if worker_id == "main" => {
                    main_leaf = leaf_id;
                    break;
                }
                WorkerEvent::StderrLine { line, .. } => {
                    eprintln!("[anix stderr] {line}");
                }
                _ => {}
            }
        }
    }

    assert!(saw_reasoning, "expected reasoning_delta from --trace-fd 9");
    assert!(
        saw_wire_output,
        "expected wire_shell_output with safe-readonly over --trace-fd 9"
    );
    assert_eq!(
        deny_exit_code,
        Some(126),
        "expected denied shell tool to return exit code 126"
    );
    assert!(
        saw_text.contains("Turn finished cleanly."),
        "expected final assistant reply, got: {saw_text:?}"
    );
    assert_eq!(
        fs::read_to_string(work.join("marker.txt")).expect("marker.txt created by edited cmd"),
        "edited-by-approval"
    );
    assert!(main_leaf.is_some(), "expected CycleEnd leaf_id");

    // Trigger online `Anix-Control: compact` and verify Compact trace event!
    main_worker.send_compact().expect("send compact");
    let mut saw_compact = false;
    let compact_deadline = Instant::now() + Duration::from_secs(15);
    while Instant::now() < compact_deadline {
        if let Ok(ev) = ev_rx.recv_timeout(Duration::from_millis(200)) {
            match ev {
                WorkerEvent::Compact {
                    worker_id, leaf_id, ..
                } if worker_id == "main" => {
                    saw_compact = true;
                    if leaf_id.is_some() {
                        main_leaf = leaf_id;
                    }
                    break;
                }
                WorkerEvent::StderrLine { line, .. } => eprintln!("[compact stderr] {line}"),
                _ => {}
            }
        }
    }
    assert!(
        saw_compact,
        "expected Compact event after Anix-Control: compact"
    );

    // Fork WAL via `anix fork` & verify WalGraph + cross-file load_recent_replay!
    let forked_wal = root.join("forked.wal.jsonl");
    run_anix_fork(&anix_bin, &main_wal, main_leaf, &forked_wal).expect("anix fork succeeds");
    let graph = WalGraph::load(&forked_wal, None).expect("load forked WalGraph");
    assert!(
        graph.branch_base.is_some(),
        "forked WAL must carry branch_base pointer"
    );
    let replay = load_recent_replay(&forked_wal, None, 10);
    assert!(
        !replay.is_empty(),
        "load_recent_replay should traverse branch_base cleanly"
    );

    // Spawn parallel worker `w1` on forked WAL with `--notify <main_inbox>` and `--idle-timeout 1`!
    let mut w1 = WorkerHandle::spawn(
        WorkerSpec {
            id: "w1".into(),
            anix_bin: anix_bin.clone(),
            agent_path: agent_path.clone(),
            cwd: work.clone(),
            wal_path: forked_wal.clone(),
            inbox_dir: w1_inbox.clone(),
            leaf: None,
            config_file: Some(cfg_path.clone()),
            provider: Some("mock".into()),
            model: Some("mock-v1".into()),
            base_shell: Some("/bin/sh".into()),
            approve_sock: server.socket_path().to_path_buf(),
            sandbox: None,
            notify_dir: Some(main_inbox.clone()),
            idle_timeout_secs: 1,
            self_exe: Some(anix_agent_bin()),
        },
        ev_tx.clone(),
    )
    .expect("spawn parallel worker w1");

    w1.send_prompt("WORKER_TASK check repo", "main")
        .expect("deliver prompt to w1");

    let mut main_notified_reply = String::new();
    let notify_deadline = Instant::now() + Duration::from_secs(20);
    while Instant::now() < notify_deadline {
        if let Ok(ev) = ev_rx.recv_timeout(Duration::from_millis(200)) {
            match ev {
                WorkerEvent::TextDelta { worker_id, text } if worker_id == "main" => {
                    main_notified_reply.push_str(&text);
                }
                WorkerEvent::CycleEnd { worker_id, .. }
                    if worker_id == "main" && !main_notified_reply.is_empty() =>
                {
                    break;
                }
                _ => {}
            }
        }
    }
    assert!(
        main_notified_reply.contains("Main received w1 completion via Maildir notify"),
        "expected main to wake via --notify Maildir callback, got: {main_notified_reply:?}"
    );

    w1.stop(libc::SIGTERM);
    main_worker.stop(libc::SIGTERM);
    drop(server);
    let _ = fs::remove_dir_all(&root);
}
