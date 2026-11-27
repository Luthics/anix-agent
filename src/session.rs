//! XDG session storage, `meta.json` persistence, built-in default `*.agent` materialization,
//! agent discovery (`~/.config/anix/agents/`), and Layer 1 `anix` binary lookup.

use std::env;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use uuid::Uuid;

use crate::cli::ApprovalPolicy;

const BUILTIN_DEFAULT_AGENT: &str = "\
#!/usr/bin/env anix
---
name: anix-agent
shell: msh
output: reply
stderr: auto
max_turns: 40
---
You are an expert POSIX systems and software engineering agent running inside the interactive `anix-agent` TUI workbench.
Be concise, accurate, and pragmatic. Inspect before editing, verify changes with real commands/tests, and summarize results clearly in Markdown.

Workbench capabilities:
- Your `shell` tool runs through the user's approval policy (`ask`/`auto`/`plan`); if a command is denied or edited, respect the user's feedback and never bypass approval.
- Built-in CLI helpers on `$PATH`: `aview <image|pdf>` to visually inspect local images/PDFs; `aplan` (`aplan <<'EOF'\n- [x] ...\n- [~] ...\n- [ ] ...\nEOF`, or `aplan clear`) to update the live TUI progress checklist on multi-step tasks without bloating history; `aclr -t <turn_id> \"<summary>\"` to fold dead-end exploration without breaking prompt cache; `anix fork \"$ANIX_WAL\" --leaf \"$ANIX_LEAF\" -o <branch.wal>` + `\"$ANIX_AGENT\" -q -c <branch.wal>` to self-fork parallel sub-agents inheriting your current context (see `<runtime_tips>` for full recipes).
";

/// Persistent metadata stored per interactive session (`meta.json`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionMeta {
    pub id: String,
    pub title: String,
    pub cwd: PathBuf,
    pub created_at: String,
    pub updated_at: String,
    pub agent_path: PathBuf,
    pub model: Option<String>,
    pub provider: Option<String>,
    #[serde(default)]
    pub reasoning_effort: Option<String>,
    pub active_wal: PathBuf,
    pub active_leaf: Option<u64>,
    pub approval_policy: ApprovalPolicy,
}

/// Discovered custom slash command (`commands/*.md`) or skill (`skills/*/SKILL.md`) surfaced in `/`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CustomSlashKind {
    Command(PathBuf),
    Skill(PathBuf),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CustomSlashEntry {
    /// Slash command name including leading `/` (e.g. `/review-pr` or `/playwright-cli`).
    pub slash_name: String,
    pub description: String,
    /// Origin badge shown in completion menu (`user`, `project`, or `skill`).
    pub source_tag: &'static str,
    pub kind: CustomSlashKind,
}

/// Persistent cross-session prompt history entry (`~/.local/share/anix-agent/history.jsonl`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct HistoryEntry {
    pub ts: String,
    pub cwd: String,
    pub text: String,
}

/// Summary row shown in `/resume` and `/sessions`.
#[derive(Debug, Clone)]
pub struct SessionSummary {
    pub meta: SessionMeta,
    pub session_dir: PathBuf,
    pub wal_bytes: u64,
    pub turn_count: usize,
    pub project_label: String,
    pub is_current_project: bool,
}

/// Manages `${XDG_DATA_HOME:-~/.local/share}/anix-agent/` paths and session directories.
#[derive(Debug, Clone)]
pub struct SessionStore {
    pub home_dir: PathBuf,
    pub data_dir: PathBuf,
    pub runtime_dir: PathBuf,
    pub config_agents_dir: PathBuf,
    pub cwd: PathBuf,
    pub project_slug: String,
}

impl SessionStore {
    pub fn open(cwd: &Path) -> io::Result<Self> {
        let home_dir = env::var_os("HOME")
            .map(PathBuf::from)
            .ok_or_else(|| io::Error::other("HOME environment variable is not set"))?;

        let data_dir = env::var_os("XDG_DATA_HOME")
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| home_dir.join(".local/share"))
            .join("anix-agent");

        let runtime_dir = match env::var_os("XDG_RUNTIME_DIR").filter(|v| !v.is_empty()) {
            Some(dir) => PathBuf::from(dir).join("anix-agent"),
            None => {
                // SAFETY: `geteuid(2)` cannot fail.
                let uid = unsafe { libc::geteuid() };
                PathBuf::from(format!("/tmp/anix-agent-{uid}"))
            }
        };

        let config_agents_dir = env::var_os("XDG_CONFIG_HOME")
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| home_dir.join(".config"))
            .join("anix/agents");

        ensure_dir_0700(&data_dir)?;
        ensure_dir_0700(&runtime_dir)?;

        let canonical_cwd = fs::canonicalize(cwd).unwrap_or_else(|_| cwd.to_path_buf());
        let project_slug = compute_project_slug(&canonical_cwd);

        Ok(Self {
            home_dir,
            data_dir,
            runtime_dir,
            config_agents_dir,
            cwd: canonical_cwd,
            project_slug,
        })
    }

    pub fn project_sessions_dir(&self) -> PathBuf {
        self.data_dir.join("sessions").join(&self.project_slug)
    }

    /// Ensure the built-in v0.4.0 default `*.agent` script exists and return its path.
    pub fn ensure_default_agent(&self) -> io::Result<PathBuf> {
        let dest = self.data_dir.join("default.agent");
        let needs_write = fs::read_to_string(&dest)
            .map(|existing| existing != BUILTIN_DEFAULT_AGENT)
            .unwrap_or(true);
        if needs_write {
            let mut f = OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .mode(0o755)
                .open(&dest)?;
            f.write_all(BUILTIN_DEFAULT_AGENT.as_bytes())?;
            f.flush()?;
        }
        Ok(dest)
    }

    /// Resolve an agent specifier (explicit path, `~/.config/anix/agents/<name>[.agent]`, or built-in default).
    pub fn resolve_agent_path(&self, spec: Option<&str>) -> io::Result<PathBuf> {
        let Some(raw) = spec.map(str::trim).filter(|s| !s.is_empty()) else {
            return self.ensure_default_agent();
        };

        let expanded = expand_tilde(raw, &self.home_dir);
        if expanded.is_file() {
            return Ok(fs::canonicalize(&expanded).unwrap_or(expanded));
        }

        let rel_cwd = self.cwd.join(&expanded);
        if rel_cwd.is_file() {
            return Ok(fs::canonicalize(&rel_cwd).unwrap_or(rel_cwd));
        }

        for cand in [
            self.config_agents_dir.join(raw),
            self.config_agents_dir.join(format!("{raw}.agent")),
        ] {
            if cand.is_file() {
                return Ok(fs::canonicalize(&cand).unwrap_or(cand));
            }
        }

        Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!(
                "agent script `{raw}` not found (checked path and `{}`)",
                self.config_agents_dir.display()
            ),
        ))
    }

    /// List available `*.agent` files under `~/.config/anix/agents/`.
    pub fn list_available_agents(&self) -> Vec<(String, PathBuf)> {
        let mut out = Vec::new();
        if let Ok(entries) = fs::read_dir(&self.config_agents_dir) {
            for entry in entries.flatten() {
                let p = entry.path();
                if p.is_file()
                    && p.extension().and_then(|e| e.to_str()) == Some("agent")
                    && let Some(stem) = p.file_stem().and_then(|s| s.to_str())
                {
                    out.push((stem.to_owned(), p));
                }
            }
        }
        out.sort_by(|a, b| a.0.cmp(&b.0));
        out
    }

    /// Discover custom markdown commands (`~/.config/anix/commands/*.md`, `<cwd>/.anix/commands/*.md`)
    /// and skills (`~/.config/anix/skills/*/SKILL.md`, `~/.agents/skills/*/SKILL.md`) for live `/` completion.
    pub fn discover_custom_slash_entries(&self) -> Vec<CustomSlashEntry> {
        let mut seen = std::collections::HashSet::new();
        let mut out = Vec::new();

        let config_anix = self
            .config_agents_dir
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| self.home_dir.join(".config/anix"));

        // 1. Project commands (`<cwd>/.anix/commands/*.md`) take highest priority.
        scan_markdown_commands(
            &self.cwd.join(".anix/commands"),
            "project",
            &mut seen,
            &mut out,
        );
        // 2. User commands (`~/.config/anix/commands/*.md`).
        scan_markdown_commands(&config_anix.join("commands"), "user", &mut seen, &mut out);
        // 3. Skills (`~/.config/anix/skills/*/SKILL.md`, `~/.agents/skills/*/SKILL.md`).
        scan_skill_dirs(&config_anix.join("skills"), "skill", &mut seen, &mut out);
        scan_skill_dirs(
            &self.home_dir.join(".agents/skills"),
            "skill",
            &mut seen,
            &mut out,
        );

        out.sort_by(|a, b| a.slash_name.cmp(&b.slash_name));
        out
    }

    /// Build an in-memory draft session without creating any files or directories on disk yet.
    /// Call [`Self::materialize_session`] once the user actually sends their first message.
    pub fn draft_session(
        &self,
        agent_path: PathBuf,
        model: Option<String>,
        provider: Option<String>,
        approval_policy: ApprovalPolicy,
    ) -> (PathBuf, SessionMeta) {
        let id = format!("s-{}", &Uuid::new_v4().simple().to_string()[..10]);
        let session_dir = self.project_sessions_dir().join(&id);
        let now = now_rfc3339();
        let meta = SessionMeta {
            id,
            title: "New session".into(),
            cwd: self.cwd.clone(),
            created_at: now.clone(),
            updated_at: now,
            agent_path,
            model,
            provider,
            reasoning_effort: None,
            active_wal: session_dir.join("main.wal.jsonl"),
            active_leaf: None,
            approval_policy,
        };
        (session_dir, meta)
    }

    /// Persist a draft session directory (`branches/`, `inbox/`, `workers/`, `meta.json`) on disk.
    pub fn materialize_session(&self, session_dir: &Path, meta: &SessionMeta) -> io::Result<()> {
        ensure_dir_0700(session_dir)?;
        ensure_dir_0700(&session_dir.join("branches"))?;
        ensure_dir_0700(&session_dir.join("inbox"))?;
        ensure_dir_0700(&session_dir.join("workers"))?;
        save_session_meta(session_dir, meta)
    }

    /// Create and immediately persist a session directory (`main.wal.jsonl`, `branches/`, `inbox/`, `meta.json`).
    pub fn create_session(
        &self,
        agent_path: PathBuf,
        model: Option<String>,
        provider: Option<String>,
        approval_policy: ApprovalPolicy,
    ) -> io::Result<(PathBuf, SessionMeta)> {
        let (session_dir, meta) = self.draft_session(agent_path, model, provider, approval_policy);
        self.materialize_session(&session_dir, &meta)?;
        Ok((session_dir, meta))
    }

    /// Lightweight read-only peek at effective `(model, provider, max_tokens)` from `agent_path`
    /// frontmatter + `~/.config/anix/config.toml` `[defaults]` so an unsaved draft session can
    /// render accurate status bar & `/context` info before spawning `anix`.
    pub fn preview_agent_defaults(
        &self,
        agent_path: &Path,
        explicit_config: Option<&Path>,
    ) -> (Option<String>, Option<String>, u32) {
        let mut model: Option<String> = None;
        let mut provider: Option<String> = None;
        let mut max_tokens: Option<u32> = None;

        if let Ok(raw) = fs::read_to_string(agent_path) {
            let mut in_fm = false;
            for line in raw.lines() {
                let t = line.trim();
                if t == "---" {
                    if !in_fm {
                        in_fm = true;
                        continue;
                    } else {
                        break;
                    }
                }
                if !in_fm {
                    continue;
                }
                if let Some((k, v)) = t.split_once(':') {
                    let key = k.trim();
                    let val = v.trim().trim_matches(|c| c == '"' || c == '\'');
                    if val.is_empty() {
                        continue;
                    }
                    match key {
                        "model" if model.is_none() => model = Some(val.to_owned()),
                        "provider" if provider.is_none() => provider = Some(val.to_owned()),
                        "max_tokens" if max_tokens.is_none() => max_tokens = val.parse().ok(),
                        _ => {}
                    }
                }
            }
        }

        let cfg_path = explicit_config.map(Path::to_path_buf).unwrap_or_else(|| {
            self.config_agents_dir
                .parent()
                .map(Path::to_path_buf)
                .unwrap_or_else(|| self.home_dir.join(".config/anix"))
                .join("config.toml")
        });
        if let Ok(raw) = fs::read_to_string(&cfg_path) {
            let mut in_defaults = false;
            for line in raw.lines() {
                let t = line.split('#').next().unwrap_or("").trim();
                if t.is_empty() {
                    continue;
                }
                if t.starts_with('[') {
                    in_defaults = t == "[defaults]";
                    continue;
                }
                if !in_defaults {
                    continue;
                }
                if let Some((k, v)) = t.split_once('=') {
                    let key = k.trim();
                    let val = v.trim().trim_matches(|c| c == '"' || c == '\'');
                    match key {
                        "model" if model.is_none() && !val.is_empty() => {
                            model = Some(val.to_owned())
                        }
                        "provider" if provider.is_none() && !val.is_empty() => {
                            provider = Some(val.to_owned())
                        }
                        "max_tokens" if max_tokens.is_none() => {
                            max_tokens = val.replace('_', "").parse().ok()
                        }
                        _ => {}
                    }
                }
            }
        }

        (model, provider, max_tokens.unwrap_or(256_000))
    }

    /// List all sessions for the current working directory, newest first.
    pub fn list_sessions(&self) -> Vec<SessionSummary> {
        let mut list = read_sessions_in_dir(&self.project_sessions_dir(), true);
        list.sort_by(|a, b| b.meta.updated_at.cmp(&a.meta.updated_at));
        list
    }

    /// List all sessions across every project under `${XDG_DATA_HOME}/anix-agent/sessions/`, newest first.
    pub fn list_all_sessions(&self) -> Vec<SessionSummary> {
        let sessions_root = self.data_dir.join("sessions");
        let Ok(project_dirs) = fs::read_dir(&sessions_root) else {
            return Vec::new();
        };
        let mut list = Vec::new();
        for entry in project_dirs.flatten() {
            let pdir = entry.path();
            if !pdir.is_dir() {
                continue;
            }
            let is_cur = pdir
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n == self.project_slug);
            list.extend(read_sessions_in_dir(&pdir, is_cur));
        }
        list.sort_by(|a, b| b.meta.updated_at.cmp(&a.meta.updated_at));
        list
    }

    /// Delete a saved session directory safely (verifying it lives under `data_dir/sessions/`).
    pub fn delete_session(&self, session_dir: &Path) -> io::Result<()> {
        let root = fs::canonicalize(self.data_dir.join("sessions"))?;
        let target = fs::canonicalize(session_dir)?;
        if !target.starts_with(&root) || target == root {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "refusing to delete path outside sessions directory",
            ));
        }
        fs::remove_dir_all(&target)
    }

    /// Path to persistent cross-session input history (`${XDG_DATA_HOME}/anix-agent/history.jsonl`).
    pub fn history_path(&self) -> PathBuf {
        self.data_dir.join("history.jsonl")
    }

    /// Load up to `max_entries` most recent persistent prompt history entries (oldest -> newest).
    pub fn load_input_history(&self, max_entries: usize) -> Vec<HistoryEntry> {
        let Ok(raw) = fs::read_to_string(self.history_path()) else {
            return Vec::new();
        };
        let mut entries = Vec::new();
        for line in raw.lines() {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            if let Ok(entry) = serde_json::from_str::<HistoryEntry>(trimmed) {
                if entry.text.trim().is_empty() {
                    continue;
                }
                if entries
                    .last()
                    .is_some_and(|prev: &HistoryEntry| prev.text == entry.text)
                {
                    continue;
                }
                entries.push(entry);
            }
        }
        if entries.len() > max_entries {
            entries.drain(0..entries.len() - max_entries);
        }
        entries
    }

    /// Append a submitted prompt to persistent cross-session history (`0600`, JSONL).
    pub fn append_input_history(&self, text: &str) -> io::Result<()> {
        let trimmed = text.trim();
        if trimmed.is_empty() {
            return Ok(());
        }
        ensure_dir_0700(&self.data_dir)?;
        let path = self.history_path();
        let entry = HistoryEntry {
            ts: now_rfc3339(),
            cwd: self.cwd.display().to_string(),
            text: trimmed.to_owned(),
        };
        let line = serde_json::to_string(&entry).map_err(io::Error::other)?;
        let mut f = OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o600)
            .open(&path)?;
        writeln!(f, "{line}")?;
        Ok(())
    }

    /// Find a session by `"latest"` or exact/prefix ID match (checks current project first, then all).
    pub fn find_session(&self, query: &str) -> Option<(PathBuf, SessionMeta)> {
        let sessions = self.list_sessions();
        if query == "latest" {
            return sessions.into_iter().next().map(|s| (s.session_dir, s.meta));
        }
        if let Some(hit) = sessions
            .into_iter()
            .find(|s| s.meta.id == query || s.meta.id.starts_with(query))
        {
            return Some((hit.session_dir, hit.meta));
        }
        self.list_all_sessions()
            .into_iter()
            .find(|s| s.meta.id == query || s.meta.id.starts_with(query))
            .map(|s| (s.session_dir, s.meta))
    }

    /// Allocate a unique branch WAL path under `<session_dir>/branches/`.
    pub fn next_branch_wal_path(&self, session_dir: &Path, name_hint: Option<&str>) -> PathBuf {
        let branches_dir = session_dir.join("branches");
        let _ = ensure_dir_0700(&branches_dir);
        let slug = name_hint
            .map(sanitize_slug)
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| format!("fork-{}", &Uuid::new_v4().simple().to_string()[..6]));
        let mut candidate = branches_dir.join(format!("{slug}.wal.jsonl"));
        if candidate.exists() {
            candidate = branches_dir.join(format!(
                "{slug}-{}.wal.jsonl",
                &Uuid::new_v4().simple().to_string()[..4]
            ));
        }
        candidate
    }
}

pub fn save_session_meta(session_dir: &Path, meta: &SessionMeta) -> io::Result<()> {
    let dest = session_dir.join("meta.json");
    let tmp = session_dir.join(format!(".meta_{}.tmp", Uuid::new_v4().simple()));
    let json = serde_json::to_string_pretty(meta)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    let mut f = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&tmp)?;
    f.write_all(json.as_bytes())?;
    f.sync_data()?;
    drop(f);
    fs::rename(&tmp, &dest).inspect_err(|_| {
        let _ = fs::remove_file(&tmp);
    })
}

/// Resolve the Layer 1 `anix` executable path (`--anix-bin` > `$ANIX_BIN` > `$PATH` > `~/.local/bin/anix` > sibling).
pub fn resolve_anix_bin(explicit: Option<&Path>) -> io::Result<PathBuf> {
    if let Some(p) = explicit {
        if is_executable(p) {
            return Ok(p.to_path_buf());
        }
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("explicit --anix-bin `{}` is not executable", p.display()),
        ));
    }
    if let Ok(env_p) = env::var("ANIX_BIN")
        && !env_p.trim().is_empty()
    {
        let p = PathBuf::from(env_p.trim());
        if is_executable(&p) {
            return Ok(p);
        }
    }
    if let Some(path_var) = env::var_os("PATH") {
        for dir in env::split_paths(&path_var) {
            let cand = dir.join("anix");
            if is_executable(&cand) {
                return Ok(cand);
            }
        }
    }
    if let Some(home) = env::var_os("HOME") {
        let local = PathBuf::from(home).join(".local/bin/anix");
        if is_executable(&local) {
            return Ok(local);
        }
    }
    if let Ok(exe) = env::current_exe()
        && let Some(dir) = exe.parent()
    {
        let sibling = dir.join("anix");
        if is_executable(&sibling) {
            return Ok(sibling);
        }
    }
    Err(io::Error::new(
        io::ErrorKind::NotFound,
        "Layer 1 `anix` binary not found in $PATH or ~/.local/bin/anix (install from https://github.com/Luthics/anix or pass --anix-bin)",
    ))
}

pub fn now_rfc3339() -> String {
    OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .unwrap_or_else(|_| "1970-01-01T00:00:00Z".into())
}

/// Format an RFC 3339 UTC timestamp (`YYYY-MM-DDTHH:MM:SS...Z`) into a crisp relative label
/// (`just now`, `5m ago`, `3h ago`, `2d ago`).
pub fn format_relative_time(rfc3339: &str) -> String {
    let Some(ts_secs) = parse_rfc3339_utc_secs(rfc3339) else {
        return rfc3339.chars().take(10).collect();
    };
    let now_secs = OffsetDateTime::now_utc().unix_timestamp();
    let diff = now_secs.saturating_sub(ts_secs);
    if diff < 60 {
        "just now".into()
    } else if diff < 3600 {
        format!("{}m ago", diff / 60)
    } else if diff < 86_400 {
        format!("{}h ago", diff / 3600)
    } else if diff < 86_400 * 30 {
        format!("{}d ago", diff / 86_400)
    } else {
        rfc3339.chars().take(10).collect()
    }
}

fn parse_rfc3339_utc_secs(s: &str) -> Option<i64> {
    if s.len() < 19 {
        return None;
    }
    let y: i32 = s.get(0..4)?.parse().ok()?;
    let mo: u8 = s.get(5..7)?.parse().ok()?;
    let d: u8 = s.get(8..10)?.parse().ok()?;
    let h: u8 = s.get(11..13)?.parse().ok()?;
    let mi: u8 = s.get(14..16)?.parse().ok()?;
    let sec: u8 = s.get(17..19)?.parse().ok()?;
    let month = time::Month::try_from(mo).ok()?;
    let date = time::Date::from_calendar_date(y, month, d).ok()?;
    let t = time::Time::from_hms(h, mi, sec).ok()?;
    Some(date.with_time(t).assume_utc().unix_timestamp())
}

fn read_sessions_in_dir(project_dir: &Path, is_current_project: bool) -> Vec<SessionSummary> {
    let Ok(entries) = fs::read_dir(project_dir) else {
        return Vec::new();
    };
    let mut list = Vec::new();
    for entry in entries.flatten() {
        let dir = entry.path();
        if !dir.is_dir() {
            continue;
        }
        let meta_path = dir.join("meta.json");
        let Ok(raw) = fs::read_to_string(&meta_path) else {
            continue;
        };
        let Ok(meta) = serde_json::from_str::<SessionMeta>(&raw) else {
            continue;
        };
        let wal_bytes = fs::metadata(&meta.active_wal).map(|m| m.len()).unwrap_or(0);
        let turn_count = count_wal_steps(&meta.active_wal);
        if is_session_empty(&dir, &meta) {
            continue;
        }
        let project_label = meta
            .cwd
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| meta.cwd.display().to_string());
        list.push(SessionSummary {
            meta,
            session_dir: dir,
            wal_bytes,
            turn_count,
            project_label,
            is_current_project,
        });
    }
    list
}

/// Returns `true` when a session directory has no user/assistant/tool messages yet and no forked
/// branch WALs (e.g. a freshly opened workbench where the user immediately ran `/resume` or quit).
pub fn is_session_empty(session_dir: &Path, meta: &SessionMeta) -> bool {
    if count_wal_steps(&meta.active_wal) > 0 {
        return false;
    }
    if let Ok(raw) = fs::read_to_string(&meta.active_wal)
        && raw.contains("\"type\":\"branch_base\"")
    {
        return false;
    }
    let branches_dir = session_dir.join("branches");
    if let Ok(entries) = fs::read_dir(&branches_dir)
        && entries.flatten().any(|e| {
            e.path()
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.ends_with(".wal.jsonl"))
        })
    {
        return false;
    }
    true
}

fn count_wal_steps(wal_path: &Path) -> usize {
    let Ok(raw) = fs::read_to_string(wal_path) else {
        return 0;
    };
    raw.lines()
        .filter(|l| l.contains("\"type\":\"message\"") && !l.contains("\"role\":\"system\""))
        .count()
}

fn scan_markdown_commands(
    dir: &Path,
    tag: &'static str,
    seen: &mut std::collections::HashSet<String>,
    out: &mut Vec<CustomSlashEntry>,
) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let p = entry.path();
        if !p.is_file() || p.extension().and_then(|e| e.to_str()) != Some("md") {
            continue;
        }
        let Some(stem) = p.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        let slash_name = format!("/{stem}");
        if !seen.insert(slash_name.clone()) {
            continue;
        }
        let desc = extract_md_description(&p)
            .unwrap_or_else(|| format!("Custom command from {}", p.display()));
        out.push(CustomSlashEntry {
            slash_name,
            description: format!("{desc} ({tag})"),
            source_tag: tag,
            kind: CustomSlashKind::Command(p),
        });
    }
}

fn scan_skill_dirs(
    dir: &Path,
    tag: &'static str,
    seen: &mut std::collections::HashSet<String>,
    out: &mut Vec<CustomSlashEntry>,
) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let sub = entry.path();
        if !sub.is_dir() {
            continue;
        }
        let skill_md = sub.join("SKILL.md");
        if !skill_md.is_file() {
            continue;
        }
        let Some(name) = sub.file_name().and_then(|s| s.to_str()) else {
            continue;
        };
        let slash_name = format!("/{name}");
        if !seen.insert(slash_name.clone()) {
            continue;
        }
        let desc =
            extract_md_description(&skill_md).unwrap_or_else(|| format!("Run skill `{name}`"));
        out.push(CustomSlashEntry {
            slash_name,
            description: format!("{desc} ({tag})"),
            source_tag: tag,
            kind: CustomSlashKind::Skill(skill_md),
        });
    }
}

fn extract_md_description(path: &Path) -> Option<String> {
    let raw = fs::read_to_string(path).ok()?;
    let trimmed = raw.trim_start();
    if let Some(rest) = trimmed.strip_prefix("---")
        && let Some((fm, body)) = rest.split_once("\n---")
    {
        for line in fm.lines() {
            let l = line.trim();
            if let Some(v) = l.strip_prefix("description:") {
                let clean = v.trim().trim_matches(|c| c == '"' || c == '\'');
                if !clean.is_empty() {
                    return Some(clean.chars().take(80).collect());
                }
            }
        }
        return first_meaningful_line(body);
    }
    first_meaningful_line(trimmed)
}

fn first_meaningful_line(body: &str) -> Option<String> {
    for line in body.lines() {
        let clean = line.trim().trim_start_matches('#').trim();
        if !clean.is_empty() {
            return Some(clean.chars().take(80).collect());
        }
    }
    None
}

pub fn ensure_dir_0700(dir: &Path) -> io::Result<()> {
    fs::create_dir_all(dir)?;
    fs::set_permissions(dir, fs::Permissions::from_mode(0o700))?;
    Ok(())
}

pub fn is_executable(path: &Path) -> bool {
    fs::metadata(path)
        .map(|m| m.is_file() && (m.permissions().mode() & 0o111 != 0))
        .unwrap_or(false)
}

fn expand_tilde(raw: &str, home_dir: &Path) -> PathBuf {
    if raw == "~" {
        return home_dir.to_path_buf();
    }
    if let Some(rest) = raw.strip_prefix("~/") {
        return home_dir.join(rest);
    }
    PathBuf::from(raw)
}

fn compute_project_slug(cwd: &Path) -> String {
    let base = cwd
        .file_name()
        .map(|s| sanitize_slug(&s.to_string_lossy()))
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "root".into());
    let mut hasher = Sha256::new();
    hasher.update(cwd.to_string_lossy().as_bytes());
    let digest = hasher.finalize();
    let hex: String = digest[..4].iter().map(|b| format!("{b:02x}")).collect();
    format!("{base}-{hex}")
}

fn sanitize_slug(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_') {
                c
            } else {
                '-'
            }
        })
        .collect::<String>()
        .trim_matches('-')
        .to_owned()
}
