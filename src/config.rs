//! Layer 3 persistent user configuration (`${XDG_CONFIG_HOME:-~/.config}/anix-agent/config.json`),
//! analogous to `~/.claude.json` / `~/.claude/settings.json`.
//!
//! Strictly scoped to interactive Layer 3 UX preferences (leaving Layer 1 kernel settings such as
//! providers, models, and API keys in `~/.config/anix/config.toml`).

use std::env;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::cli::ApprovalPolicy;
use crate::session::ensure_dir_0700;
use crate::statusbar::{DEFAULT_STATUS_BAR, resolve_preset};

/// Behavior after an inline `! <cmd>` direct shell command completes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BangMode {
    /// Run locally and display output only; do NOT trigger or send to the model.
    #[default]
    Silent,
    /// Run locally; do NOT immediately wake the model, but attach output to your next prompt.
    Attach,
    /// Run locally and immediately deliver command + output to the worker's Maildir to trigger the model.
    Trigger,
}

impl BangMode {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Silent => "silent",
            Self::Attach => "attach",
            Self::Trigger => "trigger",
        }
    }

    pub fn parse_str(raw: &str) -> Result<Self, String> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "silent" | "off" | "false" | "none" => Ok(Self::Silent),
            "attach" | "context" => Ok(Self::Attach),
            "trigger" | "auto" | "on" | "true" => Ok(Self::Trigger),
            other => Err(format!(
                "invalid bang_mode `{other}` (expected silent|attach|trigger)"
            )),
        }
    }
}

/// Persistent Layer 3 user configuration stored at `~/.config/anix-agent/config.json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct AgentConfig {
    /// Default shell approval policy when `--mode` is not explicitly passed on CLI (`ask|auto|plan`).
    pub default_mode: ApprovalPolicy,
    /// What to do after `! <cmd>` finishes (`silent` | `attach` | `trigger`).
    pub bang_mode: BangMode,
    /// Whether thinking blocks and tool outputs start expanded (`Ctrl+O`).
    pub expand_details: bool,
    /// Whether bare `anix-agent` automatically resumes the most recent session in `cwd`.
    pub auto_resume_last: bool,
    /// Number of recent turns replayed on `-r` / `/resume`.
    pub replay_turns: usize,
    /// Shell executable used to run inline `! <cmd>` commands (defaults to `$SHELL` or `/bin/sh`).
    pub bang_shell: String,
    /// Persistent allowlist of command basenames auto-approved in `ask` mode.
    pub always_allowed_commands: Vec<String>,
    /// DIY status bar template rendered dynamically with `{var}` placeholders (see `/statusbar vars`).
    pub status_bar: String,
}

impl Default for AgentConfig {
    fn default() -> Self {
        let bang_shell = env::var("SHELL")
            .ok()
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| "/bin/sh".into());
        Self {
            default_mode: ApprovalPolicy::Ask,
            bang_mode: BangMode::Silent,
            expand_details: false,
            auto_resume_last: false,
            replay_turns: 8,
            bang_shell,
            always_allowed_commands: Vec::new(),
            status_bar: DEFAULT_STATUS_BAR.to_owned(),
        }
    }
}

impl AgentConfig {
    /// Resolve `${XDG_CONFIG_HOME:-~/.config}/anix-agent/config.json`.
    pub fn default_path() -> io::Result<PathBuf> {
        let config_root = match env::var_os("XDG_CONFIG_HOME").filter(|v| !v.is_empty()) {
            Some(dir) => PathBuf::from(dir),
            None => {
                let home = env::var_os("HOME")
                    .map(PathBuf::from)
                    .ok_or_else(|| io::Error::other("HOME environment variable is not set"))?;
                home.join(".config")
            }
        };
        Ok(config_root.join("anix-agent/config.json"))
    }

    /// Load config from `path`, or materialize defaults (chmod `0600`) if missing.
    pub fn load_or_init_at(path: &Path) -> io::Result<Self> {
        if path.is_file() {
            let raw = fs::read_to_string(path)?;
            let cfg: Self = serde_json::from_str(&raw).map_err(|e| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("invalid JSON in `{}`: {e}", path.display()),
                )
            })?;
            return Ok(cfg);
        }
        let cfg = Self::default();
        cfg.save_to(path)?;
        Ok(cfg)
    }

    /// Load or create the default user config file, returning `(path, config)`.
    pub fn load_or_init() -> (PathBuf, Self) {
        let Ok(path) = Self::default_path() else {
            return (
                PathBuf::from("~/.config/anix-agent/config.json"),
                Self::default(),
            );
        };
        match Self::load_or_init_at(&path) {
            Ok(cfg) => (path, cfg),
            Err(_) => (path, Self::default()),
        }
    }

    /// Atomically write config JSON to `path` (`tmp -> fsync -> rename`, mode `0600`).
    pub fn save_to(&self, path: &Path) -> io::Result<()> {
        let Some(parent) = path.parent() else {
            return Err(io::Error::other("invalid config path"));
        };
        ensure_dir_0700(parent)?;
        let tmp = parent.join(format!(".config_{}.tmp", Uuid::new_v4().simple()));
        let mut json = serde_json::to_string_pretty(self)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        json.push('\n');
        let mut f = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&tmp)?;
        f.write_all(json.as_bytes())?;
        f.sync_data()?;
        drop(f);
        fs::rename(&tmp, path).inspect_err(|_| {
            let _ = fs::remove_file(&tmp);
        })
    }

    /// Mutate a named configuration key via `/config <key> <value>`.
    pub fn set_field(&mut self, key: &str, value: &str) -> Result<String, String> {
        match key.trim() {
            "bang_mode" => {
                let m = BangMode::parse_str(value)?;
                self.bang_mode = m;
                Ok(format!("bang_mode = \"{}\"", m.as_str()))
            }
            "bang_trigger_model" => {
                let m = BangMode::parse_str(value)?;
                self.bang_mode = m;
                Ok(format!("bang_mode = \"{}\"", m.as_str()))
            }
            "default_mode" => {
                let p = ApprovalPolicy::parse_str(value)?;
                self.default_mode = p;
                Ok(format!("default_mode = \"{}\"", p.as_str()))
            }
            "expand_details" => {
                let b = parse_bool(value)?;
                self.expand_details = b;
                Ok(format!("expand_details = {b}"))
            }
            "auto_resume_last" => {
                let b = parse_bool(value)?;
                self.auto_resume_last = b;
                Ok(format!("auto_resume_last = {b}"))
            }
            "replay_turns" => {
                let n: usize = value
                    .trim()
                    .parse()
                    .map_err(|_| format!("invalid integer `{value}`"))?;
                self.replay_turns = n.min(200);
                Ok(format!("replay_turns = {}", self.replay_turns))
            }
            "bang_shell" => {
                let s = value.trim();
                if s.is_empty() {
                    return Err("bang_shell cannot be empty".into());
                }
                self.bang_shell = s.to_owned();
                Ok(format!("bang_shell = \"{s}\""))
            }
            "status_bar" => {
                let tpl = resolve_preset(value)
                    .map(ToOwned::to_owned)
                    .unwrap_or_else(|| value.to_owned());
                if tpl.trim().is_empty() {
                    return Err("status_bar template cannot be empty".into());
                }
                self.status_bar = tpl.clone();
                Ok(format!("status_bar = \"{tpl}\""))
            }
            other => Err(format!(
                "unknown config key `{other}` (valid: bang_mode, default_mode, status_bar, auto_resume_last, expand_details, replay_turns, bang_shell)"
            )),
        }
    }
}

fn parse_bool(raw: &str) -> Result<bool, String> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "true" | "1" | "yes" | "on" => Ok(true),
        "false" | "0" | "no" | "off" => Ok(false),
        other => Err(format!("invalid boolean `{other}` (expected true|false)")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_round_trips_and_sets_bang_mode() {
        let dir = PathBuf::from(format!(
            "/tmp/ax-cfg-{}",
            &Uuid::new_v4().simple().to_string()[..8]
        ));
        let path = dir.join("config.json");
        let mut cfg = AgentConfig::load_or_init_at(&path).unwrap();
        assert_eq!(cfg.bang_mode, BangMode::Silent);
        assert_eq!(cfg.default_mode, ApprovalPolicy::Ask);

        cfg.set_field("bang_mode", "trigger").unwrap();
        cfg.set_field("auto_resume_last", "true").unwrap();
        cfg.save_to(&path).unwrap();

        let reloaded = AgentConfig::load_or_init_at(&path).unwrap();
        assert_eq!(reloaded.bang_mode, BangMode::Trigger);
        assert!(reloaded.auto_resume_last);

        let _ = fs::remove_dir_all(&dir);
    }
}
