use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, Sender};

use anyhow::Result;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::layout::Rect;

use crate::filetree::FileTree;
use crate::pane::Pane;
use crate::preview::Preview;

/// Events dispatched within the app.
pub enum AppEvent {
    /// PTY output received for a pane.
    PtyOutput(#[allow(dead_code)] usize),
    /// PTY process exited for a pane.
    PtyEof(usize),
}

/// Split direction for layout.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum SplitDirection {
    Vertical,
    Horizontal,
}

/// Which area has focus.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum FocusTarget {
    Pane,
    FileTree,
}

/// Binary tree node for pane layout.
pub enum LayoutNode {
    Leaf {
        pane_id: usize,
    },
    Split {
        direction: SplitDirection,
        first: Box<LayoutNode>,
        second: Box<LayoutNode>,
    },
}

impl LayoutNode {
    /// Collect all leaf pane IDs in visual order (left-to-right, top-to-bottom).
    pub fn collect_pane_ids(&self) -> Vec<usize> {
        match self {
            LayoutNode::Leaf { pane_id } => vec![*pane_id],
            LayoutNode::Split { first, second, .. } => {
                let mut ids = first.collect_pane_ids();
                ids.extend(second.collect_pane_ids());
                ids
            }
        }
    }

    /// Calculate the Rect for each pane given an available area.
    pub fn calculate_rects(&self, area: Rect) -> Vec<(usize, Rect)> {
        match self {
            LayoutNode::Leaf { pane_id } => vec![(*pane_id, area)],
            LayoutNode::Split {
                direction,
                first,
                second,
            } => {
                let (first_area, second_area) = split_rect(area, *direction);
                let mut result = first.calculate_rects(first_area);
                result.extend(second.calculate_rects(second_area));
                result
            }
        }
    }

    /// Split the leaf with the given pane_id into two leaves.
    pub fn split_pane(&mut self, target_id: usize, new_id: usize, direction: SplitDirection) -> bool {
        match self {
            LayoutNode::Leaf { pane_id } => {
                if *pane_id == target_id {
                    let old_id = *pane_id;
                    *self = LayoutNode::Split {
                        direction,
                        first: Box::new(LayoutNode::Leaf { pane_id: old_id }),
                        second: Box::new(LayoutNode::Leaf { pane_id: new_id }),
                    };
                    true
                } else {
                    false
                }
            }
            LayoutNode::Split { first, second, .. } => {
                first.split_pane(target_id, new_id, direction)
                    || second.split_pane(target_id, new_id, direction)
            }
        }
    }

    /// Remove a pane by ID.
    pub fn remove_pane(&mut self, target_id: usize) -> bool {
        match self {
            LayoutNode::Leaf { .. } => false,
            LayoutNode::Split { first, second, .. } => {
                if let LayoutNode::Leaf { pane_id } = first.as_ref() {
                    if *pane_id == target_id {
                        let second = std::mem::replace(
                            second.as_mut(),
                            LayoutNode::Leaf { pane_id: 0 },
                        );
                        *self = second;
                        return true;
                    }
                }
                if let LayoutNode::Leaf { pane_id } = second.as_ref() {
                    if *pane_id == target_id {
                        let first = std::mem::replace(
                            first.as_mut(),
                            LayoutNode::Leaf { pane_id: 0 },
                        );
                        *self = first;
                        return true;
                    }
                }
                first.remove_pane(target_id) || second.remove_pane(target_id)
            }
        }
    }

    /// Count the number of leaf panes.
    pub fn pane_count(&self) -> usize {
        match self {
            LayoutNode::Leaf { .. } => 1,
            LayoutNode::Split { first, second, .. } => {
                first.pane_count() + second.pane_count()
            }
        }
    }
}

/// Split a Rect into two halves based on direction (50:50).
fn split_rect(area: Rect, direction: SplitDirection) -> (Rect, Rect) {
    match direction {
        SplitDirection::Vertical => {
            let half = area.width / 2;
            let first = Rect::new(area.x, area.y, half, area.height);
            let second = Rect::new(area.x + half, area.y, area.width - half, area.height);
            (first, second)
        }
        SplitDirection::Horizontal => {
            let half = area.height / 2;
            let first = Rect::new(area.x, area.y, area.width, half);
            let second = Rect::new(area.x, area.y + half, area.width, area.height - half);
            (first, second)
        }
    }
}

/// Application state.
pub struct App {
    pub panes: HashMap<usize, Pane>,
    pub layout: LayoutNode,
    pub focused_pane_id: usize,
    pub should_quit: bool,
    pub event_tx: Sender<AppEvent>,
    pub event_rx: Receiver<AppEvent>,
    next_pane_id: usize,
    pub dirty: bool,
    // File tree & preview
    pub file_tree: FileTree,
    pub file_tree_visible: bool,
    pub preview: Preview,
    pub focus_target: FocusTarget,
    // Cached rects for mouse hit testing (updated on each render)
    pub last_pane_rects: Vec<(usize, Rect)>,
    pub last_file_tree_rect: Option<Rect>,
    pub last_preview_rect: Option<Rect>,
}

impl App {
    /// Create a new App with a single pane.
    pub fn new(rows: u16, cols: u16) -> Result<Self> {
        let (event_tx, event_rx) = mpsc::channel();

        let pane_rows = rows.saturating_sub(4);
        let pane_cols = cols.saturating_sub(2);

        let pane = Pane::new(1, pane_rows, pane_cols, event_tx.clone())?;

        let mut panes = HashMap::new();
        panes.insert(1, pane);

        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));

        Ok(Self {
            panes,
            layout: LayoutNode::Leaf { pane_id: 1 },
            focused_pane_id: 1,
            should_quit: false,
            event_tx,
            event_rx,
            next_pane_id: 2,
            dirty: true,
            file_tree: FileTree::new(cwd),
            file_tree_visible: true,
            preview: Preview::new(),
            focus_target: FocusTarget::Pane,
            last_pane_rects: Vec::new(),
            last_file_tree_rect: None,
            last_preview_rect: None,
        })
    }

    /// Handle a key event. Returns true if the event was consumed by the app.
    pub fn handle_key_event(&mut self, key: KeyEvent) -> Result<bool> {
        // Ctrl+Q — always quit
        if key.modifiers == KeyModifiers::CONTROL && key.code == KeyCode::Char('q') {
            self.should_quit = true;
            return Ok(true);
        }

        // File tree mode keys (when file tree is focused)
        if self.focus_target == FocusTarget::FileTree {
            // Allow Ctrl+F to toggle tree off even in tree mode
            if key.modifiers == KeyModifiers::CONTROL && key.code == KeyCode::Char('f') {
                self.toggle_file_tree();
                return Ok(true);
            }
            return self.handle_file_tree_key(key);
        }

        // --- Pane mode below ---

        // Ctrl+F — toggle file tree (only when tree is hidden or pane-focused)
        if key.modifiers == KeyModifiers::CONTROL && key.code == KeyCode::Char('f') {
            self.toggle_file_tree();
            return Ok(true);
        }

        let multi_pane = self.layout.pane_count() > 1;

        match (key.modifiers, key.code) {
            // Ctrl+D — split vertical (always, since there's no other way to split)
            (KeyModifiers::CONTROL, KeyCode::Char('d')) => {
                self.split_focused_pane(SplitDirection::Vertical)?;
                Ok(true)
            }
            // Ctrl+E — split horizontal (always)
            (KeyModifiers::CONTROL, KeyCode::Char('e')) => {
                self.split_focused_pane(SplitDirection::Horizontal)?;
                Ok(true)
            }
            // Ctrl+W — close pane only when multi-pane, else forward to PTY
            (KeyModifiers::CONTROL, KeyCode::Char('w')) => {
                if multi_pane {
                    self.close_focused_pane();
                    Ok(true)
                } else {
                    Ok(false)
                }
            }
            // Tab — focus cycle only when multi-pane, else forward to PTY
            (KeyModifiers::NONE, KeyCode::Tab) => {
                if multi_pane {
                    self.focus_next();
                    Ok(true)
                } else {
                    Ok(false)
                }
            }
            // Shift+Tab — reverse focus cycle only when multi-pane
            (KeyModifiers::SHIFT, KeyCode::BackTab) => {
                if multi_pane {
                    self.focus_prev();
                    Ok(true)
                } else {
                    Ok(false)
                }
            }
            _ => Ok(false),
        }
    }

    /// Handle keys when file tree has focus.
    fn handle_file_tree_key(&mut self, key: KeyEvent) -> Result<bool> {
        match key.code {
            KeyCode::Char('j') | KeyCode::Down => {
                self.file_tree.move_down();
                Ok(true)
            }
            KeyCode::Char('k') | KeyCode::Up => {
                self.file_tree.move_up();
                Ok(true)
            }
            KeyCode::Enter => {
                if let Some(path) = self.file_tree.toggle_or_select() {
                    self.preview.load(&path);
                }
                Ok(true)
            }
            KeyCode::Esc => {
                // If preview is open, close it first; otherwise return to pane
                if self.preview.is_active() {
                    self.preview.close();
                } else {
                    self.focus_target = FocusTarget::Pane;
                }
                Ok(true)
            }
            _ => Ok(true), // Consume all keys in file tree mode
        }
    }

    /// Toggle file tree visibility and focus.
    fn toggle_file_tree(&mut self) {
        if self.file_tree_visible && self.focus_target == FocusTarget::FileTree {
            // Focused on tree → hide it and return to pane
            self.file_tree_visible = false;
            self.focus_target = FocusTarget::Pane;
            self.preview.close();
        } else if self.file_tree_visible {
            // Visible but pane focused → focus tree
            self.focus_target = FocusTarget::FileTree;
        } else {
            // Hidden → show and focus
            self.file_tree_visible = true;
            self.focus_target = FocusTarget::FileTree;
        }
    }

    const MAX_PANES: usize = 16;
    const MIN_PANE_WIDTH: u16 = 20;
    const MIN_PANE_HEIGHT: u16 = 5;

    /// Split the currently focused pane.
    fn split_focused_pane(&mut self, direction: SplitDirection) -> Result<()> {
        if self.layout.pane_count() >= Self::MAX_PANES {
            return Ok(());
        }

        // Check if the focused pane is large enough to split
        if let Some(&(_, rect)) = self
            .last_pane_rects
            .iter()
            .find(|(id, _)| *id == self.focused_pane_id)
        {
            match direction {
                SplitDirection::Vertical => {
                    if rect.width / 2 < Self::MIN_PANE_WIDTH {
                        return Ok(());
                    }
                }
                SplitDirection::Horizontal => {
                    if rect.height / 2 < Self::MIN_PANE_HEIGHT {
                        return Ok(());
                    }
                }
            }
        }

        let new_id = self.next_pane_id;
        self.next_pane_id = self.next_pane_id.wrapping_add(1);

        let pane = Pane::new(new_id, 10, 40, self.event_tx.clone())?;
        self.panes.insert(new_id, pane);

        self.layout.split_pane(self.focused_pane_id, new_id, direction);

        Ok(())
    }

    /// Close the currently focused pane (no-op if only one pane).
    fn close_focused_pane(&mut self) {
        if self.layout.pane_count() <= 1 {
            return;
        }

        let pane_ids = self.layout.collect_pane_ids();
        let current_idx = pane_ids.iter().position(|&id| id == self.focused_pane_id);

        self.layout.remove_pane(self.focused_pane_id);

        if let Some(mut pane) = self.panes.remove(&self.focused_pane_id) {
            pane.kill();
        }

        let remaining_ids = self.layout.collect_pane_ids();
        if let Some(idx) = current_idx {
            let new_idx = if idx >= remaining_ids.len() {
                remaining_ids.len().saturating_sub(1)
            } else {
                idx
            };
            self.focused_pane_id = remaining_ids[new_idx];
        } else if let Some(&first) = remaining_ids.first() {
            self.focused_pane_id = first;
        }
    }

    /// Focus the next pane in visual order.
    fn focus_next(&mut self) {
        let ids = self.layout.collect_pane_ids();
        if ids.len() <= 1 {
            return;
        }
        if let Some(idx) = ids.iter().position(|&id| id == self.focused_pane_id) {
            let next_idx = (idx + 1) % ids.len();
            self.focused_pane_id = ids[next_idx];
        }
    }

    /// Focus the previous pane in visual order.
    fn focus_prev(&mut self) {
        let ids = self.layout.collect_pane_ids();
        if ids.len() <= 1 {
            return;
        }
        if let Some(idx) = ids.iter().position(|&id| id == self.focused_pane_id) {
            let prev_idx = if idx == 0 { ids.len() - 1 } else { idx - 1 };
            self.focused_pane_id = ids[prev_idx];
        }
    }

    /// Handle mouse events.
    pub fn handle_mouse_event(&mut self, mouse: MouseEvent) {
        match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                let col = mouse.column;
                let row = mouse.row;

                // Check file tree click
                if let Some(rect) = self.last_file_tree_rect {
                    if col >= rect.x
                        && col < rect.x + rect.width
                        && row >= rect.y
                        && row < rect.y + rect.height
                    {
                        self.focus_target = FocusTarget::FileTree;
                        // Calculate which entry was clicked
                        let inner_y = row.saturating_sub(rect.y + 1); // -1 for border
                        let entry_idx =
                            self.file_tree.scroll_offset + inner_y as usize;
                        let entry_count = self.file_tree.visible_entries().len();
                        if entry_idx < entry_count {
                            self.file_tree.selected_index = entry_idx;
                            if let Some(path) = self.file_tree.toggle_or_select() {
                                self.preview.load(&path);
                            }
                        }
                        return;
                    }
                }

                // Check pane clicks
                for &(pane_id, rect) in &self.last_pane_rects {
                    if col >= rect.x
                        && col < rect.x + rect.width
                        && row >= rect.y
                        && row < rect.y + rect.height
                    {
                        self.focused_pane_id = pane_id;
                        self.focus_target = FocusTarget::Pane;
                        return;
                    }
                }
            }
            MouseEventKind::ScrollUp => {
                let col = mouse.column;
                let row = mouse.row;

                // Scroll in file tree
                if let Some(rect) = self.last_file_tree_rect {
                    if col >= rect.x
                        && col < rect.x + rect.width
                        && row >= rect.y
                        && row < rect.y + rect.height
                    {
                        self.file_tree.scroll_up(3);
                        return;
                    }
                }

                // Scroll in preview
                if let Some(rect) = self.last_preview_rect {
                    if col >= rect.x
                        && col < rect.x + rect.width
                        && row >= rect.y
                        && row < rect.y + rect.height
                    {
                        self.preview.scroll_up(3);
                        return;
                    }
                }
            }
            MouseEventKind::ScrollDown => {
                let col = mouse.column;
                let row = mouse.row;

                // Scroll in file tree
                if let Some(rect) = self.last_file_tree_rect {
                    if col >= rect.x
                        && col < rect.x + rect.width
                        && row >= rect.y
                        && row < rect.y + rect.height
                    {
                        self.file_tree.scroll_down(3);
                        return;
                    }
                }

                // Scroll in preview
                if let Some(rect) = self.last_preview_rect {
                    if col >= rect.x
                        && col < rect.x + rect.width
                        && row >= rect.y
                        && row < rect.y + rect.height
                    {
                        self.preview.scroll_down(3);
                        return;
                    }
                }
            }
            _ => {}
        }
    }

    /// Forward raw key input to the focused pane's PTY.
    pub fn forward_key_to_pty(&mut self, key: KeyEvent) -> Result<()> {
        if let Some(pane) = self.panes.get_mut(&self.focused_pane_id) {
            if let Some(bytes) = key_event_to_bytes(&key) {
                pane.write_input(&bytes)?;
            }
        }
        Ok(())
    }

    /// Drain PTY output events. Returns true if any events were received.
    pub fn drain_pty_events(&mut self) -> bool {
        let mut had_events = false;
        while let Ok(event) = self.event_rx.try_recv() {
            had_events = true;
            if let AppEvent::PtyEof(pane_id) = event {
                if let Some(pane) = self.panes.get_mut(&pane_id) {
                    pane.exited = true;
                }
            }
        }
        if had_events {
            self.dirty = true;
        }
        had_events
    }

    /// Clean shutdown: kill all PTY processes.
    pub fn shutdown(&mut self) {
        for pane in self.panes.values_mut() {
            pane.kill();
        }
    }
}

/// Convert a crossterm KeyEvent into bytes suitable for PTY input.
fn key_event_to_bytes(key: &KeyEvent) -> Option<Vec<u8>> {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);

    match key.code {
        KeyCode::Char(c) => {
            if ctrl {
                let ctrl_byte = (c.to_ascii_lowercase() as u8).wrapping_sub(b'a').wrapping_add(1);
                if ctrl_byte <= 26 {
                    Some(vec![ctrl_byte])
                } else {
                    Some(c.to_string().into_bytes())
                }
            } else {
                Some(c.to_string().into_bytes())
            }
        }
        KeyCode::Enter => Some(vec![b'\r']),
        KeyCode::Backspace => Some(vec![0x7f]),
        KeyCode::Delete => Some(b"\x1b[3~".to_vec()),
        KeyCode::Tab => Some(vec![b'\t']),
        KeyCode::Esc => Some(vec![0x1b]),
        KeyCode::Up => Some(b"\x1b[A".to_vec()),
        KeyCode::Down => Some(b"\x1b[B".to_vec()),
        KeyCode::Right => Some(b"\x1b[C".to_vec()),
        KeyCode::Left => Some(b"\x1b[D".to_vec()),
        KeyCode::Home => Some(b"\x1b[H".to_vec()),
        KeyCode::End => Some(b"\x1b[F".to_vec()),
        KeyCode::PageUp => Some(b"\x1b[5~".to_vec()),
        KeyCode::PageDown => Some(b"\x1b[6~".to_vec()),
        KeyCode::Insert => Some(b"\x1b[2~".to_vec()),
        KeyCode::F(n) => {
            let seq = match n {
                1 => "\x1bOP",
                2 => "\x1bOQ",
                3 => "\x1bOR",
                4 => "\x1bOS",
                5 => "\x1b[15~",
                6 => "\x1b[17~",
                7 => "\x1b[18~",
                8 => "\x1b[19~",
                9 => "\x1b[20~",
                10 => "\x1b[21~",
                11 => "\x1b[23~",
                12 => "\x1b[24~",
                _ => return None,
            };
            Some(seq.as_bytes().to_vec())
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_layout_single_pane() {
        let layout = LayoutNode::Leaf { pane_id: 1 };
        assert_eq!(layout.pane_count(), 1);
        assert_eq!(layout.collect_pane_ids(), vec![1]);
    }

    #[test]
    fn test_layout_split_vertical() {
        let mut layout = LayoutNode::Leaf { pane_id: 1 };
        layout.split_pane(1, 2, SplitDirection::Vertical);
        assert_eq!(layout.pane_count(), 2);
        assert_eq!(layout.collect_pane_ids(), vec![1, 2]);
    }

    #[test]
    fn test_layout_split_horizontal() {
        let mut layout = LayoutNode::Leaf { pane_id: 1 };
        layout.split_pane(1, 2, SplitDirection::Horizontal);
        assert_eq!(layout.pane_count(), 2);
        assert_eq!(layout.collect_pane_ids(), vec![1, 2]);
    }

    #[test]
    fn test_layout_nested_split() {
        let mut layout = LayoutNode::Leaf { pane_id: 1 };
        layout.split_pane(1, 2, SplitDirection::Vertical);
        layout.split_pane(1, 3, SplitDirection::Horizontal);
        assert_eq!(layout.pane_count(), 3);
        assert_eq!(layout.collect_pane_ids(), vec![1, 3, 2]);
    }

    #[test]
    fn test_layout_remove_pane() {
        let mut layout = LayoutNode::Leaf { pane_id: 1 };
        layout.split_pane(1, 2, SplitDirection::Vertical);
        assert_eq!(layout.pane_count(), 2);

        layout.remove_pane(2);
        assert_eq!(layout.pane_count(), 1);
        assert_eq!(layout.collect_pane_ids(), vec![1]);
    }

    #[test]
    fn test_layout_remove_first_pane() {
        let mut layout = LayoutNode::Leaf { pane_id: 1 };
        layout.split_pane(1, 2, SplitDirection::Vertical);

        layout.remove_pane(1);
        assert_eq!(layout.pane_count(), 1);
        assert_eq!(layout.collect_pane_ids(), vec![2]);
    }

    #[test]
    fn test_calculate_rects_vertical() {
        let layout = LayoutNode::Split {
            direction: SplitDirection::Vertical,
            first: Box::new(LayoutNode::Leaf { pane_id: 1 }),
            second: Box::new(LayoutNode::Leaf { pane_id: 2 }),
        };
        let area = Rect::new(0, 0, 100, 50);
        let rects = layout.calculate_rects(area);
        assert_eq!(rects.len(), 2);
        assert_eq!(rects[0], (1, Rect::new(0, 0, 50, 50)));
        assert_eq!(rects[1], (2, Rect::new(50, 0, 50, 50)));
    }

    #[test]
    fn test_calculate_rects_horizontal() {
        let layout = LayoutNode::Split {
            direction: SplitDirection::Horizontal,
            first: Box::new(LayoutNode::Leaf { pane_id: 1 }),
            second: Box::new(LayoutNode::Leaf { pane_id: 2 }),
        };
        let area = Rect::new(0, 0, 100, 50);
        let rects = layout.calculate_rects(area);
        assert_eq!(rects.len(), 2);
        assert_eq!(rects[0], (1, Rect::new(0, 0, 100, 25)));
        assert_eq!(rects[1], (2, Rect::new(0, 25, 100, 25)));
    }

    #[test]
    fn test_focus_cycling() {
        let ids = vec![1, 2, 3];
        let current = 0;
        let next = (current + 1) % ids.len();
        assert_eq!(next, 1);

        let current = 2;
        let next = (current + 1) % ids.len();
        assert_eq!(next, 0);
    }
}
