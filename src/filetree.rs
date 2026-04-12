use std::fs;
use std::path::{Path, PathBuf};

/// A node in the file tree.
#[derive(Debug, Clone)]
pub struct FileEntry {
    pub name: String,
    pub path: PathBuf,
    pub is_dir: bool,
    pub is_expanded: bool,
    pub children: Vec<FileEntry>,
    pub depth: usize,
}

impl FileEntry {
    /// Build a file tree from a directory path.
    pub fn from_dir(path: &Path, depth: usize, max_depth: usize) -> Option<Self> {
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| path.to_string_lossy().to_string());

        if !path.is_dir() {
            return Some(Self {
                name,
                path: path.to_path_buf(),
                is_dir: false,
                is_expanded: false,
                children: Vec::new(),
                depth,
            });
        }

        let children = if depth < max_depth {
            scan_directory(path, depth + 1, max_depth)
        } else {
            Vec::new()
        };

        Some(Self {
            name,
            path: path.to_path_buf(),
            is_dir: true,
            is_expanded: false,
            children,
            depth,
        })
    }
}

/// Scan a directory and return sorted entries (dirs first, then files, alphabetical).
/// Maximum entries per directory to prevent DoS from huge directories.
const MAX_ENTRIES_PER_DIR: usize = 500;

fn scan_directory(path: &Path, depth: usize, max_depth: usize) -> Vec<FileEntry> {
    let entries = match fs::read_dir(path) {
        Ok(entries) => entries,
        Err(_) => return Vec::new(),
    };

    let mut dirs = Vec::new();
    let mut files = Vec::new();
    let mut count = 0;

    for entry in entries.flatten() {
        if count >= MAX_ENTRIES_PER_DIR {
            break;
        }

        let entry_path = entry.path();
        let name = entry
            .file_name()
            .to_string_lossy()
            .to_string();

        // Skip hidden files/directories
        if name.starts_with('.') {
            continue;
        }

        // Skip symlinks to prevent traversal outside the project
        if entry_path.symlink_metadata().map_or(true, |m| m.is_symlink()) {
            continue;
        }

        if let Some(file_entry) = FileEntry::from_dir(&entry_path, depth, max_depth) {
            if file_entry.is_dir {
                dirs.push(file_entry);
            } else {
                files.push(file_entry);
            }
        }

        count += 1;
    }

    // Sort alphabetically (case-insensitive)
    dirs.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
    files.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));

    // Directories first, then files
    dirs.extend(files);
    dirs
}

/// File tree state for the sidebar.
#[allow(dead_code)]
pub struct FileTree {
    pub root_path: PathBuf,
    pub entries: Vec<FileEntry>,
    pub selected_index: usize,
    pub scroll_offset: usize,
    /// Flattened list of visible entries for rendering.
    flat_entries: Vec<FlatEntry>,
}

/// A flattened entry for rendering.
#[derive(Debug, Clone)]
pub struct FlatEntry {
    pub name: String,
    pub path: PathBuf,
    pub is_dir: bool,
    pub is_expanded: bool,
    pub depth: usize,
}

impl FileTree {
    /// Create a new file tree from a directory.
    pub fn new(root_path: PathBuf) -> Self {
        let entries = scan_directory(&root_path, 0, 5);
        let mut tree = Self {
            root_path,
            entries,
            selected_index: 0,
            scroll_offset: 0,
            flat_entries: Vec::new(),
        };
        tree.rebuild_flat();
        tree
    }

    /// Rebuild the flattened entry list from the tree structure.
    fn rebuild_flat(&mut self) {
        self.flat_entries.clear();
        for entry in &self.entries {
            Self::flatten_entry(entry, &mut self.flat_entries);
        }
    }

    fn flatten_entry(entry: &FileEntry, flat: &mut Vec<FlatEntry>) {
        flat.push(FlatEntry {
            name: entry.name.clone(),
            path: entry.path.clone(),
            is_dir: entry.is_dir,
            is_expanded: entry.is_expanded,
            depth: entry.depth,
        });

        if entry.is_dir && entry.is_expanded {
            for child in &entry.children {
                Self::flatten_entry(child, flat);
            }
        }
    }

    /// Get the flattened list of visible entries.
    pub fn visible_entries(&self) -> &[FlatEntry] {
        &self.flat_entries
    }

    /// Get the currently selected entry.
    #[allow(dead_code)]
    pub fn selected_entry(&self) -> Option<&FlatEntry> {
        self.flat_entries.get(self.selected_index)
    }

    /// Move selection up.
    pub fn move_up(&mut self) {
        if self.selected_index > 0 {
            self.selected_index -= 1;
        }
    }

    /// Move selection down.
    pub fn move_down(&mut self) {
        if self.selected_index + 1 < self.flat_entries.len() {
            self.selected_index += 1;
        }
    }

    /// Handle Enter key on selected entry.
    /// Returns Some(path) if a file was selected for preview.
    pub fn toggle_or_select(&mut self) -> Option<PathBuf> {
        if let Some(flat_entry) = self.flat_entries.get(self.selected_index).cloned() {
            if flat_entry.is_dir {
                // Toggle expand/collapse
                self.toggle_dir(&flat_entry.path);
                self.rebuild_flat();
                None
            } else {
                // Return file path for preview
                Some(flat_entry.path)
            }
        } else {
            None
        }
    }

    /// Toggle a directory's expanded state.
    fn toggle_dir(&mut self, path: &Path) {
        for entry in &mut self.entries {
            if Self::toggle_dir_recursive(entry, path) {
                return;
            }
        }
    }

    fn toggle_dir_recursive(entry: &mut FileEntry, path: &Path) -> bool {
        if entry.path == path && entry.is_dir {
            entry.is_expanded = !entry.is_expanded;
            // Load children if first expansion and children are empty
            if entry.is_expanded && entry.children.is_empty() {
                entry.children = scan_directory(&entry.path, entry.depth + 1, 5);
            }
            return true;
        }
        for child in &mut entry.children {
            if Self::toggle_dir_recursive(child, path) {
                return true;
            }
        }
        false
    }

    /// Adjust scroll offset to keep selected item visible.
    pub fn ensure_visible(&mut self, visible_height: usize) {
        if visible_height == 0 {
            return;
        }
        if self.selected_index < self.scroll_offset {
            self.scroll_offset = self.selected_index;
        }
        if self.selected_index >= self.scroll_offset + visible_height {
            self.scroll_offset = self.selected_index - visible_height + 1;
        }
    }

    /// Scroll up by amount.
    pub fn scroll_up(&mut self, amount: usize) {
        self.scroll_offset = self.scroll_offset.saturating_sub(amount);
    }

    /// Scroll down by amount.
    pub fn scroll_down(&mut self, amount: usize) {
        let max_offset = self.flat_entries.len().saturating_sub(1);
        self.scroll_offset = (self.scroll_offset + amount).min(max_offset);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_scan_directory_skips_hidden() {
        // Use the current project directory as test target
        let entries = scan_directory(Path::new("."), 0, 1);
        for entry in &entries {
            assert!(
                !entry.name.starts_with('.'),
                "Hidden entry should be skipped: {}",
                entry.name
            );
        }
    }

    #[test]
    fn test_scan_directory_dirs_before_files() {
        let entries = scan_directory(Path::new("."), 0, 1);
        let mut seen_file = false;
        for entry in &entries {
            if !entry.is_dir {
                seen_file = true;
            }
            if entry.is_dir && seen_file {
                panic!("Directory {} found after files", entry.name);
            }
        }
    }

    #[test]
    fn test_file_tree_navigation() {
        let mut tree = FileTree::new(PathBuf::from("."));
        let initial = tree.selected_index;
        assert_eq!(initial, 0);

        if tree.visible_entries().len() > 1 {
            tree.move_down();
            assert_eq!(tree.selected_index, 1);
            tree.move_up();
            assert_eq!(tree.selected_index, 0);
        }

        // Moving up at 0 should stay at 0
        tree.move_up();
        assert_eq!(tree.selected_index, 0);
    }
}
