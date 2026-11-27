# anix-agent

**Lightweight POSIX-native interactive terminal workbench (Layer 3)** for the [Anix](https://github.com/Luthics/anix) OS Agent kernel (`v0.0.9+`).

`anix-agent` strictly obeys the Layer 3 boundary: it never embeds LLM SDKs, WAL storage engines, or shell executors. Instead, it drives the pure 3-primitive `anix` kernel (`*.agent` + Tree WAL + Maildir) exclusively through its documented POSIX external wire protocols:

* **Dynamic inline streaming terminal (`CjkSafeBackend` + `DynamicInlineTerminal`)**: zero Alt-Screen hijack, preserving native terminal scrollback and mouse text selection, dynamic height expansion, CJK-safe rendering, tree-style streaming (`❯`, `○ Thought`, `● shell(...)` + `⎿`), live Markdown rendering, and a DIY variable-interpolated status bar (`/statusbar`).
* **Non-conflicting interactive approval (`approve-wrap`)**: wraps the kernel's `--shell` chain (`anix-agent approve-wrap --ipc <sock> -- msh`) over a private Unix domain socket so interactive `[y] Allow / [a] Always / [n] Deny / [e] Edit` and `Shift+Tab` mode switching (`ask` / `auto` / `plan`) work smoothly inside the TUI without fighting over `/dev/tty`.
* **Tree WAL branching, rewind & session management**: `/tree` renders the full Tree WAL graph (including cross-file `branch_base` chains) to switch `--leaf <id>` or spawn parallel branches via `anix fork`; `/rewind` (or double-`Esc` on empty prompt) rewinds conversation turns via Tree WAL leaves; `/resume` provides a searchable multi-project conversation picker (`F2` rename, `F4` delete); `/compact` triggers online compaction via `Anix-Control: compact`; `/context` visualizes a 10×10 WAL token budget grid.
* **Direct shell & Unix input ergonomics**: `! <cmd>` / `!!<cmd>` execute shell commands directly with configurable post-run behavior (`silent` / `attach` / `trigger` in `~/.config/anix-agent/config.json`), alongside `/config` 3-tab settings, `Ctrl+G` (`$EDITOR`), `Ctrl+S` prompt stash, `Ctrl+Z` POSIX suspend, `\Enter` newline, `?` shortcut help, and custom slash commands/skills discovery.
* **Multi-agent Maildir orchestration**: `/spawn <agent> [--fork [<leaf>]] [prompt]` launches parallel `--inbox` workers with `--notify` callbacks; `/workers` (`Ctrl+W`) monitors workers, active `hold` leases, and lets you switch views or dispatch messages via Maildir.

## Requirements

* `anix` (`v0.0.9+`) installed in `$PATH` or `~/.local/bin/anix` (from [Luthics/anix](https://github.com/Luthics/anix)).

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
| `/help` / `?` | Show available commands and 3-column shortcut cheatsheet |
| `! <cmd>` / `!!<cmd>` | Run direct shell command (`!` follows `bang_mode`: `silent`/`attach`/`trigger`; `!!` toggles auto-trigger) |
| `/resume` / `/sessions` | Searchable multi-project conversation picker (`Tab` Project/All, `F2` rename, `F4` delete) |
| `/rewind` / `/undo` | Rewind conversation via Tree WAL leaves (or press `Esc` twice on an empty prompt) |
| `/tree` | Browse Tree WAL branch graph (`Enter`: switch `--leaf`, `f`: `anix fork`) |
| `/fork [name] [--leaf <id>]` | Fork current branch into a new WAL file and optionally switch/spawn |
| `/compact` | Trigger online context compaction via Maildir (`Anix-Control: compact`) |
| `/context` | Inspect WAL token budget breakdown with 10×10 grid visualization |
| `/config` | Interactive 3-tab settings panel (`Settings` / `Status` / `Usage`), saved to `~/.config/anix-agent/config.json` |
| `/statusbar [vars\|edit\|preset\|reset]` | DIY live variable-interpolated status bar editor (`default` / `minimal` / `full`) |
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
| `Ctrl+G` / `Ctrl+S` / `Ctrl+Z` | Edit prompt in `$EDITOR` / stash & pop prompt / POSIX suspend (`SIGTSTP`) |
| `Esc` / `Ctrl+C` | Close overlay, interrupt active turn, or double-`Esc` to clear input / open `/rewind` |
| `\Enter` / `Alt+Enter` / `Ctrl+J` | Insert newline in input box |
| `Tab` | Autocomplete `/` slash commands, custom commands/skills, or `@path` files |

## Roadmap / TODO

* [x] **Persistent input history + `Ctrl+R` reverse search**: persists prompt history across sessions (`~/.local/share/anix-agent/history.jsonl`) with interactive `Ctrl+R` incremental search.
* [x] **Clipboard image paste (`Ctrl+V`) & drag-and-drop image paths**: captures clipboard images to private temp files or detects dragged `.png/.jpg/.webp/.gif` paths as `[Image #N]` chips and passes them natively via `Anix-Media:` Maildir headers.
* [ ] **Code rollback in `/rewind`**: capture zero-pollution workspace snapshots per user turn via a temporary `GIT_INDEX_FILE` + `git write-tree` (stored as `wal_leaf_id -> tree_sha` in session metadata, without touching `.git/index` or `git stash`), enabling *Restore code & conversation* and *Restore code only* even for arbitrary `msh`/shell edits.

## License

Apache-2.0 — see [LICENSE](LICENSE).
