//! CJK/Unicode-aware multi-line input widget with cursor navigation, command history,
//! and an interactive inline completion palette for `/commands`, sub-arguments, and `@path`.

use std::fs;
use std::path::Path;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

/// Number of visible rows in the inline completion dropdown window.
pub const COMPLETION_VISIBLE_ROWS: usize = 5;

/// Metadata for a built-in slash command shown in the live completion palette.
#[derive(Debug, Clone, Copy)]
pub struct SlashCommandSpec {
    pub name: &'static str,
    pub args_hint: &'static str,
    pub description: &'static str,
    pub execute_on_enter: bool,
}

/// All built-in slash commands offered by live completion and `/help`.
pub const SLASH_COMMAND_SPECS: &[SlashCommandSpec] = &[
    SlashCommandSpec {
        name: "/agent",
        args_hint: "<name|path>",
        description: "Switch focused worker *.agent script",
        execute_on_enter: false,
    },
    SlashCommandSpec {
        name: "/compact",
        args_hint: "",
        description: "Compact context online (Anix-Control: compact)",
        execute_on_enter: true,
    },
    SlashCommandSpec {
        name: "/fork",
        args_hint: "[name]",
        description: "Fork active WAL into an independent branch",
        execute_on_enter: true,
    },
    SlashCommandSpec {
        name: "/help",
        args_hint: "",
        description: "Show all slash commands and keybindings",
        execute_on_enter: true,
    },
    SlashCommandSpec {
        name: "/inspect",
        args_hint: "<node_id>",
        description: "Inspect full payload of a Tree WAL node",
        execute_on_enter: false,
    },
    SlashCommandSpec {
        name: "/mode",
        args_hint: "<ask|auto|plan>",
        description: "Set shell approval policy (Shift+Tab)",
        execute_on_enter: false,
    },
    SlashCommandSpec {
        name: "/model",
        args_hint: "<model_id>",
        description: "Switch focused worker LLM model",
        execute_on_enter: false,
    },
    SlashCommandSpec {
        name: "/new",
        args_hint: "",
        description: "Start a fresh persistent session",
        execute_on_enter: true,
    },
    SlashCommandSpec {
        name: "/quit",
        args_hint: "",
        description: "Gracefully stop all workers and exit",
        execute_on_enter: true,
    },
    SlashCommandSpec {
        name: "/send",
        args_hint: "<worker> <msg>",
        description: "Send a Maildir message to a running worker",
        execute_on_enter: false,
    },
    SlashCommandSpec {
        name: "/sessions",
        args_hint: "",
        description: "Browse and switch saved project sessions",
        execute_on_enter: true,
    },
    SlashCommandSpec {
        name: "/spawn",
        args_hint: "<agent> [prompt]",
        description: "Spawn a parallel worker with --notify callback",
        execute_on_enter: false,
    },
    SlashCommandSpec {
        name: "/tree",
        args_hint: "",
        description: "Browse Tree WAL branches and switch leaf",
        execute_on_enter: true,
    },
    SlashCommandSpec {
        name: "/workers",
        args_hint: "",
        description: "Manage live workers and active hold leases (^W)",
        execute_on_enter: true,
    },
];

/// Dynamic runtime context passed into live completion (cwd, discovered agents, active workers).
#[derive(Debug, Clone, Copy)]
pub struct CompletionContext<'a> {
    pub cwd: &'a Path,
    pub agents: &'a [String],
    pub workers: &'a [String],
}

/// Single candidate row displayed in the live inline completion dropdown.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompletionItem {
    pub label: String,
    pub insert: String,
    pub description: String,
    pub execute_on_enter: bool,
    pub append_space: bool,
}

/// Active state of the live inline completion dropdown.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompletionMenu {
    pub token_byte_start: usize,
    pub token_byte_end: usize,
    pub items: Vec<CompletionItem>,
    pub selected: usize,
    pub scroll: usize,
}

impl CompletionMenu {
    pub fn visible_slice(&self, max_rows: usize) -> &[CompletionItem] {
        let end = (self.scroll + max_rows).min(self.items.len());
        &self.items[self.scroll.min(end)..end]
    }

    pub fn hidden_above(&self) -> usize {
        self.scroll
    }

    pub fn hidden_below(&self, max_rows: usize) -> usize {
        self.items.len().saturating_sub(self.scroll + max_rows)
    }

    fn move_up(&mut self, visible_rows: usize) {
        if self.items.is_empty() {
            return;
        }
        if self.selected == 0 {
            self.selected = self.items.len() - 1;
        } else {
            self.selected -= 1;
        }
        self.clamp_scroll(visible_rows);
    }

    fn move_down(&mut self, visible_rows: usize) {
        if self.items.is_empty() {
            return;
        }
        self.selected = (self.selected + 1) % self.items.len();
        self.clamp_scroll(visible_rows);
    }

    fn clamp_scroll(&mut self, visible_rows: usize) {
        let rows = visible_rows.max(1);
        if self.selected < self.scroll {
            self.scroll = self.selected;
        } else if self.selected >= self.scroll + rows {
            self.scroll = self.selected + 1 - rows;
        }
    }
}

/// Outcome of feeding a key event into [`InputBox`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InputAction {
    None,
    Submit(String),
}

/// Multi-line text input box maintaining cursor by char offset, command history,
/// and live interactive completion dropdown state.
#[derive(Debug, Default)]
pub struct InputBox {
    text: String,
    cursor_char: usize,
    history: Vec<String>,
    history_idx: Option<usize>,
    draft_before_history: String,
    completion: Option<CompletionMenu>,
    completion_dismissed: bool,
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

    pub fn completion(&self) -> Option<&CompletionMenu> {
        self.completion.as_ref()
    }

    pub fn dismiss_completion(&mut self) -> bool {
        if self.completion.is_some() {
            self.completion = None;
            self.completion_dismissed = true;
            true
        } else {
            false
        }
    }

    pub fn clear(&mut self) {
        self.text.clear();
        self.cursor_char = 0;
        self.history_idx = None;
        self.completion = None;
        self.completion_dismissed = false;
    }

    pub fn set_text(&mut self, value: impl Into<String>) {
        self.text = value.into();
        self.cursor_char = self.text.chars().count();
        self.history_idx = None;
        self.completion_dismissed = false;
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

    /// Handle a key event; manages live completion navigation/selection automatically.
    pub fn handle_key(&mut self, key: KeyEvent, ctx: CompletionContext<'_>) -> InputAction {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        let shift = key.modifiers.contains(KeyModifiers::SHIFT);

        // Live completion menu interception when visible.
        if self.completion.is_some() {
            match key.code {
                KeyCode::Esc => {
                    self.dismiss_completion();
                    return InputAction::None;
                }
                KeyCode::Up => {
                    if let Some(ref mut menu) = self.completion {
                        menu.move_up(COMPLETION_VISIBLE_ROWS);
                    }
                    return InputAction::None;
                }
                KeyCode::Down => {
                    if let Some(ref mut menu) = self.completion {
                        menu.move_down(COMPLETION_VISIBLE_ROWS);
                    }
                    return InputAction::None;
                }
                KeyCode::Char('p') if ctrl => {
                    if let Some(ref mut menu) = self.completion {
                        menu.move_up(COMPLETION_VISIBLE_ROWS);
                    }
                    return InputAction::None;
                }
                KeyCode::Char('n') if ctrl => {
                    if let Some(ref mut menu) = self.completion {
                        menu.move_down(COMPLETION_VISIBLE_ROWS);
                    }
                    return InputAction::None;
                }
                KeyCode::Tab => {
                    self.accept_selected_completion(false, ctx);
                    return InputAction::None;
                }
                KeyCode::Enter if !alt && !shift => {
                    if let Some(submitted) = self.accept_selected_completion(true, ctx) {
                        return InputAction::Submit(submitted);
                    }
                    return InputAction::None;
                }
                _ => {}
            }
        }

        let mut edited = false;
        let action = match key.code {
            KeyCode::Enter if alt || shift => {
                self.insert_char('\n');
                edited = true;
                InputAction::None
            }
            KeyCode::Char('j') if ctrl => {
                self.insert_char('\n');
                edited = true;
                InputAction::None
            }
            KeyCode::Enter => {
                let submitted = std::mem::take(&mut self.text);
                self.cursor_char = 0;
                self.history_idx = None;
                self.completion = None;
                self.completion_dismissed = false;
                self.record_history(submitted.clone());
                return InputAction::Submit(submitted);
            }
            KeyCode::Tab => {
                // Re-open dismissed completion menu or trigger immediate completion.
                self.completion_dismissed = false;
                self.refresh_completion(ctx);
                if self.completion.as_ref().is_some_and(|m| m.items.len() == 1) {
                    self.accept_selected_completion(false, ctx);
                }
                return InputAction::None;
            }
            KeyCode::Backspace => {
                self.delete_char_before();
                edited = true;
                InputAction::None
            }
            KeyCode::Delete => {
                self.delete_char_at();
                edited = true;
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
                edited = true;
                InputAction::None
            }
            KeyCode::Char('k') if ctrl => {
                self.kill_to_line_end();
                edited = true;
                InputAction::None
            }
            KeyCode::Char('w') if ctrl => {
                self.delete_prev_word();
                edited = true;
                InputAction::None
            }
            KeyCode::Char(c) if !ctrl && !alt => {
                self.insert_char(c);
                edited = true;
                InputAction::None
            }
            _ => InputAction::None,
        };

        if edited {
            self.completion_dismissed = false;
        }
        if !self.completion_dismissed {
            self.refresh_completion(ctx);
        }
        action
    }

    /// Recompute live completion candidates at the current cursor position.
    pub fn refresh_completion(&mut self, ctx: CompletionContext<'_>) {
        let prev_label = self
            .completion
            .as_ref()
            .and_then(|m| m.items.get(m.selected))
            .map(|it| it.label.clone());

        let next = compute_completion(&self.text, self.byte_index_of(self.cursor_char), ctx);
        self.completion = next.map(|mut menu| {
            if let Some(ref lbl) = prev_label
                && let Some(pos) = menu.items.iter().position(|it| &it.label == lbl)
            {
                menu.selected = pos;
                menu.clamp_scroll(COMPLETION_VISIBLE_ROWS);
            }
            menu
        });
    }

    fn accept_selected_completion(
        &mut self,
        from_enter: bool,
        ctx: CompletionContext<'_>,
    ) -> Option<String> {
        let menu = self.completion.take()?;
        let item = menu.items.get(menu.selected)?.clone();

        let mut replacement = item.insert.clone();
        let should_exec = from_enter && item.execute_on_enter;
        if !should_exec && item.append_space {
            replacement.push(' ');
        }

        self.text
            .replace_range(menu.token_byte_start..menu.token_byte_end, &replacement);
        self.cursor_char = self.text[..menu.token_byte_start + replacement.len()]
            .chars()
            .count();

        if should_exec {
            let submitted = std::mem::take(&mut self.text);
            self.cursor_char = 0;
            self.history_idx = None;
            self.completion_dismissed = false;
            self.record_history(submitted.clone());
            return Some(submitted);
        }

        // Refresh so chained sub-completions (e.g. `/mode ` -> `ask|auto|plan`, `@src/` -> files) open automatically!
        self.completion_dismissed = false;
        self.refresh_completion(ctx);
        None
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
}

fn compute_completion(
    text: &str,
    cursor_byte: usize,
    ctx: CompletionContext<'_>,
) -> Option<CompletionMenu> {
    let prefix = &text[..cursor_byte];
    let token_start = prefix
        .rfind(char::is_whitespace)
        .map(|i| i + 1)
        .unwrap_or(0);
    let token = &prefix[token_start..];

    // 1. `@path` filesystem completion anywhere in the prompt.
    if let Some(rel) = token.strip_prefix('@') {
        let items = list_path_completion_items(ctx.cwd, rel);
        if items.is_empty() {
            return None;
        }
        return Some(CompletionMenu {
            token_byte_start: token_start,
            token_byte_end: cursor_byte,
            items,
            selected: 0,
            scroll: 0,
        });
    }

    // Only trigger slash-command & slash-argument menus on single-line slash inputs.
    if text.contains('\n') || !text.starts_with('/') {
        return None;
    }

    // 2. First token: `/command` live palette (prefix > substring > fuzzy subsequence).
    if token_start == 0 {
        let query = token.to_ascii_lowercase();
        let needle = query.strip_prefix('/').unwrap_or(&query);
        let mut prefix_hits = Vec::new();
        let mut contains_hits = Vec::new();
        let mut fuzzy_hits = Vec::new();
        for spec in SLASH_COMMAND_SPECS {
            let name_lower = spec.name.to_ascii_lowercase();
            let bare = name_lower.strip_prefix('/').unwrap_or(&name_lower);
            let item = CompletionItem {
                label: if spec.args_hint.is_empty() {
                    spec.name.to_owned()
                } else {
                    format!("{} {}", spec.name, spec.args_hint)
                },
                insert: spec.name.to_owned(),
                description: spec.description.to_owned(),
                execute_on_enter: spec.execute_on_enter,
                append_space: !spec.execute_on_enter,
            };
            if needle.is_empty() || bare.starts_with(needle) {
                prefix_hits.push(item);
            } else if bare.contains(needle) {
                contains_hits.push(item);
            } else if is_fuzzy_subsequence(needle, bare) {
                fuzzy_hits.push(item);
            }
        }
        prefix_hits.extend(contains_hits);
        prefix_hits.extend(fuzzy_hits);
        if prefix_hits.is_empty() {
            return None;
        }
        return Some(CompletionMenu {
            token_byte_start: 0,
            token_byte_end: cursor_byte,
            items: prefix_hits,
            selected: 0,
            scroll: 0,
        });
    }

    // 3. Second token: contextual sub-completions for `/mode`, `/agent`, `/spawn`, `/send`.
    let before_token = prefix[..token_start].trim();
    let mut parts = before_token.split_whitespace();
    let cmd = parts.next()?;
    let extra_arg_count = parts.count();
    let q = token.to_ascii_lowercase();

    let items: Vec<CompletionItem> = match (cmd, extra_arg_count) {
        ("/mode", 0) => [
            ("ask", "Auto-allow safe reads; prompt inline for writes"),
            ("auto", "Automatically approve all shell commands"),
            ("plan", "Read-only planning mode; block mutating commands"),
        ]
        .into_iter()
        .filter(|(m, _)| m.starts_with(&q))
        .map(|(m, desc)| CompletionItem {
            label: m.to_owned(),
            insert: m.to_owned(),
            description: desc.to_owned(),
            execute_on_enter: true,
            append_space: false,
        })
        .collect(),
        ("/agent", 0) => ctx
            .agents
            .iter()
            .filter(|a| a.to_ascii_lowercase().starts_with(&q))
            .map(|a| CompletionItem {
                label: a.clone(),
                insert: a.clone(),
                description: format!("~/.config/anix/agents/{a}.agent"),
                execute_on_enter: true,
                append_space: false,
            })
            .collect(),
        ("/spawn", 0) => ctx
            .agents
            .iter()
            .filter(|a| a.to_ascii_lowercase().starts_with(&q))
            .map(|a| CompletionItem {
                label: a.clone(),
                insert: a.clone(),
                description: format!("Spawn background worker ({a}.agent)"),
                execute_on_enter: false,
                append_space: true,
            })
            .collect(),
        ("/send", 0) => ctx
            .workers
            .iter()
            .filter(|w| w.to_ascii_lowercase().starts_with(&q))
            .map(|w| CompletionItem {
                label: w.clone(),
                insert: w.clone(),
                description: format!("Deliver Maildir message to worker `{w}`"),
                execute_on_enter: false,
                append_space: true,
            })
            .collect(),
        _ => Vec::new(),
    };

    if items.is_empty() {
        return None;
    }
    Some(CompletionMenu {
        token_byte_start: token_start,
        token_byte_end: cursor_byte,
        items,
        selected: 0,
        scroll: 0,
    })
}

fn list_path_completion_items(cwd: &Path, partial: &str) -> Vec<CompletionItem> {
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
    let mut matches: Vec<(bool, String)> = Vec::new();
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
            matches.push((is_dir, full));
        }
    }
    matches.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
    matches.truncate(24);
    matches
        .into_iter()
        .map(|(is_dir, full)| CompletionItem {
            label: format!("@{full}"),
            insert: format!("@{full}"),
            description: if is_dir { "directory" } else { "file" }.to_owned(),
            execute_on_enter: false,
            append_space: !is_dir,
        })
        .collect()
}

fn is_fuzzy_subsequence(needle: &str, haystack: &str) -> bool {
    let mut h_chars = haystack.chars();
    for nc in needle.chars() {
        if !h_chars.any(|hc| hc == nc) {
            return false;
        }
    }
    true
}

/// Helper measuring terminal display columns for a string slice.
pub fn display_width(s: &str) -> usize {
    s.width()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_ctx(cwd: &Path) -> CompletionContext<'_> {
        CompletionContext {
            cwd,
            agents: &[],
            workers: &[],
        }
    }

    #[test]
    fn cjk_editing_multiline_and_live_slash_palette_work() {
        let cwd = std::env::temp_dir();
        let ctx = test_ctx(&cwd);
        let mut input = InputBox::new();

        for ch in ['你', '好'] {
            input.handle_key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE), ctx);
        }
        let (lines, (row, col)) = input.visual_lines_and_cursor(20);
        assert_eq!(lines, vec!["你好"]);
        assert_eq!((row, col), (0, 4));

        // Alt+Enter inserts newline; Enter submits.
        input.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::ALT), ctx);
        input.handle_key(KeyEvent::new(KeyCode::Char('!'), KeyModifiers::NONE), ctx);
        assert_eq!(
            input.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), ctx),
            InputAction::Submit("你好\n!".into())
        );

        // Typing `/` immediately opens full live dropdown!
        input.handle_key(KeyEvent::new(KeyCode::Char('/'), KeyModifiers::NONE), ctx);
        let menu = input.completion().expect("live slash menu opens on `/`");
        assert_eq!(menu.items.len(), SLASH_COMMAND_SPECS.len());
        assert_eq!(menu.selected, 0);

        // Down arrow moves selection; Esc cancels menu.
        input.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE), ctx);
        assert_eq!(input.completion().unwrap().selected, 1);
        input.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), ctx);
        assert!(input.completion().is_none());

        // Typing `mo` filters to `/mode` & `/model`; Tab on `/mode` opens `ask|auto|plan` sub-menu!
        input.clear();
        for ch in ['/', 'm', 'o', 'd', 'e'] {
            input.handle_key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE), ctx);
        }
        let m = input.completion().unwrap();
        assert_eq!(m.items[0].insert, "/mode");
        input.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE), ctx);
        assert_eq!(input.text(), "/mode ");
        let sub = input.completion().expect("chained /mode sub-menu");
        assert_eq!(sub.items.len(), 3);
        assert_eq!(sub.items[0].label, "ask");
        // Down -> `auto`, Enter -> submits `/mode auto` immediately!
        input.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE), ctx);
        assert_eq!(
            input.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), ctx),
            InputAction::Submit("/mode auto".into())
        );
    }
}
