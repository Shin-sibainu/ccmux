# ccmux — Claude Code Multiplexer

## Overview
Rust TUI tool for managing multiple Claude Code instances in split panes.

## Tech Stack
- Rust (stable), ratatui + crossterm, portable-pty, vt100

## Build & Run
```bash
cargo build          # Debug build
cargo build --release # Release build
cargo test           # Run tests
cargo run            # Run the app
```

## Architecture
- `main.rs` — Entry point, terminal setup, event loop
- `app.rs` — App state, event dispatching, layout tree
- `pane.rs` — PTY management, vt100 terminal emulation, shell detection
- `ui.rs` — ratatui rendering, layout calculation, theme
- `filetree.rs` — File tree sidebar
- `preview.rs` — File preview panel

## Key Design Decisions
- **vt100 crate** for terminal emulation (not ANSI stripping) — needed for Claude Code's interactive UI
- **Binary tree layout** for recursive pane splitting
- **Per-PTY reader threads** with mpsc channel to main event loop
- PTY resize via both `master_pty.resize()` and `vt100_parser.set_size()`

## Shell Detection Priority
- Windows: Git Bash → PowerShell
- Unix: $SHELL → /bin/sh
