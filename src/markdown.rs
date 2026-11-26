//! Lightweight, CJK-aware Markdown -> `ratatui::text::Line<'static>` renderer for inline
//! scrollback rendering and streaming previews.

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

/// Render a Markdown document into styled, width-wrapped terminal lines.
pub fn render_markdown(md: &str, width: usize) -> Vec<Line<'static>> {
    let width = width.max(20);
    let mut out = Vec::new();
    let mut in_code_block = false;
    let mut code_lang = String::new();

    for raw_line in md.lines() {
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
            continue;
        }

        if trimmed.is_empty() {
            out.push(Line::default());
            continue;
        }

        // Horizontal rule
        if is_horizontal_rule(trimmed) {
            let rule_w = width.min(60);
            out.push(Line::from(Span::styled(
                "─".repeat(rule_w),
                Style::default().fg(Color::DarkGray),
            )));
            continue;
        }

        // Headings
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
            for chunk in wrap_plain_str(rest, width) {
                out.push(Line::from(Span::styled(chunk, style)));
            }
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
            continue;
        }

        // Bullet / numbered lists
        if let Some((indent_prefix, bullet, rest)) = parse_list_item(raw_line) {
            let prefix_str = format!("{indent_prefix}{bullet} ");
            let cont_prefix = " ".repeat(prefix_str.width());
            let body_w = width.saturating_sub(prefix_str.width()).max(12);
            let spans = parse_inline_spans(rest, Style::default());
            let wrapped = wrap_styled_spans(&spans, body_w);
            for (idx, mut line_spans) in wrapped.into_iter().enumerate() {
                let mut full = Vec::with_capacity(line_spans.len() + 1);
                if idx == 0 {
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
            continue;
        }

        // Regular paragraph line with inline formatting
        let spans = parse_inline_spans(raw_line, Style::default());
        for line_spans in wrap_styled_spans(&spans, width) {
            out.push(Line::from(line_spans));
        }
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
        if chars[i] == '`' {
            if let Some(end) = chars[i + 1..].iter().position(|&c| c == '`') {
                if !buf.is_empty() {
                    out.push((std::mem::take(&mut buf), base));
                }
                let code: String = chars[i + 1..i + 1 + end].iter().collect();
                out.push((code, base.fg(Color::Yellow).add_modifier(Modifier::BOLD)));
                i += end + 2;
                continue;
            }
        }

        // Bold: **...**
        if i + 1 < chars.len() && chars[i] == '*' && chars[i + 1] == '*' {
            if let Some(rel_end) = find_double_star(&chars[i + 2..]) {
                if !buf.is_empty() {
                    out.push((std::mem::take(&mut buf), base));
                }
                let bold_text: String = chars[i + 2..i + 2 + rel_end].iter().collect();
                out.push((bold_text, base.add_modifier(Modifier::BOLD)));
                i += rel_end + 4;
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
    fn renders_headings_code_blocks_and_cjk_wrapping() {
        let md = "# Title\n\n- **Bold** item with `inline_code`\n\n```rust\nlet x = 1;\n```\n\n你好世界测试中文自动换行功能";
        let lines = render_markdown(md, 16);
        assert!(lines.len() >= 8);
        assert!(lines[0].to_string().contains("Title"));
        assert!(lines.iter().any(|l| l.to_string().contains("╭─ rust")));
        assert!(lines.iter().any(|l| l.to_string().contains("let x = 1;")));
    }
}
