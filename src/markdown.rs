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
    let mut code_lang = String::new();
    let mut idx = 0usize;

    while idx < raw_lines.len() {
        let raw_line = raw_lines[idx];
        let trimmed = raw_line.trim_start();

        if let Some(fence_rest) = trimmed.strip_prefix("```") {
            if !in_code_block {
                in_code_block = true;
                code_lang = fence_rest.trim().to_owned();
                let header = if code_lang.is_empty() {
                    "╭─ code".to_owned()
                } else {
                    format!("╭─ {code_lang}")
                };
                out.push(Line::from(Span::styled(
                    header,
                    Style::default().fg(Color::DarkGray),
                )));
            } else {
                in_code_block = false;
                code_lang.clear();
                out.push(Line::from(Span::styled(
                    "╰─",
                    Style::default().fg(Color::DarkGray),
                )));
            }
            idx += 1;
            continue;
        }

        if in_code_block {
            let code_width = width.saturating_sub(2).max(12);
            for chunk in wrap_plain_str(raw_line, code_width) {
                out.push(Line::from(vec![
                    Span::styled("│ ", Style::default().fg(Color::DarkGray)),
                    Span::styled(chunk, Style::default().fg(Color::Yellow)),
                ]));
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

        // Horizontal rule
        if is_horizontal_rule(trimmed) {
            let rule_w = width.min(60);
            out.push(Line::from(Span::styled(
                "─".repeat(rule_w),
                Style::default().fg(Color::DarkGray),
            )));
            idx += 1;
            continue;
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
            idx += 1;
            continue;
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
            idx += 1;
            continue;
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
            idx += 1;
            continue;
        }

        // Regular paragraph line with inline formatting
        let spans = parse_inline_spans(raw_line, Style::default());
        for line_spans in wrap_styled_spans(&spans, width) {
            out.push(Line::from(line_spans));
        }
        idx += 1;
    }

    if in_code_block {
        out.push(Line::from(Span::styled(
            "╰─",
            Style::default().fg(Color::DarkGray),
        )));
    }

    if out.is_empty() {
        out.push(Line::default());
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
}
