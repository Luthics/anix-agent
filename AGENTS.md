# AGENTS.md — anix-agent (Layer 3) Engineering Guide

## Architecture Boundary (Strict Layer 3)

* `anix-agent` is a Layer 3 human-facing interactive workbench built strictly on top of [Luthics/anix](https://github.com/Luthics/anix) (`anix-core` RFC v0.4.0).
* **MUST NOT** embed LLM HTTP clients, reimplement WAL writing, or bypass `anix`'s external protocols.
* **MUST** drive `anix` exclusively via POSIX subprocess contracts: `--inbox`, `--trace-fd`, `-c`/`--leaf`, `anix fork`/`fsck`/`log`, Maildir `Anix-*` envelopes, and the `--shell` wrapper chain (`anix-agent approve-wrap --ipc <sock> -- <shell>`).
* Keep the binary lightweight (pure `std` + `ratatui` + `crossterm` + `anix-proto`, no heavy async runtime, static musl friendly).

## Quality Gates

Before committing:
1. `cargo fmt --all -- --check`
2. `cargo clippy --all-targets -- -D warnings`
3. `cargo test`
4. Use GPG-signed Conventional Commits (`git commit -S`).
