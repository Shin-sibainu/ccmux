mod app;
mod claude_monitor;
mod filetree;
mod pane;
mod preview;
mod ui;
mod version_check;

use std::io;
use std::panic;
use std::time::Duration;

use anyhow::Result;
use crossterm::event::{self, Event, KeyEventKind};
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use crossterm::execute;
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;

fn main() -> Result<()> {
    // Detect if running inside another ccmux instance
    if std::env::var("CCMUX").is_ok() {
        eprintln!("ccmux: already running inside a ccmux pane (nested instance not allowed).");
        eprintln!("       Open a new tab with Ctrl+T or split with Ctrl+D / Ctrl+E instead.");
        std::process::exit(1);
    }

    // If a directory is passed as argument, cd into it first
    if let Some(dir) = std::env::args().nth(1) {
        let path = std::path::Path::new(&dir);
        if path.is_dir() {
            std::env::set_current_dir(path)?;
        } else {
            eprintln!("ccmux: not a directory: {}", dir);
            std::process::exit(1);
        }
    }

    // Install panic hook to restore terminal state on crash
    let default_hook = panic::take_hook();
    panic::set_hook(Box::new(move |info| {
        let _ = disable_raw_mode();
        let _ = execute!(io::stdout(), crossterm::event::DisableMouseCapture);
        let _ = execute!(io::stdout(), crossterm::event::DisableBracketedPaste);
        let _ = execute!(io::stdout(), LeaveAlternateScreen);
        default_hook(info);
    }));

    // Setup terminal
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    execute!(stdout, crossterm::event::EnableMouseCapture)?;
    execute!(stdout, crossterm::event::EnableBracketedPaste)?;

    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    // Get initial terminal size
    let size = terminal.size()?;

    // Create app
    let mut app = app::App::new(size.height, size.width)?;

    // Main event loop
    let result = run_event_loop(&mut terminal, &mut app);

    // Cleanup
    app.shutdown();
    disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        crossterm::event::DisableMouseCapture
    )?;
    execute!(
        terminal.backend_mut(),
        crossterm::event::DisableBracketedPaste
    )?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;

    result
}

fn run_event_loop(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    app: &mut app::App,
) -> Result<()> {
    let mut paste_buffer: Vec<u8> = Vec::new();

    loop {
        // Drain any PTY output events
        app.drain_pty_events();

        // After paste, wait a few frames for PTY echo to settle
        if app.paste_cooldown > 0 {
            app.paste_cooldown -= 1;
            if app.paste_cooldown == 0 {
                app.dirty = true;
            }
        }

        // After a layout change (split/close/sidebar/terminal resize),
        // wait a few frames so child PTYs can respond to SIGWINCH with
        // a fresh redraw. Prevents the "old buffer at new size" flash.
        if app.resize_cooldown > 0 {
            app.resize_cooldown -= 1;
            if app.resize_cooldown == 0 {
                app.dirty = true;
            }
        }

        // Only render when something changed (and no cooldown is active)
        if app.dirty && app.paste_cooldown == 0 && app.resize_cooldown == 0 {
            app.dirty = false;
            terminal.draw(|frame| {
                ui::render(app, frame);
            })?;
        }

        if app.should_quit {
            break;
        }

        // Poll for crossterm events with a short timeout (~30fps)
        if event::poll(Duration::from_millis(33))? {
            match event::read()? {
                Event::Key(key) => {
                    if key.kind == KeyEventKind::Press {
                        let consumed = app.handle_key_event(key)?;
                        if !consumed {
                            // Collect rapid key events as potential paste
                            if let Some(bytes) = crate::app::key_event_to_bytes_pub(&key) {
                                paste_buffer.extend_from_slice(&bytes);
                                // Drain all immediately available key events (paste burst)
                                while event::poll(Duration::from_millis(1))? {
                                    if let Event::Key(k) = event::read()? {
                                        if k.kind == KeyEventKind::Press {
                                            if app.handle_key_event(k)? {
                                                // Shortcut consumed — flush buffer first
                                                if !paste_buffer.is_empty() {
                                                    flush_paste_buffer(app, &mut paste_buffer)?;
                                                }
                                                break;
                                            }
                                            if let Some(b) = crate::app::key_event_to_bytes_pub(&k) {
                                                paste_buffer.extend_from_slice(&b);
                                            }
                                        }
                                    } else {
                                        break;
                                    }
                                }
                                flush_paste_buffer(app, &mut paste_buffer)?;
                            }
                        }
                        app.dirty = true;
                    }
                }
                Event::Paste(text) => {
                    app.forward_paste_to_pty(&text)?;
                    app.paste_cooldown = 5;
                    app.dirty = true;
                }
                Event::Mouse(mouse) => {
                    app.handle_mouse_event(mouse);
                    app.dirty = true;
                }
                Event::Resize(cols, rows) => {
                    // Propagate the new terminal size to App so every
                    // pane's PTY gets a prompt SIGWINCH, and hold the
                    // paint for a few frames while the children redraw.
                    app.on_terminal_resize(cols, rows);
                }
                _ => {}
            }
        }
    }

    Ok(())
}

/// Flush accumulated key buffer to PTY. If multiple characters were collected
/// (indicating a paste), wrap in bracketed paste sequences.
fn flush_paste_buffer(app: &mut app::App, buffer: &mut Vec<u8>) -> Result<()> {
    if buffer.is_empty() {
        return Ok(());
    }

    let focused_id = app.ws().focused_pane_id;
    if let Some(pane) = app.ws_mut().panes.get_mut(&focused_id) {
        pane.scroll_reset();
        if buffer.len() > 6 {
            // Likely a paste — wrap in bracketed paste
            let mut data = Vec::with_capacity(buffer.len() + 12);
            data.extend_from_slice(b"\x1b[200~");
            data.extend_from_slice(buffer);
            data.extend_from_slice(b"\x1b[201~");
            pane.write_input(&data)?;
            app.paste_cooldown = 5;
        } else {
            // Normal typing — send directly
            pane.write_input(buffer)?;
        }
    }
    buffer.clear();
    Ok(())
}
