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
use ratatui::buffer::{Buffer, Cell};
use ratatui::layout::{Constraint, Direction, Layout, Position, Rect, Size};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Widget};
use unicode_width::UnicodeWidthStr;

use crate::approve::{
    ApprovalResponseWire, ApprovalServer, PendingApproval, extract_argv0, try_auto_decide,
};
use crate::cli::{ApprovalPolicy, Cli};
use crate::config::{AgentConfig, BangMode};
use crate::input::{
    COMPLETION_VISIBLE_ROWS, CompletionContext, CompletionMenu, InputAction, InputBox,
};
use crate::markdown::{render_markdown, wrap_prefixed_lines};
use crate::session::{
    CustomSlashEntry, CustomSlashKind, SessionMeta, SessionStore, SessionSummary,
    format_relative_time, now_rfc3339, resolve_anix_bin, save_session_meta,
};
use crate::statusbar::{
    AVAILABLE_VARS, DEFAULT_STATUS_BAR, PRESET_FULL, PRESET_MINIMAL, StatusVars, detect_git_branch,
    render_status_bar, resolve_preset,
};
use crate::wal_view::{
    ContextBreakdown, RewindPoint, WalGraph, analyze_wal_context, load_recent_replay,
};
use crate::worker::{
    TrackedHold, WorkerEvent, WorkerHandle, WorkerSpec, ensure_maildir, run_anix_fork,
};

const RESUME_PAGE_SIZE: usize = 6;
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

/// Lightweight frame target exposing the subset of `ratatui::Frame` used by all UI widgets.
struct RenderTarget<'a> {
    area: Rect,
    buf: &'a mut Buffer,
    cursor: Option<(u16, u16)>,
}

impl RenderTarget<'_> {
    fn area(&self) -> Rect {
        self.area
    }

    fn render_widget<W: Widget>(&mut self, widget: W, area: Rect) {
        widget.render(area, self.buf);
    }

    fn set_cursor_position(&mut self, pos: (u16, u16)) {
        self.cursor = Some(pos);
    }
}

/// Dynamic-height inline terminal built on [`CjkSafeBackend`] + [`ratatui::buffer::Buffer`].
///
/// Unlike `ratatui::Viewport::Inline(fixed)`, this shrinks cleanly to 4 lines when idle and
/// expands organically ONLY when streaming, typing multi-line input, or opening `/`, `?`,
/// approval cards, or `/resume` — with zero wasted blank rows, zero Alt-Screen hijack, and
/// zero CJK double-width spacing bugs.
struct DynamicInlineTerminal<W: io::Write> {
    backend: CjkSafeBackend<W>,
    vp_top: u16,
    vp_height: u16,
    prev_buf: Option<Buffer>,
}

impl DynamicInlineTerminal<Stdout> {
    fn enter() -> io::Result<Self> {
        let mut backend = CjkSafeBackend::new(io::stdout());
        let pos = backend.get_cursor_position().unwrap_or(Position::new(0, 0));
        let size = backend.size().unwrap_or(Size::new(80, 24));
        Ok(Self {
            backend,
            vp_top: pos.y.min(size.height.saturating_sub(1)),
            vp_height: 0,
            prev_buf: None,
        })
    }
}

impl<W: io::Write> DynamicInlineTerminal<W> {
    fn size(&self) -> io::Result<Size> {
        self.backend.size()
    }

    fn on_resize(&mut self) {
        self.prev_buf = None;
        if let Ok(sz) = self.backend.size() {
            self.vp_top = self.vp_top.min(sz.height.saturating_sub(1));
        }
    }

    fn clear_viewport_for_external(&mut self) -> io::Result<()> {
        if self.vp_height > 0 {
            self.backend
                .set_cursor_position(Position::new(0, self.vp_top))?;
            self.backend.clear_region(ClearType::AfterCursor)?;
            self.vp_height = 0;
            self.prev_buf = None;
        }
        self.backend.show_cursor()?;
        self.backend.flush()
    }

    fn resync_after_external(&mut self) {
        self.prev_buf = None;
        self.vp_height = 0;
        let sz = self.backend.size().unwrap_or(Size::new(80, 24));
        if let Ok(pos) = self.backend.get_cursor_position() {
            self.vp_top = pos.y.min(sz.height.saturating_sub(1));
        } else {
            self.vp_top = sz.height.saturating_sub(1);
        }
    }

    fn insert_before<F>(&mut self, height: u16, draw_fn: F) -> io::Result<()>
    where
        F: FnOnce(&mut Buffer),
    {
        if height == 0 {
            return Ok(());
        }
        let sz = self.backend.size().unwrap_or(Size::new(80, 24));
        let screen_w = sz.width.max(20);
        let screen_h = sz.height.max(4);

        // Clear any live viewport rows before writing committed scrollback lines.
        if self.vp_height > 0 {
            self.backend.hide_cursor()?;
            self.backend
                .set_cursor_position(Position::new(0, self.vp_top))?;
            self.backend.clear_region(ClearType::AfterCursor)?;
            self.vp_height = 0;
            self.prev_buf = None;
        }

        let area = Rect::new(0, 0, screen_w, height);
        let mut buf = Buffer::empty(area);
        draw_fn(&mut buf);

        let w = screen_w as usize;
        let max_chunk = (screen_h.saturating_sub(1)).max(1) as usize;
        let mut row_start = 0usize;
        let total_rows = height as usize;

        while row_start < total_rows {
            let k = (total_rows - row_start).min(max_chunk);
            let k_u16 = k as u16;
            if self.vp_top.saturating_add(k_u16) > screen_h {
                let scroll = self.vp_top.saturating_add(k_u16) - screen_h;
                self.backend
                    .set_cursor_position(Position::new(0, screen_h.saturating_sub(1)))?;
                self.backend.append_lines(scroll)?;
                self.vp_top = self.vp_top.saturating_sub(scroll);
            }
            let slice = &buf.content[row_start * w..(row_start + k) * w];
            let base_y = self.vp_top;
            let iter = slice
                .iter()
                .enumerate()
                .map(|(i, c)| ((i % w) as u16, base_y + (i / w) as u16, c));
            self.backend.draw(iter)?;
            self.backend.flush()?;

            self.vp_top = self.vp_top.saturating_add(k_u16);
            if self.vp_top >= screen_h {
                self.backend
                    .set_cursor_position(Position::new(0, screen_h.saturating_sub(1)))?;
                self.backend.append_lines(1)?;
                self.vp_top = screen_h.saturating_sub(1);
            }
            row_start += k;
        }
        Ok(())
    }

    fn draw_dynamic<F>(&mut self, desired_height: u16, render_fn: F) -> io::Result<()>
    where
        F: FnOnce(&mut RenderTarget<'_>),
    {
        let sz = self.backend.size().unwrap_or(Size::new(80, 24));
        let screen_w = sz.width.max(20);
        let screen_h = sz.height.max(4);
        let h = desired_height.clamp(4, screen_h);

        self.backend.hide_cursor()?;

        if self.vp_top.saturating_add(h) > screen_h {
            let scroll = self.vp_top.saturating_add(h) - screen_h;
            self.backend
                .set_cursor_position(Position::new(0, screen_h.saturating_sub(1)))?;
            self.backend.append_lines(scroll)?;
            self.vp_top = self.vp_top.saturating_sub(scroll);
            self.prev_buf = None;
        } else if h < self.vp_height {
            self.backend
                .set_cursor_position(Position::new(0, self.vp_top.saturating_add(h)))?;
            self.backend.clear_region(ClearType::AfterCursor)?;
            self.prev_buf = None;
        }

        let area = Rect::new(0, 0, screen_w, h);
        let mut new_buf = Buffer::empty(area);
        let mut target = RenderTarget {
            area,
            buf: &mut new_buf,
            cursor: None,
        };
        render_fn(&mut target);
        let cursor = target.cursor;

        let base_y = self.vp_top;
        if let Some(ref old) = self.prev_buf
            && old.area == area
        {
            let diff = old.diff(&new_buf);
            let iter = diff.into_iter().map(|(x, y, c)| (x, base_y + y, c));
            self.backend.draw(iter)?;
        } else {
            let w = screen_w as usize;
            let iter = new_buf
                .content
                .iter()
                .enumerate()
                .map(|(i, c)| ((i % w) as u16, base_y + (i / w) as u16, c));
            self.backend.draw(iter)?;
        }

        if let Some((cx, cy)) = cursor {
            self.backend
                .set_cursor_position(Position::new(cx, base_y.saturating_add(cy)))?;
            self.backend.show_cursor()?;
        }
        self.backend.flush()?;

        self.prev_buf = Some(new_buf);
        self.vp_height = h;
        Ok(())
    }

    fn finish(&mut self) -> io::Result<()> {
        let sz = self.backend.size().unwrap_or(Size::new(80, 24));
        let bottom_y = self
            .vp_top
            .saturating_add(self.vp_height)
            .min(sz.height.saturating_sub(1));
        self.backend
            .set_cursor_position(Position::new(0, bottom_y))?;
        self.backend.append_lines(1)?;
        self.backend.show_cursor()?;
        self.backend.flush()
    }
}

type UiTerminal = DynamicInlineTerminal<Stdout>;

struct BangResult {
    cmd: String,
    exit_code: i32,
    output: String,
    override_mode: Option<BangMode>,
}

enum AppEvent {
    Key(KeyEvent),
    Resize,
    Tick,
    Worker(WorkerEvent),
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ResumeTab {
    Project,
    All,
}

enum ResumeMode {
    Browse,
    Rename { buf: String },
    ConfirmDelete,
}

struct ResumePicker {
    tab: ResumeTab,
    all_items: Vec<SessionSummary>,
    query: String,
    selected: usize,
    mode: ResumeMode,
    status_msg: Option<String>,
}

impl ResumePicker {
    fn filtered_indices(&self) -> Vec<usize> {
        let q_raw = self.query.trim();
        let (title_only, q_lower) = if let Some(rest) = q_raw.strip_prefix("title:") {
            (true, rest.trim().to_ascii_lowercase())
        } else {
            (false, q_raw.to_ascii_lowercase())
        };

        self.all_items
            .iter()
            .enumerate()
            .filter(|(_, item)| {
                if self.tab == ResumeTab::Project && !item.is_current_project {
                    return false;
                }
                if q_lower.is_empty() {
                    return true;
                }
                if title_only {
                    item.meta.title.to_ascii_lowercase().contains(&q_lower)
                } else {
                    item.meta.title.to_ascii_lowercase().contains(&q_lower)
                        || item.meta.id.to_ascii_lowercase().contains(&q_lower)
                        || item.project_label.to_ascii_lowercase().contains(&q_lower)
                }
            })
            .map(|(idx, _)| idx)
            .collect()
    }

    fn clamp_selection(&mut self) {
        let len = self.filtered_indices().len();
        if len == 0 {
            self.selected = 0;
        } else if self.selected >= len {
            self.selected = len - 1;
        }
    }

    fn selected_item(&self) -> Option<&SessionSummary> {
        let filtered = self.filtered_indices();
        let raw_idx = *filtered.get(self.selected)?;
        self.all_items.get(raw_idx)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ConfigTab {
    Settings,
    Status,
    Usage,
}

struct ConfigPicker {
    tab: ConfigTab,
    selected: usize,
    filter: String,
    status_msg: Option<String>,
}

struct RewindPicker {
    points: Vec<RewindPoint>,
    selected: usize,
    scroll: usize,
    choosing_action: bool,
    action_idx: usize,
}

enum Overlay {
    None,
    Shortcuts,
    Config(ConfigPicker),
    Rewind(RewindPicker),
    Tree {
        graph: WalGraph,
        selected: usize,
        scroll: usize,
    },
    Workers {
        selected: usize,
    },
    Sessions(ResumePicker),
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
    app_tx: mpsc::Sender<AppEvent>,

    agent_config_path: PathBuf,
    agent_config: AgentConfig,

    session_dir: PathBuf,
    meta: SessionMeta,
    policy: ApprovalPolicy,
    always_allowed: HashSet<String>,
    expand_details: bool,

    workers: BTreeMap<String, WorkerUiState>,
    focused_worker: String,
    next_worker_seq: u32,

    input: InputBox,
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
                let (dir, m) = store.create_session(
                    agent_path,
                    cli.model.clone(),
                    cli.provider.clone(),
                    effective_mode,
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
            effective_mode,
        )?;
        (dir, m, false)
    };

    if effective_mode != meta.approval_policy && !is_resumed {
        meta.approval_policy = effective_mode;
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
        policy,
        always_allowed,
        expand_details,
        workers: BTreeMap::new(),
        focused_worker: "main".into(),
        next_worker_seq: 1,
        input: InputBox::new(),
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

    let init_w = terminal.size().map(|s| s.width).unwrap_or(80);
    terminal.draw_dynamic(app.desired_viewport_height(init_w), |f| app.render(f))?;

    while !app.should_quit {
        let Ok(ev) = app_rx.recv() else { break };
        match ev {
            AppEvent::Key(key) => {
                app.handle_key(&mut terminal, key)?;
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
                Ok(AppEvent::Resize) => terminal.on_resize(),
                Ok(AppEvent::Tick) => app.on_tick(&mut terminal)?,
                Ok(AppEvent::Worker(w)) => app.handle_worker_event(&mut terminal, w)?,
                Ok(AppEvent::Approval(a)) => app.handle_incoming_approval(&mut terminal, a)?,
                Ok(AppEvent::BangDone(r)) => app.handle_bang_done(&mut terminal, r)?,
                Err(_) => break,
            }
        }
        if !app.should_quit {
            let cur_w = terminal.size().map(|s| s.width).unwrap_or(80);
            terminal.draw_dynamic(app.desired_viewport_height(cur_w), |f| app.render(f))?;
        }
    }

    stop_flag.store(true, Ordering::Relaxed);
    app.shutdown_all_workers();
    let _ = reader_handle.join();
    let _ = terminal.finish();
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
        let tips = Line::from(vec![Span::styled(
            "  /help · /tree · /fork · /spawn · /workers (^W) · /compact · ⇧Tab mode · ^O expand",
            Style::default().fg(Color::DarkGray),
        )]);
        self.push_lines(terminal, vec![header, tips, Line::default()])
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

    fn spawn_main_worker(&mut self, terminal: &mut UiTerminal) -> io::Result<()> {
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
        terminal: &mut UiTerminal,
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
        self.approval_queue.push_back(pending);
        self.promote_next_approval();
        Ok(())
    }

    fn handle_worker_event(
        &mut self,
        terminal: &mut UiTerminal,
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
            WorkerEvent::InboxSteered { worker_id, count } => {
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
        terminal: &mut UiTerminal,
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
        let mut lines = vec![Line::from(header_spans)];

        if let Some(ref raw) = tool.output {
            let out_lines: Vec<&str> = raw.lines().collect();
            if self.expand_details {
                for (idx, l) in out_lines.iter().take(60).enumerate() {
                    let pfx = if idx == 0 { "  ⎿ " } else { "    " };
                    lines.extend(wrap_prefixed_lines(
                        l,
                        pfx,
                        Style::default().fg(Color::DarkGray),
                        Style::default().fg(Color::Gray),
                        width,
                    ));
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
                    lines.push(Line::from(vec![
                        Span::styled(pfx, Style::default().fg(Color::DarkGray)),
                        Span::styled(
                            truncate_single_line(l, width.saturating_sub(6)),
                            Style::default().fg(Color::DarkGray),
                        ),
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

        // Priority 2: Active interactive overlay (`/tree`, `/workers`, `/sessions`, `?` shortcuts)
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

        // `Ctrl+S`: stash non-empty draft prompt or restore stashed draft
        if key.code == KeyCode::Char('s') && ctrl {
            if !self.input.is_empty() {
                let draft = self.input.text().to_owned();
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

        // Double-tap `Esc` within 450ms:
        // - Non-empty input -> clear input
        // - Empty input     -> open `/rewind` conversation timeline (Claude Code style!)
        if key.code == KeyCode::Esc && self.input.completion().is_none() {
            self.hint_line = None;
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
        let session_pairs: Vec<(String, String)> = if self.input.text().starts_with("/resume") {
            self.store
                .list_sessions()
                .into_iter()
                .take(12)
                .map(|s| (s.meta.id, s.meta.title))
                .collect()
        } else {
            Vec::new()
        };
        let comp_ctx = CompletionContext {
            cwd: &self.store.cwd,
            agents: &agent_names,
            workers: &worker_names,
            sessions: &session_pairs,
            custom_commands: &self.custom_commands,
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

    fn handle_overlay_key(&mut self, terminal: &mut UiTerminal, key: KeyEvent) -> io::Result<()> {
        let overlay = std::mem::replace(&mut self.overlay, Overlay::None);
        match overlay {
            Overlay::None => {}
            Overlay::Shortcuts => match key.code {
                KeyCode::Esc | KeyCode::Char('?') | KeyCode::Char('q') => {}
                _ => {
                    // Dismiss shortcuts and immediately pass key through to normal input handling!
                    return self.handle_key(terminal, key);
                }
            },
            Overlay::Config(mut picker) => {
                picker.status_msg = None;
                let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
                let visible_keys = self.filtered_config_indices(&picker.filter);
                match key.code {
                    KeyCode::Esc => {
                        if !picker.filter.is_empty() {
                            picker.filter.clear();
                            picker.selected = 0;
                            self.overlay = Overlay::Config(picker);
                        }
                    }
                    KeyCode::Tab => {
                        picker.tab = match picker.tab {
                            ConfigTab::Settings => ConfigTab::Status,
                            ConfigTab::Status => ConfigTab::Usage,
                            ConfigTab::Usage => ConfigTab::Settings,
                        };
                        picker.selected = 0;
                        self.overlay = Overlay::Config(picker);
                    }
                    KeyCode::Up => {
                        picker.selected = picker.selected.saturating_sub(1);
                        self.overlay = Overlay::Config(picker);
                    }
                    KeyCode::Down => {
                        if picker.tab == ConfigTab::Settings
                            && picker.selected + 1 < visible_keys.len()
                        {
                            picker.selected += 1;
                        }
                        self.overlay = Overlay::Config(picker);
                    }
                    KeyCode::Enter | KeyCode::Char(' ') if picker.tab == ConfigTab::Settings => {
                        if let Some(&setting_idx) = visible_keys.get(picker.selected) {
                            let summary = self.toggle_config_setting(setting_idx);
                            picker.status_msg = Some(summary.clone());
                            self.push_notice(terminal, Color::Green, format!("⎿ {summary}"))?;
                        }
                        self.overlay = Overlay::Config(picker);
                    }
                    KeyCode::Backspace if picker.tab == ConfigTab::Settings => {
                        picker.filter.pop();
                        picker.selected = 0;
                        self.overlay = Overlay::Config(picker);
                    }
                    KeyCode::Char(c) if !ctrl && picker.tab == ConfigTab::Settings && c != ' ' => {
                        picker.filter.push(c);
                        picker.selected = 0;
                        self.overlay = Overlay::Config(picker);
                    }
                    _ => {
                        self.overlay = Overlay::Config(picker);
                    }
                }
            }
            Overlay::Rewind(mut picker) => {
                if picker.choosing_action {
                    match key.code {
                        KeyCode::Esc => {
                            picker.choosing_action = false;
                            self.overlay = Overlay::Rewind(picker);
                        }
                        KeyCode::Up | KeyCode::Char('k') => {
                            picker.action_idx = picker.action_idx.saturating_sub(1);
                            self.overlay = Overlay::Rewind(picker);
                        }
                        KeyCode::Down | KeyCode::Char('j') => {
                            if picker.action_idx < 2 {
                                picker.action_idx += 1;
                            }
                            self.overlay = Overlay::Rewind(picker);
                        }
                        KeyCode::Char('1') => {
                            if let Some(pt) = picker.points.get(picker.selected).cloned() {
                                self.apply_rewind_action(terminal, &pt, 0)?;
                            }
                        }
                        KeyCode::Char('2') => {
                            if let Some(pt) = picker.points.get(picker.selected).cloned() {
                                self.apply_rewind_action(terminal, &pt, 1)?;
                            }
                        }
                        KeyCode::Char('3') => {
                            if let Some(pt) = picker.points.get(picker.selected).cloned() {
                                self.apply_rewind_action(terminal, &pt, 2)?;
                            }
                        }
                        KeyCode::Enter => {
                            let act = picker.action_idx;
                            if let Some(pt) = picker.points.get(picker.selected).cloned() {
                                self.apply_rewind_action(terminal, &pt, act)?;
                            }
                        }
                        _ => {
                            self.overlay = Overlay::Rewind(picker);
                        }
                    }
                } else {
                    match key.code {
                        KeyCode::Esc | KeyCode::Char('q') => {}
                        KeyCode::Up | KeyCode::Char('k') => {
                            picker.selected = picker.selected.saturating_sub(1);
                            if picker.selected < picker.scroll {
                                picker.scroll = picker.selected;
                            }
                            self.overlay = Overlay::Rewind(picker);
                        }
                        KeyCode::Down | KeyCode::Char('j') => {
                            if picker.selected + 1 < picker.points.len() {
                                picker.selected += 1;
                            }
                            if picker.selected >= picker.scroll + 6 {
                                picker.scroll = picker.selected.saturating_sub(5);
                            }
                            self.overlay = Overlay::Rewind(picker);
                        }
                        KeyCode::Enter => {
                            if !picker.points.is_empty() {
                                picker.choosing_action = true;
                                picker.action_idx = 0;
                                self.overlay = Overlay::Rewind(picker);
                            }
                        }
                        _ => {
                            self.overlay = Overlay::Rewind(picker);
                        }
                    }
                }
            }
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
            Overlay::Sessions(mut picker) => {
                picker.status_msg = None;
                let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
                let mode = std::mem::replace(&mut picker.mode, ResumeMode::Browse);
                match mode {
                    ResumeMode::Rename { mut buf } => match key.code {
                        KeyCode::Esc => {
                            picker.mode = ResumeMode::Browse;
                            self.overlay = Overlay::Sessions(picker);
                        }
                        KeyCode::Enter => {
                            let new_title = buf.trim().to_owned();
                            if !new_title.is_empty()
                                && let Some(sel) = picker.selected_item().cloned()
                            {
                                let mut updated = sel.meta.clone();
                                updated.title = new_title.clone();
                                updated.updated_at = now_rfc3339();
                                if save_session_meta(&sel.session_dir, &updated).is_ok() {
                                    if updated.id == self.meta.id {
                                        self.meta.title = new_title.clone();
                                    }
                                    picker.all_items = self.store.list_all_sessions();
                                    picker.status_msg = Some(format!("renamed -> \"{new_title}\""));
                                }
                            }
                            picker.mode = ResumeMode::Browse;
                            picker.clamp_selection();
                            self.overlay = Overlay::Sessions(picker);
                        }
                        KeyCode::Backspace => {
                            buf.pop();
                            picker.mode = ResumeMode::Rename { buf };
                            self.overlay = Overlay::Sessions(picker);
                        }
                        KeyCode::Char(c) if !ctrl => {
                            buf.push(c);
                            picker.mode = ResumeMode::Rename { buf };
                            self.overlay = Overlay::Sessions(picker);
                        }
                        _ => {
                            picker.mode = ResumeMode::Rename { buf };
                            self.overlay = Overlay::Sessions(picker);
                        }
                    },
                    ResumeMode::ConfirmDelete => match key.code {
                        KeyCode::Char('y') | KeyCode::Char('Y') => {
                            if let Some(sel) = picker.selected_item().cloned() {
                                if sel.meta.id == self.meta.id {
                                    picker.status_msg =
                                        Some("cannot delete the currently active session".into());
                                } else if let Err(err) = self.store.delete_session(&sel.session_dir)
                                {
                                    picker.status_msg = Some(format!("delete failed: {err}"));
                                } else {
                                    picker.all_items = self.store.list_all_sessions();
                                    picker.status_msg =
                                        Some(format!("deleted session {}", sel.meta.id));
                                }
                            }
                            picker.mode = ResumeMode::Browse;
                            picker.clamp_selection();
                            self.overlay = Overlay::Sessions(picker);
                        }
                        _ => {
                            picker.mode = ResumeMode::Browse;
                            self.overlay = Overlay::Sessions(picker);
                        }
                    },
                    ResumeMode::Browse => {
                        let filtered_len = picker.filtered_indices().len();
                        match key.code {
                            KeyCode::Esc => {
                                if !picker.query.is_empty() {
                                    picker.query.clear();
                                    picker.selected = 0;
                                    self.overlay = Overlay::Sessions(picker);
                                }
                                // Otherwise closes overlay cleanly.
                            }
                            KeyCode::Tab => {
                                picker.tab = match picker.tab {
                                    ResumeTab::Project => ResumeTab::All,
                                    ResumeTab::All => ResumeTab::Project,
                                };
                                picker.selected = 0;
                                picker.clamp_selection();
                                self.overlay = Overlay::Sessions(picker);
                            }
                            KeyCode::Up => {
                                picker.selected = picker.selected.saturating_sub(1);
                                self.overlay = Overlay::Sessions(picker);
                            }
                            KeyCode::Down => {
                                if picker.selected + 1 < filtered_len {
                                    picker.selected += 1;
                                }
                                self.overlay = Overlay::Sessions(picker);
                            }
                            KeyCode::Char('p') if ctrl => {
                                picker.selected = picker.selected.saturating_sub(1);
                                self.overlay = Overlay::Sessions(picker);
                            }
                            KeyCode::Char('n') if ctrl => {
                                if picker.selected + 1 < filtered_len {
                                    picker.selected += 1;
                                }
                                self.overlay = Overlay::Sessions(picker);
                            }
                            KeyCode::Left | KeyCode::PageUp => {
                                picker.selected = picker.selected.saturating_sub(RESUME_PAGE_SIZE);
                                self.overlay = Overlay::Sessions(picker);
                            }
                            KeyCode::Right | KeyCode::PageDown => {
                                if filtered_len > 0 {
                                    picker.selected =
                                        (picker.selected + RESUME_PAGE_SIZE).min(filtered_len - 1);
                                }
                                self.overlay = Overlay::Sessions(picker);
                            }
                            KeyCode::F(2) => {
                                if let Some(sel) = picker.selected_item() {
                                    picker.mode = ResumeMode::Rename {
                                        buf: sel.meta.title.clone(),
                                    };
                                }
                                self.overlay = Overlay::Sessions(picker);
                            }
                            KeyCode::F(4) | KeyCode::Delete => {
                                if let Some(sel) = picker.selected_item() {
                                    if sel.meta.id == self.meta.id {
                                        picker.status_msg = Some(
                                            "cannot delete the currently active session".into(),
                                        );
                                    } else {
                                        picker.mode = ResumeMode::ConfirmDelete;
                                    }
                                }
                                self.overlay = Overlay::Sessions(picker);
                            }
                            KeyCode::Enter => {
                                if let Some(chosen) = picker.selected_item().cloned() {
                                    if chosen.meta.id == self.meta.id {
                                        self.push_notice(
                                            terminal,
                                            Color::DarkGray,
                                            format!("already on session {}", self.meta.id),
                                        )?;
                                    } else {
                                        self.switch_session(
                                            terminal,
                                            chosen.session_dir,
                                            chosen.meta,
                                        )?;
                                    }
                                }
                            }
                            KeyCode::Backspace => {
                                picker.query.pop();
                                picker.selected = 0;
                                picker.clamp_selection();
                                self.overlay = Overlay::Sessions(picker);
                            }
                            KeyCode::Char(c) if !ctrl => {
                                picker.query.push(c);
                                picker.selected = 0;
                                picker.clamp_selection();
                                self.overlay = Overlay::Sessions(picker);
                            }
                            _ => {
                                self.overlay = Overlay::Sessions(picker);
                            }
                        }
                    }
                }
            }
        }
        Ok(())
    }

    fn switch_session(
        &mut self,
        terminal: &mut UiTerminal,
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

    fn submit_user_input(&mut self, terminal: &mut UiTerminal, text: String) -> io::Result<()> {
        if text.starts_with('/') {
            return self.handle_slash_command(terminal, &text);
        }
        if text.starts_with('!') {
            return self.run_bang_command(terminal, &text);
        }

        // Update session title on first user prompt.
        if self.meta.title == "New session" {
            self.meta.title = truncate_single_line(&text, 48);
            self.meta.updated_at = now_rfc3339();
            let _ = save_session_meta(&self.session_dir, &self.meta);
        }

        let width = self.term_width(terminal);
        let prefix = if self.focused_worker == "main" {
            "❯ ".to_owned()
        } else {
            format!("❯ [{}] ", self.focused_worker)
        };
        let mut lines = wrap_prefixed_lines(
            &text,
            &prefix,
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
            Style::default().fg(Color::White),
            width,
        );
        lines.push(Line::default());
        self.push_lines(terminal, lines)?;

        let payload = if self.pending_bang_context.is_empty() {
            text
        } else {
            let ctx_block = std::mem::take(&mut self.pending_bang_context).join("\n\n");
            format!("{ctx_block}\n\n{text}")
        };

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
            if let Err(err) = st.handle.send_prompt(&payload, "user") {
                self.push_notice(
                    terminal,
                    Color::Red,
                    format!("failed delivering prompt to `{wid}`: {err}"),
                )?;
            }
        }
        Ok(())
    }

    fn run_bang_command(&mut self, terminal: &mut UiTerminal, raw: &str) -> io::Result<()> {
        let (override_mode, cmd_str) = if let Some(rest) = raw.strip_prefix("!!") {
            (Some(BangMode::Trigger), rest.trim())
        } else if let Some(rest) = raw.strip_prefix('!') {
            (None, rest.trim())
        } else {
            return Ok(());
        };

        if cmd_str.is_empty() {
            return self.push_notice(
                terminal,
                Color::DarkGray,
                format!(
                    "usage: !<shell command> (current bang_mode={}, use `!!<cmd>` to force trigger model, or `/config bang_mode silent|attach|trigger`)",
                    self.agent_config.bang_mode.as_str()
                ),
            );
        }

        let cmd = cmd_str.to_owned();
        self.running_bang_cmd = Some(cmd.clone());
        self.push_notice(terminal, Color::Yellow, format!("! $ {cmd}"))?;

        let shell = self.agent_config.bang_shell.clone();
        let cwd = self.store.cwd.clone();
        let tx = self.app_tx.clone();
        thread::spawn(move || {
            let result = match std::process::Command::new(&shell)
                .args(["-c", &cmd])
                .current_dir(&cwd)
                .output()
            {
                Ok(out) => {
                    let mut combined = String::from_utf8_lossy(&out.stdout).into_owned();
                    let err_s = String::from_utf8_lossy(&out.stderr);
                    if !err_s.is_empty() {
                        if !combined.is_empty() && !combined.ends_with('\n') {
                            combined.push('\n');
                        }
                        combined.push_str(&err_s);
                    }
                    BangResult {
                        cmd,
                        exit_code: out.status.code().unwrap_or(1),
                        output: combined,
                        override_mode,
                    }
                }
                Err(err) => BangResult {
                    cmd,
                    exit_code: 127,
                    output: format!("failed to spawn `{shell}`: {err}"),
                    override_mode,
                },
            };
            let _ = tx.send(AppEvent::BangDone(result));
        });
        Ok(())
    }

    fn handle_bang_done(&mut self, terminal: &mut UiTerminal, res: BangResult) -> io::Result<()> {
        self.running_bang_cmd = None;
        let width = self.term_width(terminal);
        let dot_color = if res.exit_code == 0 {
            Color::Green
        } else {
            Color::Red
        };
        let mut header_spans = vec![
            Span::styled("● ", Style::default().fg(dot_color)),
            Span::styled(
                "! ",
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                truncate_single_line(&res.cmd, width.saturating_sub(16)),
                Style::default().fg(Color::White),
            ),
        ];
        if res.exit_code != 0 {
            header_spans.push(Span::styled(
                format!(" [rc={}]", res.exit_code),
                Style::default().fg(Color::Red),
            ));
        }
        let mut lines = vec![Line::from(header_spans)];

        let out_lines: Vec<&str> = res.output.lines().collect();
        let limit = if self.expand_details { 80 } else { 12 };
        for (idx, l) in out_lines.iter().take(limit).enumerate() {
            let pfx = if idx == 0 { "  ⎿ " } else { "    " };
            lines.extend(wrap_prefixed_lines(
                l,
                pfx,
                Style::default().fg(Color::DarkGray),
                Style::default().fg(Color::Gray),
                width,
            ));
        }
        if out_lines.len() > limit {
            lines.push(Line::from(Span::styled(
                format!(
                    "    … +{} lines (ctrl+o to expand)",
                    out_lines.len() - limit
                ),
                Style::default().fg(Color::DarkGray),
            )));
        }

        let effective_mode = res.override_mode.unwrap_or(self.agent_config.bang_mode);
        let formatted_block = format!(
            "<shell_command cmd=\"{}\" exit_code=\"{}\">\n{}\n</shell_command>",
            res.cmd.replace('"', "'"),
            res.exit_code,
            res.output.trim_end()
        );

        match effective_mode {
            BangMode::Silent => {
                lines.push(Line::default());
                self.push_lines(terminal, lines)?;
            }
            BangMode::Attach => {
                self.pending_bang_context.push(formatted_block);
                lines.push(Line::from(Span::styled(
                    "  ↳ attached to next prompt (model not triggered · `/config bang_mode`)",
                    Style::default().fg(Color::DarkGray),
                )));
                lines.push(Line::default());
                self.push_lines(terminal, lines)?;
            }
            BangMode::Trigger => {
                lines.push(Line::from(Span::styled(
                    "  ↳ triggering model with command output…",
                    Style::default().fg(Color::Cyan),
                )));
                lines.push(Line::default());
                self.push_lines(terminal, lines)?;

                let wid = self.focused_worker.clone();
                if let Some(st) = self.workers.get(&wid)
                    && !st.handle.alive
                    && wid == "main"
                {
                    self.spawn_main_worker(terminal)?;
                }
                if let Some(st) = self.workers.get_mut(&wid) {
                    st.awaiting_user_cycle = true;
                    let prompt = format!(
                        "I ran `{}` directly in the terminal:\n\n{}",
                        res.cmd, formatted_block
                    );
                    if let Err(err) = st.handle.send_prompt(&prompt, "user_bang") {
                        self.push_notice(
                            terminal,
                            Color::Red,
                            format!("failed delivering `!` result to `{wid}`: {err}"),
                        )?;
                    }
                }
            }
        }
        Ok(())
    }

    fn handle_slash_command(&mut self, terminal: &mut UiTerminal, raw: &str) -> io::Result<()> {
        let mut parts = raw.split_whitespace();
        let cmd = parts.next().unwrap_or("");
        let rest = raw[cmd.len()..].trim();

        match cmd {
            "/help" => {
                let help = "\
Commands:
  !<cmd>                             Run a shell command directly (mode via `/config bang_mode`; `!!<cmd>` forces model trigger)
  /config [key] [value]              View or update `~/.config/anix-agent/config.json`
  /resume [session_id]               Search, rename, delete, or resume saved conversations
  /tree                              Browse Tree WAL branches (Enter switch leaf, f fork, i inspect)
  /fork [name] [--leaf <id>]         Fork active WAL via `anix fork` and switch onto new branch
  /compact                           Trigger online context compaction (`Anix-Control: compact`)
  /spawn [--keep] [--fork [leaf]] <agent> [prompt]
                                     Spawn a parallel `anix` worker with `--notify` back to `main`
  /workers                           Manage live workers & active `hold` leases (shortcut: Ctrl+W)
  /send <worker> <message>           Send a Maildir message to a running worker
  /inspect <node_id>                 Print full payload of a Tree WAL node
  /model <model_id>                  Switch focused worker's LLM model (respawns cleanly on WAL)
  /agent <path|name>                 Switch focused worker's `*.agent` script
  /mode <ask|auto|plan>              Set approval mode (shortcut: Shift+Tab)
  /new / /clear                      Start a fresh persistent session
  /quit                              Shut down all workers and exit

Keys:
  Enter submit · Alt+Enter / Ctrl+J newline · Tab complete /cmd & @path
  Shift+Tab cycle ask/auto/plan · Ctrl+O toggle thinking/tool expansion · Ctrl+W workers · Ctrl+C interrupt";
                self.push_notice(terminal, Color::Gray, help)
            }
            "/statusbar" => {
                if rest.is_empty() {
                    return self.push_notice(
                        terminal,
                        Color::Gray,
                        format!(
                            "current status_bar:\n  {}\n\nDIY commands:\n  /statusbar vars                 List all {{variables}} with live values\n  /statusbar edit                 Pre-fill template in input to edit live\n  /statusbar preset <minimal|default|full>\n  /statusbar reset                Restore default template\n  /statusbar <template>           Apply & save custom template (supports {{var}}, {{var:color+bold}}, {{?cond ...}}, {{right}})",
                            self.agent_config.status_bar
                        ),
                    );
                }
                if rest.eq_ignore_ascii_case("vars") {
                    let vars = self.build_status_vars();
                    let mut rows = vec!["Available status_bar variables (use `{name}`, `{name:color+bold}`, `{?name ...}`, `{right}`):".to_owned()];
                    for &(name, desc) in AVAILABLE_VARS {
                        let live = if name == "right" {
                            "(layout)".to_owned()
                        } else {
                            vars.resolve(name)
                                .map(|(v, _)| if v.is_empty() { "(empty)".into() } else { v })
                                .unwrap_or_default()
                        };
                        rows.push(format!("  {{{name:<13}}}  {live:<20}  {desc}"));
                    }
                    return self.push_notice(terminal, Color::Gray, rows.join("\n"));
                }
                if rest.eq_ignore_ascii_case("edit") {
                    self.input
                        .set_text(format!("/statusbar {}", self.agent_config.status_bar));
                    self.hint_line =
                        Some("Edit template above and press Enter to save live".into());
                    return Ok(());
                }
                let new_tpl = if rest.eq_ignore_ascii_case("reset") {
                    DEFAULT_STATUS_BAR.to_owned()
                } else if let Some(preset_name) = rest.strip_prefix("preset ") {
                    match resolve_preset(preset_name.trim()) {
                        Some(p) => p.to_owned(),
                        None => {
                            return self.push_notice(
                                terminal,
                                Color::Red,
                                "unknown preset (expected: default | minimal | full)",
                            );
                        }
                    }
                } else if let Some(p) = resolve_preset(rest) {
                    p.to_owned()
                } else {
                    rest.to_owned()
                };
                self.agent_config.status_bar = new_tpl.clone();
                if let Err(err) = self.agent_config.save_to(&self.agent_config_path) {
                    self.push_notice(
                        terminal,
                        Color::Red,
                        format!("updated status_bar in memory, but save failed: {err}"),
                    )
                } else {
                    self.push_notice(
                        terminal,
                        Color::Green,
                        format!(
                            "◈ saved status_bar -> {}\n  {new_tpl}",
                            self.agent_config_path.display()
                        ),
                    )
                }
            }
            "/rewind" | "/undo" => self.open_rewind_picker(terminal),
            "/context" => self.print_context_grid(terminal),
            "/config" => {
                if rest.is_empty() {
                    self.overlay = Overlay::Config(ConfigPicker {
                        tab: ConfigTab::Settings,
                        selected: 0,
                        filter: String::new(),
                        status_msg: None,
                    });
                    return Ok(());
                }
                let mut sp = rest.splitn(2, char::is_whitespace);
                let key = sp.next().unwrap_or("");
                let val = sp.next().map(str::trim).unwrap_or("");
                if val.is_empty() {
                    return self.push_notice(
                        terminal,
                        Color::Red,
                        "usage: /config <key> <value> (e.g. `/config bang_mode trigger`)",
                    );
                }
                match self.agent_config.set_field(key, val) {
                    Ok(summary) => {
                        self.expand_details = self.agent_config.expand_details;
                        if key == "default_mode" {
                            self.policy = self.agent_config.default_mode;
                            self.meta.approval_policy = self.policy;
                            let _ = save_session_meta(&self.session_dir, &self.meta);
                        }
                        if let Err(err) = self.agent_config.save_to(&self.agent_config_path) {
                            self.push_notice(
                                terminal,
                                Color::Red,
                                format!("updated {summary} in memory, but save failed: {err}"),
                            )
                        } else {
                            self.push_notice(
                                terminal,
                                Color::Green,
                                format!(
                                    "◈ saved {summary} -> {}",
                                    self.agent_config_path.display()
                                ),
                            )
                        }
                    }
                    Err(msg) => self.push_notice(terminal, Color::Red, msg),
                }
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
                if !rest.is_empty() {
                    return match self.store.find_session(rest) {
                        Some((dir, meta)) => {
                            if meta.id == self.meta.id {
                                self.push_notice(
                                    terminal,
                                    Color::DarkGray,
                                    format!("already on session {}", self.meta.id),
                                )
                            } else {
                                self.switch_session(terminal, dir, meta)
                            }
                        }
                        None => self.push_notice(
                            terminal,
                            Color::Yellow,
                            format!("session `{rest}` not found (run `/resume` to browse)"),
                        ),
                    };
                }
                let all_items = self.store.list_all_sessions();
                if all_items.is_empty() {
                    self.push_notice(terminal, Color::DarkGray, "no saved conversations yet")
                } else {
                    let has_cur = all_items.iter().any(|s| s.is_current_project);
                    let mut picker = ResumePicker {
                        tab: if has_cur {
                            ResumeTab::Project
                        } else {
                            ResumeTab::All
                        },
                        all_items,
                        query: String::new(),
                        selected: 0,
                        mode: ResumeMode::Browse,
                        status_msg: None,
                    };
                    picker.clamp_selection();
                    self.overlay = Overlay::Sessions(picker);
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
            other => {
                // Check discovered custom commands (`commands/*.md`) and skills (`skills/*/SKILL.md`).
                if let Some(entry) = self
                    .custom_commands
                    .iter()
                    .find(|e| e.slash_name.eq_ignore_ascii_case(other))
                    .cloned()
                {
                    return self.execute_custom_slash_entry(terminal, &entry, rest);
                }
                self.push_notice(
                    terminal,
                    Color::Yellow,
                    format!("unknown command `{other}` (try /help)"),
                )
            }
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
        terminal: &mut UiTerminal,
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

    fn cmd_spawn_worker(&mut self, terminal: &mut UiTerminal, rest: &str) -> io::Result<()> {
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
        terminal: &mut UiTerminal,
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

    fn input_box_height(&self, width: u16) -> u16 {
        let text_w = (width as usize).saturating_sub(8).max(8);
        let (visual_lines, _) = self.input.visual_lines_and_cursor(text_w);
        let rows = visual_lines.len().clamp(1, 5) as u16;
        rows + 2 // top & bottom subtle horizontal rule
    }

    fn live_stream_height(&self, width: u16) -> u16 {
        let mut count = 0u16;
        if self.running_bang_cmd.is_some() {
            count += 1;
        }
        if let Some(st) = self.workers.get(&self.focused_worker) {
            if st.handle.busy {
                count += 1;
            }
            count += st.active_tools.len().min(3) as u16;
            if !st.streaming_reasoning.trim().is_empty() && st.streaming_text.trim().is_empty() {
                count += 1;
            }
            if !st.streaming_text.is_empty() {
                let rendered =
                    render_markdown(&st.streaming_text, (width as usize).saturating_sub(2));
                count += rendered.len().clamp(1, 5) as u16;
            }
        }
        if count == 0 && self.hint_line.is_some() {
            return 1;
        }
        count.min(7)
    }

    fn desired_viewport_height(&self, width: u16) -> u16 {
        if self.active_approval.is_some() {
            return 5 + self.input_box_height(width) + 1;
        }
        match &self.overlay {
            Overlay::Sessions(_) => 14,
            Overlay::Config(_) => 13,
            Overlay::Rewind(_) => 13,
            Overlay::Tree { .. } => 10 + self.input_box_height(width) + 1,
            Overlay::Workers { .. } => 7 + self.input_box_height(width) + 1,
            Overlay::Shortcuts => self.input_box_height(width) + 8 + 1,
            Overlay::None => {
                if let Some(menu) = self.input.completion() {
                    let items_h = menu.items.len().min(COMPLETION_VISIBLE_ROWS) as u16;
                    let overflow_h = u16::from(
                        menu.hidden_above() > 0 || menu.hidden_below(COMPLETION_VISIBLE_ROWS) > 0,
                    );
                    return self.input_box_height(width) + items_h + overflow_h + 2 + 1;
                }
                let live_h = self.live_stream_height(width);
                live_h + self.input_box_height(width) + 1
            }
        }
    }

    fn render(&self, f: &mut RenderTarget<'_>) {
        let area = f.area();
        let input_h = self.input_box_height(area.width);

        // When `/resume` or `/config` full-width picker is active, give it the viewport above status bar.
        if self.active_approval.is_none()
            && let Overlay::Sessions(ref picker) = self.overlay
        {
            let chunks = Layout::default()
                .direction(Direction::Vertical)
                .constraints([Constraint::Min(6), Constraint::Length(1)])
                .split(area);
            self.render_sessions_overlay(f, picker, chunks[0]);
            self.render_status_bar(f, chunks[1]);
            return;
        }
        if self.active_approval.is_none()
            && let Overlay::Config(ref picker) = self.overlay
        {
            let chunks = Layout::default()
                .direction(Direction::Vertical)
                .constraints([Constraint::Min(6), Constraint::Length(1)])
                .split(area);
            self.render_config_overlay(f, picker, chunks[0]);
            self.render_status_bar(f, chunks[1]);
            return;
        }
        if self.active_approval.is_none()
            && let Overlay::Rewind(ref picker) = self.overlay
        {
            let chunks = Layout::default()
                .direction(Direction::Vertical)
                .constraints([Constraint::Min(6), Constraint::Length(1)])
                .split(area);
            self.render_rewind_overlay(f, picker, chunks[0]);
            self.render_status_bar(f, chunks[1]);
            return;
        }

        // When `?` shortcuts panel is open, show sleek prompt + 3-col shortcuts below it + status bar.
        if self.active_approval.is_none() && matches!(self.overlay, Overlay::Shortcuts) {
            let chunks = Layout::default()
                .direction(Direction::Vertical)
                .constraints([
                    Constraint::Length(input_h),
                    Constraint::Min(4),
                    Constraint::Length(1),
                ])
                .split(area);
            self.render_input_box(f, chunks[0]);
            self.render_shortcuts_panel(f, chunks[1]);
            self.render_status_bar(f, chunks[2]);
            return;
        }

        // When the live slash / `@path` completion palette is open, dock the sleek prompt
        // directly above the live dropdown.
        if self.active_approval.is_none()
            && matches!(self.overlay, Overlay::None)
            && let Some(menu) = self.input.completion()
        {
            let chunks = Layout::default()
                .direction(Direction::Vertical)
                .constraints([
                    Constraint::Length(input_h),
                    Constraint::Min(3),
                    Constraint::Length(1),
                ])
                .split(area);
            self.render_input_box(f, chunks[0]);
            self.render_completion_palette(f, menu, chunks[1]);
            self.render_status_bar(f, chunks[2]);
            return;
        }

        // Active approval card or `/tree` / `/workers` overlay
        if self.active_approval.is_some() || !matches!(self.overlay, Overlay::None) {
            let chunks = Layout::default()
                .direction(Direction::Vertical)
                .constraints([
                    Constraint::Min(4),
                    Constraint::Length(input_h),
                    Constraint::Length(1),
                ])
                .split(area);
            if let Some(ref active) = self.active_approval {
                self.render_approval_card(f, active, chunks[0]);
            } else {
                match &self.overlay {
                    Overlay::Tree {
                        graph,
                        selected,
                        scroll,
                    } => self.render_tree_overlay(f, graph, *selected, *scroll, chunks[0]),
                    Overlay::Workers { selected } => {
                        self.render_workers_overlay(f, *selected, chunks[0])
                    }
                    Overlay::Sessions(picker) => {
                        self.render_sessions_overlay(f, picker, chunks[0]);
                    }
                    Overlay::None
                    | Overlay::Shortcuts
                    | Overlay::Config(_)
                    | Overlay::Rewind(_) => {}
                }
            }
            self.render_input_box(f, chunks[1]);
            self.render_status_bar(f, chunks[2]);
            return;
        }

        // Default compact mode: 0 live rows when idle -> EXACTLY 4 lines total!
        let live_h = self.live_stream_height(area.width);
        if live_h == 0 {
            let chunks = Layout::default()
                .direction(Direction::Vertical)
                .constraints([Constraint::Length(input_h), Constraint::Length(1)])
                .split(area);
            self.render_input_box(f, chunks[0]);
            self.render_status_bar(f, chunks[1]);
        } else {
            let chunks = Layout::default()
                .direction(Direction::Vertical)
                .constraints([
                    Constraint::Min(1),
                    Constraint::Length(input_h),
                    Constraint::Length(1),
                ])
                .split(area);
            self.render_live_stream(f, chunks[0]);
            self.render_input_box(f, chunks[1]);
            self.render_status_bar(f, chunks[2]);
        }
    }

    fn render_shortcuts_panel(&self, f: &mut RenderTarget<'_>, area: Rect) {
        let rows: &[(&str, &str, &str)] = &[
            (
                "! for shell mode",
                "\\⏎ / alt+⏎ newline",
                "ctrl + g edit in $EDITOR",
            ),
            (
                "!! force-trigger model",
                "shift + tab cycle mode",
                "ctrl + s stash / pop draft",
            ),
            (
                "/ for cmds & skills",
                "esc ×2 clear / rewind",
                "ctrl + z suspend (fg back)",
            ),
            (
                "@ for file paths",
                "ctrl + o expand output",
                "ctrl + w manage workers",
            ),
            (
                "/resume browse chats",
                "/config settings panel",
                "/context token usage grid",
            ),
            (
                "/tree /fork WAL branch",
                "/statusbar DIY bar",
                "ctrl + d on empty to quit",
            ),
        ];
        let col_w = ((area.width as usize).saturating_sub(4) / 3).max(20);
        let mut lines = Vec::with_capacity(rows.len() + 2);
        for &(c1, c2, c3) in rows {
            lines.push(Line::from(vec![
                Span::raw("  "),
                Span::styled(
                    pad_display_width(c1, col_w),
                    Style::default().fg(Color::Gray),
                ),
                Span::styled(
                    pad_display_width(c2, col_w),
                    Style::default().fg(Color::Gray),
                ),
                Span::styled(
                    truncate_single_line(c3, col_w),
                    Style::default().fg(Color::Gray),
                ),
            ]));
        }
        lines.push(Line::from(Span::styled(
            "  Press Esc or ? to close",
            Style::default().fg(Color::DarkGray),
        )));
        f.render_widget(Paragraph::new(lines), area);
    }

    fn config_setting_rows(&self) -> Vec<(&'static str, String)> {
        let preset_name = if self.agent_config.status_bar == DEFAULT_STATUS_BAR {
            "default".to_owned()
        } else if self.agent_config.status_bar == PRESET_MINIMAL {
            "minimal".to_owned()
        } else if self.agent_config.status_bar == PRESET_FULL {
            "full".to_owned()
        } else {
            "custom".to_owned()
        };
        vec![
            (
                "Bang mode (!cmd)",
                self.agent_config.bang_mode.as_str().to_owned(),
            ),
            (
                "Default approval mode",
                self.agent_config.default_mode.as_str().to_owned(),
            ),
            (
                "Auto-resume last session",
                self.agent_config.auto_resume_last.to_string(),
            ),
            (
                "Expand thinking & tools",
                self.agent_config.expand_details.to_string(),
            ),
            (
                "Replay turns on /resume",
                self.agent_config.replay_turns.to_string(),
            ),
            ("Status bar preset", preset_name),
        ]
    }

    fn filtered_config_indices(&self, filter: &str) -> Vec<usize> {
        let q = filter.trim().to_ascii_lowercase();
        self.config_setting_rows()
            .iter()
            .enumerate()
            .filter(|(_, (label, val))| {
                q.is_empty()
                    || label.to_ascii_lowercase().contains(&q)
                    || val.to_ascii_lowercase().contains(&q)
            })
            .map(|(idx, _)| idx)
            .collect()
    }

    fn toggle_config_setting(&mut self, idx: usize) -> String {
        let summary = match idx {
            0 => {
                self.agent_config.bang_mode = match self.agent_config.bang_mode {
                    BangMode::Silent => BangMode::Attach,
                    BangMode::Attach => BangMode::Trigger,
                    BangMode::Trigger => BangMode::Silent,
                };
                format!("bang_mode = {}", self.agent_config.bang_mode.as_str())
            }
            1 => {
                self.agent_config.default_mode = self.agent_config.default_mode.cycle_next();
                self.policy = self.agent_config.default_mode;
                self.meta.approval_policy = self.policy;
                let _ = save_session_meta(&self.session_dir, &self.meta);
                format!("default_mode = {}", self.agent_config.default_mode.as_str())
            }
            2 => {
                self.agent_config.auto_resume_last = !self.agent_config.auto_resume_last;
                format!("auto_resume_last = {}", self.agent_config.auto_resume_last)
            }
            3 => {
                self.agent_config.expand_details = !self.agent_config.expand_details;
                self.expand_details = self.agent_config.expand_details;
                format!("expand_details = {}", self.agent_config.expand_details)
            }
            4 => {
                self.agent_config.replay_turns = match self.agent_config.replay_turns {
                    0..=4 => 8,
                    5..=8 => 16,
                    9..=16 => 32,
                    _ => 4,
                };
                format!("replay_turns = {}", self.agent_config.replay_turns)
            }
            _ => {
                let next = if self.agent_config.status_bar == DEFAULT_STATUS_BAR {
                    ("minimal", PRESET_MINIMAL)
                } else if self.agent_config.status_bar == PRESET_MINIMAL {
                    ("full", PRESET_FULL)
                } else {
                    ("default", DEFAULT_STATUS_BAR)
                };
                self.agent_config.status_bar = next.1.to_owned();
                format!("status_bar preset = {}", next.0)
            }
        };
        let _ = self.agent_config.save_to(&self.agent_config_path);
        summary
    }

    fn render_config_overlay(&self, f: &mut RenderTarget<'_>, picker: &ConfigPicker, area: Rect) {
        let block = Block::default()
            .borders(Borders::TOP)
            .border_style(Style::default().fg(Color::DarkGray));
        let inner = block.inner(area);
        f.render_widget(block, area);

        let tab_style = |active: bool| {
            if active {
                Style::default()
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD | Modifier::UNDERLINED)
            } else {
                Style::default().fg(Color::DarkGray)
            }
        };

        let mut lines: Vec<Line<'static>> = Vec::with_capacity(inner.height as usize);
        lines.push(Line::from(vec![
            Span::raw("  "),
            Span::styled("Settings", tab_style(picker.tab == ConfigTab::Settings)),
            Span::raw("   "),
            Span::styled("Status", tab_style(picker.tab == ConfigTab::Status)),
            Span::raw("   "),
            Span::styled("Usage", tab_style(picker.tab == ConfigTab::Usage)),
            Span::styled("   (tab to cycle)", Style::default().fg(Color::DarkGray)),
        ]));
        lines.push(Line::default());

        match picker.tab {
            ConfigTab::Settings => {
                if picker.filter.is_empty() {
                    let mut search_spans = vec![Span::styled(
                        "  Type to filter settings…",
                        Style::default()
                            .fg(Color::DarkGray)
                            .add_modifier(Modifier::ITALIC),
                    )];
                    if let Some(ref msg) = picker.status_msg {
                        search_spans.push(Span::styled(
                            format!("  · saved {msg}"),
                            Style::default().fg(Color::Green),
                        ));
                    }
                    lines.push(Line::from(search_spans));
                } else {
                    lines.push(Line::from(vec![
                        Span::styled("  Filter: ", Style::default().fg(Color::Cyan)),
                        Span::styled(
                            format!("{}▏", picker.filter),
                            Style::default().fg(Color::White),
                        ),
                    ]));
                }

                let all_rows = self.config_setting_rows();
                let visible = self.filtered_config_indices(&picker.filter);
                let col_w = 34usize.min((inner.width as usize / 2).max(20));
                if visible.is_empty() {
                    lines.push(Line::from(Span::styled(
                        "  (no matching settings)",
                        Style::default().fg(Color::DarkGray),
                    )));
                } else {
                    for (pos, &idx) in visible.iter().enumerate() {
                        let (label, val) = &all_rows[idx];
                        let is_sel = pos == picker.selected;
                        let pfx = if is_sel { "> " } else { "  " };
                        let (ps, ls, vs) = if is_sel {
                            (
                                Style::default()
                                    .fg(Color::Cyan)
                                    .add_modifier(Modifier::BOLD),
                                Style::default()
                                    .fg(Color::Cyan)
                                    .add_modifier(Modifier::BOLD),
                                Style::default()
                                    .fg(Color::Yellow)
                                    .add_modifier(Modifier::BOLD),
                            )
                        } else {
                            (
                                Style::default().fg(Color::DarkGray),
                                Style::default().fg(Color::White),
                                Style::default().fg(Color::Gray),
                            )
                        };
                        lines.push(Line::from(vec![
                            Span::styled(pfx, ps),
                            Span::styled(pad_display_width(label, col_w), ls),
                            Span::raw("  "),
                            Span::styled(val.clone(), vs),
                        ]));
                    }
                }
            }
            ConfigTab::Status => {
                let vars = self.build_status_vars();
                let rows = [
                    ("Session", format!("{} ({})", self.meta.id, self.meta.title)),
                    ("Agent", self.meta.agent_path.display().to_string()),
                    ("Model", format!("{} ({})", vars.model, vars.provider)),
                    ("WAL", self.meta.active_wal.display().to_string()),
                    ("Config JSON", self.agent_config_path.display().to_string()),
                    ("Layer 1 anix", self.anix_bin.display().to_string()),
                    (
                        "Custom cmds/skills",
                        format!("{} discovered", self.custom_commands.len()),
                    ),
                ];
                let w = (inner.width as usize).saturating_sub(24).max(16);
                for (k, v) in rows {
                    lines.push(Line::from(vec![
                        Span::styled(format!("  {k:<20}"), Style::default().fg(Color::DarkGray)),
                        Span::styled(
                            truncate_single_line(&v, w),
                            Style::default().fg(Color::White),
                        ),
                    ]));
                }
            }
            ConfigTab::Usage => {
                let vars = self.build_status_vars();
                let st = self.workers.get(&self.focused_worker);
                let (in_t, out_t, cache_t, reason_t) = match st {
                    Some(w) => (
                        w.handle.session_usage.input_tokens,
                        w.handle.session_usage.output_tokens,
                        w.handle.session_usage.cached(),
                        w.handle.session_usage.reasoning(),
                    ),
                    None => (0, 0, 0, 0),
                };
                let rows = [
                    (
                        "Context watermark",
                        format!(
                            "{} ({}/{})",
                            vars.ctx_pct_num
                                .map(|p| format!("{p}%"))
                                .unwrap_or_else(|| "-".into()),
                            vars.ctx_tokens,
                            vars.ctx_max
                        ),
                    ),
                    ("Input tokens", fmt_k(in_t)),
                    ("  ├─ Cached", fmt_k(cache_t)),
                    ("Output tokens", fmt_k(out_t)),
                    ("  └─ Reasoning", fmt_k(reason_t)),
                    ("Active workers", self.workers.len().to_string()),
                ];
                for (k, v) in rows {
                    lines.push(Line::from(vec![
                        Span::styled(format!("  {k:<22}"), Style::default().fg(Color::DarkGray)),
                        Span::styled(v, Style::default().fg(Color::Cyan)),
                    ]));
                }
            }
        }

        while lines.len() + 1 < inner.height as usize {
            lines.push(Line::default());
        }
        lines.push(Line::from(vec![
            Span::styled("  ↑/↓ ", Style::default().fg(Color::Cyan)),
            Span::styled("Navigate  ", Style::default().fg(Color::DarkGray)),
            Span::styled("enter/space ", Style::default().fg(Color::Cyan)),
            Span::styled("Toggle  ", Style::default().fg(Color::DarkGray)),
            Span::styled("tab ", Style::default().fg(Color::Cyan)),
            Span::styled("Switch Tab  ", Style::default().fg(Color::DarkGray)),
            Span::styled("esc ", Style::default().fg(Color::Cyan)),
            Span::styled("Close", Style::default().fg(Color::DarkGray)),
        ]));
        f.render_widget(Paragraph::new(lines), inner);
    }

    fn open_rewind_picker(&mut self, terminal: &mut UiTerminal) -> io::Result<()> {
        let (wal_path, leaf) = self.focused_wal_and_leaf();
        match WalGraph::load(&wal_path, leaf) {
            Ok(graph) => {
                let points = graph.rewind_points();
                if points.is_empty() {
                    return self.push_notice(
                        terminal,
                        Color::DarkGray,
                        "⎿ nothing to rewind yet (send a message first)",
                    );
                }
                let selected = points.len() - 1;
                let scroll = selected.saturating_sub(5);
                self.overlay = Overlay::Rewind(RewindPicker {
                    points,
                    selected,
                    scroll,
                    choosing_action: false,
                    action_idx: 0,
                });
                Ok(())
            }
            Err(err) => self.push_notice(
                terminal,
                Color::Red,
                format!("failed reading WAL for /rewind: {err}"),
            ),
        }
    }

    fn apply_rewind_action(
        &mut self,
        terminal: &mut UiTerminal,
        pt: &RewindPoint,
        action_idx: usize,
    ) -> io::Result<()> {
        let wid = self.focused_worker.clone();
        match action_idx {
            0 => {
                // Rewind conversation before this prompt & load prompt back into input to edit/resend.
                let target = pt.before_leaf.unwrap_or(pt.after_leaf);
                self.respawn_worker(terminal, &wid, None, Some(Some(target)), None, None)?;
                self.input.set_text(pt.prompt_text.clone());
                self.push_notice(
                    terminal,
                    Color::Cyan,
                    format!(
                        "⎿ rewound conversation to leaf #{target} before prompt #{} (original branch preserved in /tree)",
                        pt.user_node_id
                    ),
                )
            }
            1 => {
                // Keep this turn & drop all later turns off the active branch.
                let target = pt.after_leaf;
                self.respawn_worker(terminal, &wid, None, Some(Some(target)), None, None)?;
                self.push_notice(
                    terminal,
                    Color::Cyan,
                    format!(
                        "⎿ rewound conversation to end of turn #{} (leaf #{target}; original branch preserved in /tree)",
                        pt.user_node_id
                    ),
                )
            }
            _ => {
                // Fork a fresh branch WAL right before this prompt and load prompt into input.
                let target = pt.before_leaf.unwrap_or(pt.after_leaf);
                self.fork_focused_wal(terminal, None, Some(target))?;
                self.input.set_text(pt.prompt_text.clone());
                Ok(())
            }
        }
    }

    fn render_rewind_overlay(&self, f: &mut RenderTarget<'_>, picker: &RewindPicker, area: Rect) {
        let block = Block::default()
            .borders(Borders::TOP)
            .border_style(Style::default().fg(Color::DarkGray));
        let inner = block.inner(area);
        f.render_widget(block, area);

        let width = inner.width as usize;
        let mut lines: Vec<Line<'static>> = Vec::with_capacity(inner.height as usize);

        lines.push(Line::from(vec![
            Span::styled(
                "  Rewind Conversation",
                Style::default()
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                "  (Tree WAL — prior branch stays safe in /tree; code rollback TODO)",
                Style::default().fg(Color::DarkGray),
            ),
        ]));
        lines.push(Line::default());

        if picker.choosing_action {
            if let Some(pt) = picker.points.get(picker.selected) {
                lines.push(Line::from(vec![
                    Span::styled("  Prompt ", Style::default().fg(Color::DarkGray)),
                    Span::styled(
                        format!("#{} ", pt.user_node_id),
                        Style::default().fg(Color::Cyan),
                    ),
                    Span::styled(
                        format!(
                            "❯ {}",
                            truncate_single_line(&pt.preview, width.saturating_sub(18))
                        ),
                        Style::default()
                            .fg(Color::White)
                            .add_modifier(Modifier::BOLD),
                    ),
                ]));
                if let Some(ref reply) = pt.reply_preview {
                    lines.push(Line::from(Span::styled(
                        format!(
                            "    ⎿ {}",
                            truncate_single_line(reply, width.saturating_sub(10))
                        ),
                        Style::default().fg(Color::DarkGray),
                    )));
                }
                lines.push(Line::default());

                let before_tag = pt
                    .before_leaf
                    .map(|l| format!("leaf #{l}"))
                    .unwrap_or_else(|| format!("leaf #{}", pt.after_leaf));
                let actions = [
                    (
                        "[1] Rewind before prompt & edit",
                        format!("restore conversation to {before_tag} and pre-fill prompt"),
                    ),
                    (
                        "[2] Rewind to end of this turn",
                        format!(
                            "keep this turn (leaf #{}) and drop later turns",
                            pt.after_leaf
                        ),
                    ),
                    (
                        "[3] Fork new branch from here",
                        format!("run `anix fork --leaf` at {before_tag} and pre-fill prompt"),
                    ),
                ];
                let col_w = 34usize.min((width / 2).max(24));
                for (idx, (title, desc)) in actions.iter().enumerate() {
                    let is_sel = idx == picker.action_idx;
                    let pfx = if is_sel { "> " } else { "  " };
                    let (ps, ts, ds) = if is_sel {
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
                        Span::styled(pfx, ps),
                        Span::styled(pad_display_width(title, col_w), ts),
                        Span::raw("  "),
                        Span::styled(
                            truncate_single_line(desc, width.saturating_sub(col_w + 5)),
                            ds,
                        ),
                    ]));
                }
            }
            while lines.len() + 1 < inner.height as usize {
                lines.push(Line::default());
            }
            lines.push(Line::from(vec![
                Span::styled("  ↑/↓ ", Style::default().fg(Color::Cyan)),
                Span::styled("Choose  ", Style::default().fg(Color::DarkGray)),
                Span::styled("enter / 1-3 ", Style::default().fg(Color::Cyan)),
                Span::styled("Apply  ", Style::default().fg(Color::DarkGray)),
                Span::styled("esc ", Style::default().fg(Color::Cyan)),
                Span::styled("Back", Style::default().fg(Color::DarkGray)),
            ]));
        } else {
            let visible = (inner.height as usize).saturating_sub(4).max(1);
            let prompt_w = width.saturating_sub(24).max(16);
            for (idx, pt) in picker
                .points
                .iter()
                .enumerate()
                .skip(picker.scroll)
                .take(visible)
            {
                let is_sel = idx == picker.selected;
                let pfx = if is_sel { "> " } else { "  " };
                let left = format!(
                    "#{:<3} ❯ {}",
                    pt.user_node_id,
                    truncate_single_line(&pt.preview, prompt_w)
                );
                let (ps, ls, ts_style) = if is_sel {
                    (
                        Style::default()
                            .fg(Color::Cyan)
                            .add_modifier(Modifier::BOLD),
                        Style::default()
                            .fg(Color::Cyan)
                            .add_modifier(Modifier::BOLD),
                        Style::default().fg(Color::Cyan),
                    )
                } else {
                    (
                        Style::default().fg(Color::DarkGray),
                        Style::default().fg(Color::White),
                        Style::default().fg(Color::DarkGray),
                    )
                };
                lines.push(Line::from(vec![
                    Span::styled(pfx, ps),
                    Span::styled(pad_display_width(&left, width.saturating_sub(13)), ls),
                    Span::raw(" "),
                    Span::styled(format!("{:>8}", pt.ts), ts_style),
                ]));
            }
            while lines.len() + 1 < inner.height as usize {
                lines.push(Line::default());
            }
            lines.push(Line::from(vec![
                Span::styled("  ↑/↓ ", Style::default().fg(Color::Cyan)),
                Span::styled("Navigate  ", Style::default().fg(Color::DarkGray)),
                Span::styled("enter ", Style::default().fg(Color::Cyan)),
                Span::styled("Select checkpoint  ", Style::default().fg(Color::DarkGray)),
                Span::styled("esc ", Style::default().fg(Color::Cyan)),
                Span::styled("Cancel", Style::default().fg(Color::DarkGray)),
            ]));
        }

        f.render_widget(Paragraph::new(lines), inner);
    }

    fn open_in_external_editor(&mut self, terminal: &mut UiTerminal) -> io::Result<()> {
        let editor = std::env::var("VISUAL")
            .ok()
            .filter(|s| !s.trim().is_empty())
            .or_else(|| {
                std::env::var("EDITOR")
                    .ok()
                    .filter(|s| !s.trim().is_empty())
            })
            .unwrap_or_else(|| "vi".into());

        let tmp_path = std::env::temp_dir().join(format!(
            "anix-edit-{}.md",
            &uuid::Uuid::new_v4().simple().to_string()[..8]
        ));
        {
            use std::io::Write;
            use std::os::unix::fs::OpenOptionsExt;
            let mut f = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&tmp_path)?;
            f.write_all(self.input.text().as_bytes())?;
            f.flush()?;
        }

        // Pause crossterm reader thread so `$EDITOR` has exclusive ownership of stdin/TTY.
        self.reader_paused.store(true, Ordering::Release);
        thread::sleep(Duration::from_millis(75));
        let _ = terminal.clear_viewport_for_external();
        let _ = disable_raw_mode();

        let status = std::process::Command::new("/bin/sh")
            .args(["-c", &format!("{editor} \"{}\"", tmp_path.display())])
            .status();

        let _ = enable_raw_mode();
        terminal.resync_after_external();
        self.reader_paused.store(false, Ordering::Release);

        match status {
            Ok(st) if st.success() => {
                if let Ok(edited) = std::fs::read_to_string(&tmp_path) {
                    let trimmed = edited.trim_end_matches('\n').to_owned();
                    self.input.set_text(trimmed);
                    self.hint_line = Some(format!("loaded prompt from `{editor}`"));
                }
            }
            Ok(st) => {
                self.hint_line = Some(format!(
                    "`{editor}` exited with code {}",
                    st.code().unwrap_or(1)
                ));
            }
            Err(err) => {
                self.hint_line = Some(format!("failed launching `{editor}`: {err}"));
            }
        }
        let _ = std::fs::remove_file(&tmp_path);
        Ok(())
    }

    fn suspend_to_shell(&mut self, terminal: &mut UiTerminal) -> io::Result<()> {
        self.reader_paused.store(true, Ordering::Release);
        thread::sleep(Duration::from_millis(75));
        let _ = terminal.clear_viewport_for_external();
        let _ = disable_raw_mode();

        // SAFETY: Standard POSIX job-control suspend; resumes cleanly when user runs `fg`.
        unsafe {
            libc::raise(libc::SIGTSTP);
        }

        let _ = enable_raw_mode();
        terminal.resync_after_external();
        self.reader_paused.store(false, Ordering::Release);
        self.hint_line = Some("resumed from background".into());
        Ok(())
    }

    fn execute_custom_slash_entry(
        &mut self,
        terminal: &mut UiTerminal,
        entry: &CustomSlashEntry,
        args: &str,
    ) -> io::Result<()> {
        match &entry.kind {
            CustomSlashKind::Command(path) => {
                let raw = match std::fs::read_to_string(path) {
                    Ok(s) => s,
                    Err(err) => {
                        return self.push_notice(
                            terminal,
                            Color::Red,
                            format!("failed reading command `{}`: {err}", path.display()),
                        );
                    }
                };
                let body = strip_frontmatter(&raw).trim();
                let expanded = if body.contains("$ARGUMENTS") {
                    body.replace("$ARGUMENTS", args)
                } else if !args.is_empty() {
                    format!("{body}\n\n{args}")
                } else {
                    body.to_owned()
                };
                self.push_notice(
                    terminal,
                    Color::Cyan,
                    format!(
                        "⎿ running custom command {} ({})",
                        entry.slash_name, entry.source_tag
                    ),
                )?;
                self.submit_user_input(terminal, expanded)
            }
            CustomSlashKind::Skill(skill_md) => {
                let prompt = if args.is_empty() {
                    format!(
                        "Read and follow the skill instructions at `{}`.",
                        skill_md.display()
                    )
                } else {
                    format!(
                        "Read and follow the skill instructions at `{}` for this task:\n\n{args}",
                        skill_md.display()
                    )
                };
                self.push_notice(
                    terminal,
                    Color::Cyan,
                    format!(
                        "⎿ invoking skill {} ({})",
                        entry.slash_name,
                        skill_md.display()
                    ),
                )?;
                self.submit_user_input(terminal, prompt)
            }
        }
    }

    fn print_context_grid(&self, terminal: &mut UiTerminal) -> io::Result<()> {
        let (wal_path, leaf) = self.focused_wal_and_leaf();
        let st = self.workers.get(&self.focused_worker);
        let (max_tokens, reported_tokens, model) = match st {
            Some(w) => {
                let m = if w.handle.current_model.is_empty() {
                    w.handle
                        .spec
                        .model
                        .as_deref()
                        .unwrap_or("default")
                        .to_owned()
                } else {
                    w.handle.current_model.clone()
                };
                match w.handle.context.as_ref() {
                    Some(c) => (u64::from(c.max_tokens), Some(c.tokens), m),
                    None => (128_000, None, m),
                }
            }
            None => (128_000, None, "default".into()),
        };

        let bd: ContextBreakdown =
            analyze_wal_context(&wal_path, leaf, max_tokens, reported_tokens);
        let max_f = bd.max_tokens.max(1) as f64;
        let pct_total = ((bd.total_tokens as f64 / max_f) * 100.0).min(100.0);

        let cells_for = |tok: u64| -> usize {
            if tok == 0 {
                0
            } else {
                (((tok as f64 / max_f) * 100.0).round() as usize).max(1)
            }
        };
        let mut grid_cells: Vec<(&'static str, Color)> = Vec::with_capacity(100);
        let push_cat = |grid: &mut Vec<(&'static str, Color)>, n: usize, color: Color| {
            for _ in 0..n {
                if grid.len() < 100 {
                    grid.push(("⛁ ", color));
                }
            }
        };
        push_cat(&mut grid_cells, cells_for(bd.system_tokens), Color::Magenta);
        push_cat(&mut grid_cells, cells_for(bd.user_tokens), Color::Cyan);
        push_cat(
            &mut grid_cells,
            cells_for(bd.assistant_tokens),
            Color::Green,
        );
        push_cat(&mut grid_cells, cells_for(bd.tool_tokens), Color::Yellow);
        push_cat(&mut grid_cells, cells_for(bd.fold_tokens), Color::Blue);
        while grid_cells.len() < 100 {
            grid_cells.push(("⛶ ", Color::DarkGray));
        }

        let free_tokens = bd.max_tokens.saturating_sub(bd.total_tokens);
        let pct_of = |v: u64| format!("{:.1}%", (v as f64 / max_f) * 100.0);
        let legend: Vec<Line<'static>> = vec![
            Line::from(Span::styled(
                model,
                Style::default()
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD),
            )),
            Line::from(Span::styled(
                format!(
                    "{}/{} tokens ({:.1}%)",
                    fmt_k(bd.total_tokens),
                    fmt_k(bd.max_tokens),
                    pct_total
                ),
                Style::default().fg(Color::Cyan),
            )),
            Line::default(),
            Line::from(Span::styled(
                "Estimated usage by category",
                Style::default().fg(Color::Gray),
            )),
            Line::from(vec![
                Span::styled("⛁ ", Style::default().fg(Color::Magenta)),
                Span::styled(
                    format!(
                        "System / updates: {} ({})",
                        fmt_k(bd.system_tokens),
                        pct_of(bd.system_tokens)
                    ),
                    Style::default().fg(Color::White),
                ),
            ]),
            Line::from(vec![
                Span::styled("⛁ ", Style::default().fg(Color::Cyan)),
                Span::styled(
                    format!(
                        "User messages:    {} ({})",
                        fmt_k(bd.user_tokens),
                        pct_of(bd.user_tokens)
                    ),
                    Style::default().fg(Color::White),
                ),
            ]),
            Line::from(vec![
                Span::styled("⛁ ", Style::default().fg(Color::Green)),
                Span::styled(
                    format!(
                        "Assistant turns:  {} ({})",
                        fmt_k(bd.assistant_tokens),
                        pct_of(bd.assistant_tokens)
                    ),
                    Style::default().fg(Color::White),
                ),
            ]),
            Line::from(vec![
                Span::styled("⛁ ", Style::default().fg(Color::Yellow)),
                Span::styled(
                    format!(
                        "Tool outputs ({}): {} ({})",
                        bd.tool_count,
                        fmt_k(bd.tool_tokens),
                        pct_of(bd.tool_tokens)
                    ),
                    Style::default().fg(Color::White),
                ),
            ]),
            Line::from(vec![
                Span::styled("⛁ ", Style::default().fg(Color::Blue)),
                Span::styled(
                    format!(
                        "Folded turns ({}): {} ({})",
                        bd.fold_count,
                        fmt_k(bd.fold_tokens),
                        pct_of(bd.fold_tokens)
                    ),
                    Style::default().fg(Color::White),
                ),
            ]),
            Line::from(vec![
                Span::styled("⛶ ", Style::default().fg(Color::DarkGray)),
                Span::styled(
                    format!(
                        "Free space:       {} ({})",
                        fmt_k(free_tokens),
                        pct_of(free_tokens)
                    ),
                    Style::default().fg(Color::DarkGray),
                ),
            ]),
        ];

        let mut out = vec![
            Line::from(vec![
                Span::styled("❯ ", Style::default().fg(Color::Cyan)),
                Span::styled("/context", Style::default().fg(Color::White)),
            ]),
            Line::from(vec![
                Span::styled("  ⎿ ", Style::default().fg(Color::DarkGray)),
                Span::styled(
                    format!(
                        "Context Usage ({} msgs · {} tools · {} custom cmds/skills)",
                        bd.message_count,
                        bd.tool_count,
                        self.custom_commands.len()
                    ),
                    Style::default()
                        .fg(Color::White)
                        .add_modifier(Modifier::BOLD),
                ),
            ]),
        ];

        for row in 0..10 {
            let mut spans = vec![Span::raw("    ")];
            for col in 0..10 {
                let (sym, color) = grid_cells[row * 10 + col];
                spans.push(Span::styled(sym, Style::default().fg(color)));
            }
            spans.push(Span::raw("  "));
            if let Some(leg) = legend.get(row) {
                spans.extend(leg.spans.clone());
            }
            out.push(Line::from(spans));
        }
        out.push(Line::default());
        self.push_lines(terminal, out)
    }

    fn render_completion_palette(
        &self,
        f: &mut RenderTarget<'_>,
        menu: &CompletionMenu,
        area: Rect,
    ) {
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

    fn render_live_stream(&self, f: &mut RenderTarget<'_>, area: Rect) {
        let mut lines: Vec<Line<'static>> = Vec::new();
        let width = area.width.saturating_sub(2) as usize;
        if let Some(ref bang_cmd) = self.running_bang_cmd {
            let spin = SPINNER_FRAMES[self.spinner_idx];
            lines.push(Line::from(vec![
                Span::styled(format!("{spin} ! "), Style::default().fg(Color::Yellow)),
                Span::styled(
                    format!(
                        "$ {}",
                        truncate_single_line(bang_cmd, width.saturating_sub(10))
                    ),
                    Style::default().fg(Color::Yellow),
                ),
            ]));
        }
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

        if lines.is_empty()
            && let Some(ref hint) = self.hint_line
        {
            lines.push(Line::from(Span::styled(
                format!("  {hint}"),
                Style::default().fg(Color::DarkGray),
            )));
        }

        f.render_widget(Paragraph::new(lines), area);
    }

    fn render_approval_card(&self, f: &mut RenderTarget<'_>, active: &ActiveApproval, area: Rect) {
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
        f: &mut RenderTarget<'_>,
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

    fn render_workers_overlay(&self, f: &mut RenderTarget<'_>, selected: usize, area: Rect) {
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

    fn render_sessions_overlay(&self, f: &mut RenderTarget<'_>, picker: &ResumePicker, area: Rect) {
        let block = Block::default()
            .borders(Borders::TOP)
            .border_style(Style::default().fg(Color::DarkGray));
        let inner = block.inner(area);
        f.render_widget(block, area);

        let width = inner.width as usize;
        let filtered = picker.filtered_indices();
        let total = filtered.len();
        let page_size = RESUME_PAGE_SIZE.min(inner.height.saturating_sub(7).max(1) as usize);
        let page = if total == 0 {
            0
        } else {
            picker.selected / page_size
        };
        let page_start = page * page_size;
        let page_end = (page_start + page_size).min(total);

        let cur_tab_style = if picker.tab == ResumeTab::Project {
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD | Modifier::UNDERLINED)
        } else {
            Style::default().fg(Color::DarkGray)
        };
        let all_tab_style = if picker.tab == ResumeTab::All {
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD | Modifier::UNDERLINED)
        } else {
            Style::default().fg(Color::DarkGray)
        };

        let mut lines: Vec<Line<'static>> = Vec::with_capacity(inner.height as usize);

        // Row 1: Tab bar (`Project   All   (tab to cycle)`)
        lines.push(Line::from(vec![
            Span::raw("  "),
            Span::styled("Project", cur_tab_style),
            Span::raw("   "),
            Span::styled("All", all_tab_style),
            Span::styled("   (tab to cycle)", Style::default().fg(Color::DarkGray)),
        ]));
        lines.push(Line::default());

        // Row 3: Section heading + optional action feedback
        let mut title_spans = vec![Span::styled(
            "  Conversations",
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        )];
        if let Some(ref msg) = picker.status_msg {
            title_spans.push(Span::styled(
                format!("  · {msg}"),
                Style::default().fg(Color::Yellow),
            ));
        }
        lines.push(Line::from(title_spans));

        // Row 4: Search / Rename / ConfirmDelete prompt bar
        match &picker.mode {
            ResumeMode::Browse => {
                if picker.query.is_empty() {
                    lines.push(Line::from(vec![
                        Span::styled(
                            "  Type to search conversations...  ",
                            Style::default()
                                .fg(Color::DarkGray)
                                .add_modifier(Modifier::ITALIC),
                        ),
                        Span::styled(
                            "(type title:<query> to filter by title)",
                            Style::default()
                                .fg(Color::DarkGray)
                                .add_modifier(Modifier::ITALIC),
                        ),
                    ]));
                } else {
                    lines.push(Line::from(vec![
                        Span::styled("  Search: ", Style::default().fg(Color::Cyan)),
                        Span::styled(
                            format!("{}▏", picker.query),
                            Style::default().fg(Color::White),
                        ),
                    ]));
                }
            }
            ResumeMode::Rename { buf } => {
                lines.push(Line::from(vec![
                    Span::styled(
                        "  Rename (Enter saves, Esc cancels): ",
                        Style::default().fg(Color::Yellow),
                    ),
                    Span::styled(format!("{buf}▏"), Style::default().fg(Color::White)),
                ]));
            }
            ResumeMode::ConfirmDelete => {
                let target_title = picker
                    .selected_item()
                    .map(|s| truncate_single_line(&s.meta.title, 36))
                    .unwrap_or_default();
                lines.push(Line::from(vec![
                    Span::styled(
                        format!("  Delete \"{target_title}\"? "),
                        Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(
                        "[y] Confirm  [any key] Cancel",
                        Style::default().fg(Color::Gray),
                    ),
                ]));
            }
        }

        // Rows 5..: Conversation list with right-aligned step count + relative timestamp
        let meta_col_w = 22usize;
        let title_budget = width.saturating_sub(meta_col_w + 4).max(12);
        if total == 0 {
            lines.push(Line::from(Span::styled(
                "  (no matching conversations)",
                Style::default().fg(Color::DarkGray),
            )));
        } else {
            for (rel_idx, &raw_idx) in filtered[page_start..page_end].iter().enumerate() {
                let abs_pos = page_start + rel_idx;
                let item = &picker.all_items[raw_idx];
                let is_sel = abs_pos == picker.selected;
                let is_cur = item.meta.id == self.meta.id;

                let prefix = if is_sel { "> " } else { "  " };
                let mut label = String::new();
                if is_cur {
                    label.push_str("[CURRENT] ");
                } else if picker.tab == ResumeTab::All && !item.is_current_project {
                    label.push_str(&format!("[{}] ", item.project_label));
                }
                label.push_str(&item.meta.title);

                let left_text = pad_display_width(&label, title_budget);
                let steps_str = format!("{} steps", item.turn_count);
                let time_str = format_relative_time(&item.meta.updated_at);
                let right_text = format!("{steps_str:>10}  {time_str:>9}");

                let (p_style, l_style, r_style) = if is_sel {
                    (
                        Style::default()
                            .fg(Color::Cyan)
                            .add_modifier(Modifier::BOLD),
                        Style::default()
                            .fg(Color::Cyan)
                            .add_modifier(Modifier::BOLD),
                        Style::default()
                            .fg(Color::Cyan)
                            .add_modifier(Modifier::ITALIC),
                    )
                } else {
                    (
                        Style::default().fg(Color::DarkGray),
                        Style::default().fg(Color::White),
                        Style::default().fg(Color::Gray),
                    )
                };

                lines.push(Line::from(vec![
                    Span::styled(prefix, p_style),
                    Span::styled(left_text, l_style),
                    Span::raw("  "),
                    Span::styled(right_text, r_style),
                ]));
            }
        }

        // Pagination count line
        if total > 0 {
            lines.push(Line::from(Span::styled(
                format!("  [{}-{} of {} items]", page_start + 1, page_end, total),
                Style::default().fg(Color::DarkGray),
            )));
        }

        while lines.len() + 1 < inner.height as usize {
            lines.push(Line::default());
        }

        // Keyboard legend footer
        lines.push(Line::from(vec![
            Span::styled(
                "Keyboard: ",
                Style::default()
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled("↑/↓ ", Style::default().fg(Color::Cyan)),
            Span::styled("Navigate  ", Style::default().fg(Color::DarkGray)),
            Span::styled("←/→ ", Style::default().fg(Color::Cyan)),
            Span::styled("Page  ", Style::default().fg(Color::DarkGray)),
            Span::styled("enter ", Style::default().fg(Color::Cyan)),
            Span::styled("Select  ", Style::default().fg(Color::DarkGray)),
            Span::styled("f2 ", Style::default().fg(Color::Cyan)),
            Span::styled("Rename  ", Style::default().fg(Color::DarkGray)),
            Span::styled("f4 ", Style::default().fg(Color::Cyan)),
            Span::styled("Delete  ", Style::default().fg(Color::DarkGray)),
            Span::styled("tab ", Style::default().fg(Color::Cyan)),
            Span::styled("Switch Tab  ", Style::default().fg(Color::DarkGray)),
            Span::styled("esc ", Style::default().fg(Color::Cyan)),
            Span::styled("Cancel", Style::default().fg(Color::DarkGray)),
        ]));

        f.render_widget(Paragraph::new(lines), inner);
    }

    fn render_input_box(&self, f: &mut RenderTarget<'_>, area: Rect) {
        let is_bang = self.input.text().starts_with('!');
        let prefix = if is_bang {
            format!("! ({}) ", self.agent_config.bang_mode.as_str())
        } else if self.focused_worker == "main" {
            "> ".to_owned()
        } else {
            format!("> [{}] ", self.focused_worker)
        };
        let prefix_w = prefix.width();
        let is_focused = self.active_approval.is_none() && matches!(self.overlay, Overlay::None);
        let prefix_color = if !is_focused {
            Color::DarkGray
        } else if is_bang {
            Color::Yellow
        } else {
            Color::Cyan
        };
        let border_color = if is_bang && is_focused {
            Color::Yellow
        } else {
            Color::DarkGray
        };
        let block = Block::default()
            .borders(Borders::TOP | Borders::BOTTOM)
            .border_style(Style::default().fg(border_color));

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
                    "Type a prompt, `/` for commands, `!` for shell, or `@` for files…",
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

    fn build_status_vars(&self) -> StatusVars {
        let st = self.workers.get(&self.focused_worker);
        let agent = self
            .meta
            .agent_path
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_else(|| "default.agent".into());
        let dir = self
            .store
            .cwd
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| self.store.cwd.display().to_string());

        let (busy, model, provider, leaf, ctx_pct_num, ctx_tokens, ctx_max, in_tok, out_tok, holds) =
            match st {
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
                    let provider = if w.handle.current_provider.is_empty() {
                        w.handle.spec.provider.as_deref().unwrap_or("-").to_owned()
                    } else {
                        w.handle.current_provider.clone()
                    };
                    let (pct, ct, cm) = match w.handle.context.as_ref() {
                        Some(c) => {
                            let pct = if c.max_tokens > 0 {
                                ((c.tokens * 100) / u64::from(c.max_tokens)).min(100)
                            } else {
                                0
                            };
                            (Some(pct), fmt_k(c.tokens), fmt_k(u64::from(c.max_tokens)))
                        }
                        None => (None, "-".into(), "-".into()),
                    };
                    (
                        w.handle.busy || self.running_bang_cmd.is_some(),
                        model,
                        provider,
                        w.handle.current_leaf,
                        pct,
                        ct,
                        cm,
                        fmt_k(w.handle.session_usage.input_tokens),
                        fmt_k(w.handle.session_usage.output_tokens),
                        w.handle.holds.len(),
                    )
                }
                None => (
                    self.running_bang_cmd.is_some(),
                    "-".into(),
                    "-".into(),
                    None,
                    None,
                    "-".into(),
                    "-".into(),
                    "0".into(),
                    "0".into(),
                    0,
                ),
            };

        StatusVars {
            busy,
            spinner: SPINNER_FRAMES[self.spinner_idx],
            mode: self.policy,
            bang_mode: self.agent_config.bang_mode,
            worker: self.focused_worker.clone(),
            agent,
            model,
            provider,
            cwd: self.store.cwd.display().to_string(),
            dir,
            git_branch: self.git_branch.clone(),
            ctx_pct_num,
            ctx_tokens,
            ctx_max,
            in_tokens: in_tok,
            out_tokens: out_tok,
            leaf,
            holds,
            workers: self.workers.len(),
            session_id: self.meta.id.clone(),
        }
    }

    fn render_status_bar(&self, f: &mut RenderTarget<'_>, area: Rect) {
        let vars = self.build_status_vars();
        let bar = render_status_bar(&self.agent_config.status_bar, &vars, area.width as usize);
        f.render_widget(Paragraph::new(bar), area);
    }
}

fn strip_frontmatter(raw: &str) -> &str {
    let trimmed = raw.trim_start();
    if let Some(rest) = trimmed.strip_prefix("---")
        && let Some((_, body)) = rest.split_once("\n---")
    {
        return body.trim_start_matches('\r').trim_start_matches('\n');
    }
    raw
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
