use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, BorderType, Paragraph};
use ratatui::Frame;

use crate::app::{App, DragTarget, FocusTarget};

// ─── Theme (Claude-inspired) ──────────────────────────────
const BG: Color = Color::Rgb(0x0d, 0x11, 0x17);
const PANEL_BG: Color = Color::Rgb(0x13, 0x17, 0x1f);
const BORDER: Color = Color::Rgb(0x2d, 0x33, 0x3b);
const FOCUS_BORDER: Color = Color::Rgb(0x58, 0xa6, 0xff);
const TEXT: Color = Color::Rgb(0xe6, 0xed, 0xf3);
const TEXT_DIM: Color = Color::Rgb(0x6e, 0x76, 0x81);
const ACCENT_GREEN: Color = Color::Rgb(0x3f, 0xb9, 0x50);
const ACCENT_BLUE: Color = Color::Rgb(0x58, 0xa6, 0xff);
const ACCENT_CLAUDE: Color = Color::Rgb(0xd9, 0x77, 0x57);
const HEADER_BG: Color = Color::Rgb(0x16, 0x1b, 0x22);
const ACTIVE_TAB_BG: Color = Color::Rgb(0x0d, 0x11, 0x17);
const ACTIVE_BG: Color = Color::Rgb(0x1c, 0x23, 0x33);
const LINE_NUM_COLOR: Color = Color::Rgb(0x3d, 0x44, 0x4d);
const SCROLL_BG: Color = Color::Rgb(0x2a, 0x1f, 0x14);

const MIN_TERMINAL_WIDTH: u16 = 40;
const MIN_TERMINAL_HEIGHT: u16 = 10;
const MIN_PANE_AREA_WIDTH: u16 = 20;

// ─── File type icons ──────────────────────────────────────
fn file_icon(name: &str) -> (&'static str, Color) {
    let ext = name.rsplit('.').next().unwrap_or("");
    match ext {
        "rs" => ("\u{1f980} ", Color::Rgb(0xde, 0x93, 0x5f)),  // 🦀 orange
        "toml" => ("\u{2699}\u{fe0f} ", Color::Rgb(0x9e, 0x9e, 0x9e)),  // ⚙️ gray
        "lock" => ("\u{1f512} ", Color::Rgb(0x9e, 0x9e, 0x9e)),  // 🔒
        "md" => ("\u{1f4c4} ", Color::Rgb(0x58, 0xa6, 0xff)),   // 📄 blue
        "json" => ("{ ", Color::Rgb(0xf1, 0xe0, 0x5a)),         // { yellow
        "yaml" | "yml" => ("~ ", Color::Rgb(0xf1, 0xe0, 0x5a)), // ~ yellow
        "js" => ("\u{26a1} ", Color::Rgb(0xf1, 0xe0, 0x5a)),    // ⚡ yellow
        "ts" | "tsx" => ("\u{26a1} ", Color::Rgb(0x31, 0x78, 0xc6)), // ⚡ blue
        "jsx" => ("\u{26a1} ", Color::Rgb(0x61, 0xda, 0xfb)),   // ⚡ cyan
        "py" => ("\u{1f40d} ", Color::Rgb(0x35, 0x72, 0xa5)),   // 🐍 blue
        "sh" | "bash" | "zsh" => ("$ ", Color::Rgb(0x3f, 0xb9, 0x50)), // $ green
        "css" | "scss" => ("# ", Color::Rgb(0x56, 0x3d, 0x7c)), // # purple
        "html" => ("< ", Color::Rgb(0xe3, 0x4c, 0x26)),         // < orange
        "gitignore" => ("\u{2022} ", Color::Rgb(0xf0, 0x50, 0x33)), // • git red
        _ => ("\u{2022} ", TEXT_DIM),                             // • default
    }
}

// ─── Main render ──────────────────────────────────────────

pub fn render(app: &mut App, frame: &mut Frame) {
    let area = frame.area();
    app.last_term_size = (area.width, area.height);

    if area.width < MIN_TERMINAL_WIDTH || area.height < MIN_TERMINAL_HEIGHT {
        let msg = Paragraph::new("Terminal too small")
            .style(Style::default().fg(TEXT_DIM).bg(BG))
            .alignment(Alignment::Center);
        frame.render_widget(msg, area);
        return;
    }

    let bg_block = Block::default().style(Style::default().bg(BG));
    frame.render_widget(bg_block, area);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1), // tab bar
            Constraint::Min(1),   // main area
            Constraint::Length(1), // status bar
        ])
        .split(area);

    render_tab_bar(app, frame, chunks[0]);
    render_main_area(app, frame, chunks[1]);
    render_status_bar(app, frame, chunks[2]);
}

// ─── Tab bar ──────────────────────────────────────────────

fn render_tab_bar(app: &mut App, frame: &mut Frame, area: Rect) {
    let mut spans = Vec::new();
    let mut tab_rects = Vec::new();
    let mut x = area.x;

    // Logo
    spans.push(Span::styled(
        " \u{25c8} ",
        Style::default().fg(ACCENT_CLAUDE).bg(HEADER_BG).add_modifier(Modifier::BOLD),
    ));
    x += 3;

    for (i, ws) in app.workspaces.iter().enumerate() {
        let is_active = i == app.active_tab;
        let label = format!(" {} ", ws.name);
        let label_width = label.len() as u16;

        if is_active {
            // Active tab: underline bar ▔ effect via bold + brighter bg
            spans.push(Span::styled(
                label.clone(),
                Style::default()
                    .fg(TEXT)
                    .bg(ACTIVE_TAB_BG)
                    .add_modifier(Modifier::BOLD | Modifier::UNDERLINED),
            ));
        } else {
            spans.push(Span::styled(
                label.clone(),
                Style::default().fg(TEXT_DIM).bg(HEADER_BG),
            ));
        }

        tab_rects.push((i, Rect::new(x, area.y, label_width, 1)));
        x += label_width;

        spans.push(Span::styled(" ", Style::default().bg(HEADER_BG)));
        x += 1;
    }

    // [+] button
    let plus_label = " + ";
    spans.push(Span::styled(
        plus_label,
        Style::default().fg(ACCENT_GREEN).bg(HEADER_BG),
    ));
    let plus_rect = Rect::new(x, area.y, plus_label.len() as u16, 1);
    x += plus_label.len() as u16;

    // Fill remaining
    let remaining = area.width.saturating_sub(x - area.x);
    if remaining > 0 {
        spans.push(Span::styled(
            " ".repeat(remaining as usize),
            Style::default().bg(HEADER_BG),
        ));
    }

    app.last_tab_rects = tab_rects;
    app.last_new_tab_rect = Some(plus_rect);

    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

// ─── Main area ────────────────────────────────────────────

fn render_main_area(app: &mut App, frame: &mut Frame, area: Rect) {
    let tree_width = app.file_tree_width;
    let preview_width = app.preview_width;

    let mut has_tree = app.ws().file_tree_visible;
    let mut has_preview = app.ws().preview.is_active();

    let needed = MIN_PANE_AREA_WIDTH
        + if has_tree { tree_width } else { 0 }
        + if has_preview { preview_width } else { 0 };
    if area.width < needed && has_preview {
        has_preview = false;
    }
    let needed = MIN_PANE_AREA_WIDTH + if has_tree { tree_width } else { 0 };
    if area.width < needed && has_tree {
        has_tree = false;
    }

    let swapped = app.layout_swapped;

    let mut constraints = Vec::new();
    if has_tree {
        constraints.push(Constraint::Length(tree_width));
    }
    if swapped && has_preview {
        constraints.push(Constraint::Length(preview_width));
    }
    constraints.push(Constraint::Min(20));
    if !swapped && has_preview {
        constraints.push(Constraint::Length(preview_width));
    }

    let chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints(constraints)
        .split(area);

    let mut idx = 0;

    if has_tree {
        app.ws_mut().last_file_tree_rect = Some(chunks[idx]);
        render_file_tree(app, frame, chunks[idx]);
        idx += 1;
    } else {
        app.ws_mut().last_file_tree_rect = None;
    }

    if swapped && has_preview {
        app.ws_mut().last_preview_rect = Some(chunks[idx]);
        render_preview(app, frame, chunks[idx]);
        idx += 1;
    }

    render_panes(app, frame, chunks[idx]);
    idx += 1;

    if !swapped && has_preview {
        app.ws_mut().last_preview_rect = Some(chunks[idx]);
        render_preview(app, frame, chunks[idx]);
    }

    if !has_preview {
        app.ws_mut().last_preview_rect = None;
    }
}

// ─── File tree ────────────────────────────────────────────

fn render_file_tree(app: &mut App, frame: &mut Frame, area: Rect) {
    let is_focused = app.ws().focus_target == FocusTarget::FileTree;
    let is_border_active = matches!(
        app.dragging.as_ref().or(app.hover_border.as_ref()),
        Some(DragTarget::FileTreeBorder)
    );
    let border_color = if is_border_active {
        ACCENT_GREEN
    } else if is_focused {
        FOCUS_BORDER
    } else {
        BORDER
    };

    let title_style = if is_focused {
        Style::default().fg(ACCENT_BLUE).add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(TEXT_DIM)
    };

    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(border_color))
        .title(Span::styled(" FILES ", title_style))
        .style(Style::default().bg(PANEL_BG));

    let inner = block.inner(area);
    frame.render_widget(block, area);

    let visible_height = inner.height as usize;
    app.ws_mut().file_tree.ensure_visible(visible_height);

    let entries = app.ws().file_tree.visible_entries();
    let scroll = app.ws().file_tree.scroll_offset;
    let selected = app.ws().file_tree.selected_index;
    let max_width = inner.width as usize;

    for (i, entry) in entries.iter().skip(scroll).take(visible_height).enumerate() {
        let y = inner.y + i as u16;
        let entry_index = scroll + i;
        let is_selected = entry_index == selected;

        // Selection indicator bar on the left
        let indicator = if is_selected { "\u{258e}" } else { " " }; // ▎ or space
        let indicator_style = if is_selected {
            Style::default().fg(ACCENT_BLUE).bg(ACTIVE_BG)
        } else {
            Style::default().fg(PANEL_BG).bg(PANEL_BG)
        };

        // Tree indent with connector lines
        let indent = if entry.depth > 0 {
            let mut s = String::new();
            for _ in 0..entry.depth.saturating_sub(1) {
                s.push_str("\u{2502} "); // │
            }
            s.push_str("\u{251c}\u{2500}"); // ├─
            s
        } else {
            String::new()
        };

        // Icon + name
        let (icon, name_display, name_color) = if entry.is_dir {
            let icon = if entry.is_expanded { "\u{1f4c2} " } else { "\u{1f4c1} " }; // 📂 / 📁
            (icon, &entry.name, ACCENT_BLUE)
        } else {
            let (icon, color) = file_icon(&entry.name);
            (icon, &entry.name, color)
        };

        let content = format!("{}{}{}", indent, icon, name_display);
        let truncated = truncate_to_width(&content, max_width.saturating_sub(1));

        // Build styled spans
        let mut spans = vec![Span::styled(indicator, indicator_style)];

        let content_style = if is_selected {
            Style::default().fg(TEXT).bg(ACTIVE_BG).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(name_color).bg(PANEL_BG)
        };

        spans.push(Span::styled(truncated, content_style));

        // Fill remaining width
        let line_widget = Paragraph::new(Line::from(spans));
        frame.render_widget(line_widget, Rect::new(inner.x, y, inner.width, 1));
    }
}

// ─── Panes ────────────────────────────────────────────────

fn render_panes(app: &mut App, frame: &mut Frame, area: Rect) {
    let rects = app.ws().layout.calculate_rects(area);
    app.ws_mut().last_pane_rects = rects.clone();

    for &(pane_id, rect) in &rects {
        if let Some(pane) = app.ws_mut().panes.get_mut(&pane_id) {
            let inner_rows = rect.height.saturating_sub(2);
            let inner_cols = rect.width.saturating_sub(2);
            let _ = pane.resize(inner_rows, inner_cols); // now returns Result<bool>
        }
    }

    // Update Claude monitor for each pane using the pane's own cwd
    // (may differ from workspace cwd if user cd'd inside the pane)
    let pane_cwds: Vec<(usize, std::path::PathBuf)> = rects
        .iter()
        .filter_map(|&(pane_id, _)| {
            app.ws()
                .panes
                .get(&pane_id)
                .map(|p| (pane_id, p.cwd.clone()))
        })
        .collect();
    for (pane_id, cwd) in pane_cwds {
        app.claude_monitor.update(pane_id, &cwd);
    }

    let focused_id = app.ws().focused_pane_id;
    let focus_target = app.ws().focus_target;
    let selection = app.selection.clone();
    for (pane_id, rect) in rects {
        if let Some(pane) = app.ws().panes.get(&pane_id) {
            let is_focused = pane_id == focused_id && focus_target == FocusTarget::Pane;
            let pane_sel = selection.as_ref().filter(|s| {
                matches!(s.target, crate::app::SelectionTarget::Pane(id) if id == pane_id)
            });
            let claude_state = app.claude_monitor.state(pane_id);
            render_single_pane(pane, is_focused, pane_sel, &claude_state, frame, rect);
        }
    }
}

fn render_single_pane(
    pane: &crate::pane::Pane,
    is_focused: bool,
    selection: Option<&crate::app::TextSelection>,
    claude_state: &crate::claude_monitor::ClaudeState,
    frame: &mut Frame,
    area: Rect,
) {
    let is_claude = pane.is_claude_running();
    let border_color = if is_focused && is_claude {
        ACCENT_CLAUDE
    } else if is_focused {
        FOCUS_BORDER
    } else {
        BORDER
    };

    let is_scrolled = pane.is_scrolled_back();
    let label = if is_claude { "claude" } else { "shell" };

    // Build claude status suffix
    let claude_suffix = if is_claude {
        let mut parts = Vec::new();
        if claude_state.subagent_count > 0 {
            // Show agent type names if available, else just count
            if !claude_state.subagent_types.is_empty() {
                parts.push(format!(
                    "\u{1f916} {}",
                    claude_state.subagent_types.join(", ")
                ));
            } else {
                parts.push(format!("\u{1f916}\u{00d7}{}", claude_state.subagent_count));
            }
        }
        if let Some(ref tool) = claude_state.current_tool {
            parts.push(format!("\u{1f527} {}", tool));
        }
        if parts.is_empty() {
            String::new()
        } else {
            format!(" {} ", parts.join(" "))
        }
    } else {
        String::new()
    };

    let pane_title = if is_focused {
        format!(" \u{25cf} {} [{}]{} ", label, pane.id, claude_suffix)
    } else {
        format!("   {} [{}]{} ", label, pane.id, claude_suffix)
    };

    let title_style = if is_focused && is_claude {
        Style::default().fg(ACCENT_CLAUDE).add_modifier(Modifier::BOLD)
    } else if is_focused {
        Style::default().fg(FOCUS_BORDER).add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(TEXT_DIM)
    };

    // Bottom title: scroll indicator OR claude stats
    let bottom_title = if is_scrolled {
        Line::from(Span::styled(
            " \u{2191} SCROLL ",
            Style::default().fg(ACCENT_CLAUDE).bg(SCROLL_BG).add_modifier(Modifier::BOLD),
        ))
    } else if is_claude {
        let mut spans = Vec::new();

        // Todo progress bar
        let (completed, total) = claude_state.todo_progress();
        if total > 0 {
            let bar = make_progress_bar(completed, total, 10);
            spans.push(Span::styled(
                format!(" \u{2713} {} {}/{} ", bar, completed, total),
                Style::default().fg(ACCENT_GREEN),
            ));
            // Show current in-progress task
            if let Some(current) = claude_state
                .todos
                .iter()
                .find(|t| t.status == "in_progress")
            {
                let short = truncate_to_width(&current.content, 30);
                spans.push(Span::styled(
                    format!("\u{25b6} {} ", short),
                    Style::default().fg(ACCENT_BLUE),
                ));
            }
        }

        // Total tokens used this session
        let total_tokens = claude_state.total_tokens();
        if total_tokens > 0 {
            spans.push(Span::styled(
                format!(" {} tokens ", format_tokens(total_tokens)),
                Style::default().fg(TEXT_DIM),
            ));
        }

        Line::from(spans)
    } else {
        Line::from("")
    };

    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(border_color))
        .title(Span::styled(pane_title, title_style))
        .title_bottom(bottom_title)
        .style(Style::default().bg(BG));

    let inner = block.inner(area);
    frame.render_widget(block, area);

    if pane.exited {
        let msg = Paragraph::new("\u{2718} Process exited")
            .style(Style::default().fg(TEXT_DIM).bg(BG))
            .alignment(Alignment::Center);
        frame.render_widget(msg, inner);
    } else {
        render_terminal_content(pane, is_focused, selection, frame, inner);
    }
}

fn render_terminal_content(
    pane: &crate::pane::Pane,
    is_focused: bool,
    selection: Option<&crate::app::TextSelection>,
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
                let display_char = if contents.is_empty() { " " } else { contents };

                let fg = vt100_color_to_ratatui(cell.fgcolor());
                let bg = vt100_color_to_ratatui(cell.bgcolor());

                let mut modifiers = Modifier::empty();
                if cell.bold() { modifiers |= Modifier::BOLD; }
                if cell.italic() { modifiers |= Modifier::ITALIC; }
                if cell.underline() { modifiers |= Modifier::UNDERLINED; }

                let style = if cell.inverse() {
                    Style::default().fg(bg).bg(fg).add_modifier(modifiers)
                } else {
                    Style::default().fg(fg).bg(bg).add_modifier(modifiers)
                };

                // Apply selection highlight (only if dragged, not single click)
                let has_selection = selection.map_or(false, |s| {
                    let (sr, sc, er, ec) = s.normalized();
                    (sr != er || sc != ec) && s.contains(row as u16, col as u16)
                });
                let final_style = if has_selection {
                    Style::default()
                        .fg(Color::Rgb(0x0d, 0x11, 0x17))
                        .bg(Color::Rgb(0x58, 0xa6, 0xff))
                } else {
                    style
                };

                if let Some(buf_cell) = buf.cell_mut((x, y)) {
                    buf_cell.set_symbol(display_char);
                    buf_cell.set_style(final_style);
                }
            }
        }
    }

    // Show cursor when focused.
    // For non-Claude panes, respect the PTY's hide_cursor request.
    // For Claude Code, always show because Claude relies on the terminal cursor.
    let show_cursor = is_focused && (!screen.hide_cursor() || pane.is_claude_running());
    if show_cursor {
        let cursor = screen.cursor_position();
        // For Claude Code, shift cursor 1 column left because Claude draws its own
        // block character at the cursor position, and the PTY cursor would otherwise
        // appear one column after with a visible gap.
        let cursor_x = if pane.is_claude_running() {
            area.x + cursor.1.saturating_sub(1)
        } else {
            area.x + cursor.1
        };
        let cursor_y = area.y + cursor.0;
        if cursor_x < area.x + area.width && cursor_y < area.y + area.height {
            frame.set_cursor_position((cursor_x, cursor_y));
        }
    }

    drop(parser); // release lock before scrollbar_info

    // Scrollbar on the right edge
    let (scroll_offset, total_lines) = pane.scrollbar_info();
    if total_lines > rows {
        let scrollbar_x = area.x + area.width - 1;
        let max_scroll = total_lines.saturating_sub(rows);
        let visible_ratio = rows as f32 / total_lines as f32;
        let thumb_height = (area.height as f32 * visible_ratio).max(1.0) as u16;

        // Position: 0 = bottom, max_scroll = top
        let scroll_ratio = if max_scroll > 0 {
            1.0 - (scroll_offset as f32 / max_scroll as f32)
        } else {
            1.0
        };
        let thumb_top = ((area.height - thumb_height) as f32 * scroll_ratio) as u16;

        let buf = frame.buffer_mut();
        for row in 0..area.height {
            let y = area.y + row;
            let is_thumb = row >= thumb_top && row < thumb_top + thumb_height;
            let (sym, style) = if is_thumb {
                ("\u{2588}", Style::default().fg(Color::Rgb(0x58, 0x5e, 0x68))) // █ thumb
            } else {
                ("\u{2502}", Style::default().fg(Color::Rgb(0x2d, 0x33, 0x3b))) // │ track
            };
            if let Some(cell) = buf.cell_mut((scrollbar_x, y)) {
                cell.set_symbol(sym);
                cell.set_style(style);
            }
        }
    }
}

// ─── Preview ──────────────────────────────────────────────

fn render_preview(app: &App, frame: &mut Frame, area: Rect) {
    let ws = app.ws();
    let is_focused = ws.focus_target == FocusTarget::Preview;
    let filename = ws.preview.filename();
    let title = format!(" {} ", filename);

    let is_border_active = matches!(
        app.dragging.as_ref().or(app.hover_border.as_ref()),
        Some(DragTarget::PreviewBorder)
    );
    let border_color = if is_border_active {
        ACCENT_GREEN
    } else if is_focused {
        ACCENT_CLAUDE
    } else {
        BORDER
    };

    // Line count in bottom-right
    let line_info = if !ws.preview.is_binary {
        let total = ws.preview.lines.len();
        let current = ws.preview.scroll_offset + 1;
        Span::styled(
            format!(" {}/{} ", current, total),
            Style::default().fg(TEXT_DIM),
        )
    } else {
        Span::default()
    };

    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(border_color))
        .title(Span::styled(
            title,
            Style::default().fg(ACCENT_CLAUDE).add_modifier(Modifier::BOLD),
        ))
        .title_bottom(line_info)
        .style(Style::default().bg(PANEL_BG));

    let inner = block.inner(area);
    frame.render_widget(block, area);

    if ws.preview.is_binary {
        let msg = Paragraph::new("\u{2718} バイナリファイルです")
            .style(Style::default().fg(TEXT_DIM).bg(PANEL_BG));
        frame.render_widget(msg, inner);
        return;
    }

    let visible_height = inner.height as usize;
    let scroll = ws.preview.scroll_offset;
    let has_highlights = !ws.preview.highlighted_lines.is_empty();

    for i in 0..visible_height {
        let line_idx = scroll + i;
        if line_idx >= ws.preview.lines.len() {
            break;
        }

        let y = inner.y + i as u16;
        let line_num = line_idx + 1;
        let num_str = format!("{:>4}\u{2502}", line_num);
        let max_content = (inner.width as usize).saturating_sub(5);

        let mut spans = vec![Span::styled(num_str, Style::default().fg(LINE_NUM_COLOR))];

        if has_highlights && line_idx < ws.preview.highlighted_lines.len() {
            let mut used_width = 0;
            for styled_span in &ws.preview.highlighted_lines[line_idx] {
                if used_width >= max_content {
                    break;
                }
                let remaining = max_content - used_width;
                let text = truncate_to_width(&styled_span.text, remaining);
                used_width += unicode_width::UnicodeWidthStr::width(text.as_str());
                let (r, g, b) = styled_span.fg;
                spans.push(Span::styled(text, Style::default().fg(Color::Rgb(r, g, b))));
            }
        } else {
            let content = truncate_to_width(&ws.preview.lines[line_idx], max_content);
            spans.push(Span::styled(content, Style::default().fg(TEXT)));
        }

        let paragraph = Paragraph::new(Line::from(spans)).style(Style::default().bg(PANEL_BG));
        frame.render_widget(paragraph, Rect::new(inner.x, y, inner.width, 1));
    }

    // Selection highlight overlay. Painted after the text so it
    // inverts colors of cells already in the buffer, matching the
    // pane selection style. The right edge of each row's highlight
    // is clamped to the actual line width (not the content area
    // width) so the painted band never extends past the text that
    // extract_preview_selected_text will actually copy.
    if let Some(sel) = app.selection.as_ref() {
        if matches!(sel.target, crate::app::SelectionTarget::Preview) {
            let (sr, sc, er, ec) = sel.normalized();
            if sr != er || sc != ec {
                let content = sel.content_rect;
                let scroll = ws.preview.scroll_offset;
                let lines_len = ws.preview.lines.len();
                let buf = frame.buffer_mut();
                for screen_row in sr..=er {
                    let y = content.y + screen_row;
                    if y >= content.y + content.height {
                        break;
                    }
                    // Resolve the absolute line index this screen row
                    // maps to under the current scroll. If it's past
                    // EOF, skip (selection was made on a longer file
                    // or we've scrolled past).
                    let abs = scroll + screen_row as usize;
                    if abs >= lines_len {
                        break;
                    }
                    let line_width = ws
                        .preview
                        .lines
                        .get(abs)
                        .map(|s| unicode_width::UnicodeWidthStr::width(s.as_str()) as u16)
                        .unwrap_or(0);
                    // No content on this line → nothing to highlight.
                    if line_width == 0 {
                        continue;
                    }

                    let col_start = if screen_row == sr { sc } else { 0 };
                    let row_col_end = if screen_row == er { ec } else {
                        line_width.saturating_sub(1)
                    };
                    // Clamp to both the content area and the actual
                    // line width so trailing blank cells don't get
                    // painted.
                    let col_end = row_col_end
                        .min(content.width.saturating_sub(1))
                        .min(line_width.saturating_sub(1));
                    if col_start > col_end {
                        continue;
                    }

                    for screen_col in col_start..=col_end {
                        let x = content.x + screen_col;
                        if x >= content.x + content.width {
                            break;
                        }
                        if let Some(cell) = buf.cell_mut((x, y)) {
                            cell.set_style(
                                Style::default()
                                    .fg(Color::Rgb(0x0d, 0x11, 0x17))
                                    .bg(Color::Rgb(0x58, 0xa6, 0xff)),
                            );
                        }
                    }
                }
            }
        }
    }
}

// ─── Status bar (context-aware) ───────────────────────────

fn render_status_bar(app: &App, frame: &mut Frame, area: Rect) {
    let focus = app.ws().focus_target;

    let hints = match focus {
        FocusTarget::Preview => Line::from(vec![
            Span::styled(" Scroll", Style::default().fg(ACCENT_BLUE)),
            Span::styled(" スクロール  ", Style::default().fg(TEXT_DIM)),
            Span::styled("^W", Style::default().fg(ACCENT_BLUE)),
            Span::styled(" 閉じる  ", Style::default().fg(TEXT_DIM)),
            Span::styled("^P", Style::default().fg(ACCENT_BLUE)),
            Span::styled(" 配置替  ", Style::default().fg(TEXT_DIM)),
            Span::styled("^Q", Style::default().fg(ACCENT_BLUE)),
            Span::styled(" 終了", Style::default().fg(TEXT_DIM)),
        ]),
        FocusTarget::FileTree => Line::from(vec![
            Span::styled(" j/k", Style::default().fg(ACCENT_BLUE)),
            Span::styled(" 移動  ", Style::default().fg(TEXT_DIM)),
            Span::styled("Enter", Style::default().fg(ACCENT_BLUE)),
            Span::styled(" 開く  ", Style::default().fg(TEXT_DIM)),
            Span::styled(".", Style::default().fg(ACCENT_BLUE)),
            Span::styled(" 隠しファイル  ", Style::default().fg(TEXT_DIM)),
            Span::styled("Esc", Style::default().fg(ACCENT_BLUE)),
            Span::styled(" 戻る  ", Style::default().fg(TEXT_DIM)),
            Span::styled("^F", Style::default().fg(ACCENT_BLUE)),
            Span::styled(" 閉じる  ", Style::default().fg(TEXT_DIM)),
            Span::styled("^Q", Style::default().fg(ACCENT_BLUE)),
            Span::styled(" 終了", Style::default().fg(TEXT_DIM)),
        ]),
        FocusTarget::Pane => Line::from(vec![
            Span::styled(" ^D", Style::default().fg(ACCENT_BLUE)),
            Span::styled(" 縦分割  ", Style::default().fg(TEXT_DIM)),
            Span::styled("^E", Style::default().fg(ACCENT_BLUE)),
            Span::styled(" 横分割  ", Style::default().fg(TEXT_DIM)),
            Span::styled("^W", Style::default().fg(ACCENT_BLUE)),
            Span::styled(" 閉じる  ", Style::default().fg(TEXT_DIM)),
            Span::styled("^T", Style::default().fg(ACCENT_BLUE)),
            Span::styled(" 新タブ  ", Style::default().fg(TEXT_DIM)),
            Span::styled("^F", Style::default().fg(ACCENT_BLUE)),
            Span::styled(" ツリー  ", Style::default().fg(TEXT_DIM)),
            Span::styled("^P", Style::default().fg(ACCENT_BLUE)),
            Span::styled(" 配置替  ", Style::default().fg(TEXT_DIM)),
            Span::styled("^Q", Style::default().fg(ACCENT_BLUE)),
            Span::styled(" 終了", Style::default().fg(TEXT_DIM)),
        ]),
    };

    let status = Paragraph::new(hints).style(Style::default().bg(HEADER_BG));
    frame.render_widget(status, area);

    // Right-side info: Claude state of focused pane
    let focused_id = app.ws().focused_pane_id;
    let claude_state = app.claude_monitor.state(focused_id);
    let has_claude = app
        .ws()
        .panes
        .get(&focused_id)
        .map_or(false, |p| p.is_claude_running());

    let mut right_spans = Vec::new();

    if has_claude {
        // Model
        if let Some(model) = claude_state.short_model() {
            right_spans.push(Span::styled(
                format!(" \u{1f9e0} {} ", model),
                Style::default().fg(ACCENT_CLAUDE),
            ));
        }

        // Context usage
        if claude_state.context_tokens > 0 {
            let ratio = claude_state.context_usage();
            let bar = make_progress_bar(
                (ratio * 10.0) as usize,
                10,
                6,
            );
            let color = if ratio > 0.9 {
                Color::Rgb(0xf8, 0x51, 0x49) // red
            } else if ratio > 0.7 {
                Color::Rgb(0xd2, 0x99, 0x22) // yellow
            } else {
                ACCENT_GREEN
            };
            right_spans.push(Span::styled(
                format!(
                    " {} {}/{} ",
                    bar,
                    format_tokens(claude_state.context_tokens),
                    format_tokens(claude_state.context_limit())
                ),
                Style::default().fg(color),
            ));
        }
    }

    // Git branch (even without claude)
    if let Some(ref branch) = claude_state.git_branch {
        let short = truncate_to_width(branch, 20);
        right_spans.push(Span::styled(
            format!(" \u{2387} {} ", short),
            Style::default().fg(ACCENT_BLUE),
        ));
    }

    // Update notice (highest priority — overrides above if present)
    if let Some(new_version) = app.version_info.update_available() {
        right_spans.push(Span::styled(
            format!(" \u{2191} v{} ", new_version),
            Style::default()
                .fg(ACCENT_CLAUDE)
                .add_modifier(Modifier::BOLD),
        ));
    }

    if !right_spans.is_empty() {
        let total_width: u16 = right_spans
            .iter()
            .map(|s| unicode_width::UnicodeWidthStr::width(s.content.as_ref()) as u16)
            .sum();
        if area.width > total_width {
            let right_rect =
                Rect::new(area.x + area.width - total_width, area.y, total_width, 1);
            let widget =
                Paragraph::new(Line::from(right_spans)).style(Style::default().bg(HEADER_BG));
            frame.render_widget(widget, right_rect);
        }
    }
}

// ─── Helpers ──────────────────────────────────────────────

/// Build a progress bar string like `▓▓▓▓░░░░░░`.
fn make_progress_bar(current: usize, total: usize, width: usize) -> String {
    if total == 0 {
        return String::new();
    }
    let filled = ((current as f32 / total as f32) * width as f32).round() as usize;
    let filled = filled.min(width);
    let mut s = String::with_capacity(width * 3);
    for _ in 0..filled {
        s.push('\u{2593}'); // ▓
    }
    for _ in filled..width {
        s.push('\u{2591}'); // ░
    }
    s
}

/// Format token count: 1234 → "1.2k", 1234567 → "1.2M"
fn format_tokens(n: u64) -> String {
    if n >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    } else if n >= 1_000 {
        format!("{:.1}k", n as f64 / 1_000.0)
    } else {
        n.to_string()
    }
}

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

fn vt100_color_to_ratatui(color: vt100::Color) -> Color {
    match color {
        vt100::Color::Default => Color::Reset,
        vt100::Color::Idx(idx) => Color::Indexed(idx),
        vt100::Color::Rgb(r, g, b) => Color::Rgb(r, g, b),
    }
}
