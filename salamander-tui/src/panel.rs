//! Navigation model shared by the two terminal panels. All file metadata comes from
//! salamander-core, exactly as in the existing GTK application.

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

use salamander_core::archive;
use salamander_core::model::{self, DirectoryListing, EntryKind, FileEntry};
use salamander_core::remote;

pub struct Panel {
    pub path: PathBuf,
    pub entries: Vec<FileEntry>,
    pub cursor: usize,
    pub scroll: usize,
    pub marked: HashSet<PathBuf>,
    pub show_hidden: bool,
    pub last_local: PathBuf,
}

impl Panel {
    pub fn new(start: PathBuf, show_hidden: bool) -> Result<Self, String> {
        let fallback = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"));
        let mut panel = Self {
            path: fallback.clone(),
            entries: Vec::new(),
            cursor: 0,
            scroll: 0,
            marked: HashSet::new(),
            show_hidden,
            last_local: fallback.clone(),
        };
        if panel.change_dir(start).is_err() {
            panel.change_dir(fallback)?;
        }
        Ok(panel)
    }

    pub fn display_path(&self) -> String {
        if let Some((id, remote_path)) = remote::split_virtual(&self.path) {
            remote::pretty(&id, &remote_path)
        } else {
            self.path.to_string_lossy().into_owned()
        }
    }

    pub fn remote(&self) -> Option<(String, PathBuf)> {
        remote::split_virtual(&self.path)
    }

    pub fn archive(&self) -> Option<(PathBuf, PathBuf)> {
        if self.remote().is_none() {
            archive::split_virtual(&self.path)
        } else {
            None
        }
    }

    pub fn is_local(&self) -> bool {
        self.remote().is_none() && self.archive().is_none()
    }

    fn listing(path: &Path, show_hidden: bool) -> Result<DirectoryListing, String> {
        if let Some((id, dir)) = remote::split_virtual(path) {
            remote::read_directory(&id, &dir, show_hidden).map_err(|e| e.to_string())
        } else if let Some((file, dir)) = archive::split_virtual(path) {
            model::read_archive_directory(&file, &dir, show_hidden).map_err(|e| e.to_string())
        } else {
            model::read_directory(path, show_hidden).map_err(|e| e.to_string())
        }
    }

    pub fn change_dir(&mut self, requested: PathBuf) -> Result<(), String> {
        let path = if remote::split_virtual(&requested).is_some() {
            requested
        } else if requested.is_absolute() {
            fs::canonicalize(&requested).unwrap_or(requested)
        } else {
            let joined = self.path.join(&requested);
            fs::canonicalize(&joined).unwrap_or(joined)
        };
        self.load(path, None, false)
    }

    fn parent_of(&self, path: &Path) -> Option<PathBuf> {
        if let Some((id, dir)) = remote::split_virtual(path) {
            if dir == Path::new("/") {
                Some(self.last_local.clone())
            } else {
                Some(remote::virtual_path(&id, dir.parent().unwrap_or(Path::new("/"))))
            }
        } else {
            path.parent().map(Path::to_path_buf)
        }
    }

    fn load(&mut self, path: PathBuf, restore: Option<PathBuf>, same_dir: bool) -> Result<(), String> {
        let listing = Self::listing(&path, self.show_hidden)?;
        let mut entries = listing.entries;
        entries.sort_by(|a, b| {
            b.is_dir_like()
                .cmp(&a.is_dir_like())
                .then_with(|| a.folded_name.cmp(&b.folded_name))
                .then_with(|| a.name.cmp(&b.name))
        });
        if !same_dir && self.is_local() {
            self.last_local = self.path.clone();
        }
        if let Some(parent) = self.parent_of(&path) {
            entries.insert(0, FileEntry::parent_link(parent));
        }
        if !same_dir {
            self.marked.clear();
            self.cursor = 0;
            self.scroll = 0;
        }
        self.path = path;
        self.entries = entries;
        if let Some(focused) = restore {
            if let Some(index) = self.entries.iter().position(|entry| entry.path == focused) {
                self.cursor = index;
            }
        }
        self.cursor = self.cursor.min(self.entries.len().saturating_sub(1));
        self.marked.retain(|path| self.entries.iter().any(|entry| &entry.path == path));
        Ok(())
    }

    pub fn refresh(&mut self) -> Result<(), String> {
        let focused = self.current().map(|entry| entry.path.clone());
        self.load(self.path.clone(), focused, true)
    }

    pub fn current(&self) -> Option<&FileEntry> {
        self.entries.get(self.cursor)
    }

    pub fn move_cursor(&mut self, delta: isize, visible: usize) {
        let count = self.entries.len();
        if count == 0 {
            self.cursor = 0;
            return;
        }
        self.cursor = self.cursor.saturating_add_signed(delta).min(count - 1);
        self.ensure_visible(visible);
    }

    pub fn ensure_visible(&mut self, visible: usize) {
        let visible = visible.max(1);
        if self.cursor < self.scroll {
            self.scroll = self.cursor;
        } else if self.cursor >= self.scroll + visible {
            self.scroll = self.cursor + 1 - visible;
        }
    }

    pub fn toggle_mark(&mut self) {
        let item = self.current().filter(|entry| entry.kind != EntryKind::Parent).map(|entry| entry.path.clone());
        if let Some(path) = item {
            if !self.marked.insert(path.clone()) {
                self.marked.remove(&path);
            }
        }
    }

    pub fn selected_paths(&self) -> Vec<PathBuf> {
        if !self.marked.is_empty() {
            self.entries.iter()
                .filter(|entry| self.marked.contains(&entry.path) && !entry.is_parent())
                .map(|entry| entry.path.clone())
                .collect()
        } else {
            self.current()
                .filter(|entry| !entry.is_parent())
                .map(|entry| vec![entry.path.clone()])
                .unwrap_or_default()
        }
    }

    pub fn selected_count(&self) -> usize {
        self.marked.len()
    }

    pub fn go_parent(&mut self) -> Result<(), String> {
        if let Some(parent) = self.parent_of(&self.path) {
            // Keep the cursor on the archive or directory we just left.
            let leaving = self.path.clone();
            self.change_dir(parent)?;
            if let Some(index) = self.entries.iter().position(|entry| entry.path == leaving) {
                self.cursor = index;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn never_marks_parent_row() {
        let mut p = Panel::new(PathBuf::from("/"), false).unwrap();
        p.entries.insert(0, FileEntry::parent_link(PathBuf::from("/")));
        p.toggle_mark();
        assert_eq!(p.selected_count(), 0);
    }

    #[test]
    fn entering_a_directory_keeps_cursor_in_range() {
        let mut p = Panel::new(PathBuf::from("/tmp"), false).unwrap();
        p.move_cursor(1_000_000, 10);
        assert!(p.entries.is_empty() || p.cursor < p.entries.len());
        p.move_cursor(-1_000_000, 10);
        assert_eq!(p.cursor, 0);
    }
}
