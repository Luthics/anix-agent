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
max_tokens: 128000
compact_threshold: 0.8
---
You are an expert POSIX systems and software engineering agent running inside `anix-agent`.
Be concise, accurate, and pragmatic. Inspect before editing, verify changes with real commands/tests,
and summarize results clearly in Markdown.
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
    pub active_wal: PathBuf,
    pub active_leaf: Option<u64>,
    pub approval_policy: ApprovalPolicy,
}

/// Summary row shown in `/sessions`.
#[derive(Debug, Clone)]
pub struct SessionSummary {
    pub meta: SessionMeta,
    pub session_dir: PathBuf,
    pub wal_bytes: u64,
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

    /// Create a brand-new persistent session directory (`main.wal.jsonl`, `branches/`, `inbox/`, `meta.json`).
    pub fn create_session(
        &self,
        agent_path: PathBuf,
        model: Option<String>,
        provider: Option<String>,
        approval_policy: ApprovalPolicy,
    ) -> io::Result<(PathBuf, SessionMeta)> {
        let id = format!("s-{}", &Uuid::new_v4().simple().to_string()[..10]);
        let session_dir = self.project_sessions_dir().join(&id);
        ensure_dir_0700(&session_dir)?;
        ensure_dir_0700(&session_dir.join("branches"))?;
        ensure_dir_0700(&session_dir.join("inbox"))?;
        ensure_dir_0700(&session_dir.join("workers"))?;

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
            active_wal: session_dir.join("main.wal.jsonl"),
            active_leaf: None,
            approval_policy,
        };
        save_session_meta(&session_dir, &meta)?;
        Ok((session_dir, meta))
    }

    /// List all sessions for the current working directory, newest first.
    pub fn list_sessions(&self) -> Vec<SessionSummary> {
        let root = self.project_sessions_dir();
        let Ok(entries) = fs::read_dir(&root) else {
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
            list.push(SessionSummary {
                meta,
                session_dir: dir,
                wal_bytes,
            });
        }
        list.sort_by(|a, b| b.meta.updated_at.cmp(&a.meta.updated_at));
        list
    }

    /// Find a session by `"latest"` or exact/prefix ID match.
    pub fn find_session(&self, query: &str) -> Option<(PathBuf, SessionMeta)> {
        let sessions = self.list_sessions();
        if query == "latest" {
            return sessions.into_iter().next().map(|s| (s.session_dir, s.meta));
        }
        sessions
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
