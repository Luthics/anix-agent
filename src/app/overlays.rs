//! Interactive inline overlays: `/resume`, `/config`, `/rewind`, `Ctrl+O` transcript,
//! `/diff`, `Ctrl+R` history search, `/model`, `/tree`, `/workers`, and `?` shortcuts.

use std::io;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

use super::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ResumeTab {
    Project,
    All,
}

pub(super) enum ResumeMode {
    Browse,
    Rename { buf: String },
    ConfirmDelete,
}

pub(super) struct ResumePicker {
    pub(super) tab: ResumeTab,
    pub(super) all_items: Vec<SessionSummary>,
    pub(super) query: String,
    pub(super) selected: usize,
    pub(super) mode: ResumeMode,
    pub(super) status_msg: Option<String>,
}

impl ResumePicker {
    pub(super) fn filtered_indices(&self) -> Vec<usize> {
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

    pub(super) fn clamp_selection(&mut self) {
        let len = self.filtered_indices().len();
        if len == 0 {
            self.selected = 0;
        } else if self.selected >= len {
            self.selected = len - 1;
        }
    }

    pub(super) fn selected_item(&self) -> Option<&SessionSummary> {
        let filtered = self.filtered_indices();
        let raw_idx = *filtered.get(self.selected)?;
        self.all_items.get(raw_idx)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ConfigTab {
    Settings,
    Status,
    Usage,
}

pub(super) struct ConfigPicker {
    pub(super) tab: ConfigTab,
    pub(super) selected: usize,
    pub(super) filter: String,
    pub(super) status_msg: Option<String>,
}

pub(super) struct RewindPicker {
    pub(super) points: Vec<RewindPoint>,
    pub(super) selected: usize,
    pub(super) scroll: usize,
    pub(super) choosing_action: bool,
    pub(super) action_idx: usize,
}

pub(super) struct TranscriptViewer {
    pub(super) lines: Vec<Line<'static>>,
    pub(super) scroll: usize,
    pub(super) entry_count: usize,
    pub(super) status_msg: Option<String>,
}

pub(super) struct DiffFileEntry {
    pub(super) path: String,
    pub(super) status: String,
    pub(super) added: usize,
    pub(super) removed: usize,
    pub(super) lines: Vec<Line<'static>>,
    pub(super) raw_diff: String,
}

pub(super) struct DiffViewer {
    pub(super) scope_label: String,
    pub(super) files: Vec<DiffFileEntry>,
    pub(super) selected_file: usize,
    pub(super) scroll: usize,
    pub(super) status_msg: Option<String>,
}

pub(super) struct HistorySearchPicker {
    pub(super) query: String,
    pub(super) selected: usize,
    pub(super) prev_draft: String,
}

pub(super) struct ModelPicker {
    pub(super) query: String,
    pub(super) selected: usize,
    pub(super) effort: Option<String>,
    pub(super) status_msg: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum HookState {
    Enabled,
    Masked,
    Missing,
}

#[derive(Debug, Clone)]
pub(super) enum MemoryEntryKind {
    Instruction {
        source_label: String,
        path: std::path::PathBuf,
        exists: bool,
        bytes: usize,
        lines: usize,
    },
    MemoryIndex {
        path: std::path::PathBuf,
        exists: bool,
        bytes: usize,
        lines: usize,
    },
    MemoryTopic {
        name: String,
        path: std::path::PathBuf,
        bytes: usize,
        lines: usize,
        mem_type: Option<String>,
    },
    PreContextHook {
        name: String,
        hook_path: std::path::PathBuf,
        state: HookState,
        summary: String,
    },
}

pub(super) struct MemoryPicker {
    pub(super) memory_dir: std::path::PathBuf,
    pub(super) items: Vec<MemoryEntryKind>,
    pub(super) selected: usize,
    pub(super) scroll: usize,
    pub(super) confirm_delete: bool,
    pub(super) status_msg: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ImportTab {
    All,
    Claude,
    Codex,
    Antigravity,
}

impl ImportTab {
    pub(super) fn next(self) -> Self {
        match self {
            Self::All => Self::Claude,
            Self::Claude => Self::Codex,
            Self::Codex => Self::Antigravity,
            Self::Antigravity => Self::All,
        }
    }

    pub(super) fn source_filter(self) -> Option<ImportSource> {
        match self {
            Self::All => None,
            Self::Claude => Some(ImportSource::Claude),
            Self::Codex => Some(ImportSource::Codex),
            Self::Antigravity => Some(ImportSource::Antigravity),
        }
    }
}

pub(super) struct ImportPicker {
    pub(super) tab: ImportTab,
    pub(super) all_items: Vec<ExternalSessionSummary>,
    pub(super) query: String,
    pub(super) selected: usize,
    pub(super) status_msg: Option<String>,
}

impl ImportPicker {
    pub(super) fn filtered_indices(&self) -> Vec<usize> {
        let q = self.query.trim().to_ascii_lowercase();
        let src_filter = self.tab.source_filter();
        self.all_items
            .iter()
            .enumerate()
            .filter(|(_, item)| {
                if let Some(src) = src_filter
                    && item.source != src
                {
                    return false;
                }
                if q.is_empty() {
                    return true;
                }
                item.title.to_ascii_lowercase().contains(&q)
                    || item.id.to_ascii_lowercase().contains(&q)
                    || item.source.label().to_ascii_lowercase().contains(&q)
                    || item
                        .cwd
                        .as_deref()
                        .is_some_and(|c| c.to_ascii_lowercase().contains(&q))
            })
            .map(|(idx, _)| idx)
            .collect()
    }

    pub(super) fn clamp_selection(&mut self) {
        let len = self.filtered_indices().len();
        if len == 0 {
            self.selected = 0;
        } else if self.selected >= len {
            self.selected = len - 1;
        }
    }

    pub(super) fn selected_item(&self) -> Option<&ExternalSessionSummary> {
        let filtered = self.filtered_indices();
        let raw_idx = *filtered.get(self.selected)?;
        self.all_items.get(raw_idx)
    }
}

pub(super) enum Overlay {
    None,
    Shortcuts,
    Config(ConfigPicker),
    Rewind(RewindPicker),
    Transcript(TranscriptViewer),
    Diff(DiffViewer),
    Memory(MemoryPicker),
    Import(ImportPicker),
    HistorySearch(HistorySearchPicker),
    Models(ModelPicker),
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

impl Workbench {
    pub(super) fn open_import_overlay(
        &mut self,
        terminal: &mut UiTerminal,
        initial_tab: ImportTab,
    ) -> io::Result<()> {
        let items = discover_all_sessions(&self.store.home_dir, &self.store.cwd, None);
        if items.is_empty() {
            return self.push_notice(
                terminal,
                Color::Yellow,
                "no importable sessions found in ~/.claude, ~/.codex, or ~/.gemini/antigravity-cli (you can also pass `/import <path.jsonl>`)",
            );
        }
        let mut picker = ImportPicker {
            tab: initial_tab,
            all_items: items,
            query: String::new(),
            selected: 0,
            status_msg: None,
        };
        picker.clamp_selection();
        self.overlay = Overlay::Import(picker);
        Ok(())
    }

    pub(super) fn open_memory_overlay(&mut self) {
        let picker = self.scan_memory_picker(0, None);
        self.overlay = Overlay::Memory(picker);
    }

    pub(super) fn scan_memory_picker(
        &self,
        keep_selected: usize,
        status_msg: Option<String>,
    ) -> MemoryPicker {
        let cwd = &self.store.cwd;
        let config_dir = anix_config_dir();
        let memory_dir = anix_memory_dir(cwd);
        let hooks_dir = anix_hooks_pre_context_dir();
        let project_root = resolve_shared_project_root(cwd);

        let mut items: Vec<MemoryEntryKind> = Vec::new();
        let mut seen_paths = std::collections::HashSet::new();

        // 1. Discovered global + project instruction files (matching `10-agents-md` order).
        let mut search_dirs: Vec<(bool, std::path::PathBuf)> = Vec::new();
        if config_dir.is_dir() {
            search_dirs.push((true, config_dir.clone()));
        }
        let cwd_canon = std::fs::canonicalize(cwd).unwrap_or_else(|_| cwd.clone());
        let mut chain = Vec::new();
        let mut cur = Some(cwd_canon.as_path());
        while let Some(d) = cur {
            chain.push(d.to_path_buf());
            if d == project_root {
                break;
            }
            cur = d.parent();
        }
        chain.reverse();
        for d in chain {
            if !search_dirs.iter().any(|(_, p)| p == &d) {
                search_dirs.push((false, d));
            }
        }

        let mut has_project_agents_md = false;
        for (is_global, dir) in search_dirs {
            for fname in [
                "AGENTS.md",
                "AGENTS.local.md",
                "CLAUDE.md",
                "CLAUDE.local.md",
            ] {
                let p = dir.join(fname);
                if !p.is_file() {
                    continue;
                }
                let canon = std::fs::canonicalize(&p).unwrap_or_else(|_| p.clone());
                if !seen_paths.insert(canon.clone()) {
                    continue;
                }
                if !is_global && fname == "AGENTS.md" {
                    has_project_agents_md = true;
                }
                let raw = std::fs::read_to_string(&canon).unwrap_or_default();
                let scope = if is_global { "global" } else { "project" };
                items.push(MemoryEntryKind::Instruction {
                    source_label: format!("{fname} ({scope})"),
                    path: canon,
                    exists: true,
                    bytes: raw.len(),
                    lines: raw.lines().count(),
                });
            }
        }
        if !has_project_agents_md {
            items.push(MemoryEntryKind::Instruction {
                source_label: "AGENTS.md (project)".into(),
                path: project_root.join("AGENTS.md"),
                exists: false,
                bytes: 0,
                lines: 0,
            });
        }

        // 2. Auto Memory (`MEMORY.md` index + topic files in `<memory_dir>`).
        let index_path = memory_dir.join("MEMORY.md");
        let (idx_exists, idx_bytes, idx_lines) = if index_path.is_file() {
            let raw = std::fs::read_to_string(&index_path).unwrap_or_default();
            (true, raw.len(), raw.lines().count())
        } else {
            (false, 0, 0)
        };
        items.push(MemoryEntryKind::MemoryIndex {
            path: index_path,
            exists: idx_exists,
            bytes: idx_bytes,
            lines: idx_lines,
        });

        if let Ok(rd) = std::fs::read_dir(&memory_dir) {
            let mut topics: Vec<std::path::PathBuf> = rd
                .flatten()
                .map(|e| e.path())
                .filter(|p| {
                    p.is_file()
                        && p.extension().and_then(|s| s.to_str()) == Some("md")
                        && p.file_name().and_then(|s| s.to_str()) != Some("MEMORY.md")
                })
                .collect();
            topics.sort();
            for tp in topics {
                let name = tp
                    .file_name()
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_default();
                let raw = std::fs::read_to_string(&tp).unwrap_or_default();
                let mem_type = parse_memory_frontmatter_type(&raw);
                items.push(MemoryEntryKind::MemoryTopic {
                    name,
                    path: tp,
                    bytes: raw.len(),
                    lines: raw.lines().count(),
                    mem_type,
                });
            }
        }

        // 3. `pre-context.d/` hooks status (standard Layer 2 hooks + any custom hooks).
        let mut seen_hooks = std::collections::HashSet::new();
        let standard_hooks: &[(&str, &str)] = &[
            ("10-agents-md", "AGENTS.md + CLAUDE.md project instructions"),
            ("15-memory", "Auto Memory (MEMORY.md 200-line index)"),
            ("20-skills", "Discovered SKILL.md index"),
            ("30-tips", "POSIX runtime tips index"),
        ];
        for &(name, summary) in standard_hooks {
            seen_hooks.insert(name.to_owned());
            let hp = hooks_dir.join(name);
            items.push(MemoryEntryKind::PreContextHook {
                name: name.to_owned(),
                state: inspect_hook_state(&hp),
                hook_path: hp,
                summary: summary.to_owned(),
            });
        }
        if let Ok(rd) = std::fs::read_dir(&hooks_dir) {
            let mut extras: Vec<String> = rd
                .flatten()
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .filter(|n| !n.starts_with('.') && !seen_hooks.contains(n))
                .collect();
            extras.sort();
            for name in extras {
                let hp = hooks_dir.join(&name);
                items.push(MemoryEntryKind::PreContextHook {
                    name,
                    state: inspect_hook_state(&hp),
                    hook_path: hp,
                    summary: "custom pre-context.d hook".into(),
                });
            }
        }

        let selected = if items.is_empty() {
            0
        } else {
            keep_selected.min(items.len() - 1)
        };
        MemoryPicker {
            memory_dir,
            items,
            selected,
            scroll: 0,
            confirm_delete: false,
            status_msg,
        }
    }

    pub(super) fn submit_memory_consolidation(
        &mut self,
        terminal: &mut UiTerminal,
    ) -> io::Result<()> {
        let mem_dir = anix_memory_dir(&self.store.cwd);
        let prompt = format!(
            "Please inspect and consolidate the project Auto Memory directory at `{}`:\n\
1. Read `MEMORY.md` and all `*.md` topic files inside it.\n\
2. Merge duplicates, verify technical notes against the current repository, and delete obsolete or easily code-derivable entries.\n\
3. Keep `MEMORY.md` as a concise index (1 line per topic linking to `<topic>.md`, well under 200 lines) and report a brief summary of what you updated or pruned.",
            mem_dir.display()
        );
        self.submit_user_input(terminal, "/memory consolidate".into(), prompt, Vec::new())
    }

    pub(super) fn toggle_pre_context_hook(
        &self,
        name: &str,
        hook_path: &std::path::Path,
        current: HookState,
    ) -> Result<String, String> {
        use std::os::unix::fs::symlink;

        if let Some(parent) = hook_path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        match current {
            HookState::Enabled => {
                // Only mask with `/dev/null` when safe (symlink or replaceable); refuse clobbering
                // non-symlink custom script silently.
                let is_symlink = std::fs::symlink_metadata(hook_path)
                    .map(|m| m.file_type().is_symlink())
                    .unwrap_or(false);
                if !is_symlink {
                    return Err(format!(
                        "`{name}` is a custom file (not a symlink); edit or mask manually"
                    ));
                }
                let _ = std::fs::remove_file(hook_path);
                symlink("/dev/null", hook_path).map_err(|e| e.to_string())?;
                Ok(format!("masked `{name}` (/dev/null) — press c to /compact"))
            }
            HookState::Masked | HookState::Missing => {
                let is_standard =
                    matches!(name, "10-agents-md" | "15-memory" | "20-skills" | "30-tips");
                if !is_standard {
                    return Err(format!(
                        "cannot auto-restore non-builtin hook `{name}`; symlink your script manually"
                    ));
                }
                let Some(hook_bin) = resolve_anix_hook_bin(&self.anix_bin) else {
                    return Err("could not locate `anix-hook` binary in libexec/anix/".into());
                };
                if hook_path.exists() || std::fs::symlink_metadata(hook_path).is_ok() {
                    let _ = std::fs::remove_file(hook_path);
                }
                symlink(&hook_bin, hook_path).map_err(|e| e.to_string())?;
                Ok(format!("enabled `{name}` — press c to /compact"))
            }
        }
    }

    pub(super) fn open_diff_overlay(
        &mut self,
        terminal: &mut UiTerminal,
        raw_arg: &str,
    ) -> io::Result<()> {
        let width = self.term_width(terminal).saturating_sub(2).max(28);
        let cwd = &self.store.cwd;
        let is_git = std::process::Command::new("git")
            .args(["rev-parse", "--is-inside-work-tree"])
            .current_dir(cwd)
            .output()
            .is_ok_and(|o| o.status.success());
        if !is_git {
            return self.push_notice(terminal, Color::Yellow, "not inside a git repository");
        }

        let arg = raw_arg.trim();
        let staged_only = arg.eq_ignore_ascii_case("staged") || arg == "--cached";
        let path_filter = if arg.is_empty() || arg.eq_ignore_ascii_case("all") || staged_only {
            None
        } else {
            Some(arg.to_ascii_lowercase())
        };

        let status_out = std::process::Command::new("git")
            .args(["status", "--porcelain=v1", "-uall"])
            .current_dir(cwd)
            .output();
        let Ok(status_out) = status_out else {
            return self.push_notice(terminal, Color::Red, "failed to invoke `git status`");
        };
        if !status_out.status.success() {
            return self.push_notice(
                terminal,
                Color::Red,
                format!(
                    "git status failed: {}",
                    String::from_utf8_lossy(&status_out.stderr).trim()
                ),
            );
        }

        let has_head = std::process::Command::new("git")
            .args(["rev-parse", "--verify", "HEAD"])
            .current_dir(cwd)
            .output()
            .is_ok_and(|o| o.status.success());

        let status_text = String::from_utf8_lossy(&status_out.stdout);
        let mut files: Vec<DiffFileEntry> = Vec::new();
        for raw_line in status_text.lines() {
            if raw_line.len() < 4 {
                continue;
            }
            let x = raw_line.as_bytes()[0] as char;
            let y = raw_line.as_bytes()[1] as char;
            let raw_path = raw_line[3..].trim();
            let path = raw_path
                .split(" -> ")
                .last()
                .unwrap_or(raw_path)
                .trim_matches('"')
                .to_owned();

            if staged_only && (x == ' ' || x == '?') {
                continue;
            }
            if let Some(ref f) = path_filter
                && !path.to_ascii_lowercase().contains(f)
            {
                continue;
            }

            let status = if x == '?' && y == '?' {
                "??".to_owned()
            } else {
                let s = format!("{x}{y}");
                s.trim().to_owned()
            };

            let raw_diff = if x == '?' && y == '?' {
                synthesize_untracked_diff(&cwd.join(&path), &path)
            } else {
                let mut cmd = std::process::Command::new("git");
                cmd.arg("diff");
                if staged_only {
                    cmd.arg("--cached");
                } else if has_head {
                    cmd.arg("HEAD");
                }
                cmd.args(["--no-color", "--", &path]).current_dir(cwd);
                match cmd.output() {
                    Ok(o) => String::from_utf8_lossy(&o.stdout).into_owned(),
                    Err(_) => String::new(),
                }
            };

            if raw_diff.trim().is_empty() {
                continue;
            }

            let mut added = 0usize;
            let mut removed = 0usize;
            let mut lines: Vec<Line<'static>> = Vec::new();
            for l in raw_diff.lines() {
                if l.starts_with('+') && !l.starts_with("+++") {
                    added += 1;
                } else if l.starts_with('-') && !l.starts_with("---") {
                    removed += 1;
                }
                lines.extend(render_diff_line("  ", "  ", l, width));
            }
            files.push(DiffFileEntry {
                path,
                status,
                added,
                removed,
                lines,
                raw_diff,
            });
        }

        if files.is_empty() {
            let scope = if staged_only {
                "staged index"
            } else {
                "working tree"
            };
            return self.push_notice(
                terminal,
                Color::Green,
                format!("✓ {scope} clean (no matching git diff)"),
            );
        }

        self.overlay = Overlay::Diff(DiffViewer {
            scope_label: if staged_only {
                "staged".into()
            } else if let Some(f) = path_filter {
                format!("filter:{f}")
            } else {
                "working tree".into()
            },
            files,
            selected_file: 0,
            scroll: 0,
            status_msg: None,
        });
        Ok(())
    }

    pub(super) fn open_transcript_overlay(&mut self, terminal: &mut UiTerminal) -> io::Result<()> {
        let width = self.term_width(terminal).saturating_sub(2).max(28);
        let mut entries: Vec<DetailEntry> = self
            .workers
            .get(&self.focused_worker)
            .map(|st| st.detail_log.clone())
            .unwrap_or_default();
        if entries.is_empty() {
            let (wal_path, leaf) = self.focused_wal_and_leaf();
            entries = load_wal_detail_entries(&wal_path, leaf, 40);
        }
        if entries.is_empty() {
            self.expand_details = !self.expand_details;
            self.hint_line = Some(format!(
                "no thinking/tool details yet · auto-expand: {}",
                if self.expand_details { "on" } else { "off" }
            ));
            return Ok(());
        }

        let mut lines: Vec<Line<'static>> = Vec::new();
        for (idx, entry) in entries.iter().enumerate() {
            if idx > 0 {
                lines.push(Line::default());
            }
            match entry {
                DetailEntry::Thought { ts, text } => {
                    lines.push(Line::from(vec![
                        Span::styled("○ ", Style::default().fg(Color::Cyan)),
                        Span::styled(
                            "Thought ",
                            Style::default()
                                .fg(Color::White)
                                .add_modifier(Modifier::BOLD),
                        ),
                        Span::styled(
                            format!(
                                "({} chars · {})",
                                text.chars().count(),
                                format_relative_time(ts)
                            ),
                            Style::default().fg(Color::DarkGray),
                        ),
                    ]));
                    for (li, l) in text.trim().lines().enumerate() {
                        let pfx = if li == 0 { "  ⎿ " } else { "    " };
                        lines.extend(wrap_prefixed_lines(
                            l,
                            pfx,
                            Style::default().fg(Color::DarkGray),
                            Style::default()
                                .fg(Color::Gray)
                                .add_modifier(Modifier::ITALIC),
                            width,
                        ));
                    }
                }
                DetailEntry::Tool {
                    ts,
                    name,
                    cmd,
                    exit_code,
                    output,
                } => {
                    let rc = exit_code.unwrap_or(0);
                    let dot_color = if rc == 0 { Color::Green } else { Color::Red };
                    let mut hdr = vec![
                        Span::styled("● ", Style::default().fg(dot_color)),
                        Span::styled(
                            format!("{name}("),
                            Style::default()
                                .fg(Color::White)
                                .add_modifier(Modifier::BOLD),
                        ),
                        Span::styled(
                            truncate_single_line(cmd, width.saturating_sub(24)),
                            Style::default().fg(Color::Yellow),
                        ),
                        Span::styled(
                            ")",
                            Style::default()
                                .fg(Color::White)
                                .add_modifier(Modifier::BOLD),
                        ),
                    ];
                    if rc != 0 {
                        hdr.push(Span::styled(
                            format!(" [rc={rc}]"),
                            Style::default().fg(Color::Red),
                        ));
                    }
                    hdr.push(Span::styled(
                        format!(" · {}", format_relative_time(ts)),
                        Style::default().fg(Color::DarkGray),
                    ));
                    lines.push(Line::from(hdr));
                    if output.trim().is_empty() {
                        lines.push(Line::from(Span::styled(
                            "  ⎿ (no output)",
                            Style::default().fg(Color::DarkGray),
                        )));
                    } else {
                        let is_diff = looks_like_unified_diff(output);
                        for (li, l) in output.lines().enumerate() {
                            let pfx = if li == 0 { "  ⎿ " } else { "    " };
                            if is_diff {
                                lines.extend(render_diff_line(pfx, "    ", l, width));
                            } else {
                                lines.extend(wrap_prefixed_lines(
                                    l,
                                    pfx,
                                    Style::default().fg(Color::DarkGray),
                                    Style::default().fg(Color::White),
                                    width,
                                ));
                            }
                        }
                    }
                }
            }
        }

        let visible_body = 11usize;
        let scroll = lines.len().saturating_sub(visible_body);
        self.overlay = Overlay::Transcript(TranscriptViewer {
            lines,
            scroll,
            entry_count: entries.len(),
            status_msg: None,
        });
        Ok(())
    }

    pub(super) fn open_history_search(&mut self) {
        if self.history_entries.is_empty() {
            self.hint_line = Some("no saved prompt history yet".into());
            return;
        }
        let prev_draft = self.input.expanded_text();
        self.overlay = Overlay::HistorySearch(HistorySearchPicker {
            query: String::new(),
            selected: 0,
            prev_draft,
        });
    }

    pub(super) fn filtered_history_indices(&self, query: &str) -> Vec<usize> {
        let q = query.trim().to_ascii_lowercase();
        let mut seen = HashSet::new();
        let mut out = Vec::new();
        for (idx, entry) in self.history_entries.iter().enumerate().rev() {
            if !seen.insert(entry.text.as_str()) {
                continue;
            }
            if q.is_empty() || entry.text.to_ascii_lowercase().contains(&q) {
                out.push(idx);
            }
        }
        out
    }

    pub(super) fn handle_overlay_key(
        &mut self,
        terminal: &mut UiTerminal,
        key: KeyEvent,
    ) -> io::Result<()> {
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
            Overlay::Transcript(mut viewer) => {
                let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
                let page = 10usize;
                let max_scroll = viewer.lines.len().saturating_sub(11);
                match key.code {
                    KeyCode::Esc | KeyCode::Char('q') => {}
                    KeyCode::Up | KeyCode::Char('k') => {
                        viewer.scroll = viewer.scroll.saturating_sub(1);
                        self.overlay = Overlay::Transcript(viewer);
                    }
                    KeyCode::Down | KeyCode::Char('j') => {
                        viewer.scroll = (viewer.scroll + 1).min(max_scroll);
                        self.overlay = Overlay::Transcript(viewer);
                    }
                    KeyCode::PageUp | KeyCode::Left => {
                        viewer.scroll = viewer.scroll.saturating_sub(page);
                        self.overlay = Overlay::Transcript(viewer);
                    }
                    KeyCode::PageDown | KeyCode::Right | KeyCode::Char(' ') => {
                        viewer.scroll = (viewer.scroll + page).min(max_scroll);
                        self.overlay = Overlay::Transcript(viewer);
                    }
                    KeyCode::Char('u') if ctrl => {
                        viewer.scroll = viewer.scroll.saturating_sub(page);
                        self.overlay = Overlay::Transcript(viewer);
                    }
                    KeyCode::Char('d') if ctrl => {
                        viewer.scroll = (viewer.scroll + page).min(max_scroll);
                        self.overlay = Overlay::Transcript(viewer);
                    }
                    KeyCode::Home | KeyCode::Char('g') => {
                        viewer.scroll = 0;
                        self.overlay = Overlay::Transcript(viewer);
                    }
                    KeyCode::End | KeyCode::Char('G') => {
                        viewer.scroll = max_scroll;
                        self.overlay = Overlay::Transcript(viewer);
                    }
                    KeyCode::Char('e') => {
                        self.expand_details = !self.expand_details;
                        self.agent_config.expand_details = self.expand_details;
                        let _ = self.agent_config.save_to(&self.agent_config_path);
                        viewer.status_msg = Some(format!(
                            "auto-expand default -> {}",
                            if self.expand_details { "on" } else { "off" }
                        ));
                        self.overlay = Overlay::Transcript(viewer);
                    }
                    _ => {
                        self.overlay = Overlay::Transcript(viewer);
                    }
                }
            }
            Overlay::Diff(mut viewer) => {
                let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
                let page = 12usize;
                let cur_len = viewer
                    .files
                    .get(viewer.selected_file)
                    .map(|f| f.lines.len())
                    .unwrap_or(0);
                let max_scroll = cur_len.saturating_sub(12);
                match key.code {
                    KeyCode::Esc | KeyCode::Char('q') => {}
                    KeyCode::Up | KeyCode::Char('k') => {
                        viewer.scroll = viewer.scroll.saturating_sub(1);
                        self.overlay = Overlay::Diff(viewer);
                    }
                    KeyCode::Down | KeyCode::Char('j') => {
                        viewer.scroll = (viewer.scroll + 1).min(max_scroll);
                        self.overlay = Overlay::Diff(viewer);
                    }
                    KeyCode::PageUp => {
                        viewer.scroll = viewer.scroll.saturating_sub(page);
                        self.overlay = Overlay::Diff(viewer);
                    }
                    KeyCode::PageDown | KeyCode::Char(' ') => {
                        viewer.scroll = (viewer.scroll + page).min(max_scroll);
                        self.overlay = Overlay::Diff(viewer);
                    }
                    KeyCode::Char('u') if ctrl => {
                        viewer.scroll = viewer.scroll.saturating_sub(page);
                        self.overlay = Overlay::Diff(viewer);
                    }
                    KeyCode::Char('d') if ctrl => {
                        viewer.scroll = (viewer.scroll + page).min(max_scroll);
                        self.overlay = Overlay::Diff(viewer);
                    }
                    KeyCode::Home | KeyCode::Char('g') => {
                        viewer.scroll = 0;
                        self.overlay = Overlay::Diff(viewer);
                    }
                    KeyCode::End | KeyCode::Char('G') => {
                        viewer.scroll = max_scroll;
                        self.overlay = Overlay::Diff(viewer);
                    }
                    KeyCode::Right | KeyCode::Tab | KeyCode::Char('l') | KeyCode::Char('n') => {
                        if !viewer.files.is_empty() {
                            viewer.selected_file = (viewer.selected_file + 1) % viewer.files.len();
                            viewer.scroll = 0;
                            viewer.status_msg = None;
                        }
                        self.overlay = Overlay::Diff(viewer);
                    }
                    KeyCode::Left | KeyCode::BackTab | KeyCode::Char('h') | KeyCode::Char('p') => {
                        if !viewer.files.is_empty() {
                            viewer.selected_file = if viewer.selected_file == 0 {
                                viewer.files.len() - 1
                            } else {
                                viewer.selected_file - 1
                            };
                            viewer.scroll = 0;
                            viewer.status_msg = None;
                        }
                        self.overlay = Overlay::Diff(viewer);
                    }
                    KeyCode::Char('c') => {
                        if let Some(file) = viewer.files.get(viewer.selected_file) {
                            match write_clipboard_text(&file.raw_diff) {
                                Ok(()) => {
                                    viewer.status_msg = Some(format!(
                                        "copied {} diff ({} lines)",
                                        file.path,
                                        file.raw_diff.lines().count()
                                    ));
                                }
                                Err(err) => {
                                    viewer.status_msg = Some(format!("copy failed: {err}"));
                                }
                            }
                        }
                        self.overlay = Overlay::Diff(viewer);
                    }
                    KeyCode::Char('r') => {
                        if let Some(file) = viewer.files.get(viewer.selected_file) {
                            let prompt = format!(
                                "Please review the current git diff for @{} ({}, +{} -{}) and point out any bugs, regressions, or improvements.",
                                file.path, viewer.scope_label, file.added, file.removed
                            );
                            self.submit_user_input(terminal, prompt.clone(), prompt, Vec::new())?;
                        }
                    }
                    KeyCode::Char('R') => {
                        let file_list: String = viewer
                            .files
                            .iter()
                            .take(20)
                            .map(|f| format!("@{}", f.path))
                            .collect::<Vec<_>>()
                            .join(" ");
                        let prompt = format!(
                            "Please review all current {} git changes ({}) and point out any bugs, edge cases, or improvements.",
                            viewer.scope_label, file_list
                        );
                        self.submit_user_input(terminal, prompt.clone(), prompt, Vec::new())?;
                    }
                    _ => {
                        self.overlay = Overlay::Diff(viewer);
                    }
                }
            }
            Overlay::Memory(mut picker) => {
                let body_rows = 11usize;
                match key.code {
                    KeyCode::Esc => {
                        if picker.confirm_delete {
                            picker.confirm_delete = false;
                            picker.status_msg = None;
                            self.overlay = Overlay::Memory(picker);
                        }
                    }
                    KeyCode::Char('q') => {}
                    KeyCode::Up | KeyCode::Char('k') => {
                        picker.confirm_delete = false;
                        if !picker.items.is_empty() {
                            picker.selected = if picker.selected == 0 {
                                picker.items.len() - 1
                            } else {
                                picker.selected - 1
                            };
                            if picker.selected < picker.scroll {
                                picker.scroll = picker.selected;
                            } else if picker.selected >= picker.scroll + body_rows {
                                picker.scroll = picker.selected + 1 - body_rows;
                            }
                        }
                        self.overlay = Overlay::Memory(picker);
                    }
                    KeyCode::Down | KeyCode::Char('j') => {
                        picker.confirm_delete = false;
                        if !picker.items.is_empty() {
                            picker.selected = (picker.selected + 1) % picker.items.len();
                            if picker.selected < picker.scroll {
                                picker.scroll = picker.selected;
                            } else if picker.selected >= picker.scroll + body_rows {
                                picker.scroll = picker.selected + 1 - body_rows;
                            }
                        }
                        self.overlay = Overlay::Memory(picker);
                    }
                    KeyCode::Enter | KeyCode::Char('e') => {
                        picker.confirm_delete = false;
                        let sel = picker.selected;
                        match picker.items.get(sel).cloned() {
                            Some(MemoryEntryKind::Instruction { path, exists, .. }) => {
                                let starter = if exists {
                                    None
                                } else {
                                    Some(
                                        "# Project Instructions\n\n\
- Build & test commands:\n\
- Architecture boundaries:\n\
- Coding conventions:\n",
                                    )
                                };
                                let msg = match self
                                    .edit_path_in_external_editor(terminal, &path, starter)?
                                {
                                    Ok(ed) => {
                                        format!("saved via `{ed}` — press c to /compact & reload")
                                    }
                                    Err(err) => err,
                                };
                                self.overlay =
                                    Overlay::Memory(self.scan_memory_picker(sel, Some(msg)));
                            }
                            Some(MemoryEntryKind::MemoryIndex { path, exists, .. }) => {
                                let starter = if exists {
                                    None
                                } else {
                                    Some(
                                        "# Project Auto Memory Index\n\n\
<!-- Keep concise (1 line per topic linking to <topic>.md; <= 200 lines) -->\n",
                                    )
                                };
                                let msg = match self
                                    .edit_path_in_external_editor(terminal, &path, starter)?
                                {
                                    Ok(ed) => {
                                        format!("saved via `{ed}` — press c to /compact & reload")
                                    }
                                    Err(err) => err,
                                };
                                self.overlay =
                                    Overlay::Memory(self.scan_memory_picker(sel, Some(msg)));
                            }
                            Some(MemoryEntryKind::MemoryTopic { path, .. }) => {
                                let msg = match self
                                    .edit_path_in_external_editor(terminal, &path, None)?
                                {
                                    Ok(ed) => format!("saved topic via `{ed}`"),
                                    Err(err) => err,
                                };
                                self.overlay =
                                    Overlay::Memory(self.scan_memory_picker(sel, Some(msg)));
                            }
                            Some(MemoryEntryKind::PreContextHook {
                                name,
                                hook_path,
                                state,
                                ..
                            }) => {
                                let msg =
                                    match self.toggle_pre_context_hook(&name, &hook_path, state) {
                                        Ok(m) | Err(m) => m,
                                    };
                                self.overlay =
                                    Overlay::Memory(self.scan_memory_picker(sel, Some(msg)));
                            }
                            None => {
                                self.overlay = Overlay::Memory(picker);
                            }
                        }
                    }
                    KeyCode::Char('t') => {
                        picker.confirm_delete = false;
                        let sel = picker.selected;
                        if let Some(MemoryEntryKind::PreContextHook {
                            name,
                            hook_path,
                            state,
                            ..
                        }) = picker.items.get(sel).cloned()
                        {
                            let msg = match self.toggle_pre_context_hook(&name, &hook_path, state) {
                                Ok(m) | Err(m) => m,
                            };
                            self.overlay = Overlay::Memory(self.scan_memory_picker(sel, Some(msg)));
                        } else {
                            picker.status_msg =
                                Some("select a pre-context hook row to toggle".into());
                            self.overlay = Overlay::Memory(picker);
                        }
                    }
                    KeyCode::Char('n') => {
                        picker.confirm_delete = false;
                        let sel = picker.selected;
                        let short = &uuid::Uuid::new_v4().simple().to_string()[..6];
                        let topic_path = picker.memory_dir.join(format!("note-{short}.md"));
                        let starter = "\
---
name: note
description: Brief one-line summary for MEMORY.md index
type: project
---

";
                        let msg = match self.edit_path_in_external_editor(
                            terminal,
                            &topic_path,
                            Some(starter),
                        )? {
                            Ok(ed) => format!(
                                "created {} via `{ed}`",
                                topic_path
                                    .file_name()
                                    .map(|s| s.to_string_lossy().into_owned())
                                    .unwrap_or_default()
                            ),
                            Err(err) => err,
                        };
                        self.overlay = Overlay::Memory(self.scan_memory_picker(sel, Some(msg)));
                    }
                    KeyCode::Char('d') => {
                        let sel = picker.selected;
                        match picker.items.get(sel).cloned() {
                            Some(MemoryEntryKind::MemoryTopic { name, path, .. }) => {
                                if picker.confirm_delete {
                                    let msg = match std::fs::remove_file(&path) {
                                        Ok(()) => format!("deleted `{name}`"),
                                        Err(e) => format!("delete failed: {e}"),
                                    };
                                    self.overlay =
                                        Overlay::Memory(self.scan_memory_picker(sel, Some(msg)));
                                } else {
                                    picker.confirm_delete = true;
                                    picker.status_msg = Some(format!(
                                        "press `d` again to delete `{name}` (Esc cancels)"
                                    ));
                                    self.overlay = Overlay::Memory(picker);
                                }
                            }
                            Some(MemoryEntryKind::MemoryIndex {
                                path, exists: true, ..
                            }) => {
                                if picker.confirm_delete {
                                    let msg = match std::fs::remove_file(&path) {
                                        Ok(()) => {
                                            "deleted `MEMORY.md` — press c to /compact".into()
                                        }
                                        Err(e) => format!("delete failed: {e}"),
                                    };
                                    self.overlay =
                                        Overlay::Memory(self.scan_memory_picker(sel, Some(msg)));
                                } else {
                                    picker.confirm_delete = true;
                                    picker.status_msg = Some(
                                        "press `d` again to delete `MEMORY.md` (Esc cancels)"
                                            .into(),
                                    );
                                    self.overlay = Overlay::Memory(picker);
                                }
                            }
                            _ => {
                                picker.status_msg =
                                    Some("only auto-memory files can be deleted here".into());
                                self.overlay = Overlay::Memory(picker);
                            }
                        }
                    }
                    KeyCode::Char('r') => {
                        self.submit_memory_consolidation(terminal)?;
                    }
                    KeyCode::Char('c') => {
                        if self.session_persisted && self.workers.contains_key(&self.focused_worker)
                        {
                            self.handle_slash_command(terminal, "/compact")?;
                        } else {
                            self.push_notice(
                                terminal,
                                Color::Cyan,
                                "fresh draft session — updated rules & memory will load automatically on your first message",
                            )?;
                        }
                    }
                    _ => {
                        self.overlay = Overlay::Memory(picker);
                    }
                }
            }
            Overlay::Import(mut picker) => {
                let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
                match key.code {
                    KeyCode::Esc => {}
                    KeyCode::Tab => {
                        picker.tab = picker.tab.next();
                        picker.selected = 0;
                        picker.status_msg = None;
                        picker.clamp_selection();
                        self.overlay = Overlay::Import(picker);
                    }
                    KeyCode::Up => {
                        let len = picker.filtered_indices().len();
                        if len > 0 {
                            picker.selected = if picker.selected == 0 {
                                len - 1
                            } else {
                                picker.selected - 1
                            };
                        }
                        self.overlay = Overlay::Import(picker);
                    }
                    KeyCode::Down => {
                        let len = picker.filtered_indices().len();
                        if len > 0 {
                            picker.selected = (picker.selected + 1) % len;
                        }
                        self.overlay = Overlay::Import(picker);
                    }
                    KeyCode::Left => {
                        let len = picker.filtered_indices().len();
                        if len > 0 {
                            let cur_page = picker.selected / RESUME_PAGE_SIZE;
                            picker.selected = cur_page.saturating_sub(1) * RESUME_PAGE_SIZE;
                        }
                        self.overlay = Overlay::Import(picker);
                    }
                    KeyCode::Right => {
                        let len = picker.filtered_indices().len();
                        if len > 0 {
                            let next =
                                ((picker.selected / RESUME_PAGE_SIZE) + 1) * RESUME_PAGE_SIZE;
                            if next < len {
                                picker.selected = next;
                            }
                        }
                        self.overlay = Overlay::Import(picker);
                    }
                    KeyCode::Backspace => {
                        picker.query.pop();
                        picker.selected = 0;
                        picker.status_msg = None;
                        picker.clamp_selection();
                        self.overlay = Overlay::Import(picker);
                    }
                    KeyCode::Char('u') if ctrl => {
                        picker.query.clear();
                        picker.selected = 0;
                        picker.status_msg = None;
                        picker.clamp_selection();
                        self.overlay = Overlay::Import(picker);
                    }
                    KeyCode::Char(c) if !ctrl => {
                        picker.query.push(c);
                        picker.selected = 0;
                        picker.status_msg = None;
                        picker.clamp_selection();
                        self.overlay = Overlay::Import(picker);
                    }
                    KeyCode::Enter => {
                        if let Some(item) = picker.selected_item().cloned() {
                            self.import_from_path(terminal, &item.path, Some(item.source))?;
                        } else {
                            picker.status_msg = Some("no matching session selected".into());
                            self.overlay = Overlay::Import(picker);
                        }
                    }
                    _ => {
                        self.overlay = Overlay::Import(picker);
                    }
                }
            }
            Overlay::HistorySearch(mut picker) => {
                let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
                let matches = self.filtered_history_indices(&picker.query);
                match key.code {
                    KeyCode::Esc => {
                        self.input.set_text(picker.prev_draft);
                    }
                    KeyCode::Char('r') if ctrl => {
                        if !matches.is_empty() {
                            picker.selected = (picker.selected + 1) % matches.len();
                        }
                        self.overlay = Overlay::HistorySearch(picker);
                    }
                    KeyCode::Down => {
                        if !matches.is_empty() && picker.selected + 1 < matches.len() {
                            picker.selected += 1;
                        }
                        self.overlay = Overlay::HistorySearch(picker);
                    }
                    KeyCode::Up => {
                        picker.selected = picker.selected.saturating_sub(1);
                        self.overlay = Overlay::HistorySearch(picker);
                    }
                    KeyCode::Enter | KeyCode::Tab => {
                        if let Some(&raw_idx) = matches.get(picker.selected)
                            && let Some(entry) = self.history_entries.get(raw_idx)
                        {
                            self.input.set_text(entry.text.clone());
                            self.hint_line =
                                Some("loaded from history (Enter to send, or edit)".into());
                        }
                    }
                    KeyCode::Backspace => {
                        picker.query.pop();
                        picker.selected = 0;
                        self.overlay = Overlay::HistorySearch(picker);
                    }
                    KeyCode::Char('u') if ctrl => {
                        picker.query.clear();
                        picker.selected = 0;
                        self.overlay = Overlay::HistorySearch(picker);
                    }
                    KeyCode::Char(c) if !ctrl => {
                        picker.query.push(c);
                        picker.selected = 0;
                        self.overlay = Overlay::HistorySearch(picker);
                    }
                    _ => {
                        self.overlay = Overlay::HistorySearch(picker);
                    }
                }
            }
            Overlay::Models(mut picker) => {
                let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
                let items = self.filtered_models(&picker.query);
                let highlighted_model = || {
                    items
                        .get(picker.selected)
                        .cloned()
                        .or_else(|| {
                            let q = picker.query.trim();
                            (!q.is_empty()).then(|| q.to_owned())
                        })
                        .unwrap_or_else(|| self.focused_model_and_effort().0)
                };
                match key.code {
                    KeyCode::Esc => {}
                    KeyCode::Up => {
                        let prev = picker.selected;
                        picker.selected = picker.selected.saturating_sub(1);
                        if picker.selected != prev {
                            self.sync_model_picker_effort(&mut picker);
                        }
                        self.overlay = Overlay::Models(picker);
                    }
                    KeyCode::Down => {
                        let prev = picker.selected;
                        if !items.is_empty() && picker.selected + 1 < items.len() {
                            picker.selected += 1;
                        }
                        if picker.selected != prev {
                            self.sync_model_picker_effort(&mut picker);
                        }
                        self.overlay = Overlay::Models(picker);
                    }
                    KeyCode::Left | KeyCode::Right => {
                        let model_for_slider = highlighted_model();
                        let profile = self.resolve_effort_profile_for(&model_for_slider);
                        let levels = profile.levels_with_current(picker.effort.as_deref());
                        if !levels.is_empty() {
                            let cur_label = picker.effort.as_deref().unwrap_or("default");
                            let cur_idx = levels
                                .iter()
                                .position(|l| l.eq_ignore_ascii_case(cur_label))
                                .unwrap_or(0);
                            let next_idx = if key.code == KeyCode::Left {
                                (cur_idx + levels.len() - 1) % levels.len()
                            } else {
                                (cur_idx + 1) % levels.len()
                            };
                            picker.effort = normalize_effort_input(&levels[next_idx]);
                            picker.status_msg = None;
                        }
                        self.overlay = Overlay::Models(picker);
                    }
                    KeyCode::Enter => {
                        let chosen = items
                            .get(picker.selected)
                            .cloned()
                            .or_else(|| {
                                let q = picker.query.trim();
                                (!q.is_empty()).then(|| q.to_owned())
                            })
                            .or_else(|| {
                                let (active, _) = self.focused_model_and_effort();
                                (!active.is_empty() && active != "default").then_some(active)
                            });
                        if let Some(model_id) = chosen {
                            self.switch_focused_model(
                                terminal,
                                &model_id,
                                Some(picker.effort.as_deref()),
                            )?;
                        }
                    }
                    KeyCode::F(4) | KeyCode::Delete => {
                        if let Some(m) = items.get(picker.selected)
                            && self.agent_config.remove_recent_model(m)
                        {
                            let _ = self.agent_config.save_to(&self.agent_config_path);
                            picker.status_msg = Some(format!("removed `{m}` from recent models"));
                            let new_len = self.filtered_models(&picker.query).len();
                            if picker.selected >= new_len {
                                picker.selected = new_len.saturating_sub(1);
                            }
                            self.sync_model_picker_effort(&mut picker);
                        }
                        self.overlay = Overlay::Models(picker);
                    }
                    KeyCode::Backspace => {
                        picker.query.pop();
                        picker.selected = 0;
                        self.sync_model_picker_effort(&mut picker);
                        self.overlay = Overlay::Models(picker);
                    }
                    KeyCode::Char('u') if ctrl => {
                        picker.query.clear();
                        picker.selected = 0;
                        self.sync_model_picker_effort(&mut picker);
                        self.overlay = Overlay::Models(picker);
                    }
                    KeyCode::Char(c) if !ctrl => {
                        picker.query.push(c);
                        picker.selected = 0;
                        self.sync_model_picker_effort(&mut picker);
                        self.overlay = Overlay::Models(picker);
                    }
                    _ => {
                        self.overlay = Overlay::Models(picker);
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
                            self.respawn_worker(
                                terminal,
                                &wid,
                                None,
                                Some(Some(lid)),
                                None,
                                None,
                                None,
                            )?;
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

    pub(super) fn config_setting_rows(&self) -> Vec<(&'static str, String)> {
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
            (
                "Notify after (secs)",
                if self.agent_config.notify_after_secs == 0 {
                    "off".to_owned()
                } else {
                    format!("{}s", self.agent_config.notify_after_secs)
                },
            ),
            ("Status bar preset", preset_name),
        ]
    }

    pub(super) fn filtered_config_indices(&self, filter: &str) -> Vec<usize> {
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

    pub(super) fn toggle_config_setting(&mut self, idx: usize) -> String {
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
                self.persist_meta_if_materialized();
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
            5 => {
                self.agent_config.notify_after_secs = match self.agent_config.notify_after_secs {
                    0 => 5,
                    1..=5 => 10,
                    6..=10 => 30,
                    11..=30 => 60,
                    _ => 0,
                };
                format!(
                    "notify_after_secs = {}",
                    self.agent_config.notify_after_secs
                )
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

    pub(super) fn open_rewind_picker(&mut self, terminal: &mut UiTerminal) -> io::Result<()> {
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

    pub(super) fn apply_rewind_action(
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
                self.respawn_worker(terminal, &wid, None, Some(Some(target)), None, None, None)?;
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
                self.respawn_worker(terminal, &wid, None, Some(Some(target)), None, None, None)?;
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
}

fn inspect_hook_state(path: &std::path::Path) -> HookState {
    if let Ok(target) = std::fs::read_link(path)
        && target == std::path::Path::new("/dev/null")
    {
        return HookState::Masked;
    }
    match std::fs::metadata(path) {
        Ok(meta) if meta.is_file() && meta.len() == 0 => HookState::Masked,
        Ok(meta) if meta.is_file() => HookState::Enabled,
        _ => HookState::Missing,
    }
}
