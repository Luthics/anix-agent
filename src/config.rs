//! Layer 3 persistent user configuration (`${XDG_CONFIG_HOME:-~/.config}/anix-agent/config.json`),
//! analogous to `~/.claude.json` / `~/.claude/settings.json`.
//!
//! Strictly scoped to interactive Layer 3 UX preferences (leaving Layer 1 kernel settings such as
//! providers, models, and API keys in `~/.config/anix/config.toml`).

use std::collections::BTreeMap;
use std::env;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::cli::ApprovalPolicy;
use crate::session::ensure_dir_0700;
use crate::statusbar::{DEFAULT_STATUS_BAR, LEGACY_DEFAULT_STATUS_BAR, resolve_preset};

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
    /// Recently used LLM model IDs (newest first) surfaced by `/model` picker & Tab completion.
    pub recent_models: Vec<String>,
    /// Preferred `reasoning_effort` per model ID (omitted or `"default"` = use provider default).
    #[serde(default)]
    pub model_efforts: BTreeMap<String, String>,
    /// Optional per-model reasoning effort mapping tables (`dsh`-inspired):
    /// key = `"<provider>:<model>"` | `"<model>"` | `"<provider>:<glob>"` | `"<glob>"`,
    /// value = map of logical tier (`"off"`, `"low"`, `"high"`, `"xhigh"`, `"max"`, ...)
    /// to `null` (omit), `"<str>"` (`reasoning_effort=<str>`), or `{"param": <json>}` object.
    #[serde(default)]
    pub model_effort_profiles: BTreeMap<String, BTreeMap<String, EffortMappingTarget>>,
    /// Emit terminal bell + OSC 9 notification when a turn exceeds this many seconds or requires
    /// shell approval (`0` disables notifications, default `10`).
    pub notify_after_secs: u64,
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
            recent_models: Vec::new(),
            model_efforts: BTreeMap::new(),
            model_effort_profiles: BTreeMap::new(),
            notify_after_secs: 10,
            status_bar: DEFAULT_STATUS_BAR.to_owned(),
        }
    }
}

/// Target wire mapping for a single logical reasoning effort tier in `model_effort_profiles`.
///
/// Supports three ergonomic JSON forms in `~/.config/anix-agent/config.json`:
/// - `null` -> omit effort params (use provider default / strip override)
/// - `"high"` -> shorthand for `{"reasoning_effort": "high"}`
/// - `{"thinking": {"type": "disabled"}}` -> arbitrary `anix --param KEY=VALUE` JSON pairs
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum EffortMappingTarget {
    /// Omit reasoning parameters (`null` in JSON).
    Omit,
    /// Shorthand mapping to `reasoning_effort=<string>` (e.g. `"high"`, `"none"`, `"max"`).
    Effort(String),
    /// Explicit map of `anix --param KEY=VALUE` JSON entries.
    Params(BTreeMap<String, serde_json::Value>),
}

impl EffortMappingTarget {
    /// Expand this mapping target into `(key, json_value)` pairs passed to `anix --param`.
    pub fn to_params(&self) -> Vec<(String, serde_json::Value)> {
        match self {
            Self::Omit => Vec::new(),
            Self::Effort(s) => {
                let t = s.trim();
                if t.is_empty() || t.eq_ignore_ascii_case("default") {
                    Vec::new()
                } else {
                    vec![(
                        "reasoning_effort".into(),
                        serde_json::Value::String(t.to_owned()),
                    )]
                }
            }
            Self::Params(map) => map
                .iter()
                .filter(|(_, v)| !v.is_null())
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect(),
        }
    }

    /// Concise human-readable wire preview shown in `/model` overlay and Tab completion.
    pub fn wire_preview(&self) -> String {
        let params = self.to_params();
        if params.is_empty() {
            return "(omit / null)".into();
        }
        params
            .into_iter()
            .map(|(k, v)| match v {
                serde_json::Value::String(s) => format!("{k}={s}"),
                other => format!("{k}={other}"),
            })
            .collect::<Vec<_>>()
            .join(", ")
    }
}

/// Resolved per-model reasoning effort spectrum and wire mapping table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedEffortProfile {
    /// Origin badge displayed in `/model` preview (e.g. `config: deepseek-*` or `builtin: deepseek-compat`).
    pub source_label: String,
    /// Ordered logical tiers (excluding `"default"`, which is always prepended at index 0).
    pub tiers: Vec<(String, EffortMappingTarget)>,
}

impl ResolvedEffortProfile {
    /// Ordered effort options for the `/model` slider (`"default"` first), preserving any
    /// custom saved `current` value so users who ran `/model effort <custom>` never lose it.
    pub fn levels_with_current(&self, current: Option<&str>) -> Vec<String> {
        let mut levels = Vec::with_capacity(self.tiers.len() + 2);
        levels.push("default".to_owned());
        for (name, _) in &self.tiers {
            if !name.eq_ignore_ascii_case("default")
                && !levels.iter().any(|l| l.eq_ignore_ascii_case(name))
            {
                levels.push(name.clone());
            }
        }
        if let Some(cur) = current.map(str::trim).filter(|s| !s.is_empty())
            && !cur.eq_ignore_ascii_case("default")
            && !levels.iter().any(|l| l.eq_ignore_ascii_case(cur))
        {
            levels.push(cur.to_ascii_lowercase());
        }
        levels
    }

    /// Resolve a logical effort tier to its wire target (`None` means use provider default).
    pub fn resolve_target(&self, effort: Option<&str>) -> Option<EffortMappingTarget> {
        let norm = effort.and_then(normalize_effort_input)?;
        if let Some((_, target)) = self
            .tiers
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case(&norm))
        {
            Some(target.clone())
        } else {
            // Custom tier entered manually via `/model effort <custom>`: pass through verbatim.
            Some(EffortMappingTarget::Effort(norm))
        }
    }

    /// Resolve a logical effort tier directly into `(key, value)` pairs for `anix --param`.
    pub fn resolve_params(&self, effort: Option<&str>) -> Vec<(String, serde_json::Value)> {
        self.resolve_target(effort)
            .map(|t| t.to_params())
            .unwrap_or_default()
    }

    /// Human-readable wire preview string for `/model` overlay and completion hints.
    pub fn wire_preview(&self, effort: Option<&str>) -> String {
        match self.resolve_target(effort) {
            None => "(provider default)".into(),
            Some(target) => target.wire_preview(),
        }
    }
}

/// Normalize user-entered reasoning effort (`"default"` / `"auto"` / `"-"` / `"reset"` -> `None`).
pub fn normalize_effort_input(raw: &str) -> Option<String> {
    let lowered = raw.trim().to_ascii_lowercase();
    if lowered.is_empty() || matches!(lowered.as_str(), "default" | "auto" | "-" | "reset") {
        None
    } else {
        Some(lowered)
    }
}

/// Built-in provider-aware + model-family fallback effort profile when no rule in
/// `config.json` `model_effort_profiles` matches `(provider, model)`.
pub fn builtin_effort_profile(provider: Option<&str>, model: &str) -> ResolvedEffortProfile {
    let p = provider.unwrap_or("").trim().to_ascii_lowercase();
    let m = model.trim().to_ascii_lowercase();
    let is_responses = p.contains("responses");
    let is_anthropic = p.contains("anthropic") || m.contains("claude");

    let effort_tier = |name: &str, wire: &str| {
        (
            name.to_owned(),
            EffortMappingTarget::Effort(wire.to_owned()),
        )
    };
    let params_tier = |name: &str, entries: Vec<(&str, serde_json::Value)>| {
        let map = entries
            .into_iter()
            .map(|(k, v)| (k.to_owned(), v))
            .collect();
        (name.to_owned(), EffortMappingTarget::Params(map))
    };

    if is_anthropic {
        ResolvedEffortProfile {
            source_label: "builtin: anthropic".into(),
            tiers: vec![
                effort_tier("off", "off"),
                effort_tier("low", "low"),
                effort_tier("medium", "medium"),
                effort_tier("high", "high"),
                effort_tier("max", "max"),
            ],
        }
    } else if m.contains("deepseek") {
        if is_responses {
            ResolvedEffortProfile {
                source_label: "builtin: deepseek-responses".into(),
                tiers: vec![
                    effort_tier("off", "none"),
                    effort_tier("low", "low"),
                    effort_tier("medium", "medium"),
                    effort_tier("high", "high"),
                    effort_tier("xhigh", "xhigh"),
                    effort_tier("max", "max"),
                ],
            }
        } else {
            // DeepSeek Chat Completions / `openai-compat`: uses `thinking.type` + `reasoning_effort`.
            ResolvedEffortProfile {
                source_label: "builtin: deepseek-compat".into(),
                tiers: vec![
                    params_tier(
                        "off",
                        vec![("thinking", serde_json::json!({ "type": "disabled" }))],
                    ),
                    params_tier(
                        "low",
                        vec![
                            ("thinking", serde_json::json!({ "type": "enabled" })),
                            ("reasoning_effort", serde_json::json!("low")),
                        ],
                    ),
                    params_tier(
                        "medium",
                        vec![
                            ("thinking", serde_json::json!({ "type": "enabled" })),
                            ("reasoning_effort", serde_json::json!("medium")),
                        ],
                    ),
                    params_tier(
                        "high",
                        vec![
                            ("thinking", serde_json::json!({ "type": "enabled" })),
                            ("reasoning_effort", serde_json::json!("high")),
                        ],
                    ),
                    params_tier(
                        "xhigh",
                        vec![
                            ("thinking", serde_json::json!({ "type": "enabled" })),
                            ("reasoning_effort", serde_json::json!("xhigh")),
                        ],
                    ),
                    params_tier(
                        "max",
                        vec![
                            ("thinking", serde_json::json!({ "type": "enabled" })),
                            ("reasoning_effort", serde_json::json!("max")),
                        ],
                    ),
                ],
            }
        }
    } else if m.starts_with("gpt-")
        || m.starts_with("o1")
        || m.starts_with("o3")
        || m.starts_with("o4")
        || m.contains("codex")
    {
        ResolvedEffortProfile {
            source_label: "builtin: openai".into(),
            tiers: vec![
                effort_tier("none", "none"),
                effort_tier("low", "low"),
                effort_tier("medium", "medium"),
                effort_tier("high", "high"),
                effort_tier("xhigh", "xhigh"),
            ],
        }
    } else if m.contains("qwen") && !is_responses {
        ResolvedEffortProfile {
            source_label: "builtin: qwen-compat".into(),
            tiers: vec![
                params_tier("off", vec![("enable_thinking", serde_json::json!(false))]),
                params_tier("on", vec![("enable_thinking", serde_json::json!(true))]),
            ],
        }
    } else if m.contains("glm") && !is_responses {
        ResolvedEffortProfile {
            source_label: "builtin: glm-compat".into(),
            tiers: vec![
                params_tier(
                    "off",
                    vec![("thinking", serde_json::json!({ "type": "disabled" }))],
                ),
                params_tier(
                    "on",
                    vec![("thinking", serde_json::json!({ "type": "enabled" }))],
                ),
            ],
        }
    } else if m.contains("gemini") {
        ResolvedEffortProfile {
            source_label: "builtin: gemini".into(),
            tiers: vec![
                effort_tier("off", "none"),
                effort_tier("low", "low"),
                effort_tier("medium", "medium"),
                effort_tier("high", "high"),
                effort_tier("max", "max"),
            ],
        }
    } else {
        ResolvedEffortProfile {
            source_label: "builtin: generic".into(),
            tiers: vec![
                effort_tier("off", if is_responses { "none" } else { "off" }),
                effort_tier("low", "low"),
                effort_tier("medium", "medium"),
                effort_tier("high", "high"),
                effort_tier("xhigh", "xhigh"),
                effort_tier("max", "max"),
            ],
        }
    }
}

fn effort_sort_rank(name: &str) -> u8 {
    match name.trim().to_ascii_lowercase().as_str() {
        "default" => 0,
        "off" | "none" | "disabled" => 1,
        "minimal" | "min" => 2,
        "low" => 3,
        "medium" | "med" => 4,
        "on" | "enabled" => 5,
        "high" => 6,
        "xhigh" => 7,
        "max" => 8,
        "ultra" => 9,
        _ => 50,
    }
}

/// Simple case-insensitive glob matcher supporting `*` and `?`.
fn glob_match_ci(pattern: &str, text: &str) -> bool {
    let p: Vec<char> = pattern.to_ascii_lowercase().chars().collect();
    let t: Vec<char> = text.to_ascii_lowercase().chars().collect();
    let (mut pi, mut ti) = (0usize, 0usize);
    let (mut star_pi, mut star_ti) = (None, 0usize);

    while ti < t.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == t[ti]) {
            pi += 1;
            ti += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star_pi = Some(pi);
            star_ti = ti;
            pi += 1;
        } else if let Some(sp) = star_pi {
            pi = sp + 1;
            star_ti += 1;
            ti = star_ti;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
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
            let mut cfg: Self = serde_json::from_str(&raw).map_err(|e| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("invalid JSON in `{}`: {e}", path.display()),
                )
            })?;
            // Gently upgrade unmodified built-in default template so `{effort}` appears cleanly.
            if cfg.status_bar == LEGACY_DEFAULT_STATUS_BAR {
                cfg.status_bar = DEFAULT_STATUS_BAR.to_owned();
            }
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

    /// Record a model ID at the front of `recent_models` (deduped, capped at 20).
    /// Returns `true` if `recent_models` changed.
    pub fn record_recent_model(&mut self, model: &str) -> bool {
        let trimmed = model.trim();
        if trimmed.is_empty() || trimmed == "default" || trimmed == "-" {
            return false;
        }
        if self.recent_models.first().map(String::as_str) == Some(trimmed) {
            return false;
        }
        self.recent_models.retain(|m| m != trimmed);
        self.recent_models.insert(0, trimmed.to_owned());
        self.recent_models.truncate(20);
        true
    }

    /// Remove a model ID from `recent_models`. Returns `true` if removed.
    pub fn remove_recent_model(&mut self, model: &str) -> bool {
        let before = self.recent_models.len();
        self.recent_models.retain(|m| m != model);
        self.recent_models.len() != before
    }

    /// Look up the saved reasoning effort for `model` (`None` = use model/provider default).
    pub fn effort_for_model(&self, model: &str) -> Option<&str> {
        let key = model.trim();
        if key.is_empty() || key == "default" || key == "-" {
            return None;
        }
        self.model_efforts
            .get(key)
            .map(String::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty() && !s.eq_ignore_ascii_case("default"))
    }

    /// Save (or clear when `None`) the preferred reasoning effort for `model`.
    /// Returns `true` if `model_efforts` changed.
    pub fn set_model_effort(&mut self, model: &str, effort: Option<&str>) -> bool {
        let key = model.trim();
        if key.is_empty() || key == "default" || key == "-" {
            return false;
        }
        match effort.and_then(normalize_effort_input) {
            Some(norm) => {
                if self.model_efforts.get(key).map(String::as_str) == Some(norm.as_str()) {
                    return false;
                }
                self.model_efforts.insert(key.to_owned(), norm);
                true
            }
            None => self.model_efforts.remove(key).is_some(),
        }
    }

    /// Resolve the effective per-model reasoning effort mapping profile for `(provider, model)`.
    ///
    /// Precedence:
    /// 1. User `model_effort_profiles`: exact `provider:model` > exact `model` > `provider:glob` > `glob`
    /// 2. Built-in provider-aware + model-family default profile ([`builtin_effort_profile`]).
    pub fn resolve_effort_profile(
        &self,
        provider: Option<&str>,
        model: &str,
    ) -> ResolvedEffortProfile {
        let model_trimmed = model.trim();
        let prov_trimmed = provider
            .map(str::trim)
            .filter(|s| !s.is_empty() && *s != "-");

        type ProfileMatch<'a> = (
            (u8, usize),
            &'a str,
            &'a BTreeMap<String, EffortMappingTarget>,
        );
        let mut best: Option<ProfileMatch<'_>> = None;

        for (raw_pat, mapping) in &self.model_effort_profiles {
            let pat = raw_pat.trim();
            if pat.is_empty() {
                continue;
            }
            // Only interpret `<prefix>:<rest>` as a provider-qualified pattern when `<prefix>`
            // matches the active provider or a known built-in dialect; otherwise preserve colons
            // inside model IDs such as Ollama's `qwen3:32b` / `deepseek-r1:14b`.
            let (has_prov, model_pat) = if let Some((pfx, rest)) = pat.split_once(':') {
                let pfx_l = pfx.trim().to_ascii_lowercase();
                let is_known_dialect = matches!(
                    pfx_l.as_str(),
                    "openai-compat" | "openai-responses" | "anthropic"
                );
                let matches_active_prov =
                    prov_trimmed.is_some_and(|p| p.eq_ignore_ascii_case(&pfx_l));
                if matches_active_prov {
                    (true, rest.trim())
                } else if is_known_dialect {
                    // Explicitly qualified for a different provider dialect -> skip.
                    continue;
                } else {
                    (false, pat)
                }
            } else {
                (false, pat)
            };

            let is_glob = model_pat.contains('*') || model_pat.contains('?');
            let matched = if is_glob {
                glob_match_ci(model_pat, model_trimmed)
            } else {
                model_pat.eq_ignore_ascii_case(model_trimmed)
            };
            if !matched {
                continue;
            }

            let rank_tier = match (has_prov, is_glob) {
                (true, false) => 4,
                (false, false) => 3,
                (true, true) => 2,
                (false, true) => 1,
            };
            let literal_len = model_pat.chars().filter(|&c| c != '*' && c != '?').count();
            let score = (rank_tier, literal_len);
            if best.as_ref().is_none_or(|(b_score, _, _)| score > *b_score) {
                best = Some((score, pat, mapping));
            }
        }

        if let Some((_, pat, mapping)) = best {
            let mut tiers: Vec<(String, EffortMappingTarget)> = mapping
                .iter()
                .filter(|(k, _)| !k.trim().eq_ignore_ascii_case("default"))
                .map(|(k, v)| (k.trim().to_owned(), v.clone()))
                .collect();
            tiers.sort_by(|(a, _), (b, _)| {
                effort_sort_rank(a)
                    .cmp(&effort_sort_rank(b))
                    .then_with(|| a.cmp(b))
            });
            return ResolvedEffortProfile {
                source_label: format!("config: {pat}"),
                tiers,
            };
        }

        builtin_effort_profile(prov_trimmed, model_trimmed)
    }

    /// Resolve a logical reasoning effort level into concrete `anix --param` `(key, value)` pairs.
    pub fn resolve_effort_params(
        &self,
        provider: Option<&str>,
        model: &str,
        effort: Option<&str>,
    ) -> Vec<(String, serde_json::Value)> {
        self.resolve_effort_profile(provider, model)
            .resolve_params(effort)
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
            "notify_after_secs" => {
                let n: u64 = value
                    .trim()
                    .parse()
                    .map_err(|_| format!("invalid integer `{value}`"))?;
                self.notify_after_secs = n.min(3600);
                Ok(format!("notify_after_secs = {}", self.notify_after_secs))
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
                "unknown config key `{other}` (valid: bang_mode, default_mode, notify_after_secs, status_bar, auto_resume_last, expand_details, replay_turns, bang_shell)"
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
        assert_eq!(cfg.notify_after_secs, 10);

        cfg.set_field("bang_mode", "trigger").unwrap();
        cfg.set_field("auto_resume_last", "true").unwrap();
        cfg.set_field("notify_after_secs", "0").unwrap();
        cfg.save_to(&path).unwrap();

        let reloaded = AgentConfig::load_or_init_at(&path).unwrap();
        assert_eq!(reloaded.bang_mode, BangMode::Trigger);
        assert!(reloaded.auto_resume_last);
        assert_eq!(reloaded.notify_after_secs, 0);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn model_effort_profiles_and_per_model_persistence() {
        let mut cfg = AgentConfig::default();

        // Built-in provider-aware profiles
        let ds_compat = cfg.resolve_effort_profile(Some("openai-compat"), "deepseek-v4.1");
        assert_eq!(ds_compat.source_label, "builtin: deepseek-compat");
        assert_eq!(
            ds_compat.resolve_params(Some("off")),
            vec![("thinking".into(), serde_json::json!({ "type": "disabled" }))]
        );
        assert_eq!(
            ds_compat.resolve_params(Some("xhigh")),
            vec![
                ("reasoning_effort".into(), serde_json::json!("xhigh")),
                ("thinking".into(), serde_json::json!({ "type": "enabled" })),
            ]
        );

        let ds_resp = cfg.resolve_effort_profile(Some("openai-responses"), "deepseek-v4.1");
        assert_eq!(ds_resp.source_label, "builtin: deepseek-responses");
        assert_eq!(
            ds_resp.resolve_params(Some("off")),
            vec![("reasoning_effort".into(), serde_json::json!("none"))]
        );

        let gpt = cfg.resolve_effort_profile(Some("openai-responses"), "gpt-5.4");
        assert_eq!(
            gpt.levels_with_current(Some("minimal")),
            vec![
                "default", "none", "low", "medium", "high", "xhigh", "minimal"
            ]
        );

        let qwen = cfg.resolve_effort_profile(Some("openai-compat"), "qwen3-235b");
        assert_eq!(
            qwen.resolve_params(Some("off")),
            vec![("enable_thinking".into(), serde_json::json!(false))]
        );

        // User-defined `dsh`-style per-model mapping (null / shorthand / object) + precedence
        let raw_profiles = serde_json::json!({
            "deepseek-*": {
                "off": null,
                "low": "low",
                "medium": "high",
                "max": { "thinking": { "type": "enabled" }, "reasoning_effort": "max" }
            },
            "openai-compat:deepseek-v4-pro": {
                "off": { "thinking": { "type": "disabled" } },
                "ultra": { "extra_body": { "reasoning_effort": "max" } }
            },
            "deepseek-r1:14b": {
                "off": { "enable_thinking": false },
                "high": "high"
            }
        });
        cfg.model_effort_profiles = serde_json::from_value(raw_profiles).unwrap();

        let glob_ds = cfg.resolve_effort_profile(Some("openai-compat"), "deepseek-flash");
        assert_eq!(glob_ds.source_label, "config: deepseek-*");
        assert_eq!(
            glob_ds.levels_with_current(None),
            vec!["default", "off", "low", "medium", "max"]
        );
        assert!(glob_ds.resolve_params(Some("off")).is_empty());
        assert_eq!(
            glob_ds.resolve_params(Some("medium")),
            vec![("reasoning_effort".into(), serde_json::json!("high"))]
        );

        let exact_ds = cfg.resolve_effort_profile(Some("openai-compat"), "deepseek-v4-pro");
        assert_eq!(
            exact_ds.source_label,
            "config: openai-compat:deepseek-v4-pro"
        );
        assert_eq!(
            exact_ds.resolve_params(Some("ultra")),
            vec![(
                "extra_body".into(),
                serde_json::json!({ "reasoning_effort": "max" })
            )]
        );

        // Model ID containing a colon (Ollama style) still matches directly!
        let ollama_ds = cfg.resolve_effort_profile(Some("openai-compat"), "deepseek-r1:14b");
        assert_eq!(ollama_ds.source_label, "config: deepseek-r1:14b");
        assert_eq!(
            ollama_ds.resolve_params(Some("off")),
            vec![("enable_thinking".into(), serde_json::json!(false))]
        );

        assert!(cfg.set_model_effort("deepseek-v4.1", Some("XHIGH")));
        assert_eq!(cfg.effort_for_model("deepseek-v4.1"), Some("xhigh"));
        assert!(!cfg.set_model_effort("deepseek-v4.1", Some("xhigh")));
        assert!(cfg.set_model_effort("deepseek-v4.1", Some("default")));
        assert_eq!(cfg.effort_for_model("deepseek-v4.1"), None);
    }
}
