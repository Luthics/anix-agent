//! Lightweight, CJK-aware Markdown -> `ratatui::text::Line<'static>` renderer for inline
//! scrollback rendering and streaming previews.
//!
//! Supports headings (with inline styles), nested `**\`code\`**` / `**bold**` / `*italic*` /
//! `` `code` ``, fenced code blocks, blockquotes, bullet & numbered lists, horizontal rules,
//! and aligned Unicode pipe tables with CJK-aware column sizing & wrapping.

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

/// Render a Markdown document into styled, width-wrapped terminal lines.
pub fn render_markdown(md: &str, width: usize) -> Vec<Line<'static>> {
    let width = width.max(20);
    let raw_lines: Vec<&str> = md.lines().collect();
    let mut out = Vec::new();
    let mut in_code_block = false;
    let mut code_is_diff = false;
    let mut code_lang_empty = false;
    let mut idx = 0usize;

    while idx < raw_lines.len() {
        let raw_line = raw_lines[idx];
        let trimmed = raw_line.trim_start();

        if let Some(fence_rest) = trimmed.strip_prefix("```") {
            if !in_code_block {
                in_code_block = true;
                code_lang_empty = fence_rest.trim().is_empty();
                code_is_diff = is_diff_fence_lang(fence_rest);
                out.push(render_code_fence_open(fence_rest));
            } else {
                in_code_block = false;
                code_is_diff = false;
                code_lang_empty = false;
                out.push(render_code_fence_close());
            }
            idx += 1;
            continue;
        }

        if in_code_block {
            if !code_is_diff && code_lang_empty && is_strong_diff_marker_line(raw_line) {
                code_is_diff = true;
            }
            if code_is_diff {
                out.extend(render_diff_line("│ ", "│ ", raw_line, width));
            } else {
                out.extend(render_code_block_line(raw_line, width));
            }
            idx += 1;
            continue;
        }

        if trimmed.is_empty() {
            out.push(Line::default());
            idx += 1;
            continue;
        }

        // Markdown pipe table (requires at least header + separator row)
        if is_table_row(trimmed)
            && idx + 1 < raw_lines.len()
            && is_table_separator(raw_lines[idx + 1].trim())
        {
            let mut table_rows: Vec<&str> = vec![trimmed];
            let mut j = idx + 2;
            while j < raw_lines.len() && is_table_row(raw_lines[j].trim()) {
                table_rows.push(raw_lines[j].trim());
                j += 1;
            }
            out.extend(render_table(&table_rows, width));
            idx = j;
            continue;
        }

        out.extend(render_single_markdown_line(raw_line, width));
        idx += 1;
    }

    if in_code_block {
        out.push(render_code_fence_close());
    }

    if out.is_empty() {
        out.push(Line::default());
    }
    out
}

/// Incremental Markdown streamer that commits completed lines/blocks (headings, paragraphs, list
/// items, code block lines, and finished pipe tables) immediately to terminal scrollback while
/// rendering only the currently-in-progress tail live without 5-line truncation.
#[derive(Debug, Default, Clone)]
pub struct MarkdownStreamer {
    pending_line: String,
    /// Completed pipe-table lines `[header, sep, row1, ...]` waiting for table end.
    pending_table: Vec<String>,
    in_code_block: bool,
    code_is_diff: bool,
    code_lang_empty: bool,
    emitted_any: bool,
    pending_blanks: usize,
    first_nonempty: Option<String>,
}

impl MarkdownStreamer {
    pub fn new() -> Self {
        Self::default()
    }

    /// Append a newly streamed text delta from the model.
    pub fn push_delta(&mut self, delta: &str) {
        if self.first_nonempty.is_none() {
            for line in delta.lines() {
                let t = line.trim();
                if !t.is_empty() {
                    self.first_nonempty = Some(t.to_owned());
                    break;
                }
            }
        }
        self.pending_line.push_str(delta);
    }

    /// Whether any visible (non-whitespace) assistant text has arrived in this turn.
    pub fn has_visible_content(&self) -> bool {
        self.emitted_any || !self.pending_table.is_empty() || !self.pending_line.trim().is_empty()
    }

    /// Short first-line preview used for background worker completion notices.
    pub fn first_nonempty_preview(&self) -> String {
        if let Some(ref s) = self.first_nonempty {
            return s.clone();
        }
        self.pending_line
            .lines()
            .map(str::trim)
            .find(|l| !l.is_empty())
            .unwrap_or("(no text)")
            .to_owned()
    }

    /// Drain all completed (`\n`-terminated) lines/blocks ready to commit to scrollback now.
    pub fn take_ready_lines(&mut self, width: usize) -> Vec<Line<'static>> {
        let width = width.max(20);
        let mut committed = Vec::new();
        while let Some(nl_pos) = self.pending_line.find('\n') {
            let mut line: String = self.pending_line.drain(..=nl_pos).collect();
            if line.ends_with('\n') {
                line.pop();
            }
            if line.ends_with('\r') {
                line.pop();
            }
            self.process_completed_line(&line, width, &mut committed);
        }
        committed
    }

    /// Render the currently-in-progress uncommitted tail (incomplete line or growing pipe table).
    pub fn preview_lines(&self, width: usize) -> Vec<Line<'static>> {
        let width = width.max(20);
        let mut out = Vec::new();
        let cur = self.pending_line.trim_end_matches('\r');

        if self.in_code_block {
            if !cur.is_empty() && !cur.trim_start().starts_with("```") {
                if self.code_is_diff || (self.code_lang_empty && is_strong_diff_marker_line(cur)) {
                    out.extend(render_diff_line("│ ", "│ ", cur, width));
                } else {
                    out.extend(render_code_block_line(cur, width));
                }
            }
            return out;
        }

        if !self.pending_table.is_empty() {
            if self.emitted_any && self.pending_blanks > 0 {
                out.push(Line::default());
            }
            if self.pending_table.len() >= 2 {
                let mut rows: Vec<&str> = Vec::with_capacity(self.pending_table.len() - 1);
                rows.push(self.pending_table[0].trim());
                for r in &self.pending_table[2..] {
                    rows.push(r.trim());
                }
                out.extend(render_table(&rows, width));
                if !cur.trim().is_empty() {
                    out.extend(render_single_markdown_line(cur, width));
                }
            } else {
                out.extend(render_single_markdown_line(&self.pending_table[0], width));
                let cur_trimmed = cur.trim();
                let looks_like_sep_prefix = cur_trimmed.starts_with('|')
                    && cur_trimmed
                        .chars()
                        .all(|c| matches!(c, '|' | '-' | ':' | ' '));
                if !cur_trimmed.is_empty() && !looks_like_sep_prefix {
                    out.extend(render_single_markdown_line(cur, width));
                }
            }
            return out;
        }

        if !cur.is_empty() && (self.emitted_any || !cur.trim().is_empty()) {
            if self.emitted_any && self.pending_blanks > 0 {
                out.push(Line::default());
            }
            let trimmed = cur.trim_start();
            if let Some(fence_rest) = trimmed.strip_prefix("```") {
                out.push(render_code_fence_open(fence_rest));
            } else {
                out.extend(render_single_markdown_line(cur, width));
            }
        }
        out
    }

    /// Flush all remaining buffered text/tables/fences at turn/tool boundary and reset state.
    /// Returns `(remaining_lines_to_commit, emitted_any_text_in_turn)`.
    pub fn finish(&mut self, width: usize) -> (Vec<Line<'static>>, bool) {
        let width = width.max(20);
        let mut out = self.take_ready_lines(width);

        if !self.pending_line.is_empty() {
            let last = std::mem::take(&mut self.pending_line);
            let clean = last.trim_end_matches('\r');
            self.process_completed_line(clean, width, &mut out);
        }

        if !self.pending_table.is_empty() {
            self.flush_pending_table(width, &mut out);
        }

        if self.in_code_block {
            out.push(render_code_fence_close());
        }

        let had_any = self.emitted_any || !out.is_empty();
        self.reset();
        (out, had_any)
    }

    pub fn reset(&mut self) {
        self.pending_line.clear();
        self.pending_table.clear();
        self.in_code_block = false;
        self.code_is_diff = false;
        self.code_lang_empty = false;
        self.emitted_any = false;
        self.pending_blanks = 0;
        self.first_nonempty = None;
    }

    fn flush_blanks(&mut self, out: &mut Vec<Line<'static>>) {
        for _ in 0..self.pending_blanks {
            out.push(Line::default());
        }
        self.pending_blanks = 0;
    }

    fn flush_pending_table(&mut self, width: usize, out: &mut Vec<Line<'static>>) {
        let table = std::mem::take(&mut self.pending_table);
        if table.is_empty() {
            return;
        }
        self.flush_blanks(out);
        self.emitted_any = true;
        if table.len() >= 2 {
            let mut rows: Vec<&str> = Vec::with_capacity(table.len() - 1);
            rows.push(table[0].trim());
            for r in &table[2..] {
                rows.push(r.trim());
            }
            out.extend(render_table(&rows, width));
        } else {
            out.extend(render_single_markdown_line(&table[0], width));
        }
    }

    fn process_completed_line(
        &mut self,
        raw_line: &str,
        width: usize,
        out: &mut Vec<Line<'static>>,
    ) {
        let trimmed = raw_line.trim_start();

        if self.in_code_block {
            if trimmed.starts_with("```") {
                self.in_code_block = false;
                self.code_is_diff = false;
                self.code_lang_empty = false;
                out.push(render_code_fence_close());
            } else {
                if !self.code_is_diff
                    && self.code_lang_empty
                    && is_strong_diff_marker_line(raw_line)
                {
                    self.code_is_diff = true;
                }
                if self.code_is_diff {
                    out.extend(render_diff_line("│ ", "│ ", raw_line, width));
                } else {
                    out.extend(render_code_block_line(raw_line, width));
                }
            }
            return;
        }

        if !self.pending_table.is_empty() {
            if self.pending_table.len() == 1 {
                if is_table_separator(trimmed.trim_end()) {
                    self.pending_table.push(raw_line.to_owned());
                    return;
                }
                // Not actually a table: flush the single buffered line first, then process `raw_line`.
                self.flush_pending_table(width, out);
            } else if is_table_row(trimmed.trim_end()) {
                self.pending_table.push(raw_line.to_owned());
                return;
            } else {
                // Table ended: commit complete aligned table, then process `raw_line`.
                self.flush_pending_table(width, out);
            }
        }

        if trimmed.is_empty() {
            if self.emitted_any {
                self.pending_blanks += 1;
            }
            return;
        }

        if let Some(fence_rest) = trimmed.strip_prefix("```") {
            self.flush_blanks(out);
            self.emitted_any = true;
            self.in_code_block = true;
            self.code_lang_empty = fence_rest.trim().is_empty();
            self.code_is_diff = is_diff_fence_lang(fence_rest);
            out.push(render_code_fence_open(fence_rest));
            return;
        }

        if is_table_row(trimmed.trim_end()) {
            self.pending_table.push(raw_line.to_owned());
            return;
        }

        self.flush_blanks(out);
        self.emitted_any = true;
        out.extend(render_single_markdown_line(raw_line, width));
    }
}

fn render_code_fence_open(fence_rest: &str) -> Line<'static> {
    let code_lang = fence_rest.trim();
    let header = if code_lang.is_empty() {
        "╭─ code".to_owned()
    } else {
        format!("╭─ {code_lang}")
    };
    Line::from(Span::styled(header, Style::default().fg(Color::DarkGray)))
}

fn render_code_fence_close() -> Line<'static> {
    Line::from(Span::styled("╰─", Style::default().fg(Color::DarkGray)))
}

fn render_code_block_line(raw_line: &str, width: usize) -> Vec<Line<'static>> {
    let code_width = width.saturating_sub(2).max(12);
    wrap_plain_str(raw_line, code_width)
        .into_iter()
        .map(|chunk| {
            Line::from(vec![
                Span::styled("│ ", Style::default().fg(Color::DarkGray)),
                Span::styled(chunk, Style::default().fg(Color::Yellow)),
            ])
        })
        .collect()
}

fn is_diff_fence_lang(fence_rest: &str) -> bool {
    matches!(
        fence_rest
            .split_whitespace()
            .next()
            .unwrap_or("")
            .to_ascii_lowercase()
            .as_str(),
        "diff" | "patch" | "udiff"
    )
}

fn is_strong_diff_marker_line(raw_line: &str) -> bool {
    raw_line.starts_with("diff --git ")
        || (raw_line.starts_with("@@ -") && raw_line.contains(" @@"))
}

/// Heuristic detecting unified diff output (`git diff`, `diff -u`, patch hunks) while avoiding
/// false positives on plain Markdown lists or prose.
pub fn looks_like_unified_diff(text: &str) -> bool {
    let mut saw_minus_hdr = false;
    let mut saw_plus_hdr = false;
    for line in text.lines().take(200) {
        if is_strong_diff_marker_line(line) {
            return true;
        }
        if line.starts_with("--- ") {
            saw_minus_hdr = true;
        } else if line.starts_with("+++ ") {
            saw_plus_hdr = true;
        }
        if saw_minus_hdr && saw_plus_hdr {
            return true;
        }
    }
    false
}

/// Color style for a single line in a unified diff.
pub fn diff_line_style(raw_line: &str) -> Style {
    if raw_line.starts_with("diff --git ")
        || raw_line.starts_with("index ")
        || raw_line.starts_with("new file mode")
        || raw_line.starts_with("deleted file mode")
        || raw_line.starts_with("rename from ")
        || raw_line.starts_with("rename to ")
    {
        Style::default()
            .fg(Color::White)
            .add_modifier(Modifier::BOLD)
    } else if raw_line.starts_with("@@") {
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD)
    } else if raw_line.starts_with("+++") {
        Style::default()
            .fg(Color::Green)
            .add_modifier(Modifier::BOLD)
    } else if raw_line.starts_with("---") {
        Style::default().fg(Color::Red).add_modifier(Modifier::BOLD)
    } else if raw_line.starts_with('+') {
        Style::default().fg(Color::Green)
    } else if raw_line.starts_with('-') {
        Style::default().fg(Color::Red)
    } else {
        Style::default().fg(Color::Gray)
    }
}

/// Render a single unified-diff line with width-aware wrapping and syntax highlighting.
pub fn render_diff_line(
    first_prefix: &str,
    cont_prefix: &str,
    raw_line: &str,
    width: usize,
) -> Vec<Line<'static>> {
    let pfx_w = first_prefix.width().max(cont_prefix.width());
    let body_w = width.saturating_sub(pfx_w).max(12);
    let style = diff_line_style(raw_line);
    let pfx_style = Style::default().fg(Color::DarkGray);
    wrap_plain_str(raw_line, body_w)
        .into_iter()
        .enumerate()
        .map(|(idx, chunk)| {
            let pfx = if idx == 0 { first_prefix } else { cont_prefix };
            Line::from(vec![
                Span::styled(pfx.to_owned(), pfx_style),
                Span::styled(chunk, style),
            ])
        })
        .collect()
}

/// Extract the body of the last fenced ```` ```...``` ```` code block in a Markdown reply
/// (powers `/copy code`).
pub fn extract_last_code_block(md: &str) -> Option<String> {
    let mut in_block = false;
    let mut cur: Vec<&str> = Vec::new();
    let mut last: Option<String> = None;

    for raw_line in md.lines() {
        let line = raw_line.trim_end_matches('\r');
        let trimmed = line.trim_start();
        if trimmed.starts_with("```") {
            if !in_block {
                in_block = true;
                cur.clear();
            } else {
                in_block = false;
                let body = cur.join("\n");
                if !body.trim().is_empty() {
                    last = Some(body);
                }
                cur.clear();
            }
        } else if in_block {
            cur.push(line);
        }
    }

    if in_block && !cur.is_empty() {
        let body = cur.join("\n");
        if !body.trim().is_empty() {
            last = Some(body);
        }
    }

    last
}

fn render_single_markdown_line(raw_line: &str, width: usize) -> Vec<Line<'static>> {
    let trimmed = raw_line.trim_start();
    let mut out = Vec::new();

    if trimmed.is_empty() {
        out.push(Line::default());
        return out;
    }

    // Horizontal rule
    if is_horizontal_rule(trimmed) {
        let rule_w = width.min(60);
        out.push(Line::from(Span::styled(
            "─".repeat(rule_w),
            Style::default().fg(Color::DarkGray),
        )));
        return out;
    }

    // Headings (with inline Markdown support, e.g. `## Title **bold** \`code\``)
    if let Some((level, rest)) = parse_heading(trimmed) {
        let style = match level {
            1 => Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD | Modifier::UNDERLINED),
            2 => Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
            _ => Style::default()
                .fg(Color::Blue)
                .add_modifier(Modifier::BOLD),
        };
        let spans = parse_inline_spans(rest, style);
        for line_spans in wrap_styled_spans(&spans, width) {
            out.push(Line::from(line_spans));
        }
        return out;
    }

    // Blockquotes
    if let Some(quote) = trimmed.strip_prefix("> ") {
        let base = Style::default()
            .fg(Color::Gray)
            .add_modifier(Modifier::ITALIC);
        let spans = parse_inline_spans(quote, base);
        let wrapped = wrap_styled_spans(&spans, width.saturating_sub(2).max(12));
        for mut line_spans in wrapped {
            let mut full = vec![Span::styled("▎ ", Style::default().fg(Color::DarkGray))];
            full.append(&mut line_spans);
            out.push(Line::from(full));
        }
        return out;
    }

    // Bullet / numbered lists
    if let Some((indent_prefix, bullet, rest)) = parse_list_item(raw_line) {
        let prefix_str = format!("{indent_prefix}{bullet} ");
        let cont_prefix = " ".repeat(prefix_str.width());
        let body_w = width.saturating_sub(prefix_str.width()).max(12);
        let spans = parse_inline_spans(rest, Style::default());
        let wrapped = wrap_styled_spans(&spans, body_w);
        for (line_idx, mut line_spans) in wrapped.into_iter().enumerate() {
            let mut full = Vec::with_capacity(line_spans.len() + 1);
            if line_idx == 0 {
                full.push(Span::styled(
                    prefix_str.clone(),
                    Style::default().fg(Color::Cyan),
                ));
            } else {
                full.push(Span::raw(cont_prefix.clone()));
            }
            full.append(&mut line_spans);
            out.push(Line::from(full));
        }
        return out;
    }

    // Regular paragraph line with inline formatting
    let spans = parse_inline_spans(raw_line, Style::default());
    for line_spans in wrap_styled_spans(&spans, width) {
        out.push(Line::from(line_spans));
    }
    out
}

/// Wrap plain text into styled `Line<'static>` rows with an optional prefix on every line.
pub fn wrap_prefixed_lines(
    text: &str,
    prefix: &str,
    prefix_style: Style,
    text_style: Style,
    width: usize,
) -> Vec<Line<'static>> {
    let body_w = width.saturating_sub(prefix.width()).max(12);
    let mut lines = Vec::new();
    for raw in text.lines() {
        for chunk in wrap_plain_str(raw, body_w) {
            lines.push(Line::from(vec![
                Span::styled(prefix.to_owned(), prefix_style),
                Span::styled(chunk, text_style),
            ]));
        }
    }
    if lines.is_empty() {
        lines.push(Line::from(Span::styled(prefix.to_owned(), prefix_style)));
    }
    lines
}

/// Wrap plain text into display-width-bounded chunks (handles CJK double-width chars cleanly).
pub fn wrap_plain_str(s: &str, max_width: usize) -> Vec<String> {
    let max_width = max_width.max(4);
    if s.is_empty() {
        return vec![String::new()];
    }
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut cur_w = 0usize;

    for ch in s.chars() {
        let cw = ch.width().unwrap_or(0);
        if cur_w + cw > max_width && !cur.is_empty() {
            out.push(std::mem::take(&mut cur));
            cur_w = 0;
        }
        cur.push(ch);
        cur_w += cw;
    }
    if !cur.is_empty() || out.is_empty() {
        out.push(cur);
    }
    out
}

fn is_table_row(trimmed: &str) -> bool {
    trimmed.starts_with('|') && trimmed.ends_with('|') && trimmed.matches('|').count() >= 2
}

fn is_table_separator(trimmed: &str) -> bool {
    if !is_table_row(trimmed) {
        return false;
    }
    let cells = split_table_cells(trimmed);
    !cells.is_empty()
        && cells.iter().all(|c| {
            let s = c.trim();
            !s.is_empty() && s.chars().all(|ch| matches!(ch, '-' | ':')) && s.contains('-')
        })
}

fn split_table_cells(row: &str) -> Vec<String> {
    let inner = row
        .trim()
        .strip_prefix('|')
        .unwrap_or(row.trim())
        .strip_suffix('|')
        .unwrap_or(row.trim());
    let mut cells = Vec::new();
    let mut cur = String::new();
    let mut in_code = false;
    let mut chars = inner.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            '\\' if chars.peek() == Some(&'|') => {
                chars.next();
                cur.push('|');
            }
            '`' => {
                in_code = !in_code;
                cur.push(ch);
            }
            '|' if !in_code => {
                cells.push(cur.trim().to_owned());
                cur.clear();
            }
            _ => cur.push(ch),
        }
    }
    cells.push(cur.trim().to_owned());
    cells
}

fn render_table(rows: &[&str], max_width: usize) -> Vec<Line<'static>> {
    let parsed: Vec<Vec<String>> = rows.iter().map(|r| split_table_cells(r)).collect();
    let col_count = parsed.iter().map(Vec::len).max().unwrap_or(0);
    if col_count == 0 {
        return Vec::new();
    }

    // Measure natural display width of each column using inline-stripped text.
    let mut col_widths = vec![3usize; col_count];
    for row in &parsed {
        for (c, cell) in row.iter().enumerate() {
            let plain_w: usize = parse_inline_spans(cell, Style::default())
                .iter()
                .map(|(t, _)| t.width())
                .sum();
            col_widths[c] = col_widths[c].max(plain_w);
        }
    }

    // Overhead: `│ ` + ` │ ` * (cols - 1) + ` │` = 3 * cols + 1
    let overhead = col_count * 3 + 1;
    let budget = max_width.saturating_sub(overhead).max(col_count * 4);
    let total_natural: usize = col_widths.iter().sum();
    if total_natural > budget {
        // Proportionally shrink wider columns while preserving a minimum readable width of 4.
        let mut excess = total_natural - budget;
        while excess > 0 {
            let Some((widest_idx, &widest)) = col_widths.iter().enumerate().max_by_key(|&(_, w)| w)
            else {
                break;
            };
            if widest <= 4 {
                break;
            }
            col_widths[widest_idx] -= 1;
            excess -= 1;
        }
    }

    let border_style = Style::default().fg(Color::DarkGray);
    let make_border = |left: &str, mid: &str, right: &str| -> Line<'static> {
        let mut s = String::from(left);
        for (idx, &w) in col_widths.iter().enumerate() {
            s.push_str(&"─".repeat(w + 2));
            if idx + 1 < col_widths.len() {
                s.push_str(mid);
            }
        }
        s.push_str(right);
        Line::from(Span::styled(s, border_style))
    };

    let mut out = Vec::new();
    out.push(make_border("┌", "┬", "┐"));

    for (row_idx, row) in parsed.iter().enumerate() {
        let base_style = if row_idx == 0 {
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default()
        };

        // Wrap each cell's styled spans to its allocated column width.
        let mut wrapped_cols: Vec<Vec<Vec<Span<'static>>>> = Vec::with_capacity(col_count);
        let mut max_sub_rows = 1usize;
        for (c, &col_w) in col_widths.iter().enumerate() {
            let cell_raw = row.get(c).map(String::as_str).unwrap_or("");
            let spans = parse_inline_spans(cell_raw, base_style);
            let lines = wrap_styled_spans(&spans, col_w);
            max_sub_rows = max_sub_rows.max(lines.len());
            wrapped_cols.push(lines);
        }

        for sub in 0..max_sub_rows {
            let mut line_spans: Vec<Span<'static>> = vec![Span::styled("│ ", border_style)];
            for (c, &col_w) in col_widths.iter().enumerate() {
                let mut used_w = 0usize;
                if let Some(cell_line) = wrapped_cols[c].get(sub) {
                    for sp in cell_line {
                        used_w += sp.content.width();
                        line_spans.push(sp.clone());
                    }
                }
                if used_w < col_w {
                    line_spans.push(Span::raw(" ".repeat(col_w - used_w)));
                }
                if c + 1 < col_count {
                    line_spans.push(Span::styled(" │ ", border_style));
                } else {
                    line_spans.push(Span::styled(" │", border_style));
                }
            }
            out.push(Line::from(line_spans));
        }

        if row_idx == 0 {
            out.push(make_border("├", "┼", "┤"));
        }
    }

    out.push(make_border("└", "┴", "┘"));
    out
}

fn is_horizontal_rule(trimmed: &str) -> bool {
    let stripped: String = trimmed.chars().filter(|c| !c.is_whitespace()).collect();
    stripped.len() >= 3
        && (stripped.chars().all(|c| c == '-')
            || stripped.chars().all(|c| c == '*')
            || stripped.chars().all(|c| c == '_'))
}

fn parse_heading(trimmed: &str) -> Option<(usize, &str)> {
    let hashes = trimmed.bytes().take_while(|&b| b == b'#').count();
    if (1..=6).contains(&hashes) {
        let rest = trimmed[hashes..].strip_prefix(' ')?;
        Some((hashes, rest.trim()))
    } else {
        None
    }
}

fn parse_list_item(raw: &str) -> Option<(&str, &str, &str)> {
    let indent_len = raw.len() - raw.trim_start_matches(' ').len();
    let indent = &raw[..indent_len];
    let rest = &raw[indent_len..];
    for marker in ["- ", "* ", "+ "] {
        if let Some(body) = rest.strip_prefix(marker) {
            return Some((indent, "•", body));
        }
    }
    let digits = rest.bytes().take_while(|b| b.is_ascii_digit()).count();
    if digits > 0 && rest[digits..].starts_with(". ") {
        let marker = &rest[..=digits];
        let body = &rest[digits + 2..];
        return Some((indent, marker, body));
    }
    None
}

fn parse_inline_spans(text: &str, base: Style) -> Vec<(String, Style)> {
    let mut out = Vec::new();
    let mut buf = String::new();
    let chars: Vec<char> = text.chars().collect();
    let mut i = 0usize;

    while i < chars.len() {
        // Inline code: `...`
        if chars[i] == '`'
            && let Some(end) = chars[i + 1..].iter().position(|&c| c == '`')
        {
            if !buf.is_empty() {
                out.push((std::mem::take(&mut buf), base));
            }
            let code: String = chars[i + 1..i + 1 + end].iter().collect();
            out.push((code, base.fg(Color::Yellow).add_modifier(Modifier::BOLD)));
            i += end + 2;
            continue;
        }

        // Bold: **...** (recursively parses nested `code` inside bold!)
        if i + 1 < chars.len()
            && chars[i] == '*'
            && chars[i + 1] == '*'
            && let Some(rel_end) = find_double_star(&chars[i + 2..])
        {
            if !buf.is_empty() {
                out.push((std::mem::take(&mut buf), base));
            }
            let bold_text: String = chars[i + 2..i + 2 + rel_end].iter().collect();
            let nested = parse_inline_spans(&bold_text, base.add_modifier(Modifier::BOLD));
            out.extend(nested);
            i += rel_end + 4;
            continue;
        }

        // Italic: *...* (avoid matching globs like `*.agent`)
        if chars[i] == '*'
            && i + 1 < chars.len()
            && !chars[i + 1].is_whitespace()
            && chars[i + 1] != '*'
            && (i == 0 || !chars[i - 1].is_alphanumeric())
            && let Some(rel_end) = find_single_star(&chars[i + 1..])
        {
            let after_idx = i + 1 + rel_end + 1;
            let boundary_ok = after_idx >= chars.len() || !chars[after_idx].is_alphanumeric();
            if boundary_ok && !chars[i + rel_end].is_whitespace() {
                if !buf.is_empty() {
                    out.push((std::mem::take(&mut buf), base));
                }
                let italic_text: String = chars[i + 1..i + 1 + rel_end].iter().collect();
                out.push((italic_text, base.add_modifier(Modifier::ITALIC)));
                i = after_idx;
                continue;
            }
        }

        buf.push(chars[i]);
        i += 1;
    }

    if !buf.is_empty() || out.is_empty() {
        out.push((buf, base));
    }
    out
}

fn find_double_star(slice: &[char]) -> Option<usize> {
    (1..slice.len().saturating_sub(1)).find(|&idx| slice[idx] == '*' && slice[idx + 1] == '*')
}

fn find_single_star(slice: &[char]) -> Option<usize> {
    (1..slice.len()).find(|&idx| slice[idx] == '*' && slice.get(idx + 1) != Some(&'*'))
}

fn wrap_styled_spans(spans: &[(String, Style)], max_width: usize) -> Vec<Vec<Span<'static>>> {
    let max_width = max_width.max(4);
    let mut lines: Vec<Vec<Span<'static>>> = Vec::new();
    let mut cur_line: Vec<Span<'static>> = Vec::new();
    let mut cur_w = 0usize;

    for (text, style) in spans {
        let mut chunk = String::new();
        for ch in text.chars() {
            let cw = ch.width().unwrap_or(0);
            if cur_w + cw > max_width && (cur_w > 0 || !chunk.is_empty()) {
                if !chunk.is_empty() {
                    cur_line.push(Span::styled(std::mem::take(&mut chunk), *style));
                }
                lines.push(std::mem::take(&mut cur_line));
                cur_w = 0;
            }
            chunk.push(ch);
            cur_w += cw;
        }
        if !chunk.is_empty() {
            cur_line.push(Span::styled(chunk, *style));
        }
    }

    if !cur_line.is_empty() || lines.is_empty() {
        lines.push(cur_line);
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_headings_nested_inline_tables_and_cjk_wrapping() {
        let md = "\
# 这个项目：**Anix** — POSIX 原生 Agent 内核

内核保留 **`shell`** 和 **`output`** 两条正交轴，且支持 `*.agent` 文件。

| 原语 | 说明 |
| :--- | :--- |
| `*.agent` | 带 Shebang 的普通进程 |
| Tree WAL | 树状日志支持 `anix fork` |

```rust
let x = 1;
```
";
        let lines = render_markdown(md, 60);
        let joined: Vec<String> = lines.iter().map(ToString::to_string).collect();
        // Heading inline `**Anix**` parsed without raw `**`
        assert!(joined[0].contains("Anix"));
        assert!(!joined[0].contains("**"));
        // Nested **`shell`** parsed without raw backticks or asterisks
        assert!(
            joined
                .iter()
                .any(|l| l.contains("shell") && !l.contains('`') && !l.contains("**"))
        );
        // Pipe table rendered as aligned Unicode box table
        assert!(joined.iter().any(|l| l.starts_with('┌')));
        assert!(joined.iter().any(|l| l.contains("Tree WAL")));
        assert!(joined.iter().any(|l| l.starts_with('└')));
        // Fenced code block intact
        assert!(joined.iter().any(|l| l.contains("╭─ rust")));
    }

    #[test]
    fn incremental_streamer_commits_blocks_in_real_time_matching_batch_render() {
        let md = "\
# 标题 **加粗**

第一段实时输出，带 `inline`。

| 级别 | 文件 | 说明 |
| :--- | :--- | :--- |
| P2 | `maildir.rs` | 批量领取中途失败 |
| P3 | `runtime.rs` | 注释同步 |

```rust
let a = 1;
let b = 2;
```

最后一行收尾";
        let width = 60;
        let expected: Vec<String> = render_markdown(md, width)
            .iter()
            .map(ToString::to_string)
            .collect();

        let mut streamer = MarkdownStreamer::new();
        let mut streamed: Vec<String> = Vec::new();
        let mut saw_midstream_commit = false;
        let mut saw_live_table_preview = false;

        // Feed in small multi-byte-safe slices to exercise mid-line and mid-table streaming.
        let chars: Vec<char> = md.chars().collect();
        for chunk in chars.chunks(7) {
            let s: String = chunk.iter().collect();
            streamer.push_delta(&s);
            let ready = streamer.take_ready_lines(width);
            if !ready.is_empty() {
                saw_midstream_commit = true;
                streamed.extend(ready.iter().map(ToString::to_string));
            }
            let preview: Vec<String> = streamer
                .preview_lines(width)
                .iter()
                .map(ToString::to_string)
                .collect();
            if preview.iter().any(|l| l.starts_with('┌')) {
                saw_live_table_preview = true;
            }
        }
        let (tail, had_any) = streamer.finish(width);
        assert!(had_any);
        streamed.extend(tail.iter().map(ToString::to_string));

        assert!(
            saw_midstream_commit,
            "completed lines must commit to scrollback during streaming, not only on finish"
        );
        assert!(
            saw_live_table_preview,
            "pipe table should render aligned live preview while streaming"
        );
        assert_eq!(streamed, expected);
    }

    #[test]
    fn extracts_last_fenced_code_block_cleanly() {
        let md = "\
First snippet:
```bash
echo one
```

Final snippet:
```rust
fn main() {
    println!(\"two\");
}
```
Done.";
        let got = extract_last_code_block(md).expect("should find code block");
        assert_eq!(got, "fn main() {\n    println!(\"two\");\n}");
        assert!(extract_last_code_block("plain text without fences").is_none());
    }

    #[test]
    fn highlights_unified_diff_in_batch_and_streaming_markdown() {
        let md = "\
```diff
diff --git a/foo.rs b/foo.rs
--- a/foo.rs
+++ b/foo.rs
@@ -1,2 +1,2 @@
-let old = 1;
+let new = 2;
 unchanged();
```";
        assert!(looks_like_unified_diff(
            "diff --git a/x b/x\n--- a/x\n+++ b/x\n@@ -1 +1 @@\n-a\n+b\n"
        ));
        assert!(!looks_like_unified_diff("- bullet one\n- bullet two\n"));

        let batch = render_markdown(md, 80);
        let find_fg = |lines: &[Line<'static>], needle: &str| -> Option<Color> {
            lines.iter().find_map(|l| {
                l.spans
                    .iter()
                    .find(|s| s.content.contains(needle))
                    .and_then(|s| s.style.fg)
            })
        };
        assert_eq!(find_fg(&batch, "diff --git"), Some(Color::White));
        assert_eq!(find_fg(&batch, "@@ -1,2"), Some(Color::Cyan));
        assert_eq!(find_fg(&batch, "-let old"), Some(Color::Red));
        assert_eq!(find_fg(&batch, "+let new"), Some(Color::Green));
        assert_eq!(find_fg(&batch, "unchanged"), Some(Color::Gray));

        let mut streamer = MarkdownStreamer::new();
        let mut streamed = Vec::new();
        for ch in md.chars() {
            streamer.push_delta(&ch.to_string());
            streamed.extend(streamer.take_ready_lines(80));
        }
        let (tail, _) = streamer.finish(80);
        streamed.extend(tail);
        assert_eq!(find_fg(&streamed, "-let old"), Some(Color::Red));
        assert_eq!(find_fg(&streamed, "+let new"), Some(Color::Green));
        assert_eq!(find_fg(&streamed, "@@ -1,2"), Some(Color::Cyan));
    }
}
