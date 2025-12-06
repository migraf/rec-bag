//! Folder tree model for browsing audio files in a hierarchical structure.

use std::path::{Path, PathBuf};

/// Represents a node in the folder tree.
#[derive(Debug, Clone, PartialEq)]
pub struct FolderNode {
    /// The name of this folder (just the folder name, not full path)
    pub name: String,
    /// Full path to this folder
    pub path: PathBuf,
    /// Number of audio files directly in this folder (not including subfolders)
    pub file_count: usize,
    /// Total number of audio files in this folder and all subfolders
    pub total_file_count: usize,
    /// Child folders
    pub children: Vec<FolderNode>,
}

impl FolderNode {
    /// Create a new folder node.
    pub fn new(name: impl Into<String>, path: PathBuf) -> Self {
        Self {
            name: name.into(),
            path,
            file_count: 0,
            total_file_count: 0,
            children: Vec::new(),
        }
    }

    /// Check if this folder has any subfolders.
    pub fn has_children(&self) -> bool {
        !self.children.is_empty()
    }

    /// Check if this folder contains any audio files (directly or in subfolders).
    pub fn has_audio_files(&self) -> bool {
        self.total_file_count > 0
    }
}

/// Audio file extensions that we recognize.
const AUDIO_EXTENSIONS: &[&str] = &["mp3", "wav", "flac", "ogg", "m4a", "aac"];

/// Check if a file extension is a supported audio format.
pub fn is_audio_extension(ext: &str) -> bool {
    AUDIO_EXTENSIONS.contains(&ext.to_lowercase().as_str())
}

/// Build a folder tree from a root path, only including folders that contain audio files.
pub fn build_folder_tree(root_path: &Path) -> Option<FolderNode> {
    if !root_path.is_dir() {
        return None;
    }

    let name = root_path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("Music")
        .to_string();

    let mut root = FolderNode::new(name, root_path.to_path_buf());
    build_tree_recursive(&mut root);

    // Only return the tree if it contains audio files
    if root.has_audio_files() {
        Some(root)
    } else {
        None
    }
}

/// Recursively build the folder tree.
fn build_tree_recursive(node: &mut FolderNode) {
    let Ok(entries) = std::fs::read_dir(&node.path) else {
        return;
    };

    let mut child_folders = Vec::new();

    for entry in entries.flatten() {
        let path = entry.path();

        if path.is_dir() {
            // Skip hidden directories
            let name = path
                .file_name()
                .and_then(|n| n.to_str())
                .map(|s| s.to_string());

            if let Some(name) = name {
                if !name.starts_with('.') {
                    let mut child = FolderNode::new(name, path);
                    build_tree_recursive(&mut child);
                    // Only include folders that contain audio files
                    if child.has_audio_files() {
                        child_folders.push(child);
                    }
                }
            }
        } else if path.is_file() {
            // Check if it's an audio file
            if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
                if is_audio_extension(ext) {
                    node.file_count += 1;
                }
            }
        }
    }

    // Sort children alphabetically
    child_folders.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));

    // Calculate total file count
    node.total_file_count = node.file_count
        + child_folders
            .iter()
            .map(|c| c.total_file_count)
            .sum::<usize>();

    node.children = child_folders;
}

/// Get all audio files from a specific folder path (not recursive - just the folder itself).
pub fn get_audio_files_in_folder(folder_path: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(folder_path) else {
        return Vec::new();
    };

    let mut files = Vec::new();

    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_file() {
            if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
                if is_audio_extension(ext) {
                    files.push(path);
                }
            }
        }
    }

    // Sort files alphabetically
    files.sort();
    files
}

/// Get all audio files from a folder and all its subfolders recursively.
pub fn get_all_audio_files_recursive(folder_path: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    collect_audio_files_recursive(folder_path, &mut files);
    files.sort();
    files
}

fn collect_audio_files_recursive(path: &Path, files: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(path) else {
        return;
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_audio_files_recursive(&path, files);
        } else if path.is_file() {
            if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
                if is_audio_extension(ext) {
                    files.push(path);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_audio_extension() {
        assert!(is_audio_extension("mp3"));
        assert!(is_audio_extension("MP3"));
        assert!(is_audio_extension("flac"));
        assert!(!is_audio_extension("txt"));
        assert!(!is_audio_extension("jpg"));
    }
}

