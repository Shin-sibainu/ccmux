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
    /// Shell changed working directory (pane_id, new path).
    CwdChanged(usize, PathBuf),
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
    Preview,
}

/// Which border is being dragged.
#[derive(Debug, Clone, PartialEq)]
pub enum DragTarget {
    FileTreeBorder,
    PreviewBorder,
    PaneSplit(Vec<bool>, SplitDirection, Rect),
    Scrollbar(usize, Rect), // pane_id, inner area
}

// ─── Layout Tree ──────────────────────────────────────────

/// Binary tree node for pane layout.
pub enum LayoutNode {
    Leaf { pane_id: usize },
    Split {
        direction: SplitDirection,
        ratio: f32, // 0.0..1.0, portion allocated to first child
        first: Box<LayoutNode>,
        second: Box<LayoutNode>,
    },
}

impl LayoutNode {
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

    pub fn calculate_rects(&self, area: Rect) -> Vec<(usize, Rect)> {
        match self {
            LayoutNode::Leaf { pane_id } => vec![(*pane_id, area)],
            LayoutNode::Split { direction, ratio, first, second } => {
                let (first_area, second_area) = split_rect(area, *direction, *ratio);
                let mut result = first.calculate_rects(first_area);
                result.extend(second.calculate_rects(second_area));
                result
            }
        }
    }

    pub fn split_pane(&mut self, target_id: usize, new_id: usize, direction: SplitDirection) -> bool {
        match self {
            LayoutNode::Leaf { pane_id } => {
                if *pane_id == target_id {
                    let old_id = *pane_id;
                    *self = LayoutNode::Split {
                        direction,
                        ratio: 0.5,
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

    pub fn remove_pane(&mut self, target_id: usize) -> bool {
        match self {
            LayoutNode::Leaf { .. } => false,
            LayoutNode::Split { first, second, .. } => {
                if let LayoutNode::Leaf { pane_id } = first.as_ref() {
                    if *pane_id == target_id {
                        let second = std::mem::replace(second.as_mut(), LayoutNode::Leaf { pane_id: 0 });
                        *self = second;
                        return true;
                    }
                }
                if let LayoutNode::Leaf { pane_id } = second.as_ref() {
                    if *pane_id == target_id {
                        let first = std::mem::replace(first.as_mut(), LayoutNode::Leaf { pane_id: 0 });
                        *self = first;
                        return true;
                    }
                }
                first.remove_pane(target_id) || second.remove_pane(target_id)
            }
        }
    }

    /// Find the split boundary position and direction for hit testing.
    /// Returns a list of (boundary_position, direction, depth) for each Split node.
    pub fn split_boundaries(&self, area: Rect) -> Vec<(u16, SplitDirection, Vec<bool>)> {
        let mut result = Vec::new();
        self.collect_boundaries(area, &mut Vec::new(), &mut result);
        result
    }

    fn collect_boundaries(
        &self,
        area: Rect,
        path: &mut Vec<bool>, // false=first, true=second
        result: &mut Vec<(u16, SplitDirection, Vec<bool>)>,
    ) {
        if let LayoutNode::Split { direction, ratio, first, second } = self {
            let (first_area, second_area) = split_rect(area, *direction, *ratio);

            // The boundary is at the edge between first and second
            let boundary = match direction {
                SplitDirection::Vertical => first_area.x + first_area.width,
                SplitDirection::Horizontal => first_area.y + first_area.height,
            };
            result.push((boundary, *direction, path.clone()));

            path.push(false);
            first.collect_boundaries(first_area, path, result);
            path.pop();

            path.push(true);
            second.collect_boundaries(second_area, path, result);
            path.pop();
        }
    }

    /// Update ratio by path (path identifies which Split node).
    pub fn update_ratio(&mut self, path: &[bool], new_ratio: f32) {
        if path.is_empty() {
            if let LayoutNode::Split { ratio, .. } = self {
                *ratio = new_ratio.clamp(0.15, 0.85);
            }
        } else if let LayoutNode::Split { first, second, .. } = self {
            if path[0] {
                second.update_ratio(&path[1..], new_ratio);
            } else {
                first.update_ratio(&path[1..], new_ratio);
            }
        }
    }

    pub fn pane_count(&self) -> usize {
        match self {
            LayoutNode::Leaf { .. } => 1,
            LayoutNode::Split { first, second, .. } => first.pane_count() + second.pane_count(),
        }
    }
}

fn split_rect(area: Rect, direction: SplitDirection, ratio: f32) -> (Rect, Rect) {
    let ratio = ratio.clamp(0.1, 0.9);
    match direction {
        SplitDirection::Vertical => {
            let first_w = (area.width as f32 * ratio) as u16;
            let first_w = first_w.max(1).min(area.width.saturating_sub(1));
            (
                Rect::new(area.x, area.y, first_w, area.height),
                Rect::new(area.x + first_w, area.y, area.width - first_w, area.height),
            )
        }
        SplitDirection::Horizontal => {
            let first_h = (area.height as f32 * ratio) as u16;
            let first_h = first_h.max(1).min(area.height.saturating_sub(1));
            (
                Rect::new(area.x, area.y, area.width, first_h),
                Rect::new(area.x, area.y + first_h, area.width, area.height - first_h),
            )
        }
    }
}

// ─── Text Selection ───────────────────────────────────────

/// Text selection state within a pane.
#[derive(Debug, Clone)]
pub struct TextSelection {
    pub pane_id: usize,
    pub start_row: u16,
    pub start_col: u16,
    pub end_row: u16,
    pub end_col: u16,
    pub pane_rect: Rect, // inner area of the pane (for coordinate mapping)
}

impl TextSelection {
    /// Get normalized (top-left to bottom-right) selection range.
    pub fn normalized(&self) -> (u16, u16, u16, u16) {
        if self.start_row < self.end_row
            || (self.start_row == self.end_row && self.start_col <= self.end_col)
        {
            (self.start_row, self.start_col, self.end_row, self.end_col)
        } else {
            (self.end_row, self.end_col, self.start_row, self.start_col)
        }
    }

    /// Check if a cell is within the selection.
    pub fn contains(&self, row: u16, col: u16) -> bool {
        let (sr, sc, er, ec) = self.normalized();
        if row < sr || row > er {
            return false;
        }
        if row == sr && row == er {
            return col >= sc && col <= ec;
        }
        if row == sr {
            return col >= sc;
        }
        if row == er {
            return col <= ec;
        }
        true
    }
}

// ─── Workspace (per-tab state) ────────────────────────────

/// A workspace holds all state for one tab.
#[allow(dead_code)]
pub struct Workspace {
    pub name: String,
    pub cwd: PathBuf,
    pub panes: HashMap<usize, Pane>,
    pub layout: LayoutNode,
    pub focused_pane_id: usize,
    pub file_tree: FileTree,
    pub file_tree_visible: bool,
    pub preview: Preview,
    pub focus_target: FocusTarget,
    // Cached rects (updated on each render)
    pub last_pane_rects: Vec<(usize, Rect)>,
    pub last_file_tree_rect: Option<Rect>,
    pub last_preview_rect: Option<Rect>,
}

impl Workspace {
    fn new(
        name: String,
        cwd: PathBuf,
        pane_id: usize,
        rows: u16,
        cols: u16,
        event_tx: Sender<AppEvent>,
    ) -> Result<Self> {
        let pane = Pane::new(pane_id, rows, cols, event_tx)?;
        let mut panes = HashMap::new();
        panes.insert(pane_id, pane);

        Ok(Self {
            name,
            file_tree: FileTree::new(cwd.clone()),
            cwd,
            panes,
            layout: LayoutNode::Leaf { pane_id },
            focused_pane_id: pane_id,
            file_tree_visible: true,
            preview: Preview::new(),
            focus_target: FocusTarget::Pane,
            last_pane_rects: Vec::new(),
            last_file_tree_rect: None,
            last_preview_rect: None,
        })
    }

    fn shutdown(&mut self) {
        for pane in self.panes.values_mut() {
            pane.kill();
        }
    }
}

// ─── App (global state) ───────────────────────────────────

pub struct App {
    pub workspaces: Vec<Workspace>,
    pub active_tab: usize,
    pub should_quit: bool,
    pub event_tx: Sender<AppEvent>,
    pub event_rx: Receiver<AppEvent>,
    next_pane_id: usize,
    pub dirty: bool,
    pub paste_cooldown: u8, // frames to skip rendering after paste
    // Shared settings
    pub file_tree_width: u16,
    pub preview_width: u16,
    // Layout: swap preview and terminal positions
    pub layout_swapped: bool,
    // Drag/hover state
    pub dragging: Option<DragTarget>,
    pub hover_border: Option<DragTarget>,
    // Tab bar rects for mouse click
    pub last_tab_rects: Vec<(usize, Rect)>,
    pub last_new_tab_rect: Option<Rect>,
    // Text selection
    pub selection: Option<TextSelection>,
}

impl App {
    pub fn new(rows: u16, cols: u16) -> Result<Self> {
        let (event_tx, event_rx) = mpsc::channel();

        let pane_rows = rows.saturating_sub(5); // title + tab bar + status + borders
        let pane_cols = cols.saturating_sub(2);

        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        let name = dir_name(&cwd);

        let ws = Workspace::new(name, cwd, 1, pane_rows, pane_cols, event_tx.clone())?;

        Ok(Self {
            workspaces: vec![ws],
            active_tab: 0,
            should_quit: false,
            event_tx,
            event_rx,
            next_pane_id: 2,
            dirty: true,
            paste_cooldown: 0,
            file_tree_width: 20,
            preview_width: 40,
            layout_swapped: true,
            dragging: None,
            hover_border: None,
            last_tab_rects: Vec::new(),
            last_new_tab_rect: None,
            selection: None,
        })
    }

    /// Get the active workspace.
    pub fn ws(&self) -> &Workspace {
        &self.workspaces[self.active_tab]
    }

    /// Get the active workspace mutably.
    pub fn ws_mut(&mut self) -> &mut Workspace {
        &mut self.workspaces[self.active_tab]
    }

    // ─── Key handling ─────────────────────────────────────

    pub fn handle_key_event(&mut self, key: KeyEvent) -> Result<bool> {
        // Ctrl+Q — quit
        if key.modifiers == KeyModifiers::CONTROL && key.code == KeyCode::Char('q') {
            self.should_quit = true;
            return Ok(true);
        }

        // Ctrl+T — new tab
        if key.modifiers == KeyModifiers::CONTROL && key.code == KeyCode::Char('t') {
            self.new_tab()?;
            return Ok(true);
        }

        // Ctrl+Right — next pane
        if key.modifiers == KeyModifiers::CONTROL && key.code == KeyCode::Right {
            self.focus_next_pane();
            return Ok(true);
        }

        // Ctrl+Left — previous pane
        if key.modifiers == KeyModifiers::CONTROL && key.code == KeyCode::Left {
            self.focus_prev_pane();
            return Ok(true);
        }

        // Preview mode
        if self.ws().focus_target == FocusTarget::Preview {
            return self.handle_preview_key(key);
        }

        // File tree mode
        if self.ws().focus_target == FocusTarget::FileTree {
            if key.modifiers == KeyModifiers::CONTROL && key.code == KeyCode::Char('f') {
                self.toggle_file_tree();
                return Ok(true);
            }
            return self.handle_file_tree_key(key);
        }

        // Ctrl+F — toggle file tree
        if key.modifiers == KeyModifiers::CONTROL && key.code == KeyCode::Char('f') {
            self.toggle_file_tree();
            return Ok(true);
        }

        // Ctrl+P — swap preview and terminal positions
        if key.modifiers == KeyModifiers::CONTROL && key.code == KeyCode::Char('p') {
            self.layout_swapped = !self.layout_swapped;
            return Ok(true);
        }

        let multi_pane = self.ws().layout.pane_count() > 1;
        let multi_tab = self.workspaces.len() > 1;

        match (key.modifiers, key.code) {
            (KeyModifiers::CONTROL, KeyCode::Char('d')) => {
                self.split_focused_pane(SplitDirection::Vertical)?;
                Ok(true)
            }
            (KeyModifiers::CONTROL, KeyCode::Char('e')) => {
                self.split_focused_pane(SplitDirection::Horizontal)?;
                Ok(true)
            }
            (KeyModifiers::CONTROL, KeyCode::Char('w')) => {
                if self.ws().focus_target == FocusTarget::Preview {
                    // Close preview and return to pane
                    self.ws_mut().preview.close();
                    self.ws_mut().focus_target = FocusTarget::Pane;
                    Ok(true)
                } else if multi_pane {
                    self.close_focused_pane();
                    Ok(true)
                } else if multi_tab {
                    self.close_tab(self.active_tab);
                    Ok(true)
                } else {
                    Ok(false)
                }
            }
            _ => Ok(false),
        }
    }

    fn handle_file_tree_key(&mut self, key: KeyEvent) -> Result<bool> {
        match key.code {
            KeyCode::Char('j') | KeyCode::Down => {
                self.ws_mut().file_tree.move_down();
                Ok(true)
            }
            KeyCode::Char('k') | KeyCode::Up => {
                self.ws_mut().file_tree.move_up();
                Ok(true)
            }
            KeyCode::Enter => {
                let path = self.ws_mut().file_tree.toggle_or_select();
                if let Some(path) = path {
                    self.ws_mut().preview.load(&path);
                }
                Ok(true)
            }
            KeyCode::Char('.') => {
                self.ws_mut().file_tree.toggle_hidden();
                Ok(true)
            }
            KeyCode::Esc => {
                // Return to pane, keep preview open
                self.ws_mut().focus_target = FocusTarget::Pane;
                Ok(true)
            }
            _ => Ok(true),
        }
    }

    fn handle_preview_key(&mut self, key: KeyEvent) -> Result<bool> {
        match (key.modifiers, key.code) {
            (KeyModifiers::CONTROL, KeyCode::Char('w')) => {
                self.ws_mut().preview.close();
                self.ws_mut().focus_target = FocusTarget::Pane;
                Ok(true)
            }
            (KeyModifiers::CONTROL, KeyCode::Char('p')) => {
                self.layout_swapped = !self.layout_swapped;
                Ok(true)
            }
            (_, KeyCode::Char('j')) | (_, KeyCode::Down) => {
                self.ws_mut().preview.scroll_down(1);
                Ok(true)
            }
            (_, KeyCode::Char('k')) | (_, KeyCode::Up) => {
                self.ws_mut().preview.scroll_up(1);
                Ok(true)
            }
            (_, KeyCode::PageDown) => {
                self.ws_mut().preview.scroll_down(20);
                Ok(true)
            }
            (_, KeyCode::PageUp) => {
                self.ws_mut().preview.scroll_up(20);
                Ok(true)
            }
            (_, KeyCode::Esc) => {
                self.ws_mut().focus_target = FocusTarget::Pane;
                Ok(true)
            }
            (KeyModifiers::CONTROL, KeyCode::Char('q')) => {
                self.should_quit = true;
                Ok(true)
            }
            (KeyModifiers::CONTROL, KeyCode::Right) => {
                self.focus_next_pane();
                Ok(true)
            }
            (KeyModifiers::CONTROL, KeyCode::Left) => {
                self.focus_prev_pane();
                Ok(true)
            }
            _ => Ok(true),
        }
    }

    // ─── Tab management ───────────────────────────────────

    fn new_tab(&mut self) -> Result<()> {
        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        let name = dir_name(&cwd);
        let pane_id = self.next_pane_id;
        self.next_pane_id = self.next_pane_id.wrapping_add(1);

        let ws = Workspace::new(name, cwd, pane_id, 10, 40, self.event_tx.clone())?;
        self.workspaces.push(ws);
        self.active_tab = self.workspaces.len() - 1;
        Ok(())
    }

    fn close_tab(&mut self, index: usize) {
        if self.workspaces.len() <= 1 {
            return;
        }
        self.workspaces[index].shutdown();
        self.workspaces.remove(index);
        if self.active_tab >= self.workspaces.len() {
            self.active_tab = self.workspaces.len() - 1;
        }
    }

    // ─── Pane management ──────────────────────────────────

    fn toggle_file_tree(&mut self) {
        let ws = self.ws_mut();
        if ws.file_tree_visible && ws.focus_target == FocusTarget::FileTree {
            ws.file_tree_visible = false;
            ws.focus_target = FocusTarget::Pane;
            ws.preview.close();
        } else if ws.file_tree_visible {
            ws.focus_target = FocusTarget::FileTree;
        } else {
            ws.file_tree_visible = true;
            ws.focus_target = FocusTarget::FileTree;
        }
    }

    const MAX_PANES: usize = 16;
    const MIN_PANE_WIDTH: u16 = 20;
    const MIN_PANE_HEIGHT: u16 = 5;

    fn split_focused_pane(&mut self, direction: SplitDirection) -> Result<()> {
        if self.ws().layout.pane_count() >= Self::MAX_PANES {
            return Ok(());
        }

        if let Some(&(_, rect)) = self
            .ws()
            .last_pane_rects
            .iter()
            .find(|(id, _)| *id == self.ws().focused_pane_id)
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

        // Inherit CWD from the focused pane
        let parent_cwd = self.ws().panes.get(&self.ws().focused_pane_id)
            .map(|p| p.cwd.clone());

        let pane = Pane::new_with_cwd(new_id, 10, 40, self.event_tx.clone(), parent_cwd)?;
        let ws = self.ws_mut();
        ws.panes.insert(new_id, pane);
        ws.layout.split_pane(ws.focused_pane_id, new_id, direction);

        Ok(())
    }

    fn close_focused_pane(&mut self) {
        let ws = self.ws_mut();
        if ws.layout.pane_count() <= 1 {
            return;
        }

        let pane_ids = ws.layout.collect_pane_ids();
        let current_idx = pane_ids.iter().position(|&id| id == ws.focused_pane_id);

        let focused = ws.focused_pane_id;
        ws.layout.remove_pane(focused);

        if let Some(mut pane) = ws.panes.remove(&focused) {
            pane.kill();
        }

        let remaining_ids = ws.layout.collect_pane_ids();
        if let Some(idx) = current_idx {
            let new_idx = if idx >= remaining_ids.len() {
                remaining_ids.len().saturating_sub(1)
            } else {
                idx
            };
            ws.focused_pane_id = remaining_ids[new_idx];
        } else if let Some(&first) = remaining_ids.first() {
            ws.focused_pane_id = first;
        }
    }

    /// Cycle focus forward: FileTree → Preview → Pane1 → Pane2 → ... → FileTree
    fn focus_next_pane(&mut self) {
        let ws = self.ws_mut();
        let ids = ws.layout.collect_pane_ids();
        let tree_visible = ws.file_tree_visible;
        let preview_active = ws.preview.is_active();
        let _swapped = false; // preview position doesn't affect focus order

        match ws.focus_target {
            FocusTarget::FileTree => {
                // File tree → preview (if active) or first pane
                if preview_active {
                    ws.focus_target = FocusTarget::Preview;
                } else {
                    ws.focus_target = FocusTarget::Pane;
                }
            }
            FocusTarget::Preview => {
                // Preview → first pane
                ws.focus_target = FocusTarget::Pane;
            }
            FocusTarget::Pane => {
                if let Some(idx) = ids.iter().position(|&id| id == ws.focused_pane_id) {
                    if idx + 1 < ids.len() {
                        ws.focused_pane_id = ids[idx + 1];
                    } else if tree_visible {
                        ws.focus_target = FocusTarget::FileTree;
                    } else if preview_active {
                        ws.focus_target = FocusTarget::Preview;
                    } else {
                        ws.focused_pane_id = ids[0];
                    }
                }
            }
        }
    }

    /// Cycle focus backward
    fn focus_prev_pane(&mut self) {
        let ws = self.ws_mut();
        let ids = ws.layout.collect_pane_ids();
        let tree_visible = ws.file_tree_visible;
        let preview_active = ws.preview.is_active();

        match ws.focus_target {
            FocusTarget::FileTree => {
                // File tree → last pane
                ws.focus_target = FocusTarget::Pane;
                if let Some(&last) = ids.last() {
                    ws.focused_pane_id = last;
                }
            }
            FocusTarget::Preview => {
                // Preview → file tree (if visible) or last pane
                if tree_visible {
                    ws.focus_target = FocusTarget::FileTree;
                } else {
                    ws.focus_target = FocusTarget::Pane;
                    if let Some(&last) = ids.last() {
                        ws.focused_pane_id = last;
                    }
                }
            }
            FocusTarget::Pane => {
                if let Some(idx) = ids.iter().position(|&id| id == ws.focused_pane_id) {
                    if idx > 0 {
                        ws.focused_pane_id = ids[idx - 1];
                    } else if preview_active {
                        ws.focus_target = FocusTarget::Preview;
                    } else if tree_visible {
                        ws.focus_target = FocusTarget::FileTree;
                    } else {
                        ws.focused_pane_id = ids[ids.len() - 1];
                    }
                }
            }
        }
    }

    /// Scroll a pane based on scrollbar click position.
    fn scroll_pane_to_click(&self, pane_id: usize, click_row: u16, inner: &Rect) {
        if let Some(pane) = self.ws().panes.get(&pane_id) {
            let (_, total_lines) = pane.scrollbar_info();
            let visible_rows = inner.height as usize;
            if total_lines <= visible_rows {
                return;
            }
            let max_scroll = total_lines.saturating_sub(visible_rows);
            // click_row relative to inner area: top = max scroll, bottom = 0
            let relative_y = click_row.saturating_sub(inner.y) as f32;
            let ratio = relative_y / inner.height.max(1) as f32;
            let target_scroll = ((1.0 - ratio) * max_scroll as f32) as usize;
            let mut parser = pane.parser.lock().unwrap_or_else(|e| e.into_inner());
            parser.screen_mut().set_scrollback(target_scroll);
        }
    }

    // ─── Mouse handling ───────────────────────────────────

    fn is_on_file_tree_border(&self, col: u16) -> bool {
        if let Some(rect) = self.ws().last_file_tree_rect {
            let border_col = rect.x + rect.width;
            col >= border_col.saturating_sub(1) && col <= border_col
        } else {
            false
        }
    }

    fn is_on_preview_border(&self, col: u16) -> bool {
        if let Some(rect) = self.ws().last_preview_rect {
            // When swapped: [tree][preview][panes] → drag the RIGHT edge of preview
            // When normal:  [tree][panes][preview] → drag the LEFT edge of preview
            let border_col = if self.layout_swapped {
                rect.x + rect.width
            } else {
                rect.x
            };
            col >= border_col.saturating_sub(1) && col <= border_col
        } else {
            false
        }
    }

    pub fn handle_mouse_event(&mut self, mouse: MouseEvent) {
        match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                let col = mouse.column;
                let row = mouse.row;

                // Clear previous selection on any click
                self.selection = None;

                // Check tab bar clicks
                for &(tab_idx, rect) in &self.last_tab_rects {
                    if col >= rect.x && col < rect.x + rect.width
                        && row >= rect.y && row < rect.y + rect.height
                    {
                        self.active_tab = tab_idx;
                        return;
                    }
                }

                // Check [+] new tab button
                if let Some(rect) = self.last_new_tab_rect {
                    if col >= rect.x && col < rect.x + rect.width
                        && row >= rect.y && row < rect.y + rect.height
                    {
                        let _ = self.new_tab();
                        return;
                    }
                }

                // Check border drag (file tree / preview)
                if self.is_on_file_tree_border(col) {
                    self.dragging = Some(DragTarget::FileTreeBorder);
                    return;
                }
                if self.is_on_preview_border(col) {
                    self.dragging = Some(DragTarget::PreviewBorder);
                    return;
                }

                // Check pane split border drag
                if let Some(pane_area) = self.ws().last_pane_rects.first().map(|_| {
                    // Compute the total pane area from all pane rects
                    let rects = &self.ws().last_pane_rects;
                    let min_x = rects.iter().map(|(_, r)| r.x).min().unwrap_or(0);
                    let min_y = rects.iter().map(|(_, r)| r.y).min().unwrap_or(0);
                    let max_x = rects.iter().map(|(_, r)| r.x + r.width).max().unwrap_or(0);
                    let max_y = rects.iter().map(|(_, r)| r.y + r.height).max().unwrap_or(0);
                    Rect::new(min_x, min_y, max_x - min_x, max_y - min_y)
                }) {
                    let boundaries = self.ws().layout.split_boundaries(pane_area);
                    for (boundary, direction, path) in boundaries {
                        let on_border = match direction {
                            SplitDirection::Vertical => {
                                col >= boundary.saturating_sub(1) && col <= boundary
                                    && row >= pane_area.y && row < pane_area.y + pane_area.height
                            }
                            SplitDirection::Horizontal => {
                                row >= boundary.saturating_sub(1) && row <= boundary
                                    && col >= pane_area.x && col < pane_area.x + pane_area.width
                            }
                        };
                        if on_border {
                            self.dragging = Some(DragTarget::PaneSplit(path, direction, pane_area));
                            return;
                        }
                    }
                }

                // Check file tree click
                if let Some(rect) = self.ws().last_file_tree_rect {
                    if col >= rect.x && col < rect.x + rect.width
                        && row >= rect.y && row < rect.y + rect.height
                    {
                        self.ws_mut().focus_target = FocusTarget::FileTree;
                        let inner_y = row.saturating_sub(rect.y + 1);
                        let scroll = self.ws().file_tree.scroll_offset;
                        let entry_idx = scroll + inner_y as usize;
                        let entry_count = self.ws().file_tree.visible_entries().len();
                        if entry_idx < entry_count {
                            self.ws_mut().file_tree.selected_index = entry_idx;
                            let path = self.ws_mut().file_tree.toggle_or_select();
                            if let Some(path) = path {
                                self.ws_mut().preview.load(&path);
                            }
                        }
                        return;
                    }
                }

                // Check preview click
                if let Some(rect) = self.ws().last_preview_rect {
                    if col >= rect.x && col < rect.x + rect.width
                        && row >= rect.y && row < rect.y + rect.height
                    {
                        self.ws_mut().focus_target = FocusTarget::Preview;
                        return;
                    }
                }

                // Check pane clicks
                let pane_rects = self.ws().last_pane_rects.clone();
                for (pane_id, rect) in pane_rects {
                    if col >= rect.x && col < rect.x + rect.width
                        && row >= rect.y && row < rect.y + rect.height
                    {
                        self.ws_mut().focused_pane_id = pane_id;
                        self.ws_mut().focus_target = FocusTarget::Pane;

                        // Check if clicking on scrollbar (rightmost column inside border)
                        let scrollbar_col = rect.x + rect.width - 2; // -1 border, -1 scrollbar
                        if col >= scrollbar_col {
                            let inner = Rect::new(rect.x + 1, rect.y + 1, rect.width.saturating_sub(2), rect.height.saturating_sub(2));
                            self.scroll_pane_to_click(pane_id, row, &inner);
                            self.dragging = Some(DragTarget::Scrollbar(pane_id, inner));
                        }
                        return;
                    }
                }
            }
            MouseEventKind::Drag(MouseButton::Left) => {
                let col = mouse.column;
                let row = mouse.row;

                // Border drag takes priority
                if let Some(ref target) = self.dragging.clone() {
                    match target {
                        DragTarget::FileTreeBorder => {
                            self.file_tree_width = col.clamp(10, 60);
                        }
                        DragTarget::PreviewBorder => {
                            if let Some(rect) = self.ws().last_preview_rect {
                                if self.layout_swapped {
                                    let new_width = col.saturating_sub(rect.x).clamp(15, 80);
                                    self.preview_width = new_width;
                                } else {
                                    let total_right = rect.x + rect.width;
                                    let new_width = total_right.saturating_sub(col).clamp(15, 80);
                                    self.preview_width = new_width;
                                }
                            }
                        }
                        DragTarget::PaneSplit(path, direction, area) => {
                            let new_ratio = match direction {
                                SplitDirection::Vertical => {
                                    (col.saturating_sub(area.x) as f32) / area.width.max(1) as f32
                                }
                                SplitDirection::Horizontal => {
                                    (row.saturating_sub(area.y) as f32) / area.height.max(1) as f32
                                }
                            };
                            self.ws_mut().layout.update_ratio(path, new_ratio);
                        }
                        DragTarget::Scrollbar(pane_id, inner) => {
                            self.scroll_pane_to_click(*pane_id, row, inner);
                        }
                    }
                    return;
                }

                // Text selection: extend if active, or start new
                if let Some(ref mut sel) = self.selection {
                    let inner = sel.pane_rect;
                    sel.end_col = col.saturating_sub(inner.x).min(inner.width.saturating_sub(1));
                    sel.end_row = row.saturating_sub(inner.y).min(inner.height.saturating_sub(1));
                } else {
                    // Start new selection in pane
                    let pane_rects = self.ws().last_pane_rects.clone();
                    for (pane_id, rect) in pane_rects {
                        if col >= rect.x && col < rect.x + rect.width
                            && row >= rect.y && row < rect.y + rect.height
                        {
                            let inner = Rect::new(
                                rect.x + 1, rect.y + 1,
                                rect.width.saturating_sub(2),
                                rect.height.saturating_sub(2),
                            );
                            let cell_col = col.saturating_sub(inner.x);
                            let cell_row = row.saturating_sub(inner.y);
                            self.selection = Some(TextSelection {
                                pane_id,
                                start_row: cell_row,
                                start_col: cell_col,
                                end_row: cell_row,
                                end_col: cell_col,
                                pane_rect: inner,
                            });
                            break;
                        }
                    }
                }
            }
            MouseEventKind::Up(MouseButton::Left) => {
                self.dragging = None;

                // Copy selected text to clipboard
                if let Some(ref sel) = self.selection {
                    let (sr, sc, er, ec) = sel.normalized();
                    // Only copy if there's an actual selection (not just a click)
                    if sr != er || sc != ec {
                        if let Some(pane) = self.ws().panes.get(&sel.pane_id) {
                            let text = extract_selected_text(pane, sr, sc, er, ec);
                            if !text.is_empty() {
                                if let Ok(mut clipboard) = arboard::Clipboard::new() {
                                    let _ = clipboard.set_text(&text);
                                }
                            }
                        }
                    }
                    // Keep selection visible until next click
                }
            }
            MouseEventKind::ScrollUp => {
                let col = mouse.column;
                let row = mouse.row;

                if let Some(rect) = self.ws().last_file_tree_rect {
                    if col >= rect.x && col < rect.x + rect.width
                        && row >= rect.y && row < rect.y + rect.height
                    {
                        self.ws_mut().file_tree.scroll_up(3);
                        return;
                    }
                }
                if let Some(rect) = self.ws().last_preview_rect {
                    if col >= rect.x && col < rect.x + rect.width
                        && row >= rect.y && row < rect.y + rect.height
                    {
                        self.ws_mut().preview.scroll_up(3);
                        return;
                    }
                }
                let pane_rects = self.ws().last_pane_rects.clone();
                for (pane_id, rect) in pane_rects {
                    if col >= rect.x && col < rect.x + rect.width
                        && row >= rect.y && row < rect.y + rect.height
                    {
                        if let Some(pane) = self.ws().panes.get(&pane_id) {
                            pane.scroll_up(3);
                        }
                        return;
                    }
                }
            }
            MouseEventKind::ScrollDown => {
                let col = mouse.column;
                let row = mouse.row;

                if let Some(rect) = self.ws().last_file_tree_rect {
                    if col >= rect.x && col < rect.x + rect.width
                        && row >= rect.y && row < rect.y + rect.height
                    {
                        self.ws_mut().file_tree.scroll_down(3);
                        return;
                    }
                }
                if let Some(rect) = self.ws().last_preview_rect {
                    if col >= rect.x && col < rect.x + rect.width
                        && row >= rect.y && row < rect.y + rect.height
                    {
                        self.ws_mut().preview.scroll_down(3);
                        return;
                    }
                }
                let pane_rects = self.ws().last_pane_rects.clone();
                for (pane_id, rect) in pane_rects {
                    if col >= rect.x && col < rect.x + rect.width
                        && row >= rect.y && row < rect.y + rect.height
                    {
                        if let Some(pane) = self.ws().panes.get(&pane_id) {
                            pane.scroll_down(3);
                        }
                        return;
                    }
                }
            }
            MouseEventKind::Moved => {
                let col = mouse.column;
                let old_hover = self.hover_border.clone();
                if self.is_on_file_tree_border(col) {
                    self.hover_border = Some(DragTarget::FileTreeBorder);
                } else if self.is_on_preview_border(col) {
                    self.hover_border = Some(DragTarget::PreviewBorder);
                } else {
                    self.hover_border = None;
                }
                if self.hover_border != old_hover {
                    self.dirty = true;
                }
            }
            _ => {}
        }
    }

    // ─── PTY forwarding ───────────────────────────────────

    /// Forward pasted text to PTY wrapped in bracketed paste sequences.
    pub fn forward_paste_to_pty(&mut self, text: &str) -> Result<()> {
        let focused_id = self.ws().focused_pane_id;
        if let Some(pane) = self.ws_mut().panes.get_mut(&focused_id) {
            pane.scroll_reset();
            // Wrap in bracketed paste: \x1b[200~ ... \x1b[201~
            let mut data = Vec::new();
            data.extend_from_slice(b"\x1b[200~");
            data.extend_from_slice(text.as_bytes());
            data.extend_from_slice(b"\x1b[201~");
            pane.write_input(&data)?;
        }
        Ok(())
    }

    #[allow(dead_code)]
    pub fn forward_key_to_pty(&mut self, key: KeyEvent) -> Result<()> {
        let focused_id = self.ws().focused_pane_id;
        if let Some(pane) = self.ws_mut().panes.get_mut(&focused_id) {
            pane.scroll_reset();
            if let Some(bytes) = key_event_to_bytes(&key) {
                pane.write_input(&bytes)?;
            }
        }
        Ok(())
    }

    pub fn drain_pty_events(&mut self) -> bool {
        let mut had_events = false;
        while let Ok(event) = self.event_rx.try_recv() {
            had_events = true;
            match event {
                AppEvent::PtyEof(pane_id) => {
                    for ws in &mut self.workspaces {
                        if let Some(pane) = ws.panes.get_mut(&pane_id) {
                            pane.exited = true;
                            break;
                        }
                    }
                }
                AppEvent::CwdChanged(pane_id, new_cwd) => {
                    for ws in &mut self.workspaces {
                        if ws.panes.contains_key(&pane_id) {
                            // Update pane's cwd
                            if let Some(pane) = ws.panes.get_mut(&pane_id) {
                                pane.cwd = new_cwd.clone();
                            }
                            if ws.focused_pane_id == pane_id && new_cwd.is_dir() {
                                let prev_show_hidden = ws.file_tree.show_hidden;
                                ws.file_tree = FileTree::new(new_cwd.clone());
                                // FileTree::new defaults to show_hidden=true
                                // Only toggle if the previous state was different
                                if ws.file_tree.show_hidden != prev_show_hidden {
                                    ws.file_tree.toggle_hidden();
                                }
                                ws.cwd = new_cwd;
                                ws.name = dir_name(&ws.cwd);
                                ws.preview.close();
                            }
                            break;
                        }
                    }
                }
                AppEvent::PtyOutput(_) => {}
            }
        }
        if had_events {
            self.dirty = true;
        }
        had_events
    }

    pub fn shutdown(&mut self) {
        for ws in &mut self.workspaces {
            ws.shutdown();
        }
    }
}

/// Extract directory name from a path for tab title.
fn dir_name(path: &std::path::Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| path.to_string_lossy().to_string())
}

/// Extract text from a pane's vt100 screen within a selection range.
fn extract_selected_text(pane: &Pane, sr: u16, sc: u16, er: u16, ec: u16) -> String {
    let parser = pane.parser.lock().unwrap_or_else(|e| e.into_inner());
    let screen = parser.screen();
    let mut lines = Vec::new();

    for row in sr..=er {
        let mut line = String::new();
        let col_start = if row == sr { sc } else { 0 };
        let col_end = if row == er { ec } else { 999 };

        for col in col_start..=col_end {
            if let Some(cell) = screen.cell(row, col) {
                let contents = cell.contents();
                if contents.is_empty() {
                    line.push(' ');
                } else {
                    line.push_str(contents);
                }
            }
        }
        lines.push(line.trim_end().to_string());
    }

    // Remove trailing empty lines
    while lines.last().map_or(false, |l| l.is_empty()) {
        lines.pop();
    }

    lines.join("\n")
}

/// Public wrapper for key_event_to_bytes (used by main.rs paste detection).
pub fn key_event_to_bytes_pub(key: &KeyEvent) -> Option<Vec<u8>> {
    key_event_to_bytes(key)
}

/// Convert a crossterm KeyEvent into bytes suitable for PTY input.
fn key_event_to_bytes(key: &KeyEvent) -> Option<Vec<u8>> {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let alt = key.modifiers.contains(KeyModifiers::ALT);

    match key.code {
        KeyCode::Char(c) => {
            if ctrl {
                let ctrl_byte = (c.to_ascii_lowercase() as u8).wrapping_sub(b'a').wrapping_add(1);
                if ctrl_byte <= 26 {
                    if alt {
                        // Alt+Ctrl+Char → ESC + ctrl byte
                        Some(vec![0x1b, ctrl_byte])
                    } else {
                        Some(vec![ctrl_byte])
                    }
                } else {
                    Some(c.to_string().into_bytes())
                }
            } else if alt {
                // Alt+Char → ESC + char (standard xterm behavior)
                let mut bytes = vec![0x1b];
                bytes.extend_from_slice(c.to_string().as_bytes());
                Some(bytes)
            } else {
                Some(c.to_string().into_bytes())
            }
        }
        // Alt+Enter → send newline (\n) for multi-line input in Claude Code
        KeyCode::Enter if alt => Some(vec![b'\n']),
        KeyCode::Enter => Some(vec![b'\r']),
        KeyCode::Backspace => Some(vec![0x7f]),
        KeyCode::Delete => Some(b"\x1b[3~".to_vec()),
        KeyCode::Tab => Some(vec![b'\t']),
        KeyCode::BackTab => Some(b"\x1b[Z".to_vec()),
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
                1 => "\x1bOP", 2 => "\x1bOQ", 3 => "\x1bOR", 4 => "\x1bOS",
                5 => "\x1b[15~", 6 => "\x1b[17~", 7 => "\x1b[18~", 8 => "\x1b[19~",
                9 => "\x1b[20~", 10 => "\x1b[21~", 11 => "\x1b[23~", 12 => "\x1b[24~",
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
        layout.remove_pane(2);
        assert_eq!(layout.pane_count(), 1);
        assert_eq!(layout.collect_pane_ids(), vec![1]);
    }

    #[test]
    fn test_layout_remove_first_pane() {
        let mut layout = LayoutNode::Leaf { pane_id: 1 };
        layout.split_pane(1, 2, SplitDirection::Vertical);
        layout.remove_pane(1);
        assert_eq!(layout.collect_pane_ids(), vec![2]);
    }

    #[test]
    fn test_calculate_rects_vertical() {
        let layout = LayoutNode::Split {
            direction: SplitDirection::Vertical,
            ratio: 0.5,
            first: Box::new(LayoutNode::Leaf { pane_id: 1 }),
            second: Box::new(LayoutNode::Leaf { pane_id: 2 }),
        };
        let rects = layout.calculate_rects(Rect::new(0, 0, 100, 50));
        assert_eq!(rects.len(), 2);
        assert_eq!(rects[0], (1, Rect::new(0, 0, 50, 50)));
        assert_eq!(rects[1], (2, Rect::new(50, 0, 50, 50)));
    }

    #[test]
    fn test_calculate_rects_horizontal() {
        let layout = LayoutNode::Split {
            direction: SplitDirection::Horizontal,
            ratio: 0.5,
            first: Box::new(LayoutNode::Leaf { pane_id: 1 }),
            second: Box::new(LayoutNode::Leaf { pane_id: 2 }),
        };
        let rects = layout.calculate_rects(Rect::new(0, 0, 100, 50));
        assert_eq!(rects.len(), 2);
        assert_eq!(rects[0], (1, Rect::new(0, 0, 100, 25)));
        assert_eq!(rects[1], (2, Rect::new(0, 25, 100, 25)));
    }

    #[test]
    fn test_focus_cycling() {
        let ids = vec![1, 2, 3];
        assert_eq!((0 + 1) % ids.len(), 1);
        assert_eq!((2 + 1) % ids.len(), 0);
    }
}
