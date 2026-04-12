mod app;
mod filetree;
mod pane;
mod preview;
mod ui;

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
    // Install panic hook to restore terminal state on crash
    let default_hook = panic::take_hook();
    panic::set_hook(Box::new(move |info| {
        let _ = disable_raw_mode();
        let _ = execute!(io::stdout(), crossterm::event::DisableMouseCapture);
        let _ = execute!(io::stdout(), LeaveAlternateScreen);
        default_hook(info);
    }));

    // Setup terminal
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    execute!(stdout, crossterm::event::EnableMouseCapture)?;

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
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;

    result
}

fn run_event_loop(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    app: &mut app::App,
) -> Result<()> {
    loop {
        // Drain any PTY output events
        app.drain_pty_events();

        // Only render when something changed
        if app.dirty {
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
                            app.forward_key_to_pty(key)?;
                        }
                        app.dirty = true;
                    }
                }
                Event::Mouse(mouse) => {
                    app.handle_mouse_event(mouse);
                    app.dirty = true;
                }
                Event::Resize(_cols, _rows) => {
                    app.dirty = true;
                }
                _ => {}
            }
        }
    }

    Ok(())
}
