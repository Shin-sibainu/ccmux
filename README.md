# ccmux

Claude Code Multiplexer — manage multiple Claude Code instances in TUI split panes.

A lightweight terminal multiplexer built specifically for running multiple [Claude Code](https://docs.anthropic.com/en/docs/claude-code) sessions side-by-side.

![ccmux screenshot](screenshot.png)

## Features

- **Multi-pane terminal** — Split vertically/horizontally, run independent PTY shells
- **Tab workspaces** — Multiple project tabs with click-to-switch
- **File tree sidebar** — Browse project files with icons, expand/collapse directories
- **Syntax-highlighted preview** — View file contents with language-aware coloring
- **Claude Code detection** — Pane border turns orange when Claude Code is running
- **cd tracking** — File tree and tab name auto-update when you change directories
- **Mouse support** — Click to focus, drag borders to resize, scroll history
- **Scrollback** — 10,000 lines of terminal history per pane
- **Dark theme** — Claude-inspired color scheme
- **Cross-platform** — Windows, macOS, Linux
- **Single binary** — ~1MB, no runtime dependencies

## Install

### Download binary (recommended)

Download the latest binary from [Releases](https://github.com/Shin-sibainu/ccmux/releases):

| Platform | File |
|----------|------|
| Windows (x64) | `ccmux-windows-x64.exe` |
| macOS (Apple Silicon) | `ccmux-macos-arm64` |
| macOS (Intel) | `ccmux-macos-x64` |
| Linux (x64) | `ccmux-linux-x64` |

> **Windows:** Microsoft Defender SmartScreen may show a warning because the binary is not code-signed. Click "More info" → "Run anyway" to proceed. This is normal for unsigned open-source software.

> **macOS/Linux:** After downloading, make the binary executable: `chmod +x ccmux-*`

### From source

```bash
git clone https://github.com/Shin-sibainu/ccmux.git
cd ccmux
cargo build --release
# Binary at target/release/ccmux (or ccmux.exe on Windows)
```

Requires [Rust](https://rustup.rs/) toolchain.

## Usage

```bash
ccmux
```

Launch from any directory. The file tree shows the current working directory.

## Keybindings

### Pane mode (default)

| Key | Action |
|-----|--------|
| `Ctrl+D` | Split vertically |
| `Ctrl+E` | Split horizontally |
| `Ctrl+W` | Close pane / tab |
| `Ctrl+T` | New tab |
| `Ctrl+F` | Toggle file tree |
| `Ctrl+P` | Swap preview/terminal layout |
| `Ctrl+Right/Left` | Cycle focus (sidebar, preview, panes) |
| `Ctrl+Q` | Quit |

### File tree mode (after `Ctrl+F`)

| Key | Action |
|-----|--------|
| `j` / `k` | Move selection |
| `Enter` | Open file / expand directory |
| `.` | Toggle hidden files |
| `Esc` | Return to pane |

### Preview mode (after focusing preview)

| Key | Action |
|-----|--------|
| `j` / `k` | Scroll |
| `Ctrl+W` | Close preview |
| `Esc` | Return to pane |

### Mouse

| Action | Effect |
|--------|--------|
| Click pane | Focus pane |
| Click tab | Switch tab |
| Click `+` | New tab |
| Drag border | Resize panels |
| Scroll wheel | Scroll file tree / preview / terminal history |

## Architecture

```
src/
├── main.rs       # Entry point, event loop, panic hook
├── app.rs        # Workspace/tab state, layout tree, key/mouse handling
├── pane.rs       # PTY management, vt100 emulation, shell detection
├── ui.rs         # ratatui rendering, theme, layout
├── filetree.rs   # File tree scanning, navigation
└── preview.rs    # File preview with syntax highlighting
```

**Key design decisions:**
- `vt100` crate for terminal emulation (not ANSI stripping) — needed for Claude Code's interactive UI
- Binary tree layout for recursive pane splitting with variable ratios
- Per-PTY reader threads with mpsc channel to main event loop
- OSC 7 detection for automatic cd tracking
- Dirty-flag rendering for minimal CPU usage when idle

## Tech Stack

- [ratatui](https://ratatui.rs/) + [crossterm](https://github.com/crossterm-rs/crossterm) — TUI framework
- [portable-pty](https://github.com/nickelc/portable-pty) — PTY abstraction (ConPTY on Windows)
- [vt100](https://crates.io/crates/vt100) — Terminal emulation
- [syntect](https://github.com/trishume/syntect) — Syntax highlighting

## Learn Claude Code

New to Claude Code? Check out [Claude Code Academy](https://claude-code-academy.dev) for tutorials and guides.

## License

MIT
