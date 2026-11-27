//! CLI parsing for `anix-agent` interactive workbench and internal `approve-wrap` subcommand.

use std::path::PathBuf;

use clap::{Parser, Subcommand, ValueEnum};

/// Live approval policy enforced by `anix-agent`'s Layer 3 `approve-wrap` + UDS server.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Default, ValueEnum, serde::Serialize, serde::Deserialize,
)]
#[serde(rename_all = "lowercase")]
pub enum ApprovalPolicy {
    /// Auto-allow safe read-only pipelines & session `always` commands; prompt inline for others.
    #[default]
    Ask,
    /// Automatically approve all shell commands without prompting.
    Auto,
    /// Read-only planning mode: auto-allow safe read-only commands, deny mutating commands to model.
    Plan,
}

impl ApprovalPolicy {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Ask => "ask",
            Self::Auto => "auto",
            Self::Plan => "plan",
        }
    }

    pub const fn cycle_next(self) -> Self {
        match self {
            Self::Ask => Self::Auto,
            Self::Auto => Self::Plan,
            Self::Plan => Self::Ask,
        }
    }

    pub fn parse_str(raw: &str) -> Result<Self, String> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "ask" => Ok(Self::Ask),
            "auto" => Ok(Self::Auto),
            "plan" => Ok(Self::Plan),
            other => Err(format!("invalid mode `{other}` (expected ask|auto|plan)")),
        }
    }
}

#[derive(Debug, Parser)]
#[command(
    name = "anix-agent",
    version,
    about = "Lightweight POSIX-native interactive terminal workbench (Layer 3) for Anix",
    args_conflicts_with_subcommands = true
)]
pub struct Cli {
    /// Path to a `*.agent` script or named agent under `~/.config/anix/agents/`.
    pub agent: Option<String>,

    /// Resume the most recent session for this working directory (or a specific session ID).
    #[arg(short = 'r', long = "resume", num_args = 0..=1, default_missing_value = "latest")]
    pub resume: Option<String>,

    /// Explicit `config.toml` forwarded to `anix --config`.
    #[arg(long = "config", value_name = "FILE")]
    pub config: Option<PathBuf>,

    /// Override LLM provider name forwarded to `anix --provider`.
    #[arg(long = "provider")]
    pub provider: Option<String>,

    /// Override LLM model ID forwarded to `anix --model`.
    #[arg(long = "model")]
    pub model: Option<String>,

    /// Underlying shell binary wrapped by `approve-wrap` (default: `msh`; pass `none` for pure-text).
    #[arg(long = "shell")]
    pub shell: Option<String>,

    /// Initial approval policy (`ask`, `auto`, or `plan`). Defaults to `~/.config/anix-agent/config.json`.
    #[arg(long = "mode", value_enum)]
    pub mode: Option<ApprovalPolicy>,

    /// Explicit path to the Layer 1 `anix` executable (defaults to `$ANIX_BIN` / `$PATH` / `~/.local/bin/anix`).
    #[arg(long = "anix-bin", value_name = "PATH")]
    pub anix_bin: Option<PathBuf>,

    /// Initial prompt to send automatically once the session boots.
    #[arg(short = 'p', long = "prompt")]
    pub prompt: Option<String>,

    /// Enable Linux Landlock sandbox forwarded to `anix --sandbox`.
    #[arg(
        long = "sandbox",
        num_args = 0..=1,
        require_equals = true,
        default_missing_value = "best-effort"
    )]
    pub sandbox: Option<String>,

    #[command(subcommand)]
    pub command: Option<SubCommand>,
}

#[derive(Debug, Subcommand)]
pub enum SubCommand {
    /// Internal Layer 3 shell wrapper invoked by `anix` via `--shell "anix-agent approve-wrap --ipc <SOCK> -- <SHELL>"`.
    #[command(name = "approve-wrap")]
    ApproveWrap {
        /// Path to the parent `anix-agent` Unix domain socket.
        #[arg(long = "ipc", value_name = "SOCK")]
        ipc: PathBuf,
        /// Worker identifier shown in approval prompts.
        #[arg(long = "worker-id", default_value = "main")]
        worker_id: String,
        /// Wrapped shell command (`-- <SHELL_BIN> [SHELL_ARGS...] -c <CMD>`).
        #[arg(last = true, required = true)]
        wrapped: Vec<String>,
    },
}
