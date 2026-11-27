//! DIY variable-interpolated status bar renderer for `anix-agent`.
//!
//! Template syntax (configured via `status_bar` in `~/.config/anix-agent/config.json` or `/statusbar`):
//! - `{var}` — insert variable with smart semantic default color
//! - `{var:color}` or `{var:color+bold}` — override color/style (`cyan`, `green`, `yellow`,
//!   `magenta`, `red`, `blue`, `white`, `gray`, `dim`, `bold`, `italic`, `underline`)
//! - `{?var ...}` — conditional block rendered ONLY when `var` is non-empty / non-zero
//! - `{right}` — push everything after this token flush to the right edge of the terminal

use std::fs;
use std::path::Path;

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthStr;

use crate::cli::ApprovalPolicy;
use crate::config::BangMode;

/// Previous built-in default template (auto-upgraded on load unless customized by user).
pub const LEGACY_DEFAULT_STATUS_BAR: &str = " {dot} {mode} · {model}{?git_branch  ⎇ {git_branch}} · ctx {ctx_pct}{?leaf  {leaf}}{?holds  ⏳{holds}}{?workers_extra  ⚡{workers}}{right}{hints} ";

/// Default clean Claude-Code-inspired DIY status bar template.
pub const DEFAULT_STATUS_BAR: &str = " {dot} {mode} · {model}{?effort  ({effort})}{?git_branch  ⎇ {git_branch}} · ctx {ctx_pct}{?leaf  {leaf}}{?holds  ⏳{holds}}{?workers_extra  ⚡{workers}}{right}{hints} ";

/// Minimalist status bar preset.
pub const PRESET_MINIMAL: &str = " {dot} {mode} {right}{model}{?effort  ({effort})} · {ctx_pct} ";

/// Full telemetry status bar preset.
pub const PRESET_FULL: &str = " {dot} [{worker}] {dir}{?git_branch ({git_branch})} │ {model}{?effort  ({effort})} │ ctx {ctx} │ tok {tokens}{?cache_pct │ cache {cache_pct}}{?elapsed │ {elapsed}} │ !:{bang_mode}{?leaf │ {leaf}}{?holds │ holds:{holds}}{right}{mode} · ? ";

/// Documentation table for `/statusbar vars`.
pub const AVAILABLE_VARS: &[(&str, &str)] = &[
    ("dot", "Live status dot / streaming spinner"),
    ("mode", "Approval policy (ask | auto | plan)"),
    (
        "bang_mode",
        "Direct `!` shell mode (silent | attach | trigger)",
    ),
    ("worker", "Focused worker ID (main, w1, ...)"),
    ("agent", "Active *.agent filename"),
    ("model", "Active LLM model ID"),
    ("effort", "Active reasoning effort level (empty if default)"),
    ("provider", "Active LLM provider name"),
    ("dir", "Current working directory basename"),
    ("cwd", "Full current working directory path"),
    (
        "git_branch",
        "Current Git branch or short commit (if in repo)",
    ),
    ("ctx_pct", "Context watermark percentage (e.g. 12%)"),
    ("ctx_tokens", "Current context tokens (e.g. 15.3k)"),
    ("ctx_max", "Max context window tokens (e.g. 256.0k)"),
    ("ctx", "Full context readout (e.g. 12% (15.3k/256.0k))"),
    ("in_tokens", "Session input tokens (e.g. 24.1k)"),
    ("out_tokens", "Session output tokens (e.g. 3.2k)"),
    ("tokens", "Combined session tokens (in/out)"),
    ("cache_pct", "Prompt cache hit rate (e.g. 84%, empty if 0)"),
    (
        "cached",
        "Session cached input tokens (e.g. 18.2k, empty if 0)",
    ),
    (
        "reasoning",
        "Session reasoning tokens (e.g. 4.1k, empty if 0)",
    ),
    (
        "elapsed",
        "Live active turn elapsed time (e.g. 6s, empty if idle)",
    ),
    ("leaf", "Active Tree WAL leaf ID (e.g. #14, empty if fresh)"),
    ("holds", "Active hold lease count (empty if 0)"),
    ("workers", "Total running worker count"),
    (
        "workers_extra",
        "Background worker count (empty if only main)",
    ),
    ("session", "Current session ID (e.g. s-a1b2c3d4)"),
    ("hints", "Compact shortcut hint (? shortcuts · ^W workers)"),
    ("right", "Layout token: right-align everything that follows"),
];

/// Live runtime values injected into the status bar template on every draw.
#[derive(Debug, Clone)]
pub struct StatusVars {
    pub busy: bool,
    pub spinner: &'static str,
    pub mode: ApprovalPolicy,
    pub bang_mode: BangMode,
    pub worker: String,
    pub agent: String,
    pub model: String,
    pub effort: Option<String>,
    pub provider: String,
    pub cwd: String,
    pub dir: String,
    pub git_branch: Option<String>,
    pub ctx_pct_num: Option<u64>,
    pub ctx_tokens: String,
    pub ctx_max: String,
    pub in_tokens: String,
    pub out_tokens: String,
    pub cache_pct_num: Option<u64>,
    pub cached_tokens: String,
    pub reasoning_tokens: String,
    pub elapsed_secs: Option<u64>,
    pub leaf: Option<u64>,
    pub holds: usize,
    pub workers: usize,
    pub session_id: String,
}

impl StatusVars {
    /// Resolve a variable name to `(text, semantic_default_style)`.
    pub fn resolve(&self, name: &str) -> Option<(String, Style)> {
        match name {
            "dot" => {
                if self.busy {
                    Some((self.spinner.to_owned(), Style::default().fg(Color::Cyan)))
                } else {
                    Some(("●".to_owned(), Style::default().fg(Color::Green)))
                }
            }
            "mode" => {
                let c = match self.mode {
                    ApprovalPolicy::Ask => Color::Yellow,
                    ApprovalPolicy::Auto => Color::Green,
                    ApprovalPolicy::Plan => Color::Magenta,
                };
                Some((
                    self.mode.as_str().to_owned(),
                    Style::default().fg(c).add_modifier(Modifier::BOLD),
                ))
            }
            "bang_mode" => Some((
                self.bang_mode.as_str().to_owned(),
                Style::default().fg(Color::Yellow),
            )),
            "worker" => Some((
                self.worker.clone(),
                Style::default()
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD),
            )),
            "agent" => Some((self.agent.clone(), Style::default().fg(Color::White))),
            "model" => Some((self.model.clone(), Style::default().fg(Color::Cyan))),
            "effort" => Some((
                self.effort.clone().unwrap_or_default(),
                Style::default().fg(Color::Magenta),
            )),
            "provider" => Some((self.provider.clone(), Style::default().fg(Color::Gray))),
            "dir" => Some((self.dir.clone(), Style::default().fg(Color::Blue))),
            "cwd" => Some((self.cwd.clone(), Style::default().fg(Color::Gray))),
            "git_branch" => Some((
                self.git_branch.clone().unwrap_or_default(),
                Style::default().fg(Color::Green),
            )),
            "ctx_pct" => {
                let (txt, color) = match self.ctx_pct_num {
                    Some(p) if p >= 85 => (format!("{p}%"), Color::Red),
                    Some(p) if p >= 70 => (format!("{p}%"), Color::Yellow),
                    Some(p) => (format!("{p}%"), Color::White),
                    None => ("-".to_owned(), Color::DarkGray),
                };
                Some((txt, Style::default().fg(color)))
            }
            "ctx_tokens" => Some((self.ctx_tokens.clone(), Style::default().fg(Color::White))),
            "ctx_max" => Some((self.ctx_max.clone(), Style::default().fg(Color::DarkGray))),
            "ctx" => {
                let (txt, color) = match self.ctx_pct_num {
                    Some(p) => {
                        let c = if p >= 85 {
                            Color::Red
                        } else if p >= 70 {
                            Color::Yellow
                        } else {
                            Color::White
                        };
                        (format!("{p}% ({}/{})", self.ctx_tokens, self.ctx_max), c)
                    }
                    None => ("-".to_owned(), Color::DarkGray),
                };
                Some((txt, Style::default().fg(color)))
            }
            "in_tokens" => Some((self.in_tokens.clone(), Style::default().fg(Color::Gray))),
            "out_tokens" => Some((self.out_tokens.clone(), Style::default().fg(Color::Gray))),
            "tokens" => Some((
                format!("{}/{}", self.in_tokens, self.out_tokens),
                Style::default().fg(Color::Gray),
            )),
            "cache_pct" => {
                let (txt, color) = match self.cache_pct_num {
                    Some(p) if p >= 50 => (format!("{p}%"), Color::Green),
                    Some(p) if p > 0 => (format!("{p}%"), Color::Yellow),
                    _ => (String::new(), Color::DarkGray),
                };
                Some((txt, Style::default().fg(color)))
            }
            "cached" => Some((
                self.cached_tokens.clone(),
                Style::default().fg(Color::Green),
            )),
            "reasoning" => Some((
                self.reasoning_tokens.clone(),
                Style::default().fg(Color::Magenta),
            )),
            "elapsed" => Some((
                self.elapsed_secs
                    .map(|s| format!("{s}s"))
                    .unwrap_or_default(),
                Style::default().fg(Color::Cyan),
            )),
            "leaf" => Some((
                self.leaf.map(|l| format!("#{l}")).unwrap_or_default(),
                Style::default().fg(Color::Gray),
            )),
            "holds" => Some((
                if self.holds > 0 {
                    self.holds.to_string()
                } else {
                    String::new()
                },
                Style::default().fg(Color::Magenta),
            )),
            "workers" => Some((self.workers.to_string(), Style::default().fg(Color::Cyan))),
            "workers_extra" => Some((
                if self.workers > 1 {
                    self.workers.to_string()
                } else {
                    String::new()
                },
                Style::default().fg(Color::Cyan),
            )),
            "session" => Some((
                self.session_id.clone(),
                Style::default().fg(Color::DarkGray),
            )),
            "hints" => Some((
                "? shortcuts · ⇧Tab mode".to_owned(),
                Style::default().fg(Color::DarkGray),
            )),
            _ => None,
        }
    }

    fn is_truthy(&self, name: &str) -> bool {
        match name {
            "busy" => self.busy,
            "effort" => self.effort.as_deref().is_some_and(|s| !s.is_empty()),
            "git_branch" => self.git_branch.as_deref().is_some_and(|s| !s.is_empty()),
            "leaf" => self.leaf.is_some(),
            "cache_pct" => self.cache_pct_num.is_some_and(|p| p > 0),
            "cached" => !self.cached_tokens.is_empty(),
            "reasoning" => !self.reasoning_tokens.is_empty(),
            "elapsed" => self.elapsed_secs.is_some(),
            "holds" => self.holds > 0,
            "workers_extra" => self.workers > 1,
            other => self
                .resolve(other)
                .is_some_and(|(v, _)| !v.is_empty() && v != "-"),
        }
    }
}

/// Render a DIY template string into a single terminal-width-aligned [`Line`].
pub fn render_status_bar(template: &str, vars: &StatusVars, width: usize) -> Line<'static> {
    let base_style = Style::default().fg(Color::DarkGray);
    let (left_tpl, right_tpl) = match template.split_once("{right}") {
        Some((l, r)) => (l, Some(r)),
        None => (template, None),
    };

    let mut left_spans = expand_template(left_tpl, vars, base_style);
    let right_spans = right_tpl
        .map(|r| expand_template(r, vars, base_style))
        .unwrap_or_default();

    let right_w: usize = right_spans.iter().map(|s| s.content.width()).sum();
    let left_w: usize = left_spans.iter().map(|s| s.content.width()).sum();

    if right_spans.is_empty() {
        return Line::from(clip_spans(left_spans, width));
    }

    if left_w + right_w < width {
        let gap = width - left_w - right_w;
        left_spans.push(Span::raw(" ".repeat(gap)));
        left_spans.extend(right_spans);
        Line::from(left_spans)
    } else if right_w + 6 < width {
        let left_budget = width.saturating_sub(right_w + 1);
        let mut out = clip_spans(left_spans, left_budget);
        let used: usize = out.iter().map(|s| s.content.width()).sum();
        let gap = width.saturating_sub(used + right_w);
        if gap > 0 {
            out.push(Span::raw(" ".repeat(gap)));
        }
        out.extend(right_spans);
        Line::from(out)
    } else {
        Line::from(clip_spans(left_spans, width))
    }
}

/// Resolve preset names (`default`, `minimal`, `full`) or return `None`.
pub fn resolve_preset(name: &str) -> Option<&'static str> {
    match name.trim().to_ascii_lowercase().as_str() {
        "default" => Some(DEFAULT_STATUS_BAR),
        "minimal" | "compact" => Some(PRESET_MINIMAL),
        "full" | "telemetry" => Some(PRESET_FULL),
        _ => None,
    }
}

/// Lightweight, zero-subprocess Git branch reader walking upward from `cwd` to `.git/HEAD`.
pub fn detect_git_branch(cwd: &Path) -> Option<String> {
    let mut cur = Some(cwd);
    while let Some(dir) = cur {
        let dot_git = dir.join(".git");
        let head_path = if dot_git.is_dir() {
            Some(dot_git.join("HEAD"))
        } else if dot_git.is_file() {
            // Git worktree / submodule `.git` file: `gitdir: <path>`
            fs::read_to_string(&dot_git)
                .ok()
                .and_then(|raw| {
                    raw.trim()
                        .strip_prefix("gitdir:")
                        .map(str::trim)
                        .map(ToOwned::to_owned)
                })
                .map(|rel| {
                    let p = Path::new(&rel);
                    if p.is_absolute() {
                        p.join("HEAD")
                    } else {
                        dir.join(p).join("HEAD")
                    }
                })
        } else {
            None
        };
        if let Some(hp) = head_path
            && let Ok(head) = fs::read_to_string(hp)
        {
            let trimmed = head.trim();
            if let Some(branch) = trimmed.strip_prefix("ref: refs/heads/") {
                return Some(branch.to_owned());
            }
            if trimmed.len() >= 7 {
                return Some(trimmed[..7].to_owned());
            }
        }
        cur = dir.parent();
    }
    None
}

fn expand_template(tpl: &str, vars: &StatusVars, base_style: Style) -> Vec<Span<'static>> {
    let mut out = Vec::new();
    let mut literal = String::new();
    let bytes = tpl.as_bytes();
    let mut i = 0usize;

    while i < bytes.len() {
        if bytes[i] == b'{' {
            // Escaped `{{` -> literal `{`
            if i + 1 < bytes.len() && bytes[i + 1] == b'{' {
                literal.push('{');
                i += 2;
                continue;
            }
            if let Some(end_idx) = find_matching_brace(tpl, i) {
                if !literal.is_empty() {
                    out.push(Span::styled(std::mem::take(&mut literal), base_style));
                }
                let inner = &tpl[i + 1..end_idx];
                if let Some(cond_body) = inner.strip_prefix('?') {
                    // `{?var <nested template>}`
                    let mut split_at = cond_body.len();
                    for (pos, ch) in cond_body.char_indices() {
                        if ch.is_whitespace() {
                            split_at = pos;
                            break;
                        }
                    }
                    let var_name = &cond_body[..split_at];
                    let body = cond_body[split_at..]
                        .strip_prefix(' ')
                        .unwrap_or(&cond_body[split_at..]);
                    if vars.is_truthy(var_name) {
                        out.extend(expand_template(body, vars, base_style));
                    }
                } else {
                    let (var_name, style_spec) = match inner.split_once(':') {
                        Some((n, s)) => (n.trim(), Some(s.trim())),
                        None => (inner.trim(), None),
                    };
                    if let Some((val, default_style)) = vars.resolve(var_name) {
                        if !val.is_empty() {
                            let final_style = match style_spec {
                                Some(spec) => apply_style_spec(default_style, spec),
                                None => default_style,
                            };
                            out.push(Span::styled(val, final_style));
                        }
                    } else {
                        // Preserve unrecognized tokens visibly so users notice typos immediately.
                        out.push(Span::styled(format!("{{{inner}}}"), base_style));
                    }
                }
                i = end_idx + 1;
                continue;
            }
        } else if bytes[i] == b'}' && i + 1 < bytes.len() && bytes[i + 1] == b'}' {
            literal.push('}');
            i += 2;
            continue;
        }

        let ch = tpl[i..].chars().next().unwrap_or(' ');
        literal.push(ch);
        i += ch.len_utf8();
    }

    if !literal.is_empty() {
        out.push(Span::styled(literal, base_style));
    }
    out
}

fn find_matching_brace(s: &str, open_idx: usize) -> Option<usize> {
    let mut depth = 0usize;
    for (offset, ch) in s[open_idx..].char_indices() {
        match ch {
            '{' => depth += 1,
            '}' => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    return Some(open_idx + offset);
                }
            }
            _ => {}
        }
    }
    None
}

fn apply_style_spec(mut style: Style, spec: &str) -> Style {
    for part in spec.split('+') {
        match part.trim().to_ascii_lowercase().as_str() {
            "cyan" => style = style.fg(Color::Cyan),
            "green" => style = style.fg(Color::Green),
            "yellow" => style = style.fg(Color::Yellow),
            "magenta" | "purple" => style = style.fg(Color::Magenta),
            "red" => style = style.fg(Color::Red),
            "blue" => style = style.fg(Color::Blue),
            "white" => style = style.fg(Color::White),
            "gray" | "grey" => style = style.fg(Color::Gray),
            "dim" | "darkgray" => style = style.fg(Color::DarkGray),
            "bold" => style = style.add_modifier(Modifier::BOLD),
            "italic" => style = style.add_modifier(Modifier::ITALIC),
            "underline" => style = style.add_modifier(Modifier::UNDERLINED),
            _ => {}
        }
    }
    style
}

fn clip_spans(spans: Vec<Span<'static>>, max_cols: usize) -> Vec<Span<'static>> {
    let mut out = Vec::new();
    let mut used = 0usize;
    for sp in spans {
        let w = sp.content.width();
        if used + w <= max_cols {
            used += w;
            out.push(sp);
        } else {
            let rem = max_cols.saturating_sub(used);
            if rem > 1 {
                let mut buf = String::new();
                let mut cur = 0usize;
                for ch in sp.content.chars() {
                    let cw = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
                    if cur + cw + 1 > rem {
                        buf.push('…');
                        break;
                    }
                    buf.push(ch);
                    cur += cw;
                }
                out.push(Span::styled(buf, sp.style));
            }
            break;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_diy_variables_conditionals_colors_and_right_alignment() {
        let vars = StatusVars {
            busy: false,
            spinner: "⠋",
            mode: ApprovalPolicy::Ask,
            bang_mode: BangMode::Silent,
            worker: "main".into(),
            agent: "default.agent".into(),
            model: "deepseek-v4.1".into(),
            effort: Some("xhigh".into()),
            provider: "openai-responses".into(),
            cwd: "/tmp/demo".into(),
            dir: "demo".into(),
            git_branch: Some("main".into()),
            ctx_pct_num: Some(12),
            ctx_tokens: "15.3k".into(),
            ctx_max: "128.0k".into(),
            in_tokens: "10.0k".into(),
            out_tokens: "2.0k".into(),
            cache_pct_num: Some(80),
            cached_tokens: "8.0k".into(),
            reasoning_tokens: "1.2k".into(),
            elapsed_secs: None,
            leaf: Some(7),
            holds: 0,
            workers: 1,
            session_id: "s-12345".into(),
        };

        let line = render_status_bar(DEFAULT_STATUS_BAR, &vars, 100);
        let text = line.to_string();
        assert!(text.contains("ask"));
        assert!(text.contains("deepseek-v4.1 (xhigh)"));
        assert!(text.contains("⎇ main"));
        assert!(text.contains("ctx 12%"));
        assert!(text.contains("#7"));
        assert!(!text.contains("⏳")); // holds == 0 -> omitted
        assert!(!text.contains("⚡")); // workers == 1 -> omitted
        assert!(text.ends_with("? shortcuts · ⇧Tab mode "));
        assert_eq!(text.width(), 100);

        // Custom DIY template with explicit color override
        let custom = render_status_bar(
            "{dir:yellow+bold} ({model:magenta}){right}{tokens}",
            &vars,
            40,
        );
        assert!(custom.to_string().starts_with("demo (deepseek-v4.1)"));
        assert!(custom.to_string().ends_with("10.0k/2.0k"));
    }
}
