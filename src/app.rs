//! Main Layer 3 Inline TUI workbench (`ratatui::Viewport::Inline` + `Terminal::insert_before`),
//! driving persistent `anix` workers via Maildir, `--trace-fd 9`, and UDS `approve-wrap`.

use std::collections::{BTreeMap, HashSet, VecDeque};
use std::error::Error;
use std::io::{self, Stdout};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::terminal::{disable_raw_mode, enable_raw_mode};
use ratatui::backend::{Backend, ClearType, WindowSize};
use ratatui::buffer::Cell;
use ratatui::layout::{Constraint, Direction, Layout, Position, Rect, Size};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Widget};
use ratatui::{Frame, Terminal, TerminalOptions, Viewport};
use unicode_width::UnicodeWidthStr;

use crate::approve::{
    ApprovalResponseWire, ApprovalServer, PendingApproval, extract_argv0, try_auto_decide,
};
use crate::cli::{ApprovalPolicy, Cli};
use crate::input::{
    COMPLETION_VISIBLE_ROWS, CompletionContext, CompletionMenu, InputAction, InputBox,
};
use crate::markdown::{render_markdown, wrap_prefixed_lines};
use crate::session::{
    SessionMeta, SessionStore, SessionSummary, now_rfc3339, resolve_anix_bin, save_session_meta,
};
use crate::wal_view::{WalGraph, load_recent_replay};
use crate::worker::{
    TrackedHold, WorkerEvent, WorkerHandle, WorkerSpec, ensure_maildir, run_anix_fork,
};

const VIEWPORT_HEIGHT: u16 = 13;
const SPINNER_FRAMES: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// CJK-safe Crossterm backend fixing `ratatui 0.29`'s `Terminal::insert_before` (`draw_lines`)
/// bug where double-width CJK/emoji trailing placeholder cells are emitted as literal spaces,
/// causing wide gaps between Chinese characters and right-edge line clipping.
struct CjkSafeBackend<W: io::Write> {
    writer: W,
}

impl<W: io::Write> CjkSafeBackend<W> {
    fn new(writer: W) -> Self {
        Self { writer }
    }
}

impl<W: io::Write> Backend for CjkSafeBackend<W> {
    fn draw<'a, I>(&mut self, content: I) -> io::Result<()>
    where
        I: Iterator<Item = (u16, u16, &'a Cell)>,
    {
        use crossterm::cursor::MoveTo;
        use crossterm::queue;
        use crossterm::style::{
            Attribute, Color as CColor, Colors, Print, SetAttribute, SetBackgroundColor, SetColors,
            SetForegroundColor,
        };

        let writer = &mut self.writer;
        let mut fg = Color::Reset;
        let mut bg = Color::Reset;
        let mut modifier = Modifier::empty();
        // Track the terminal's ACTUAL cursor position (accounting for multi-width glyphs!)
        // so consecutive CJK characters emit as a contiguous UTF-8 stream with zero MoveTo churn.
        let mut cursor_pos: Option<(u16, u16)> = None;
        let mut current_row: Option<u16> = None;
        let mut skip_until_x: u16 = 0;

        for (x, y, cell) in content {
            if current_row != Some(y) {
                current_row = Some(y);
                skip_until_x = 0;
            }
            if cell.skip || (x < skip_until_x && cell.symbol() == " ") {
                continue;
            }

            if cursor_pos != Some((x, y)) {
                queue!(writer, MoveTo(x, y))?;
            }

            if cell.modifier != modifier {
                let removed = modifier - cell.modifier;
                if removed.contains(Modifier::REVERSED) {
                    queue!(writer, SetAttribute(Attribute::NoReverse))?;
                }
                if removed.contains(Modifier::BOLD) || removed.contains(Modifier::DIM) {
                    queue!(writer, SetAttribute(Attribute::NormalIntensity))?;
                }
                if removed.contains(Modifier::ITALIC) {
                    queue!(writer, SetAttribute(Attribute::NoItalic))?;
                }
                if removed.contains(Modifier::UNDERLINED) {
                    queue!(writer, SetAttribute(Attribute::NoUnderline))?;
                }
                if removed.contains(Modifier::CROSSED_OUT) {
                    queue!(writer, SetAttribute(Attribute::NotCrossedOut))?;
                }

                let added = cell.modifier - modifier;
                if added.contains(Modifier::REVERSED) {
                    queue!(writer, SetAttribute(Attribute::Reverse))?;
                }
                if added.contains(Modifier::BOLD)
                    || (removed.contains(Modifier::DIM) && cell.modifier.contains(Modifier::BOLD))
                {
                    queue!(writer, SetAttribute(Attribute::Bold))?;
                }
                if added.contains(Modifier::DIM)
                    || (removed.contains(Modifier::BOLD) && cell.modifier.contains(Modifier::DIM))
                {
                    queue!(writer, SetAttribute(Attribute::Dim))?;
                }
                if added.contains(Modifier::ITALIC) {
                    queue!(writer, SetAttribute(Attribute::Italic))?;
                }
                if added.contains(Modifier::UNDERLINED) {
                    queue!(writer, SetAttribute(Attribute::Underlined))?;
                }
                if added.contains(Modifier::CROSSED_OUT) {
                    queue!(writer, SetAttribute(Attribute::CrossedOut))?;
                }
                modifier = cell.modifier;
            }

            if cell.fg != fg || cell.bg != bg {
                queue!(
                    writer,
                    SetColors(Colors::new(cell.fg.into(), cell.bg.into()))
                )?;
                fg = cell.fg;
                bg = cell.bg;
            }

            let sym = cell.symbol();
            queue!(writer, Print(sym))?;

            let w = sym.width().max(1) as u16;
            skip_until_x = x.saturating_add(w);
            cursor_pos = Some((x.saturating_add(w), y));
        }

        queue!(
            writer,
            SetForegroundColor(CColor::Reset),
            SetBackgroundColor(CColor::Reset),
            SetAttribute(Attribute::Reset),
        )
    }

    fn append_lines(&mut self, n: u16) -> io::Result<()> {
        for _ in 0..n {
            crossterm::queue!(self.writer, crossterm::style::Print("\n"))?;
        }
        self.writer.flush()
    }

    fn hide_cursor(&mut self) -> io::Result<()> {
        crossterm::execute!(self.writer, crossterm::cursor::Hide)
    }

    fn show_cursor(&mut self) -> io::Result<()> {
        crossterm::execute!(self.writer, crossterm::cursor::Show)
    }

    fn get_cursor_position(&mut self) -> io::Result<Position> {
        crossterm::cursor::position().map(|(x, y)| Position::new(x, y))
    }

    fn set_cursor_position<P: Into<Position>>(&mut self, position: P) -> io::Result<()> {
        let p = position.into();
        crossterm::execute!(self.writer, crossterm::cursor::MoveTo(p.x, p.y))
    }

    fn clear(&mut self) -> io::Result<()> {
        self.clear_region(ClearType::All)
    }

    fn clear_region(&mut self, clear_type: ClearType) -> io::Result<()> {
        let ct = match clear_type {
            ClearType::All => crossterm::terminal::ClearType::All,
            ClearType::AfterCursor => crossterm::terminal::ClearType::FromCursorDown,
            ClearType::BeforeCursor => crossterm::terminal::ClearType::FromCursorUp,
            ClearType::CurrentLine => crossterm::terminal::ClearType::CurrentLine,
            ClearType::UntilNewLine => crossterm::terminal::ClearType::UntilNewLine,
        };
        crossterm::execute!(self.writer, crossterm::terminal::Clear(ct))
    }

    fn size(&self) -> io::Result<Size> {
        crossterm::terminal::size().map(|(w, h)| Size::new(w, h))
    }

    fn window_size(&mut self) -> io::Result<WindowSize> {
        let ws = crossterm::terminal::window_size()?;
        Ok(WindowSize {
            columns_rows: Size::new(ws.columns, ws.rows),
            pixels: Size::new(ws.width, ws.height),
        })
    }

    fn flush(&mut self) -> io::Result<()> {
        self.writer.flush()
    }
}

type UiTerminal = Terminal<CjkSafeBackend<Stdout>>;

enum AppEvent {
    Key(KeyEvent),
    Resize,
    Tick,
    Worker(WorkerEvent),
    Approval(PendingApproval),
}

#[derive(Debug, Clone)]
struct ToolRunState {
    id: String,
    name: String,
    cmd: String,
    output: Option<String>,
    exit_code: Option<i32>,
    turn_id: Option<u64>,
}

struct WorkerUiState {
    handle: WorkerHandle,
    streaming_reasoning: String,
    streaming_text: String,
    active_tools: Vec<ToolRunState>,
    awaiting_user_cycle: bool,
}

impl WorkerUiState {
    fn new(handle: WorkerHandle) -> Self {
        Self {
            handle,
            streaming_reasoning: String::new(),
            streaming_text: String::new(),
            active_tools: Vec::new(),
            awaiting_user_cycle: false,
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

enum Overlay {
    None,
    Tree {
        graph: WalGraph,
        selected: usize,
        scroll: usize,
    },
    Workers {
        selected: usize,
    },
    Sessions {
        items: Vec<SessionSummary>,
        selected: usize,
    },
}

struct TerminalGuard;

impl TerminalGuard {
    fn enter() -> io::Result<Self> {
        enable_raw_mode()?;
        let orig_hook = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            let _ = disable_raw_mode();
            let _ = crossterm::execute!(io::stdout(), crossterm::cursor::Show);
            orig_hook(info);
        }));
        Ok(Self)
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = crossterm::execute!(io::stdout(), crossterm::cursor::Show);
    }
}

struct Workbench {
    store: SessionStore,
    anix_bin: PathBuf,
    config_file: Option<PathBuf>,
    base_shell: Option<String>,
    sandbox: Option<String>,
    approval_server: ApprovalServer,
    worker_event_tx: mpsc::Sender<WorkerEvent>,

    session_dir: PathBuf,
    meta: SessionMeta,
    policy: ApprovalPolicy,
    always_allowed: HashSet<String>,
    expand_details: bool,

    workers: BTreeMap<String, WorkerUiState>,
    focused_worker: String,
    next_worker_seq: u32,

    input: InputBox,
    hint_line: Option<String>,
    approval_queue: VecDeque<PendingApproval>,
    active_approval: Option<ActiveApproval>,
    overlay: Overlay,

    spinner_idx: usize,
    should_quit: bool,
}

/// Boot and run the interactive `anix-agent` Inline TUI workbench.
pub fn run_interactive(cli: Cli) -> Result<(), Box<dyn Error>> {
    let cwd = std::env::current_dir()?;
    let store = SessionStore::open(&cwd)?;
    let anix_bin = resolve_anix_bin(cli.anix_bin.as_deref())?;

    let (session_dir, mut meta, is_resumed) = if let Some(ref query) = cli.resume {
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
                (dir, m, true)
            }
            None => {
                let agent_path = store.resolve_agent_path(cli.agent.as_deref())?;
                let (dir, m) = store.create_session(
                    agent_path,
                    cli.model.clone(),
                    cli.provider.clone(),
                    cli.mode,
                )?;
                (dir, m, false)
            }
        }
    } else {
        let agent_path = store.resolve_agent_path(cli.agent.as_deref())?;
        let (dir, m) = store.create_session(
            agent_path,
            cli.model.clone(),
            cli.provider.clone(),
            cli.mode,
        )?;
        (dir, m, false)
    };

    if cli.mode != meta.approval_policy && !is_resumed {
        meta.approval_policy = cli.mode;
        let _ = save_session_meta(&session_dir, &meta);
    }

    let (app_tx, app_rx) = mpsc::channel::<AppEvent>();
    let (worker_tx, worker_rx) = mpsc::channel::<WorkerEvent>();

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

    // Enter raw mode & allocate Inline viewport (NO alternate screen hijack!).
    let _guard = TerminalGuard::enter()?;
    let backend = CjkSafeBackend::new(io::stdout());
    let mut terminal = Terminal::with_options(
        backend,
        TerminalOptions {
            viewport: Viewport::Inline(VIEWPORT_HEIGHT),
        },
    )?;

    // Start dedicated crossterm keyboard + resize + tick thread.
    let stop_flag = Arc::new(AtomicBool::new(false));
    let stop_reader = Arc::clone(&stop_flag);
    let tx_term = app_tx.clone();
    let reader_handle = thread::spawn(move || {
        let mut last_tick = Instant::now();
        while !stop_reader.load(Ordering::Relaxed) {
            if event::poll(Duration::from_millis(60)).unwrap_or(false) {
                match event::read() {
                    Ok(Event::Key(k)) if k.kind != KeyEventKind::Release => {
                        if tx_term.send(AppEvent::Key(k)).is_err() {
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
    let mut app = Workbench {
        store,
        anix_bin,
        config_file: cli.config,
        base_shell: cli.shell,
        sandbox: cli.sandbox,
        approval_server,
        worker_event_tx: worker_tx,
        session_dir,
        meta,
        policy,
        always_allowed: HashSet::new(),
        expand_details: false,
        workers: BTreeMap::new(),
        focused_worker: "main".into(),
        next_worker_seq: 1,
        input: InputBox::new(),
        hint_line: None,
        approval_queue: VecDeque::new(),
        active_approval: None,
        overlay: Overlay::None,
        spinner_idx: 0,
        should_quit: false,
    };

    app.print_banner(&mut terminal, is_resumed)?;
    if is_resumed {
        app.replay_active_wal(&mut terminal)?;
    }
    app.spawn_main_worker(&mut terminal)?;

    if let Some(initial_prompt) = cli.prompt
        && !initial_prompt.trim().is_empty()
    {
        app.submit_user_input(&mut terminal, initial_prompt)?;
    }

    terminal.draw(|f| app.render(f))?;

    while !app.should_quit {
        let Ok(ev) = app_rx.recv() else { break };
        match ev {
            AppEvent::Key(key) => {
                app.handle_key(&mut terminal, key)?;
            }
            AppEvent::Resize => {}
            AppEvent::Tick => {
                app.on_tick(&mut terminal)?;
            }
            AppEvent::Worker(wev) => {
                app.handle_worker_event(&mut terminal, wev)?;
            }
            AppEvent::Approval(pending) => {
                app.handle_incoming_approval(&mut terminal, pending)?;
            }
        }
        // Drain any immediately queued events before redrawing to keep streaming ultra-smooth.
        while !app.should_quit {
            match app_rx.try_recv() {
                Ok(AppEvent::Key(k)) => app.handle_key(&mut terminal, k)?,
                Ok(AppEvent::Resize) => {}
                Ok(AppEvent::Tick) => app.on_tick(&mut terminal)?,
                Ok(AppEvent::Worker(w)) => app.handle_worker_event(&mut terminal, w)?,
                Ok(AppEvent::Approval(a)) => app.handle_incoming_approval(&mut terminal, a)?,
                Err(_) => break,
            }
        }
        if !app.should_quit {
            terminal.draw(|f| app.render(f))?;
        }
    }

    stop_flag.store(true, Ordering::Relaxed);
    app.shutdown_all_workers();
    let _ = reader_handle.join();
    // Leave cursor cleanly below the inline viewport on exit.
    let _ = terminal.show_cursor();
    Ok(())
}

impl Workbench {
    fn term_width(&self, terminal: &UiTerminal) -> usize {
        terminal
            .size()
            .map(|s| (s.width as usize).saturating_sub(1))
            .unwrap_or(79)
            .max(30)
    }

    fn push_lines(
        &self,
        terminal: &mut Terminal<CjkSafeBackend<Stdout>>,
        lines: Vec<Line<'static>>,
    ) -> io::Result<()> {
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
        terminal: &mut Terminal<CjkSafeBackend<Stdout>>,
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

    fn print_banner(
        &self,
        terminal: &mut Terminal<CjkSafeBackend<Stdout>>,
        resumed: bool,
    ) -> io::Result<()> {
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
        let tips = Line::from(vec![Span::styled(
            "  /help · /tree · /fork · /spawn · /workers (^W) · /compact · ⇧Tab mode · ^O expand",
            Style::default().fg(Color::DarkGray),
        )]);
        self.push_lines(terminal, vec![header, tips, Line::default()])
    }

    fn replay_active_wal(&self, terminal: &mut Terminal<CjkSafeBackend<Stdout>>) -> io::Result<()> {
        let turns = load_recent_replay(&self.meta.active_wal, self.meta.active_leaf, 8);
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
        notify_dir: Option<PathBuf>,
        idle_timeout_secs: u64,
    ) -> WorkerSpec {
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
            base_shell: self.base_shell.clone(),
            approve_sock: self.approval_server.socket_path().to_path_buf(),
            sandbox: self.sandbox.clone(),
            notify_dir,
            idle_timeout_secs,
            self_exe: None,
        }
    }

    fn spawn_main_worker(
        &mut self,
        terminal: &mut Terminal<CjkSafeBackend<Stdout>>,
    ) -> io::Result<()> {
        if let Some(mut old) = self.workers.remove("main") {
            old.handle.stop(libc::SIGTERM);
        }
        let spec = self.build_worker_spec(
            "main".into(),
            self.meta.agent_path.clone(),
            self.meta.active_wal.clone(),
            self.main_inbox_dir(),
            self.meta.active_leaf,
            self.meta.model.clone(),
            self.meta.provider.clone(),
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

    fn respawn_worker(
        &mut self,
        terminal: &mut Terminal<CjkSafeBackend<Stdout>>,
        worker_id: &str,
        new_wal: Option<PathBuf>,
        new_leaf: Option<Option<u64>>,
        new_model: Option<Option<String>>,
        new_agent: Option<PathBuf>,
    ) -> io::Result<()> {
        let Some(mut old) = self.workers.remove(worker_id) else {
            return Ok(());
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

        if worker_id == "main" {
            self.meta.active_wal = spec.wal_path.clone();
            self.meta.active_leaf = spec.leaf;
            self.meta.model = spec.model.clone();
            self.meta.agent_path = spec.agent_path.clone();
            self.meta.updated_at = now_rfc3339();
            let _ = save_session_meta(&self.session_dir, &self.meta);
        }

        match WorkerHandle::spawn(spec, self.worker_event_tx.clone()) {
            Ok(handle) => {
                self.workers
                    .insert(worker_id.to_owned(), WorkerUiState::new(handle));
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

    fn on_tick(&mut self, terminal: &mut Terminal<CjkSafeBackend<Stdout>>) -> io::Result<()> {
        self.spinner_idx = (self.spinner_idx + 1) % SPINNER_FRAMES.len();
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
        terminal: &mut Terminal<CjkSafeBackend<Stdout>>,
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
        self.approval_queue.push_back(pending);
        self.promote_next_approval();
        Ok(())
    }

    fn handle_worker_event(
        &mut self,
        terminal: &mut Terminal<CjkSafeBackend<Stdout>>,
        ev: WorkerEvent,
    ) -> io::Result<()> {
        let width = self.term_width(terminal);
        match ev {
            WorkerEvent::RunStart {
                worker_id,
                model,
                provider,
                leaf_id,
                ..
            } => {
                if let Some(st) = self.workers.get_mut(&worker_id) {
                    st.handle.current_model = model;
                    st.handle.current_provider = provider;
                    if leaf_id.is_some() {
                        st.handle.current_leaf = leaf_id;
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
            WorkerEvent::ReasoningDelta { worker_id, text } => {
                if let Some(st) = self.workers.get_mut(&worker_id) {
                    st.streaming_reasoning.push_str(&text);
                }
            }
            WorkerEvent::TextDelta { worker_id, text } => {
                if let Some(st) = self.workers.get_mut(&worker_id) {
                    st.streaming_text.push_str(&text);
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
                    st.active_tools.push(ToolRunState {
                        id: tool_call_id,
                        name: tool_name,
                        cmd,
                        output: None,
                        exit_code: None,
                        turn_id: None,
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
            WorkerEvent::CycleEnd {
                worker_id,
                reason,
                turns,
                session_usage,
                context,
                leaf_id,
                ..
            } => {
                if let Some(st) = self.workers.get_mut(&worker_id) {
                    st.handle.busy = false;
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
                    let preview = first_nonempty_line(&st.streaming_text);
                    st.streaming_reasoning.clear();
                    st.streaming_text.clear();
                    self.push_notice(
                        terminal,
                        Color::Cyan,
                        format!(
                            "✓ [worker:{worker_id}] cycle done ({reason}, {turns}t): {preview}"
                        ),
                    )?;
                }
            }
            WorkerEvent::Idle { worker_id, .. } => {
                if let Some(st) = self.workers.get_mut(&worker_id) {
                    st.handle.busy = false;
                }
            }
            WorkerEvent::ProviderError {
                worker_id,
                code,
                message,
            } => {
                if let Some(st) = self.workers.get_mut(&worker_id) {
                    st.handle.busy = false;
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

    fn flush_focused_buffers(
        &mut self,
        terminal: &mut Terminal<CjkSafeBackend<Stdout>>,
        add_trailing_blank: bool,
    ) -> io::Result<()> {
        let width = self.term_width(terminal);
        let Some(st) = self.workers.get_mut(&self.focused_worker) else {
            return Ok(());
        };
        let reasoning = std::mem::take(&mut st.streaming_reasoning);
        let text = std::mem::take(&mut st.streaming_text);

        let mut lines = Vec::new();
        let trimmed_reason = reasoning.trim();
        if !trimmed_reason.is_empty() {
            if self.expand_details {
                lines.push(Line::from(Span::styled(
                    "╭─ thinking",
                    Style::default().fg(Color::DarkGray),
                )));
                lines.extend(wrap_prefixed_lines(
                    trimmed_reason,
                    "│ ",
                    Style::default().fg(Color::DarkGray),
                    Style::default()
                        .fg(Color::DarkGray)
                        .add_modifier(Modifier::ITALIC),
                    width,
                ));
                lines.push(Line::from(Span::styled(
                    "╰─",
                    Style::default().fg(Color::DarkGray),
                )));
            } else {
                let char_count = trimmed_reason.chars().count();
                let preview = truncate_single_line(trimmed_reason, width.saturating_sub(24));
                lines.push(Line::from(vec![
                    Span::styled("💭 ", Style::default().fg(Color::DarkGray)),
                    Span::styled(
                        format!("thinking ({char_count} chars · ^O): "),
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
        }

        let trimmed_text = text.trim();
        if !trimmed_text.is_empty() {
            lines.extend(render_markdown(trimmed_text, width));
            if add_trailing_blank {
                lines.push(Line::default());
            }
        }

        self.push_lines(terminal, lines)
    }

    fn print_tool_card(
        &self,
        terminal: &mut Terminal<CjkSafeBackend<Stdout>>,
        tool: &ToolRunState,
        width: usize,
    ) -> io::Result<()> {
        let rc = tool.exit_code.unwrap_or(0);
        let (icon, color) = if rc == 0 {
            ("✓", Color::Green)
        } else {
            ("✗", Color::Red)
        };
        let cmd_preview = truncate_single_line(&tool.cmd, width.saturating_sub(24));
        let mut lines = vec![Line::from(vec![
            Span::styled(
                format!("{icon} "),
                Style::default().fg(color).add_modifier(Modifier::BOLD),
            ),
            Span::styled(format!("{} ", tool.name), Style::default().fg(Color::Cyan)),
            Span::styled(
                format!("$ {cmd_preview}"),
                Style::default().fg(Color::White),
            ),
            Span::styled(format!("  [rc={rc}]"), Style::default().fg(color)),
        ])];

        if let Some(ref raw) = tool.output {
            let out_lines: Vec<&str> = raw.lines().collect();
            if self.expand_details {
                for l in out_lines.iter().take(60) {
                    lines.extend(wrap_prefixed_lines(
                        l,
                        "  │ ",
                        Style::default().fg(Color::DarkGray),
                        Style::default().fg(Color::Gray),
                        width,
                    ));
                }
                if out_lines.len() > 60 {
                    lines.push(Line::from(Span::styled(
                        format!("  … ({} more lines)", out_lines.len() - 60),
                        Style::default().fg(Color::DarkGray),
                    )));
                }
            } else if !out_lines.is_empty() {
                let shown = out_lines.len().min(3);
                for l in &out_lines[..shown] {
                    lines.push(Line::from(vec![
                        Span::styled("  │ ", Style::default().fg(Color::DarkGray)),
                        Span::styled(
                            truncate_single_line(l, width.saturating_sub(6)),
                            Style::default().fg(Color::DarkGray),
                        ),
                    ]));
                }
                if out_lines.len() > shown {
                    lines.push(Line::from(Span::styled(
                        format!(
                            "  ╰─ +{} more lines (press ^O to expand)",
                            out_lines.len() - shown
                        ),
                        Style::default().fg(Color::DarkGray),
                    )));
                }
            }
        }
        self.push_lines(terminal, lines)
    }

    fn handle_key(
        &mut self,
        terminal: &mut Terminal<CjkSafeBackend<Stdout>>,
        key: KeyEvent,
    ) -> io::Result<()> {
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
            if let Some(st) = self.workers.get(&self.focused_worker)
                && st.handle.busy
            {
                let pid = st.handle.pid as libc::pid_t;
                if pid > 0 {
                    // SAFETY: Forwarding SIGINT to active direct child `anix`.
                    unsafe {
                        libc::kill(pid, libc::SIGINT);
                    }
                }
                self.push_notice(
                    terminal,
                    Color::Yellow,
                    format!("sent SIGINT to worker `{}`", self.focused_worker),
                )?;
                return Ok(());
            }
            self.should_quit = true;
            return Ok(());
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
            let _ = save_session_meta(&self.session_dir, &self.meta);
            self.hint_line = Some(format!(
                "approval mode switched -> {}",
                self.policy.as_str()
            ));
            return Ok(());
        }
        if key.code == KeyCode::Char('o') && ctrl {
            self.expand_details = !self.expand_details;
            self.hint_line = Some(format!(
                "thinking & tool details: {}",
                if self.expand_details {
                    "expanded"
                } else {
                    "compact"
                }
            ));
            return Ok(());
        }
        if key.code == KeyCode::Char('w') && ctrl {
            if matches!(self.overlay, Overlay::Workers { .. }) {
                self.overlay = Overlay::None;
            } else {
                self.overlay = Overlay::Workers { selected: 0 };
            }
            return Ok(());
        }

        // Priority 2: Active interactive overlay (`/tree`, `/workers`, `/sessions`)
        if !matches!(self.overlay, Overlay::None) {
            return self.handle_overlay_key(terminal, key);
        }

        if key.code == KeyCode::Esc && self.input.completion().is_none() {
            self.hint_line = None;
            return Ok(());
        }

        // Priority 3: Multi-line input box with live `/command` & `@path` dropdown palette
        let agent_names: Vec<String> = self
            .store
            .list_available_agents()
            .into_iter()
            .map(|(n, _)| n)
            .collect();
        let worker_names: Vec<String> = self.workers.keys().cloned().collect();
        let comp_ctx = CompletionContext {
            cwd: &self.store.cwd,
            agents: &agent_names,
            workers: &worker_names,
        };
        match self.input.handle_key(key, comp_ctx) {
            InputAction::None => {
                self.hint_line = None;
            }
            InputAction::Submit(raw) => {
                self.hint_line = None;
                let trimmed = raw.trim();
                if !trimmed.is_empty() {
                    self.submit_user_input(terminal, trimmed.to_owned())?;
                }
            }
        }
        Ok(())
    }

    fn handle_approval_key(
        &mut self,
        terminal: &mut Terminal<CjkSafeBackend<Stdout>>,
        key: KeyEvent,
    ) -> io::Result<()> {
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

    fn handle_overlay_key(
        &mut self,
        terminal: &mut Terminal<CjkSafeBackend<Stdout>>,
        key: KeyEvent,
    ) -> io::Result<()> {
        let overlay = std::mem::replace(&mut self.overlay, Overlay::None);
        match overlay {
            Overlay::None => {}
            Overlay::Tree {
                graph,
                mut selected,
                mut scroll,
            } => match key.code {
                KeyCode::Esc | KeyCode::Char('q') => {}
                KeyCode::Up | KeyCode::Char('k') => {
                    selected = selected.saturating_sub(1);
                    if selected < scroll {
                        scroll = selected;
                    }
                    self.overlay = Overlay::Tree {
                        graph,
                        selected,
                        scroll,
                    };
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    if selected + 1 < graph.rows.len() {
                        selected += 1;
                    }
                    if selected >= scroll + 6 {
                        scroll = selected.saturating_sub(5);
                    }
                    self.overlay = Overlay::Tree {
                        graph,
                        selected,
                        scroll,
                    };
                }
                KeyCode::Enter => {
                    if let Some(row) = graph.rows.get(selected) {
                        if !row.selectable_leaf {
                            self.hint_line =
                                Some(format!("node #{} ({}) cannot be a leaf", row.id, row.kind));
                            self.overlay = Overlay::Tree {
                                graph,
                                selected,
                                scroll,
                            };
                        } else {
                            let wid = self.focused_worker.clone();
                            let lid = row.id;
                            self.respawn_worker(terminal, &wid, None, Some(Some(lid)), None, None)?;
                            self.push_notice(
                                terminal,
                                Color::Cyan,
                                format!("⎇ [{wid}] switched active leaf -> #{lid} ({})", row.kind),
                            )?;
                        }
                    }
                }
                KeyCode::Char('f') => {
                    if let Some(row) = graph.rows.get(selected) {
                        let lid = row.id;
                        self.fork_focused_wal(terminal, None, Some(lid))?;
                    }
                }
                KeyCode::Char('i') => {
                    if let Some(row) = graph.rows.get(selected) {
                        self.print_wal_node_inspect(terminal, row)?;
                    }
                    self.overlay = Overlay::Tree {
                        graph,
                        selected,
                        scroll,
                    };
                }
                _ => {
                    self.overlay = Overlay::Tree {
                        graph,
                        selected,
                        scroll,
                    };
                }
            },
            Overlay::Workers { mut selected } => {
                let ids: Vec<String> = self.workers.keys().cloned().collect();
                match key.code {
                    KeyCode::Esc | KeyCode::Char('q') => {}
                    KeyCode::Up | KeyCode::Char('k') => {
                        selected = selected.saturating_sub(1);
                        self.overlay = Overlay::Workers { selected };
                    }
                    KeyCode::Down | KeyCode::Char('j') => {
                        if selected + 1 < ids.len() {
                            selected += 1;
                        }
                        self.overlay = Overlay::Workers { selected };
                    }
                    KeyCode::Enter => {
                        if let Some(id) = ids.get(selected) {
                            self.focused_worker = id.clone();
                            self.push_notice(
                                terminal,
                                Color::Cyan,
                                format!("▶ switched focus to worker `{id}`"),
                            )?;
                        }
                    }
                    KeyCode::Char('x') | KeyCode::Delete => {
                        if let Some(id) = ids.get(selected) {
                            if id == "main" {
                                self.hint_line = Some("cannot kill `main` worker".into());
                                self.overlay = Overlay::Workers { selected };
                            } else {
                                if let Some(mut w) = self.workers.remove(id) {
                                    w.handle.stop(libc::SIGTERM);
                                }
                                if self.focused_worker == *id {
                                    self.focused_worker = "main".into();
                                }
                                self.push_notice(
                                    terminal,
                                    Color::Yellow,
                                    format!("■ stopped worker `{id}`"),
                                )?;
                                self.overlay = Overlay::Workers {
                                    selected: selected.saturating_sub(1),
                                };
                            }
                        }
                    }
                    _ => {
                        self.overlay = Overlay::Workers { selected };
                    }
                }
            }
            Overlay::Sessions {
                items,
                mut selected,
            } => match key.code {
                KeyCode::Esc | KeyCode::Char('q') => {}
                KeyCode::Up | KeyCode::Char('k') => {
                    selected = selected.saturating_sub(1);
                    self.overlay = Overlay::Sessions { items, selected };
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    if selected + 1 < items.len() {
                        selected += 1;
                    }
                    self.overlay = Overlay::Sessions { items, selected };
                }
                KeyCode::Enter => {
                    if let Some(chosen) = items.into_iter().nth(selected) {
                        self.switch_session(terminal, chosen.session_dir, chosen.meta)?;
                    }
                }
                _ => {
                    self.overlay = Overlay::Sessions { items, selected };
                }
            },
        }
        Ok(())
    }

    fn switch_session(
        &mut self,
        terminal: &mut Terminal<CjkSafeBackend<Stdout>>,
        new_dir: PathBuf,
        new_meta: SessionMeta,
    ) -> io::Result<()> {
        self.shutdown_all_workers();
        self.session_dir = new_dir;
        self.policy = new_meta.approval_policy;
        self.meta = new_meta;
        self.focused_worker = "main".into();
        self.push_notice(
            terminal,
            Color::Green,
            format!(
                "◈ switched to session {} ({})",
                self.meta.id, self.meta.title
            ),
        )?;
        self.replay_active_wal(terminal)?;
        self.spawn_main_worker(terminal)
    }

    fn submit_user_input(
        &mut self,
        terminal: &mut Terminal<CjkSafeBackend<Stdout>>,
        text: String,
    ) -> io::Result<()> {
        if text.starts_with('/') {
            return self.handle_slash_command(terminal, &text);
        }

        // Update session title on first user prompt.
        if self.meta.title == "New session" {
            self.meta.title = truncate_single_line(&text, 48);
            self.meta.updated_at = now_rfc3339();
            let _ = save_session_meta(&self.session_dir, &self.meta);
        }

        let width = self.term_width(terminal);
        let prefix = if self.focused_worker == "main" {
            "› ".to_owned()
        } else {
            format!("› [{}] ", self.focused_worker)
        };
        let mut lines = wrap_prefixed_lines(
            &text,
            &prefix,
            Style::default()
                .fg(Color::Green)
                .add_modifier(Modifier::BOLD),
            Style::default().fg(Color::White),
            width,
        );
        lines.push(Line::default());
        self.push_lines(terminal, lines)?;

        let wid = self.focused_worker.clone();
        // Respawn main automatically if it unexpectedly exited.
        if let Some(st) = self.workers.get(&wid)
            && !st.handle.alive
            && wid == "main"
        {
            self.spawn_main_worker(terminal)?;
        }
        if let Some(st) = self.workers.get_mut(&wid) {
            st.awaiting_user_cycle = true;
            if let Err(err) = st.handle.send_prompt(&text, "user") {
                self.push_notice(
                    terminal,
                    Color::Red,
                    format!("failed delivering prompt to `{wid}`: {err}"),
                )?;
            }
        }
        Ok(())
    }

    fn handle_slash_command(
        &mut self,
        terminal: &mut Terminal<CjkSafeBackend<Stdout>>,
        raw: &str,
    ) -> io::Result<()> {
        let mut parts = raw.split_whitespace();
        let cmd = parts.next().unwrap_or("");
        let rest = raw[cmd.len()..].trim();

        match cmd {
            "/help" => {
                let help = "\
Commands:
  /tree                              Browse Tree WAL branches (Enter switch leaf, f fork, i inspect)
  /fork [name] [--leaf <id>]         Fork active WAL via `anix fork` and switch onto new branch
  /compact                           Trigger online context compaction (`Anix-Control: compact`)
  /sessions                          Browse & switch persistent sessions for this project
  /spawn [--keep] [--fork [leaf]] <agent> [prompt]
                                     Spawn a parallel `anix` worker with `--notify` back to `main`
  /workers                           Manage live workers & active `hold` leases (shortcut: Ctrl+W)
  /send <worker> <message>           Send a Maildir message to a running worker
  /inspect <node_id>                 Print full payload of a Tree WAL node
  /model <model_id>                  Switch focused worker's LLM model (respawns cleanly on WAL)
  /agent <path|name>                 Switch focused worker's `*.agent` script
  /mode <ask|auto|plan>              Set approval mode (shortcut: Shift+Tab)
  /new                               Start a fresh persistent session
  /quit                              Shut down all workers and exit

Keys:
  Enter submit · Alt+Enter / Ctrl+J newline · Tab complete /cmd & @path
  Shift+Tab cycle ask/auto/plan · Ctrl+O toggle thinking/tool expansion · Ctrl+W workers · Ctrl+C interrupt";
                self.push_notice(terminal, Color::Gray, help)
            }
            "/quit" | "/exit" => {
                self.should_quit = true;
                Ok(())
            }
            "/tree" => {
                let (wal_path, leaf) = self.focused_wal_and_leaf();
                match WalGraph::load(&wal_path, leaf) {
                    Ok(graph) if !graph.rows.is_empty() => {
                        let selected = graph
                            .rows
                            .iter()
                            .position(|r| r.is_head)
                            .unwrap_or(graph.rows.len() - 1);
                        let scroll = selected.saturating_sub(4);
                        self.overlay = Overlay::Tree {
                            graph,
                            selected,
                            scroll,
                        };
                        Ok(())
                    }
                    Ok(_) => self.push_notice(
                        terminal,
                        Color::DarkGray,
                        "WAL is empty — send a prompt first before opening /tree",
                    ),
                    Err(err) => self.push_notice(
                        terminal,
                        Color::Red,
                        format!("failed reading WAL `{}`: {err}", wal_path.display()),
                    ),
                }
            }
            "/fork" => {
                let tokens: Vec<&str> = rest.split_whitespace().collect();
                let mut name: Option<String> = None;
                let mut leaf: Option<u64> = None;
                let mut i = 0;
                while i < tokens.len() {
                    if tokens[i] == "--leaf" && i + 1 < tokens.len() {
                        match tokens[i + 1].parse::<u64>() {
                            Ok(id) => leaf = Some(id),
                            Err(_) => {
                                return self.push_notice(
                                    terminal,
                                    Color::Red,
                                    "usage: /fork [name] [--leaf <node_id>]",
                                );
                            }
                        }
                        i += 2;
                    } else if name.is_none() {
                        name = Some(tokens[i].to_owned());
                        i += 1;
                    } else {
                        i += 1;
                    }
                }
                self.fork_focused_wal(terminal, name.as_deref(), leaf)
            }
            "/compact" => {
                let wid = self.focused_worker.clone();
                if let Some(st) = self.workers.get_mut(&wid) {
                    match st.handle.send_compact() {
                        Ok(()) => self.push_notice(
                            terminal,
                            Color::Cyan,
                            format!("🗜 sent `Anix-Control: compact` to `{wid}`"),
                        ),
                        Err(err) => self.push_notice(
                            terminal,
                            Color::Red,
                            format!("compact delivery failed: {err}"),
                        ),
                    }
                } else {
                    Ok(())
                }
            }
            "/sessions" | "/resume" => {
                let items = self.store.list_sessions();
                if items.is_empty() {
                    self.push_notice(terminal, Color::DarkGray, "no saved sessions yet")
                } else {
                    self.overlay = Overlay::Sessions { items, selected: 0 };
                    Ok(())
                }
            }
            "/workers" => {
                self.overlay = Overlay::Workers { selected: 0 };
                Ok(())
            }
            "/spawn" => self.cmd_spawn_worker(terminal, rest),
            "/send" => {
                let mut sp = rest.splitn(2, char::is_whitespace);
                let Some(target) = sp.next().filter(|s| !s.is_empty()) else {
                    return self.push_notice(
                        terminal,
                        Color::Red,
                        "usage: /send <worker_id> <message>",
                    );
                };
                let Some(msg) = sp.next().map(str::trim).filter(|s| !s.is_empty()) else {
                    return self.push_notice(
                        terminal,
                        Color::Red,
                        "usage: /send <worker_id> <message>",
                    );
                };
                if let Some(st) = self.workers.get_mut(target) {
                    match st.handle.send_prompt(msg, "user") {
                        Ok(_) => self.push_notice(
                            terminal,
                            Color::Green,
                            format!("✉ delivered message to worker `{target}`"),
                        ),
                        Err(err) => self.push_notice(
                            terminal,
                            Color::Red,
                            format!("failed sending to `{target}`: {err}"),
                        ),
                    }
                } else {
                    self.push_notice(
                        terminal,
                        Color::Red,
                        format!("unknown worker `{target}` (use /workers to inspect)"),
                    )
                }
            }
            "/inspect" => {
                let Ok(node_id) = rest.parse::<u64>() else {
                    return self.push_notice(terminal, Color::Red, "usage: /inspect <node_id>");
                };
                let (wal_path, leaf) = self.focused_wal_and_leaf();
                match WalGraph::load(&wal_path, leaf) {
                    Ok(graph) => match graph.find_node(node_id) {
                        Some(row) => self.print_wal_node_inspect(terminal, row),
                        None => self.push_notice(
                            terminal,
                            Color::Yellow,
                            format!("node #{node_id} not found in {}", wal_path.display()),
                        ),
                    },
                    Err(err) => {
                        self.push_notice(terminal, Color::Red, format!("failed reading WAL: {err}"))
                    }
                }
            }
            "/model" => {
                if rest.is_empty() {
                    return self.push_notice(terminal, Color::Red, "usage: /model <model_id>");
                }
                let wid = self.focused_worker.clone();
                self.respawn_worker(
                    terminal,
                    &wid,
                    None,
                    None,
                    Some(Some(rest.to_owned())),
                    None,
                )?;
                self.push_notice(
                    terminal,
                    Color::Cyan,
                    format!("◈ [{wid}] switched model -> {rest}"),
                )
            }
            "/agent" => {
                if rest.is_empty() {
                    let available = self.store.list_available_agents();
                    let names: Vec<String> = available.into_iter().map(|(n, _)| n).collect();
                    let listing = if names.is_empty() {
                        "none in ~/.config/anix/agents/".to_owned()
                    } else {
                        names.join(", ")
                    };
                    return self.push_notice(
                        terminal,
                        Color::Gray,
                        format!("usage: /agent <path|name> (discovered: {listing})"),
                    );
                }
                match self.store.resolve_agent_path(Some(rest)) {
                    Ok(path) => {
                        let wid = self.focused_worker.clone();
                        self.respawn_worker(terminal, &wid, None, None, None, Some(path.clone()))?;
                        self.push_notice(
                            terminal,
                            Color::Cyan,
                            format!("◈ [{wid}] switched agent -> {}", path.display()),
                        )
                    }
                    Err(err) => self.push_notice(terminal, Color::Red, err.to_string()),
                }
            }
            "/mode" => {
                if rest.is_empty() {
                    return self.push_notice(
                        terminal,
                        Color::Gray,
                        format!(
                            "current mode: {} (usage: /mode ask|auto|plan or Shift+Tab)",
                            self.policy.as_str()
                        ),
                    );
                }
                match ApprovalPolicy::parse_str(rest) {
                    Ok(p) => {
                        self.policy = p;
                        self.meta.approval_policy = p;
                        let _ = save_session_meta(&self.session_dir, &self.meta);
                        self.push_notice(
                            terminal,
                            Color::Cyan,
                            format!("◈ approval mode -> {}", p.as_str()),
                        )
                    }
                    Err(msg) => self.push_notice(terminal, Color::Red, msg),
                }
            }
            "/new" | "/clear" => {
                let (dir, meta) = self.store.create_session(
                    self.meta.agent_path.clone(),
                    self.meta.model.clone(),
                    self.meta.provider.clone(),
                    self.policy,
                )?;
                self.switch_session(terminal, dir, meta)
            }
            other => self.push_notice(
                terminal,
                Color::Yellow,
                format!("unknown command `{other}` (try /help)"),
            ),
        }
    }

    fn focused_wal_and_leaf(&self) -> (PathBuf, Option<u64>) {
        if let Some(st) = self.workers.get(&self.focused_worker) {
            (st.handle.spec.wal_path.clone(), st.handle.current_leaf)
        } else {
            (self.meta.active_wal.clone(), self.meta.active_leaf)
        }
    }

    fn fork_focused_wal(
        &mut self,
        terminal: &mut Terminal<CjkSafeBackend<Stdout>>,
        name: Option<&str>,
        leaf: Option<u64>,
    ) -> io::Result<()> {
        let (src_wal, cur_leaf) = self.focused_wal_and_leaf();
        if !src_wal.is_file() {
            return self.push_notice(
                terminal,
                Color::Yellow,
                "cannot fork: active WAL does not exist yet (send a prompt first)",
            );
        }
        let target_leaf = leaf.or(cur_leaf);
        let dst_wal = self.store.next_branch_wal_path(&self.session_dir, name);
        match run_anix_fork(&self.anix_bin, &src_wal, target_leaf, &dst_wal) {
            Ok(msg) => {
                let wid = self.focused_worker.clone();
                self.respawn_worker(
                    terminal,
                    &wid,
                    Some(dst_wal.clone()),
                    Some(None),
                    None,
                    None,
                )?;
                self.push_notice(
                    terminal,
                    Color::Green,
                    format!(
                        "⎇ forked -> {} {}",
                        dst_wal
                            .file_name()
                            .map(|s| s.to_string_lossy().to_string())
                            .unwrap_or_default(),
                        msg
                    ),
                )
            }
            Err(err) => {
                self.push_notice(terminal, Color::Red, format!("`anix fork` failed: {err}"))
            }
        }
    }

    fn cmd_spawn_worker(
        &mut self,
        terminal: &mut Terminal<CjkSafeBackend<Stdout>>,
        rest: &str,
    ) -> io::Result<()> {
        let tokens: Vec<&str> = rest.split_whitespace().collect();
        if tokens.is_empty() {
            return self.push_notice(
                terminal,
                Color::Red,
                "usage: /spawn [--keep] [--fork [leaf_id]] <agent> [prompt...]",
            );
        }
        let mut keep_alive = false;
        let mut do_fork = false;
        let mut fork_leaf: Option<u64> = None;
        let mut idx = 0usize;
        while idx < tokens.len() {
            match tokens[idx] {
                "--keep" => {
                    keep_alive = true;
                    idx += 1;
                }
                "--fork" => {
                    do_fork = true;
                    idx += 1;
                    if idx < tokens.len()
                        && let Ok(lid) = tokens[idx].parse::<u64>()
                    {
                        fork_leaf = Some(lid);
                        idx += 1;
                    }
                }
                _ => break,
            }
        }
        let Some(&agent_spec) = tokens.get(idx) else {
            return self.push_notice(
                terminal,
                Color::Red,
                "usage: /spawn [--keep] [--fork [leaf_id]] <agent> [prompt...]",
            );
        };
        let prompt = if idx + 1 < tokens.len() {
            Some(tokens[idx + 1..].join(" "))
        } else {
            None
        };

        let agent_path = match self.store.resolve_agent_path(Some(agent_spec)) {
            Ok(p) => p,
            Err(err) => return self.push_notice(terminal, Color::Red, err.to_string()),
        };

        let worker_id = format!("w{}", self.next_worker_seq);
        self.next_worker_seq += 1;
        let worker_dir = self.session_dir.join("workers").join(&worker_id);
        let worker_inbox = worker_dir.join("inbox");
        ensure_maildir(&worker_inbox)?;

        let wal_path = if do_fork {
            let (src_wal, cur_leaf) = self.focused_wal_and_leaf();
            if !src_wal.is_file() {
                return self.push_notice(
                    terminal,
                    Color::Yellow,
                    "cannot --fork: current WAL does not exist yet",
                );
            }
            let dst = worker_dir.join("forked.wal.jsonl");
            if let Err(err) = run_anix_fork(&self.anix_bin, &src_wal, fork_leaf.or(cur_leaf), &dst)
            {
                return self.push_notice(
                    terminal,
                    Color::Red,
                    format!("`anix fork` for worker failed: {err}"),
                );
            }
            dst
        } else {
            worker_dir.join("worker.wal.jsonl")
        };

        // One-shot prompt workers idle-exit after 2s so `anix --notify` automatically posts back to `main`.
        let idle_timeout_secs = if prompt.is_some() && !keep_alive {
            2
        } else {
            0
        };
        let spec = self.build_worker_spec(
            worker_id.clone(),
            agent_path.clone(),
            wal_path,
            worker_inbox,
            None,
            self.meta.model.clone(),
            self.meta.provider.clone(),
            Some(self.main_inbox_dir()),
            idle_timeout_secs,
        );

        match WorkerHandle::spawn(spec, self.worker_event_tx.clone()) {
            Ok(mut handle) => {
                if let Some(ref p) = prompt {
                    let _ = handle.send_prompt(p, "main");
                }
                let pid = handle.pid;
                self.workers
                    .insert(worker_id.clone(), WorkerUiState::new(handle));
                let mode_tag = if idle_timeout_secs > 0 {
                    "auto-notify on completion"
                } else {
                    "persistent"
                };
                self.push_notice(
                    terminal,
                    Color::Cyan,
                    format!(
                        "⚡ spawned worker `{worker_id}` (pid={pid}, {mode_tag}, agent={})",
                        agent_path
                            .file_name()
                            .map(|s| s.to_string_lossy().to_string())
                            .unwrap_or_default()
                    ),
                )
            }
            Err(err) => self.push_notice(
                terminal,
                Color::Red,
                format!("failed spawning worker `{worker_id}`: {err}"),
            ),
        }
    }

    fn print_wal_node_inspect(
        &self,
        terminal: &mut Terminal<CjkSafeBackend<Stdout>>,
        row: &crate::wal_view::WalNodeRow,
    ) -> io::Result<()> {
        let width = self.term_width(terminal);
        let mut lines = vec![Line::from(vec![
            Span::styled(
                format!("╭─ node #{} ", row.id),
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!(
                    "kind={} parent={} turn={} ts={}",
                    row.kind,
                    row.parent_id
                        .map(|p| format!("#{p}"))
                        .unwrap_or_else(|| "root".into()),
                    row.turn_id
                        .map(|t| t.to_string())
                        .unwrap_or_else(|| "-".into()),
                    row.ts
                ),
                Style::default().fg(Color::DarkGray),
            ),
        ])];
        lines.extend(wrap_prefixed_lines(
            &row.full_text,
            "│ ",
            Style::default().fg(Color::DarkGray),
            Style::default().fg(Color::White),
            width,
        ));
        lines.push(Line::from(Span::styled(
            "╰─",
            Style::default().fg(Color::DarkGray),
        )));
        self.push_lines(terminal, lines)
    }

    fn render(&self, f: &mut Frame<'_>) {
        let area = f.area();

        // When the live slash / `@path` completion palette is open, dock the sleek prompt
        // directly above the live dropdown (Claude Code / Antigravity CLI style).
        if self.active_approval.is_none()
            && matches!(self.overlay, Overlay::None)
            && let Some(menu) = self.input.completion()
        {
            let chunks = Layout::default()
                .direction(Direction::Vertical)
                .constraints([
                    Constraint::Length(3), // Ruled prompt line (`─` + `> /...` + `─`)
                    Constraint::Min(7),    // Live completion rows + `↓ N more` + nav footer
                    Constraint::Length(1), // 1-line live status bar
                ])
                .split(area);
            self.render_input_box(f, chunks[0]);
            self.render_completion_palette(f, menu, chunks[1]);
            self.render_status_bar(f, chunks[2]);
            return;
        }

        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Min(5),    // Live streaming / Approval Card / Overlay
                Constraint::Length(4), // Multi-line ruled input area
                Constraint::Length(1), // 1-line live status bar
            ])
            .split(area);

        if let Some(ref active) = self.active_approval {
            self.render_approval_card(f, active, chunks[0]);
        } else {
            match &self.overlay {
                Overlay::None => self.render_live_stream(f, chunks[0]),
                Overlay::Tree {
                    graph,
                    selected,
                    scroll,
                } => self.render_tree_overlay(f, graph, *selected, *scroll, chunks[0]),
                Overlay::Workers { selected } => {
                    self.render_workers_overlay(f, *selected, chunks[0])
                }
                Overlay::Sessions { items, selected } => {
                    self.render_sessions_overlay(f, items, *selected, chunks[0]);
                }
            }
        }

        self.render_input_box(f, chunks[1]);
        self.render_status_bar(f, chunks[2]);
    }

    fn render_completion_palette(&self, f: &mut Frame<'_>, menu: &CompletionMenu, area: Rect) {
        let visible_rows = COMPLETION_VISIBLE_ROWS.min(area.height.saturating_sub(2) as usize);
        let col_width = 24usize.min((area.width as usize / 2).max(14));
        let desc_max = (area.width as usize).saturating_sub(col_width + 4).max(10);

        let mut lines = Vec::with_capacity(visible_rows + 3);
        for (offset, item) in menu.visible_slice(visible_rows).iter().enumerate() {
            let abs_idx = menu.scroll + offset;
            let is_sel = abs_idx == menu.selected;
            let indicator = if is_sel { "> " } else { "  " };
            let label = pad_display_width(&item.label, col_width);
            let desc = truncate_single_line(&item.description, desc_max);

            let (ind_style, lbl_style, desc_style) = if is_sel {
                (
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                    Style::default().fg(Color::White),
                )
            } else {
                (
                    Style::default().fg(Color::DarkGray),
                    Style::default().fg(Color::White),
                    Style::default().fg(Color::Gray),
                )
            };
            lines.push(Line::from(vec![
                Span::styled(indicator, ind_style),
                Span::styled(label, lbl_style),
                Span::raw("  "),
                Span::styled(desc, desc_style),
            ]));
        }

        let above = menu.hidden_above();
        let below = menu.hidden_below(visible_rows);
        if above > 0 || below > 0 {
            let mut parts = Vec::new();
            if above > 0 {
                parts.push(format!("↑ {above} more"));
            }
            if below > 0 {
                parts.push(format!("↓ {below} more"));
            }
            lines.push(Line::from(Span::styled(
                format!("  {}", parts.join(" · ")),
                Style::default().fg(Color::DarkGray),
            )));
        }

        if lines.len() + 1 < area.height as usize {
            lines.push(Line::default());
        }
        lines.push(Line::from(vec![
            Span::styled("  ↑/↓ ", Style::default().fg(Color::White)),
            Span::styled("Navigate · ", Style::default().fg(Color::DarkGray)),
            Span::styled("enter ", Style::default().fg(Color::White)),
            Span::styled("Select · ", Style::default().fg(Color::DarkGray)),
            Span::styled("tab ", Style::default().fg(Color::White)),
            Span::styled("Complete · ", Style::default().fg(Color::DarkGray)),
            Span::styled("esc ", Style::default().fg(Color::White)),
            Span::styled("Cancel", Style::default().fg(Color::DarkGray)),
        ]));

        f.render_widget(Paragraph::new(lines), area);
    }

    fn render_live_stream(&self, f: &mut Frame<'_>, area: Rect) {
        let mut lines: Vec<Line<'static>> = Vec::new();
        let width = area.width.saturating_sub(2) as usize;
        if let Some(st) = self.workers.get(&self.focused_worker) {
            if st.handle.busy {
                let spin = SPINNER_FRAMES[self.spinner_idx];
                lines.push(Line::from(vec![
                    Span::styled(format!("{spin} "), Style::default().fg(Color::Cyan)),
                    Span::styled(
                        format!("[{}] streaming…", self.focused_worker),
                        Style::default().fg(Color::Cyan),
                    ),
                ]));
            }
            if !st.active_tools.is_empty() {
                for t in &st.active_tools {
                    lines.push(Line::from(vec![
                        Span::styled("⚙ ", Style::default().fg(Color::Yellow)),
                        Span::styled(
                            format!(
                                "{} $ {}",
                                t.name,
                                truncate_single_line(&t.cmd, width.saturating_sub(10))
                            ),
                            Style::default().fg(Color::Yellow),
                        ),
                    ]));
                }
            }
            if !st.streaming_reasoning.trim().is_empty() && st.streaming_text.trim().is_empty() {
                let tail = tail_preview(&st.streaming_reasoning, width.saturating_sub(14));
                lines.push(Line::from(vec![
                    Span::styled("💭 ", Style::default().fg(Color::DarkGray)),
                    Span::styled(
                        tail,
                        Style::default()
                            .fg(Color::DarkGray)
                            .add_modifier(Modifier::ITALIC),
                    ),
                ]));
            }
            if !st.streaming_text.is_empty() {
                let rendered = render_markdown(&st.streaming_text, width);
                let avail = area.height.saturating_sub(lines.len() as u16) as usize;
                let start = rendered.len().saturating_sub(avail.max(1));
                lines.extend(rendered.into_iter().skip(start));
            }
        }

        if lines.is_empty() {
            if let Some(ref hint) = self.hint_line {
                lines.push(Line::from(Span::styled(
                    format!("  {hint}"),
                    Style::default().fg(Color::DarkGray),
                )));
            } else {
                lines.push(Line::from(Span::styled(
                    "  Ready — type a message or /help (Tab completes /commands & @paths)",
                    Style::default().fg(Color::DarkGray),
                )));
            }
        }

        let block = Block::default()
            .borders(Borders::TOP)
            .border_style(Style::default().fg(Color::DarkGray));
        f.render_widget(Paragraph::new(lines).block(block), area);
    }

    fn render_approval_card(&self, f: &mut Frame<'_>, active: &ActiveApproval, area: Rect) {
        let queued = self.approval_queue.len();
        let title = if queued > 0 {
            format!(
                " ⚠ Approve Shell Command [{}] (+{queued} queued) ",
                active.pending.worker_id
            )
        } else {
            format!(" ⚠ Approve Shell Command [{}] ", active.pending.worker_id)
        };
        let block = Block::default()
            .title(Span::styled(
                title,
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            ))
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Yellow));

        let inner_w = area.width.saturating_sub(4) as usize;
        let argv0 = extract_argv0(&active.pending.cmd).unwrap_or_else(|| "cmd".into());
        let mut lines = vec![
            Line::from(vec![
                Span::styled(
                    "$ ",
                    Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    truncate_single_line(&active.pending.cmd, inner_w.saturating_sub(2)),
                    Style::default()
                        .fg(Color::White)
                        .add_modifier(Modifier::BOLD),
                ),
            ]),
            Line::from(Span::styled(
                format!(
                    "cwd: {}",
                    truncate_single_line(&active.pending.cwd, inner_w.saturating_sub(5))
                ),
                Style::default().fg(Color::DarkGray),
            )),
        ];

        match &active.submode {
            ApprovalSubMode::Choose => {
                lines.push(Line::from(vec![
                    Span::styled(
                        "[y] ",
                        Style::default()
                            .fg(Color::Green)
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::raw("Allow once  "),
                    Span::styled(
                        "[a] ",
                        Style::default()
                            .fg(Color::Cyan)
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::raw(format!("Always `{argv0}`  ")),
                    Span::styled(
                        "[n] ",
                        Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
                    ),
                    Span::raw("Deny  "),
                    Span::styled(
                        "[e] ",
                        Style::default()
                            .fg(Color::Yellow)
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::raw("Edit"),
                ]));
            }
            ApprovalSubMode::DenyReason { buf } => {
                lines.push(Line::from(vec![
                    Span::styled(
                        "Deny reason (Enter confirms, Esc back): ",
                        Style::default().fg(Color::Red),
                    ),
                    Span::styled(format!("{buf}▏"), Style::default().fg(Color::White)),
                ]));
            }
            ApprovalSubMode::EditCmd { buf } => {
                lines.push(Line::from(vec![
                    Span::styled(
                        "Edit cmd (Enter executes, Esc back): ",
                        Style::default().fg(Color::Yellow),
                    ),
                    Span::styled(format!("{buf}▏"), Style::default().fg(Color::White)),
                ]));
            }
        }

        f.render_widget(Paragraph::new(lines).block(block), area);
    }

    fn render_tree_overlay(
        &self,
        f: &mut Frame<'_>,
        graph: &WalGraph,
        selected: usize,
        scroll: usize,
        area: Rect,
    ) {
        let base_hint = graph
            .branch_base
            .as_deref()
            .map(|b| format!(" base={b}"))
            .unwrap_or_default();
        let block = Block::default()
            .title(Span::styled(
                format!(
                    " ⎇ Tree WAL ({} nodes{base_hint}) · Enter switch · f fork · i inspect · Esc close ",
                    graph.rows.len()
                ),
                Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
            ))
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Cyan));

        let visible = area.height.saturating_sub(2) as usize;
        let inner_w = area.width.saturating_sub(4) as usize;
        let mut lines = Vec::new();
        for (idx, row) in graph.rows.iter().enumerate().skip(scroll).take(visible) {
            let is_sel = idx == selected;
            let mark = if row.is_head { "*" } else { " " };
            let raw = format!(
                "{mark} {}#{} [{}] {}",
                row.tree_prefix, row.id, row.kind, row.preview
            );
            let clipped = truncate_single_line(&raw, inner_w);
            let style = if is_sel {
                Style::default()
                    .bg(Color::Blue)
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD)
            } else if row.on_active_lineage {
                Style::default().fg(Color::White)
            } else {
                Style::default().fg(Color::DarkGray)
            };
            lines.push(Line::from(Span::styled(clipped, style)));
        }
        f.render_widget(Paragraph::new(lines).block(block), area);
    }

    fn render_workers_overlay(&self, f: &mut Frame<'_>, selected: usize, area: Rect) {
        let block = Block::default()
            .title(Span::styled(
                " ⚡ Workers · Enter focus · x stop · Esc close ",
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ))
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Cyan));

        let mut lines = Vec::new();
        let now = Instant::now();
        for (idx, (id, st)) in self.workers.iter().enumerate() {
            let focus = if *id == self.focused_worker {
                "▶"
            } else {
                " "
            };
            let state = if !st.handle.alive {
                "exited"
            } else if st.handle.busy {
                "busy"
            } else {
                "idle"
            };
            let holds_str = if st.handle.holds.is_empty() {
                String::new()
            } else {
                let max_rem = st
                    .handle
                    .holds
                    .iter()
                    .map(|h| h.deadline.saturating_duration_since(now).as_secs())
                    .max()
                    .unwrap_or(0);
                format!(" holds:{}({max_rem}s)", st.handle.holds.len())
            };
            let leaf_str = st
                .handle
                .current_leaf
                .map(|l| format!("#{l}"))
                .unwrap_or_else(|| "-".into());
            let row = format!(
                "{focus} {id:<8} pid:{:<6} {state:<6} leaf:{leaf_str:<6} model:{}{holds_str}",
                st.handle.pid,
                if st.handle.current_model.is_empty() {
                    "-"
                } else {
                    &st.handle.current_model
                }
            );
            let style = if idx == selected {
                Style::default()
                    .bg(Color::Blue)
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::White)
            };
            lines.push(Line::from(Span::styled(row, style)));
        }
        f.render_widget(Paragraph::new(lines).block(block), area);
    }

    fn render_sessions_overlay(
        &self,
        f: &mut Frame<'_>,
        items: &[SessionSummary],
        selected: usize,
        area: Rect,
    ) {
        let block = Block::default()
            .title(Span::styled(
                " ◈ Sessions · Enter switch · Esc close ",
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ))
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Cyan));

        let visible = area.height.saturating_sub(2) as usize;
        let inner_w = area.width.saturating_sub(4) as usize;
        let mut lines = Vec::new();
        for (idx, item) in items.iter().enumerate().take(visible) {
            let active = if item.meta.id == self.meta.id {
                "*"
            } else {
                " "
            };
            let kb = item.wal_bytes / 1024;
            let row = format!(
                "{active} {}  {:>4}KB  {}",
                item.meta.id, kb, item.meta.title
            );
            let style = if idx == selected {
                Style::default()
                    .bg(Color::Blue)
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::White)
            };
            lines.push(Line::from(Span::styled(
                truncate_single_line(&row, inner_w),
                style,
            )));
        }
        f.render_widget(Paragraph::new(lines).block(block), area);
    }

    fn render_input_box(&self, f: &mut Frame<'_>, area: Rect) {
        let prefix = if self.focused_worker == "main" {
            "> ".to_owned()
        } else {
            format!("> [{}] ", self.focused_worker)
        };
        let prefix_w = prefix.width();
        let is_focused = self.active_approval.is_none() && matches!(self.overlay, Overlay::None);
        let prefix_color = if is_focused {
            Color::Cyan
        } else {
            Color::DarkGray
        };
        let block = Block::default()
            .borders(Borders::TOP | Borders::BOTTOM)
            .border_style(Style::default().fg(Color::DarkGray));

        let inner = block.inner(area);
        let text_w = (inner.width as usize).saturating_sub(prefix_w).max(4);
        let (visual_lines, (cur_row, cur_col)) = self.input.visual_lines_and_cursor(text_w);
        let max_rows = inner.height.max(1) as usize;
        let scroll_y = if cur_row >= max_rows {
            cur_row + 1 - max_rows
        } else {
            0
        };
        let cont_pad = " ".repeat(prefix_w);
        let show_placeholder = self.input.is_empty() && is_focused;
        let visible: Vec<Line<'static>> = if show_placeholder {
            vec![Line::from(vec![
                Span::styled(
                    prefix,
                    Style::default()
                        .fg(prefix_color)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    "Type a prompt, `/` for commands, or `@` for files…",
                    Style::default().fg(Color::DarkGray),
                ),
            ])]
        } else {
            visual_lines
                .into_iter()
                .enumerate()
                .skip(scroll_y)
                .take(max_rows)
                .map(|(row_idx, line)| {
                    if row_idx == 0 {
                        Line::from(vec![
                            Span::styled(
                                prefix.clone(),
                                Style::default()
                                    .fg(prefix_color)
                                    .add_modifier(Modifier::BOLD),
                            ),
                            Span::raw(line),
                        ])
                    } else {
                        Line::from(vec![Span::raw(cont_pad.clone()), Span::raw(line)])
                    }
                })
                .collect()
        };

        f.render_widget(Paragraph::new(visible).block(block), area);

        if is_focused {
            let cx = inner.x + ((prefix_w + cur_col) as u16).min(inner.width.saturating_sub(1));
            let cy = inner.y + ((cur_row - scroll_y) as u16).min(inner.height.saturating_sub(1));
            f.set_cursor_position((cx, cy));
        }
    }

    fn render_status_bar(&self, f: &mut Frame<'_>, area: Rect) {
        let st = self.workers.get(&self.focused_worker);
        let (busy, model, leaf, ctx_str, usage_str, holds_count) = match st {
            Some(w) => {
                let model = if w.handle.current_model.is_empty() {
                    w.handle
                        .spec
                        .model
                        .as_deref()
                        .unwrap_or("default")
                        .to_owned()
                } else {
                    w.handle.current_model.clone()
                };
                let leaf = w
                    .handle
                    .current_leaf
                    .map(|l| format!("#{l}"))
                    .unwrap_or_else(|| "-".into());
                let ctx = w
                    .handle
                    .context
                    .as_ref()
                    .map(|c| {
                        let pct = if c.max_tokens > 0 {
                            ((c.tokens * 100) / u64::from(c.max_tokens)).min(100)
                        } else {
                            0
                        };
                        format!(
                            "{pct}% ({}/{})",
                            fmt_k(c.tokens),
                            fmt_k(u64::from(c.max_tokens))
                        )
                    })
                    .unwrap_or_else(|| "-".into());
                let usage = format!(
                    "{}/{}",
                    fmt_k(w.handle.session_usage.input_tokens),
                    fmt_k(w.handle.session_usage.output_tokens)
                );
                (w.handle.busy, model, leaf, ctx, usage, w.handle.holds.len())
            }
            None => (false, "-".into(), "-".into(), "-".into(), "0/0".into(), 0),
        };

        let mode_color = match self.policy {
            ApprovalPolicy::Ask => Color::Yellow,
            ApprovalPolicy::Auto => Color::Green,
            ApprovalPolicy::Plan => Color::Magenta,
        };
        let dot = if busy {
            SPINNER_FRAMES[self.spinner_idx]
        } else {
            "●"
        };
        let dot_color = if busy { Color::Cyan } else { Color::Green };

        let bar = Line::from(vec![
            Span::styled(format!(" {dot} "), Style::default().fg(dot_color)),
            Span::styled(
                self.focused_worker.clone(),
                Style::default()
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(" │ ", Style::default().fg(Color::DarkGray)),
            Span::styled(model, Style::default().fg(Color::Cyan)),
            Span::styled(" │ ctx ", Style::default().fg(Color::DarkGray)),
            Span::styled(ctx_str, Style::default().fg(Color::White)),
            Span::styled(" │ tok ", Style::default().fg(Color::DarkGray)),
            Span::styled(usage_str, Style::default().fg(Color::Gray)),
            Span::styled(" │ mode:", Style::default().fg(Color::DarkGray)),
            Span::styled(
                self.policy.as_str(),
                Style::default().fg(mode_color).add_modifier(Modifier::BOLD),
            ),
            Span::styled(format!(" │ leaf:{leaf}"), Style::default().fg(Color::Gray)),
            Span::styled(
                format!(" │ holds:{holds_count} │ workers:{}", self.workers.len()),
                Style::default().fg(Color::DarkGray),
            ),
        ]);

        f.render_widget(
            Paragraph::new(bar).style(Style::default().bg(Color::Black)),
            area,
        );
    }
}

fn fmt_k(n: u64) -> String {
    if n >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    } else if n >= 1_000 {
        format!("{:.1}k", n as f64 / 1_000.0)
    } else {
        n.to_string()
    }
}

fn first_nonempty_line(s: &str) -> String {
    s.lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .map(|l| truncate_single_line(l, 64))
        .unwrap_or_else(|| "(no output)".into())
}

fn tail_preview(s: &str, max_cols: usize) -> String {
    let single = s.replace(['\r', '\n'], " ");
    let trimmed = single.trim();
    if trimmed.width() <= max_cols {
        return trimmed.to_owned();
    }
    let mut rev_chars = Vec::new();
    let mut w = 0usize;
    for ch in trimmed.chars().rev() {
        let cw = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
        if w + cw + 1 > max_cols {
            break;
        }
        rev_chars.push(ch);
        w += cw;
    }
    rev_chars.reverse();
    format!("…{}", rev_chars.into_iter().collect::<String>())
}

fn pad_display_width(raw: &str, target_cols: usize) -> String {
    let clipped = truncate_single_line(raw, target_cols);
    let w = clipped.width();
    if w >= target_cols {
        clipped
    } else {
        format!("{clipped}{}", " ".repeat(target_cols - w))
    }
}

fn truncate_single_line(raw: &str, max_cols: usize) -> String {
    let max_cols = max_cols.max(4);
    let single = raw.replace(['\r', '\n'], " ");
    let trimmed = single.trim();
    let mut out = String::new();
    let mut w = 0usize;
    for ch in trimmed.chars() {
        let cw = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
        if w + cw + 1 > max_cols {
            out.push('…');
            break;
        }
        out.push(ch);
        w += cw;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::buffer::Buffer;

    #[test]
    fn cjk_safe_backend_skips_wide_char_trailing_cells_in_insert_before() {
        let sample = "这个项目：Anix — POSIX 原生 Agent 内核";
        let width: u16 = 60;
        let area = Rect::new(0, 0, width, 1);
        let mut buf = Buffer::empty(area);
        Paragraph::new(Line::from(sample)).render(area, &mut buf);

        // Simulate `ratatui 0.29`'s `Terminal::draw_lines` (which yields EVERY raw buffer cell).
        let mut backend = CjkSafeBackend::new(Vec::<u8>::new());
        let w = width as usize;
        let iter = buf
            .content
            .iter()
            .enumerate()
            .map(|(i, c)| ((i % w) as u16, (i / w) as u16, c));
        backend.draw(iter).unwrap();
        backend.flush().unwrap();

        let rendered = String::from_utf8(backend.writer).unwrap();
        assert!(
            rendered.contains(sample),
            "expected contiguous CJK output without phantom spaces, got: {rendered:?}"
        );
        assert!(
            !rendered.contains("这 个"),
            "must NOT insert phantom space between CJK chars: {rendered:?}"
        );
    }
}
