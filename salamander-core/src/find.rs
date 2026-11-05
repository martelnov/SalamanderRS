//! GTK-free recursive file and content search engine.
use std::fs::{self, File};
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use crate::model::Mask;

#[derive(Clone, Debug)]
pub struct Query {
    pub root: PathBuf,
    pub mask: Mask,
    /// Content to search for; empty means name-only search.
    pub content: Vec<u8>,
    pub case_sensitive: bool,
    pub include_hidden: bool,
    pub include_dirs: bool,
}

pub enum FindEvent {
    Directory(PathBuf),
    Found(PathBuf),
    Finished { found: u64, errors: u64, cancelled: bool },
}

/// Walks the tree without following symlinks. `on_event` returning false stops the search.
pub fn search(query: &Query, cancel: &AtomicBool, mut on_event: impl FnMut(FindEvent) -> bool) {
    let needle = if query.case_sensitive {
        query.content.clone()
    } else {
        query.content.to_ascii_lowercase()
    };
    let mut buffer = vec![0u8; 256 * 1024];
    let mut stack = vec![query.root.clone()];
    let (mut found, mut errors) = (0u64, 0u64);
    let mut stopped = false;

    'walk: while let Some(dir) = stack.pop() {
        if cancel.load(Ordering::Relaxed) || !on_event(FindEvent::Directory(dir.clone())) {
            stopped = true;
            break;
        }
        let entries = match fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(_) => {
                errors += 1;
                continue;
            }
        };
        let mut subdirs = Vec::new();
        for entry in entries {
            if cancel.load(Ordering::Relaxed) {
                stopped = true;
                break 'walk;
            }
            let Ok(entry) = entry else {
                errors += 1;
                continue;
            };
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if !query.include_hidden && name.starts_with('.') {
                continue;
            }
            let Ok(file_type) = entry.file_type() else {
                errors += 1;
                continue;
            };
            let path = entry.path();
            if file_type.is_dir() {
                subdirs.push(path.clone());
            }
            if !query.mask.matches(&name) {
                continue;
            }
            let matched = if file_type.is_dir() {
                query.include_dirs && needle.is_empty()
            } else if needle.is_empty() {
                true
            } else if file_type.is_file() {
                match contains(&path, &needle, query.case_sensitive, &mut buffer, cancel) {
                    Ok(found) => found,
                    Err(_) => {
                        errors += 1;
                        false
                    }
                }
            } else {
                false
            };
            if matched {
                found += 1;
                if !on_event(FindEvent::Found(path)) {
                    stopped = true;
                    break 'walk;
                }
            }
        }
        // Reverse so that the stack pops subdirectories in directory order.
        subdirs.reverse();
        stack.extend(subdirs);
    }
    let cancelled = stopped || cancel.load(Ordering::Relaxed);
    on_event(FindEvent::Finished {
        found,
        errors,
        cancelled,
    });
}

/// Streams the file looking for `needle` (already lower-cased when case-insensitive).
fn contains(
    path: &Path,
    needle: &[u8],
    case_sensitive: bool,
    buffer: &mut [u8],
    cancel: &AtomicBool,
) -> io::Result<bool> {
    let mut file = File::open(path)?;
    let overlap = needle.len().saturating_sub(1);
    let mut window: Vec<u8> = Vec::with_capacity(buffer.len() + overlap);
    loop {
        if cancel.load(Ordering::Relaxed) {
            return Ok(false);
        }
        let read = file.read(buffer)?;
        if read == 0 {
            return Ok(false);
        }
        let chunk = &mut buffer[..read];
        if !case_sensitive {
            chunk.make_ascii_lowercase();
        }
        window.extend_from_slice(chunk);
        if window.windows(needle.len()).any(|candidate| candidate == needle) {
            return Ok(true);
        }
        let keep = window.len().min(overlap);
        window.drain(..window.len() - keep);
    }
}


#[cfg(test)]
mod tests {
    use super::*;

    fn run(query: &Query) -> Vec<PathBuf> {
        let mut found = Vec::new();
        search(query, &AtomicBool::new(false), |event| {
            if let FindEvent::Found(path) = event {
                found.push(path);
            }
            true
        });
        found.sort();
        found
    }

    fn query(root: &Path, mask: &str, content: &str) -> Query {
        Query {
            root: root.to_path_buf(),
            mask: Mask::parse(mask),
            content: content.as_bytes().to_vec(),
            case_sensitive: false,
            include_hidden: false,
            include_dirs: true,
        }
    }

    #[test]
    fn finds_by_mask_recursively_skipping_hidden() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        fs::create_dir_all(root.join("a/b")).unwrap();
        fs::create_dir_all(root.join(".git")).unwrap();
        fs::write(root.join("a/one.rs"), "").unwrap();
        fs::write(root.join("a/b/two.RS"), "").unwrap();
        fs::write(root.join(".git/three.rs"), "").unwrap();
        fs::write(root.join("a/readme.md"), "").unwrap();

        assert_eq!(
            run(&query(root, "*.rs", "")),
            vec![root.join("a/b/two.RS"), root.join("a/one.rs")]
        );
        let mut with_hidden = query(root, "*.rs", "");
        with_hidden.include_hidden = true;
        assert_eq!(run(&with_hidden).len(), 3);
    }

    #[test]
    fn content_search_handles_case_and_chunk_boundaries() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let mut big = vec![b'x'; 256 * 1024 - 3];
        big.extend_from_slice(b"NEEDLE");
        fs::write(root.join("big.bin"), &big).unwrap();
        fs::write(root.join("small.txt"), "a needle here").unwrap();
        fs::write(root.join("none.txt"), "nothing").unwrap();

        assert_eq!(
            run(&query(root, "*", "needle")),
            vec![root.join("big.bin"), root.join("small.txt")]
        );
        let mut sensitive = query(root, "*", "needle");
        sensitive.case_sensitive = true;
        assert_eq!(run(&sensitive), vec![root.join("small.txt")]);
    }

    #[test]
    fn reports_finish_once_and_honours_stop() {
        let temp = tempfile::tempdir().unwrap();
        for index in 0..5 {
            fs::write(temp.path().join(format!("f{index}")), "").unwrap();
        }
        let mut found = 0;
        let mut finished = 0;
        search(
            &query(temp.path(), "*", ""),
            &AtomicBool::new(false),
            |event| match event {
                FindEvent::Found(_) => {
                    found += 1;
                    found < 2
                }
                FindEvent::Finished { cancelled, .. } => {
                    assert!(cancelled);
                    finished += 1;
                    true
                }
                FindEvent::Directory(_) => true,
            },
        );
        assert_eq!(found, 2);
        assert_eq!(finished, 1);
    }
}
