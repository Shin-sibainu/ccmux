use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui::Frame;
use crate::app::{App, FocusTarget};

// Theme colors
pub const BG: Color = Color::Rgb(0x0d, 0x11, 0x17);
pub const PANEL_BG: Color = Color::Rgb(0x16, 0x1b, 0x22);
pub const BORDER: Color = Color::Rgb(0x30, 0x36, 0x3d);
pub const FOCUS_BORDER: Color = Color::Rgb(0x58, 0xa6, 0xff);
pub const TEXT: Color = Color::Rgb(0xe6, 0xed, 0xf3);
pub const TEXT_DIM: Color = Color::Rgb(0x8b, 0x94, 0x9e);
pub const ACCENT_GREEN: Color = Color::Rgb(0x3f, 0xb9, 0x50);
#[allow(dead_code)]
pub const ACCENT_YELLOW: Color = Color::Rgb(0xd2, 0x99, 0x22);
pub const HEADER_BG: Color = Color::Rgb(0x21, 0x26, 0x2d);

const FILE_TREE_WIDTH: u16 = 20;
const PREVIEW_WIDTH: u16 = 40;

const MIN_TERMINAL_WIDTH: u16 = 40;
const MIN_TERMINAL_HEIGHT: u16 = 10;

/// Render the entire UI.
pub fn render(app: &mut App, frame: &mut Frame) {
    let area = frame.area();

    // Show message if terminal is too small
    if area.width < MIN_TERMINAL_WIDTH || area.height < MIN_TERMINAL_HEIGHT {
        let msg = Paragraph::new("Terminal too small")
            .style(Style::default().fg(TEXT_DIM).bg(BG))
            .alignment(ratatui::layout::Alignment::Center);
        frame.render_widget(msg, area);
        return;
    }

    // Clear background
    let bg_block = Block::default().style(Style::default().bg(BG));
    frame.render_widget(bg_block, area);

    // Layout: title bar (1) | main area | status bar (1)
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(1),
            Constraint::Length(1),
        ])
        .split(area);

    render_title_bar(app, frame, chunks[0]);
    render_main_area(app, frame, chunks[1]);
    render_status_bar(frame, chunks[2]);
}

/// Render the title bar.
fn render_title_bar(app: &App, frame: &mut Frame, area: Rect) {
    let pane_count = app.layout.pane_count();
    let title_text = if pane_count > 1 {
        format!(" ccmux — {} panes", pane_count)
    } else {
        " ccmux".to_string()
    };

    let title = Paragraph::new(Line::from(vec![
        Span::styled(title_text, Style::default().fg(TEXT)),
    ]))
    .style(Style::default().bg(HEADER_BG));

    frame.render_widget(title, area);
}

/// Minimum pane area width to keep the UI usable.
const MIN_PANE_AREA_WIDTH: u16 = 20;

/// Render the main area: [file tree] | panes | [preview]
fn render_main_area(app: &mut App, frame: &mut Frame, area: Rect) {
    // Auto-hide panels if terminal is too narrow
    let mut has_tree = app.file_tree_visible;
    let mut has_preview = app.preview.is_active();

    let needed = MIN_PANE_AREA_WIDTH
        + if has_tree { FILE_TREE_WIDTH } else { 0 }
        + if has_preview { PREVIEW_WIDTH } else { 0 };

    if area.width < needed && has_preview {
        has_preview = false;
    }
    let needed = MIN_PANE_AREA_WIDTH + if has_tree { FILE_TREE_WIDTH } else { 0 };
    if area.width < needed && has_tree {
        has_tree = false;
    }

    // Build horizontal constraints dynamically
    let mut constraints = Vec::new();
    if has_tree {
        constraints.push(Constraint::Length(FILE_TREE_WIDTH));
    }
    constraints.push(Constraint::Min(20)); // pane area
    if has_preview {
        constraints.push(Constraint::Length(PREVIEW_WIDTH));
    }

    let chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints(constraints)
        .split(area);

    let mut idx = 0;

    if has_tree {
        app.last_file_tree_rect = Some(chunks[idx]);
        render_file_tree(app, frame, chunks[idx]);
        idx += 1;
    } else {
        app.last_file_tree_rect = None;
    }

    render_panes(app, frame, chunks[idx]);
    idx += 1;

    if has_preview {
        app.last_preview_rect = Some(chunks[idx]);
        render_preview(app, frame, chunks[idx]);
    } else {
        app.last_preview_rect = None;
    }
}

/// Render the file tree sidebar.
fn render_file_tree(app: &mut App, frame: &mut Frame, area: Rect) {
    let is_focused = app.focus_target == FocusTarget::FileTree;
    let border_color = if is_focused { FOCUS_BORDER } else { BORDER };

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(border_color))
        .title(Span::styled(" FILES ", Style::default().fg(TEXT)))
        .style(Style::default().bg(PANEL_BG));

    let inner = block.inner(area);
    frame.render_widget(block, area);

    let visible_height = inner.height as usize;
    app.file_tree.ensure_visible(visible_height);

    let entries = app.file_tree.visible_entries();
    let scroll = app.file_tree.scroll_offset;
    let selected = app.file_tree.selected_index;

    for (i, entry) in entries.iter().skip(scroll).take(visible_height).enumerate() {
        let y = inner.y + i as u16;
        let entry_index = scroll + i;

        // Indent based on depth
        let indent = "  ".repeat(entry.depth);

        // Icon
        let icon = if entry.is_dir {
            if entry.is_expanded { "\u{25bc} " } else { "\u{25b6} " }
        } else {
            "  "
        };

        let name = &entry.name;
        let display = format!("{}{}{}", indent, icon, name);

        // Truncate to fit (respecting CJK double-width characters)
        let max_width = inner.width as usize;
        let truncated = truncate_to_width(&display, max_width);

        let style = if entry_index == selected {
            Style::default()
                .fg(TEXT)
                .bg(Color::Rgb(0x1c, 0x23, 0x33))
                .add_modifier(Modifier::BOLD)
        } else if entry.is_dir {
            Style::default().fg(FOCUS_BORDER)
        } else {
            Style::default().fg(TEXT_DIM)
        };

        let line = Paragraph::new(truncated).style(style);
        frame.render_widget(
            line,
            Rect::new(inner.x, y, inner.width, 1),
        );
    }
}

/// Render all panes using the layout tree.
fn render_panes(app: &mut App, frame: &mut Frame, area: Rect) {
    let rects = app.layout.calculate_rects(area);

    // Cache rects for mouse hit testing
    app.last_pane_rects = rects.clone();

    // Resize PTYs to match their actual rects
    for &(pane_id, rect) in &rects {
        if let Some(pane) = app.panes.get_mut(&pane_id) {
            let inner_rows = rect.height.saturating_sub(2);
            let inner_cols = rect.width.saturating_sub(2);
            let _ = pane.resize(inner_rows, inner_cols);
        }
    }

    // Render each pane
    for (pane_id, rect) in rects {
        if let Some(pane) = app.panes.get(&pane_id) {
            let is_focused = pane_id == app.focused_pane_id
                && app.focus_target == FocusTarget::Pane;
            render_single_pane(pane, is_focused, frame, rect);
        }
    }
}

/// Render a single pane with its border and terminal content.
fn render_single_pane(pane: &crate::pane::Pane, is_focused: bool, frame: &mut Frame, area: Rect) {
    let border_color = if is_focused { FOCUS_BORDER } else { BORDER };

    let pane_title = if is_focused {
        format!(" claude [{}] \u{25cf} ", pane.id)
    } else {
        format!(" claude [{}] ", pane.id)
    };

    let title_style = if is_focused {
        Style::default().fg(ACCENT_GREEN)
    } else {
        Style::default().fg(TEXT_DIM)
    };

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(border_color))
        .title(Span::styled(pane_title, title_style))
        .style(Style::default().bg(BG));

    let inner = block.inner(area);
    frame.render_widget(block, area);

    if pane.exited {
        let msg = Paragraph::new("[Process exited]")
            .style(Style::default().fg(TEXT_DIM).bg(BG))
            .alignment(ratatui::layout::Alignment::Center);
        frame.render_widget(msg, inner);
    } else {
        render_terminal_content(pane, is_focused, frame, inner);
    }
}

/// Render terminal content from a pane's vt100 parser.
fn render_terminal_content(
    pane: &crate::pane::Pane,
    is_focused: bool,
    frame: &mut Frame,
    area: Rect,
) {
    let parser = pane.parser.lock().unwrap_or_else(|e| e.into_inner());
    let screen = parser.screen();

    let rows = area.height as usize;
    let cols = area.width as usize;
    let buf = frame.buffer_mut();

    for row in 0..rows {
        for col in 0..cols {
            let cell = screen.cell(row as u16, col as u16);
            if let Some(cell) = cell {
                let x = area.x + col as u16;
                let y = area.y + row as u16;

                let contents = cell.contents();
                // Use &str directly — no heap allocation
                let display_char = if contents.is_empty() { " " } else { contents };

                let fg = vt100_color_to_ratatui(cell.fgcolor());
                let bg = vt100_color_to_ratatui(cell.bgcolor());

                // Build modifier flags once
                let mut modifiers = Modifier::empty();
                if cell.bold() {
                    modifiers |= Modifier::BOLD;
                }
                if cell.italic() {
                    modifiers |= Modifier::ITALIC;
                }
                if cell.underline() {
                    modifiers |= Modifier::UNDERLINED;
                }

                let style = if cell.inverse() {
                    Style::default().fg(bg).bg(fg).add_modifier(modifiers)
                } else {
                    Style::default().fg(fg).bg(bg).add_modifier(modifiers)
                };

                if let Some(buf_cell) = buf.cell_mut((x, y)) {
                    buf_cell.set_symbol(display_char);
                    buf_cell.set_style(style);
                }
            }
        }
    }

    // Show cursor only for the focused pane
    if is_focused {
        let cursor = screen.cursor_position();
        let cursor_x = area.x + cursor.1;
        let cursor_y = area.y + cursor.0;
        if cursor_x < area.x + area.width && cursor_y < area.y + area.height {
            frame.set_cursor_position((cursor_x, cursor_y));
        }
    }
}

/// Render the file preview panel.
fn render_preview(app: &App, frame: &mut Frame, area: Rect) {
    let filename = app.preview.filename();
    let title = format!(" {} ", filename);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(BORDER))
        .title(Span::styled(title, Style::default().fg(TEXT)))
        .style(Style::default().bg(PANEL_BG));

    let inner = block.inner(area);
    frame.render_widget(block, area);

    if app.preview.is_binary {
        let msg = Paragraph::new("バイナリファイルです")
            .style(Style::default().fg(TEXT_DIM).bg(PANEL_BG));
        frame.render_widget(msg, inner);
        return;
    }

    let visible_height = inner.height as usize;
    let scroll = app.preview.scroll_offset;

    for (i, line) in app
        .preview
        .lines
        .iter()
        .skip(scroll)
        .take(visible_height)
        .enumerate()
    {
        let y = inner.y + i as u16;
        let line_num = scroll + i + 1;

        // Line number (4 chars) + content
        let num_str = format!("{:>4} ", line_num);
        let max_content = (inner.width as usize).saturating_sub(5);
        let content = truncate_to_width(line, max_content);

        // Style: line number in dim, content in normal
        let paragraph = Paragraph::new(Line::from(vec![
            Span::styled(
                num_str,
                Style::default().fg(TEXT_DIM),
            ),
            Span::styled(
                content,
                Style::default().fg(TEXT),
            ),
        ]))
        .style(Style::default().bg(PANEL_BG));

        frame.render_widget(
            paragraph,
            Rect::new(inner.x, y, inner.width, 1),
        );
    }
}

/// Truncate a string to fit within a given display width,
/// respecting CJK double-width characters.
fn truncate_to_width(s: &str, max_width: usize) -> String {
    let mut result = String::new();
    let mut width = 0;
    for ch in s.chars() {
        let ch_width = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
        if width + ch_width > max_width {
            break;
        }
        result.push(ch);
        width += ch_width;
    }
    result
}

/// Convert vt100 color to ratatui color.
fn vt100_color_to_ratatui(color: vt100::Color) -> Color {
    match color {
        vt100::Color::Default => Color::Reset,
        vt100::Color::Idx(idx) => Color::Indexed(idx),
        vt100::Color::Rgb(r, g, b) => Color::Rgb(r, g, b),
    }
}

/// Render the status bar with keybinding hints.
fn render_status_bar(frame: &mut Frame, area: Rect) {
    let hints = Line::from(vec![
        Span::styled(" ^D", Style::default().fg(FOCUS_BORDER)),
        Span::styled(" 縦分割  ", Style::default().fg(TEXT_DIM)),
        Span::styled("^E", Style::default().fg(FOCUS_BORDER)),
        Span::styled(" 横分割  ", Style::default().fg(TEXT_DIM)),
        Span::styled("^W", Style::default().fg(FOCUS_BORDER)),
        Span::styled(" 閉じる  ", Style::default().fg(TEXT_DIM)),
        Span::styled("Tab", Style::default().fg(FOCUS_BORDER)),
        Span::styled(" 切替  ", Style::default().fg(TEXT_DIM)),
        Span::styled("^F", Style::default().fg(FOCUS_BORDER)),
        Span::styled(" ツリー  ", Style::default().fg(TEXT_DIM)),
        Span::styled("^Q", Style::default().fg(FOCUS_BORDER)),
        Span::styled(" 終了", Style::default().fg(TEXT_DIM)),
    ]);

    let status = Paragraph::new(hints).style(Style::default().bg(HEADER_BG));
    frame.render_widget(status, area);
}
