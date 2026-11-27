//! Main Layer 3 Inline TUI workbench (`DynamicInlineTerminal` + `insert_before`),
//! driving persistent `anix` workers via Maildir, `--trace-fd 9`, and UDS `approve-wrap`.

mod commands;
mod overlays;
mod render;
mod terminal;
mod util;

use std::collections::{BTreeMap, HashSet, VecDeque};
use std::error::Error;
use std::io;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::layout::Size;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Widget};

use crate::approve::{
    ApprovalResponseWire, ApprovalServer, PendingApproval, extract_argv0, try_auto_decide,
};
use crate::cli::{ApprovalPolicy, Cli};
use crate::config::{AgentConfig, BangMode, ResolvedEffortProfile, normalize_effort_input};
use crate::import::{
    ExternalSessionSummary, ImportSource, discover_all_sessions, load_external_session,
    write_imported_wal,
};
use crate::input::{
    BuiltinSlashCommand, COMPLETION_VISIBLE_ROWS, CompletionContext, CompletionMenu, InputAction,
    InputBox, SLASH_COMMAND_SPECS, resolve_builtin_slash,
};
use crate::markdown::{
    MarkdownStreamer, diff_line_style, extract_last_code_block, looks_like_unified_diff,
    render_diff_line, render_markdown, wrap_prefixed_lines,
};
use crate::session::{
    CustomSlashEntry, CustomSlashKind, HistoryEntry, SessionMeta, SessionStore, SessionSummary,
    format_relative_time, now_rfc3339, resolve_anix_bin, save_session_meta,
};
use crate::statusbar::{
    AVAILABLE_VARS, DEFAULT_STATUS_BAR, PRESET_FULL, PRESET_MINIMAL, StatusVars, detect_git_branch,
    render_status_bar, resolve_preset,
};
use crate::wal_view::{
    ContextBreakdown, DetailEntry, ExportMetadata, RewindPoint, WalGraph, analyze_wal_context,
    export_wal_to_markdown, load_last_assistant_reply, load_recent_replay, load_wal_detail_entries,
    load_wal_usage_summary,
};
use crate::worker::{
    PlanStep, PlanStepStatus, TrackedHold, WorkerEvent, WorkerHandle, WorkerMessage, WorkerSpec,
    ensure_maildir, run_anix_fork,
};

use overlays::{
    ConfigPicker, ConfigTab, DiffViewer, HistorySearchPicker, HookState, ImportPicker, ImportTab,
    MemoryEntryKind, MemoryPicker, ModelPicker, Overlay, ResumeMode, ResumePicker, ResumeTab,
    RewindPicker, TranscriptViewer,
};
use terminal::{DynamicInlineTerminal, RenderTarget, TerminalGuard, UiTerminal, clear_tty_flusho};
use util::{
    anix_config_dir, anix_hooks_pre_context_dir, anix_memory_dir, capture_clipboard_image,
    fmt_duration, fmt_k, pad_display_width, parse_memory_frontmatter_type, read_clipboard_text,
    resolve_anix_hook_bin, resolve_shared_project_root, scan_workspace_files, strip_frontmatter,
    synthesize_untracked_diff, tail_preview, truncate_single_line, write_clipboard_text,
};

const RESUME_PAGE_SIZE: usize = 6;
const SPINNER_FRAMES: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

struct BangResult {
    cmd: String,
    exit_code: i32,
    output: String,
    override_mode: Option<BangMode>,
}

enum AppEvent {
    Key(KeyEvent),
    Paste(String),
    Resize,
    Tick,
    Worker(WorkerMessage),
    Approval(PendingApproval),
    BangDone(BangResult),
}

#[derive(Debug, Clone)]
struct ToolRunState {
    id: String,
    name: String,
    cmd: String,
    output: Option<String>,
    exit_code: Option<i32>,
    turn_id: Option<u64>,
    started_at: Instant,
    duration: Option<Duration>,
}

struct WorkerUiState {
    handle: WorkerHandle,
    streaming_reasoning: String,
    text_streamer: MarkdownStreamer,
    turn_reply_buf: String,
    last_assistant_reply: String,
    active_tools: Vec<ToolRunState>,
    plan_steps: Vec<PlanStep>,
    awaiting_user_cycle: bool,
    busy_since: Option<Instant>,
    queued_steering: Vec<String>,
    detail_log: Vec<DetailEntry>,
}

impl WorkerUiState {
    fn new(mut handle: WorkerHandle) -> Self {
        // Pre-seed resumed session stats directly from WAL so status bar & `/context` reflect
        // prior usage immediately on frame 1 (overridden authoritatively once `RunStart` arrives).
        let (wal_usage, wal_ctx) =
            load_wal_usage_summary(&handle.spec.wal_path, handle.current_leaf);
        if wal_usage.input_tokens > 0 || wal_usage.output_tokens > 0 {
            handle.session_usage = wal_usage;
        }
        if let Some(tokens) = wal_ctx {
            handle.context = Some(anix_proto::trace::ContextWatermark {
                tokens,
                max_tokens: 256_000,
            });
        }
        Self {
            handle,
            streaming_reasoning: String::new(),
            text_streamer: MarkdownStreamer::new(),
            turn_reply_buf: String::new(),
            last_assistant_reply: String::new(),
            active_tools: Vec::new(),
            plan_steps: Vec::new(),
            awaiting_user_cycle: false,
            busy_since: None,
            queued_steering: Vec::new(),
            detail_log: Vec::new(),
        }
    }

    fn push_detail(&mut self, entry: DetailEntry) {
        self.detail_log.push(entry);
        if self.detail_log.len() > 60 {
            self.detail_log.drain(0..self.detail_log.len() - 60);
        }
    }
}

enum ApprovalSubMode {
    Choose,
    DenyReason { buf: String },
    EditCmd { buf: String },
}

struct ActiveApproval {
    pending: PendingApproval,
    submode: ApprovalSubMode,
}

struct Workbench {
    store: SessionStore,
    anix_bin: PathBuf,
    config_file: Option<PathBuf>,
    base_shell: Option<String>,
    sandbox: Option<String>,
    approval_server: ApprovalServer,
    worker_event_tx: mpsc::Sender<WorkerMessage>,
    app_tx: mpsc::Sender<AppEvent>,

    agent_config_path: PathBuf,
    agent_config: AgentConfig,

    session_dir: PathBuf,
    meta: SessionMeta,
    /// False for a fresh in-memory draft session until the user actually sends their first message.
    session_persisted: bool,
    /// Last persisted session `(id, title)` cleared via `/clear` or `/new`, shown at the top as `/resume <id>`.
    last_cleared_session: Option<(String, String)>,
    draft_model: String,
    draft_provider: String,
    draft_max_tokens: u32,
    policy: ApprovalPolicy,
    always_allowed: HashSet<String>,
    expand_details: bool,

    workers: BTreeMap<String, WorkerUiState>,
    focused_worker: String,
    next_worker_seq: u32,

    input: InputBox,
    history_entries: Vec<HistoryEntry>,
    stashed_prompt: Option<String>,
    last_esc_at: Option<Instant>,
    reader_paused: Arc<AtomicBool>,
    custom_commands: Vec<CustomSlashEntry>,
    hint_line: Option<String>,
    running_bang_cmd: Option<String>,
    pending_bang_context: Vec<String>,
    approval_queue: VecDeque<PendingApproval>,
    active_approval: Option<ActiveApproval>,
    overlay: Overlay,

    spinner_idx: usize,
    git_branch: Option<String>,
    last_git_check: Instant,
    workspace_files: Vec<String>,
    workspace_files_refreshed: Option<Instant>,
    should_quit: bool,
}

/// Boot and run the interactive `anix-agent` Inline TUI workbench.
pub fn run_interactive(cli: Cli) -> Result<(), Box<dyn Error>> {
    let cwd = std::env::current_dir()?;
    let store = SessionStore::open(&cwd)?;
    let anix_bin = resolve_anix_bin(cli.anix_bin.as_deref())?;
    let (agent_config_path, agent_config) = AgentConfig::load_or_init();
    let effective_mode = cli.mode.unwrap_or(agent_config.default_mode);
    let effective_resume = cli.resume.clone().or_else(|| {
        if agent_config.auto_resume_last {
            Some("latest".to_owned())
        } else {
            None
        }
    });

    let (session_dir, mut meta, is_resumed) = if let Some(ref query) = effective_resume {
        match store.find_session(query) {
            Some((dir, mut m)) => {
                if let Some(ref a) = cli.agent {
                    m.agent_path = store.resolve_agent_path(Some(a))?;
                }
                if let Some(ref model) = cli.model {
                    m.model = Some(model.clone());
                }
                if let Some(ref prov) = cli.provider {
                    m.provider = Some(prov.clone());
                }
                if let Some(explicit_mode) = cli.mode {
                    m.approval_policy = explicit_mode;
                }
                (dir, m, true)
            }
            None => {
                let agent_path = store.resolve_agent_path(cli.agent.as_deref())?;
                let (dir, m) = store.draft_session(
                    agent_path,
                    cli.model.clone(),
                    cli.provider.clone(),
                    effective_mode,
                );
                (dir, m, false)
            }
        }
    } else {
        let agent_path = store.resolve_agent_path(cli.agent.as_deref())?;
        let (dir, m) = store.draft_session(
            agent_path,
            cli.model.clone(),
            cli.provider.clone(),
            effective_mode,
        );
        (dir, m, false)
    };

    if effective_mode != meta.approval_policy && !is_resumed {
        meta.approval_policy = effective_mode;
    }

    let (app_tx, app_rx) = mpsc::channel::<AppEvent>();
    let (worker_tx, worker_rx) = mpsc::channel::<WorkerMessage>();

    // Forward all worker trace/stderr events into the unified event loop.
    let tx_w = app_tx.clone();
    thread::spawn(move || {
        while let Ok(ev) = worker_rx.recv() {
            if tx_w.send(AppEvent::Worker(ev)).is_err() {
                break;
            }
        }
    });

    // Start session-scoped 0600 UDS approval server.
    let tx_a = app_tx.clone();
    let approval_server = ApprovalServer::start(&store.runtime_dir, move |pending| {
        let _ = tx_a.send(AppEvent::Approval(pending));
    })?;

    // Enter raw mode & initialize dynamic-height inline viewport (NO alternate screen hijack!).
    let _guard = TerminalGuard::enter()?;
    let mut terminal = DynamicInlineTerminal::enter()?;

    // Start dedicated crossterm keyboard + resize + tick thread (pausable for `$EDITOR` / `Ctrl+Z`).
    let stop_flag = Arc::new(AtomicBool::new(false));
    let reader_paused = Arc::new(AtomicBool::new(false));
    let stop_reader = Arc::clone(&stop_flag);
    let paused_reader = Arc::clone(&reader_paused);
    let tx_term = app_tx.clone();
    let reader_handle = thread::spawn(move || {
        let mut last_tick = Instant::now();
        while !stop_reader.load(Ordering::Relaxed) {
            if paused_reader.load(Ordering::Acquire) {
                thread::sleep(Duration::from_millis(25));
                continue;
            }
            if event::poll(Duration::from_millis(60)).unwrap_or(false) {
                if paused_reader.load(Ordering::Acquire) {
                    continue;
                }
                match event::read() {
                    Ok(Event::Key(k)) if k.kind != KeyEventKind::Release => {
                        if tx_term.send(AppEvent::Key(k)).is_err() {
                            break;
                        }
                    }
                    Ok(Event::Paste(text)) => {
                        if tx_term.send(AppEvent::Paste(text)).is_err() {
                            break;
                        }
                    }
                    Ok(Event::Resize(_, _)) if tx_term.send(AppEvent::Resize).is_err() => {
                        break;
                    }
                    _ => {}
                }
            }
            if last_tick.elapsed() >= Duration::from_millis(120) {
                last_tick = Instant::now();
                if tx_term.send(AppEvent::Tick).is_err() {
                    break;
                }
            }
        }
    });

    let policy = meta.approval_policy;
    let expand_details = agent_config.expand_details;
    let always_allowed: HashSet<String> = agent_config
        .always_allowed_commands
        .iter()
        .cloned()
        .collect();
    let git_branch = detect_git_branch(&store.cwd);
    let custom_commands = store.discover_custom_slash_entries();
    let (cfg_model, cfg_provider, draft_max_tokens) =
        store.preview_agent_defaults(&meta.agent_path, cli.config.as_deref());
    let draft_model = meta
        .model
        .clone()
        .or(cfg_model)
        .unwrap_or_else(|| "default".into());
    let draft_provider = meta
        .provider
        .clone()
        .or(cfg_provider)
        .unwrap_or_else(|| "-".into());
    let history_entries = store.load_input_history(500);
    let mut input = InputBox::new();
    input.load_history(history_entries.iter().map(|e| e.text.clone()).collect());
    let mut app = Workbench {
        store,
        anix_bin,
        config_file: cli.config,
        base_shell: cli.shell,
        sandbox: cli.sandbox,
        approval_server,
        worker_event_tx: worker_tx,
        app_tx,
        agent_config_path,
        agent_config,
        session_dir,
        meta,
        session_persisted: is_resumed,
        last_cleared_session: None,
        draft_model,
        draft_provider,
        draft_max_tokens,
        policy,
        always_allowed,
        expand_details,
        workers: BTreeMap::new(),
        focused_worker: "main".into(),
        next_worker_seq: 1,
        input,
        history_entries,
        stashed_prompt: None,
        last_esc_at: None,
        reader_paused,
        custom_commands,
        hint_line: None,
        running_bang_cmd: None,
        pending_bang_context: Vec::new(),
        approval_queue: VecDeque::new(),
        active_approval: None,
        overlay: Overlay::None,
        spinner_idx: 0,
        git_branch,
        last_git_check: Instant::now(),
        workspace_files: Vec::new(),
        workspace_files_refreshed: None,
        should_quit: false,
    };

    app.print_banner(&mut terminal, is_resumed)?;
    if is_resumed {
        app.replay_active_wal(&mut terminal)?;
        app.spawn_main_worker(&mut terminal)?;
    }

    if let Some(initial_prompt) = cli.prompt
        && !initial_prompt.trim().is_empty()
    {
        app.submit_user_input(
            &mut terminal,
            initial_prompt.clone(),
            initial_prompt,
            Vec::new(),
        )?;
    }

    let init_sz = terminal.size().unwrap_or(Size::new(80, 24));
    terminal.draw_dynamic(app.desired_viewport_height(init_sz), |f| app.render(f))?;
    app.sync_window_title(&mut terminal);

    while !app.should_quit {
        let Ok(ev) = app_rx.recv() else { break };
        match ev {
            AppEvent::Key(key) => {
                app.handle_key(&mut terminal, key)?;
            }
            AppEvent::Paste(raw) => {
                app.handle_paste(&mut terminal, raw);
            }
            AppEvent::Resize => {
                terminal.on_resize();
            }
            AppEvent::Tick => {
                app.on_tick(&mut terminal)?;
            }
            AppEvent::Worker(wev) => {
                app.handle_worker_event(&mut terminal, wev)?;
            }
            AppEvent::Approval(pending) => {
                app.handle_incoming_approval(&mut terminal, pending)?;
            }
            AppEvent::BangDone(res) => {
                app.handle_bang_done(&mut terminal, res)?;
            }
        }
        // Drain any immediately queued events before redrawing to keep streaming ultra-smooth.
        while !app.should_quit {
            match app_rx.try_recv() {
                Ok(AppEvent::Key(k)) => app.handle_key(&mut terminal, k)?,
                Ok(AppEvent::Paste(raw)) => app.handle_paste(&mut terminal, raw),
                Ok(AppEvent::Resize) => terminal.on_resize(),
                Ok(AppEvent::Tick) => app.on_tick(&mut terminal)?,
                Ok(AppEvent::Worker(w)) => app.handle_worker_event(&mut terminal, w)?,
                Ok(AppEvent::Approval(a)) => app.handle_incoming_approval(&mut terminal, a)?,
                Ok(AppEvent::BangDone(r)) => app.handle_bang_done(&mut terminal, r)?,
                Err(_) => break,
            }
        }
        if !app.should_quit {
            let cur_sz = terminal.size().unwrap_or(Size::new(80, 24));
            let _ = terminal.begin_sync_update();
            let frame_res = app.commit_ready_stream(&mut terminal).and_then(|()| {
                terminal.draw_dynamic(app.desired_viewport_height(cur_sz), |f| app.render(f))
            });
            let _ = terminal.end_sync_update();
            frame_res?;
            app.sync_window_title(&mut terminal);
        }
    }

    stop_flag.store(true, Ordering::Relaxed);
    app.shutdown_all_workers();
    let _ = reader_handle.join();
    let _ = terminal.finish();
    Ok(())
}

impl Workbench {
    fn sync_window_title(&self, terminal: &mut UiTerminal) {
        let dir = self
            .store
            .cwd
            .file_name()
            .and_then(|s| s.to_str())
            .filter(|s| !s.is_empty())
            .unwrap_or("anix");
        let is_busy = self.running_bang_cmd.is_some()
            || self
                .workers
                .get(&self.focused_worker)
                .is_some_and(|w| w.handle.busy);
        let title = if self.active_approval.is_some() {
            format!("⚠ anix-agent [approve] — {dir}")
        } else if is_busy {
            format!("▶ anix-agent — {dir}")
        } else {
            format!("anix-agent — {dir}")
        };
        terminal.set_window_title(&title);
    }

    fn term_width(&self, terminal: &UiTerminal) -> usize {
        terminal
            .size()
            .map(|s| (s.width as usize).saturating_sub(1))
            .unwrap_or(79)
            .max(30)
    }

    fn push_lines(&self, terminal: &mut UiTerminal, lines: Vec<Line<'static>>) -> io::Result<()> {
        if lines.is_empty() {
            return Ok(());
        }
        let max_chunk = terminal
            .size()
            .map(|s| s.height.saturating_sub(2).max(1) as usize)
            .unwrap_or(20);
        for chunk in lines.chunks(max_chunk) {
            let chunk_vec = chunk.to_vec();
            let h = chunk_vec.len() as u16;
            terminal.insert_before(h, |buf| {
                Paragraph::new(chunk_vec).render(buf.area, buf);
            })?;
        }
        Ok(())
    }

    fn push_notice(
        &self,
        terminal: &mut UiTerminal,
        color: Color,
        text: impl AsRef<str>,
    ) -> io::Result<()> {
        let width = self.term_width(terminal);
        let lines = wrap_prefixed_lines(
            text.as_ref(),
            "  ",
            Style::default().fg(color),
            Style::default().fg(color),
            width,
        );
        self.push_lines(terminal, lines)
    }

    fn print_banner(&self, terminal: &mut UiTerminal, resumed: bool) -> io::Result<()> {
        let agent_name = self
            .meta
            .agent_path
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_else(|| "default.agent".into());
        let tag = if resumed { "resumed" } else { "new" };
        let header = Line::from(vec![
            Span::styled(
                "◈ anix-agent ",
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!("v{} ", env!("CARGO_PKG_VERSION")),
                Style::default().fg(Color::DarkGray),
            ),
            Span::styled(
                format!("({tag} {}) ", self.meta.id),
                Style::default().fg(Color::Green),
            ),
            Span::styled(
                format!("agent:{agent_name} "),
                Style::default().fg(Color::White),
            ),
            Span::styled(
                format!("cwd:{}", self.store.cwd.display()),
                Style::default().fg(Color::DarkGray),
            ),
        ]);
        self.push_lines(terminal, vec![header, Line::default()])
    }

    fn replay_active_wal(&self, terminal: &mut UiTerminal) -> io::Result<()> {
        let turns = load_recent_replay(
            &self.meta.active_wal,
            self.meta.active_leaf,
            self.agent_config.replay_turns,
        );
        if turns.is_empty() {
            return Ok(());
        }
        let width = self.term_width(terminal);
        let mut lines = vec![Line::from(Span::styled(
            format!("─── resumed last {} turns ───", turns.len()),
            Style::default().fg(Color::DarkGray),
        ))];
        for turn in turns {
            match turn.role.as_str() {
                "user" => {
                    lines.extend(wrap_prefixed_lines(
                        &turn.text,
                        "› ",
                        Style::default()
                            .fg(Color::Green)
                            .add_modifier(Modifier::BOLD),
                        Style::default().fg(Color::Gray),
                        width,
                    ));
                }
                "assistant" => {
                    lines.extend(render_markdown(&turn.text, width.saturating_sub(2)));
                }
                other => {
                    lines.extend(wrap_prefixed_lines(
                        &turn.text,
                        &format!("[{other}] "),
                        Style::default().fg(Color::DarkGray),
                        Style::default().fg(Color::DarkGray),
                        width,
                    ));
                }
            }
            lines.push(Line::default());
        }
        self.push_lines(terminal, lines)
    }

    fn main_inbox_dir(&self) -> PathBuf {
        self.session_dir.join("inbox")
    }

    #[allow(clippy::too_many_arguments)]
    fn build_worker_spec(
        &self,
        id: String,
        agent_path: PathBuf,
        wal_path: PathBuf,
        inbox_dir: PathBuf,
        leaf: Option<u64>,
        model: Option<String>,
        provider: Option<String>,
        reasoning_effort: Option<String>,
        notify_dir: Option<PathBuf>,
        idle_timeout_secs: u64,
    ) -> WorkerSpec {
        let eff_model = model.as_deref().unwrap_or(&self.draft_model);
        let eff_provider = provider.as_deref().or_else(|| {
            let p = self.draft_provider.trim();
            (!p.is_empty() && p != "-").then_some(p)
        });
        let effort_params = Some(self.agent_config.resolve_effort_params(
            eff_provider,
            eff_model,
            reasoning_effort.as_deref(),
        ));
        WorkerSpec {
            id,
            anix_bin: self.anix_bin.clone(),
            agent_path,
            cwd: self.store.cwd.clone(),
            wal_path,
            inbox_dir,
            leaf,
            config_file: self.config_file.clone(),
            provider,
            model,
            reasoning_effort,
            effort_params,
            base_shell: self.base_shell.clone(),
            approve_sock: self.approval_server.socket_path().to_path_buf(),
            sandbox: self.sandbox.clone(),
            notify_dir,
            idle_timeout_secs,
            self_exe: None,
        }
    }

    fn refresh_draft_defaults(&mut self) {
        let (cfg_model, cfg_provider, max_tok) = self
            .store
            .preview_agent_defaults(&self.meta.agent_path, self.config_file.as_deref());
        self.draft_model = self
            .meta
            .model
            .clone()
            .or(cfg_model)
            .unwrap_or_else(|| "default".into());
        self.draft_provider = self
            .meta
            .provider
            .clone()
            .or(cfg_provider)
            .unwrap_or_else(|| "-".into());
        self.draft_max_tokens = max_tok;
    }

    fn persist_meta_if_materialized(&self) {
        if self.session_persisted {
            let _ = save_session_meta(&self.session_dir, &self.meta);
        }
    }

    /// Materialize the draft session directory on disk (if not yet persisted) and ensure the
    /// `main` `anix` worker process is running.
    fn ensure_main_worker_ready(&mut self, terminal: &mut UiTerminal) -> io::Result<()> {
        if !self.session_persisted {
            self.store
                .materialize_session(&self.session_dir, &self.meta)?;
            self.session_persisted = true;
        }
        let needs_spawn = self.workers.get("main").is_none_or(|st| !st.handle.alive);
        if needs_spawn {
            self.spawn_main_worker(terminal)?;
        }
        Ok(())
    }

    fn spawn_main_worker(&mut self, terminal: &mut UiTerminal) -> io::Result<()> {
        if let Some(mut old) = self.workers.remove("main") {
            old.handle.stop(libc::SIGTERM);
        }
        let eff_model = self.meta.model.as_deref().unwrap_or(&self.draft_model);
        let effort = self.meta.reasoning_effort.clone().or_else(|| {
            self.agent_config
                .effort_for_model(eff_model)
                .map(ToOwned::to_owned)
        });
        let spec = self.build_worker_spec(
            "main".into(),
            self.meta.agent_path.clone(),
            self.meta.active_wal.clone(),
            self.main_inbox_dir(),
            self.meta.active_leaf,
            self.meta.model.clone(),
            self.meta.provider.clone(),
            effort,
            None,
            0,
        );
        match WorkerHandle::spawn(spec, self.worker_event_tx.clone()) {
            Ok(handle) => {
                self.workers
                    .insert("main".into(), WorkerUiState::new(handle));
                Ok(())
            }
            Err(err) => {
                self.push_notice(
                    terminal,
                    Color::Red,
                    format!("failed to start `anix` worker: {err}"),
                )?;
                Err(err)
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn respawn_worker(
        &mut self,
        terminal: &mut UiTerminal,
        worker_id: &str,
        new_wal: Option<PathBuf>,
        new_leaf: Option<Option<u64>>,
        new_model: Option<Option<String>>,
        new_agent: Option<PathBuf>,
        new_effort: Option<Option<String>>,
    ) -> io::Result<()> {
        let keep_plan = new_wal.is_none() && new_leaf.is_none();
        let Some(mut old) = self.workers.remove(worker_id) else {
            // Still an unsaved draft session (no `anix` spawned yet): just update in-memory meta!
            if worker_id == "main" {
                if let Some(w) = new_wal {
                    self.meta.active_wal = w;
                }
                if let Some(l) = new_leaf {
                    self.meta.active_leaf = l;
                }
                if let Some(m) = new_model {
                    self.meta.model = m;
                }
                if let Some(a) = new_agent {
                    self.meta.agent_path = a;
                }
                if let Some(e) = new_effort {
                    self.meta.reasoning_effort = e;
                }
                self.meta.updated_at = now_rfc3339();
                self.refresh_draft_defaults();
                self.persist_meta_if_materialized();
            }
            return Ok(());
        };
        let prev_plan = if keep_plan {
            std::mem::take(&mut old.plan_steps)
        } else {
            Vec::new()
        };
        old.handle.stop(libc::SIGTERM);
        let mut spec = old.handle.spec.clone();
        if let Some(w) = new_wal {
            spec.wal_path = w;
        }
        if let Some(l) = new_leaf {
            spec.leaf = l;
        }
        if let Some(m) = new_model {
            spec.model = m;
        }
        if let Some(a) = new_agent {
            spec.agent_path = a;
        }
        if let Some(e) = new_effort {
            spec.reasoning_effort = e;
        }
        let eff_model = spec.model.as_deref().unwrap_or(&self.draft_model);
        let eff_provider = spec.provider.as_deref().or_else(|| {
            let p = self.draft_provider.trim();
            (!p.is_empty() && p != "-").then_some(p)
        });
        spec.effort_params = Some(self.agent_config.resolve_effort_params(
            eff_provider,
            eff_model,
            spec.reasoning_effort.as_deref(),
        ));

        if worker_id == "main" {
            self.meta.active_wal = spec.wal_path.clone();
            self.meta.active_leaf = spec.leaf;
            self.meta.model = spec.model.clone();
            self.meta.agent_path = spec.agent_path.clone();
            self.meta.reasoning_effort = spec.reasoning_effort.clone();
            self.meta.updated_at = now_rfc3339();
            self.refresh_draft_defaults();
            self.persist_meta_if_materialized();
        }

        match WorkerHandle::spawn(spec, self.worker_event_tx.clone()) {
            Ok(handle) => {
                let mut ui_state = WorkerUiState::new(handle);
                ui_state.plan_steps = prev_plan;
                self.workers.insert(worker_id.to_owned(), ui_state);
                Ok(())
            }
            Err(err) => self.push_notice(
                terminal,
                Color::Red,
                format!("failed to respawn worker `{worker_id}`: {err}"),
            ),
        }
    }

    fn shutdown_all_workers(&mut self) {
        if let Some(active) = self.active_approval.take() {
            active.pending.abort("workbench shutting down");
        }
        while let Some(p) = self.approval_queue.pop_front() {
            p.abort("workbench shutting down");
        }
        for (_, mut w) in std::mem::take(&mut self.workers) {
            w.handle.stop(libc::SIGTERM);
        }
    }

    fn on_tick(&mut self, terminal: &mut UiTerminal) -> io::Result<()> {
        self.spinner_idx = (self.spinner_idx + 1) % SPINNER_FRAMES.len();
        if self.last_git_check.elapsed() >= Duration::from_secs(3) {
            self.last_git_check = Instant::now();
            self.git_branch = detect_git_branch(&self.store.cwd);
        }
        let mut exited = Vec::new();
        for (id, st) in &mut self.workers {
            if let Some(code) = st.handle.poll_exit() {
                exited.push((id.clone(), code));
            }
            let now = Instant::now();
            st.handle.holds.retain(|h| h.deadline > now);
        }
        for (id, code) in exited {
            self.push_notice(
                terminal,
                if code == 0 {
                    Color::DarkGray
                } else {
                    Color::Yellow
                },
                format!("worker `{id}` exited (code={code})"),
            )?;
        }
        self.promote_next_approval();
        Ok(())
    }

    fn promote_next_approval(&mut self) {
        if self.active_approval.is_some() {
            return;
        }
        while let Some(pending) = self.approval_queue.pop_front() {
            if let Some(decision) = try_auto_decide(self.policy, &self.always_allowed, &pending.cmd)
            {
                match decision {
                    ApprovalResponseWire::Allow { cmd } => pending.allow(cmd),
                    ApprovalResponseWire::Deny { reason } => pending.deny(reason),
                    ApprovalResponseWire::Abort { reason } => pending.abort(reason),
                }
            } else {
                self.active_approval = Some(ActiveApproval {
                    pending,
                    submode: ApprovalSubMode::Choose,
                });
                break;
            }
        }
    }

    fn handle_incoming_approval(
        &mut self,
        terminal: &mut UiTerminal,
        pending: PendingApproval,
    ) -> io::Result<()> {
        if let Some(decision) = try_auto_decide(self.policy, &self.always_allowed, &pending.cmd) {
            match decision {
                ApprovalResponseWire::Allow { cmd } => pending.allow(cmd),
                ApprovalResponseWire::Deny { reason } => {
                    let wid = pending.worker_id.clone();
                    let cmd = pending.cmd.clone();
                    pending.deny(reason);
                    self.push_notice(
                        terminal,
                        Color::Yellow,
                        format!("⊘ [{wid}] plan-mode blocked: $ {cmd}"),
                    )?;
                }
                ApprovalResponseWire::Abort { reason } => pending.abort(reason),
            }
            return Ok(());
        }
        let had_active = self.active_approval.is_some();
        let cmd_preview = truncate_single_line(&pending.cmd, 48);
        self.approval_queue.push_back(pending);
        self.promote_next_approval();
        if !had_active && self.active_approval.is_some() && self.agent_config.notify_after_secs > 0
        {
            terminal.notify_user(&format!("anix-agent: approval required — {cmd_preview}"));
        }
        Ok(())
    }

    fn handle_worker_event(
        &mut self,
        terminal: &mut UiTerminal,
        msg: WorkerMessage,
    ) -> io::Result<()> {
        let wid = msg.event.worker_id();
        match self.workers.get(wid) {
            Some(st) if st.handle.instance_id == msg.instance_id => {}
            _ => return Ok(()),
        }
        let ev = msg.event;
        let width = self.term_width(terminal);
        match ev {
            WorkerEvent::RunStart {
                worker_id,
                model,
                provider,
                leaf_id,
                session_usage,
                context,
                ..
            } => {
                if self.agent_config.record_recent_model(&model) {
                    let _ = self.agent_config.save_to(&self.agent_config_path);
                }
                if let Some(st) = self.workers.get_mut(&worker_id) {
                    st.handle.current_model = model;
                    st.handle.current_provider = provider;
                    if leaf_id.is_some() {
                        st.handle.current_leaf = leaf_id;
                    }
                    if let Some(u) = session_usage {
                        st.handle.session_usage = u;
                    }
                    if let Some(c) = context {
                        st.handle.context = Some(c);
                    }
                }
            }
            WorkerEvent::CycleStart {
                worker_id,
                cycle_id,
                source,
            } => {
                let mut woke_async = false;
                if let Some(st) = self.workers.get_mut(&worker_id) {
                    st.handle.busy = true;
                    st.turn_reply_buf.clear();
                    if !st.plan_steps.is_empty()
                        && st.plan_steps.iter().all(|s| {
                            matches!(s.status, PlanStepStatus::Done | PlanStepStatus::Skipped)
                        })
                    {
                        st.plan_steps.clear();
                    }
                    if st.busy_since.is_none() {
                        st.busy_since = Some(Instant::now());
                    }
                    if !st.awaiting_user_cycle {
                        woke_async = true;
                    }
                    st.awaiting_user_cycle = false;
                }
                if woke_async && source == "inbox" {
                    self.push_notice(
                        terminal,
                        Color::Cyan,
                        format!("✉ [{worker_id}] woke from Maildir inbox (cycle #{cycle_id})"),
                    )?;
                }
            }
            WorkerEvent::InboxSteered { worker_id, count } => {
                if let Some(st) = self.workers.get_mut(&worker_id) {
                    let drop_n = (count as usize).min(st.queued_steering.len());
                    st.queued_steering.drain(0..drop_n);
                }
                self.push_notice(
                    terminal,
                    Color::Cyan,
                    format!("✉ [{worker_id}] steered +{count} inbox msg(s) into active cycle"),
                )?;
            }
            WorkerEvent::ReasoningDelta { worker_id, text } => {
                if let Some(st) = self.workers.get_mut(&worker_id) {
                    st.streaming_reasoning.push_str(&text);
                }
            }
            WorkerEvent::TextDelta { worker_id, text } => {
                if let Some(st) = self.workers.get_mut(&worker_id) {
                    st.turn_reply_buf.push_str(&text);
                    st.text_streamer.push_delta(&text);
                }
            }
            WorkerEvent::ToolStart {
                worker_id,
                tool_call_id,
                tool_name,
                cmd,
            } => {
                // Flush any preliminary assistant text/thinking before tool execution.
                if worker_id == self.focused_worker {
                    self.flush_focused_buffers(terminal, false)?;
                }
                if let Some(st) = self.workers.get_mut(&worker_id) {
                    if !st.turn_reply_buf.trim().is_empty() {
                        st.last_assistant_reply = std::mem::take(&mut st.turn_reply_buf);
                    } else {
                        st.turn_reply_buf.clear();
                    }
                    st.active_tools.push(ToolRunState {
                        id: tool_call_id,
                        name: tool_name,
                        cmd,
                        output: None,
                        exit_code: None,
                        turn_id: None,
                        started_at: Instant::now(),
                        duration: None,
                    });
                }
            }
            WorkerEvent::ToolOutput {
                worker_id,
                tool_call_id,
                raw_output,
            } => {
                if let Some(st) = self.workers.get_mut(&worker_id)
                    && let Some(t) = st.active_tools.iter_mut().find(|t| t.id == tool_call_id)
                {
                    t.output = Some(raw_output);
                }
            }
            WorkerEvent::ToolEnd {
                worker_id,
                tool_call_id,
                turn_id,
                exit_code,
                ..
            } => {
                let finished = self.workers.get_mut(&worker_id).and_then(|st| {
                    st.active_tools
                        .iter()
                        .position(|t| t.id == tool_call_id)
                        .map(|idx| {
                            let mut t = st.active_tools.remove(idx);
                            t.exit_code = Some(exit_code);
                            t.turn_id = Some(turn_id);
                            t.duration = Some(t.started_at.elapsed());
                            st.push_detail(DetailEntry::Tool {
                                ts: now_rfc3339(),
                                name: t.name.clone(),
                                cmd: t.cmd.clone(),
                                exit_code: Some(exit_code),
                                output: t.output.clone().unwrap_or_default(),
                            });
                            t
                        })
                });
                if let Some(tool) = finished
                    && worker_id == self.focused_worker
                {
                    self.print_tool_card(terminal, &tool, width)?;
                }
            }
            WorkerEvent::HoldStarted {
                worker_id,
                id,
                deadline_sec,
                label,
            } => {
                if let Some(st) = self.workers.get_mut(&worker_id) {
                    st.handle.holds.retain(|h| h.id != id);
                    st.handle.holds.push(TrackedHold {
                        id: id.clone(),
                        label: label.clone(),
                        deadline: Instant::now() + Duration::from_secs(deadline_sec),
                    });
                }
                let lbl = label.unwrap_or_else(|| id.clone());
                self.push_notice(
                    terminal,
                    Color::Magenta,
                    format!("⏳ [{worker_id}] hold started: {lbl} ({deadline_sec}s lease)"),
                )?;
            }
            WorkerEvent::HoldReleased {
                worker_id,
                id,
                reason,
            } => {
                if let Some(st) = self.workers.get_mut(&worker_id) {
                    st.handle.holds.retain(|h| h.id != id);
                }
                self.push_notice(
                    terminal,
                    Color::DarkGray,
                    format!("✓ [{worker_id}] hold released: {id} ({reason})"),
                )?;
            }
            WorkerEvent::HoldExpired {
                worker_id,
                id,
                label,
            } => {
                if let Some(st) = self.workers.get_mut(&worker_id) {
                    st.handle.holds.retain(|h| h.id != id);
                }
                let lbl = label.unwrap_or(id);
                self.push_notice(
                    terminal,
                    Color::Yellow,
                    format!("⌛ [{worker_id}] hold expired: {lbl}"),
                )?;
            }
            WorkerEvent::Compact {
                worker_id,
                reason,
                checkpoint_turn,
                leaf_id,
            } => {
                if let Some(st) = self.workers.get_mut(&worker_id)
                    && leaf_id.is_some()
                {
                    st.handle.current_leaf = leaf_id;
                }
                if worker_id == "main" && leaf_id.is_some() {
                    self.meta.active_leaf = leaf_id;
                    let _ = save_session_meta(&self.session_dir, &self.meta);
                }
                self.push_notice(
                    terminal,
                    Color::Cyan,
                    format!(
                        "🗜 [{worker_id}] compacted context ({reason}, checkpoint turn #{checkpoint_turn})"
                    ),
                )?;
            }
            WorkerEvent::Fold {
                worker_id,
                from_turn,
                to_turn,
                summary,
            } => {
                self.push_notice(
                    terminal,
                    Color::DarkGray,
                    format!("⊟ [{worker_id}] folded turns {from_turn}..{to_turn}: {summary}"),
                )?;
            }
            WorkerEvent::PlanUpdate { worker_id, steps } => {
                if let Some(st) = self.workers.get_mut(&worker_id) {
                    st.plan_steps = steps;
                }
            }
            WorkerEvent::CycleEnd {
                worker_id,
                reason,
                turns,
                session_usage,
                context,
                leaf_id,
                ..
            } => {
                let mut cycle_elapsed: Option<Duration> = None;
                if let Some(st) = self.workers.get_mut(&worker_id) {
                    cycle_elapsed = st.busy_since.map(|t| t.elapsed());
                    st.handle.busy = false;
                    st.busy_since = None;
                    st.queued_steering.clear();
                    if !st.turn_reply_buf.trim().is_empty() {
                        st.last_assistant_reply = std::mem::take(&mut st.turn_reply_buf);
                    } else {
                        st.turn_reply_buf.clear();
                    }
                    if let Some(u) = session_usage {
                        st.handle.session_usage = u;
                    }
                    if context.is_some() {
                        st.handle.context = context;
                    }
                    if leaf_id.is_some() {
                        st.handle.current_leaf = leaf_id;
                    }
                }
                if worker_id == "main" {
                    if leaf_id.is_some() {
                        self.meta.active_leaf = leaf_id;
                    }
                    self.meta.updated_at = now_rfc3339();
                    let _ = save_session_meta(&self.session_dir, &self.meta);
                }

                if worker_id == self.focused_worker {
                    self.flush_focused_buffers(terminal, true)?;
                } else if let Some(st) = self.workers.get_mut(&worker_id) {
                    let preview =
                        truncate_single_line(&st.text_streamer.first_nonempty_preview(), 64);
                    st.streaming_reasoning.clear();
                    st.text_streamer.reset();
                    self.push_notice(
                        terminal,
                        Color::Cyan,
                        format!(
                            "✓ [worker:{worker_id}] cycle done ({reason}, {turns}t): {preview}"
                        ),
                    )?;
                }

                if self.agent_config.notify_after_secs > 0
                    && let Some(elapsed) = cycle_elapsed
                    && elapsed >= Duration::from_secs(self.agent_config.notify_after_secs)
                {
                    terminal.notify_user(&format!(
                        "anix-agent: [{worker_id}] finished ({})",
                        fmt_duration(elapsed)
                    ));
                }
            }
            WorkerEvent::Idle { worker_id, .. } => {
                if let Some(st) = self.workers.get_mut(&worker_id) {
                    st.handle.busy = false;
                    st.busy_since = None;
                    st.queued_steering.clear();
                }
            }
            WorkerEvent::ProviderError {
                worker_id,
                code,
                message,
            } => {
                if let Some(st) = self.workers.get_mut(&worker_id) {
                    st.handle.busy = false;
                    st.busy_since = None;
                    st.queued_steering.clear();
                }
                self.push_notice(
                    terminal,
                    Color::Red,
                    format!("✗ [{worker_id}] provider error ({code}): {message}"),
                )?;
            }
            WorkerEvent::Warning { worker_id, message } => {
                self.push_notice(
                    terminal,
                    Color::Yellow,
                    format!("⚠ [{worker_id}] {message}"),
                )?;
            }
            WorkerEvent::StderrLine { worker_id, line } => {
                self.push_notice(
                    terminal,
                    Color::DarkGray,
                    format!("[{worker_id} stderr] {line}"),
                )?;
            }
            WorkerEvent::Exited {
                worker_id,
                exit_code,
            } => {
                if let Some(st) = self.workers.get_mut(&worker_id) {
                    st.handle.alive = false;
                    st.handle.busy = false;
                    st.busy_since = None;
                    st.queued_steering.clear();
                }
                self.push_notice(
                    terminal,
                    if exit_code == 0 {
                        Color::DarkGray
                    } else {
                        Color::Yellow
                    },
                    format!("● [{worker_id}] finished (exit={exit_code})"),
                )?;
            }
        }
        Ok(())
    }

    fn format_reasoning_lines(&self, reasoning: &str, width: usize) -> Vec<Line<'static>> {
        let mut lines = Vec::new();
        let trimmed_reason = reasoning.trim();
        if trimmed_reason.is_empty() {
            return lines;
        }
        if self.expand_details {
            lines.push(Line::from(vec![
                Span::styled("○ ", Style::default().fg(Color::DarkGray)),
                Span::styled(
                    "Thought",
                    Style::default()
                        .fg(Color::Gray)
                        .add_modifier(Modifier::BOLD),
                ),
            ]));
            for (idx, chunk_line) in trimmed_reason.lines().enumerate() {
                let pfx = if idx == 0 { "  ⎿ " } else { "    " };
                lines.extend(wrap_prefixed_lines(
                    chunk_line,
                    pfx,
                    Style::default().fg(Color::DarkGray),
                    Style::default()
                        .fg(Color::DarkGray)
                        .add_modifier(Modifier::ITALIC),
                    width,
                ));
            }
        } else {
            let char_count = trimmed_reason.chars().count();
            let preview = truncate_single_line(trimmed_reason, width.saturating_sub(30));
            lines.push(Line::from(vec![
                Span::styled("○ ", Style::default().fg(Color::DarkGray)),
                Span::styled(
                    format!("Thought ({char_count} chars · ^O): "),
                    Style::default().fg(Color::DarkGray),
                ),
                Span::styled(
                    preview,
                    Style::default()
                        .fg(Color::DarkGray)
                        .add_modifier(Modifier::ITALIC),
                ),
            ]));
        }
        lines
    }

    /// Incrementally commit completed Markdown lines/blocks from the focused worker straight into
    /// native terminal scrollback during streaming, eliminating 5-line truncation and final jumps.
    fn commit_ready_stream(&mut self, terminal: &mut UiTerminal) -> io::Result<()> {
        if self.active_approval.is_some() || !matches!(self.overlay, Overlay::None) {
            return Ok(());
        }
        let width = self.term_width(terminal);
        let (reasoning_to_flush, ready_text) = {
            let Some(st) = self.workers.get_mut(&self.focused_worker) else {
                return Ok(());
            };
            let reasoning = if st.text_streamer.has_visible_content()
                && !st.streaming_reasoning.trim().is_empty()
            {
                let r = std::mem::take(&mut st.streaming_reasoning);
                st.push_detail(DetailEntry::Thought {
                    ts: now_rfc3339(),
                    text: r.clone(),
                });
                r
            } else {
                String::new()
            };
            let ready = st.text_streamer.take_ready_lines(width);
            (reasoning, ready)
        };

        let mut lines = self.format_reasoning_lines(&reasoning_to_flush, width);
        lines.extend(ready_text);
        if !lines.is_empty() {
            self.push_lines(terminal, lines)?;
        }
        Ok(())
    }

    fn flush_focused_buffers(
        &mut self,
        terminal: &mut UiTerminal,
        add_trailing_blank: bool,
    ) -> io::Result<()> {
        let width = self.term_width(terminal);
        let (reasoning, tail_lines, emitted_text) = {
            let Some(st) = self.workers.get_mut(&self.focused_worker) else {
                return Ok(());
            };
            let reasoning = std::mem::take(&mut st.streaming_reasoning);
            if !reasoning.trim().is_empty() {
                st.push_detail(DetailEntry::Thought {
                    ts: now_rfc3339(),
                    text: reasoning.clone(),
                });
            }
            let (tail, had_text) = st.text_streamer.finish(width);
            (reasoning, tail, had_text)
        };

        let mut lines = self.format_reasoning_lines(&reasoning, width);
        lines.extend(tail_lines);
        if add_trailing_blank && emitted_text {
            lines.push(Line::default());
        }

        self.push_lines(terminal, lines)
    }

    fn print_tool_card(
        &self,
        terminal: &mut UiTerminal,
        tool: &ToolRunState,
        width: usize,
    ) -> io::Result<()> {
        let rc = tool.exit_code.unwrap_or(0);
        let dot_color = if rc == 0 { Color::Green } else { Color::Red };
        let cmd_preview = truncate_single_line(&tool.cmd, width.saturating_sub(24));
        let mut header_spans = vec![
            Span::styled("● ", Style::default().fg(dot_color)),
            Span::styled(
                format!("{}(", tool.name),
                Style::default()
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(cmd_preview, Style::default().fg(Color::Gray)),
            Span::styled(
                ")",
                Style::default()
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD),
            ),
        ];
        if rc != 0 {
            header_spans.push(Span::styled(
                format!(" [rc={rc}]"),
                Style::default().fg(Color::Red),
            ));
        }
        let mut meta_parts = Vec::new();
        if let Some(dur) = tool.duration {
            meta_parts.push(fmt_duration(dur));
        }
        if let Some(ref raw) = tool.output {
            let n = raw.lines().count();
            if n > 0 {
                meta_parts.push(if n == 1 {
                    "1 line".to_owned()
                } else {
                    format!("{n} lines")
                });
            }
        }
        if !meta_parts.is_empty() {
            header_spans.push(Span::styled(
                format!(" · {}", meta_parts.join(" · ")),
                Style::default().fg(Color::DarkGray),
            ));
        }
        let mut lines = vec![Line::from(header_spans)];

        if let Some(ref raw) = tool.output {
            let out_lines: Vec<&str> = raw.lines().collect();
            let is_diff = looks_like_unified_diff(raw);
            if self.expand_details {
                for (idx, l) in out_lines.iter().take(60).enumerate() {
                    let pfx = if idx == 0 { "  ⎿ " } else { "    " };
                    if is_diff {
                        lines.extend(render_diff_line(pfx, "    ", l, width));
                    } else {
                        lines.extend(wrap_prefixed_lines(
                            l,
                            pfx,
                            Style::default().fg(Color::DarkGray),
                            Style::default().fg(Color::Gray),
                            width,
                        ));
                    }
                }
                if out_lines.len() > 60 {
                    lines.push(Line::from(Span::styled(
                        format!("    … +{} more lines", out_lines.len() - 60),
                        Style::default().fg(Color::DarkGray),
                    )));
                }
            } else if !out_lines.is_empty() {
                let shown = out_lines.len().min(3);
                for (idx, l) in out_lines[..shown].iter().enumerate() {
                    let pfx = if idx == 0 { "  ⎿ " } else { "    " };
                    let line_style = if is_diff {
                        diff_line_style(l)
                    } else {
                        Style::default().fg(Color::DarkGray)
                    };
                    lines.push(Line::from(vec![
                        Span::styled(pfx, Style::default().fg(Color::DarkGray)),
                        Span::styled(truncate_single_line(l, width.saturating_sub(6)), line_style),
                    ]));
                }
                if out_lines.len() > shown {
                    lines.push(Line::from(Span::styled(
                        format!(
                            "    … +{} lines (ctrl+o to expand)",
                            out_lines.len() - shown
                        ),
                        Style::default().fg(Color::DarkGray),
                    )));
                }
            }
        }
        self.push_lines(terminal, lines)
    }

    fn handle_key(&mut self, terminal: &mut UiTerminal, key: KeyEvent) -> io::Result<()> {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);

        // Global shortcuts that work regardless of overlay (except typing inside approval input).
        if key.code == KeyCode::Char('d')
            && ctrl
            && self.input.is_empty()
            && self.active_approval.is_none()
        {
            self.should_quit = true;
            return Ok(());
        }
        if key.code == KeyCode::Char('c') && ctrl {
            if self.active_approval.is_some() {
                if let Some(active) = self.active_approval.take() {
                    active.pending.deny("Cancelled by user (Ctrl+C)");
                }
                self.promote_next_approval();
                return Ok(());
            }
            if !matches!(self.overlay, Overlay::None) {
                self.overlay = Overlay::None;
                return Ok(());
            }
            if !self.input.is_empty() {
                self.input.clear();
                self.hint_line = None;
                return Ok(());
            }
            // Interrupt busy focused worker cleanly or exit if idle.
            if self.interrupt_focused_worker(terminal)? {
                return Ok(());
            }
            self.should_quit = true;
            return Ok(());
        }

        if key.code == KeyCode::Char('z') && ctrl {
            return self.suspend_to_shell(terminal);
        }

        // Priority 1: Interactive Approval Card
        if self.active_approval.is_some() {
            return self.handle_approval_key(terminal, key);
        }

        // Global live toggles
        if key.code == KeyCode::BackTab
            || (key.code == KeyCode::Tab && key.modifiers.contains(KeyModifiers::SHIFT))
        {
            self.policy = self.policy.cycle_next();
            self.meta.approval_policy = self.policy;
            self.persist_meta_if_materialized();
            self.hint_line = Some(format!(
                "approval mode switched -> {}",
                self.policy.as_str()
            ));
            return Ok(());
        }
        if key.code == KeyCode::Char('o') && ctrl {
            if matches!(self.overlay, Overlay::Transcript(_)) {
                self.overlay = Overlay::None;
                return Ok(());
            }
            if matches!(self.overlay, Overlay::None) {
                return self.open_transcript_overlay(terminal);
            }
        }
        if key.code == KeyCode::Char('w') && ctrl {
            if matches!(self.overlay, Overlay::Workers { .. }) {
                self.overlay = Overlay::None;
            } else {
                self.overlay = Overlay::Workers { selected: 0 };
            }
            return Ok(());
        }
        if key.code == KeyCode::Char('r') && ctrl && matches!(self.overlay, Overlay::None) {
            self.open_history_search();
            return Ok(());
        }

        // Priority 2: Active interactive overlay (`/tree`, `/workers`, `/sessions`, `?`, `Ctrl+O`, `Ctrl+R`)
        if !matches!(self.overlay, Overlay::None) {
            return self.handle_overlay_key(terminal, key);
        }

        // Pressing `?` on an empty prompt opens Claude-Code-style shortcuts panel right below prompt.
        if key.code == KeyCode::Char('?') && !ctrl && self.input.is_empty() {
            self.overlay = Overlay::Shortcuts;
            return Ok(());
        }

        // `Ctrl+G`: edit current prompt in `$VISUAL` / `$EDITOR` / `vi`
        if key.code == KeyCode::Char('g') && ctrl {
            return self.open_in_external_editor(terminal);
        }

        // `Ctrl+V`: paste clipboard image as `[Image #N]` attachment (falling back to clipboard text)
        if key.code == KeyCode::Char('v') && ctrl {
            self.handle_ctrl_v_paste(terminal);
            return Ok(());
        }

        // `Ctrl+S`: stash non-empty draft prompt or restore stashed draft
        if key.code == KeyCode::Char('s') && ctrl {
            if !self.input.is_empty() {
                let draft = self.input.expanded_text();
                self.stashed_prompt = Some(draft);
                self.input.clear();
                self.hint_line =
                    Some("stashed draft (press Ctrl+S or send a command to restore)".into());
            } else if let Some(saved) = self.stashed_prompt.take() {
                self.input.set_text(saved);
                self.hint_line = Some("restored stashed draft".into());
            } else {
                self.hint_line = Some("nothing to stash".into());
            }
            return Ok(());
        }

        // `Esc` when no completion dropdown is open:
        // - If focused worker is busy -> single Esc interrupts active inference cleanly (preserving any draft!)
        // - Otherwise double-tap `Esc` within 450ms -> clear non-empty input, or open `/rewind` when empty
        if key.code == KeyCode::Esc && self.input.completion().is_none() {
            self.hint_line = None;
            if self.interrupt_focused_worker(terminal)? {
                self.last_esc_at = None;
                return Ok(());
            }
            let now = Instant::now();
            let is_double = self
                .last_esc_at
                .is_some_and(|prev| now.duration_since(prev) <= Duration::from_millis(450));
            if !self.input.is_empty() {
                if is_double {
                    self.input.clear();
                    self.last_esc_at = None;
                    self.hint_line = Some("cleared input".into());
                } else {
                    self.last_esc_at = Some(now);
                    self.hint_line = Some("press Esc again to clear input".into());
                }
            } else if is_double {
                self.last_esc_at = None;
                self.open_rewind_picker(terminal)?;
            } else {
                self.last_esc_at = Some(now);
                self.hint_line = Some("press Esc again to /rewind conversation".into());
            }
            return Ok(());
        }
        self.last_esc_at = None;

        // Priority 3: Multi-line input box with live `/command` & `@path` dropdown palette
        let agent_names: Vec<String> = self
            .store
            .list_available_agents()
            .into_iter()
            .map(|(n, _)| n)
            .collect();
        let worker_names: Vec<String> = self.workers.keys().cloned().collect();
        let session_pairs: Vec<(String, String)> = if self.input.text().starts_with("/resume")
            || self.input.text().starts_with("/sessions")
        {
            self.store
                .list_sessions()
                .into_iter()
                .take(12)
                .map(|s| (s.meta.id, s.meta.title))
                .collect()
        } else {
            Vec::new()
        };
        let models = self.known_models();
        let active_provider = self.focused_provider();
        if key.code == KeyCode::Char('@') || self.input.text().contains('@') {
            self.ensure_workspace_files_fresh();
        }
        let comp_ctx = CompletionContext {
            cwd: &self.store.cwd,
            files: &self.workspace_files,
            agents: &agent_names,
            workers: &worker_names,
            sessions: &session_pairs,
            models: &models,
            provider: active_provider.as_deref(),
            agent_config: Some(&self.agent_config),
            custom_commands: &self.custom_commands,
        };
        match self.input.handle_key(key, comp_ctx) {
            InputAction::None => {
                self.hint_line = None;
            }
            InputAction::Submit {
                display,
                payload,
                media,
            } => {
                self.hint_line = None;
                let trimmed_payload = payload.trim();
                let trimmed_display = display.trim();
                if !trimmed_payload.is_empty() || !media.is_empty() {
                    if !trimmed_payload.is_empty() {
                        self.record_persistent_history(trimmed_payload);
                    }
                    self.submit_user_input(
                        terminal,
                        trimmed_display.to_owned(),
                        trimmed_payload.to_owned(),
                        media,
                    )?;
                    // Auto-restore stashed draft after sending an interruptive command/message.
                    if self.input.is_empty()
                        && let Some(stashed) = self.stashed_prompt.take()
                    {
                        self.input.set_text(stashed);
                        self.hint_line = Some("restored stashed draft".into());
                    }
                }
            }
        }
        Ok(())
    }

    fn interrupt_focused_worker(&mut self, terminal: &mut UiTerminal) -> io::Result<bool> {
        let wid = self.focused_worker.clone();
        let pid = self
            .workers
            .get(&wid)
            .filter(|st| st.handle.busy)
            .map(|st| st.handle.pid as libc::pid_t);
        let Some(pid) = pid else {
            return Ok(false);
        };
        if pid > 0 {
            // SAFETY: Forwarding SIGINT to active direct child `anix`.
            unsafe {
                libc::kill(pid, libc::SIGINT);
            }
        }
        if let Some(st) = self.workers.get_mut(&wid) {
            st.queued_steering.clear();
        }
        self.push_notice(
            terminal,
            Color::Yellow,
            format!("⏹ interrupted [{wid}] (SIGINT)"),
        )?;
        Ok(true)
    }

    fn record_persistent_history(&mut self, text: &str) {
        let trimmed = text.trim();
        if trimmed.is_empty() {
            return;
        }
        if self
            .history_entries
            .last()
            .is_some_and(|prev| prev.text == trimmed)
        {
            return;
        }
        let entry = HistoryEntry {
            ts: now_rfc3339(),
            cwd: self.store.cwd.display().to_string(),
            text: trimmed.to_owned(),
        };
        self.history_entries.push(entry);
        if self.history_entries.len() > 500 {
            let excess = self.history_entries.len() - 500;
            self.history_entries.drain(0..excess);
        }
        let _ = self.store.append_input_history(trimmed);
    }

    fn handle_paste(&mut self, _terminal: &mut UiTerminal, raw: String) {
        let single_line = raw.replace("\r\n", " ").replace(['\r', '\n'], " ");
        if let Some(ref mut active) = self.active_approval {
            match &mut active.submode {
                ApprovalSubMode::EditCmd { buf } | ApprovalSubMode::DenyReason { buf } => {
                    buf.push_str(&single_line);
                }
                ApprovalSubMode::Choose => {}
            }
            return;
        }
        if matches!(self.overlay, Overlay::Models(_)) {
            if let Overlay::Models(mut picker) = std::mem::replace(&mut self.overlay, Overlay::None)
            {
                picker.query.push_str(&single_line);
                picker.selected = 0;
                self.sync_model_picker_effort(&mut picker);
                self.overlay = Overlay::Models(picker);
            }
            return;
        }
        match &mut self.overlay {
            Overlay::HistorySearch(picker) => {
                picker.query.push_str(&single_line);
                picker.selected = 0;
                return;
            }
            Overlay::Import(picker) => {
                picker.query.push_str(&single_line);
                picker.selected = 0;
                picker.clamp_selection();
                return;
            }
            Overlay::Sessions(picker) => {
                match &mut picker.mode {
                    ResumeMode::Browse => {
                        picker.query.push_str(&single_line);
                        picker.selected = 0;
                        picker.clamp_selection();
                    }
                    ResumeMode::Rename { buf } => buf.push_str(&single_line),
                    ResumeMode::ConfirmDelete => {}
                }
                return;
            }
            Overlay::Config(picker) if picker.tab == ConfigTab::Settings => {
                picker.filter.push_str(&single_line);
                picker.selected = 0;
                return;
            }
            Overlay::None => {}
            _ => return,
        }

        let agent_names: Vec<String> = self
            .store
            .list_available_agents()
            .into_iter()
            .map(|(n, _)| n)
            .collect();
        let worker_names: Vec<String> = self.workers.keys().cloned().collect();
        let session_pairs: Vec<(String, String)> = Vec::new();
        let models = self.known_models();
        let active_provider = self.focused_provider();
        if raw.contains('@') || self.input.text().contains('@') {
            self.ensure_workspace_files_fresh();
        }
        let comp_ctx = CompletionContext {
            cwd: &self.store.cwd,
            files: &self.workspace_files,
            agents: &agent_names,
            workers: &worker_names,
            sessions: &session_pairs,
            models: &models,
            provider: active_provider.as_deref(),
            agent_config: Some(&self.agent_config),
            custom_commands: &self.custom_commands,
        };
        if let Some(fold_hint) = self.input.handle_paste(&raw, comp_ctx) {
            self.hint_line = Some(fold_hint);
        } else {
            self.hint_line = None;
        }
    }

    fn handle_ctrl_v_paste(&mut self, terminal: &mut UiTerminal) {
        match capture_clipboard_image(&self.store.runtime_dir) {
            Ok(Some((mime, path))) => {
                let tok = self.input.attach_image(mime, path);
                self.hint_line = Some(format!("attached clipboard image {tok}"));
            }
            Ok(None) => {
                if let Some(text) = read_clipboard_text() {
                    self.handle_paste(terminal, text);
                } else {
                    self.hint_line = Some("clipboard is empty (no image or text found)".into());
                }
            }
            Err(err) => {
                self.hint_line = Some(format!("clipboard image failed: {err}"));
            }
        }
    }

    fn ensure_workspace_files_fresh(&mut self) {
        let stale = self
            .workspace_files_refreshed
            .is_none_or(|t| t.elapsed() >= Duration::from_secs(5));
        if stale {
            self.workspace_files = scan_workspace_files(&self.store.cwd);
            self.workspace_files_refreshed = Some(Instant::now());
        }
    }

    fn handle_approval_key(&mut self, terminal: &mut UiTerminal, key: KeyEvent) -> io::Result<()> {
        let Some(mut active) = self.active_approval.take() else {
            return Ok(());
        };
        match active.submode {
            ApprovalSubMode::Choose => match key.code {
                KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter => {
                    let wid = active.pending.worker_id.clone();
                    let cmd = active.pending.cmd.clone();
                    active.pending.allow(cmd.clone());
                    self.push_notice(
                        terminal,
                        Color::Green,
                        format!("✓ [{wid}] approved: $ {cmd}"),
                    )?;
                    self.promote_next_approval();
                    return Ok(());
                }
                KeyCode::Char('a') | KeyCode::Char('A') => {
                    let wid = active.pending.worker_id.clone();
                    let cmd = active.pending.cmd.clone();
                    if let Some(bin) = extract_argv0(&cmd) {
                        self.always_allowed.insert(bin.clone());
                        self.push_notice(
                            terminal,
                            Color::Green,
                            format!("✓ [{wid}] always-allowed `{bin}` for this session"),
                        )?;
                    }
                    active.pending.allow(cmd);
                    self.promote_next_approval();
                    return Ok(());
                }
                KeyCode::Char('n') | KeyCode::Char('N') => {
                    active.submode = ApprovalSubMode::DenyReason { buf: String::new() };
                }
                KeyCode::Char('e') | KeyCode::Char('E') => {
                    let cmd = active.pending.cmd.clone();
                    active.submode = ApprovalSubMode::EditCmd { buf: cmd };
                }
                KeyCode::Esc => {
                    let wid = active.pending.worker_id.clone();
                    let cmd = active.pending.cmd.clone();
                    active.pending.deny("Denied by user");
                    self.push_notice(
                        terminal,
                        Color::Yellow,
                        format!("✗ [{wid}] denied: $ {cmd}"),
                    )?;
                    self.promote_next_approval();
                    return Ok(());
                }
                _ => {}
            },
            ApprovalSubMode::DenyReason { mut buf } => match key.code {
                KeyCode::Enter => {
                    let wid = active.pending.worker_id.clone();
                    let cmd = active.pending.cmd.clone();
                    let reason = if buf.trim().is_empty() {
                        "Denied by user".to_owned()
                    } else {
                        format!("Denied by user: {}", buf.trim())
                    };
                    active.pending.deny(reason.clone());
                    self.push_notice(
                        terminal,
                        Color::Yellow,
                        format!("✗ [{wid}] {reason} ($ {cmd})"),
                    )?;
                    self.promote_next_approval();
                    return Ok(());
                }
                KeyCode::Esc => {
                    active.submode = ApprovalSubMode::Choose;
                }
                KeyCode::Backspace => {
                    buf.pop();
                    active.submode = ApprovalSubMode::DenyReason { buf };
                }
                KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                    buf.push(c);
                    active.submode = ApprovalSubMode::DenyReason { buf };
                }
                _ => {
                    active.submode = ApprovalSubMode::DenyReason { buf };
                }
            },
            ApprovalSubMode::EditCmd { mut buf } => match key.code {
                KeyCode::Enter => {
                    let wid = active.pending.worker_id.clone();
                    let final_cmd = buf.trim().to_owned();
                    if final_cmd.is_empty() {
                        active.pending.deny("Edited command was empty");
                    } else {
                        active.pending.allow(final_cmd.clone());
                        self.push_notice(
                            terminal,
                            Color::Green,
                            format!("✓ [{wid}] approved edited: $ {final_cmd}"),
                        )?;
                    }
                    self.promote_next_approval();
                    return Ok(());
                }
                KeyCode::Esc => {
                    active.submode = ApprovalSubMode::Choose;
                }
                KeyCode::Backspace => {
                    buf.pop();
                    active.submode = ApprovalSubMode::EditCmd { buf };
                }
                KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                    buf.push(c);
                    active.submode = ApprovalSubMode::EditCmd { buf };
                }
                _ => {
                    active.submode = ApprovalSubMode::EditCmd { buf };
                }
            },
        }
        self.active_approval = Some(active);
        Ok(())
    }
}
