//! Dynamic inline viewport sizing and all Ratatui rendering routines (input box, live stream,
//! approval card, completion palette, overlays, and DIY status bar).

use std::time::Instant;

use ratatui::layout::{Constraint, Direction, Layout, Rect, Size};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
use unicode_width::UnicodeWidthStr;

use super::*;

impl Workbench {
    pub(super) fn input_box_height(&self, width: u16) -> u16 {
        let text_w = (width as usize).saturating_sub(8).max(8);
        let (visual_lines, _) = self.input.visual_lines_and_cursor(text_w);
        let rows = visual_lines.len().clamp(1, 5) as u16;
        rows + 2 // top & bottom subtle horizontal rule
    }

    pub(super) fn live_stream_height(&self, width: u16, max_h: u16) -> u16 {
        let stream_w = (width as usize).saturating_sub(1).max(30);
        let mut status_count = 0u16;
        let mut preview_count = 0u16;
        if self.running_bang_cmd.is_some() {
            status_count += 1;
        }
        if let Some(st) = self.workers.get(&self.focused_worker) {
            let preview = st.text_streamer.preview_lines(stream_w);
            preview_count = preview.len().min(max_h as usize) as u16;
            if !st.plan_steps.is_empty() {
                status_count += 1 + st.plan_steps.len().min(5) as u16;
            }
            if !st.queued_steering.is_empty() {
                status_count += 1;
            }
            if st.handle.busy {
                status_count += 1;
            }
            status_count += st.active_tools.len().min(3) as u16;
            if !st.streaming_reasoning.trim().is_empty() && !st.text_streamer.has_visible_content()
            {
                status_count += 1;
            }
        }
        let total = preview_count.saturating_add(status_count);
        if total == 0 && self.hint_line.is_some() {
            return 1;
        }
        total.min(max_h.max(1))
    }

    pub(super) fn desired_viewport_height(&self, size: Size) -> u16 {
        let width = size.width;
        let input_h = self.input_box_height(width);
        if self.active_approval.is_some() {
            return 5 + input_h + 1;
        }
        match &self.overlay {
            Overlay::Sessions(_) => 14,
            Overlay::Config(_) => 13,
            Overlay::Rewind(_) => 13,
            Overlay::Transcript(_) => 16.min(size.height.saturating_sub(1)).max(8),
            Overlay::Diff(_) => 18.min(size.height.saturating_sub(1)).max(10),
            Overlay::Memory(_) => 17.min(size.height.saturating_sub(1)).max(10),
            Overlay::Import(_) => 14,
            Overlay::HistorySearch(_) => 11,
            Overlay::Models(_) => 14,
            Overlay::Tree { .. } => 10 + input_h + 1,
            Overlay::Workers { .. } => 7 + input_h + 1,
            Overlay::Shortcuts => input_h + 8 + 1,
            Overlay::None => {
                if let Some(menu) = self.input.completion() {
                    let items_h = menu.items.len().min(COMPLETION_VISIBLE_ROWS) as u16;
                    let overflow_h = u16::from(
                        menu.hidden_above() > 0 || menu.hidden_below(COMPLETION_VISIBLE_ROWS) > 0,
                    );
                    return input_h + items_h + overflow_h + 2 + 1;
                }
                let max_live_h = size.height.saturating_sub(input_h + 2).max(4);
                let live_h = self.live_stream_height(width, max_live_h);
                live_h + input_h + 1
            }
        }
    }

    pub(super) fn render(&self, f: &mut RenderTarget<'_>) {
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
        if self.active_approval.is_none()
            && let Overlay::Transcript(ref viewer) = self.overlay
        {
            let chunks = Layout::default()
                .direction(Direction::Vertical)
                .constraints([Constraint::Min(6), Constraint::Length(1)])
                .split(area);
            self.render_transcript_overlay(f, viewer, chunks[0]);
            self.render_status_bar(f, chunks[1]);
            return;
        }
        if self.active_approval.is_none()
            && let Overlay::Diff(ref viewer) = self.overlay
        {
            let chunks = Layout::default()
                .direction(Direction::Vertical)
                .constraints([Constraint::Min(6), Constraint::Length(1)])
                .split(area);
            self.render_diff_overlay(f, viewer, chunks[0]);
            self.render_status_bar(f, chunks[1]);
            return;
        }
        if self.active_approval.is_none()
            && let Overlay::Memory(ref picker) = self.overlay
        {
            let chunks = Layout::default()
                .direction(Direction::Vertical)
                .constraints([Constraint::Min(6), Constraint::Length(1)])
                .split(area);
            self.render_memory_overlay(f, picker, chunks[0]);
            self.render_status_bar(f, chunks[1]);
            return;
        }
        if self.active_approval.is_none()
            && let Overlay::Import(ref picker) = self.overlay
        {
            let chunks = Layout::default()
                .direction(Direction::Vertical)
                .constraints([Constraint::Min(6), Constraint::Length(1)])
                .split(area);
            self.render_import_overlay(f, picker, chunks[0]);
            self.render_status_bar(f, chunks[1]);
            return;
        }
        if self.active_approval.is_none()
            && let Overlay::HistorySearch(ref picker) = self.overlay
        {
            let chunks = Layout::default()
                .direction(Direction::Vertical)
                .constraints([Constraint::Min(6), Constraint::Length(1)])
                .split(area);
            self.render_history_search_overlay(f, picker, chunks[0]);
            self.render_status_bar(f, chunks[1]);
            return;
        }
        if self.active_approval.is_none()
            && let Overlay::Models(ref picker) = self.overlay
        {
            let chunks = Layout::default()
                .direction(Direction::Vertical)
                .constraints([Constraint::Min(6), Constraint::Length(1)])
                .split(area);
            self.render_models_overlay(f, picker, chunks[0]);
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
                    | Overlay::Rewind(_)
                    | Overlay::Transcript(_)
                    | Overlay::Diff(_)
                    | Overlay::Memory(_)
                    | Overlay::Import(_)
                    | Overlay::HistorySearch(_)
                    | Overlay::Models(_) => {}
                }
            }
            self.render_input_box(f, chunks[1]);
            self.render_status_bar(f, chunks[2]);
            return;
        }

        // Default compact mode: 0 live rows when idle -> EXACTLY 4 lines total!
        let max_live_h = area.height.saturating_sub(input_h + 1);
        let live_h = self.live_stream_height(area.width, max_live_h);
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

    pub(super) fn render_shortcuts_panel(&self, f: &mut RenderTarget<'_>, area: Rect) {
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
                "esc interrupt / ×2 rewind",
                "ctrl + r search history",
            ),
            (
                "@ for file paths",
                "ctrl + o expand details",
                "ctrl + z suspend (fg back)",
            ),
            (
                "/resume browse chats",
                "ctrl + v paste image/text",
                "/model pick/switch model",
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

    pub(super) fn render_config_overlay(
        &self,
        f: &mut RenderTarget<'_>,
        picker: &ConfigPicker,
        area: Rect,
    ) {
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
                let cached_label = if in_t > 0 && cache_t > 0 {
                    let hit = ((cache_t * 100) / in_t).min(100);
                    format!("{} ({hit}% hit)", fmt_k(cache_t))
                } else {
                    fmt_k(cache_t)
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
                    ("  ├─ Cached", cached_label),
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

    pub(super) fn render_rewind_overlay(
        &self,
        f: &mut RenderTarget<'_>,
        picker: &RewindPicker,
        area: Rect,
    ) {
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

    pub(super) fn render_transcript_overlay(
        &self,
        f: &mut RenderTarget<'_>,
        viewer: &TranscriptViewer,
        area: Rect,
    ) {
        let block = Block::default()
            .borders(Borders::TOP)
            .border_style(Style::default().fg(Color::DarkGray));
        let inner = block.inner(area);
        f.render_widget(block, area);

        let body_h = (inner.height as usize).saturating_sub(2).max(1);
        let total = viewer.lines.len();
        let start = viewer.scroll.min(total.saturating_sub(body_h));
        let end = (start + body_h).min(total);
        let auto_tag = if self.expand_details { "on" } else { "off" };

        let mut out = Vec::with_capacity(inner.height as usize);
        let mut hdr = vec![
            Span::styled(
                "  Expanded Details ",
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!(
                    "({} items · lines {}-{} / {} · auto-expand:{auto_tag})",
                    viewer.entry_count,
                    start + 1,
                    end,
                    total
                ),
                Style::default().fg(Color::DarkGray),
            ),
        ];
        if let Some(ref msg) = viewer.status_msg {
            hdr.push(Span::styled(
                format!(" · {msg}"),
                Style::default().fg(Color::Green),
            ));
        }
        out.push(Line::from(hdr));

        for line in &viewer.lines[start..end] {
            out.push(line.clone());
        }
        while out.len() + 1 < inner.height as usize {
            out.push(Line::default());
        }
        out.push(Line::from(vec![
            Span::styled("  ↑/↓/j/k ", Style::default().fg(Color::Cyan)),
            Span::styled("Scroll  ", Style::default().fg(Color::DarkGray)),
            Span::styled("PgUp/PgDn ", Style::default().fg(Color::Cyan)),
            Span::styled("Page  ", Style::default().fg(Color::DarkGray)),
            Span::styled("g/G ", Style::default().fg(Color::Cyan)),
            Span::styled("Top/Bottom  ", Style::default().fg(Color::DarkGray)),
            Span::styled("e ", Style::default().fg(Color::Cyan)),
            Span::styled("Toggle default  ", Style::default().fg(Color::DarkGray)),
            Span::styled("ctrl+o / esc ", Style::default().fg(Color::Cyan)),
            Span::styled("Close", Style::default().fg(Color::DarkGray)),
        ]));
        f.render_widget(Paragraph::new(out), inner);
    }

    pub(super) fn render_diff_overlay(
        &self,
        f: &mut RenderTarget<'_>,
        viewer: &DiffViewer,
        area: Rect,
    ) {
        let block = Block::default()
            .borders(Borders::TOP)
            .border_style(Style::default().fg(Color::DarkGray));
        let inner = block.inner(area);
        f.render_widget(block, area);

        let total_add: usize = viewer.files.iter().map(|e| e.added).sum();
        let total_del: usize = viewer.files.iter().map(|e| e.removed).sum();
        let cur = viewer.files.get(viewer.selected_file);

        let mut out = Vec::with_capacity(inner.height as usize);
        let mut hdr = vec![
            Span::styled(
                "  Workspace Diff ",
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!(
                    "({} · {} file{} · ",
                    viewer.scope_label,
                    viewer.files.len(),
                    if viewer.files.len() == 1 { "" } else { "s" }
                ),
                Style::default().fg(Color::DarkGray),
            ),
            Span::styled(format!("+{total_add}"), Style::default().fg(Color::Green)),
            Span::styled(" ", Style::default().fg(Color::DarkGray)),
            Span::styled(format!("-{total_del}"), Style::default().fg(Color::Red)),
            Span::styled(")", Style::default().fg(Color::DarkGray)),
        ];
        if let Some(ref msg) = viewer.status_msg {
            hdr.push(Span::styled(
                format!(" · {msg}"),
                Style::default().fg(Color::Green),
            ));
        }
        out.push(Line::from(hdr));

        // File selector strip: show active file prominently plus adjacent tabs within width.
        let mut tab_spans: Vec<Span<'static>> = vec![Span::styled(
            format!(
                "  [{}/{}] ",
                viewer.selected_file + 1,
                viewer.files.len().max(1)
            ),
            Style::default().fg(Color::DarkGray),
        )];
        let max_cols = inner.width.saturating_sub(14) as usize;
        let mut used_cols = 0usize;
        let tab_start = viewer.selected_file.saturating_sub(1);
        for (idx, file) in viewer.files.iter().enumerate().skip(tab_start) {
            let label = format!(
                "{} {} (+{} -{})",
                file.status, file.path, file.added, file.removed
            );
            let label_w = label.width() + 4;
            if used_cols > 0 && used_cols + label_w > max_cols {
                tab_spans.push(Span::styled(" …", Style::default().fg(Color::DarkGray)));
                break;
            }
            if used_cols > 0 {
                tab_spans.push(Span::styled("  │  ", Style::default().fg(Color::DarkGray)));
                used_cols += 5;
            }
            if idx == viewer.selected_file {
                tab_spans.push(Span::styled(
                    label,
                    Style::default()
                        .fg(Color::White)
                        .add_modifier(Modifier::BOLD | Modifier::UNDERLINED),
                ));
            } else {
                tab_spans.push(Span::styled(label, Style::default().fg(Color::DarkGray)));
            }
            used_cols += label_w;
        }
        out.push(Line::from(tab_spans));

        // Scrollable colorized diff body.
        let body_h = (inner.height as usize).saturating_sub(3).max(1);
        if let Some(file) = cur {
            let total = file.lines.len();
            let start = viewer.scroll.min(total.saturating_sub(body_h));
            let end = (start + body_h).min(total);
            for line in &file.lines[start..end] {
                out.push(line.clone());
            }
        }
        while out.len() + 1 < inner.height as usize {
            out.push(Line::default());
        }
        out.push(Line::from(vec![
            Span::styled("  ↑/↓ ", Style::default().fg(Color::Cyan)),
            Span::styled("Scroll  ", Style::default().fg(Color::DarkGray)),
            Span::styled("←/→/Tab ", Style::default().fg(Color::Cyan)),
            Span::styled("File  ", Style::default().fg(Color::DarkGray)),
            Span::styled("r ", Style::default().fg(Color::Cyan)),
            Span::styled("Review file  ", Style::default().fg(Color::DarkGray)),
            Span::styled("R ", Style::default().fg(Color::Cyan)),
            Span::styled("Review all  ", Style::default().fg(Color::DarkGray)),
            Span::styled("c ", Style::default().fg(Color::Cyan)),
            Span::styled("Copy  ", Style::default().fg(Color::DarkGray)),
            Span::styled("esc ", Style::default().fg(Color::Cyan)),
            Span::styled("Close", Style::default().fg(Color::DarkGray)),
        ]));
        f.render_widget(Paragraph::new(out), inner);
    }

    pub(super) fn render_memory_overlay(
        &self,
        f: &mut RenderTarget<'_>,
        picker: &MemoryPicker,
        area: Rect,
    ) {
        let block = Block::default()
            .borders(Borders::TOP)
            .border_style(Style::default().fg(Color::DarkGray));
        let inner = block.inner(area);
        f.render_widget(block, area);

        let width = inner.width as usize;
        let mut out = Vec::with_capacity(inner.height as usize);
        let mut hdr = vec![
            Span::styled(
                "  Memory & Context Sources ",
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!(
                    "({})",
                    truncate_single_line(
                        &picker.memory_dir.display().to_string(),
                        width.saturating_sub(36)
                    )
                ),
                Style::default().fg(Color::DarkGray),
            ),
        ];
        if let Some(ref msg) = picker.status_msg {
            hdr.push(Span::styled(
                format!(" · {msg}"),
                Style::default().fg(if picker.confirm_delete {
                    Color::Yellow
                } else {
                    Color::Green
                }),
            ));
        }
        out.push(Line::from(hdr));

        // Render rows with compact inline section tags so everything stays aligned and scrollable.
        let body_h = (inner.height as usize).saturating_sub(2).max(1);
        let start = picker.scroll.min(picker.items.len().saturating_sub(body_h));
        let end = (start + body_h).min(picker.items.len());

        for idx in start..end {
            let is_sel = idx == picker.selected;
            let cursor = if is_sel { "▸ " } else { "  " };
            let cur_style = if is_sel {
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::DarkGray)
            };
            let main_style = if is_sel {
                Style::default()
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::Gray)
            };

            let line = match &picker.items[idx] {
                MemoryEntryKind::Instruction {
                    source_label,
                    path,
                    exists,
                    bytes,
                    lines,
                } => {
                    let meta = if *exists {
                        format!(
                            "~{} tok · {}L · {}",
                            ((*bytes) / 3).max(1),
                            lines,
                            truncate_single_line(
                                &path.display().to_string(),
                                width.saturating_sub(48)
                            )
                        )
                    } else {
                        "not created — press e to create".to_owned()
                    };
                    Line::from(vec![
                        Span::styled(format!("  {cursor}"), cur_style),
                        Span::styled("[rule]  ", Style::default().fg(Color::Blue)),
                        Span::styled(pad_display_width(source_label, 22), main_style),
                        Span::styled(
                            meta,
                            Style::default().fg(if *exists {
                                Color::DarkGray
                            } else {
                                Color::Yellow
                            }),
                        ),
                    ])
                }
                MemoryEntryKind::MemoryIndex {
                    exists,
                    bytes,
                    lines,
                    ..
                } => {
                    let meta = if *exists {
                        format!(
                            "{lines}/200 lines · {} B (~{} tok)",
                            bytes,
                            ((*bytes) / 3).max(1)
                        )
                    } else {
                        "empty — press e to create or let model record notes".to_owned()
                    };
                    Line::from(vec![
                        Span::styled(format!("  {cursor}"), cur_style),
                        Span::styled("[index] ", Style::default().fg(Color::Magenta)),
                        Span::styled(pad_display_width("MEMORY.md", 22), main_style),
                        Span::styled(
                            meta,
                            Style::default().fg(if *lines > 200 {
                                Color::Yellow
                            } else {
                                Color::DarkGray
                            }),
                        ),
                    ])
                }
                MemoryEntryKind::MemoryTopic {
                    name,
                    bytes,
                    lines,
                    mem_type,
                    ..
                } => {
                    let type_badge = mem_type
                        .as_deref()
                        .map(|t| format!("[{t}] "))
                        .unwrap_or_default();
                    Line::from(vec![
                        Span::styled(format!("  {cursor}"), cur_style),
                        Span::styled("[topic] ", Style::default().fg(Color::Cyan)),
                        Span::styled(pad_display_width(name, 22), main_style),
                        Span::styled(type_badge, Style::default().fg(Color::Yellow)),
                        Span::styled(
                            format!("{lines}L · {bytes} B"),
                            Style::default().fg(Color::DarkGray),
                        ),
                    ])
                }
                MemoryEntryKind::PreContextHook {
                    name,
                    state,
                    summary,
                    ..
                } => {
                    let (badge, badge_color) = match state {
                        HookState::Enabled => ("● on    ", Color::Green),
                        HookState::Masked => ("○ mask  ", Color::Yellow),
                        HookState::Missing => ("· off   ", Color::DarkGray),
                    };
                    Line::from(vec![
                        Span::styled(format!("  {cursor}"), cur_style),
                        Span::styled(badge, Style::default().fg(badge_color)),
                        Span::styled(pad_display_width(name, 22), main_style),
                        Span::styled(
                            truncate_single_line(summary, width.saturating_sub(36)),
                            Style::default().fg(Color::DarkGray),
                        ),
                    ])
                }
            };
            out.push(line);
        }

        while out.len() + 1 < inner.height as usize {
            out.push(Line::default());
        }
        out.push(Line::from(vec![
            Span::styled("  ↑/↓ ", Style::default().fg(Color::Cyan)),
            Span::styled("Select  ", Style::default().fg(Color::DarkGray)),
            Span::styled("e/Enter ", Style::default().fg(Color::Cyan)),
            Span::styled("Edit/Toggle  ", Style::default().fg(Color::DarkGray)),
            Span::styled("n ", Style::default().fg(Color::Cyan)),
            Span::styled("New note  ", Style::default().fg(Color::DarkGray)),
            Span::styled("d ", Style::default().fg(Color::Cyan)),
            Span::styled("Delete  ", Style::default().fg(Color::DarkGray)),
            Span::styled("t ", Style::default().fg(Color::Cyan)),
            Span::styled("Mask hook  ", Style::default().fg(Color::DarkGray)),
            Span::styled("r ", Style::default().fg(Color::Cyan)),
            Span::styled("Consolidate  ", Style::default().fg(Color::DarkGray)),
            Span::styled("c ", Style::default().fg(Color::Cyan)),
            Span::styled("Reload  ", Style::default().fg(Color::DarkGray)),
            Span::styled("esc ", Style::default().fg(Color::Cyan)),
            Span::styled("Close", Style::default().fg(Color::DarkGray)),
        ]));
        f.render_widget(Paragraph::new(out), inner);
    }

    pub(super) fn render_import_overlay(
        &self,
        f: &mut RenderTarget<'_>,
        picker: &ImportPicker,
        area: Rect,
    ) {
        let block = Block::default()
            .borders(Borders::TOP)
            .border_style(Style::default().fg(Color::DarkGray));
        let inner = block.inner(area);
        f.render_widget(block, area);

        let width = inner.width as usize;
        let filtered = picker.filtered_indices();
        let total = filtered.len();
        let page = picker.selected / RESUME_PAGE_SIZE;
        let page_start = page * RESUME_PAGE_SIZE;
        let page_end = (page_start + RESUME_PAGE_SIZE).min(total);

        let tab_style = |active: bool| {
            if active {
                Style::default()
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD | Modifier::UNDERLINED)
            } else {
                Style::default().fg(Color::DarkGray)
            }
        };

        let claude_n = picker
            .all_items
            .iter()
            .filter(|i| i.source == ImportSource::Claude)
            .count();
        let codex_n = picker
            .all_items
            .iter()
            .filter(|i| i.source == ImportSource::Codex)
            .count();
        let agy_n = picker
            .all_items
            .iter()
            .filter(|i| i.source == ImportSource::Antigravity)
            .count();

        let mut lines: Vec<Line<'static>> = Vec::with_capacity(inner.height as usize);
        lines.push(Line::from(vec![
            Span::raw("  "),
            Span::styled(
                format!("All ({})", picker.all_items.len()),
                tab_style(picker.tab == ImportTab::All),
            ),
            Span::raw("   "),
            Span::styled(
                format!("Claude ({claude_n})"),
                tab_style(picker.tab == ImportTab::Claude),
            ),
            Span::raw("   "),
            Span::styled(
                format!("Codex ({codex_n})"),
                tab_style(picker.tab == ImportTab::Codex),
            ),
            Span::raw("   "),
            Span::styled(
                format!("Antigravity ({agy_n})"),
                tab_style(picker.tab == ImportTab::Antigravity),
            ),
            Span::styled("   (tab to switch)", Style::default().fg(Color::DarkGray)),
        ]));
        lines.push(Line::default());

        let mut hdr = vec![Span::styled(
            "  Import External Session",
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        )];
        if let Some(ref msg) = picker.status_msg {
            hdr.push(Span::styled(
                format!("  · {msg}"),
                Style::default().fg(Color::Yellow),
            ));
        }
        lines.push(Line::from(hdr));

        if picker.query.is_empty() {
            lines.push(Line::from(Span::styled(
                "  Type to filter by title, cwd, or session ID…",
                Style::default()
                    .fg(Color::DarkGray)
                    .add_modifier(Modifier::ITALIC),
            )));
        } else {
            lines.push(Line::from(vec![
                Span::styled("  Search: ", Style::default().fg(Color::Cyan)),
                Span::styled(picker.query.clone(), Style::default().fg(Color::White)),
            ]));
        }

        if filtered.is_empty() {
            lines.push(Line::from(Span::styled(
                "  (no matching external sessions)",
                Style::default().fg(Color::DarkGray),
            )));
        } else {
            for (rel_idx, &raw_idx) in filtered[page_start..page_end].iter().enumerate() {
                let abs_idx = page_start + rel_idx;
                let item = &picker.all_items[raw_idx];
                let is_sel = abs_idx == picker.selected;
                let (badge_txt, badge_color) = match item.source {
                    ImportSource::Claude => ("[claude]", Color::Magenta),
                    ImportSource::Codex => ("[codex] ", Color::Green),
                    ImportSource::Antigravity => ("[agy]   ", Color::Cyan),
                };
                let cwd_tag = if item.matches_cwd { "[cwd] " } else { "" };
                let right = format!(
                    "{:>3} turns  {:>8}",
                    item.turn_count,
                    format_relative_time(&item.updated_at)
                );
                let left_budget = width.saturating_sub(right.width() + 16).max(16);
                let title_raw = format!("{cwd_tag}{}", item.title);
                let title_text = pad_display_width(&title_raw, left_budget);

                let cursor = if is_sel { "> " } else { "  " };
                let title_style = if is_sel {
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(Color::White)
                };
                let right_style = if is_sel {
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::ITALIC)
                } else {
                    Style::default().fg(Color::DarkGray)
                };

                lines.push(Line::from(vec![
                    Span::styled(cursor, title_style),
                    Span::styled(format!("{badge_txt} "), Style::default().fg(badge_color)),
                    Span::styled(title_text, title_style),
                    Span::raw("  "),
                    Span::styled(right, right_style),
                ]));
            }
            lines.push(Line::from(Span::styled(
                format!("  [{}-{} of {total} sessions]", page_start + 1, page_end),
                Style::default().fg(Color::DarkGray),
            )));
        }

        while lines.len() + 1 < inner.height as usize {
            lines.push(Line::default());
        }
        lines.push(Line::from(vec![
            Span::styled("  ↑/↓ ", Style::default().fg(Color::Cyan)),
            Span::styled("Select  ", Style::default().fg(Color::DarkGray)),
            Span::styled("←/→ ", Style::default().fg(Color::Cyan)),
            Span::styled("Page  ", Style::default().fg(Color::DarkGray)),
            Span::styled("tab ", Style::default().fg(Color::Cyan)),
            Span::styled("Source  ", Style::default().fg(Color::DarkGray)),
            Span::styled("enter ", Style::default().fg(Color::Cyan)),
            Span::styled("Import & open  ", Style::default().fg(Color::DarkGray)),
            Span::styled("esc ", Style::default().fg(Color::Cyan)),
            Span::styled("Cancel", Style::default().fg(Color::DarkGray)),
        ]));
        f.render_widget(Paragraph::new(lines), inner);
    }

    pub(super) fn render_history_search_overlay(
        &self,
        f: &mut RenderTarget<'_>,
        picker: &HistorySearchPicker,
        area: Rect,
    ) {
        let block = Block::default()
            .borders(Borders::TOP)
            .border_style(Style::default().fg(Color::DarkGray));
        let inner = block.inner(area);
        f.render_widget(block, area);

        let width = inner.width as usize;
        let matches = self.filtered_history_indices(&picker.query);
        let mut out = Vec::with_capacity(inner.height as usize);
        out.push(Line::from(vec![
            Span::styled(
                "  (reverse-i-search) ",
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                if picker.query.is_empty() {
                    "type to filter history…".to_owned()
                } else {
                    picker.query.clone()
                },
                if picker.query.is_empty() {
                    Style::default().fg(Color::DarkGray)
                } else {
                    Style::default()
                        .fg(Color::White)
                        .add_modifier(Modifier::BOLD)
                },
            ),
            Span::styled(
                format!("  ({} matches)", matches.len()),
                Style::default().fg(Color::DarkGray),
            ),
        ]));
        out.push(Line::default());

        let visible = (inner.height as usize).saturating_sub(3).max(1);
        let scroll = picker.selected.saturating_sub(visible.saturating_sub(1));
        if matches.is_empty() {
            out.push(Line::from(Span::styled(
                "    (no matching history entries)",
                Style::default().fg(Color::DarkGray),
            )));
        } else {
            let text_w = width.saturating_sub(18).max(16);
            for (rank, &raw_idx) in matches.iter().enumerate().skip(scroll).take(visible) {
                let Some(entry) = self.history_entries.get(raw_idx) else {
                    continue;
                };
                let is_sel = rank == picker.selected;
                let pfx = if is_sel { "> " } else { "  " };
                let snippet = truncate_single_line(&entry.text, text_w);
                let rel = format_relative_time(&entry.ts);
                let (ps, ts, rs) = if is_sel {
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
                out.push(Line::from(vec![
                    Span::styled(pfx, ps),
                    Span::styled(pad_display_width(&snippet, text_w), ts),
                    Span::raw("  "),
                    Span::styled(format!("{rel:>8}"), rs),
                ]));
            }
        }

        while out.len() + 1 < inner.height as usize {
            out.push(Line::default());
        }
        out.push(Line::from(vec![
            Span::styled("  enter/tab ", Style::default().fg(Color::Cyan)),
            Span::styled("Load  ", Style::default().fg(Color::DarkGray)),
            Span::styled("ctrl+r / ↑↓ ", Style::default().fg(Color::Cyan)),
            Span::styled("Next/Prev  ", Style::default().fg(Color::DarkGray)),
            Span::styled("ctrl+u ", Style::default().fg(Color::Cyan)),
            Span::styled("Clear query  ", Style::default().fg(Color::DarkGray)),
            Span::styled("esc ", Style::default().fg(Color::Cyan)),
            Span::styled("Cancel", Style::default().fg(Color::DarkGray)),
        ]));
        f.render_widget(Paragraph::new(out), inner);
    }

    pub(super) fn render_models_overlay(
        &self,
        f: &mut RenderTarget<'_>,
        picker: &ModelPicker,
        area: Rect,
    ) {
        let block = Block::default()
            .borders(Borders::TOP)
            .border_style(Style::default().fg(Color::DarkGray));
        let inner = block.inner(area);
        f.render_widget(block, area);

        let width = inner.width as usize;
        let (active_model, _) = self.focused_model_and_effort();
        let items = self.filtered_models(&picker.query);
        let highlighted_model = items
            .get(picker.selected)
            .cloned()
            .or_else(|| {
                let q = picker.query.trim();
                (!q.is_empty()).then(|| q.to_owned())
            })
            .unwrap_or_else(|| active_model.clone());

        let mut out = Vec::with_capacity(inner.height as usize);
        let mut hdr = vec![
            Span::styled(
                "  Model Picker ",
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                if picker.query.is_empty() {
                    "type to filter or enter any custom model ID…".to_owned()
                } else {
                    format!("> {}", picker.query)
                },
                if picker.query.is_empty() {
                    Style::default().fg(Color::DarkGray)
                } else {
                    Style::default()
                        .fg(Color::White)
                        .add_modifier(Modifier::BOLD)
                },
            ),
        ];
        if let Some(ref msg) = picker.status_msg {
            hdr.push(Span::styled(
                format!(" · {msg}"),
                Style::default().fg(Color::Yellow),
            ));
        }
        out.push(Line::from(hdr));
        out.push(Line::default());

        // Reserve 4 bottom rows: blank separator + Reasoning Effort slider + Wire preview + footer hints.
        let visible = (inner.height as usize).saturating_sub(6).max(1);
        let scroll = picker.selected.saturating_sub(visible.saturating_sub(1));
        if items.is_empty() {
            let custom = picker.query.trim();
            out.push(Line::from(vec![
                Span::styled(
                    "> ",
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    if custom.is_empty() {
                        "(no recent models — type a model ID and press Enter)".to_owned()
                    } else {
                        format!("Use custom model `{custom}`")
                    },
                    Style::default().fg(Color::Yellow),
                ),
            ]));
        } else {
            let col_w = width.saturating_sub(28).max(18);
            for (idx, m) in items.iter().enumerate().skip(scroll).take(visible) {
                let is_sel = idx == picker.selected;
                let is_active = m == &active_model;
                let pfx = if is_sel { "> " } else { "  " };
                let badge = if is_active { "[ACTIVE]" } else { "        " };
                let row_effort = if is_sel {
                    picker.effort.clone()
                } else {
                    self.saved_effort_for_model(m)
                };
                let (ps, ms, bs) = if is_sel {
                    (
                        Style::default()
                            .fg(Color::Cyan)
                            .add_modifier(Modifier::BOLD),
                        Style::default()
                            .fg(Color::Cyan)
                            .add_modifier(Modifier::BOLD),
                        Style::default().fg(Color::Green),
                    )
                } else {
                    (
                        Style::default().fg(Color::DarkGray),
                        Style::default().fg(Color::White),
                        Style::default().fg(Color::Green),
                    )
                };
                let mut row_spans = vec![
                    Span::styled(pfx, ps),
                    Span::styled(pad_display_width(m, col_w), ms),
                    Span::raw("  "),
                    Span::styled(badge, bs),
                ];
                if let Some(eff) = row_effort {
                    row_spans.push(Span::styled(
                        format!("  effort:{eff}"),
                        Style::default().fg(Color::Magenta),
                    ));
                }
                out.push(Line::from(row_spans));
            }
        }

        while out.len() + 4 < inner.height as usize {
            out.push(Line::default());
        }
        out.push(Line::default());

        // Interactive model-aware Reasoning Effort selector strip (`←/→`) + transparent wire preview.
        let profile = self.resolve_effort_profile_for(&highlighted_model);
        let levels = profile.levels_with_current(picker.effort.as_deref());
        let cur_eff = picker.effort.as_deref().unwrap_or("default");
        let mut eff_spans = vec![Span::styled(
            "  Reasoning Effort: ",
            Style::default()
                .fg(Color::Gray)
                .add_modifier(Modifier::BOLD),
        )];
        for lvl in &levels {
            let is_cur = lvl.eq_ignore_ascii_case(cur_eff);
            if is_cur {
                eff_spans.push(Span::styled(
                    format!("[{lvl}]"),
                    Style::default()
                        .fg(Color::Magenta)
                        .add_modifier(Modifier::BOLD),
                ));
            } else {
                eff_spans.push(Span::styled(
                    lvl.clone(),
                    Style::default().fg(Color::DarkGray),
                ));
            }
            eff_spans.push(Span::raw("  "));
        }
        out.push(Line::from(eff_spans));

        let wire_str = profile.wire_preview(picker.effort.as_deref());
        let wire_budget = width
            .saturating_sub(profile.source_label.len() + 14)
            .max(18);
        out.push(Line::from(vec![
            Span::styled("  Wire: ", Style::default().fg(Color::DarkGray)),
            Span::styled(
                truncate_single_line(&wire_str, wire_budget),
                if picker.effort.is_some() {
                    Style::default().fg(Color::Gray)
                } else {
                    Style::default().fg(Color::DarkGray)
                },
            ),
            Span::styled(
                format!("  ({})", profile.source_label),
                Style::default().fg(Color::DarkGray),
            ),
        ]));

        out.push(Line::from(vec![
            Span::styled("  ↑/↓ ", Style::default().fg(Color::Cyan)),
            Span::styled("Model  ", Style::default().fg(Color::DarkGray)),
            Span::styled("←/→ ", Style::default().fg(Color::Magenta)),
            Span::styled("Effort  ", Style::default().fg(Color::DarkGray)),
            Span::styled("enter ", Style::default().fg(Color::Cyan)),
            Span::styled("Apply  ", Style::default().fg(Color::DarkGray)),
            Span::styled("f4/del ", Style::default().fg(Color::Cyan)),
            Span::styled("Remove  ", Style::default().fg(Color::DarkGray)),
            Span::styled("esc ", Style::default().fg(Color::Cyan)),
            Span::styled("Cancel", Style::default().fg(Color::DarkGray)),
        ]));
        f.render_widget(Paragraph::new(out), inner);
    }

    pub(super) fn render_completion_palette(
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

    pub(super) fn render_live_stream(&self, f: &mut RenderTarget<'_>, area: Rect) {
        let width = (area.width as usize).saturating_sub(1).max(30);
        let mut preview_lines: Vec<Line<'static>> = Vec::new();
        let mut status_lines: Vec<Line<'static>> = Vec::new();

        if let Some(st) = self.workers.get(&self.focused_worker) {
            // Render in-progress text/table AT THE VERY TOP so it seamlessly continues the
            // incrementally committed scrollback above with zero spinner gap between them.
            preview_lines = st.text_streamer.preview_lines(width);

            if !st.plan_steps.is_empty() {
                status_lines.extend(plan_checklist_lines(&st.plan_steps, width));
            }
            if !st.streaming_reasoning.trim().is_empty() && !st.text_streamer.has_visible_content()
            {
                let tail = tail_preview(&st.streaming_reasoning, width.saturating_sub(14));
                status_lines.push(Line::from(vec![
                    Span::styled("💭 ", Style::default().fg(Color::DarkGray)),
                    Span::styled(
                        tail,
                        Style::default()
                            .fg(Color::DarkGray)
                            .add_modifier(Modifier::ITALIC),
                    ),
                ]));
            }
            if !st.active_tools.is_empty() {
                for t in st.active_tools.iter().take(3) {
                    let secs = t.started_at.elapsed().as_secs();
                    status_lines.push(Line::from(vec![
                        Span::styled("⚙ ", Style::default().fg(Color::Yellow)),
                        Span::styled(
                            format!(
                                "{} $ {}",
                                t.name,
                                truncate_single_line(&t.cmd, width.saturating_sub(18))
                            ),
                            Style::default().fg(Color::Yellow),
                        ),
                        Span::styled(format!(" ({secs}s)"), Style::default().fg(Color::DarkGray)),
                    ]));
                }
            }
            if !st.queued_steering.is_empty() {
                let n = st.queued_steering.len();
                let preview = st.queued_steering.last().map(String::as_str).unwrap_or("");
                status_lines.push(Line::from(vec![
                    Span::styled(
                        format!("  ↳ queued steering ({n}): "),
                        Style::default().fg(Color::Magenta),
                    ),
                    Span::styled(
                        truncate_single_line(preview, width.saturating_sub(42)),
                        Style::default().fg(Color::White),
                    ),
                    Span::styled(
                        " (injects at next turn)",
                        Style::default().fg(Color::DarkGray),
                    ),
                ]));
            }
            if st.handle.busy {
                let spin = SPINNER_FRAMES[self.spinner_idx];
                let elapsed = st.busy_since.map(|t| t.elapsed().as_secs()).unwrap_or(0);
                let phase = if !st.active_tools.is_empty() {
                    "running tool…"
                } else {
                    "streaming…"
                };
                status_lines.push(Line::from(vec![
                    Span::styled(format!("{spin} "), Style::default().fg(Color::Cyan)),
                    Span::styled(
                        format!("[{}] {phase} ", self.focused_worker),
                        Style::default().fg(Color::Cyan),
                    ),
                    Span::styled(
                        format!("({elapsed}s · esc to interrupt)"),
                        Style::default().fg(Color::DarkGray),
                    ),
                ]));
            }
        }
        if let Some(ref bang_cmd) = self.running_bang_cmd {
            let spin = SPINNER_FRAMES[self.spinner_idx];
            status_lines.push(Line::from(vec![
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

        let total_avail = area.height as usize;
        let status_reserve = status_lines.len().min(total_avail);
        let preview_avail = total_avail.saturating_sub(status_reserve);
        let preview_start = preview_lines.len().saturating_sub(preview_avail);

        let mut lines: Vec<Line<'static>> = preview_lines.into_iter().skip(preview_start).collect();
        lines.extend(status_lines.into_iter().take(status_reserve));

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

    pub(super) fn render_approval_card(
        &self,
        f: &mut RenderTarget<'_>,
        active: &ActiveApproval,
        area: Rect,
    ) {
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

    pub(super) fn render_tree_overlay(
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

    pub(super) fn render_workers_overlay(
        &self,
        f: &mut RenderTarget<'_>,
        selected: usize,
        area: Rect,
    ) {
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

    pub(super) fn render_sessions_overlay(
        &self,
        f: &mut RenderTarget<'_>,
        picker: &ResumePicker,
        area: Rect,
    ) {
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

    pub(super) fn render_input_box(&self, f: &mut RenderTarget<'_>, area: Rect) {
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

    pub(super) fn build_status_vars(&self) -> StatusVars {
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

        let (
            busy,
            model,
            effort,
            provider,
            leaf,
            ctx_pct_num,
            ctx_tokens,
            ctx_max,
            in_tok,
            out_tok,
            cache_pct_num,
            cached_tokens,
            reasoning_tokens,
            elapsed_secs,
            holds,
        ) = match st {
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
                let effort = w.handle.spec.reasoning_effort.clone();
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
                let in_raw = w.handle.session_usage.input_tokens;
                let cache_raw = w.handle.session_usage.cached();
                let reason_raw = w.handle.session_usage.reasoning();
                let cache_pct = if in_raw > 0 && cache_raw > 0 {
                    Some(((cache_raw * 100) / in_raw).min(100))
                } else {
                    None
                };
                let cached_s = if cache_raw > 0 {
                    fmt_k(cache_raw)
                } else {
                    String::new()
                };
                let reason_s = if reason_raw > 0 {
                    fmt_k(reason_raw)
                } else {
                    String::new()
                };
                let elapsed = if w.handle.busy {
                    w.busy_since.map(|t| t.elapsed().as_secs())
                } else {
                    None
                };
                (
                    w.handle.busy || self.running_bang_cmd.is_some(),
                    model,
                    effort,
                    provider,
                    w.handle.current_leaf,
                    pct,
                    ct,
                    cm,
                    fmt_k(in_raw),
                    fmt_k(w.handle.session_usage.output_tokens),
                    cache_pct,
                    cached_s,
                    reason_s,
                    elapsed,
                    w.handle.holds.len(),
                )
            }
            None => {
                let draft_effort = self.meta.reasoning_effort.clone().or_else(|| {
                    self.agent_config
                        .effort_for_model(&self.draft_model)
                        .map(ToOwned::to_owned)
                });
                (
                    self.running_bang_cmd.is_some(),
                    self.draft_model.clone(),
                    draft_effort,
                    self.draft_provider.clone(),
                    self.meta.active_leaf,
                    Some(0),
                    "0".into(),
                    fmt_k(u64::from(self.draft_max_tokens)),
                    "0".into(),
                    "0".into(),
                    None,
                    String::new(),
                    String::new(),
                    None,
                    0,
                )
            }
        };

        StatusVars {
            busy,
            spinner: SPINNER_FRAMES[self.spinner_idx],
            mode: self.policy,
            bang_mode: self.agent_config.bang_mode,
            worker: self.focused_worker.clone(),
            agent,
            model,
            effort,
            provider,
            cwd: self.store.cwd.display().to_string(),
            dir,
            git_branch: self.git_branch.clone(),
            ctx_pct_num,
            ctx_tokens,
            ctx_max,
            in_tokens: in_tok,
            out_tokens: out_tok,
            cache_pct_num,
            cached_tokens,
            reasoning_tokens,
            elapsed_secs,
            leaf,
            holds,
            workers: self.workers.len(),
            session_id: self.meta.id.clone(),
        }
    }

    pub(super) fn render_status_bar(&self, f: &mut RenderTarget<'_>, area: Rect) {
        let vars = self.build_status_vars();
        let bar = render_status_bar(&self.agent_config.status_bar, &vars, area.width as usize);
        f.render_widget(Paragraph::new(bar), area);
    }
}

/// Build the compact ephemeral live checklist rows rendered above the input box (`0` rows when
/// empty, `1` header + up to `5` windowed steps focused around the active `in_progress` step).
fn plan_checklist_lines(steps: &[PlanStep], width: usize) -> Vec<Line<'static>> {
    if steps.is_empty() {
        return Vec::new();
    }
    let total = steps.len();
    let done = steps
        .iter()
        .filter(|s| s.status == PlanStepStatus::Done)
        .count();
    let all_finished = steps
        .iter()
        .all(|s| matches!(s.status, PlanStepStatus::Done | PlanStepStatus::Skipped));

    let max_visible = 5usize;
    let start = if total <= max_visible {
        0
    } else {
        let focus = steps
            .iter()
            .position(|s| s.status == PlanStepStatus::InProgress)
            .or_else(|| {
                steps.iter().position(|s| {
                    matches!(s.status, PlanStepStatus::Pending | PlanStepStatus::Failed)
                })
            })
            .unwrap_or(total.saturating_sub(1));
        focus.saturating_sub(1).min(total - max_visible)
    };
    let visible = &steps[start..(start + max_visible).min(total)];

    let mut out = Vec::with_capacity(1 + visible.len());
    let mut header_spans = if all_finished {
        vec![
            Span::styled("  ✓ ", Style::default().fg(Color::Green)),
            Span::styled(
                format!("Plan ({done}/{total} done)"),
                Style::default()
                    .fg(Color::Green)
                    .add_modifier(Modifier::BOLD),
            ),
        ]
    } else {
        vec![
            Span::styled("  ☰ ", Style::default().fg(Color::Cyan)),
            Span::styled(
                format!("Plan ({done}/{total})"),
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
        ]
    };
    if total > max_visible {
        let end = start + visible.len();
        header_spans.push(Span::styled(
            format!(" · showing {}–{} of {total}", start + 1, end),
            Style::default().fg(Color::DarkGray),
        ));
    }
    out.push(Line::from(header_spans));

    let title_budget = width.saturating_sub(8).max(16);
    for step in visible {
        let (icon, icon_style, title_style) = match step.status {
            PlanStepStatus::Done => (
                "✓ ",
                Style::default().fg(Color::Green),
                Style::default().fg(Color::DarkGray),
            ),
            PlanStepStatus::InProgress => (
                "▸ ",
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
                Style::default()
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD),
            ),
            PlanStepStatus::Pending => (
                "○ ",
                Style::default().fg(Color::DarkGray),
                Style::default().fg(Color::Gray),
            ),
            PlanStepStatus::Skipped => (
                "− ",
                Style::default().fg(Color::DarkGray),
                Style::default().fg(Color::DarkGray),
            ),
            PlanStepStatus::Failed => (
                "✗ ",
                Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
                Style::default().fg(Color::Red),
            ),
        };
        out.push(Line::from(vec![
            Span::raw("    "),
            Span::styled(icon, icon_style),
            Span::styled(truncate_single_line(&step.title, title_budget), title_style),
        ]));
    }
    out
}
