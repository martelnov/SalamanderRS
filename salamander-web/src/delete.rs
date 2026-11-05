//! F8: descriptor-relative deletion for the confined web file manager.
//!
//! Never resolve the user's selected paths again by their absolute names.
//! Recursion is anchored to open directory descriptors and refuses to cross
//! submounts. This is not an atomic delete: another process with write access
//! to the same directory can still replace names between inspection/unlinkat.

use std::collections::HashSet;
use std::ffi::{CStr, CString};
use std::fs::File;
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::fs::MetadataExt;

use super::{transfer_http, valid_entry_name, ApiError};

const MAX_DEPTH: usize = 64;
const MAX_ENTRIES: usize = 100_000;

struct DeleteBudget {
    entries: usize,
}

struct DirectoryStream(*mut libc::DIR);
impl Drop for DirectoryStream {
    fn drop(&mut self) {
        // SAFETY: fdopendir produced an owned DIR*.
        unsafe { libc::closedir(self.0); }
    }
}

/// Delete the selected items, preserving the old API's partial-failure
/// reporting. Validate every requested name before deleting anything.
pub(super) fn delete_selected_beneath(
    root: &File,
    relative: &str,
    names: &[String],
) -> Result<(usize, Vec<String>), ApiError> {
    if names.is_empty() || names.len() > 128 {
        return Err(ApiError::bad_request("Označte 1 až 128 položek."));
    }
    let mut seen = HashSet::new();
    for name in names {
        if !valid_entry_name(name) || !seen.insert(name.as_str()) {
            return Err(ApiError::bad_request("Neplatný nebo duplicitní název položky."));
        }
    }
    let parent = transfer_http::open_relative_directory(root, relative)?;
    let mut budget = DeleteBudget { entries: names.len() };
    let mut errors = Vec::new();
    for name in names {
        // Names have been validated as single filename components without NUL.
        let name_c = CString::new(name.as_str())
            .map_err(|_| ApiError::bad_request("Neplatný název položky."))?;
        if let Err(error) = remove_entry(&parent, &name_c, 0, true, &mut budget) {
            errors.push(format!("{name}: {error}"));
        }
    }
    Ok((names.len(), errors))
}

/// Opens a *new* directory stream for a pinned directory. `try_clone()` would
/// share the original directory offset; `openat(".")` does not.
fn directory_names(directory: &File, budget: &mut DeleteBudget) -> io::Result<Vec<CString>> {
    // SAFETY: dot is a constant NUL-terminated component, and directory is a
    // valid open directory descriptor.
    let fd = unsafe {
        libc::openat(
            directory.as_raw_fd(), c".".as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC | libc::O_NOFOLLOW,
        )
    };
    if fd < 0 { return Err(io::Error::last_os_error()); }
    // SAFETY: fd is a new, owned descriptor; fdopendir takes ownership on success.
    let raw_dir = unsafe { libc::fdopendir(fd) };
    if raw_dir.is_null() {
        let error = io::Error::last_os_error();
        // SAFETY: fdopendir failed and did not take ownership.
        unsafe { libc::close(fd); }
        return Err(error);
    }
    let stream = DirectoryStream(raw_dir);
    let mut names = Vec::new();
    loop {
        // POSIX readdir returns NULL both for EOF and error; reset errno first.
        // SAFETY: Linux stores errno in thread-local storage.
        unsafe { *libc::__errno_location() = 0; }
        // SAFETY: stream is open until the guard is dropped.
        let entry = unsafe { libc::readdir(stream.0) };
        if entry.is_null() {
            let error = io::Error::last_os_error();
            if error.raw_os_error() == Some(0) { break; }
            return Err(error);
        }
        // SAFETY: readdir gives a NUL-terminated name valid until the next call.
        let name = unsafe { CStr::from_ptr((*entry).d_name.as_ptr()) };
        if name.to_bytes() == b"." || name.to_bytes() == b".." { continue; }
        if budget.entries >= MAX_ENTRIES {
            return Err(io::Error::other("Překročen limit položek pro jedno mazání."));
        }
        budget.entries += 1;
        names.push(name.to_owned());
    }
    Ok(names)
}

/// Compare the *opened* directory with its name in the parent before final
/// removal. This detects many directory substitutions, but does not remove the
/// unavoidable last name-to-inode race between this check and unlinkat.
fn check_directory_still_named(parent: &File, name: &CStr, opened: &File) -> io::Result<()> {
    let metadata = opened.metadata()?;
    let mut current = std::mem::MaybeUninit::<libc::stat>::uninit();
    // SAFETY: current has space for an initialized stat and name is NUL-ended.
    let rc = unsafe {
        libc::fstatat(parent.as_raw_fd(), name.as_ptr(), current.as_mut_ptr(), libc::AT_SYMLINK_NOFOLLOW)
    };
    if rc != 0 { return Err(io::Error::last_os_error()); }
    // SAFETY: fstatat succeeded and populated the struct.
    let current = unsafe { current.assume_init() };
    if current.st_dev as u64 != metadata.dev() || current.st_ino as u64 != metadata.ino()
        || (current.st_mode & libc::S_IFMT) != libc::S_IFDIR
    {
        return Err(io::Error::from_raw_os_error(libc::ESTALE));
    }
    Ok(())
}

fn unlink(parent: &File, name: &CStr, flags: i32) -> io::Result<()> {
    // SAFETY: parent is a pinned directory and name is one directory entry,
    // never a slash-containing pathname. unlinkat does not follow symlinks.
    let rc = unsafe { libc::unlinkat(parent.as_raw_fd(), name.as_ptr(), flags) };
    if rc < 0 { Err(io::Error::last_os_error()) } else { Ok(()) }
}

fn remove_entry(
    parent: &File, name: &CStr, depth: usize, top_level: bool,
    budget: &mut DeleteBudget,
) -> io::Result<()> {
    // Internal resumable-upload checkpoints are not user files. Refuse to
    // remove them, including when recursively deleting their parent.
    if name.to_bytes().starts_with(b".salamander-resume-") {
        return Err(io::Error::new(io::ErrorKind::PermissionDenied, "Interní soubor přenosu nelze smazat."));
    }
    let kind = transfer_http::existing_kind(parent, name)?
        .ok_or_else(|| io::Error::from_raw_os_error(libc::ENOENT))?;
    match kind {
        libc::S_IFREG => unlink(parent, name, 0),
        libc::S_IFLNK if !top_level => unlink(parent, name, 0),
        libc::S_IFDIR => {
            if depth >= MAX_DEPTH {
                return Err(io::Error::other("Překročen limit hloubky adresářů pro mazání."));
            }
            // openat2 NO_SYMLINKS|BENEATH|NO_XDEV: don't descend into
            // symlinks, path escapes or mounted filesystems inside a tree.
            let child = transfer_http::checked_openat2_no_xdev(
                parent.as_raw_fd(), name,
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC | libc::O_NOFOLLOW,
            )?;
            // Read before deleting: modifications during readdir are otherwise
            // unspecified, and the cap limits memory and denial-of-service.
            let children = directory_names(&child, budget)?;
            for child_name in children {
                remove_entry(&child, &child_name, depth + 1, false, budget)?;
            }
            check_directory_still_named(parent, name, &child)?;
            unlink(parent, name, libc::AT_REMOVEDIR)
        }
        _ => Err(io::Error::new(io::ErrorKind::PermissionDenied,
            "Mazat lze pouze běžné soubory a adresáře (uvnitř adresářů i odkazy).")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::symlink;

    #[test]
    fn recursive_delete_never_follows_nested_symlinks() {
        let tmp = tempfile::tempdir().unwrap();
        let data = tmp.path().join("data");
        let outside = tmp.path().join("outside");
        fs::create_dir(&data).unwrap();
        fs::create_dir(&outside).unwrap();
        fs::write(outside.join("keep.txt"), b"keep").unwrap();
        fs::create_dir_all(data.join("tree/inside")).unwrap();
        fs::write(data.join("tree/inside/remove.txt"), b"remove").unwrap();
        symlink(&outside, data.join("tree/inside/outside-link")).unwrap();
        let handle = File::open(&data).unwrap();
        let (count, errors) = delete_selected_beneath(&handle, "", &["tree".into()]).unwrap();
        assert_eq!(count, 1);
        assert!(errors.is_empty(), "{errors:?}");
        assert!(!data.join("tree").exists());
        assert_eq!(fs::read(outside.join("keep.txt")).unwrap(), b"keep");
    }

    #[test]
    fn refuses_traversal_and_top_level_symlinks() {
        let tmp = tempfile::tempdir().unwrap();
        let data = tmp.path().join("data");
        let outside = tmp.path().join("outside");
        fs::create_dir(&data).unwrap();
        fs::create_dir(&outside).unwrap();
        fs::write(outside.join("victim"), b"safe").unwrap();
        symlink(&outside, data.join("link")).unwrap();
        let handle = File::open(&data).unwrap();
        assert!(delete_selected_beneath(&handle, "../outside", &["victim".into()]).is_err());
        assert!(delete_selected_beneath(&handle, "link", &["victim".into()]).is_err());
        let (_, errors) = delete_selected_beneath(&handle, "", &["link".into()]).unwrap();
        assert_eq!(errors.len(), 1);
        assert!(data.join("link").is_symlink());
        assert_eq!(fs::read(outside.join("victim")).unwrap(), b"safe");
    }

    #[test]
    fn validates_entire_selection_before_deleting() {
        let tmp = tempfile::tempdir().unwrap();
        let data = tmp.path().join("data");
        fs::create_dir(&data).unwrap();
        fs::write(data.join("keep"), b"keep").unwrap();
        let handle = File::open(&data).unwrap();
        assert!(delete_selected_beneath(&handle, "", &["keep".into(), "../escape".into()]).is_err());
        assert!(delete_selected_beneath(&handle, "", &["keep".into(), "keep".into()]).is_err());
        assert_eq!(fs::read(data.join("keep")).unwrap(), b"keep");
        let (_, errors) = delete_selected_beneath(&handle, "", &["keep".into()]).unwrap();
        assert!(errors.is_empty(), "{errors:?}");
        assert!(!data.join("keep").exists());
    }

    #[test]
    fn refuses_internal_resumable_checkpoint() {
        let tmp = tempfile::tempdir().unwrap();
        let data = tmp.path().join("data");
        fs::create_dir_all(data.join("tree")).unwrap();
        fs::write(data.join("tree/.salamander-resume-internal"), b"keep").unwrap();
        let handle = File::open(&data).unwrap();
        let (_, errors) = delete_selected_beneath(&handle, "", &["tree".into()]).unwrap();
        assert_eq!(errors.len(), 1);
        assert!(data.join("tree/.salamander-resume-internal").exists());
    }
}
