//! CJK/Unicode-aware multi-line input widget with cursor navigation, command history,
//! and an interactive inline completion palette for `/commands`, sub-arguments, and `@path`.

use std::fs;
use std::path::{Path, PathBuf};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

/// Number of visible rows in the inline completion dropdown window.
pub const COMPLETION_VISIBLE_ROWS: usize = 5;

/// Canonical built-in slash command variants (single source of truth shared by `/` live
/// completion, `/help`, and `handle_slash_command` dispatch).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BuiltinSlashCommand {
    Agent,
    Compact,
    Config,
    Context,
    Copy,
    Diff,
    Export,
    Fork,
    Help,
    Import,
    Inspect,
    Memory,
    Mode,
    Model,
    New,
    Quit,
    Resume,
    Rewind,
    Send,
    Spawn,
    StatusBar,
    Tree,
    Workers,
}

/// Metadata for a built-in slash command (or alias) shown in live completion and `/help`.
#[derive(Debug, Clone, Copy)]
pub struct SlashCommandSpec {
    pub command: BuiltinSlashCommand,
    pub name: &'static str,
    pub args_hint: &'static str,
    pub description: &'static str,
    pub execute_on_enter: bool,
}

/// All built-in slash commands and aliases offered by live completion, `/help`, and command dispatch.
pub const SLASH_COMMAND_SPECS: &[SlashCommandSpec] = &[
    SlashCommandSpec {
        command: BuiltinSlashCommand::Agent,
        name: "/agent",
        args_hint: "<name|path>",
        description: "Switch focused worker *.agent script",
        execute_on_enter: false,
    },
    SlashCommandSpec {
        command: BuiltinSlashCommand::New,
        name: "/clear",
        args_hint: "",
        description: "Clear screen & start a fresh session (shows /resume hint)",
        execute_on_enter: true,
    },
    SlashCommandSpec {
        command: BuiltinSlashCommand::Compact,
        name: "/compact",
        args_hint: "",
        description: "Compact context online (Anix-Control: compact)",
        execute_on_enter: true,
    },
    SlashCommandSpec {
        command: BuiltinSlashCommand::Config,
        name: "/config",
        args_hint: "[key] [value]",
        description: "Interactive settings panel & ~/.config/anix-agent/config.json",
        execute_on_enter: true,
    },
    SlashCommandSpec {
        command: BuiltinSlashCommand::Context,
        name: "/context",
        args_hint: "",
        description: "Visualize context token usage grid by category",
        execute_on_enter: true,
    },
    SlashCommandSpec {
        command: BuiltinSlashCommand::Copy,
        name: "/copy",
        args_hint: "[code|all]",
        description: "Copy last assistant reply (or last code block) to clipboard",
        execute_on_enter: true,
    },
    SlashCommandSpec {
        command: BuiltinSlashCommand::Diff,
        name: "/diff",
        args_hint: "[staged|path]",
        description: "Inspect interactive colorized git diff overlay",
        execute_on_enter: true,
    },
    SlashCommandSpec {
        command: BuiltinSlashCommand::Quit,
        name: "/exit",
        args_hint: "",
        description: "Alias for /quit (stop all workers and exit)",
        execute_on_enter: true,
    },
    SlashCommandSpec {
        command: BuiltinSlashCommand::Export,
        name: "/export",
        args_hint: "[path.md|clipboard]",
        description: "Export active conversation branch to Markdown",
        execute_on_enter: true,
    },
    SlashCommandSpec {
        command: BuiltinSlashCommand::Fork,
        name: "/fork",
        args_hint: "[name]",
        description: "Fork active WAL into an independent branch",
        execute_on_enter: true,
    },
    SlashCommandSpec {
        command: BuiltinSlashCommand::Help,
        name: "/help",
        args_hint: "",
        description: "Show all slash commands and keybindings",
        execute_on_enter: true,
    },
    SlashCommandSpec {
        command: BuiltinSlashCommand::Import,
        name: "/import",
        args_hint: "[claude|codex|antigravity|<path>]",
        description: "Import a session from Claude Code, Codex, or Antigravity",
        execute_on_enter: true,
    },
    SlashCommandSpec {
        command: BuiltinSlashCommand::Inspect,
        name: "/inspect",
        args_hint: "<node_id>",
        description: "Inspect full payload of a Tree WAL node",
        execute_on_enter: false,
    },
    SlashCommandSpec {
        command: BuiltinSlashCommand::Memory,
        name: "/memory",
        args_hint: "[consolidate]",
        description: "Inspect & edit project rules, auto-memory, and pre-context hooks",
        execute_on_enter: true,
    },
    SlashCommandSpec {
        command: BuiltinSlashCommand::Mode,
        name: "/mode",
        args_hint: "<ask|auto|plan>",
        description: "Set shell approval policy (Shift+Tab)",
        execute_on_enter: false,
    },
    SlashCommandSpec {
        command: BuiltinSlashCommand::Model,
        name: "/model",
        args_hint: "[model_id] [effort]",
        description: "Pick/switch model & reasoning effort (/model effort <lvl>)",
        execute_on_enter: true,
    },
    SlashCommandSpec {
        command: BuiltinSlashCommand::New,
        name: "/new",
        args_hint: "",
        description: "Start a fresh session",
        execute_on_enter: true,
    },
    SlashCommandSpec {
        command: BuiltinSlashCommand::Quit,
        name: "/quit",
        args_hint: "",
        description: "Gracefully stop all workers and exit",
        execute_on_enter: true,
    },
    SlashCommandSpec {
        command: BuiltinSlashCommand::Resume,
        name: "/resume",
        args_hint: "[session_id]",
        description: "Search, rename, delete, or resume saved conversations",
        execute_on_enter: true,
    },
    SlashCommandSpec {
        command: BuiltinSlashCommand::Rewind,
        name: "/rewind",
        args_hint: "",
        description: "Rewind conversation to a prior user prompt (Esc ×2)",
        execute_on_enter: true,
    },
    SlashCommandSpec {
        command: BuiltinSlashCommand::Send,
        name: "/send",
        args_hint: "<worker> <msg>",
        description: "Send a Maildir message to a running worker",
        execute_on_enter: false,
    },
    SlashCommandSpec {
        command: BuiltinSlashCommand::Resume,
        name: "/sessions",
        args_hint: "[session_id]",
        description: "Alias for /resume (open conversation picker)",
        execute_on_enter: true,
    },
    SlashCommandSpec {
        command: BuiltinSlashCommand::Spawn,
        name: "/spawn",
        args_hint: "<agent> [prompt]",
        description: "Spawn a parallel worker with --notify callback",
        execute_on_enter: false,
    },
    SlashCommandSpec {
        command: BuiltinSlashCommand::StatusBar,
        name: "/statusbar",
        args_hint: "[vars|preset|<tpl>]",
        description: "Customize DIY variable-based status bar live",
        execute_on_enter: true,
    },
    SlashCommandSpec {
        command: BuiltinSlashCommand::Tree,
        name: "/tree",
        args_hint: "",
        description: "Browse Tree WAL branches and switch leaf",
        execute_on_enter: true,
    },
    SlashCommandSpec {
        command: BuiltinSlashCommand::Rewind,
        name: "/undo",
        args_hint: "",
        description: "Alias for /rewind (rewind conversation)",
        execute_on_enter: true,
    },
    SlashCommandSpec {
        command: BuiltinSlashCommand::Workers,
        name: "/workers",
        args_hint: "",
        description: "Manage live workers and active hold leases (^W)",
        execute_on_enter: true,
    },
];

/// Resolve a slash token (e.g. `"/exit"`, `"/clear"`, `"/undo"`) against [`SLASH_COMMAND_SPECS`].
pub fn resolve_builtin_slash(cmd: &str) -> Option<BuiltinSlashCommand> {
    SLASH_COMMAND_SPECS
        .iter()
        .find(|s| s.name.eq_ignore_ascii_case(cmd))
        .map(|s| s.command)
}

use crate::config::AgentConfig;
use crate::session::CustomSlashEntry;

/// Dynamic runtime context passed into live completion (cwd, discovered agents, active workers,
/// saved sessions, recent models, and custom commands/skills).
#[derive(Debug, Clone, Copy)]
pub struct CompletionContext<'a> {
    pub cwd: &'a Path,
    pub files: &'a [String],
    pub agents: &'a [String],
    pub workers: &'a [String],
    pub sessions: &'a [(String, String)],
    pub models: &'a [String],
    pub provider: Option<&'a str>,
    pub agent_config: Option<&'a AgentConfig>,
    pub custom_commands: &'a [CustomSlashEntry],
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
    /// `display` keeps compact `[Pasted text #N +K lines]` / `[Image #N]` chips for scrollback echo;
    /// `payload` has every paste chip expanded to its full original text for the model;
    /// `media` carries `(mime, path)` pairs for all `[Image #N]` chips still present in the prompt.
    Submit {
        display: String,
        payload: String,
        media: Vec<(String, PathBuf)>,
    },
}

/// Large pasted text block folded into an inline chip token (Claude Code style).
#[derive(Debug, Clone, PartialEq, Eq)]
struct PastedBlock {
    token: String,
    content: String,
}

/// Local image file attached via `Ctrl+V` clipboard capture or drag/drop image path paste.
#[derive(Debug, Clone, PartialEq, Eq)]
struct AttachedMedia {
    token: String,
    mime: String,
    path: PathBuf,
}

/// Multi-line text input box maintaining cursor by char offset, command history,
/// folded paste chips, attached images, and live interactive completion dropdown state.
#[derive(Debug, Default)]
pub struct InputBox {
    text: String,
    cursor_char: usize,
    history: Vec<String>,
    history_idx: Option<usize>,
    draft_before_history: String,
    completion: Option<CompletionMenu>,
    completion_dismissed: bool,
    pasted_blocks: Vec<PastedBlock>,
    next_paste_id: usize,
    attached_media: Vec<AttachedMedia>,
    next_image_id: usize,
}

impl InputBox {
    pub fn new() -> Self {
        Self::default()
    }

    /// Seed cross-session persistent history (oldest -> newest).
    pub fn load_history(&mut self, entries: Vec<String>) {
        self.history = entries;
        self.history_idx = None;
    }

    pub fn text(&self) -> &str {
        &self.text
    }

    /// Return the current draft with any folded `[Pasted text #...]` chips expanded.
    pub fn expanded_text(&self) -> String {
        self.expand_pasted(&self.text)
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
        self.pasted_blocks.clear();
        self.next_paste_id = 0;
        self.attached_media.clear();
        self.next_image_id = 0;
    }

    pub fn set_text(&mut self, value: impl Into<String>) {
        self.text = value.into();
        self.cursor_char = self.text.chars().count();
        self.history_idx = None;
        self.completion_dismissed = false;
        self.pasted_blocks.clear();
        self.next_paste_id = 0;
        self.attached_media.clear();
        self.next_image_id = 0;
    }

    /// Attach a local image file as a compact `[Image #N]` chip at the cursor position.
    pub fn attach_image(&mut self, mime: String, path: PathBuf) -> String {
        self.next_image_id += 1;
        let id = self.next_image_id;
        let token = match path.file_name().and_then(|n| n.to_str()) {
            Some(fname) if !fname.is_empty() && !fname.starts_with("clip-") => {
                format!("[Image #{id}: {fname}]")
            }
            _ => format!("[Image #{id}]"),
        };
        self.attached_media.push(AttachedMedia {
            token: token.clone(),
            mime,
            path,
        });
        self.insert_str(&token);
        token
    }

    /// Handle a terminal Bracketed Paste payload without ever triggering premature `Enter` submit.
    /// - Single or multiple dragged/pasted local paths (macOS Finder / Linux file managers / `file://` URIs):
    ///   images attach as `[Image #N: name]` chips; code/doc files & dirs clean escapes into `@relative/path`.
    /// - Large pastes (>= 5 lines or >= 400 chars) fold into `[Pasted text #N +K lines]` and expand on submit.
    pub fn handle_paste(&mut self, raw: &str, ctx: CompletionContext<'_>) -> Option<String> {
        let normalized = raw.replace("\r\n", "\n").replace('\r', "\n");
        if normalized.is_empty() {
            return None;
        }
        // Auto-detect single or multi-file dragged/pasted local paths (images + workspace files).
        if let Some(dragged) = detect_dragged_files(&normalized, ctx.cwd) {
            let needs_leading_space = self.cursor_char > 0
                && self
                    .text
                    .chars()
                    .nth(self.cursor_char - 1)
                    .is_some_and(|c| !c.is_whitespace());
            if needs_leading_space {
                self.insert_str(" ");
            }
            let mut img_tokens: Vec<String> = Vec::new();
            let mut file_refs: Vec<String> = Vec::new();
            for (idx, item) in dragged.into_iter().enumerate() {
                if idx > 0 {
                    self.insert_str(" ");
                }
                match item {
                    DraggedFile::Image { mime, path } => {
                        let tok = self.attach_image(mime, path);
                        img_tokens.push(tok);
                    }
                    DraggedFile::Path { display_ref } => {
                        self.insert_str(&display_ref);
                        file_refs.push(display_ref);
                    }
                }
            }
            // Dragged paths/images are already complete; dismiss completion so immediate `Enter`
            // submits cleanly instead of re-selecting the inserted `@path`.
            self.completion = None;
            self.completion_dismissed = true;
            let hint = match (img_tokens.len(), file_refs.len()) {
                (1, 0) => format!("attached image {}", img_tokens[0]),
                (n, 0) => format!("attached {n} images: {}", img_tokens.join(" ")),
                (0, 1) => format!("inserted path {}", file_refs[0]),
                (0, m) => format!("inserted {m} paths: {}", file_refs.join(" ")),
                (n, m) => format!("attached {n} image(s) & {m} path(s)"),
            };
            return Some(hint);
        }
        let line_count = normalized.lines().count().max(1);
        let char_count = normalized.chars().count();
        let hint = if line_count >= 5 || char_count >= 400 {
            self.next_paste_id += 1;
            let id = self.next_paste_id;
            let extra = line_count.saturating_sub(1);
            let token = if extra > 0 {
                format!("[Pasted text #{id} +{extra} lines]")
            } else {
                format!("[Pasted text #{id} {char_count} chars]")
            };
            self.pasted_blocks.push(PastedBlock {
                token: token.clone(),
                content: normalized,
            });
            self.insert_str(&token);
            Some(format!(
                "pasted #{id} ({line_count} lines, {char_count} chars — expanded on submit)"
            ))
        } else {
            self.insert_str(&normalized);
            None
        };
        self.completion_dismissed = false;
        self.refresh_completion(ctx);
        hint
    }

    fn collect_active_media(&self, display: &str) -> Vec<(String, PathBuf)> {
        self.attached_media
            .iter()
            .filter(|m| display.contains(&m.token))
            .map(|m| (m.mime.clone(), m.path.clone()))
            .collect()
    }

    fn expand_pasted(&self, input: &str) -> String {
        if self.pasted_blocks.is_empty() {
            return input.to_owned();
        }
        let mut out = input.to_owned();
        for blk in &self.pasted_blocks {
            if out.contains(&blk.token) {
                out = out.replace(&blk.token, &blk.content);
            }
        }
        out
    }

    fn insert_str(&mut self, s: &str) {
        let byte_idx = self.byte_index_of(self.cursor_char);
        self.text.insert_str(byte_idx, s);
        self.cursor_char += s.chars().count();
        self.history_idx = None;
    }

    fn try_delete_paste_chip_before_cursor(&mut self) -> bool {
        if (self.pasted_blocks.is_empty() && self.attached_media.is_empty())
            || self.cursor_char == 0
        {
            return false;
        }
        let end_byte = self.byte_index_of(self.cursor_char);
        let prefix = &self.text[..end_byte];
        for idx in 0..self.pasted_blocks.len() {
            let tok = &self.pasted_blocks[idx].token;
            if prefix.ends_with(tok.as_str()) {
                let start_byte = end_byte - tok.len();
                let tok_chars = tok.chars().count();
                self.text.replace_range(start_byte..end_byte, "");
                self.cursor_char = self.cursor_char.saturating_sub(tok_chars);
                if !self.text.contains(tok.as_str()) {
                    self.pasted_blocks.remove(idx);
                }
                self.history_idx = None;
                return true;
            }
        }
        for idx in 0..self.attached_media.len() {
            let tok = &self.attached_media[idx].token;
            if prefix.ends_with(tok.as_str()) {
                let start_byte = end_byte - tok.len();
                let tok_chars = tok.chars().count();
                self.text.replace_range(start_byte..end_byte, "");
                self.cursor_char = self.cursor_char.saturating_sub(tok_chars);
                if !self.text.contains(tok.as_str()) {
                    self.attached_media.remove(idx);
                }
                self.history_idx = None;
                return true;
            }
        }
        false
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
                    if let Some(action) = self.accept_selected_completion(true, ctx) {
                        return action;
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
            KeyCode::Enter if self.char_before_cursor() == Some('\\') => {
                // `\` + Enter newline (universal across macOS/Linux/SSH terminals!)
                self.delete_char_before();
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
                let display = std::mem::take(&mut self.text);
                let payload = self.expand_pasted(&display);
                let media = self.collect_active_media(&display);
                self.pasted_blocks.clear();
                self.next_paste_id = 0;
                self.attached_media.clear();
                self.next_image_id = 0;
                self.cursor_char = 0;
                self.history_idx = None;
                self.completion = None;
                self.completion_dismissed = false;
                self.record_history(payload.clone());
                return InputAction::Submit {
                    display,
                    payload,
                    media,
                };
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
                if !self.try_delete_paste_chip_before_cursor() {
                    self.delete_char_before();
                }
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
    ) -> Option<InputAction> {
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
            let display = std::mem::take(&mut self.text);
            let payload = self.expand_pasted(&display);
            let media = self.collect_active_media(&display);
            self.pasted_blocks.clear();
            self.next_paste_id = 0;
            self.attached_media.clear();
            self.next_image_id = 0;
            self.cursor_char = 0;
            self.history_idx = None;
            self.completion_dismissed = false;
            self.record_history(payload.clone());
            return Some(InputAction::Submit {
                display,
                payload,
                media,
            });
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

    fn char_before_cursor(&self) -> Option<char> {
        if self.cursor_char == 0 {
            return None;
        }
        self.text.chars().nth(self.cursor_char - 1)
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

    // 1. `@path` recursive fuzzy filesystem completion anywhere in the prompt.
    if let Some(rel) = token.strip_prefix('@') {
        let items = list_path_completion_items(ctx.cwd, ctx.files, rel);
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
        for custom in ctx.custom_commands {
            // Skip if a built-in command already owns this exact name.
            if SLASH_COMMAND_SPECS
                .iter()
                .any(|s| s.name.eq_ignore_ascii_case(&custom.slash_name))
            {
                continue;
            }
            let name_lower = custom.slash_name.to_ascii_lowercase();
            let bare = name_lower.strip_prefix('/').unwrap_or(&name_lower);
            let item = CompletionItem {
                label: custom.slash_name.clone(),
                insert: custom.slash_name.clone(),
                description: custom.description.clone(),
                execute_on_enter: true,
                append_space: true,
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

    // 3. Contextual sub-completions for `/mode`, `/agent`, `/spawn`, `/send`, `/resume`, `/config`.
    let before_token = prefix[..token_start].trim();
    let tokens_before: Vec<&str> = before_token.split_whitespace().collect();
    let cmd = *tokens_before.first()?;
    let second_arg = tokens_before.get(1).copied();
    let extra_arg_count = tokens_before.len().saturating_sub(1);
    let q = token.to_ascii_lowercase();

    let items: Vec<CompletionItem> = match (cmd, extra_arg_count, second_arg) {
        ("/copy", 0, _) => [
            ("code", "Copy the last fenced code block from the reply"),
            ("all", "Copy the full last assistant reply"),
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
        ("/diff", 0, _) => [
            ("all", "Show working tree + staged changes"),
            ("staged", "Show staged (--cached) changes only"),
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
        ("/export", 0, _) => [(
            "clipboard",
            "Copy formatted Markdown transcript to clipboard",
        )]
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
        ("/memory", 0, _) => [(
            "consolidate",
            "Ask model to deduplicate, verify, and prune project auto-memory",
        )]
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
        ("/import", 0, _) => [
            (
                "all",
                "Browse all Claude Code, Codex, and Antigravity sessions",
            ),
            ("claude", "Browse Claude Code sessions (~/.claude/projects)"),
            ("codex", "Browse Codex CLI sessions (~/.codex/sessions)"),
            (
                "antigravity",
                "Browse Antigravity sessions (~/.gemini/antigravity-cli/brain)",
            ),
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
        ("/mode", 0, _) => [
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
        ("/config", 0, _) => [
            (
                "bang_mode",
                "Behavior after `!cmd`: silent | attach | trigger",
            ),
            ("default_mode", "Default approval mode: ask | auto | plan"),
            (
                "auto_resume_last",
                "Auto-resume latest cwd session on boot: true | false",
            ),
            (
                "expand_details",
                "Expand thinking & tool output by default: true | false",
            ),
            ("replay_turns", "Turns replayed on /resume (default 8)"),
            (
                "notify_after_secs",
                "Bell/OSC 9 alert threshold in secs (0 disables, default 10)",
            ),
            ("bang_shell", "Shell used for `!cmd` (default $SHELL)"),
            (
                "status_bar",
                "DIY variable template (default | minimal | full | custom)",
            ),
        ]
        .into_iter()
        .filter(|(k, _)| k.starts_with(&q))
        .map(|(k, desc)| CompletionItem {
            label: k.to_owned(),
            insert: k.to_owned(),
            description: desc.to_owned(),
            execute_on_enter: false,
            append_space: true,
        })
        .collect(),
        ("/config", 1, Some("bang_mode")) => [
            ("silent", "Run `!cmd` locally only; do NOT trigger model"),
            (
                "attach",
                "Run locally; attach output to your next prompt without waking model",
            ),
            (
                "trigger",
                "Run locally and immediately wake model with command output",
            ),
        ]
        .into_iter()
        .filter(|(v, _)| v.starts_with(&q))
        .map(|(v, desc)| CompletionItem {
            label: v.to_owned(),
            insert: v.to_owned(),
            description: desc.to_owned(),
            execute_on_enter: true,
            append_space: false,
        })
        .collect(),
        ("/config", 1, Some("default_mode")) => [
            ("ask", "Auto-allow safe reads; prompt inline for writes"),
            ("auto", "Automatically approve all shell commands"),
            ("plan", "Read-only planning mode; block mutating commands"),
        ]
        .into_iter()
        .filter(|(v, _)| v.starts_with(&q))
        .map(|(v, desc)| CompletionItem {
            label: v.to_owned(),
            insert: v.to_owned(),
            description: desc.to_owned(),
            execute_on_enter: true,
            append_space: false,
        })
        .collect(),
        ("/config", 1, Some("auto_resume_last" | "expand_details")) => {
            [("true", "Enable setting"), ("false", "Disable setting")]
                .into_iter()
                .filter(|(v, _)| v.starts_with(&q))
                .map(|(v, desc)| CompletionItem {
                    label: v.to_owned(),
                    insert: v.to_owned(),
                    description: desc.to_owned(),
                    execute_on_enter: true,
                    append_space: false,
                })
                .collect()
        }
        ("/config", 1, Some("notify_after_secs")) => [
            ("0", "Disable terminal bell / OSC 9 alerts"),
            ("5", "Alert on tasks >= 5s or approval prompts"),
            ("10", "Default: alert on tasks >= 10s or approval prompts"),
            ("30", "Alert on tasks >= 30s or approval prompts"),
            ("60", "Alert on tasks >= 60s or approval prompts"),
        ]
        .into_iter()
        .filter(|(v, _)| v.starts_with(&q))
        .map(|(v, desc)| CompletionItem {
            label: v.to_owned(),
            insert: v.to_owned(),
            description: desc.to_owned(),
            execute_on_enter: true,
            append_space: false,
        })
        .collect(),
        ("/config", 1, Some("status_bar")) => [
            ("default", "Clean mode + model + git + ctx + hints"),
            ("minimal", "Ultra-compact mode + model + ctx_pct"),
            (
                "full",
                "Full telemetry bar (worker, dir, git, ctx, tokens, bang)",
            ),
        ]
        .into_iter()
        .filter(|(v, _)| v.starts_with(&q))
        .map(|(v, desc)| CompletionItem {
            label: v.to_owned(),
            insert: v.to_owned(),
            description: desc.to_owned(),
            execute_on_enter: true,
            append_space: false,
        })
        .collect(),
        ("/statusbar", 0, _) => [
            ("vars", "List all {variables} with their live values"),
            ("preset", "Switch preset: default | minimal | full"),
            (
                "edit",
                "Pre-fill current status_bar template into input to edit live",
            ),
            ("reset", "Restore default status bar template"),
        ]
        .into_iter()
        .filter(|(v, _)| v.starts_with(&q))
        .map(|(v, desc)| CompletionItem {
            label: v.to_owned(),
            insert: v.to_owned(),
            description: desc.to_owned(),
            execute_on_enter: v != "preset",
            append_space: v == "preset",
        })
        .collect(),
        ("/statusbar", 1, Some("preset")) => [
            ("default", "Clean mode + model + git + ctx + hints"),
            ("minimal", "Ultra-compact mode + model + ctx_pct"),
            (
                "full",
                "Full telemetry bar (worker, dir, git, ctx, tokens, bang)",
            ),
        ]
        .into_iter()
        .filter(|(v, _)| v.starts_with(&q))
        .map(|(v, desc)| CompletionItem {
            label: v.to_owned(),
            insert: v.to_owned(),
            description: desc.to_owned(),
            execute_on_enter: true,
            append_space: false,
        })
        .collect(),
        ("/agent", 0, _) => ctx
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
        ("/spawn", 0, _) => ctx
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
        ("/send", 0, _) => ctx
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
        ("/model", 0, _) => {
            let mut out = Vec::new();
            if "effort".starts_with(&q) {
                out.push(CompletionItem {
                    label: "effort".into(),
                    insert: "effort".into(),
                    description: "Set reasoning effort for active model".into(),
                    execute_on_enter: false,
                    append_space: true,
                });
            }
            for m in ctx
                .models
                .iter()
                .filter(|m| m.to_ascii_lowercase().contains(&q))
            {
                out.push(CompletionItem {
                    label: m.clone(),
                    insert: m.clone(),
                    description: "Recent / saved model".into(),
                    execute_on_enter: true,
                    append_space: false,
                });
            }
            out
        }
        ("/model", 1, Some(first)) => {
            let target_model = if first.eq_ignore_ascii_case("effort") {
                ctx.models.first().map(String::as_str).unwrap_or("")
            } else {
                first
            };
            let profile = match ctx.agent_config {
                Some(cfg) => cfg.resolve_effort_profile(ctx.provider, target_model),
                None => crate::config::builtin_effort_profile(ctx.provider, target_model),
            };
            let levels = profile.levels_with_current(None);
            levels
                .into_iter()
                .filter(|lvl| lvl.starts_with(&q))
                .map(|lvl| {
                    let description = if lvl == "default" {
                        "Use model/provider default reasoning effort".into()
                    } else {
                        format!("→ {}", profile.wire_preview(Some(&lvl)))
                    };
                    CompletionItem {
                        label: lvl.clone(),
                        insert: lvl,
                        description,
                        execute_on_enter: true,
                        append_space: false,
                    }
                })
                .collect()
        }
        ("/resume" | "/sessions", 0, _) => ctx
            .sessions
            .iter()
            .filter(|(id, title)| {
                id.to_ascii_lowercase().contains(&q) || title.to_ascii_lowercase().contains(&q)
            })
            .map(|(id, title)| CompletionItem {
                label: id.clone(),
                insert: id.clone(),
                description: title.clone(),
                execute_on_enter: true,
                append_space: false,
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

fn list_path_completion_items(
    cwd: &Path,
    workspace_files: &[String],
    partial: &str,
) -> Vec<CompletionItem> {
    // 1. Bare `@` or explicit directory prefix (`@src/`): show direct directory entries first.
    let direct_dir_entries = || -> Vec<(bool, String)> {
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
        let fp_lower = file_prefix.to_ascii_lowercase();
        let mut out: Vec<(bool, String)> = Vec::new();
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if !file_prefix.starts_with('.') && name.starts_with('.') {
                continue;
            }
            if name.to_ascii_lowercase().starts_with(&fp_lower) {
                let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
                let mut full = format!("{dir_prefix}{name}");
                if is_dir {
                    full.push('/');
                }
                out.push((is_dir, full));
            }
        }
        out.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
        out
    };

    if partial.is_empty() {
        let mut matches = direct_dir_entries();
        matches.truncate(24);
        return matches
            .into_iter()
            .map(|(is_dir, full)| CompletionItem {
                label: format!("@{full}"),
                insert: format!("@{full}"),
                description: if is_dir { "directory" } else { "file" }.to_owned(),
                execute_on_enter: false,
                append_space: !is_dir,
            })
            .collect();
    }

    // 2. Recursive fuzzy search across cached workspace files (when available).
    let q = partial.to_ascii_lowercase();
    let mut scored: Vec<(u8, usize, bool, String)> = Vec::new();
    let mut seen = std::collections::HashSet::new();

    // Include matching directories from direct read_dir when user is drilling down (`@src/`).
    if partial.ends_with('/') || workspace_files.is_empty() {
        for (is_dir, full) in direct_dir_entries() {
            if seen.insert(full.clone()) {
                let tier = if is_dir { 0 } else { 1 };
                let len = full.len();
                scored.push((tier, len, is_dir, full));
            }
        }
    }

    for rel in workspace_files {
        let rel_lower = rel.to_ascii_lowercase();
        let basename = rel_lower.rsplit('/').next().unwrap_or(&rel_lower);
        let tier = if basename.starts_with(&q) {
            Some(0u8)
        } else if rel_lower.starts_with(&q) {
            Some(1u8)
        } else if basename.contains(&q) {
            Some(2u8)
        } else if rel_lower.contains(&q) {
            Some(3u8)
        } else if is_fuzzy_subsequence(&q, basename) {
            Some(4u8)
        } else if is_fuzzy_subsequence(&q, &rel_lower) {
            Some(5u8)
        } else {
            None
        };
        if let Some(t) = tier
            && seen.insert(rel.clone())
        {
            scored.push((t, rel.len(), false, rel.clone()));
        }
    }

    scored.sort_by(|a, b| {
        a.0.cmp(&b.0)
            .then_with(|| a.1.cmp(&b.1))
            .then_with(|| a.3.cmp(&b.3))
    });
    scored.truncate(24);

    scored
        .into_iter()
        .map(|(_, _, is_dir, full)| {
            let desc = if is_dir {
                "directory".to_owned()
            } else if let Some((dir, _)) = full.rsplit_once('/') {
                format!("{dir}/")
            } else {
                "file".to_owned()
            };
            CompletionItem {
                label: format!("@{full}"),
                insert: format!("@{full}"),
                description: desc,
                execute_on_enter: false,
                append_space: !is_dir,
            }
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

#[derive(Debug, Clone, PartialEq, Eq)]
enum DraggedFile {
    Image { mime: String, path: PathBuf },
    Path { display_ref: String },
}

/// Split a shell-escaped or quoted drag-and-drop paste string into individual candidate paths
/// (handles macOS Finder `\ ` escaping, single/double quotes, newline-separated drops, and `file://` URIs).
fn split_dragged_path_tokens(raw: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut cur = String::new();
    let mut quote: Option<char> = None;
    let mut chars = raw.chars().peekable();

    while let Some(c) = chars.next() {
        match quote {
            Some(q) => {
                if c == q {
                    quote = None;
                } else if q == '"' && c == '\\' {
                    if let Some(next) = chars.next() {
                        cur.push(next);
                    }
                } else {
                    cur.push(c);
                }
            }
            None => match c {
                '\'' | '"' if cur.is_empty() => {
                    quote = Some(c);
                }
                '\\' => {
                    if let Some(&next) = chars.peek()
                        && (next.is_whitespace()
                            || matches!(
                                next,
                                '(' | ')' | '[' | ']' | '\'' | '"' | '&' | '#' | '$' | ';' | '\\'
                            ))
                    {
                        cur.push(next);
                        chars.next();
                    } else {
                        cur.push(c);
                    }
                }
                ch if ch.is_whitespace() => {
                    if !cur.is_empty() {
                        tokens.push(std::mem::take(&mut cur));
                    }
                }
                _ => cur.push(c),
            },
        }
    }
    if !cur.is_empty() {
        tokens.push(cur);
    }
    tokens
}

fn decode_file_uri(raw: &str) -> Option<String> {
    let rest = raw.strip_prefix("file://")?;
    let path_part = rest.strip_prefix("localhost").unwrap_or(rest);
    let mut bytes = Vec::with_capacity(path_part.len());
    let raw_bytes = path_part.as_bytes();
    let mut i = 0usize;
    while i < raw_bytes.len() {
        if raw_bytes[i] == b'%'
            && i + 2 < raw_bytes.len()
            && let Ok(hex_str) = std::str::from_utf8(&raw_bytes[i + 1..i + 3])
            && let Ok(val) = u8::from_str_radix(hex_str, 16)
        {
            bytes.push(val);
            i += 3;
            continue;
        }
        bytes.push(raw_bytes[i]);
        i += 1;
    }
    String::from_utf8(bytes).ok()
}

fn image_mime_for_path(path: &Path) -> Option<&'static str> {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())?
        .to_ascii_lowercase();
    match ext.as_str() {
        "png" => Some("image/png"),
        "jpg" | "jpeg" => Some("image/jpeg"),
        "webp" => Some("image/webp"),
        "gif" => Some("image/gif"),
        _ => None,
    }
}

/// Detect whether a paste payload consists exclusively of 1..=32 existing local file/dir paths
/// (drag-and-drop from macOS Finder, Linux file managers, or IDE file trees).
fn detect_dragged_files(raw: &str, cwd: &Path) -> Option<Vec<DraggedFile>> {
    let tokens = split_dragged_path_tokens(raw);
    if tokens.is_empty() || tokens.len() > 32 {
        return None;
    }
    let cwd_canon = fs::canonicalize(cwd).unwrap_or_else(|_| cwd.to_path_buf());
    let mut out = Vec::with_capacity(tokens.len());

    for tok in tokens {
        let cleaned = decode_file_uri(&tok).unwrap_or(tok);
        let looks_like_explicit_path = cleaned.starts_with('/')
            || cleaned.starts_with("~/")
            || cleaned.starts_with("./")
            || cleaned.starts_with("../");
        let candidate = if let Some(rest) = cleaned.strip_prefix("~/") {
            let home = std::env::var_os("HOME").map(PathBuf::from)?;
            home.join(rest)
        } else {
            let p = PathBuf::from(&cleaned);
            if p.is_absolute() { p } else { cwd.join(p) }
        };

        if !candidate.exists() {
            return None;
        }

        if candidate.is_file()
            && let Some(mime) = image_mime_for_path(&candidate)
        {
            let canonical = fs::canonicalize(&candidate).unwrap_or(candidate);
            out.push(DraggedFile::Image {
                mime: mime.to_owned(),
                path: canonical,
            });
            continue;
        }

        // For non-image files/dirs, require an explicit path prefix (`/`, `~/`, `./`, `../`)
        // so pasting ordinary words like `README` or `src` never gets rewritten unexpectedly.
        if !looks_like_explicit_path {
            return None;
        }
        let is_dir = candidate.is_dir();
        let canonical = fs::canonicalize(&candidate).unwrap_or(candidate);
        let rel_display = match canonical.strip_prefix(&cwd_canon) {
            Ok(rel) if !rel.as_os_str().is_empty() => rel.display().to_string(),
            _ => canonical.display().to_string(),
        };
        let suffix = if is_dir && !rel_display.ends_with('/') {
            "/"
        } else {
            ""
        };
        let display_ref = if rel_display.contains(' ') {
            format!("@\"{rel_display}{suffix}\"")
        } else {
            format!("@{rel_display}{suffix}")
        };
        out.push(DraggedFile::Path { display_ref });
    }

    Some(out)
}

/// Detect whether a single pasted token is a path to an existing local image file
/// (handles quotes and shell-escaped spaces from macOS Finder drag-and-drop).
pub fn detect_image_file_path(raw: &str, cwd: &Path) -> Option<(String, PathBuf)> {
    let mut dragged = detect_dragged_files(raw, cwd)?;
    if dragged.len() != 1 {
        return None;
    }
    match dragged.pop()? {
        DraggedFile::Image { mime, path } => Some((mime, path)),
        DraggedFile::Path { .. } => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_ctx(cwd: &Path) -> CompletionContext<'_> {
        CompletionContext {
            cwd,
            files: &[],
            agents: &[],
            workers: &[],
            sessions: &[],
            models: &[],
            provider: None,
            agent_config: None,
            custom_commands: &[],
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
            InputAction::Submit {
                display: "你好\n!".into(),
                payload: "你好\n!".into(),
                media: Vec::new(),
            }
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
            InputAction::Submit {
                display: "/mode auto".into(),
                payload: "/mode auto".into(),
                media: Vec::new(),
            }
        );

        // Aliases (`/exit`, `/clear`, `/undo`) share the same single source of truth as dispatch.
        for (typed, expected, cmd) in [
            ("/ex", "/exit", BuiltinSlashCommand::Quit),
            ("/cl", "/clear", BuiltinSlashCommand::New),
            ("/un", "/undo", BuiltinSlashCommand::Rewind),
        ] {
            input.clear();
            for ch in typed.chars() {
                input.handle_key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE), ctx);
            }
            let menu = input.completion().expect("alias completion opens");
            assert_eq!(menu.items[0].insert, expected);
            assert_eq!(resolve_builtin_slash(expected), Some(cmd));
        }

        // Bracketed paste: short multi-line paste inserts inline without submitting.
        input.clear();
        assert!(input.handle_paste("line1\r\nline2\rline3", ctx).is_none());
        assert_eq!(input.text(), "line1\nline2\nline3");

        // Large paste (>= 5 lines) folds into a compact chip, expands on submit, and deletes atomically.
        input.clear();
        input.set_text("check: ");
        let big = "a\nb\nc\nd\ne\nf";
        let hint = input.handle_paste(big, ctx);
        assert!(hint.is_some());
        assert_eq!(input.text(), "check: [Pasted text #1 +5 lines]");
        assert_eq!(input.expanded_text(), format!("check: {big}"));
        assert_eq!(
            input.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), ctx),
            InputAction::Submit {
                display: "check: [Pasted text #1 +5 lines]".into(),
                payload: format!("check: {big}"),
                media: Vec::new(),
            }
        );

        // Atomic backspace deletes the whole paste chip in one stroke.
        input.clear();
        input.set_text("x ");
        let _ = input.handle_paste(big, ctx);
        assert_eq!(input.text(), "x [Pasted text #1 +5 lines]");
        input.handle_key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE), ctx);
        assert_eq!(input.text(), "x ");

        // Attached image chip carries media on submit and deletes atomically on Backspace.
        input.clear();
        let img_path = PathBuf::from("/tmp/clip-1234.png");
        let tok = input.attach_image("image/png".into(), img_path.clone());
        assert_eq!(tok, "[Image #1]");
        input.handle_key(KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE), ctx);
        input.handle_key(KeyEvent::new(KeyCode::Char('?'), KeyModifiers::NONE), ctx);
        assert_eq!(
            input.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), ctx),
            InputAction::Submit {
                display: "[Image #1] ?".into(),
                payload: "[Image #1] ?".into(),
                media: vec![("image/png".into(), img_path)],
            }
        );
    }

    #[test]
    fn recursive_fuzzy_at_completion_and_multi_file_drag_drop_work() {
        let tmp = std::env::temp_dir().join(format!("anix-drop-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(tmp.join("src")).unwrap();
        let img1 = tmp.join("My Shot.png");
        let img2 = tmp.join("chart 2.jpg");
        let code = tmp.join("src/app.rs");
        fs::write(&img1, b"png").unwrap();
        fs::write(&img2, b"jpg").unwrap();
        fs::write(&code, b"fn main() {}").unwrap();

        let files = vec![
            "Cargo.toml".to_owned(),
            "src/app.rs".to_owned(),
            "src/markdown.rs".to_owned(),
            "src/wal_view.rs".to_owned(),
            "tests/app_helper.rs".to_owned(),
        ];
        let ctx = CompletionContext {
            cwd: &tmp,
            files: &files,
            agents: &[],
            workers: &[],
            sessions: &[],
            models: &[],
            provider: None,
            agent_config: None,
            custom_commands: &[],
        };

        // `@app` matches `src/app.rs` first via recursive basename prefix ranking.
        let mut input = InputBox::new();
        for ch in "@app".chars() {
            input.handle_key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE), ctx);
        }
        let menu = input.completion().expect("recursive @app menu opens");
        assert_eq!(menu.items[0].insert, "@src/app.rs");

        // `@mrd` fuzzy-matches `src/markdown.rs`.
        input.clear();
        for ch in "@mrd".chars() {
            input.handle_key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE), ctx);
        }
        let menu = input.completion().expect("fuzzy @mrd menu opens");
        assert_eq!(menu.items[0].insert, "@src/markdown.rs");

        // Multi-file drag-and-drop: 2 images (escaped space + single-quoted) + 1 code file.
        input.clear();
        let escaped_img1 = img1.display().to_string().replace(' ', "\\ ");
        let drop_payload = format!("{} '{}' {}", escaped_img1, img2.display(), code.display());
        let hint = input.handle_paste(&drop_payload, ctx);
        assert!(
            hint.as_deref()
                .unwrap_or("")
                .contains("2 image(s) & 1 path(s)")
        );
        assert_eq!(
            input.text(),
            "[Image #1: My Shot.png] [Image #2: chart 2.jpg] @src/app.rs"
        );
        let action = input.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), ctx);
        match action {
            InputAction::Submit { media, display, .. } => {
                assert_eq!(media.len(), 2);
                assert_eq!(media[0].0, "image/png");
                assert_eq!(media[1].0, "image/jpeg");
                assert!(display.ends_with("@src/app.rs"));
            }
            InputAction::None => panic!("expected submit"),
        }

        let _ = fs::remove_dir_all(&tmp);
    }
}
