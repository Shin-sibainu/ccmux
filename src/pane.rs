use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::thread;

use anyhow::{Context, Result};
use portable_pty::{native_pty_system, Child, CommandBuilder, MasterPty, PtySize};

use crate::app::AppEvent;

/// A terminal pane wrapping a PTY and vt100 parser.
pub struct Pane {
    pub id: usize,
    master: Box<dyn MasterPty + Send>,
    writer: Box<dyn Write + Send>,
    pub parser: Arc<Mutex<vt100::Parser>>,
    child: Box<dyn Child + Send + Sync>,
    _reader_handle: thread::JoinHandle<()>,
    last_rows: u16,
    last_cols: u16,
    pub exited: bool,
    /// Current OSC 0/2 window title. The reader thread keeps this up to date
    /// for OSC tracking / potential future title display; Claude detection no
    /// longer reads it directly (it derives `claude_active` from title events).
    #[allow(dead_code)]
    pub title: Arc<Mutex<String>>,
    /// Whether Claude Code is currently running in this pane. Set by the
    /// reader thread via title hysteresis (see `pty_reader_thread`): turned
    /// on by a Claude title, off by a shell prompt title, held across the
    /// `<spinner> <session-name>` titles in between. Read by the UI.
    claude_active: Arc<std::sync::atomic::AtomicBool>,
    pub cwd: PathBuf,
    pub total_scrollback: Arc<std::sync::atomic::AtomicUsize>,
}

impl Pane {
    /// Create a new pane with a PTY shell.
    pub fn new(id: usize, rows: u16, cols: u16, event_tx: Sender<AppEvent>) -> Result<Self> {
        Self::new_with_cwd(id, rows, cols, event_tx, None)
    }

    pub fn new_with_cwd(id: usize, rows: u16, cols: u16, event_tx: Sender<AppEvent>, cwd: Option<PathBuf>) -> Result<Self> {
        let pty_system = native_pty_system();

        let pty_size = PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        };

        let pair = pty_system
            .openpty(pty_size)
            .context("Failed to open PTY")?;

        let shell = detect_shell();
        let mut cmd = CommandBuilder::new(&shell);

        let shell_name = shell
            .file_name()
            .map(|n| n.to_string_lossy().to_lowercase())
            .unwrap_or_default();

        if shell_name.contains("bash") || shell_name.contains("zsh") {
            cmd.arg("--login");
        }

        let work_dir = cwd.unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
        cmd.cwd(&work_dir);
        cmd.env("TERM", "xterm-256color");
        cmd.env("CCMUX", "1"); // marker to detect nested ccmux

        let child = pair
            .slave
            .spawn_command(cmd)
            .context("Failed to spawn shell")?;

        // Drop the slave side — we only use master
        drop(pair.slave);

        let writer = pair
            .master
            .take_writer()
            .context("Failed to take PTY writer")?;

        // Scrollback buffer: 10000 lines of history
        let parser = Arc::new(Mutex::new(vt100::Parser::new(rows, cols, 10000)));
        let pane_title = Arc::new(Mutex::new(String::new()));

        let reader = pair
            .master
            .try_clone_reader()
            .context("Failed to clone PTY reader")?;

        let parser_clone = Arc::clone(&parser);
        let title_clone = Arc::clone(&pane_title);
        let claude_active = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let claude_active_clone = Arc::clone(&claude_active);
        let scrollback_counter = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let scrollback_clone = Arc::clone(&scrollback_counter);
        let reader_handle = thread::spawn(move || {
            pty_reader_thread(
                reader,
                parser_clone,
                title_clone,
                claude_active_clone,
                scrollback_clone,
                id,
                event_tx,
            );
        });

        let mut pane = Self {
            id,
            master: pair.master,
            writer,
            parser,
            child,
            _reader_handle: reader_handle,
            last_rows: rows,
            last_cols: cols,
            exited: false,
            title: pane_title,
            claude_active,
            cwd: work_dir,
            total_scrollback: scrollback_counter,
        };

        // Inject OSC 7 hook after shell starts
        // Leading space prevents it from appearing in bash history
        if shell_name.contains("bash") {
            let setup = concat!(
                " __ccmux_osc7() { printf '\\033]7;file://%s%s\\007' \"$HOSTNAME\" \"$PWD\"; };",
                " PROMPT_COMMAND=\"__ccmux_osc7;${PROMPT_COMMAND}\";",
                " clear\n",
            );
            let _ = pane.write_input(setup.as_bytes());
        } else if shell_name.contains("zsh") {
            let setup = concat!(
                " __ccmux_osc7() { printf '\\033]7;file://%s%s\\007' \"$HOST\" \"$PWD\"; };",
                " precmd_functions+=(__ccmux_osc7);",
                " clear\n",
            );
            let _ = pane.write_input(setup.as_bytes());
        }

        Ok(pane)
    }

    /// Write input bytes to the PTY (keyboard input from user).
    pub fn write_input(&mut self, data: &[u8]) -> Result<()> {
        if self.exited {
            return Ok(());
        }
        if self.writer.write_all(data).is_err() || self.writer.flush().is_err() {
            self.exited = true;
        }
        Ok(())
    }

    /// Resize the PTY and vt100 parser. Returns `true` if the size
    /// actually changed (useful for callers that want to know whether
    /// a SIGWINCH was sent to the child). No-op and returns `false`
    /// when the size hasn't changed.
    pub fn resize(&mut self, rows: u16, cols: u16) -> Result<bool> {
        if rows == 0 || cols == 0 {
            return Ok(false);
        }

        // Skip if size hasn't changed
        if rows == self.last_rows && cols == self.last_cols {
            return Ok(false);
        }

        self.last_rows = rows;
        self.last_cols = cols;

        self.master
            .resize(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .context("Failed to resize PTY")?;

        let mut parser = self.parser.lock().unwrap_or_else(|e| e.into_inner());
        parser.screen_mut().set_size(rows, cols);
        // Clear the screen buffer to avoid rendering stale content at the new size.
        // The TUI app (e.g. Claude Code) receives SIGWINCH and will redraw.
        // A brief blank frame is preferable to overlapping garbled output.
        parser.process(b"\x1b[2J\x1b[H");

        Ok(true)
    }

    /// Scroll the terminal view up (into scrollback history).
    pub fn scroll_up(&self, lines: usize) {
        let mut parser = self.parser.lock().unwrap_or_else(|e| e.into_inner());
        let current = parser.screen().scrollback();
        parser.screen_mut().set_scrollback(current + lines);
    }

    /// Get scrollbar info: (current_offset, max_offset).
    /// max_offset is estimated by trying to scroll to a large value and checking.
    pub fn scrollbar_info(&self) -> (usize, usize) {
        let parser = self.parser.lock().unwrap_or_else(|e| e.into_inner());
        let screen = parser.screen();
        let current = screen.scrollback();
        // Estimate max by checking: set_scrollback clamps to actual scrollback length
        // We can't query it directly, so use the stored total_scrollback as estimate
        let total = self.total_scrollback.load(std::sync::atomic::Ordering::Relaxed);
        (current, total)
    }

    /// Scroll the terminal view down (towards current output).
    pub fn scroll_down(&self, lines: usize) {
        let mut parser = self.parser.lock().unwrap_or_else(|e| e.into_inner());
        let current = parser.screen().scrollback();
        parser.screen_mut().set_scrollback(current.saturating_sub(lines));
    }

    /// Reset scroll to the bottom (live view).
    pub fn scroll_reset(&self) {
        let mut parser = self.parser.lock().unwrap_or_else(|e| e.into_inner());
        parser.screen_mut().set_scrollback(0);
    }

    /// Check if the terminal is scrolled back.
    pub fn is_scrolled_back(&self) -> bool {
        let parser = self.parser.lock().unwrap_or_else(|e| e.into_inner());
        parser.screen().scrollback() > 0
    }

    /// Check if the PTY application has enabled bracketed paste mode.
    pub fn is_bracketed_paste_enabled(&self) -> bool {
        let parser = self.parser.lock().unwrap_or_else(|e| e.into_inner());
        parser.screen().bracketed_paste()
    }

    /// Check if the PTY application is using the alternate screen buffer
    /// (e.g. Claude Code, vim, htop).  Apps in altbuf typically handle
    /// scrolling internally, so wheel events should be forwarded to the
    /// PTY instead of consumed by ccmux's scrollback.
    pub fn is_alternate_screen(&self) -> bool {
        let parser = self.parser.lock().unwrap_or_else(|e| e.into_inner());
        parser.screen().alternate_screen()
    }

    /// Check if the PTY application has mouse capture enabled.
    pub fn is_mouse_capture_enabled(&self) -> bool {
        let parser = self.parser.lock().unwrap_or_else(|e| e.into_inner());
        !matches!(
            parser.screen().mouse_protocol_mode(),
            vt100::MouseProtocolMode::None
        )
    }

    /// Check if Claude Code is running in this pane.
    ///
    /// Returns the hysteresis flag maintained by the reader thread rather than
    /// re-judging the current title: Claude rewrites its title to
    /// `<spinner> <session-name>` while running, which `title_indicates_claude`
    /// alone would not recognise. See `pty_reader_thread` for the state machine.
    pub fn is_claude_running(&self) -> bool {
        self.claude_active.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Kill the PTY child process.
    pub fn kill(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Drop for Pane {
    fn drop(&mut self) {
        self.kill();
    }
}

/// Background thread that reads PTY output and feeds it to vt100 parser.
fn pty_reader_thread(
    mut reader: Box<dyn Read + Send>,
    parser: Arc<Mutex<vt100::Parser>>,
    title: Arc<Mutex<String>>,
    claude_active: Arc<std::sync::atomic::AtomicBool>,
    scrollback_count: Arc<std::sync::atomic::AtomicUsize>,
    pane_id: usize,
    event_tx: Sender<AppEvent>,
) {
    let mut buf = [0u8; 4096];
    loop {
        match reader.read(&mut buf) {
            Ok(0) => {
                let _ = event_tx.send(AppEvent::PtyEof(pane_id));
                break;
            }
            Ok(n) => {
                let data = &buf[..n];

                // Track scrollback lines (count newlines)
                let newlines = data.iter().filter(|&&b| b == b'\n').count();
                if newlines > 0 {
                    scrollback_count.fetch_add(newlines, std::sync::atomic::Ordering::Relaxed);
                }

                // Detect OSC 7 (cwd notification)
                if let Some(path) = extract_osc7(data) {
                    let _ = event_tx.send(AppEvent::CwdChanged(pane_id, path));
                }

                // Detect OSC 0/2 (window title) — drives Claude Code detection.
                if let Some(new_title) = extract_osc_title(data) {
                    // Hysteresis (see `next_claude_active`): held across the
                    // `<spinner> <session-name>` titles so a named/continued
                    // Claude session stays detected.
                    let prev = claude_active.load(std::sync::atomic::Ordering::Relaxed);
                    let next = next_claude_active(prev, &new_title);
                    if next != prev {
                        claude_active.store(next, std::sync::atomic::Ordering::Relaxed);
                    }
                    if let Ok(mut t) = title.lock() {
                        *t = new_title;
                    }
                }

                let mut parser = parser.lock().unwrap_or_else(|e| e.into_inner());
                parser.process(data);
                drop(parser);
                let _ = event_tx.send(AppEvent::PtyOutput(pane_id));
            }
            Err(_) => {
                break;
            }
        }
    }
}

/// Extract path from OSC 7 escape sequence: \x1b]7;file://HOST/PATH(\x07|\x1b\\)
fn extract_osc7(data: &[u8]) -> Option<PathBuf> {
    let s = std::str::from_utf8(data).ok()?;

    // Look for OSC 7 pattern
    let marker = "\x1b]7;";
    let start = s.find(marker)?;
    let rest = &s[start + marker.len()..];

    // Find the terminator: BEL (\x07) or ST (\x1b\\)
    let end = rest.find('\x07')
        .or_else(|| rest.find("\x1b\\"));

    let uri = &rest[..end?];

    // Parse file:// URI → extract path
    // Formats: file://hostname/path, file:///path, file:///c/Users/...
    if let Some(path_str) = uri.strip_prefix("file://") {
        // Skip hostname part: find the path starting with /
        // file://hostname/path → skip "hostname", take "/path"
        // file:///path → hostname is empty, take "/path"
        let path = if path_str.starts_with('/') {
            // No hostname (file:///path)
            path_str
        } else if let Some(slash_pos) = path_str.find('/') {
            // Has hostname (file://host/path)
            &path_str[slash_pos..]
        } else {
            return None;
        };

        // On Windows/MSYS2, convert /c/Users/... to C:\Users\...
        #[cfg(windows)]
        {
            let path_bytes = path.as_bytes();
            if path_bytes.len() >= 3
                && path_bytes[0] == b'/'
                && path_bytes[1].is_ascii_alphabetic()
                && path_bytes[2] == b'/'
            {
                let drive = path_bytes[1].to_ascii_uppercase() as char;
                let rest = &path[2..];
                let win_path = format!("{}:{}", drive, rest.replace('/', "\\"));
                return Some(PathBuf::from(win_path));
            }
        }
        return Some(PathBuf::from(path));
    }

    None
}

/// Check whether a window title indicates Claude Code is running.
///
/// Observed title formats across Claude Code versions:
/// - `Claude Code` / `Claude Code - <project>` (~v2.1.1xx)
/// - `claude` — set at process startup (v2.1.140+, issue #15)
/// - `✳ Claude Code` / `· Claude Code` / `* Claude Code` — spinner glyph +
///   title while the interactive UI is running (v2.1.2xx; the glyph rotates
///   through `· ✢ * ✳ ✶ ✻ ✽` while working)
/// - `claude · resume` — session resume picker
///
/// A leading run of non-alphanumeric characters (the spinner glyph and
/// whitespace) is stripped, then the title must be exactly "claude" or start
/// with "claude code" / "claude · " (case-insensitive).  We deliberately do
/// NOT match a title that merely *contains* "claude" somewhere, so a process
/// that happens to have the word in its title (e.g. an editor opening
/// claude.md) does not trigger the detection (battle-log round 6).
///
/// Accepting the bare title "claude" is a deliberate, minimal relaxation of
/// the round-6 rule — required because v2.1.140+ emits exactly that at
/// startup.  It is an exact match (not a prefix), and the detection only
/// drives UI decoration (border color, cursor, status bar), not a security
/// boundary.
fn title_indicates_claude(title: &str) -> bool {
    let lower = title.to_lowercase();
    // `char::is_alphanumeric` is Unicode-aware: CJK and other letters are NOT
    // stripped, only symbols/punctuation/whitespace, so e.g. "実行中 claude"
    // does not sneak past the prefix check.
    let stripped = lower.trim_start_matches(|c: char| !c.is_alphanumeric());
    stripped == "claude"
        || stripped.starts_with("claude code")
        || stripped.starts_with("claude \u{b7} ")
}

/// Check whether a window title is a shell prompt title (i.e. Claude Code is
/// NOT running here).
///
/// Used as the OFF trigger of the Claude-detection hysteresis: once a pane has
/// been marked as running Claude (via [`title_indicates_claude`]), it stays so
/// even after Claude rewrites its title to `<spinner> <session-name>`, until a
/// shell prompt title appears — meaning Claude has exited and the shell is back.
///
/// Recognised shell titles (case-insensitive):
/// - `MINGW64:` / `MINGW32:` / `MSYS:` prefixes — Git Bash / MSYS set these via
///   `PROMPT_COMMAND` (e.g. `MINGW64:/c/Users/foo`).
/// - a known shell-executable name suffix (`bash.exe` / `powershell.exe` /
///   `pwsh.exe` / `cmd.exe`) — the PTY's initial title is often the shell exe
///   path (`C:\Program Files\Git\bin\bash.exe`). Restricted to specific shell
///   exes so that a Claude session/topic name merely ending in `.exe` (e.g.
///   `✳ fix build.exe`) is NOT mistaken for a shell prompt.
///
/// This is deliberately Git-Bash-centric (ccmux's primary shell). PowerShell /
/// custom-prompt titles that match neither are a known gap (see design ADR-2).
fn title_is_shell_prompt(title: &str) -> bool {
    let lower = title.to_lowercase();
    lower.starts_with("mingw64:")
        || lower.starts_with("mingw32:")
        || lower.starts_with("msys:")
        || lower.ends_with("bash.exe")
        || lower.ends_with("powershell.exe")
        || lower.ends_with("pwsh.exe")
        || lower.ends_with("cmd.exe")
}

/// Hysteresis transition for Claude-Code detection, given the current state and
/// a newly observed window title:
/// - a Claude title turns it **on** (start of a session);
/// - a shell prompt title turns it **off** (Claude exited, shell is back);
/// - anything else (the `<spinner> <session-name>` titles Claude emits while
///   running, or an unrelated app's title) leaves the state **unchanged**.
///
/// Because only a Claude title can turn it on, a pane that has never shown a
/// Claude title stays off even while displaying a session-name-like title.
fn next_claude_active(current: bool, new_title: &str) -> bool {
    if title_indicates_claude(new_title) {
        true
    } else if title_is_shell_prompt(new_title) {
        false
    } else {
        current
    }
}

/// Extract window title from OSC 0 or OSC 2: \x1b]0;TITLE\x07 or \x1b]2;TITLE\x07
fn extract_osc_title(data: &[u8]) -> Option<String> {
    let s = std::str::from_utf8(data).ok()?;
    // Look for OSC 0 or OSC 2
    for marker in &["\x1b]0;", "\x1b]2;"] {
        if let Some(start) = s.find(marker) {
            let rest = &s[start + marker.len()..];
            let end = rest.find('\x07')
                .or_else(|| rest.find("\x1b\\"));
            if let Some(end) = end {
                return Some(rest[..end].to_string());
            }
        }
    }
    None
}

/// Detect the appropriate shell to launch.
pub fn detect_shell() -> PathBuf {
    #[cfg(windows)]
    {
        detect_shell_windows()
    }
    #[cfg(not(windows))]
    {
        detect_shell_unix()
    }
}

#[cfg(windows)]
fn detect_shell_windows() -> PathBuf {
    // Try Git Bash first
    let git_bash_paths = [
        r"C:\Program Files\Git\bin\bash.exe",
        r"C:\Program Files (x86)\Git\bin\bash.exe",
    ];

    for path in &git_bash_paths {
        let p = PathBuf::from(path);
        if p.exists() {
            return p;
        }
    }

    // Try bash in PATH
    if let Ok(output) = std::process::Command::new("where")
        .arg("bash")
        .output()
    {
        if output.status.success() {
            let stdout = String::from_utf8_lossy(&output.stdout);
            if let Some(line) = stdout.lines().next() {
                let p = PathBuf::from(line.trim());
                if p.exists() {
                    return p;
                }
            }
        }
    }

    // Fallback to PowerShell
    PathBuf::from("powershell.exe")
}

#[cfg(not(windows))]
fn detect_shell_unix() -> PathBuf {
    if let Ok(shell) = std::env::var("SHELL") {
        let p = PathBuf::from(&shell);
        if p.exists() {
            return p;
        }
    }
    PathBuf::from("/bin/sh")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_title_indicates_claude_legacy_formats() {
        // <= v2.1.1xx
        assert!(title_indicates_claude("Claude Code"));
        assert!(title_indicates_claude("Claude Code - myproject"));
        assert!(title_indicates_claude("Claude Code v2.1.104"));
    }

    #[test]
    fn test_title_indicates_claude_bare_claude() {
        // v2.1.140+ sets the title to just "claude" at startup (issue #15)
        assert!(title_indicates_claude("claude"));
        assert!(title_indicates_claude("Claude"));
    }

    #[test]
    fn test_title_indicates_claude_spinner_prefix() {
        // v2.1.2xx: "<spinner glyph> <title>" while the UI is running
        assert!(title_indicates_claude("✳ Claude Code"));
        assert!(title_indicates_claude("· Claude Code"));
        assert!(title_indicates_claude("* Claude Code"));
        assert!(title_indicates_claude("✻ Claude Code"));
    }

    #[test]
    fn test_title_indicates_claude_resume_picker() {
        assert!(title_indicates_claude("claude · resume"));
    }

    #[test]
    fn test_title_indicates_claude_rejects_incidental_matches() {
        assert!(!title_indicates_claude(""));
        assert!(!title_indicates_claude("bash"));
        assert!(!title_indicates_claude("MINGW64:/c/Users/foo"));
        // "claude" appearing mid-title must not match (battle-log round 6)
        assert!(!title_indicates_claude("claude.md - VIM"));
        assert!(!title_indicates_claude("my claude code notes"));
        assert!(!title_indicates_claude("claudette"));
        // Leading CJK text is not a spinner glyph and must not be stripped
        assert!(!title_indicates_claude("実行中 claude"));
    }

    #[test]
    fn test_title_is_shell_prompt_recognises_shell_titles() {
        // Git Bash / MSYS PROMPT_COMMAND titles
        assert!(title_is_shell_prompt("MINGW64:/c/Users/foo"));
        assert!(title_is_shell_prompt("MINGW32:/c/Users/foo"));
        assert!(title_is_shell_prompt("MSYS:/c/Users/foo"));
        // Initial PTY title = shell executable path
        assert!(title_is_shell_prompt("C:\\Program Files\\Git\\bin\\bash.exe"));
        assert!(title_is_shell_prompt("powershell.exe"));
        assert!(title_is_shell_prompt("cmd.exe"));
    }

    #[test]
    fn test_next_claude_active_hysteresis() {
        // ON: a Claude title starts detection regardless of prior state.
        assert!(next_claude_active(false, "claude"));
        assert!(next_claude_active(false, "✳ Claude Code"));
        assert!(next_claude_active(true, "claude"));

        // HELD: session-name / spinner titles keep the current state — this is
        // the core fix (a named/continued session stays detected).
        assert!(next_claude_active(true, "✳ ccmux-display-optimization"));
        assert!(next_claude_active(true, "⠂ ccmux-display-optimization"));
        assert!(next_claude_active(true, "")); // empty title: unchanged

        // OFF: a shell prompt title ends detection (Claude exited).
        assert!(!next_claude_active(true, "MINGW64:/c/Users/foo"));
        assert!(!next_claude_active(true, "C:\\Program Files\\Git\\bin\\bash.exe"));

        // Stays OFF: a pane that never showed a Claude title is not turned on
        // by a session-name-like title (prevents false positives).
        assert!(!next_claude_active(false, "✳ ccmux-display-optimization"));
        assert!(!next_claude_active(false, "MINGW64:/c/Users/foo"));

        // HELD across a session/topic name that merely ends in ".exe": only
        // known shell exes count as shell prompts, so this stays detected.
        assert!(next_claude_active(true, "✳ fix build.exe"));
    }

    #[test]
    fn test_claude_active_full_lifecycle() {
        // A realistic sequence of title events from pane start through a named
        // Claude session and back to the shell.
        let steps: &[(&str, bool)] = &[
            ("C:\\Program Files\\Git\\bin\\bash.exe", false), // shell spawn
            ("MINGW64:/c/Users/foo", false),                 // shell prompt
            ("claude", true),                                // claude starts
            ("✳ Claude Code", true),                         // UI up (unnamed)
            ("✳ my-session", true),                          // renamed: held
            ("⠂ my-session", true),                          // spinner: held
            ("⠐ my-session", true),                          // spinner: held
            ("MINGW64:/c/Users/foo", false),                 // claude exits
            ("MINGW64:/c/Users/foo", false),                 // stays off
        ];
        let mut active = false;
        for &(title, expected) in steps {
            active = next_claude_active(active, title);
            assert_eq!(active, expected, "after title {:?}", title);
        }
    }

    #[test]
    fn test_title_is_shell_prompt_rejects_claude_and_session_names() {
        // Claude titles must NOT be seen as shell (would break the OFF trigger)
        assert!(!title_is_shell_prompt("claude"));
        assert!(!title_is_shell_prompt("✳ Claude Code"));
        assert!(!title_is_shell_prompt("claude · resume"));
        // Session/topic names claude rewrites the title to — must be "held"
        // (neither claude nor shell), so title_is_shell_prompt is false.
        assert!(!title_is_shell_prompt("✳ ccmux-display-optimization"));
        assert!(!title_is_shell_prompt("⠂ ccmux-display-optimization"));
        // Empty title is not a shell prompt
        assert!(!title_is_shell_prompt(""));
    }

    #[test]
    fn test_detect_shell_returns_valid_path() {
        let shell = detect_shell();
        assert!(
            !shell.as_os_str().is_empty(),
            "Shell path should not be empty"
        );
    }

    #[cfg(windows)]
    #[test]
    fn test_detect_shell_windows_returns_exe() {
        let shell = detect_shell();
        let ext = shell
            .extension()
            .map(|e| e.to_string_lossy().to_lowercase());
        assert_eq!(ext.as_deref(), Some("exe"), "Windows shell should be .exe");
    }

    #[cfg(not(windows))]
    #[test]
    fn test_detect_shell_unix_uses_shell_env() {
        let shell = detect_shell();
        if let Ok(env_shell) = std::env::var("SHELL") {
            assert_eq!(
                shell,
                PathBuf::from(&env_shell),
                "Should use $SHELL env var"
            );
        }
    }
}
