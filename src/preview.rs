use std::fs::File;
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};

const MAX_PREVIEW_LINES: usize = 500;
const BINARY_CHECK_BYTES: usize = 8192;
const MAX_FILE_SIZE: u64 = 10 * 1024 * 1024; // 10MB

/// File preview state.
pub struct Preview {
    pub file_path: Option<PathBuf>,
    pub lines: Vec<String>,
    pub scroll_offset: usize,
    pub is_binary: bool,
}

impl Preview {
    pub fn new() -> Self {
        Self {
            file_path: None,
            lines: Vec::new(),
            scroll_offset: 0,
            is_binary: false,
        }
    }

    /// Load a file for preview.
    pub fn load(&mut self, path: &Path) {
        // Don't reload the same file
        if self.file_path.as_deref() == Some(path) {
            return;
        }

        self.file_path = Some(path.to_path_buf());
        self.scroll_offset = 0;
        self.lines.clear();
        self.is_binary = false;

        // Check file size first to avoid OOM
        let metadata = match std::fs::metadata(path) {
            Ok(m) => m,
            Err(_) => {
                self.lines = vec!["ファイルを読み込めませんでした".to_string()];
                return;
            }
        };

        if !metadata.is_file() {
            self.lines = vec!["通常ファイルではありません".to_string()];
            return;
        }

        if metadata.len() > MAX_FILE_SIZE {
            self.lines = vec![format!(
                "ファイルが大きすぎます（{:.1}MB > {:.0}MB）",
                metadata.len() as f64 / 1024.0 / 1024.0,
                MAX_FILE_SIZE as f64 / 1024.0 / 1024.0
            )];
            return;
        }

        // Check if file is binary (read only first N bytes)
        if is_binary_file(path) {
            self.is_binary = true;
            return;
        }

        // Read text file line-by-line (bounded)
        match File::open(path) {
            Ok(file) => {
                let reader = BufReader::new(file);
                self.lines = reader
                    .lines()
                    .take(MAX_PREVIEW_LINES)
                    .filter_map(|l| l.ok())
                    .collect();
            }
            Err(_) => {
                self.lines = vec!["ファイルを読み込めませんでした".to_string()];
            }
        }
    }

    /// Close the preview.
    pub fn close(&mut self) {
        self.file_path = None;
        self.lines.clear();
        self.scroll_offset = 0;
        self.is_binary = false;
    }

    /// Check if preview is active.
    pub fn is_active(&self) -> bool {
        self.file_path.is_some()
    }

    /// Get the filename for display.
    pub fn filename(&self) -> String {
        self.file_path
            .as_ref()
            .and_then(|p| p.file_name())
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default()
    }

    /// Scroll up by amount.
    pub fn scroll_up(&mut self, amount: usize) {
        self.scroll_offset = self.scroll_offset.saturating_sub(amount);
    }

    /// Scroll down by amount.
    pub fn scroll_down(&mut self, amount: usize) {
        let max_offset = self.lines.len().saturating_sub(1);
        self.scroll_offset = (self.scroll_offset + amount).min(max_offset);
    }
}

/// Check if a file is likely binary by reading only the first N bytes.
fn is_binary_file(path: &Path) -> bool {
    let file = match File::open(path) {
        Ok(f) => f,
        Err(_) => return false,
    };
    let mut reader = BufReader::new(file);
    let mut buf = [0u8; BINARY_CHECK_BYTES];
    match reader.read(&mut buf) {
        Ok(n) => buf[..n].contains(&0),
        Err(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_preview_initial_state() {
        let preview = Preview::new();
        assert!(!preview.is_active());
        assert!(preview.lines.is_empty());
    }

    #[test]
    fn test_preview_load_text_file() {
        let mut preview = Preview::new();
        preview.load(Path::new("Cargo.toml"));
        assert!(preview.is_active());
        assert!(!preview.is_binary);
        assert!(!preview.lines.is_empty());
    }

    #[test]
    fn test_preview_close() {
        let mut preview = Preview::new();
        preview.load(Path::new("Cargo.toml"));
        assert!(preview.is_active());

        preview.close();
        assert!(!preview.is_active());
        assert!(preview.lines.is_empty());
    }

    #[test]
    fn test_preview_scroll() {
        let mut preview = Preview::new();
        preview.lines = (0..100).map(|i| format!("line {}", i)).collect();
        preview.scroll_down(10);
        assert_eq!(preview.scroll_offset, 10);
        preview.scroll_up(5);
        assert_eq!(preview.scroll_offset, 5);
        preview.scroll_up(100);
        assert_eq!(preview.scroll_offset, 0);
    }
}
