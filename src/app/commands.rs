//! Slash command dispatcher, `!cmd` local shell runner, session switching, worker spawning,
//! Tree WAL fork/inspect/context views, external `$EDITOR`, and `Ctrl+Z` suspend.

use std::io;
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::thread;

use crossterm::event::{DisableBracketedPaste, EnableBracketedPaste};
use crossterm::terminal::{disable_raw_mode, enable_raw_mode};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

use super::*;

impl Workbench {
    pub(super) fn known_models(&self) -> Vec<String> {
        let mut out = Vec::new();
        let mut push_unique = |m: &str| {
            let t = m.trim();
            if !t.is_empty() && t != "default" && t != "-" && !out.iter().any(|x| x == t) {
                out.push(t.to_owned());
            }
        };
        if let Some(st) = self.workers.get(&self.focused_worker) {
            push_unique(&st.handle.current_model);
        }
        if let Some(ref m) = self.meta.model {
            push_unique(m);
        }
        push_unique(&self.draft_model);
        for m in &self.agent_config.recent_models {
            push_unique(m);
        }
        out
    }

    pub(super) fn filtered_models(&self, query: &str) -> Vec<String> {
        let q = query.trim().to_ascii_lowercase();
        self.known_models()
            .into_iter()
            .filter(|m| q.is_empty() || m.to_ascii_lowercase().contains(&q))
            .collect()
    }

    pub(super) fn focused_provider(&self) -> Option<String> {
        if let Some(st) = self.workers.get(&self.focused_worker) {
            if !st.handle.current_provider.is_empty() {
                return Some(st.handle.current_provider.clone());
            }
            if let Some(ref p) = st.handle.spec.provider {
                return Some(p.clone());
            }
        }
        self.meta.provider.clone().or_else(|| {
            let p = self.draft_provider.trim();
            (!p.is_empty() && p != "-").then(|| p.to_owned())
        })
    }

    pub(super) fn resolve_effort_profile_for(&self, model: &str) -> ResolvedEffortProfile {
        let prov = self.focused_provider();
        self.agent_config
            .resolve_effort_profile(prov.as_deref(), model)
    }

    pub(super) fn focused_model_and_effort(&self) -> (String, Option<String>) {
        if let Some(st) = self.workers.get(&self.focused_worker) {
            let model = if !st.handle.current_model.is_empty() {
                st.handle.current_model.clone()
            } else {
                st.handle
                    .spec
                    .model
                    .clone()
                    .unwrap_or_else(|| self.draft_model.clone())
            };
            let effort = st.handle.spec.reasoning_effort.clone();
            (model, effort)
        } else {
            let model = self
                .meta
                .model
                .clone()
                .unwrap_or_else(|| self.draft_model.clone());
            let effort = self.meta.reasoning_effort.clone().or_else(|| {
                self.agent_config
                    .effort_for_model(&model)
                    .map(ToOwned::to_owned)
            });
            (model, effort)
        }
    }

    pub(super) fn saved_effort_for_model(&self, model: &str) -> Option<String> {
        let (active_model, active_effort) = self.focused_model_and_effort();
        if model == active_model {
            active_effort
        } else {
            self.agent_config
                .effort_for_model(model)
                .map(ToOwned::to_owned)
        }
    }

    pub(super) fn sync_model_picker_effort(&self, picker: &mut ModelPicker) {
        let items = self.filtered_models(&picker.query);
        let highlighted = items.get(picker.selected).cloned().or_else(|| {
            let q = picker.query.trim();
            (!q.is_empty()).then(|| q.to_owned())
        });
        if let Some(m) = highlighted {
            picker.effort = self.saved_effort_for_model(&m);
        } else {
            let (_, active_effort) = self.focused_model_and_effort();
            picker.effort = active_effort;
        }
    }

    pub(super) fn open_model_picker(&mut self) {
        let (active_model, active_effort) = self.focused_model_and_effort();
        let known = self.known_models();
        let selected = known.iter().position(|m| m == &active_model).unwrap_or(0);
        let mut picker = ModelPicker {
            query: String::new(),
            selected,
            effort: active_effort,
            status_msg: None,
        };
        self.sync_model_picker_effort(&mut picker);
        self.overlay = Overlay::Models(picker);
    }

    pub(super) fn switch_focused_model(
        &mut self,
        terminal: &mut UiTerminal,
        model_id: &str,
        effort_override: Option<Option<&str>>,
    ) -> io::Result<()> {
        let target = model_id.trim();
        if target.is_empty() {
            return Ok(());
        }
        let (active_model, _) = self.focused_model_and_effort();
        let same_model = active_model == target;

        let mut config_dirty = self.agent_config.record_recent_model(target);
        let final_effort = match effort_override {
            Some(explicit) => {
                let norm = explicit.and_then(normalize_effort_input);
                if self.agent_config.set_model_effort(target, norm.as_deref()) {
                    config_dirty = true;
                }
                norm
            }
            None => self
                .agent_config
                .effort_for_model(target)
                .map(ToOwned::to_owned),
        };
        if config_dirty {
            let _ = self.agent_config.save_to(&self.agent_config_path);
        }

        let wid = self.focused_worker.clone();
        self.respawn_worker(
            terminal,
            &wid,
            None,
            None,
            Some(Some(target.to_owned())),
            None,
            Some(final_effort.clone()),
        )?;
        let profile = self.resolve_effort_profile_for(target);
        let wire = profile.wire_preview(final_effort.as_deref());
        let msg = if same_model {
            let eff_label = final_effort.as_deref().unwrap_or("default");
            if final_effort.is_some() {
                format!("◈ [{wid}] {target} reasoning effort -> {eff_label} ({wire})")
            } else {
                format!("◈ [{wid}] {target} reasoning effort -> default")
            }
        } else if let Some(ref eff) = final_effort {
            format!("◈ [{wid}] switched model -> {target} (effort: {eff} · {wire})")
        } else {
            format!("◈ [{wid}] switched model -> {target}")
        };
        self.push_notice(terminal, Color::Cyan, msg)
    }

    pub(super) fn switch_session(
        &mut self,
        terminal: &mut UiTerminal,
        new_dir: PathBuf,
        new_meta: SessionMeta,
    ) -> io::Result<()> {
        self.shutdown_all_workers();
        self.session_dir = new_dir;
        self.policy = new_meta.approval_policy;
        self.meta = new_meta;
        self.session_persisted = true;
        self.focused_worker = "main".into();
        self.refresh_draft_defaults();
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

    pub(super) fn submit_user_input(
        &mut self,
        terminal: &mut UiTerminal,
        display: String,
        payload_text: String,
        media: Vec<(String, PathBuf)>,
    ) -> io::Result<()> {
        if media.is_empty() && payload_text.starts_with('/') {
            return self.handle_slash_command(terminal, &payload_text);
        }
        if media.is_empty() && payload_text.starts_with('!') {
            return self.run_bang_command(terminal, &payload_text);
        }

        let effective_display = if display.trim().is_empty() && !media.is_empty() {
            format!("[{} image(s)]", media.len())
        } else {
            display
        };
        let effective_payload_text = if payload_text.trim().is_empty() && !media.is_empty() {
            "Please inspect the attached image(s).".to_owned()
        } else {
            payload_text
        };

        // Update session title before materializing so `meta.json` lands on disk with the real title.
        if self.meta.title == "New session" {
            self.meta.title = truncate_single_line(&effective_display, 48);
            self.meta.updated_at = now_rfc3339();
            self.persist_meta_if_materialized();
        }

        let width = self.term_width(terminal);
        let prefix = if self.focused_worker == "main" {
            "❯ ".to_owned()
        } else {
            format!("❯ [{}] ", self.focused_worker)
        };
        let mut lines = wrap_prefixed_lines(
            &effective_display,
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
            effective_payload_text
        } else {
            let ctx_block = std::mem::take(&mut self.pending_bang_context).join("\n\n");
            format!("{ctx_block}\n\n{effective_payload_text}")
        };

        let wid = self.focused_worker.clone();
        // Lazily materialize draft session & spawn `main` on the very first real user prompt.
        if wid == "main" {
            self.ensure_main_worker_ready(terminal)?;
        }
        if let Some(st) = self.workers.get_mut(&wid) {
            let was_busy = st.handle.busy;
            st.awaiting_user_cycle = true;
            if st.busy_since.is_none() {
                st.busy_since = Some(Instant::now());
            }
            if let Err(err) = st.handle.send_prompt_with_media(&payload, "user", &media) {
                self.push_notice(
                    terminal,
                    Color::Red,
                    format!("failed delivering prompt to `{wid}`: {err}"),
                )?;
            } else if was_busy {
                st.queued_steering
                    .push(truncate_single_line(&effective_display, 64));
            }
        }
        Ok(())
    }

    pub(super) fn run_bang_command(
        &mut self,
        terminal: &mut UiTerminal,
        raw: &str,
    ) -> io::Result<()> {
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

    pub(super) fn handle_bang_done(
        &mut self,
        terminal: &mut UiTerminal,
        res: BangResult,
    ) -> io::Result<()> {
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
        let is_diff = looks_like_unified_diff(&res.output);
        let limit = if self.expand_details { 80 } else { 12 };
        for (idx, l) in out_lines.iter().take(limit).enumerate() {
            let pfx = if idx == 0 { "  ⎿ " } else { "    " };
            if is_diff {
                lines.extend(render_diff_line(pfx, "    ", l, width));
            } else {
                lines.extend(wrap_prefixed_lines(
                    l,
                    pfx,
                    Style::default().fg(Color::DarkGray),
                    Style::default().fg(Color::Gray),
                    width,
                ));
            }
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

        if let Some(st) = self.workers.get_mut(&self.focused_worker) {
            st.push_detail(DetailEntry::Tool {
                ts: now_rfc3339(),
                name: "!".into(),
                cmd: res.cmd.clone(),
                exit_code: Some(res.exit_code),
                output: res.output.clone(),
            });
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
                if self.meta.title == "New session" {
                    self.meta.title = truncate_single_line(&format!("! {}", res.cmd), 48);
                    self.meta.updated_at = now_rfc3339();
                    self.persist_meta_if_materialized();
                }
                if wid == "main" {
                    self.ensure_main_worker_ready(terminal)?;
                }
                if let Some(st) = self.workers.get_mut(&wid) {
                    let was_busy = st.handle.busy;
                    st.awaiting_user_cycle = true;
                    if st.busy_since.is_none() {
                        st.busy_since = Some(Instant::now());
                    }
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
                    } else if was_busy {
                        st.queued_steering
                            .push(truncate_single_line(&format!("! {}", res.cmd), 64));
                    }
                }
            }
        }
        Ok(())
    }

    pub(super) fn handle_slash_command(
        &mut self,
        terminal: &mut UiTerminal,
        raw: &str,
    ) -> io::Result<()> {
        let mut parts = raw.split_whitespace();
        let cmd = parts.next().unwrap_or("");
        let rest = raw[cmd.len()..].trim();

        let Some(builtin) = resolve_builtin_slash(cmd) else {
            // Check discovered custom commands (`commands/*.md`) and skills (`skills/*/SKILL.md`).
            if let Some(entry) = self
                .custom_commands
                .iter()
                .find(|e| e.slash_name.eq_ignore_ascii_case(cmd))
                .cloned()
            {
                return self.execute_custom_slash_entry(terminal, &entry, rest);
            }
            return self.push_notice(
                terminal,
                Color::Yellow,
                format!("unknown command `{cmd}` (try /help)"),
            );
        };

        match builtin {
            BuiltinSlashCommand::Copy => {
                let mode = rest.trim().to_ascii_lowercase();
                if !matches!(mode.as_str(), "" | "all" | "code") {
                    return self.push_notice(
                        terminal,
                        Color::Yellow,
                        "usage: /copy [code|all] (default: copy full last assistant reply)",
                    );
                }
                let in_mem = self.workers.get(&self.focused_worker).and_then(|st| {
                    if !st.turn_reply_buf.trim().is_empty() {
                        Some(st.turn_reply_buf.clone())
                    } else if !st.last_assistant_reply.trim().is_empty() {
                        Some(st.last_assistant_reply.clone())
                    } else {
                        None
                    }
                });
                let source = in_mem.or_else(|| {
                    let (wal_path, leaf) = self.focused_wal_and_leaf();
                    load_last_assistant_reply(&wal_path, leaf)
                });
                let Some(reply) = source else {
                    return self.push_notice(
                        terminal,
                        Color::Yellow,
                        "no assistant reply yet to copy",
                    );
                };
                let (label, payload) = if mode == "code" {
                    let Some(block) = extract_last_code_block(&reply) else {
                        return self.push_notice(
                            terminal,
                            Color::Yellow,
                            "no fenced code block found in the last assistant reply (use `/copy` for full reply)",
                        );
                    };
                    ("last code block", block)
                } else {
                    ("last reply", reply)
                };
                match write_clipboard_text(&payload) {
                    Ok(()) => {
                        let chars = payload.chars().count();
                        let line_cnt = payload.lines().count().max(1);
                        self.push_notice(
                            terminal,
                            Color::Green,
                            format!(
                                "📋 copied {label} to clipboard ({chars} chars · {line_cnt} lines)"
                            ),
                        )
                    }
                    Err(err) => self.push_notice(
                        terminal,
                        Color::Red,
                        format!("✗ clipboard copy failed: {err}"),
                    ),
                }
            }
            BuiltinSlashCommand::Diff => self.open_diff_overlay(terminal, rest),
            BuiltinSlashCommand::Import => self.cmd_import_session(terminal, rest),
            BuiltinSlashCommand::Memory => {
                let sub = rest.trim().to_ascii_lowercase();
                if matches!(sub.as_str(), "consolidate" | "dream" | "compact") {
                    self.submit_memory_consolidation(terminal)
                } else {
                    self.open_memory_overlay();
                    Ok(())
                }
            }
            BuiltinSlashCommand::Export => {
                let (wal_path, leaf) = self.focused_wal_and_leaf();
                let model = self
                    .workers
                    .get(&self.focused_worker)
                    .map(|st| st.handle.current_model.as_str())
                    .filter(|s| !s.is_empty())
                    .unwrap_or(&self.draft_model);
                let title = if self.meta.title.trim().is_empty() {
                    self.meta.id.as_str()
                } else {
                    self.meta.title.as_str()
                };
                let cwd_str = self.store.cwd.display().to_string();
                let exported_at = now_rfc3339();
                let Some(markdown) = export_wal_to_markdown(
                    &wal_path,
                    leaf,
                    ExportMetadata {
                        session_id: &self.meta.id,
                        title,
                        cwd: &cwd_str,
                        model,
                        exported_at: &exported_at,
                    },
                ) else {
                    return self.push_notice(
                        terminal,
                        Color::Yellow,
                        "no conversation turns yet to export",
                    );
                };

                if rest.eq_ignore_ascii_case("clipboard") {
                    return match write_clipboard_text(&markdown) {
                        Ok(()) => self.push_notice(
                            terminal,
                            Color::Green,
                            format!(
                                "📋 exported Markdown transcript to clipboard ({} chars · {} lines)",
                                markdown.chars().count(),
                                markdown.lines().count()
                            ),
                        ),
                        Err(err) => self.push_notice(
                            terminal,
                            Color::Red,
                            format!("✗ clipboard export failed: {err}"),
                        ),
                    };
                }

                let default_name = format!("anix-export-{}.md", self.meta.id);
                let dest = if rest.is_empty() {
                    self.store.cwd.join(&default_name)
                } else {
                    let expanded = if let Some(after) = rest.strip_prefix("~/") {
                        std::env::var_os("HOME")
                            .map(|h| PathBuf::from(h).join(after))
                            .unwrap_or_else(|| PathBuf::from(rest))
                    } else {
                        let p = PathBuf::from(rest);
                        if p.is_absolute() {
                            p
                        } else {
                            self.store.cwd.join(p)
                        }
                    };
                    if expanded.is_dir() {
                        expanded.join(&default_name)
                    } else if expanded.extension().is_none() {
                        expanded.with_extension("md")
                    } else {
                        expanded
                    }
                };

                if let Some(parent) = dest.parent()
                    && !parent.as_os_str().is_empty()
                    && let Err(err) = std::fs::create_dir_all(parent)
                {
                    return self.push_notice(
                        terminal,
                        Color::Red,
                        format!("✗ export failed creating directory: {err}"),
                    );
                }
                match std::fs::write(&dest, markdown.as_bytes()) {
                    Ok(()) => self.push_notice(
                        terminal,
                        Color::Green,
                        format!(
                            "📝 exported conversation to {} ({} bytes · {} lines)",
                            dest.display(),
                            markdown.len(),
                            markdown.lines().count()
                        ),
                    ),
                    Err(err) => self.push_notice(
                        terminal,
                        Color::Red,
                        format!("✗ export failed writing `{}`: {err}", dest.display()),
                    ),
                }
            }
            BuiltinSlashCommand::Help => {
                let mut rows = vec![
                    "Commands:".to_owned(),
                    "  !<cmd> / !!<cmd>                 Run shell command directly (`!!` forces model trigger)".to_owned(),
                ];
                for spec in SLASH_COMMAND_SPECS {
                    let lhs = if spec.args_hint.is_empty() {
                        spec.name.to_owned()
                    } else {
                        format!("{} {}", spec.name, spec.args_hint)
                    };
                    rows.push(format!("  {lhs:<32} {}", spec.description));
                }
                rows.push(String::new());
                rows.push("Keys:".to_owned());
                rows.push("  Enter submit · \\Enter / Alt+Enter / Ctrl+J newline · Tab complete /cmd & @path".to_owned());
                rows.push("  Shift+Tab cycle mode · Ctrl+O expand · Ctrl+W workers · Ctrl+G $EDITOR · Ctrl+S stash · Esc×2 rewind · ? shortcuts".to_owned());
                self.push_notice(terminal, Color::Gray, rows.join("\n"))
            }
            BuiltinSlashCommand::StatusBar => {
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
            BuiltinSlashCommand::Rewind => self.open_rewind_picker(terminal),
            BuiltinSlashCommand::Context => self.print_context_grid(terminal),
            BuiltinSlashCommand::Config => {
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
                            self.persist_meta_if_materialized();
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
            BuiltinSlashCommand::Quit => {
                self.should_quit = true;
                Ok(())
            }
            BuiltinSlashCommand::Tree => {
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
            BuiltinSlashCommand::Fork => {
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
            BuiltinSlashCommand::Compact => {
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
                    self.push_notice(
                        terminal,
                        Color::DarkGray,
                        "nothing to compact yet — send a prompt first",
                    )
                }
            }
            BuiltinSlashCommand::Resume => {
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
            BuiltinSlashCommand::Workers => {
                self.overlay = Overlay::Workers { selected: 0 };
                Ok(())
            }
            BuiltinSlashCommand::Spawn => self.cmd_spawn_worker(terminal, rest),
            BuiltinSlashCommand::Send => {
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
            BuiltinSlashCommand::Inspect => {
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
            BuiltinSlashCommand::Model => {
                if rest.is_empty() {
                    self.open_model_picker();
                    return Ok(());
                }
                let mut parts = rest.split_whitespace();
                let first = parts.next().unwrap_or("");
                let second = parts.next();
                if first.eq_ignore_ascii_case("effort") {
                    let (active_model, active_effort) = self.focused_model_and_effort();
                    let Some(lvl) = second else {
                        let profile = self.resolve_effort_profile_for(&active_model);
                        let levels = profile.levels_with_current(active_effort.as_deref());
                        let cur = active_effort.as_deref().unwrap_or("default");
                        let wire = profile.wire_preview(active_effort.as_deref());
                        return self.push_notice(
                            terminal,
                            Color::Gray,
                            format!(
                                "current: {active_model} (effort: {cur} · wire: {wire} · {})\nusage: /model effort <{}> (customize per-model mapping in ~/.config/anix-agent/config.json `model_effort_profiles`)",
                                profile.source_label,
                                levels.join("|")
                            ),
                        );
                    };
                    let norm = normalize_effort_input(lvl);
                    return self.switch_focused_model(
                        terminal,
                        &active_model,
                        Some(norm.as_deref()),
                    );
                }
                let effort_override = second.map(normalize_effort_input);
                match effort_override {
                    Some(norm) => self.switch_focused_model(terminal, first, Some(norm.as_deref())),
                    None => self.switch_focused_model(terminal, first, None),
                }
            }
            BuiltinSlashCommand::Agent => {
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
                        self.respawn_worker(
                            terminal,
                            &wid,
                            None,
                            None,
                            None,
                            Some(path.clone()),
                            None,
                        )?;
                        self.push_notice(
                            terminal,
                            Color::Cyan,
                            format!("◈ [{wid}] switched agent -> {}", path.display()),
                        )
                    }
                    Err(err) => self.push_notice(terminal, Color::Red, err.to_string()),
                }
            }
            BuiltinSlashCommand::Mode => {
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
                        self.persist_meta_if_materialized();
                        self.push_notice(
                            terminal,
                            Color::Cyan,
                            format!("◈ approval mode -> {}", p.as_str()),
                        )
                    }
                    Err(msg) => self.push_notice(terminal, Color::Red, msg),
                }
            }
            BuiltinSlashCommand::New => {
                let prev_session = if self.session_persisted {
                    Some((self.meta.id.clone(), self.meta.title.clone()))
                } else {
                    self.last_cleared_session.clone()
                };
                self.last_cleared_session = prev_session.clone();

                self.shutdown_all_workers();
                let (dir, meta) = self.store.draft_session(
                    self.meta.agent_path.clone(),
                    self.meta.model.clone(),
                    self.meta.provider.clone(),
                    self.policy,
                );
                self.session_dir = dir;
                self.meta = meta;
                self.session_persisted = false;
                self.focused_worker = "main".into();
                self.hint_line = None;
                self.pending_bang_context.clear();
                self.refresh_draft_defaults();

                terminal.clear_screen()?;
                if let Some((prev_id, prev_title)) = prev_session {
                    let width = self.term_width(terminal);
                    let title_budget = width.saturating_sub(prev_id.len() + 38).max(12);
                    let title_preview = truncate_single_line(&prev_title, title_budget);
                    let resume_line = Line::from(vec![
                        Span::styled("↺ ", Style::default().fg(Color::DarkGray)),
                        Span::styled(
                            format!("/resume {prev_id}"),
                            Style::default()
                                .fg(Color::Cyan)
                                .add_modifier(Modifier::BOLD),
                        ),
                        Span::styled(
                            format!("  restore previous session ({title_preview})"),
                            Style::default().fg(Color::DarkGray),
                        ),
                    ]);
                    self.push_lines(terminal, vec![resume_line])?;
                }
                self.print_banner(terminal, false)
            }
        }
    }

    pub(super) fn focused_wal_and_leaf(&self) -> (PathBuf, Option<u64>) {
        if let Some(st) = self.workers.get(&self.focused_worker) {
            (st.handle.spec.wal_path.clone(), st.handle.current_leaf)
        } else {
            (self.meta.active_wal.clone(), self.meta.active_leaf)
        }
    }

    pub(super) fn fork_focused_wal(
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

    pub(super) fn cmd_spawn_worker(
        &mut self,
        terminal: &mut UiTerminal,
        rest: &str,
    ) -> io::Result<()> {
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

        self.ensure_main_worker_ready(terminal)?;

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
        let eff_model = self.meta.model.as_deref().unwrap_or(&self.draft_model);
        let effort = self.meta.reasoning_effort.clone().or_else(|| {
            self.agent_config
                .effort_for_model(eff_model)
                .map(ToOwned::to_owned)
        });
        let spec = self.build_worker_spec(
            worker_id.clone(),
            agent_path.clone(),
            wal_path,
            worker_inbox,
            None,
            self.meta.model.clone(),
            self.meta.provider.clone(),
            effort,
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

    pub(super) fn print_wal_node_inspect(
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

    pub(super) fn cmd_import_session(
        &mut self,
        terminal: &mut UiTerminal,
        raw_arg: &str,
    ) -> io::Result<()> {
        let arg = raw_arg.trim();
        if arg.is_empty() || arg.eq_ignore_ascii_case("all") {
            return self.open_import_overlay(terminal, ImportTab::All);
        }
        if let Some(src) = ImportSource::parse_filter(arg) {
            let tab = match src {
                ImportSource::Claude => ImportTab::Claude,
                ImportSource::Codex => ImportTab::Codex,
                ImportSource::Antigravity => ImportTab::Antigravity,
            };
            return self.open_import_overlay(terminal, tab);
        }

        // Direct path import (`/import /path/to/transcript.jsonl`).
        let expanded = if let Some(after) = arg.strip_prefix("~/") {
            self.store.home_dir.join(after)
        } else {
            let p = PathBuf::from(arg);
            if p.is_absolute() {
                p
            } else {
                self.store.cwd.join(p)
            }
        };
        if expanded.is_file() {
            return self.import_from_path(terminal, &expanded, None);
        }

        // Otherwise open picker pre-filtered with the argument as a search query.
        self.open_import_overlay(terminal, ImportTab::All)?;
        if let Overlay::Import(ref mut picker) = self.overlay {
            picker.query = arg.to_owned();
            picker.selected = 0;
            picker.clamp_selection();
        }
        Ok(())
    }

    pub(super) fn import_from_path(
        &mut self,
        terminal: &mut UiTerminal,
        path: &std::path::Path,
        hint: Option<ImportSource>,
    ) -> io::Result<()> {
        let conv = match load_external_session(path, hint) {
            Ok(c) => c,
            Err(err) => {
                return self.push_notice(terminal, Color::Red, format!("✗ import failed: {err}"));
            }
        };

        let base_prompt = std::fs::read_to_string(&self.meta.agent_path)
            .map(|raw| strip_frontmatter(&raw).to_owned())
            .unwrap_or_default();
        let (new_dir, mut new_meta) = self.store.create_session(
            self.meta.agent_path.clone(),
            self.meta.model.clone(),
            self.meta.provider.clone(),
            self.policy,
        )?;
        let wal_path = new_meta.active_wal.clone();
        let shell_name = self.base_shell.as_deref().unwrap_or("msh");

        let leaf_id = match write_imported_wal(
            &wal_path,
            &new_meta.id,
            &base_prompt,
            &self.store.cwd,
            &anix_hooks_pre_context_dir(),
            shell_name,
            &conv,
            &self.anix_bin,
        ) {
            Ok(leaf) => leaf,
            Err(err) => {
                let _ = std::fs::remove_dir_all(&new_dir);
                return self.push_notice(terminal, Color::Red, format!("✗ import failed: {err}"));
            }
        };

        new_meta.title = format!("[{}] {}", conv.source.badge(), conv.title);
        new_meta.active_leaf = Some(leaf_id);
        new_meta.updated_at = now_rfc3339();
        save_session_meta(&new_dir, &new_meta)?;

        let orig_cwd_note = conv
            .original_cwd
            .as_deref()
            .map(|c| format!(" · orig cwd: {c}"))
            .unwrap_or_default();
        self.push_notice(
            terminal,
            Color::Green,
            format!(
                "⇣ imported {} session ({} turns · anix fsck ok{orig_cwd_note})",
                conv.source.label(),
                conv.turns.len(),
            ),
        )?;
        self.switch_session(terminal, new_dir, new_meta)
    }

    pub(super) fn edit_path_in_external_editor(
        &mut self,
        terminal: &mut UiTerminal,
        path: &std::path::Path,
        starter: Option<&str>,
    ) -> io::Result<Result<String, String>> {
        let editor = std::env::var("VISUAL")
            .ok()
            .filter(|s| !s.trim().is_empty())
            .or_else(|| {
                std::env::var("EDITOR")
                    .ok()
                    .filter(|s| !s.trim().is_empty())
            })
            .unwrap_or_else(|| "vi".into());

        if !path.exists() {
            if let Some(parent) = path.parent()
                && !parent.as_os_str().is_empty()
            {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(path, starter.unwrap_or(""))?;
        }

        self.reader_paused.store(true, Ordering::Release);
        thread::sleep(Duration::from_millis(75));
        let _ = terminal.clear_viewport_for_external();
        let _ = crossterm::execute!(io::stdout(), DisableBracketedPaste);
        let _ = disable_raw_mode();

        let status = std::process::Command::new("/bin/sh")
            .args([
                "-c",
                &format!(
                    "{editor} \"{}\"",
                    path.display().to_string().replace('"', "\\\"")
                ),
            ])
            .status();

        clear_tty_flusho();
        let _ = enable_raw_mode();
        let _ = crossterm::execute!(io::stdout(), EnableBracketedPaste);
        terminal.resync_after_external();
        self.reader_paused.store(false, Ordering::Release);

        Ok(match status {
            Ok(st) if st.success() => Ok(editor),
            Ok(st) => Err(format!("`{editor}` exited with status {st}")),
            Err(err) => Err(format!("failed launching `{editor}`: {err}")),
        })
    }

    pub(super) fn open_in_external_editor(&mut self, terminal: &mut UiTerminal) -> io::Result<()> {
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
            f.write_all(self.input.expanded_text().as_bytes())?;
            f.flush()?;
        }

        // Pause crossterm reader thread so `$EDITOR` has exclusive ownership of stdin/TTY.
        self.reader_paused.store(true, Ordering::Release);
        thread::sleep(Duration::from_millis(75));
        let _ = terminal.clear_viewport_for_external();
        let _ = crossterm::execute!(io::stdout(), DisableBracketedPaste);
        let _ = disable_raw_mode();

        let status = std::process::Command::new("/bin/sh")
            .args(["-c", &format!("{editor} \"{}\"", tmp_path.display())])
            .status();

        clear_tty_flusho();
        let _ = enable_raw_mode();
        let _ = crossterm::execute!(io::stdout(), EnableBracketedPaste);
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

    pub(super) fn suspend_to_shell(&mut self, terminal: &mut UiTerminal) -> io::Result<()> {
        self.reader_paused.store(true, Ordering::Release);
        thread::sleep(Duration::from_millis(75));
        let _ = terminal.clear_viewport_for_external();
        let _ = crossterm::execute!(io::stdout(), DisableBracketedPaste);
        let _ = disable_raw_mode();

        // SAFETY: Standard POSIX job-control suspend; resumes cleanly when user runs `fg`.
        unsafe {
            libc::raise(libc::SIGTSTP);
        }

        clear_tty_flusho();
        let _ = enable_raw_mode();
        let _ = crossterm::execute!(io::stdout(), EnableBracketedPaste);
        terminal.resync_after_external();
        self.reader_paused.store(false, Ordering::Release);
        self.hint_line = Some("resumed from background".into());
        Ok(())
    }

    pub(super) fn execute_custom_slash_entry(
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
                self.submit_user_input(terminal, expanded.clone(), expanded, Vec::new())
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
                self.submit_user_input(terminal, prompt.clone(), prompt, Vec::new())
            }
        }
    }

    pub(super) fn print_context_grid(&self, terminal: &mut UiTerminal) -> io::Result<()> {
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
                    None => (u64::from(self.draft_max_tokens), None, m),
                }
            }
            None => (
                u64::from(self.draft_max_tokens),
                Some(0),
                self.draft_model.clone(),
            ),
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
}
