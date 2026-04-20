use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::Instant;

use anyhow::Result;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::layout::Rect;

use crate::filetree::FileTree;
use crate::ipc::{self, PaneInfo, PaneRef};
use crate::layout_config::{DirectionSpec, LayoutConfig, LayoutNodeSpec};
use crate::pane::Pane;
use crate::preview::Preview;

/// Commands that flow from the IPC server thread into the App's event
/// loop. Each variant carries a `oneshot::Sender` so the server thread
/// can block-wait for the App to finish processing.
#[allow(dead_code)] // constructed by the IPC server (wired in Step 3.3)
#[derive(Debug)]
pub enum AppCommand {
    /// Snapshot the pane list of the active workspace.
    List {
        reply: oneshot::Sender<Vec<PaneInfo>>,
    },
    /// Write `data` to the target pane's PTY.
    Send {
        target: PaneRef,
        data: Vec<u8>,
        append_enter: bool,
        reply: oneshot::Sender<std::result::Result<(), String>>,
    },
    /// Move keyboard focus to the target pane in the active workspace.
    Focus {
        target: PaneRef,
        reply: oneshot::Sender<std::result::Result<(), String>>,
    },
    /// Split the target pane. If `command` is given, it's queued on the
    /// new pane and flushed when its shell prompt appears. If `name` is
    /// given, it's registered so later IPC calls can address the pane by
    /// name. Returns the new pane's id on success.
    Split {
        target: PaneRef,
        direction: ipc::Direction,
        command: Option<String>,
        name: Option<String>,
        reply: oneshot::Sender<std::result::Result<usize, String>>,
    },
    /// Open a new tab with a fresh single pane. Focus switches to the
    /// new tab (mirrors the Alt+T keybinding). Returns the new pane's
    /// id on success.
    NewTab {
        command: Option<String>,
        name: Option<String>,
        label: Option<String>,
        reply: oneshot::Sender<std::result::Result<usize, String>>,
    },
}

/// Resolve a [`PaneRef`] to a concrete pane id.
///
/// Pure helper separated from `Workspace` so it can be unit-tested
/// without spawning real PTYs. Returns `None` when the reference points
/// at a pane that no longer exists (e.g. a stale name or a closed id).
pub(crate) fn resolve_pane_ref_impl(
    r: &PaneRef,
    pane_names: &HashMap<String, usize>,
    known_ids: &HashSet<usize>,
    focused: usize,
) -> Option<usize> {
    match r {
        PaneRef::Focused => known_ids.contains(&focused).then_some(focused),
        PaneRef::Id(id) => known_ids.contains(id).then_some(*id),
        PaneRef::Name(name) => pane_names
            .get(name)
            .copied()
            .filter(|id| known_ids.contains(id)),
    }
}

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
#[derive(Debug)]
pub enum LayoutNode {
    Leaf {
        pane_id: usize,
    },
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
            LayoutNode::Split {
                direction,
                ratio,
                first,
                second,
            } => {
                let (first_area, second_area) = split_rect(area, *direction, *ratio);
                let mut result = first.calculate_rects(first_area);
                result.extend(second.calculate_rects(second_area));
                result
            }
        }
    }

    pub fn split_pane(
        &mut self,
        target_id: usize,
        new_id: usize,
        direction: SplitDirection,
    ) -> bool {
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
                        let second =
                            std::mem::replace(second.as_mut(), LayoutNode::Leaf { pane_id: 0 });
                        *self = second;
                        return true;
                    }
                }
                if let LayoutNode::Leaf { pane_id } = second.as_ref() {
                    if *pane_id == target_id {
                        let first =
                            std::mem::replace(first.as_mut(), LayoutNode::Leaf { pane_id: 0 });
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
        if let LayoutNode::Split {
            direction,
            ratio,
            first,
            second,
        } = self
        {
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

/// What the current text selection is anchored to.
#[derive(Debug, Clone, PartialEq)]
pub enum SelectionTarget {
    Pane(usize),
    Preview,
}

/// Text selection state. Works for both terminal panes and the file
/// preview panel — `target` tells rendering and extraction which
/// source to read.
///
/// Coordinate semantics differ by target:
/// - **Pane**: start/end rows+cols are screen-relative to
///   `content_rect` (the inner area of the pane border).
/// - **Preview**: rows are **absolute line indices** into
///   `preview.lines`; cols are **char offsets** within the line.
///   This lets the selection survive vertical and horizontal
///   scrolling — overlay rendering subtracts the current scroll
///   to turn source coords back into screen coords.
#[derive(Debug, Clone)]
pub struct TextSelection {
    pub target: SelectionTarget,
    pub start_row: u32,
    pub start_col: u32,
    pub end_row: u32,
    pub end_col: u32,
    /// Content area used for coordinate mapping — the inside of the
    /// pane border, or (for previews) the area excluding the line
    /// number gutter.
    pub content_rect: Rect,
}

impl TextSelection {
    /// Get normalized (top-left to bottom-right) selection range.
    pub fn normalized(&self) -> (u32, u32, u32, u32) {
        if self.start_row < self.end_row
            || (self.start_row == self.end_row && self.start_col <= self.end_col)
        {
            (self.start_row, self.start_col, self.end_row, self.end_col)
        } else {
            (self.end_row, self.end_col, self.start_row, self.start_col)
        }
    }

    /// Check if a cell is within the selection.
    pub fn contains(&self, row: u32, col: u32) -> bool {
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
    /// Session-only rename; when Some, takes precedence over `name` for
    /// display. Not persisted; `cd` does not touch this.
    pub custom_name: Option<String>,
    pub cwd: PathBuf,
    pub panes: HashMap<usize, Pane>,
    pub layout: LayoutNode,
    pub focused_pane_id: usize,
    /// Stable human-friendly name → pane id map, populated by layout
    /// files (Phase 2) and by IPC `Split { id: Some(name), .. }` calls
    /// (Phase 3). Never shrinks automatically; closing a pane leaves the
    /// name entry dangling and `resolve_pane_ref_impl` treats it as
    /// unknown.
    pub pane_names: HashMap<String, usize>,
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
            custom_name: None,
            file_tree: FileTree::new(cwd.clone()),
            cwd,
            panes,
            layout: LayoutNode::Leaf { pane_id },
            focused_pane_id: pane_id,
            pane_names: HashMap::new(),
            file_tree_visible: true,
            preview: Preview::new(),
            focus_target: FocusTarget::Pane,
            last_pane_rects: Vec::new(),
            last_file_tree_rect: None,
            last_preview_rect: None,
        })
    }

    /// Resolve a [`PaneRef`] against this workspace's panes and names.
    pub fn resolve_pane_ref(&self, r: &PaneRef) -> Option<usize> {
        let known: HashSet<usize> = self.panes.keys().copied().collect();
        resolve_pane_ref_impl(r, &self.pane_names, &known, self.focused_pane_id)
    }

    fn shutdown(&mut self) {
        for pane in self.panes.values_mut() {
            pane.kill();
        }
    }

    /// Tab label to show in the UI: custom rename wins over the
    /// cwd-derived name.
    pub fn display_name(&self) -> &str {
        self.custom_name.as_deref().unwrap_or(&self.name)
    }
}

// ─── App (global state) ───────────────────────────────────

pub struct App {
    pub workspaces: Vec<Workspace>,
    pub active_tab: usize,
    pub should_quit: bool,
    pub event_tx: Sender<AppEvent>,
    pub event_rx: Receiver<AppEvent>,
    /// Clonable sender for the IPC server thread. Drop the server thread
    /// to stop producing commands; the receiver lives on the App side.
    pub command_tx: Sender<AppCommand>,
    command_rx: Receiver<AppCommand>,
    next_pane_id: usize,
    pub dirty: bool,
    pub paste_cooldown: u8, // frames to skip rendering after paste
    /// Frames to skip rendering after a layout change (split, close,
    /// sidebar toggle, terminal resize). Gives Claude Code / bash time
    /// to process SIGWINCH and send a fresh redraw before we paint,
    /// avoiding the brief "old buffer at new size" garbled frame.
    pub resize_cooldown: u8,
    /// Last known terminal size (cols, rows). Updated from main.rs on
    /// Event::Resize and from ui::render on every frame. Used by
    /// `relayout_panes()` so layout-change handlers can resize PTYs
    /// without needing a Frame reference.
    pub last_term_size: (u16, u16),
    // Shared settings
    pub file_tree_width: u16,
    pub preview_width: u16,
    // Layout: swap preview and terminal positions
    pub layout_swapped: bool,
    // Toggle status bar visibility (Alt+S)
    pub status_bar_visible: bool,
    // Drag/hover state
    pub dragging: Option<DragTarget>,
    pub hover_border: Option<DragTarget>,
    // Tab bar rects for mouse click
    pub last_tab_rects: Vec<(usize, Rect)>,
    pub last_new_tab_rect: Option<Rect>,
    /// Active tab rename input buffer. When `Some`, key input is
    /// routed to this buffer instead of the focused PTY; Enter commits
    /// to the active workspace's `custom_name`, Esc cancels.
    pub rename_input: Option<String>,
    /// (tab index, timestamp) of the last left-click on a tab label.
    /// Used to detect a double-click → enter rename mode.
    last_tab_click: Option<(usize, Instant)>,
    // Text selection
    pub selection: Option<TextSelection>,
    // Version check (background)
    pub version_info: crate::version_check::VersionInfo,
    // Claude Code JSONL monitoring
    pub claude_monitor: crate::claude_monitor::ClaudeMonitor,
    // Reusable clipboard handle (lazy-initialized)
    clipboard: Option<arboard::Clipboard>,
}

impl App {
    pub fn new(rows: u16, cols: u16) -> Result<Self> {
        let (event_tx, event_rx) = mpsc::channel();
        let (command_tx, command_rx) = mpsc::channel();

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
            command_tx,
            command_rx,
            next_pane_id: 2,
            dirty: true,
            paste_cooldown: 0,
            resize_cooldown: 0,
            last_term_size: (cols, rows),
            file_tree_width: 20,
            preview_width: 40,
            layout_swapped: true,
            status_bar_visible: true,
            dragging: None,
            hover_border: None,
            last_tab_rects: Vec::new(),
            last_new_tab_rect: None,
            rename_input: None,
            last_tab_click: None,
            selection: None,
            version_info: {
                let info = crate::version_check::VersionInfo::new();
                crate::version_check::spawn_check(info.clone());
                info
            },
            claude_monitor: crate::claude_monitor::ClaudeMonitor::new(),
            clipboard: None,
        })
    }

    /// Copy text to clipboard, reusing the handle if available.
    fn copy_to_clipboard(&mut self, text: &str) {
        if self.clipboard.is_none() {
            self.clipboard = arboard::Clipboard::new().ok();
        }
        if let Some(ref mut cb) = self.clipboard {
            let _ = cb.set_text(text);
        }
    }

    /// Drop the current selection if it targets the preview. Called
    /// whenever preview state shifts (scroll, new file) so the
    /// highlighted range can't point at different text than what
    /// Ctrl+C or mouse-up actually copies.
    fn clear_selection_if_preview(&mut self) {
        if matches!(
            self.selection.as_ref().map(|s| &s.target),
            Some(SelectionTarget::Preview)
        ) {
            self.selection = None;
        }
    }

    /// Recompute pane rectangles and apply sizes to every PTY in the
    /// active workspace. Returns `true` if any pane was actually
    /// resized (so callers can decide whether to enter the post-resize
    /// cooldown). Safe to call without a Frame — uses the cached
    /// `last_term_size`.
    pub fn relayout_panes(&mut self) -> bool {
        let (cols, rows) = self.last_term_size;
        if cols < 20 || rows < 5 {
            return false;
        }

        // Mirror the area math in ui::render / render_main_area,
        // including the fallback where tree / preview are hidden when
        // the terminal is too narrow. Keeping these in sync prevents
        // PTY size drift from the actually-painted pane size.
        const MIN_PANE_AREA_WIDTH: u16 = 20;
        let tab_h = 1u16;
        let status_h: u16 = if self.status_bar_visible || self.rename_input.is_some() {
            1
        } else {
            0
        };
        let main_h = rows.saturating_sub(tab_h + status_h);

        let mut has_tree = self.ws().file_tree_visible;
        let mut has_preview = self.ws().preview.is_active();
        let tree_w_nom = self.file_tree_width;
        let preview_w_nom = self.preview_width;

        let needed = MIN_PANE_AREA_WIDTH
            + if has_tree { tree_w_nom } else { 0 }
            + if has_preview { preview_w_nom } else { 0 };
        if cols < needed && has_preview {
            has_preview = false;
        }
        let needed = MIN_PANE_AREA_WIDTH + if has_tree { tree_w_nom } else { 0 };
        if cols < needed && has_tree {
            has_tree = false;
        }

        let tree_w = if has_tree { tree_w_nom } else { 0 };
        let preview_w = if has_preview { preview_w_nom } else { 0 };
        let pane_w = cols.saturating_sub(tree_w).saturating_sub(preview_w);

        // x/y exact values don't matter for calculate_rects' sub-areas,
        // only width/height propagate into the recursive split sizes.
        let pane_area = Rect::new(0, tab_h, pane_w, main_h);
        let rects = self.ws().layout.calculate_rects(pane_area);

        let mut any_changed = false;
        for (pane_id, rect) in &rects {
            if let Some(pane) = self.ws_mut().panes.get_mut(pane_id) {
                let inner_rows = rect.height.saturating_sub(2);
                let inner_cols = rect.width.saturating_sub(2);
                if pane.resize(inner_rows, inner_cols).unwrap_or(false) {
                    any_changed = true;
                }
            }
        }

        self.ws_mut().last_pane_rects = rects;
        any_changed
    }

    /// Mark a layout change: apply resizes immediately and, if sizes
    /// actually changed, delay the next paint for a few frames so the
    /// PTY child can respond to SIGWINCH with a fresh redraw before
    /// we render. When no size changes happen (e.g. a sidebar toggle
    /// that fits in the same remaining width) we skip the cooldown so
    /// the UI stays responsive. Also drops any live selection, whose
    /// stored `content_rect` / `pane_id` could reference a layout that
    /// no longer exists.
    pub fn mark_layout_change(&mut self) {
        let changed = self.relayout_panes();
        if changed {
            // Take max so a freshly-triggered layout change on top of
            // an existing cooldown doesn't prematurely cut the wait.
            self.resize_cooldown = self.resize_cooldown.max(5);
        }
        // Any in-flight selection is bound to the old geometry.
        self.selection = None;
        self.dirty = true;
    }

    /// Called from main.rs on crossterm Resize events so we can update
    /// the cached terminal size and propagate the resize into panes.
    pub fn on_terminal_resize(&mut self, cols: u16, rows: u16) {
        self.last_term_size = (cols, rows);
        self.mark_layout_change();
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
        // Rename mode — swallow all input until Enter/Esc.
        if self.rename_input.is_some() {
            return Ok(self.handle_rename_key(key));
        }

        // Ctrl+Q — quit
        if key.modifiers == KeyModifiers::CONTROL && key.code == KeyCode::Char('q') {
            self.should_quit = true;
            return Ok(true);
        }

        // Alt+R — rename active tab (session only)
        if key.modifiers == KeyModifiers::ALT
            && matches!(key.code, KeyCode::Char('r') | KeyCode::Char('R'))
        {
            self.rename_input = Some(String::new());
            if !self.status_bar_visible {
                self.mark_layout_change();
            }
            return Ok(true);
        }

        // Ctrl+C — if text is selected, copy to clipboard instead of sending SIGINT
        if key.modifiers == KeyModifiers::CONTROL && key.code == KeyCode::Char('c') {
            if let Some(ref sel) = self.selection.clone() {
                let (sr, sc, er, ec) = sel.normalized();
                if sr != er || sc != ec {
                    let text = match sel.target {
                        SelectionTarget::Pane(pane_id) => self
                            .ws()
                            .panes
                            .get(&pane_id)
                            .map(|p| extract_selected_text(p, sr, sc, er, ec))
                            .unwrap_or_default(),
                        SelectionTarget::Preview => {
                            extract_preview_selected_text(&self.ws().preview, sr, sc, er, ec)
                        }
                    };
                    if !text.is_empty() {
                        self.copy_to_clipboard(&text);
                    }
                    self.selection = None;
                    return Ok(true);
                }
            }
            // No selection — fall through to forward Ctrl+C to PTY
        }

        // Ctrl+T / Alt+T — new tab (Alt+T groups with Alt-based tab nav)
        if (key.modifiers == KeyModifiers::CONTROL || key.modifiers == KeyModifiers::ALT)
            && matches!(key.code, KeyCode::Char('t') | KeyCode::Char('T'))
        {
            self.new_tab()?;
            return Ok(true);
        }

        // Alt+Right — next tab
        if key.modifiers == KeyModifiers::ALT && key.code == KeyCode::Right {
            if !self.workspaces.is_empty() {
                self.active_tab = (self.active_tab + 1) % self.workspaces.len();
            }
            return Ok(true);
        }

        // Alt+Left — previous tab
        if key.modifiers == KeyModifiers::ALT && key.code == KeyCode::Left {
            if !self.workspaces.is_empty() {
                self.active_tab = if self.active_tab == 0 {
                    self.workspaces.len() - 1
                } else {
                    self.active_tab - 1
                };
            }
            return Ok(true);
        }

        // Alt+S — toggle status bar
        if key.modifiers == KeyModifiers::ALT
            && matches!(key.code, KeyCode::Char('s') | KeyCode::Char('S'))
        {
            self.status_bar_visible = !self.status_bar_visible;
            self.mark_layout_change();
            return Ok(true);
        }

        // Alt+1 .. Alt+9 — jump to tab N
        if key.modifiers == KeyModifiers::ALT {
            if let KeyCode::Char(c) = key.code {
                if let Some(digit) = c.to_digit(10) {
                    if digit >= 1 && (digit as usize) <= self.workspaces.len() {
                        self.active_tab = (digit as usize) - 1;
                        return Ok(true);
                    }
                }
            }
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

    fn handle_rename_key(&mut self, key: KeyEvent) -> bool {
        let Some(buf) = self.rename_input.as_mut() else {
            return false;
        };
        let needs_relayout = !self.status_bar_visible;
        match key.code {
            KeyCode::Esc => {
                self.rename_input = None;
                if needs_relayout {
                    self.mark_layout_change();
                }
            }
            KeyCode::Enter => {
                let trimmed = buf.trim().to_string();
                self.ws_mut().custom_name = if trimmed.is_empty() {
                    None
                } else {
                    Some(trimmed)
                };
                self.rename_input = None;
                if needs_relayout {
                    self.mark_layout_change();
                }
            }
            KeyCode::Backspace => {
                buf.pop();
            }
            KeyCode::Char(c) => {
                // Ignore chars combined with Ctrl/Alt so shortcuts like
                // Ctrl+C don't leak into the buffer as literal letters.
                // Shift is fine — that's just uppercase.
                if key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
                {
                    return true;
                }
                // Cap at something sane so a stuck key can't grow the tab bar forever.
                if buf.chars().count() < 32 {
                    buf.push(c);
                }
            }
            _ => return true,
        }
        self.dirty = true;
        true
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
                    self.clear_selection_if_preview();
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
                self.clear_selection_if_preview();
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
            // Horizontal scroll — unmodified arrow keys and vim-style h/l.
            // Ctrl+Left/Right remain focus navigation (matched below).
            (KeyModifiers::NONE, KeyCode::Right)
            | (KeyModifiers::NONE, KeyCode::Char('l'))
            | (KeyModifiers::SHIFT, KeyCode::Right) => {
                self.ws_mut().preview.scroll_right(4);
                Ok(true)
            }
            (KeyModifiers::NONE, KeyCode::Left)
            | (KeyModifiers::NONE, KeyCode::Char('h'))
            | (KeyModifiers::SHIFT, KeyCode::Left) => {
                self.ws_mut().preview.scroll_left(4);
                Ok(true)
            }
            (KeyModifiers::NONE, KeyCode::Home) => {
                self.ws_mut().preview.h_scroll_offset = 0;
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
        // Clean up claude monitor state for all panes in this tab
        let pane_ids: Vec<usize> = self.workspaces[index].panes.keys().copied().collect();
        for pane_id in pane_ids {
            self.claude_monitor.remove(pane_id);
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
        let was_visible = ws.file_tree_visible;
        let will_be_visible;
        if ws.file_tree_visible && ws.focus_target == FocusTarget::FileTree {
            // Closing the tree — keep the preview open so the user can
            // continue reading the file they just opened. Focus moves
            // to the preview if it's active, otherwise back to the pane.
            ws.file_tree_visible = false;
            ws.focus_target = if ws.preview.is_active() {
                FocusTarget::Preview
            } else {
                FocusTarget::Pane
            };
            will_be_visible = false;
        } else if ws.file_tree_visible {
            ws.focus_target = FocusTarget::FileTree;
            will_be_visible = true;
        } else {
            ws.file_tree_visible = true;
            ws.focus_target = FocusTarget::FileTree;
            will_be_visible = true;
        }

        // Only relayout if the pane area actually changes (visibility flipped).
        if was_visible != will_be_visible {
            self.mark_layout_change();
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
        let parent_cwd = self
            .ws()
            .panes
            .get(&self.ws().focused_pane_id)
            .map(|p| p.cwd.clone());

        let pane = Pane::new_with_cwd(new_id, 10, 40, self.event_tx.clone(), parent_cwd)?;
        let ws = self.ws_mut();
        ws.panes.insert(new_id, pane);
        ws.layout.split_pane(ws.focused_pane_id, new_id, direction);
        // Focus moves to the freshly-created pane so the user can type
        // in it immediately after splitting.
        ws.focused_pane_id = new_id;

        self.mark_layout_change();
        Ok(())
    }

    /// Apply a multi-pane layout to the active workspace.
    ///
    /// The workspace is expected to contain a single initial pane created
    /// by `App::new`. The root of the layout tree is mapped onto that
    /// initial pane; splits are produced by recursively driving
    /// `split_focused_pane`. Each `Pane` node's `command` (if any) is
    /// queued via `Pane::queue_startup_command`, which the main event
    /// loop will flush once the shell prompt is observed.
    ///
    /// Note: Phase 2 ignores the per-split `ratio`; splits use the
    /// existing equal-split semantics. A follow-up may extend this.
    pub fn apply_layout(&mut self, config: &LayoutConfig) -> Result<()> {
        let initial_pane_id = self.ws().focused_pane_id;
        self.apply_layout_node(&config.root, initial_pane_id)?;
        Ok(())
    }

    fn apply_layout_node(&mut self, node: &LayoutNodeSpec, target_pane_id: usize) -> Result<()> {
        match node {
            LayoutNodeSpec::Pane { id, command } => {
                // Register the leaf's id as a human-friendly name so IPC
                // clients (and external tools like `ccmux-send`) can
                // target this pane later without tracking numeric ids.
                if !id.is_empty() {
                    self.ws_mut().pane_names.insert(id.clone(), target_pane_id);
                }
                if let Some(cmd) = command {
                    if let Some(pane) = self.ws_mut().panes.get_mut(&target_pane_id) {
                        pane.queue_startup_command(cmd);
                    }
                }
                Ok(())
            }
            LayoutNodeSpec::Split {
                direction,
                ratio: _,
                first,
                second,
            } => {
                // split_focused_pane operates on the focused pane and
                // moves focus to the freshly-created child, so we focus
                // the target first, then capture the new id afterwards.
                self.ws_mut().focused_pane_id = target_pane_id;
                let split_dir = match direction {
                    DirectionSpec::Vertical => SplitDirection::Vertical,
                    DirectionSpec::Horizontal => SplitDirection::Horizontal,
                };
                self.split_focused_pane(split_dir)?;
                let new_pane_id = self.ws().focused_pane_id;
                self.apply_layout_node(first, target_pane_id)?;
                self.apply_layout_node(second, new_pane_id)?;
                Ok(())
            }
        }
    }

    fn close_focused_pane(&mut self) {
        let focused = self.ws().focused_pane_id;
        let ws = self.ws_mut();
        if ws.layout.pane_count() <= 1 {
            return;
        }

        let pane_ids = ws.layout.collect_pane_ids();
        let current_idx = pane_ids.iter().position(|&id| id == focused);

        ws.layout.remove_pane(focused);

        if let Some(mut pane) = ws.panes.remove(&focused) {
            pane.kill();
        }

        // Clean up claude monitor state for this pane
        self.claude_monitor.remove(focused);
        let ws = self.ws_mut();

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

        self.mark_layout_change();
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
        // Cancel any in-progress rename on mouse click so
        // the buffer can't silently migrate to another tab.
        if matches!(mouse.kind, MouseEventKind::Down(_)) && self.rename_input.is_some() {
            let needs_relayout = !self.status_bar_visible;
            self.rename_input = None;
            self.dirty = true;
            if needs_relayout {
                self.mark_layout_change();
            }
        }

        match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                let col = mouse.column;
                let row = mouse.row;

                // Clear previous selection on any click
                self.selection = None;

                // Check tab bar clicks
                for &(tab_idx, rect) in &self.last_tab_rects {
                    if col >= rect.x
                        && col < rect.x + rect.width
                        && row >= rect.y
                        && row < rect.y + rect.height
                    {
                        let now = Instant::now();
                        let is_double = matches!(
                            self.last_tab_click,
                            Some((prev_idx, prev_t))
                                if prev_idx == tab_idx
                                    && now.duration_since(prev_t).as_millis() < 500
                        );
                        self.active_tab = tab_idx;
                        if is_double {
                            self.rename_input = Some(String::new());
                            self.last_tab_click = None;
                        } else {
                            self.last_tab_click = Some((tab_idx, now));
                        }
                        self.dirty = true;
                        return;
                    }
                }
                // Click missed the tab bar — reset double-click tracker.
                self.last_tab_click = None;

                // Check [+] new tab button
                if let Some(rect) = self.last_new_tab_rect {
                    if col >= rect.x
                        && col < rect.x + rect.width
                        && row >= rect.y
                        && row < rect.y + rect.height
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
                                col >= boundary.saturating_sub(1)
                                    && col <= boundary
                                    && row >= pane_area.y
                                    && row < pane_area.y + pane_area.height
                            }
                            SplitDirection::Horizontal => {
                                row >= boundary.saturating_sub(1)
                                    && row <= boundary
                                    && col >= pane_area.x
                                    && col < pane_area.x + pane_area.width
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
                    if col >= rect.x
                        && col < rect.x + rect.width
                        && row >= rect.y
                        && row < rect.y + rect.height
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
                                self.clear_selection_if_preview();
                                self.ws_mut().preview.load(&path);
                            }
                        }
                        return;
                    }
                }

                // Check preview click
                if let Some(rect) = self.ws().last_preview_rect {
                    if col >= rect.x
                        && col < rect.x + rect.width
                        && row >= rect.y
                        && row < rect.y + rect.height
                    {
                        self.ws_mut().focus_target = FocusTarget::Preview;
                        return;
                    }
                }

                // Check pane clicks
                let pane_rects = self.ws().last_pane_rects.clone();
                for (pane_id, rect) in pane_rects {
                    if col >= rect.x
                        && col < rect.x + rect.width
                        && row >= rect.y
                        && row < rect.y + rect.height
                    {
                        self.ws_mut().focused_pane_id = pane_id;
                        self.ws_mut().focus_target = FocusTarget::Pane;

                        // Check if clicking on scrollbar (rightmost column inside border)
                        let scrollbar_col = rect.x + rect.width - 2; // -1 border, -1 scrollbar
                        if col >= scrollbar_col {
                            let inner = Rect::new(
                                rect.x + 1,
                                rect.y + 1,
                                rect.width.saturating_sub(2),
                                rect.height.saturating_sub(2),
                            );
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
                    let inner = sel.content_rect;
                    match sel.target {
                        SelectionTarget::Pane(_) => {
                            // Pane: screen-relative coords inside inner.
                            sel.end_col = col
                                .saturating_sub(inner.x)
                                .min(inner.width.saturating_sub(1))
                                as u32;
                            sel.end_row = row
                                .saturating_sub(inner.y)
                                .min(inner.height.saturating_sub(1))
                                as u32;
                        }
                        SelectionTarget::Preview => {
                            // Preview: translate screen coords to
                            // source (absolute line + char offset)
                            // using the current scroll state.
                            let scroll_v = self.ws().preview.scroll_offset;
                            let h_scroll = self.ws().preview.h_scroll_offset;

                            let mut screen_col = col.saturating_sub(inner.x);
                            let mut screen_row = row.saturating_sub(inner.y);

                            // Auto-scroll when drag reaches an edge.
                            // Move the underlying scroll by one step
                            // so the cursor can "pull" more content
                            // into view. Clamp screen position so the
                            // computed source coord tracks the new edge.
                            if col < inner.x {
                                self.ws_mut().preview.scroll_left(2);
                                screen_col = 0;
                            } else if col >= inner.x + inner.width {
                                self.ws_mut().preview.scroll_right(2);
                                screen_col = inner.width.saturating_sub(1);
                            }
                            if row < inner.y {
                                self.ws_mut().preview.scroll_up(1);
                                screen_row = 0;
                            } else if row >= inner.y + inner.height {
                                self.ws_mut().preview.scroll_down(1);
                                screen_row = inner.height.saturating_sub(1);
                            }

                            // Re-read scroll state in case we changed it above.
                            let scroll_v = self.ws().preview.scroll_offset.max(scroll_v);
                            let h_scroll = self.ws().preview.h_scroll_offset.max(h_scroll);
                            // Clamp end_row to a valid absolute line index.
                            let lines_len = self.ws().preview.lines.len();
                            let abs_row =
                                (scroll_v + screen_row as usize).min(lines_len.saturating_sub(1));
                            let abs_col = screen_col as usize + h_scroll;
                            // Update the selection endpoint (source coords).
                            if let Some(sel) = self.selection.as_mut() {
                                sel.end_row = abs_row as u32;
                                sel.end_col = abs_col as u32;
                            }
                        }
                    }
                } else {
                    // Start new selection — try pane areas first, then preview
                    let pane_rects = self.ws().last_pane_rects.clone();
                    let mut started = false;
                    for (pane_id, rect) in pane_rects {
                        if col >= rect.x
                            && col < rect.x + rect.width
                            && row >= rect.y
                            && row < rect.y + rect.height
                        {
                            let inner = Rect::new(
                                rect.x + 1,
                                rect.y + 1,
                                rect.width.saturating_sub(2),
                                rect.height.saturating_sub(2),
                            );
                            let cell_col = col.saturating_sub(inner.x) as u32;
                            let cell_row = row.saturating_sub(inner.y) as u32;
                            self.selection = Some(TextSelection {
                                target: SelectionTarget::Pane(pane_id),
                                start_row: cell_row,
                                start_col: cell_col,
                                end_row: cell_row,
                                end_col: cell_col,
                                content_rect: inner,
                            });
                            started = true;
                            break;
                        }
                    }
                    // Preview drag selection. Content area is the inside
                    // of the preview border minus the 5-column line-number
                    // gutter (format "{:>4}│"). Selection stores source
                    // coords (abs line index, char offset) so it can
                    // survive scrolling.
                    if !started {
                        if let Some(rect) = self.ws().last_preview_rect {
                            if col >= rect.x
                                && col < rect.x + rect.width
                                && row >= rect.y
                                && row < rect.y + rect.height
                            {
                                const GUTTER: u16 = 5;
                                let inner = Rect::new(
                                    rect.x + 1 + GUTTER,
                                    rect.y + 1,
                                    rect.width.saturating_sub(2 + GUTTER),
                                    rect.height.saturating_sub(2),
                                );
                                // Ignore drags that start inside the gutter
                                if col >= inner.x && row >= inner.y {
                                    let screen_col = col.saturating_sub(inner.x);
                                    let screen_row = row.saturating_sub(inner.y);
                                    let scroll_v = self.ws().preview.scroll_offset;
                                    let h_scroll = self.ws().preview.h_scroll_offset;
                                    let lines_len = self.ws().preview.lines.len();
                                    let abs_row = (scroll_v + screen_row as usize)
                                        .min(lines_len.saturating_sub(1));
                                    let abs_col = screen_col as usize + h_scroll;
                                    self.selection = Some(TextSelection {
                                        target: SelectionTarget::Preview,
                                        start_row: abs_row as u32,
                                        start_col: abs_col as u32,
                                        end_row: abs_row as u32,
                                        end_col: abs_col as u32,
                                        content_rect: inner,
                                    });
                                }
                            }
                        }
                    }
                }
            }
            MouseEventKind::Up(MouseButton::Left) => {
                self.dragging = None;

                // Copy selected text to clipboard
                if let Some(sel) = self.selection.clone() {
                    let (sr, sc, er, ec) = sel.normalized();
                    if sr != er || sc != ec {
                        let text = match sel.target {
                            SelectionTarget::Pane(pane_id) => self
                                .ws()
                                .panes
                                .get(&pane_id)
                                .map(|p| extract_selected_text(p, sr, sc, er, ec))
                                .unwrap_or_default(),
                            SelectionTarget::Preview => {
                                extract_preview_selected_text(&self.ws().preview, sr, sc, er, ec)
                            }
                        };
                        if !text.is_empty() {
                            self.copy_to_clipboard(&text);
                        }
                    }
                    // Keep selection visible until next click
                }
            }
            MouseEventKind::ScrollUp => {
                let col = mouse.column;
                let row = mouse.row;

                if let Some(rect) = self.ws().last_file_tree_rect {
                    if col >= rect.x
                        && col < rect.x + rect.width
                        && row >= rect.y
                        && row < rect.y + rect.height
                    {
                        self.ws_mut().file_tree.scroll_up(3);
                        return;
                    }
                }
                if let Some(rect) = self.ws().last_preview_rect {
                    if col >= rect.x
                        && col < rect.x + rect.width
                        && row >= rect.y
                        && row < rect.y + rect.height
                    {
                        self.ws_mut().preview.scroll_up(3);
                        return;
                    }
                }
                let pane_rects = self.ws().last_pane_rects.clone();
                for (pane_id, rect) in pane_rects {
                    if col >= rect.x
                        && col < rect.x + rect.width
                        && row >= rect.y
                        && row < rect.y + rect.height
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
                    if col >= rect.x
                        && col < rect.x + rect.width
                        && row >= rect.y
                        && row < rect.y + rect.height
                    {
                        self.ws_mut().file_tree.scroll_down(3);
                        return;
                    }
                }
                if let Some(rect) = self.ws().last_preview_rect {
                    if col >= rect.x
                        && col < rect.x + rect.width
                        && row >= rect.y
                        && row < rect.y + rect.height
                    {
                        self.ws_mut().preview.scroll_down(3);
                        return;
                    }
                }
                let pane_rects = self.ws().last_pane_rects.clone();
                for (pane_id, rect) in pane_rects {
                    if col >= rect.x
                        && col < rect.x + rect.width
                        && row >= rect.y
                        && row < rect.y + rect.height
                    {
                        if let Some(pane) = self.ws().panes.get(&pane_id) {
                            pane.scroll_down(3);
                        }
                        return;
                    }
                }
            }
            MouseEventKind::ScrollLeft => {
                let col = mouse.column;
                let row = mouse.row;
                if let Some(rect) = self.ws().last_preview_rect {
                    if col >= rect.x
                        && col < rect.x + rect.width
                        && row >= rect.y
                        && row < rect.y + rect.height
                    {
                        self.ws_mut().preview.scroll_left(4);
                    }
                }
            }
            MouseEventKind::ScrollRight => {
                let col = mouse.column;
                let row = mouse.row;
                if let Some(rect) = self.ws().last_preview_rect {
                    if col >= rect.x
                        && col < rect.x + rect.width
                        && row >= rect.y
                        && row < rect.y + rect.height
                    {
                        self.ws_mut().preview.scroll_right(4);
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

    /// Forward pasted text to PTY, wrapping in bracketed paste only if
    /// the PTY application has enabled the mode (e.g. Claude Code, modern
    /// readline). Sending bracketed paste to a shell that hasn't opted in
    /// causes the escape sequences to appear as literal text (issue #2).
    pub fn forward_paste_to_pty(&mut self, text: &str) -> Result<()> {
        let focused_id = self.ws().focused_pane_id;
        if let Some(pane) = self.ws_mut().panes.get_mut(&focused_id) {
            pane.scroll_reset();
            if pane.is_bracketed_paste_enabled() {
                let mut data = Vec::with_capacity(text.len() + 12);
                data.extend_from_slice(b"\x1b[200~");
                data.extend_from_slice(text.as_bytes());
                data.extend_from_slice(b"\x1b[201~");
                pane.write_input(&data)?;
            } else {
                pane.write_input(text.as_bytes())?;
            }
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
                    // Security: resolve symlinks and relative components.
                    // Reject paths that don't resolve to a real directory
                    // (prevents OSC 7 escape sequence path injection).
                    let new_cwd = match new_cwd.canonicalize() {
                        Ok(p) if p.is_dir() => p,
                        _ => continue,
                    };
                    for ws in &mut self.workspaces {
                        if ws.panes.contains_key(&pane_id) {
                            // Update pane's cwd
                            if let Some(pane) = ws.panes.get_mut(&pane_id) {
                                pane.cwd = new_cwd.clone();
                            }
                            if ws.focused_pane_id == pane_id {
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

    /// Drain any pending IPC commands and dispatch them. Safe to call
    /// every frame — it's a no-op when the channel is empty. Commands
    /// always target the active workspace.
    pub fn drain_app_commands(&mut self) {
        // Pop into a local Vec first so we can borrow `self` mutably
        // inside `handle_app_command` without fighting the receiver
        // borrow.
        let mut cmds: Vec<AppCommand> = Vec::new();
        while let Ok(cmd) = self.command_rx.try_recv() {
            cmds.push(cmd);
        }
        for cmd in cmds {
            self.handle_app_command(cmd);
        }
    }

    fn handle_app_command(&mut self, cmd: AppCommand) {
        match cmd {
            AppCommand::List { reply } => {
                let ws = self.ws();
                let focused = ws.focused_pane_id;
                // Invert pane_names so we can look up by id.
                let mut name_by_id: HashMap<usize, String> = HashMap::new();
                for (name, id) in &ws.pane_names {
                    name_by_id.insert(*id, name.clone());
                }
                let mut infos: Vec<PaneInfo> = Vec::new();
                for id in ws.layout.collect_pane_ids() {
                    infos.push(PaneInfo {
                        id,
                        name: name_by_id.get(&id).cloned(),
                        focused: id == focused,
                    });
                }
                let _ = reply.send(infos);
            }
            AppCommand::Send {
                target,
                data,
                append_enter,
                reply,
            } => {
                let result = self.handle_send(&target, &data, append_enter);
                let _ = reply.send(result);
            }
            AppCommand::Focus { target, reply } => {
                let result = self.handle_focus(&target);
                let _ = reply.send(result);
            }
            AppCommand::Split {
                target,
                direction,
                command,
                name,
                reply,
            } => {
                let result = self.handle_split(&target, direction, command, name);
                let _ = reply.send(result);
            }
            AppCommand::NewTab {
                command,
                name,
                label,
                reply,
            } => {
                let result = self.handle_new_tab(command, name, label);
                let _ = reply.send(result);
            }
        }
    }

    fn handle_new_tab(
        &mut self,
        command: Option<String>,
        name: Option<String>,
        label: Option<String>,
    ) -> std::result::Result<usize, String> {
        // Delegate to the existing keybinding-driven new_tab so we
        // don't drift from the interactive behavior (pane id bookkeeping,
        // active_tab update, cwd inheritance). The freshly-created tab
        // becomes the active one, so `ws_mut()` points at it.
        self.new_tab().map_err(|e| e.to_string())?;
        let new_pane_id = self.ws().focused_pane_id;
        if let Some(cmd) = command {
            if let Some(pane) = self.ws_mut().panes.get_mut(&new_pane_id) {
                pane.queue_startup_command(&cmd);
            }
        }
        if let Some(name) = name {
            if !name.is_empty() {
                self.ws_mut().pane_names.insert(name, new_pane_id);
            }
        }
        if let Some(label) = label {
            if !label.is_empty() {
                self.ws_mut().custom_name = Some(label);
            }
        }
        self.dirty = true;
        Ok(new_pane_id)
    }

    fn handle_send(
        &mut self,
        target: &PaneRef,
        data: &[u8],
        append_enter: bool,
    ) -> std::result::Result<(), String> {
        let pane_id = self
            .ws()
            .resolve_pane_ref(target)
            .ok_or_else(|| format!("pane not found: {target:?}"))?;
        let pane = self
            .ws_mut()
            .panes
            .get_mut(&pane_id)
            .ok_or_else(|| "pane vanished".to_string())?;
        pane.write_input(data).map_err(|e| e.to_string())?;
        if append_enter {
            pane.write_input(b"\r").map_err(|e| e.to_string())?;
        }
        self.dirty = true;
        Ok(())
    }

    fn handle_focus(&mut self, target: &PaneRef) -> std::result::Result<(), String> {
        let pane_id = self
            .ws()
            .resolve_pane_ref(target)
            .ok_or_else(|| format!("pane not found: {target:?}"))?;
        let ws = self.ws_mut();
        ws.focused_pane_id = pane_id;
        ws.focus_target = FocusTarget::Pane;
        self.dirty = true;
        Ok(())
    }

    fn handle_split(
        &mut self,
        target: &PaneRef,
        direction: ipc::Direction,
        command: Option<String>,
        name: Option<String>,
    ) -> std::result::Result<usize, String> {
        let target_pane_id = self
            .ws()
            .resolve_pane_ref(target)
            .ok_or_else(|| format!("pane not found: {target:?}"))?;
        let prev_focus = self.ws().focused_pane_id;
        self.ws_mut().focused_pane_id = target_pane_id;
        let split_dir = match direction {
            ipc::Direction::Vertical => SplitDirection::Vertical,
            ipc::Direction::Horizontal => SplitDirection::Horizontal,
        };
        self.split_focused_pane(split_dir)
            .map_err(|e| e.to_string())?;
        let new_pane_id = self.ws().focused_pane_id;
        // split_focused_pane is a no-op (keeps focus) when the pane is
        // too small or the workspace is already at MAX_PANES; detect
        // that by checking whether focus actually advanced.
        if new_pane_id == target_pane_id {
            self.ws_mut().focused_pane_id = prev_focus;
            return Err("split refused (max panes reached or pane too small)".to_string());
        }
        if let Some(cmd) = command {
            if let Some(pane) = self.ws_mut().panes.get_mut(&new_pane_id) {
                pane.queue_startup_command(&cmd);
            }
        }
        if let Some(name) = name {
            if !name.is_empty() {
                self.ws_mut().pane_names.insert(name, new_pane_id);
            }
        }
        Ok(new_pane_id)
    }
}

/// Extract directory name from a path for tab title.
fn dir_name(path: &std::path::Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| path.to_string_lossy().to_string())
}

/// Extract text from a pane's vt100 screen within a selection range.
fn extract_selected_text(pane: &Pane, sr: u32, sc: u32, er: u32, ec: u32) -> String {
    let parser = pane.parser.lock().unwrap_or_else(|e| e.into_inner());
    let screen = parser.screen();
    let mut lines = Vec::new();

    for row in sr..=er {
        let mut line = String::new();
        let col_start = if row == sr { sc } else { 0 };
        let col_end = if row == er { ec } else { 999 };

        for col in col_start..=col_end {
            if let Some(cell) = screen.cell(row as u16, col as u16) {
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

/// Extract text from the file preview within a selection range.
/// `sr`/`er` are absolute line indices; `sc`/`ec` are char offsets
/// within the line (selection is stored in source coordinates so it
/// survives scrolling). Trailing empty lines are stripped.
fn extract_preview_selected_text(
    preview: &crate::preview::Preview,
    sr: u32,
    sc: u32,
    er: u32,
    ec: u32,
) -> String {
    let lines = &preview.lines;
    let mut out: Vec<String> = Vec::new();

    for abs_row in sr..=er {
        let idx = abs_row as usize;
        if idx >= lines.len() {
            break;
        }
        let line = &lines[idx];
        let chars: Vec<char> = line.chars().collect();

        let col_start = if abs_row == sr { sc as usize } else { 0 };
        let col_end_inclusive = if abs_row == er {
            ec as usize
        } else {
            chars.len().saturating_sub(1)
        };

        let start = col_start.min(chars.len());
        let end = (col_end_inclusive.saturating_add(1)).min(chars.len());
        let slice: String = if start < end {
            chars[start..end].iter().collect()
        } else {
            String::new()
        };
        out.push(slice);
    }

    // Strip trailing empty lines only.
    while out.last().map_or(false, |l| l.is_empty()) {
        out.pop();
    }

    out.join("\n")
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
                let ctrl_byte = (c.to_ascii_lowercase() as u8)
                    .wrapping_sub(b'a')
                    .wrapping_add(1);
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

    // ─── resolve_pane_ref_impl (Phase 3 Step 3.2) ────────────

    fn mk_ids(ids: &[usize]) -> HashSet<usize> {
        ids.iter().copied().collect()
    }

    #[test]
    fn resolve_focused_returns_focused_id_when_known() {
        let names = HashMap::new();
        let ids = mk_ids(&[1, 2, 3]);
        assert_eq!(
            resolve_pane_ref_impl(&PaneRef::Focused, &names, &ids, 2),
            Some(2)
        );
    }

    #[test]
    fn resolve_focused_returns_none_when_focus_stale() {
        let names = HashMap::new();
        let ids = mk_ids(&[1, 3]);
        assert_eq!(
            resolve_pane_ref_impl(&PaneRef::Focused, &names, &ids, 2),
            None
        );
    }

    #[test]
    fn resolve_by_id_returns_id_when_known() {
        let names = HashMap::new();
        let ids = mk_ids(&[1, 2, 3]);
        assert_eq!(
            resolve_pane_ref_impl(&PaneRef::Id(3), &names, &ids, 1),
            Some(3)
        );
    }

    #[test]
    fn resolve_by_id_returns_none_when_unknown() {
        let names = HashMap::new();
        let ids = mk_ids(&[1, 2]);
        assert_eq!(
            resolve_pane_ref_impl(&PaneRef::Id(99), &names, &ids, 1),
            None
        );
    }

    #[test]
    fn resolve_by_name_returns_id_when_registered() {
        let mut names = HashMap::new();
        names.insert("engineering".to_string(), 7);
        let ids = mk_ids(&[1, 7]);
        assert_eq!(
            resolve_pane_ref_impl(&PaneRef::Name("engineering".into()), &names, &ids, 1),
            Some(7)
        );
    }

    #[test]
    fn resolve_by_name_returns_none_when_unregistered() {
        let names = HashMap::new();
        let ids = mk_ids(&[1, 7]);
        assert_eq!(
            resolve_pane_ref_impl(&PaneRef::Name("missing".into()), &names, &ids, 1),
            None
        );
    }

    #[test]
    fn resolve_by_name_returns_none_when_pane_closed() {
        // Name still registered but the pane has been removed — the
        // dangling entry must not resolve to a ghost id.
        let mut names = HashMap::new();
        names.insert("engineering".to_string(), 7);
        let ids = mk_ids(&[1]); // 7 has been closed
        assert_eq!(
            resolve_pane_ref_impl(&PaneRef::Name("engineering".into()), &names, &ids, 1),
            None
        );
    }

    // ─── apply_layout integration (Phase 2 review fix) ────────
    //
    // These tests spawn real PTYs through App::new / Pane::new_with_cwd
    // because apply_layout drives split_focused_pane, which unavoidably
    // creates child shell processes. On a dev machine this costs a few
    // milliseconds per pane; in CI it's measurable but acceptable for a
    // handful of tests. Each test calls `app.shutdown()` at the end so
    // the spawned shells don't linger.

    fn make_pane_spec(id: &str) -> crate::layout_config::LayoutNodeSpec {
        crate::layout_config::LayoutNodeSpec::Pane {
            id: id.to_string(),
            command: None,
        }
    }

    #[test]
    fn apply_layout_maps_split_first_to_target_and_second_to_new() {
        // Given a 2-pane Split spec, after apply_layout:
        // - pane_names["left"]  must point at the workspace's original
        //   pane (what we split off)
        // - pane_names["right"] must point at the freshly-created pane
        // - the LayoutNode tree must have Leaf(left) in `first` and
        //   Leaf(right) in `second`
        // Regression guard for the reviewer's concern that the
        // first/second recursion arms in apply_layout_node might drift.
        let cfg = crate::layout_config::LayoutConfig {
            version: 1,
            name: "test".into(),
            root: crate::layout_config::LayoutNodeSpec::Split {
                direction: crate::layout_config::DirectionSpec::Vertical,
                ratio: 0.5,
                first: Box::new(make_pane_spec("left")),
                second: Box::new(make_pane_spec("right")),
            },
        };

        let mut app = App::new(40, 80).expect("App::new");
        let initial_pane_id = app.ws().focused_pane_id;

        app.apply_layout(&cfg).expect("apply_layout");

        let ws = app.ws();
        let left_id = *ws
            .pane_names
            .get("left")
            .expect("pane_names[left] should be registered");
        let right_id = *ws
            .pane_names
            .get("right")
            .expect("pane_names[right] should be registered");

        assert_eq!(
            left_id, initial_pane_id,
            "`first` spec ('left') must map to the original/split-target pane"
        );
        assert_ne!(
            right_id, initial_pane_id,
            "`second` spec ('right') must map to a newly-spawned pane"
        );

        match &ws.layout {
            LayoutNode::Split { first, second, .. } => {
                match first.as_ref() {
                    LayoutNode::Leaf { pane_id } => {
                        assert_eq!(*pane_id, left_id, "layout.first must be the 'left' pane")
                    }
                    other => panic!("expected Leaf in first, got {other:?}"),
                }
                match second.as_ref() {
                    LayoutNode::Leaf { pane_id } => {
                        assert_eq!(*pane_id, right_id, "layout.second must be the 'right' pane")
                    }
                    other => panic!("expected Leaf in second, got {other:?}"),
                }
            }
            other => panic!("expected Split at root, got {other:?}"),
        }

        app.shutdown();
    }

    #[test]
    fn apply_layout_nested_split_preserves_positions() {
        // A right-heavy tree: Split { first: "A", second: Split { "B", "C" } }.
        // After apply, the LayoutNode must mirror that shape exactly.
        let cfg = crate::layout_config::LayoutConfig {
            version: 1,
            name: "nested".into(),
            root: crate::layout_config::LayoutNodeSpec::Split {
                direction: crate::layout_config::DirectionSpec::Vertical,
                ratio: 0.5,
                first: Box::new(make_pane_spec("A")),
                second: Box::new(crate::layout_config::LayoutNodeSpec::Split {
                    direction: crate::layout_config::DirectionSpec::Horizontal,
                    ratio: 0.5,
                    first: Box::new(make_pane_spec("B")),
                    second: Box::new(make_pane_spec("C")),
                }),
            },
        };

        let mut app = App::new(40, 80).expect("App::new");
        app.apply_layout(&cfg).expect("apply_layout");

        let ws = app.ws();
        let a_id = *ws.pane_names.get("A").expect("A registered");
        let b_id = *ws.pane_names.get("B").expect("B registered");
        let c_id = *ws.pane_names.get("C").expect("C registered");

        match &ws.layout {
            LayoutNode::Split {
                first: outer_first,
                second: outer_second,
                ..
            } => {
                match outer_first.as_ref() {
                    LayoutNode::Leaf { pane_id } => assert_eq!(*pane_id, a_id),
                    other => panic!("outer.first must be Leaf(A), got {other:?}"),
                }
                match outer_second.as_ref() {
                    LayoutNode::Split {
                        first: inner_first,
                        second: inner_second,
                        ..
                    } => {
                        match inner_first.as_ref() {
                            LayoutNode::Leaf { pane_id } => assert_eq!(*pane_id, b_id),
                            other => panic!("inner.first must be Leaf(B), got {other:?}"),
                        }
                        match inner_second.as_ref() {
                            LayoutNode::Leaf { pane_id } => assert_eq!(*pane_id, c_id),
                            other => panic!("inner.second must be Leaf(C), got {other:?}"),
                        }
                    }
                    other => panic!("outer.second must be a Split, got {other:?}"),
                }
            }
            other => panic!("expected Split at root, got {other:?}"),
        }

        app.shutdown();
    }

    #[test]
    fn app_command_channel_sends_and_receives() {
        // Smoke-test that the AppCommand channel round-trips without
        // panicking. We can't exercise handle_* without spawning PTYs,
        // but confirming the types fit together catches breakage.
        let (tx, rx) = mpsc::channel::<AppCommand>();
        let (reply_tx, reply_rx) = oneshot::channel();
        tx.send(AppCommand::List { reply: reply_tx }).unwrap();
        match rx.try_recv() {
            Ok(AppCommand::List { reply }) => {
                reply.send(Vec::new()).unwrap();
                let list = reply_rx.recv().unwrap();
                assert!(list.is_empty());
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }
}
