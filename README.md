# anix-agent

**Lightweight POSIX-native interactive terminal workbench (Layer 3)** for the [Anix](https://github.com/Luthics/anix) OS Agent kernel (`v0.4.0+`).

`anix-agent` strictly obeys the Layer 3 boundary: it never embeds LLM SDKs, WAL storage engines, or shell executors. Instead, it drives the pure 3-primitive `anix` kernel (`*.agent` + Tree WAL + Maildir) exclusively through its documented POSIX external wire protocols:

* **Inline streaming terminal (`ratatui::Viewport::Inline` + `insert_before`)**: zero Alt-Screen hijack, preserving native terminal scrollback and mouse text selection, with live Markdown rendering, collapsible Thinking/Tool previews, multi-line CJK-aware input, and a live status bar (`model` / `ctx` watermark / `usage` / `leaf` / `holds` / `workers`).
* **Non-conflicting interactive approval (`approve-wrap`)**: wraps the kernel's `--shell` chain (`anix-agent approve-wrap --ipc <sock> -- msh`) over a private Unix domain socket so interactive `[y] Allow / [a] Always / [n] Deny / [e] Edit` and `Shift+Tab` mode switching (`ask` / `auto` / `plan`) work smoothly inside the TUI without fighting over `/dev/tty`.
* **Tree WAL branching & session management**: `/tree` renders the full Tree WAL graph (including cross-file `branch_base` chains) to switch `--leaf <id>` in place or spawn parallel branches via `anix fork`; `/compact` triggers online compaction via `Anix-Control: compact`; `/sessions` and `-r` resume prior sessions.
* **Multi-agent Maildir orchestration**: `/spawn <agent> [--fork [<leaf>]] [prompt]` launches parallel `--inbox` workers with `--notify` callbacks; `/workers` (`Ctrl+W`) monitors workers, active `hold` leases, and lets you switch views or dispatch messages via Maildir.

## Requirements

* `anix` (`v0.4.0+`) installed in `$PATH` or `~/.local/bin/anix` (from [Luthics/anix](https://github.com/Luthics/anix)).

## Build & Install

```sh
cargo build --release
install -m 0755 target/release/anix-agent ~/.local/bin/anix-agent
```

## Quick Start

```sh
# Launch interactive workbench in current directory
anix-agent

# Launch with a specific *.agent script or model override
anix-agent ~/.config/anix/agents/bash.agent --model deepseek-v4.1-flash

# Resume the most recent session for this project
anix-agent -r

# Start directly in Auto or Plan (read-only) approval mode
anix-agent --mode auto
anix-agent --mode plan
```

## Slash Commands & Keybindings

| Command / Key | Action |
| :--- | :--- |
| `/help` | Show available commands and keybindings |
| `/tree` | Browse Tree WAL branch graph (`Enter`: switch `--leaf`, `f`: `anix fork`) |
| `/fork [name] [--leaf <id>]` | Fork current branch into a new WAL file and optionally switch/spawn |
| `/compact` | Trigger online context compaction via Maildir (`Anix-Control: compact`) |
| `/sessions` | Browse and switch/resume project sessions |
| `/spawn <agent> [--fork] [prompt]` | Spawn a parallel Maildir worker agent (`--notify` wired to main inbox) |
| `/workers` / `Ctrl+W` | Open worker & active `hold` lease drawer (`Enter`: focus, `k`: stop) |
| `/send <worker> <msg>` | Deliver a Maildir message to a spawned worker |
| `/inspect <node_id>` | Print full committed node/tool output into terminal scrollback |
| `/model <id>` | Switch model (restarts `anix` cleanly on the same WAL) |
| `/agent <path\|name>` | Switch active `*.agent` script |
| `/mode <ask\|auto\|plan>` / `Shift+Tab` | Switch approval policy live without restarting `anix` |
| `/new` | Start a brand-new session |
| `/quit` / `Ctrl+D` | Gracefully stop managed workers and exit |
| `Ctrl+O` | Toggle expanded/compact tool & thinking output |
| `Esc` / `Ctrl+C` | Close overlay or interrupt active inference turn (respawns cleanly on WAL) |
| `Alt+Enter` / `Ctrl+J` | Insert newline in input box |
| `Tab` | Autocomplete `/` slash commands or `@path` files |

## License

Apache-2.0 — see [LICENSE](LICENSE).
