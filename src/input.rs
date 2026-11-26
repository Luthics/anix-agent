//! CJK/Unicode-aware multi-line input widget with cursor navigation, command history,
//! and `Tab` completion for slash commands and `@path` filesystem references.

use std::fs;
use std::path::Path;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

/// All built-in slash commands offered by `Tab` completion.
pub const SLASH_COMMANDS: &[&str] = &[
    "/help",
    "/tree",
    "/fork",
    "/compact",
    "/sessions",
    "/spawn",
    "/workers",
    "/send",
    "/inspect",
    "/model",
    "/agent",
    "/mode",
    "/new",
    "/quit",
];

/// Outcome of feeding a key event into [`InputBox`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InputAction {
    None,
    Submit(String),
    CompletionHint(String),
}

/// Multi-line text input box maintaining cursor by char offset and command history.
#[derive(Debug, Default)]
pub struct InputBox {
    text: String,
    cursor_char: usize,
    history: Vec<String>,
    history_idx: Option<usize>,
    draft_before_history: String,
}

impl InputBox {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn text(&self) -> &str {
        &self.text
    }

    pub fn is_empty(&self) -> bool {
        self.text.is_empty()
    }

    pub fn clear(&mut self) {
        self.text.clear();
        self.cursor_char = 0;
        self.history_idx = None;
    }

    pub fn set_text(&mut self, value: impl Into<String>) {
        self.text = value.into();
        self.cursor_char = self.text.chars().count();
        self.history_idx = None;
    }

    /// Push a submitted line into history (deduplicating immediate repeats).
    pub fn record_history(&mut self, entry: String) {
        let trimmed = entry.trim();
        if trimmed.is_empty() {
            return;
        }
        if self.history.last().map(String::as_str) != Some(trimmed) {
            self.history.push(trimmed.to_owned());
        }
        self.history_idx = None;
    }

    /// Handle a key event; returns `Submit(text)` on `Enter` (without newline modifiers).
    pub fn handle_key(&mut self, key: KeyEvent, cwd: &Path) -> InputAction {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        let shift = key.modifiers.contains(KeyModifiers::SHIFT);

        match key.code {
            KeyCode::Enter if alt || shift => {
                self.insert_char('\n');
                InputAction::None
            }
            KeyCode::Char('j') if ctrl => {
                self.insert_char('\n');
                InputAction::None
            }
            KeyCode::Enter => {
                let submitted = std::mem::take(&mut self.text);
                self.cursor_char = 0;
                self.history_idx = None;
                self.record_history(submitted.clone());
                InputAction::Submit(submitted)
            }
            KeyCode::Tab => self.complete_at_cursor(cwd),
            KeyCode::Backspace => {
                self.delete_char_before();
                InputAction::None
            }
            KeyCode::Delete => {
                self.delete_char_at();
                InputAction::None
            }
            KeyCode::Left => {
                self.cursor_char = self.cursor_char.saturating_sub(1);
                InputAction::None
            }
            KeyCode::Right => {
                let len = self.char_len();
                if self.cursor_char < len {
                    self.cursor_char += 1;
                }
                InputAction::None
            }
            KeyCode::Home => {
                self.move_line_start();
                InputAction::None
            }
            KeyCode::End => {
                self.move_line_end();
                InputAction::None
            }
            KeyCode::Up => {
                if !self.move_up_line() {
                    self.history_prev();
                }
                InputAction::None
            }
            KeyCode::Down => {
                if !self.move_down_line() {
                    self.history_next();
                }
                InputAction::None
            }
            KeyCode::Char('a') if ctrl => {
                self.move_line_start();
                InputAction::None
            }
            KeyCode::Char('e') if ctrl => {
                self.move_line_end();
                InputAction::None
            }
            KeyCode::Char('u') if ctrl => {
                self.kill_to_line_start();
                InputAction::None
            }
            KeyCode::Char('k') if ctrl => {
                self.kill_to_line_end();
                InputAction::None
            }
            KeyCode::Char('w') if ctrl => {
                self.delete_prev_word();
                InputAction::None
            }
            KeyCode::Char(c) if !ctrl && !alt => {
                self.insert_char(c);
                InputAction::None
            }
            _ => InputAction::None,
        }
    }

    /// Compute `(visual_row, visual_col)` of the cursor for a wrapped viewport of `inner_width` columns.
    pub fn visual_lines_and_cursor(&self, inner_width: usize) -> (Vec<String>, (usize, usize)) {
        let max_w = inner_width.max(4);
        let mut lines: Vec<String> = Vec::new();
        let mut cur_line = String::new();
        let mut cur_w = 0usize;
        let mut cursor_pos = (0usize, 0usize);

        for (idx, ch) in self.text.chars().enumerate() {
            if idx == self.cursor_char {
                cursor_pos = (lines.len(), cur_w);
            }
            if ch == '\n' {
                lines.push(std::mem::take(&mut cur_line));
                cur_w = 0;
                continue;
            }
            let cw = ch.width().unwrap_or(0);
            if cur_w + cw > max_w && !cur_line.is_empty() {
                lines.push(std::mem::take(&mut cur_line));
                cur_w = 0;
            }
            cur_line.push(ch);
            cur_w += cw;
        }
        if self.cursor_char >= self.char_len() {
            cursor_pos = (lines.len(), cur_w);
        }
        lines.push(cur_line);
        (lines, cursor_pos)
    }

    fn char_len(&self) -> usize {
        self.text.chars().count()
    }

    fn byte_index_of(&self, char_idx: usize) -> usize {
        self.text
            .char_indices()
            .nth(char_idx)
            .map(|(b, _)| b)
            .unwrap_or(self.text.len())
    }

    fn insert_char(&mut self, ch: char) {
        let b = self.byte_index_of(self.cursor_char);
        self.text.insert(b, ch);
        self.cursor_char += 1;
        self.history_idx = None;
    }

    fn delete_char_before(&mut self) {
        if self.cursor_char == 0 {
            return;
        }
        let start = self.byte_index_of(self.cursor_char - 1);
        let end = self.byte_index_of(self.cursor_char);
        self.text.replace_range(start..end, "");
        self.cursor_char -= 1;
    }

    fn delete_char_at(&mut self) {
        let len = self.char_len();
        if self.cursor_char >= len {
            return;
        }
        let start = self.byte_index_of(self.cursor_char);
        let end = self.byte_index_of(self.cursor_char + 1);
        self.text.replace_range(start..end, "");
    }

    fn move_line_start(&mut self) {
        let chars: Vec<char> = self.text.chars().collect();
        let mut i = self.cursor_char;
        while i > 0 && chars[i - 1] != '\n' {
            i -= 1;
        }
        self.cursor_char = i;
    }

    fn move_line_end(&mut self) {
        let chars: Vec<char> = self.text.chars().collect();
        let mut i = self.cursor_char;
        while i < chars.len() && chars[i] != '\n' {
            i += 1;
        }
        self.cursor_char = i;
    }

    fn kill_to_line_start(&mut self) {
        let old = self.cursor_char;
        self.move_line_start();
        let start = self.byte_index_of(self.cursor_char);
        let end = self.byte_index_of(old);
        self.text.replace_range(start..end, "");
    }

    fn kill_to_line_end(&mut self) {
        let start_char = self.cursor_char;
        let chars: Vec<char> = self.text.chars().collect();
        let mut end_char = start_char;
        while end_char < chars.len() && chars[end_char] != '\n' {
            end_char += 1;
        }
        let start = self.byte_index_of(start_char);
        let end = self.byte_index_of(end_char);
        self.text.replace_range(start..end, "");
    }

    fn delete_prev_word(&mut self) {
        if self.cursor_char == 0 {
            return;
        }
        let chars: Vec<char> = self.text.chars().collect();
        let mut i = self.cursor_char;
        while i > 0 && chars[i - 1].is_whitespace() && chars[i - 1] != '\n' {
            i -= 1;
        }
        while i > 0 && !chars[i - 1].is_whitespace() {
            i -= 1;
        }
        let start = self.byte_index_of(i);
        let end = self.byte_index_of(self.cursor_char);
        self.text.replace_range(start..end, "");
        self.cursor_char = i;
    }

    fn move_up_line(&mut self) -> bool {
        let chars: Vec<char> = self.text.chars().collect();
        let mut line_start = self.cursor_char;
        while line_start > 0 && chars[line_start - 1] != '\n' {
            line_start -= 1;
        }
        if line_start == 0 {
            return false;
        }
        let col = self.cursor_char - line_start;
        let prev_end = line_start - 1;
        let mut prev_start = prev_end;
        while prev_start > 0 && chars[prev_start - 1] != '\n' {
            prev_start -= 1;
        }
        let prev_len = prev_end - prev_start;
        self.cursor_char = prev_start + col.min(prev_len);
        true
    }

    fn move_down_line(&mut self) -> bool {
        let chars: Vec<char> = self.text.chars().collect();
        let mut line_start = self.cursor_char;
        while line_start > 0 && chars[line_start - 1] != '\n' {
            line_start -= 1;
        }
        let mut line_end = self.cursor_char;
        while line_end < chars.len() && chars[line_end] != '\n' {
            line_end += 1;
        }
        if line_end >= chars.len() {
            return false;
        }
        let col = self.cursor_char - line_start;
        let next_start = line_end + 1;
        let mut next_end = next_start;
        while next_end < chars.len() && chars[next_end] != '\n' {
            next_end += 1;
        }
        let next_len = next_end - next_start;
        self.cursor_char = next_start + col.min(next_len);
        true
    }

    fn history_prev(&mut self) {
        if self.history.is_empty() {
            return;
        }
        let next_idx = match self.history_idx {
            None => {
                self.draft_before_history = self.text.clone();
                self.history.len() - 1
            }
            Some(0) => 0,
            Some(i) => i - 1,
        };
        self.history_idx = Some(next_idx);
        self.text = self.history[next_idx].clone();
        self.cursor_char = self.text.chars().count();
    }

    fn history_next(&mut self) {
        let Some(idx) = self.history_idx else {
            return;
        };
        if idx + 1 >= self.history.len() {
            self.history_idx = None;
            self.text = std::mem::take(&mut self.draft_before_history);
        } else {
            self.history_idx = Some(idx + 1);
            self.text = self.history[idx + 1].clone();
        }
        self.cursor_char = self.text.chars().count();
    }

    fn complete_at_cursor(&mut self, cwd: &Path) -> InputAction {
        let byte_end = self.byte_index_of(self.cursor_char);
        let prefix = &self.text[..byte_end];
        let token_start = prefix
            .rfind(|c: char| c.is_whitespace())
            .map(|i| i + 1)
            .unwrap_or(0);
        let token = &prefix[token_start..];

        // Slash command completion at start of buffer.
        if token_start == 0 && token.starts_with('/') {
            let matches: Vec<&str> = SLASH_COMMANDS
                .iter()
                .copied()
                .filter(|cmd| cmd.starts_with(token))
                .collect();
            return self.apply_completion(token_start, byte_end, &matches, true);
        }

        // `@path` filesystem completion anywhere in input.
        if let Some(rel) = token.strip_prefix('@') {
            let candidates = list_path_completions(cwd, rel);
            let with_at: Vec<String> = candidates.into_iter().map(|c| format!("@{c}")).collect();
            let refs: Vec<&str> = with_at.iter().map(String::as_str).collect();
            return self.apply_completion(token_start, byte_end, &refs, false);
        }

        InputAction::None
    }

    fn apply_completion(
        &mut self,
        byte_start: usize,
        byte_end: usize,
        matches: &[&str],
        append_space_on_exact: bool,
    ) -> InputAction {
        match matches {
            [] => InputAction::None,
            [single] => {
                let mut replacement = (*single).to_owned();
                if append_space_on_exact || !replacement.ends_with('/') {
                    replacement.push(' ');
                }
                self.text.replace_range(byte_start..byte_end, &replacement);
                self.cursor_char = self.text[..byte_start + replacement.len()].chars().count();
                InputAction::None
            }
            many => {
                let lcp = longest_common_prefix(many);
                if lcp.len() > byte_end - byte_start {
                    self.text.replace_range(byte_start..byte_end, &lcp);
                    self.cursor_char = self.text[..byte_start + lcp.len()].chars().count();
                }
                InputAction::CompletionHint(many.join("  "))
            }
        }
    }
}

fn list_path_completions(cwd: &Path, partial: &str) -> Vec<String> {
    let (dir_prefix, file_prefix) = match partial.rfind('/') {
        Some(idx) => (&partial[..=idx], &partial[idx + 1..]),
        None => ("", partial),
    };
    let search_dir = if dir_prefix.is_empty() {
        cwd.to_path_buf()
    } else {
        cwd.join(dir_prefix)
    };
    let Ok(entries) = fs::read_dir(&search_dir) else {
        return Vec::new();
    };
    let mut matches = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if !file_prefix.starts_with('.') && name.starts_with('.') {
            continue;
        }
        if name.starts_with(file_prefix) {
            let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
            let mut full = format!("{dir_prefix}{name}");
            if is_dir {
                full.push('/');
            }
            matches.push(full);
        }
    }
    matches.sort();
    matches.truncate(12);
    matches
}

fn longest_common_prefix(items: &[&str]) -> String {
    let Some(first) = items.first() else {
        return String::new();
    };
    let mut prefix: Vec<char> = first.chars().collect();
    for item in &items[1..] {
        let chars: Vec<char> = item.chars().collect();
        let common = prefix
            .iter()
            .zip(chars.iter())
            .take_while(|(a, b)| a == b)
            .count();
        prefix.truncate(common);
    }
    prefix.into_iter().collect()
}

/// Helper measuring terminal display columns for a string slice.
pub fn display_width(s: &str) -> usize {
    s.width()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cjk_editing_multiline_and_tab_completion_work() {
        let cwd = std::env::temp_dir();
        let mut input = InputBox::new();
        for ch in ['你', '好'] {
            input.handle_key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE), &cwd);
        }
        let (lines, (row, col)) = input.visual_lines_and_cursor(20);
        assert_eq!(lines, vec!["你好"]);
        assert_eq!((row, col), (0, 4));

        // Alt+Enter inserts newline; Enter submits.
        input.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::ALT), &cwd);
        input.handle_key(KeyEvent::new(KeyCode::Char('!'), KeyModifiers::NONE), &cwd);
        assert_eq!(
            input.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &cwd),
            InputAction::Submit("你好\n!".into())
        );

        // Slash completion
        input.set_text("/tr");
        input.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE), &cwd);
        assert_eq!(input.text(), "/tree ");
    }
}
