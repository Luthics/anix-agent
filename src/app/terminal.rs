//! CJK-safe inline terminal backend (`CjkSafeBackend` + `DynamicInlineTerminal`),
//! synchronized updates (`CSI ? 2026`), OSC 2 window title sync, BEL/OSC 9 alerts,
//! and raw-mode / `FLUSHO` terminal guard.

use std::io::{self, Stdout};

use crossterm::event::{DisableBracketedPaste, EnableBracketedPaste};
use crossterm::terminal::{disable_raw_mode, enable_raw_mode};
use ratatui::backend::{Backend, ClearType, WindowSize};
use ratatui::buffer::{Buffer, Cell};
use ratatui::layout::{Position, Rect, Size};
use ratatui::style::{Color, Modifier};
use ratatui::widgets::Widget;
use unicode_width::UnicodeWidthStr;

/// CJK-safe Crossterm backend fixing `ratatui 0.29`'s `Terminal::insert_before` (`draw_lines`)
/// bug where double-width CJK/emoji trailing placeholder cells are emitted as literal spaces,
/// causing wide gaps between Chinese characters and right-edge line clipping.
pub(super) struct CjkSafeBackend<W: io::Write> {
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
pub(super) struct RenderTarget<'a> {
    area: Rect,
    buf: &'a mut Buffer,
    cursor: Option<(u16, u16)>,
}

impl RenderTarget<'_> {
    pub(super) fn area(&self) -> Rect {
        self.area
    }

    pub(super) fn render_widget<W: Widget>(&mut self, widget: W, area: Rect) {
        widget.render(area, self.buf);
    }

    pub(super) fn set_cursor_position(&mut self, pos: (u16, u16)) {
        self.cursor = Some(pos);
    }
}

/// Dynamic-height inline terminal built on [`CjkSafeBackend`] + [`ratatui::buffer::Buffer`].
///
/// Unlike `ratatui::Viewport::Inline(fixed)`, this shrinks cleanly to 4 lines when idle and
/// expands organically ONLY when streaming, typing multi-line input, or opening `/`, `?`,
/// approval cards, or `/resume` — with zero wasted blank rows, zero Alt-Screen hijack, and
/// zero CJK double-width spacing bugs.
pub(super) struct DynamicInlineTerminal<W: io::Write> {
    backend: CjkSafeBackend<W>,
    vp_top: u16,
    vp_height: u16,
    prev_buf: Option<Buffer>,
    last_title: String,
}

impl DynamicInlineTerminal<Stdout> {
    pub(super) fn enter() -> io::Result<Self> {
        let mut backend = CjkSafeBackend::new(io::stdout());
        let pos = backend.get_cursor_position().unwrap_or(Position::new(0, 0));
        let size = backend.size().unwrap_or(Size::new(80, 24));
        Ok(Self {
            backend,
            vp_top: pos.y.min(size.height.saturating_sub(1)),
            vp_height: 0,
            prev_buf: None,
            last_title: String::new(),
        })
    }
}

impl<W: io::Write> DynamicInlineTerminal<W> {
    pub(super) fn size(&self) -> io::Result<Size> {
        self.backend.size()
    }

    pub(super) fn on_resize(&mut self) {
        self.prev_buf = None;
        if let Ok(sz) = self.backend.size() {
            self.vp_top = self.vp_top.min(sz.height.saturating_sub(1));
        }
    }

    pub(super) fn clear_viewport_for_external(&mut self) -> io::Result<()> {
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

    /// Clear the visible terminal screen and scrollback buffer (`CSI 2 J` + `CSI 3 J` + `CSI H`),
    /// resetting the inline viewport anchor to row `0` so fresh session output starts cleanly at the top.
    pub(super) fn clear_screen(&mut self) -> io::Result<()> {
        self.backend.hide_cursor()?;
        crossterm::execute!(
            self.backend.writer,
            crossterm::terminal::Clear(crossterm::terminal::ClearType::All),
            crossterm::terminal::Clear(crossterm::terminal::ClearType::Purge),
            crossterm::cursor::MoveTo(0, 0),
        )?;
        self.vp_top = 0;
        self.vp_height = 0;
        self.prev_buf = None;
        self.backend.flush()
    }

    pub(super) fn resync_after_external(&mut self) {
        self.prev_buf = None;
        self.vp_height = 0;
        let sz = self.backend.size().unwrap_or(Size::new(80, 24));
        if let Ok(pos) = self.backend.get_cursor_position() {
            self.vp_top = pos.y.min(sz.height.saturating_sub(1));
        } else {
            self.vp_top = sz.height.saturating_sub(1);
        }
    }

    pub(super) fn insert_before<F>(&mut self, height: u16, draw_fn: F) -> io::Result<()>
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

    pub(super) fn draw_dynamic<F>(&mut self, desired_height: u16, render_fn: F) -> io::Result<()>
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

    pub(super) fn begin_sync_update(&mut self) -> io::Result<()> {
        write!(self.backend.writer, "\x1b[?2026h")
    }

    pub(super) fn end_sync_update(&mut self) -> io::Result<()> {
        write!(self.backend.writer, "\x1b[?2026l")?;
        self.backend.flush()
    }

    /// Sync terminal / tmux window title via OSC 2 (`ESC ] 2 ; <title> BEL`), deduped so
    /// unchanged frames emit zero escape bytes.
    pub(super) fn set_window_title(&mut self, title: &str) {
        let clean = sanitize_osc_text(title);
        if clean == self.last_title {
            return;
        }
        if write!(self.backend.writer, "\x1b]2;{clean}\x07").is_ok() {
            let _ = self.backend.flush();
            self.last_title = clean;
        }
    }

    /// Emit terminal bell (`BEL`) + OSC 9 desktop notification for long-task completion or
    /// interactive shell approval requests.
    pub(super) fn notify_user(&mut self, msg: &str) {
        let clean = sanitize_osc_text(msg);
        let _ = write!(self.backend.writer, "\x07\x1b]9;{clean}\x07");
        let _ = self.backend.flush();
    }

    pub(super) fn finish(&mut self) -> io::Result<()> {
        if !self.last_title.is_empty() {
            let _ = write!(self.backend.writer, "\x1b]2;\x07");
            self.last_title.clear();
        }
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

pub(super) type UiTerminal = DynamicInlineTerminal<Stdout>;

/// Clear macOS/BSD `FLUSHO` output-discard latch in case `Ctrl+O` (`VDISCARD`) was hit while in
/// cooked mode (`$EDITOR` / `Ctrl+Z`), preventing silent stdout swallowing.
pub(super) fn clear_tty_flusho() {
    #[cfg(any(
        target_os = "macos",
        target_os = "freebsd",
        target_os = "openbsd",
        target_os = "netbsd"
    ))]
    unsafe {
        let mut tio: libc::termios = std::mem::zeroed();
        if libc::tcgetattr(libc::STDOUT_FILENO, &mut tio) == 0 && (tio.c_lflag & libc::FLUSHO) != 0
        {
            tio.c_lflag &= !libc::FLUSHO;
            let _ = libc::tcsetattr(libc::STDOUT_FILENO, libc::TCSANOW, &tio);
        }
    }
}

pub(super) struct TerminalGuard;

impl TerminalGuard {
    pub(super) fn enter() -> io::Result<Self> {
        clear_tty_flusho();
        enable_raw_mode()?;
        let _ = crossterm::execute!(io::stdout(), EnableBracketedPaste);
        let orig_hook = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            let _ = crossterm::execute!(io::stdout(), DisableBracketedPaste);
            let _ = disable_raw_mode();
            let _ = crossterm::execute!(io::stdout(), crossterm::cursor::Show);
            orig_hook(info);
        }));
        Ok(Self)
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = crossterm::execute!(io::stdout(), DisableBracketedPaste);
        let _ = disable_raw_mode();
        let _ = crossterm::execute!(io::stdout(), crossterm::cursor::Show);
    }
}

pub(super) fn sanitize_osc_text(raw: &str) -> String {
    raw.chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect::<String>()
        .trim()
        .to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::buffer::Buffer;
    use ratatui::text::Line;
    use ratatui::widgets::Paragraph;

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

    #[test]
    fn osc_title_dedup_and_bell_osc9_alert_sequences() {
        let mut term = DynamicInlineTerminal {
            backend: CjkSafeBackend::new(Vec::<u8>::new()),
            vp_top: 0,
            vp_height: 0,
            prev_buf: None,
            last_title: String::new(),
        };
        term.set_window_title("▶ anix-agent — demo\x07\n");
        let after_first = term.backend.writer.len();
        // Repeating the identical sanitized title must emit zero extra bytes.
        term.set_window_title("▶ anix-agent — demo");
        assert_eq!(term.backend.writer.len(), after_first);

        term.notify_user("anix-agent: finished (12.4s)");
        let out = String::from_utf8(term.backend.writer).unwrap();
        assert!(out.contains("\x1b]2;▶ anix-agent — demo\x07"));
        assert!(out.contains("\x07\x1b]9;anix-agent: finished (12.4s)\x07"));
    }

    #[test]
    fn clear_screen_resets_viewport_and_emits_clear_sequences() {
        let mut term = DynamicInlineTerminal {
            backend: CjkSafeBackend::new(Vec::<u8>::new()),
            vp_top: 12,
            vp_height: 4,
            prev_buf: Some(Buffer::empty(Rect::new(0, 0, 10, 4))),
            last_title: String::new(),
        };
        term.clear_screen().unwrap();
        assert_eq!(term.vp_top, 0);
        assert_eq!(term.vp_height, 0);
        assert!(term.prev_buf.is_none());
        let out = String::from_utf8(term.backend.writer).unwrap();
        assert!(out.contains("\x1b[2J"), "expected CSI 2 J, got: {out:?}");
        assert!(out.contains("\x1b[3J"), "expected CSI 3 J, got: {out:?}");
        assert!(
            out.contains("\x1b[1;1H"),
            "expected MoveTo(0,0), got: {out:?}"
        );
    }
}
