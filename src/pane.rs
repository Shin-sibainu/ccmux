use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
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
    pub title: Arc<Mutex<String>>,
    pub cwd: PathBuf,
    pub total_scrollback: Arc<std::sync::atomic::AtomicUsize>,
    /// Bytes to write into the PTY once the shell prompt is ready.
    /// `None` means no command queued (or already flushed).
    pub pending_startup: Option<Vec<u8>>,
    /// Set to `true` by the reader thread once a shell prompt has been
    /// observed. Used to gate `pending_startup` flushing so the command
    /// is not eaten by an initializing shell.
    pub prompt_seen: Arc<AtomicBool>,
}

impl Pane {
    /// Create a new pane with a PTY shell.
    pub fn new(id: usize, rows: u16, cols: u16, event_tx: Sender<AppEvent>) -> Result<Self> {
        Self::new_with_cwd(id, rows, cols, event_tx, None)
    }

    pub fn new_with_cwd(
        id: usize,
        rows: u16,
        cols: u16,
        event_tx: Sender<AppEvent>,
        cwd: Option<PathBuf>,
    ) -> Result<Self> {
        let pty_system = native_pty_system();

        let pty_size = PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        };

        let pair = pty_system.openpty(pty_size).context("Failed to open PTY")?;

        let shell = detect_shell();
        let mut cmd = CommandBuilder::new(&shell);

        let shell_name = shell
            .file_name()
            .map(|n| n.to_string_lossy().to_lowercase())
            .unwrap_or_default();

        if shell_name.contains("bash") || shell_name.contains("zsh") {
            cmd.arg("--login");
        }

        let work_dir =
            cwd.unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
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
        let scrollback_counter = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let scrollback_clone = Arc::clone(&scrollback_counter);
        let prompt_seen = Arc::new(AtomicBool::new(false));
        let prompt_seen_clone = Arc::clone(&prompt_seen);
        let reader_handle = thread::spawn(move || {
            pty_reader_thread(
                reader,
                parser_clone,
                title_clone,
                scrollback_clone,
                prompt_seen_clone,
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
            cwd: work_dir,
            total_scrollback: scrollback_counter,
            pending_startup: None,
            prompt_seen,
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
        let total = self
            .total_scrollback
            .load(std::sync::atomic::Ordering::Relaxed);
        (current, total)
    }

    /// Scroll the terminal view down (towards current output).
    pub fn scroll_down(&self, lines: usize) {
        let mut parser = self.parser.lock().unwrap_or_else(|e| e.into_inner());
        let current = parser.screen().scrollback();
        parser
            .screen_mut()
            .set_scrollback(current.saturating_sub(lines));
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

    /// Check if Claude Code is running in this pane (by window title).
    pub fn is_claude_running(&self) -> bool {
        if let Ok(t) = self.title.lock() {
            let lower = t.to_lowercase();
            lower.contains("claude")
        } else {
            false
        }
    }

    /// Kill the PTY child process.
    pub fn kill(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }

    /// Queue a command to be written into the PTY once the shell prompt
    /// is ready. A trailing newline is appended automatically so the
    /// command is executed as soon as the shell sees it.
    pub fn queue_startup_command(&mut self, cmd: &str) {
        let mut data = cmd.as_bytes().to_vec();
        if !data.ends_with(b"\n") {
            data.push(b'\n');
        }
        self.pending_startup = Some(data);
    }

    /// If a startup command is queued and the shell prompt has been
    /// observed, write the command into the PTY and clear the queue.
    /// Returns `Ok(true)` if a flush happened, `Ok(false)` otherwise.
    /// Acquire ordering pairs with the reader thread's `Release` store.
    pub fn try_flush_startup(&mut self) -> std::io::Result<bool> {
        if self.pending_startup.is_none() {
            return Ok(false);
        }
        if !self.prompt_seen.load(Ordering::Acquire) {
            return Ok(false);
        }
        if let Some(data) = self.pending_startup.take() {
            // Mirror `write_input`: any write OR flush failure marks the
            // pane as exited and is reported as a no-op flush so callers
            // do not see partial-write panics.
            if self.writer.write_all(&data).is_err() || self.writer.flush().is_err() {
                self.exited = true;
                return Ok(false);
            }
            return Ok(true);
        }
        Ok(false)
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
    scrollback_count: Arc<std::sync::atomic::AtomicUsize>,
    prompt_seen: Arc<AtomicBool>,
    pane_id: usize,
    event_tx: Sender<AppEvent>,
) {
    // Rolling tail of the most recent bytes read from the PTY. Used to
    // detect a shell prompt that may straddle two reader chunks. Capped
    // so the buffer cannot grow without bound.
    const TAIL_CAP: usize = 256;
    let mut tail: Vec<u8> = Vec::with_capacity(TAIL_CAP * 2);

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

                // Detect OSC 7 (cwd notification). Bash/zsh emit this on
                // every prompt thanks to the hook injected in `Pane::new`,
                // so its presence is also a strong "prompt is up" signal.
                // Release ordering pairs with the Acquire load in
                // `Pane::try_flush_startup` so the queued startup command
                // is published to the main thread atomically.
                if let Some(path) = extract_osc7(data) {
                    prompt_seen.store(true, Ordering::Release);
                    // Drop the rolling tail once the latch is set so we
                    // do not retain memory for the rest of the session.
                    tail = Vec::new();
                    let _ = event_tx.send(AppEvent::CwdChanged(pane_id, path));
                }

                // Detect OSC 0/2 (window title) — used to detect Claude Code
                if let Some(new_title) = extract_osc_title(data) {
                    if let Ok(mut t) = title.lock() {
                        *t = new_title;
                    }
                }

                // Heuristic prompt detection over a rolling tail so prompts
                // that straddle two reads are still picked up.
                if !prompt_seen.load(Ordering::Acquire) {
                    tail.extend_from_slice(data);
                    if tail.len() > TAIL_CAP * 2 {
                        let drop = tail.len() - TAIL_CAP;
                        tail.drain(..drop);
                    }
                    if is_prompt_ready(&tail) {
                        prompt_seen.store(true, Ordering::Release);
                        // Tail no longer needed once the flag latches on.
                        tail = Vec::new();
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
    let end = rest.find('\x07').or_else(|| rest.find("\x1b\\"));

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

/// Extract window title from OSC 0 or OSC 2: \x1b]0;TITLE\x07 or \x1b]2;TITLE\x07
fn extract_osc_title(data: &[u8]) -> Option<String> {
    let s = std::str::from_utf8(data).ok()?;
    // Look for OSC 0 or OSC 2
    for marker in &["\x1b]0;", "\x1b]2;"] {
        if let Some(start) = s.find(marker) {
            let rest = &s[start + marker.len()..];
            let end = rest.find('\x07').or_else(|| rest.find("\x1b\\"));
            if let Some(end) = end {
                return Some(rest[..end].to_string());
            }
        }
    }
    None
}

/// Returns `true` if `buf` looks like the recently-emitted bytes end with
/// a shell prompt (`$`, `>`, `%`, or `#`), optionally followed by trailing
/// whitespace and CSI/ANSI escape sequences such as color resets.
///
/// This is intentionally conservative: it strips only ANSI CSI sequences
/// (`ESC [ ... <final-byte>`) and trailing ASCII whitespace. False
/// negatives (e.g. exotic prompt styles) only delay startup-command flush
/// by one PTY read cycle. False positives risk firing the startup command
/// against a still-initializing shell.
pub fn is_prompt_ready(buf: &[u8]) -> bool {
    let stripped = strip_csi_escapes(buf);
    let trimmed = trim_ascii_whitespace_end(&stripped);
    let Some(&last) = trimmed.last() else {
        return false;
    };
    if !matches!(last, b'$' | b'>' | b'%' | b'#') {
        return false;
    }
    // Guard against common non-prompt endings that happen to finish
    // with a prompt-like character:
    // - PowerShell / npm-style progress bars: `[====>]` redrawing can
    //   leave `====>` visible mid-frame before the closing bracket.
    // - Percentage readouts: `50%` ends in `%` (zsh's prompt marker).
    // Each guard rejects a specific combination of (last, prev) that is
    // overwhelmingly output, not a prompt.
    if let Some(&prev) = trimmed.get(trimmed.len().saturating_sub(2)) {
        if last == b'>' && matches!(prev, b'=' | b'-' | b'~' | b'.' | b'*') {
            return false;
        }
        if last == b'%' && prev.is_ascii_digit() {
            return false;
        }
    }
    true
}

fn strip_csi_escapes(buf: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(buf.len());
    let mut i = 0;
    while i < buf.len() {
        if buf[i] == 0x1b && i + 1 < buf.len() && buf[i + 1] == b'[' {
            i += 2;
            while i < buf.len() {
                let c = buf[i];
                i += 1;
                if (0x40..=0x7E).contains(&c) {
                    break;
                }
            }
        } else {
            out.push(buf[i]);
            i += 1;
        }
    }
    out
}

fn trim_ascii_whitespace_end(buf: &[u8]) -> &[u8] {
    let mut end = buf.len();
    while end > 0 && matches!(buf[end - 1], b' ' | b'\t' | b'\r' | b'\n') {
        end -= 1;
    }
    &buf[..end]
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
    if let Ok(output) = std::process::Command::new("where").arg("bash").output() {
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

    // -- is_prompt_ready -------------------------------------------------

    #[test]
    fn prompt_ready_dollar_with_space() {
        assert!(is_prompt_ready(b"user@host:~$ "));
    }

    #[test]
    fn prompt_ready_powershell_chevron() {
        assert!(is_prompt_ready(b"PS C:\\> "));
    }

    #[test]
    fn prompt_ready_zsh_percent() {
        assert!(is_prompt_ready(b"% "));
    }

    #[test]
    fn prompt_ready_root_hash() {
        assert!(is_prompt_ready(b"root@host:/# "));
    }

    #[test]
    fn prompt_not_ready_when_loading() {
        assert!(!is_prompt_ready(b"loading dependencies..."));
    }

    #[test]
    fn prompt_ready_strips_trailing_ansi_color() {
        // Common: prompt char then color reset
        assert!(is_prompt_ready(b"user@host:~$ \x1b[0m"));
    }

    #[test]
    fn prompt_not_ready_for_empty_input() {
        assert!(!is_prompt_ready(b""));
    }

    #[test]
    fn prompt_not_ready_when_only_motd_text() {
        assert!(!is_prompt_ready(b"Welcome to Ubuntu 22.04 LTS"));
    }

    // ─── progress-bar / output misfire guards ────────────────

    #[test]
    fn prompt_not_ready_for_progress_bar_equals_chevron() {
        // Mid-redraw progress bar: `[====>   ]` truncated to `====>`
        // before the closing bracket comes through. Must not trigger.
        assert!(!is_prompt_ready(b"loading [====>"));
    }

    #[test]
    fn prompt_not_ready_for_dashed_progress_chevron() {
        // `--->` style progress marker (common in make-style output).
        assert!(!is_prompt_ready(b"step 3 --->"));
    }

    #[test]
    fn prompt_not_ready_for_asterisk_chevron() {
        assert!(!is_prompt_ready(b"***>"));
    }

    #[test]
    fn prompt_not_ready_for_percentage_readout() {
        // `50%` at end of a progress line should NOT look like a zsh
        // prompt.
        assert!(!is_prompt_ready(b"Downloading... 50%"));
    }

    #[test]
    fn prompt_not_ready_for_hundred_percent() {
        assert!(!is_prompt_ready(b"Done: 100%"));
    }

    #[test]
    fn prompt_ready_powershell_with_real_path_before_chevron() {
        // Regression guard: the previous char in a PowerShell prompt is
        // a letter or path separator (`>` after `e` or `\`), not an
        // ASCII-art character — must still trigger.
        assert!(is_prompt_ready(b"PS C:\\Users\\me>"));
        assert!(is_prompt_ready(b"PS C:\\Users\\me> "));
    }

    #[test]
    fn prompt_ready_zsh_percent_after_space() {
        // Bare `%` preceded by whitespace stays a valid zsh prompt.
        assert!(is_prompt_ready(b"user ~/dir % "));
    }
}
