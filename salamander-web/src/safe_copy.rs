//! F5 foundation: copy *one regular file* between pinned directories.
//!
//! This is deliberately not wired to /api/copy yet. The existing F5 supports
//! directories, merge, overwrite and user conflict resolution; changing its
//! dispatcher before those cases have a secure equivalent would be a regression.
//! Do not treat the web file manager as production-ready while /api/copy still
//! calls the pathname-based common core.
//!
//! Caller contract: directory handles must come from open_relative_directory
//! under the pinned DATA/MODELS roots. Source data is read from an opened
//! file descriptor; publication uses single-component names under a pinned
//! destination directory. Concurrent writers still require container isolation.
//! Nested mountpoints are allowed when made visible by AI Platform; Linux
//! openat2 still forbids symlinks and traversal above the pinned root.

use std::ffi::{CStr, CString};
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use super::{transfer_http, valid_entry_name};

const BUFFER_SIZE: usize = 1024 * 1024;
static NEXT_TEMP_ID: AtomicU64 = AtomicU64::new(1);

/// A file with an unreleased temporary name must be unlinked on all exits.
/// All operations are relative to the pinned destination directory handle.
struct TempCopy<'a> {
    parent: &'a File,
    name: CString,
    output: File,
    committed: bool,
}

impl Drop for TempCopy<'_> {
    fn drop(&mut self) {
        if !self.committed {
            // SAFETY: name was created as one component in parent.
            unsafe { libc::unlinkat(self.parent.as_raw_fd(), self.name.as_ptr(), 0) };
        }
    }
}

fn open_temp(parent: &File) -> io::Result<TempCopy<'_>> {
    for _ in 0..32 {
        let id = NEXT_TEMP_ID.fetch_add(1, Ordering::Relaxed);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH).unwrap_or_default().as_nanos();
        let name = CString::new(format!(
            ".salamander-copy-{}-{nanos:x}-{id:x}.tmp", std::process::id(),
        )).expect("generated name has no NUL");
        // SAFETY: parent is a directory handle, name is a single component.
        // O_EXCL and 0600 guarantee that no existing entry is reused.
        let fd = unsafe {
            libc::openat(
                parent.as_raw_fd(), name.as_ptr(),
                libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL
                    | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                0o600,
            )
        };
        if fd >= 0 {
            return Ok(TempCopy {
                parent,
                name,
                // SAFETY: openat returned a new, owned file descriptor.
                output: unsafe { File::from_raw_fd(fd) },
                committed: false,
            });
        }
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::AlreadyExists {
            return Err(error);
        }
    }
    Err(io::Error::new(io::ErrorKind::AlreadyExists, "Nelze vytvořit dočasný soubor."))
}

fn checked_name(name: &str) -> io::Result<CString> {
    if !valid_entry_name(name) {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "Neplatný název položky."));
    }
    CString::new(name).map_err(|_| {
        io::Error::new(io::ErrorKind::InvalidInput, "Neplatný název položky.")
    })
}

fn rename_no_replace(parent: &File, from: &CStr, to: &CStr) -> io::Result<()> {
    // SAFETY: both names are single components of the same pinned directory.
    // RENAME_NOREPLACE must never clobber a concurrent destination creation.
    let rc = unsafe {
        libc::syscall(
            libc::SYS_renameat2,
            parent.as_raw_fd(), from.as_ptr(),
            parent.as_raw_fd(), to.as_ptr(),
            transfer_http::RENAME_NOREPLACE,
        )
    };
    if rc < 0 { Err(io::Error::last_os_error()) } else { Ok(()) }
}

fn preserve_regular_metadata(file: &File, metadata: &fs::Metadata) -> io::Result<()> {
    // File permission bits only: do not propagate setuid/setgid/sticky into
    // an untrusted upload-like write. Owner/group belong to the container UID.
    // SAFETY: file is an open, owned temporary file descriptor.
    if unsafe { libc::fchmod(file.as_raw_fd(), metadata.permissions().mode() & 0o777) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let times = [
        libc::timespec {
            tv_sec: metadata.atime() as libc::time_t,
            tv_nsec: metadata.atime_nsec() as libc::c_long,
        },
        libc::timespec {
            tv_sec: metadata.mtime() as libc::time_t,
            tv_nsec: metadata.mtime_nsec() as libc::c_long,
        },
    ];
    // SAFETY: times contains two valid timespec values, and fd is open.
    if unsafe { libc::futimens(file.as_raw_fd(), times.as_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Descriptor-relative, streamed copy of a single regular file.
///
/// - Never follows source symlinks or overwrites any existing target.
/// - Source and destination directories must already be opened under a root.
/// - Copies through a private 0600 temporary file, then publishes atomically.
/// - `progress` is called with bytes copied; returning an error cancels the
///   operation and removes the temporary file.
/// - Preserves ordinary file mode and mtime/atime (not ACLs or xattrs yet).
/// - Returns an error if source size or mtime changed while it was copied.
///
/// This is a *building block* for F5, not yet a replacement for its HTTP API.
#[allow(dead_code)]
pub(super) fn copy_regular_no_replace<F>(
    source_parent: &File,
    source_name: &str,
    target_parent: &File,
    target_name: &str,
    mut progress: F,
) -> io::Result<u64>
where
    F: FnMut(u64) -> io::Result<()>,
{
    let source_name = checked_name(source_name)?;
    let target_name = checked_name(target_name)?;
    let a = source_parent.metadata()?;
    let b = target_parent.metadata()?;
    if a.dev() == b.dev() && a.ino() == b.ino() && source_name == target_name {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "Zdroj a cíl jsou stejný soubor."));
    }

    // O_NONBLOCK avoids accidentally blocking forever if the source name is
    // swapped to a FIFO. fstat below rejects non-regular files.
    let mut input = transfer_http::checked_openat2(
        source_parent.as_raw_fd(), &source_name,
        libc::O_RDONLY | libc::O_NONBLOCK | libc::O_NOFOLLOW | libc::O_CLOEXEC,
    )?;
    let before = input.metadata()?;
    if !before.is_file() {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "Kopírovat lze pouze běžný soubor."));
    }

    let mut temp = open_temp(target_parent)?;
    let mut buffer = vec![0u8; BUFFER_SIZE];
    let mut copied: u64 = 0;
    loop {
        let count = input.read(&mut buffer)?;
        if count == 0 { break; }
        temp.output.write_all(&buffer[..count])?;
        copied = copied.saturating_add(count as u64);
        progress(copied)?;
    }

    let after = input.metadata()?;
    if before.dev() != after.dev() || before.ino() != after.ino()
        || before.len() != after.len()
        || before.mtime() != after.mtime()
        || before.mtime_nsec() != after.mtime_nsec()
        || copied != before.len()
    {
        return Err(io::Error::other("Zdroj se během kopírování změnil; cíl nebyl zveřejněn."));
    }

    preserve_regular_metadata(&temp.output, &before)?;
    temp.output.sync_all()?;
    rename_no_replace(target_parent, &temp.name, &target_name)?;
    temp.committed = true;
    Ok(copied)
}

// F5 stage 2: recursive directory copies. This helper is intentionally not
// exposed as an HTTP handler until conflict and progress semantics are ported.
// A directory tree is first built in a private staging directory, then
// atomically published with RENAME_NOREPLACE. This avoids publishing partial
// trees if any child is unsupported or the copy is interrupted.
//
// Like the stage-1 file primitive, this is a trusted-UID building block and
// not a secure boundary against other processes mutating the same directories.
const MAX_COPY_DEPTH: usize = 64;
const MAX_COPY_ENTRIES: usize = 100_000;

struct DirStream(*mut libc::DIR);
impl Drop for DirStream {
    fn drop(&mut self) {
        // SAFETY: fdopendir transfers ownership to the DIR*.
        unsafe { libc::closedir(self.0); }
    }
}

fn entry_names(directory: &File) -> io::Result<Vec<CString>> {
    // A new open file description is necessary; dup() would share readdir's
    // offset with any other user of the directory descriptor.
    // SAFETY: dot is a fixed, NUL-terminated path component.
    let fd = unsafe {
        libc::openat(
            directory.as_raw_fd(), c".".as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC | libc::O_NOFOLLOW,
        )
    };
    if fd < 0 { return Err(io::Error::last_os_error()); }
    // SAFETY: fdopendir takes ownership only on success.
    let raw = unsafe { libc::fdopendir(fd) };
    if raw.is_null() {
        let err = io::Error::last_os_error();
        unsafe { libc::close(fd); }
        return Err(err);
    }
    let stream = DirStream(raw);
    let mut names = Vec::new();
    loop {
        // SAFETY: Linux thread-local errno; readdir returns NULL at EOF/error.
        unsafe { *libc::__errno_location() = 0; }
        let entry = unsafe { libc::readdir(stream.0) };
        if entry.is_null() {
            let err = io::Error::last_os_error();
            if err.raw_os_error() == Some(0) { break; }
            return Err(err);
        }
        // SAFETY: d_name is a NUL-terminated string valid until next readdir.
        let name = unsafe { CStr::from_ptr((*entry).d_name.as_ptr()) };
        if name.to_bytes() == b"." || name.to_bytes() == b".." { continue; }
        if names.len() >= MAX_COPY_ENTRIES {
            return Err(io::Error::other("Překročen limit položek kopírovaného adresáře."));
        }
        names.push(name.to_owned());
    }
    Ok(names)
}

fn make_stage_name() -> CString {
    let id = NEXT_TEMP_ID.fetch_add(1, Ordering::Relaxed);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH).unwrap_or_default().as_nanos();
    CString::new(format!(".salamander-copy-dir-{}-{nanos:x}-{id:x}.tmp", std::process::id()))
        .expect("generated name has no NUL")
}

/// Remove *only* a directory created as private staging, relative to its
/// pinned parent. On errors it leaves the incomplete directory for diagnosis;
/// it never falls back to a potentially unconfined pathname-based rm -rf.
fn cleanup_stage_dir(parent: &File, name: &CStr) -> io::Result<()> {
    let dir = transfer_http::checked_openat2(
        parent.as_raw_fd(), name,
        libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
    )?;
    // Copying may already have restored source mode (including read-only).
    // Reopen children as owner by temporarily granting user rwx on staging.
    // SAFETY: dir is the already-opened staging directory, not a pathname.
    if unsafe { libc::fchmod(dir.as_raw_fd(), 0o700) } != 0 {
        return Err(io::Error::last_os_error());
    }
    for child in entry_names(&dir)? {
        let mut current = std::mem::MaybeUninit::<libc::stat>::uninit();
        // SAFETY: fstatat does not follow child symlinks, including dangling ones.
        if unsafe {
            libc::fstatat(
                dir.as_raw_fd(), child.as_ptr(), current.as_mut_ptr(),
                libc::AT_SYMLINK_NOFOLLOW,
            )
        } != 0 {
            return Err(io::Error::last_os_error());
        }
        let kind = unsafe { current.assume_init() }.st_mode & libc::S_IFMT;
        if kind == libc::S_IFDIR {
            cleanup_stage_dir(&dir, &child)?;
        } else {
            // SAFETY: child is a single component in an opened directory.
            if unsafe { libc::unlinkat(dir.as_raw_fd(), child.as_ptr(), 0) } != 0 {
                return Err(io::Error::last_os_error());
            }
        }
    }
    // SAFETY: AT_REMOVEDIR acts on one entry in the pinned parent.
    if unsafe { libc::unlinkat(parent.as_raw_fd(), name.as_ptr(), libc::AT_REMOVEDIR) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

struct TempCopyDirectory<'a> {
    parent: &'a File,
    name: CString,
    directory: File,
    published: bool,
}
impl Drop for TempCopyDirectory<'_> {
    fn drop(&mut self) {
        if !self.published {
            // We still own an open descriptor even if preserving metadata
            // made the staging root non-writable before a failed rename.
            // SAFETY: the handle belongs to the temporary directory.
            unsafe { libc::fchmod(self.directory.as_raw_fd(), 0o700) };
            if let Err(error) = cleanup_stage_dir(self.parent, &self.name) {
                eprintln!("Salamander: neúspěšné odstranění dočasného adresáře: {error}");
            }
        }
    }
}

fn make_temp_directory(parent: &File) -> io::Result<TempCopyDirectory<'_>> {
    for _ in 0..32 {
        let name = make_stage_name();
        // SAFETY: name is a generated single component, unique and private.
        let rc = unsafe { libc::mkdirat(parent.as_raw_fd(), name.as_ptr(), 0o700) };
        if rc == 0 {
            let directory = match transfer_http::checked_openat2(
                parent.as_raw_fd(), &name,
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            ) {
                Ok(dir) => dir,
                Err(error) => {
                    unsafe { libc::unlinkat(parent.as_raw_fd(), name.as_ptr(), libc::AT_REMOVEDIR) };
                    return Err(error);
                }
            };
            return Ok(TempCopyDirectory { parent, name, directory, published: false });
        }
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::AlreadyExists { return Err(error); }
    }
    Err(io::Error::new(io::ErrorKind::AlreadyExists, "Nelze vytvořit dočasný adresář."))
}

struct CopyBudget {
    entries: usize,
    bytes: u64,
}

fn copy_children<F>(
    source: &File,
    dest: &File,
    depth: usize,
    budget: &mut CopyBudget,
    progress: &mut F,
) -> io::Result<()>
where
    F: FnMut(u64) -> io::Result<()>,
{
    if depth >= MAX_COPY_DEPTH {
        return Err(io::Error::other("Překročen limit hloubky kopírování."));
    }
    for child in entry_names(source)? {
        budget.entries += 1;
        if budget.entries > MAX_COPY_ENTRIES {
            return Err(io::Error::other("Překročen limit počtu kopírovaných položek."));
        }
        // Cancellation is polled even for trees of empty directories.
        progress(budget.bytes)?;
        let name = child.to_str()
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "Název souboru není UTF-8."))?;
        checked_name(name)?;
        let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
        // SAFETY: one entry component; do not follow nested symlinks.
        if unsafe {
            libc::fstatat(
                source.as_raw_fd(), child.as_ptr(), stat.as_mut_ptr(),
                libc::AT_SYMLINK_NOFOLLOW,
            )
        } != 0 {
            return Err(io::Error::last_os_error());
        }
        let kind = unsafe { stat.assume_init() }.st_mode & libc::S_IFMT;
        if kind == libc::S_IFREG {
            let base = budget.bytes;
            let copied = copy_regular_no_replace(source, name, dest, name, |file_bytes| {
                progress(base.saturating_add(file_bytes))
            })?;
            budget.bytes = budget.bytes.saturating_add(copied);
        } else if kind == libc::S_IFDIR {
            let opened = transfer_http::checked_openat2(
                source.as_raw_fd(), &child,
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )?;
            let before = opened.metadata()?;
            // SAFETY: dst is a private staging directory and name is one
            // validated component; collisions fail rather than being merged.
            if unsafe { libc::mkdirat(dest.as_raw_fd(), child.as_ptr(), 0o700) } != 0 {
                return Err(io::Error::last_os_error());
            }
            let new_dest = transfer_http::checked_openat2(
                dest.as_raw_fd(), &child,
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )?;
            copy_children(&opened, &new_dest, depth + 1, budget, progress)?;
            let after = opened.metadata()?;
            if before.dev() != after.dev() || before.ino() != after.ino()
                || before.mtime() != after.mtime()
                || before.mtime_nsec() != after.mtime_nsec()
            {
                return Err(io::Error::other("Zdrojový adresář se během kopírování změnil."));
            }
            preserve_regular_metadata(&new_dest, &before)?;
            new_dest.sync_all()?;
        } else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Kopírovaný strom obsahuje symbolický odkaz nebo speciální soubor.",
            ));
        }
    }
    Ok(())
}

/// Descriptor-relative, no-overwrite copy of a directory tree.
///
/// The destination is invisible until all contents have been copied into a
/// private staging directory and the final RENAME_NOREPLACE succeeds. A
/// callback error cancels copying; a best-effort cleanup removes staging.
/// Mounted filesystems inside source are allowed, but a source must not be
/// copied into its own descendants (the eventual HTTP caller must enforce
/// this based on validated selection paths before invoking the worker).
/// Symlinks, special files and non-UTF-8 nested names abort the entire copy.
/// This operation is not exposed as HTTP F5 yet.
#[allow(dead_code)]
pub(super) fn copy_directory_no_replace<F>(
    source_parent: &File,
    source_name: &str,
    target_parent: &File,
    target_name: &str,
    mut progress: F,
) -> io::Result<u64>
where
    F: FnMut(u64) -> io::Result<()>,
{
    let source_name = checked_name(source_name)?;
    let target_name = checked_name(target_name)?;
    let a = source_parent.metadata()?;
    let b = target_parent.metadata()?;
    if a.dev() == b.dev() && a.ino() == b.ino() && source_name == target_name {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "Zdroj a cíl jsou stejný adresář."));
    }
    let source = transfer_http::checked_openat2(
        source_parent.as_raw_fd(), &source_name,
        libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
    )?;
    let before = source.metadata()?;
    let mut stage = make_temp_directory(target_parent)?;
    let mut budget = CopyBudget { entries: 0, bytes: 0 };
    copy_children(&source, &stage.directory, 0, &mut budget, &mut progress)?;
    let after = source.metadata()?;
    if before.dev() != after.dev() || before.ino() != after.ino()
        || before.mtime() != after.mtime()
        || before.mtime_nsec() != after.mtime_nsec()
    {
        return Err(io::Error::other("Zdrojový adresář se během kopírování změnil."));
    }
    preserve_regular_metadata(&stage.directory, &before)?;
    stage.directory.sync_all()?;
    rename_no_replace(target_parent, &stage.name, &target_name)?;
    stage.published = true;
    Ok(budget.bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    fn setup() -> (tempfile::TempDir, File, File) {
        let temp = tempfile::tempdir().unwrap();
        fs::create_dir(temp.path().join("data")).unwrap();
        fs::create_dir(temp.path().join("models")).unwrap();
        let data = File::open(temp.path().join("data")).unwrap();
        let models = File::open(temp.path().join("models")).unwrap();
        (temp, data, models)
    }

    fn assert_no_staging(dir: &std::path::Path) {
        assert!(fs::read_dir(dir).unwrap().all(|entry| {
            !entry.unwrap().file_name().to_string_lossy().starts_with(".salamander-copy-")
        }));
    }

    #[test]
    fn copy_preserves_bytes_mode_and_mtime() {
        let (temp, data, models) = setup();
        let source = temp.path().join("data/source.txt");
        fs::write(&source, b"SalamanderRS").unwrap();
        fs::set_permissions(&source, fs::Permissions::from_mode(0o640)).unwrap();
        let source_mtime = fs::metadata(&source).unwrap().mtime();
        let mut updates = Vec::new();
        let bytes = copy_regular_no_replace(&data, "source.txt", &models, "target.txt", |n| {
            updates.push(n);
            Ok(())
        }).unwrap();
        assert_eq!(bytes, 12);
        assert_eq!(updates.last().copied(), Some(bytes));
        let target = temp.path().join("models/target.txt");
        assert_eq!(fs::read(&target).unwrap(), b"SalamanderRS");
        let metadata = fs::metadata(target).unwrap();
        assert_eq!(metadata.permissions().mode() & 0o777, 0o640);
        assert_eq!(metadata.mtime(), source_mtime);
        assert_no_staging(&temp.path().join("models"));
    }

    #[test]
    fn collision_never_overwrites_regular_file_or_symlink() {
        let (temp, data, models) = setup();
        fs::write(temp.path().join("data/source"), b"new").unwrap();
        fs::write(temp.path().join("models/target"), b"old").unwrap();
        let error = copy_regular_no_replace(&data, "source", &models, "target", |_| Ok(())).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(fs::read(temp.path().join("models/target")).unwrap(), b"old");
        assert_no_staging(&temp.path().join("models"));

        fs::remove_file(temp.path().join("models/target")).unwrap();
        fs::write(temp.path().join("outside"), b"keep").unwrap();
        symlink(temp.path().join("outside"), temp.path().join("models/target")).unwrap();
        let error = copy_regular_no_replace(&data, "source", &models, "target", |_| Ok(())).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(fs::read(temp.path().join("outside")).unwrap(), b"keep");
        assert_no_staging(&temp.path().join("models"));
    }

    #[test]
    fn source_symlink_and_invalid_components_are_rejected() {
        let (temp, data, models) = setup();
        fs::write(temp.path().join("outside"), b"outside").unwrap();
        symlink(temp.path().join("outside"), temp.path().join("data/link")).unwrap();
        assert!(copy_regular_no_replace(&data, "link", &models, "copied", |_| Ok(())).is_err());
        assert!(copy_regular_no_replace(&data, "../outside", &models, "copied", |_| Ok(())).is_err());
        assert!(copy_regular_no_replace(&data, "link", &models, "../escape", |_| Ok(())).is_err());
        assert!(!temp.path().join("models/copied").exists());
        assert_no_staging(&temp.path().join("models"));
    }

    #[test]
    fn cancellation_cleans_up_temporary_file() {
        let (temp, data, models) = setup();
        fs::write(temp.path().join("data/large"), vec![0x51u8; BUFFER_SIZE + 1]).unwrap();
        let error = copy_regular_no_replace(&data, "large", &models, "copy", |_| {
            Err(io::Error::new(io::ErrorKind::Interrupted, "cancelled"))
        }).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::Interrupted);
        assert!(!temp.path().join("models/copy").exists());
        assert_no_staging(&temp.path().join("models"));
    }

    #[test]
    fn cannot_copy_name_on_same_directory_to_itself() {
        let (temp, data, _models) = setup();
        fs::write(temp.path().join("data/same"), b"same").unwrap();
        let error = copy_regular_no_replace(&data, "same", &data, "same", |_| Ok(())).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
    }
    #[test]
    fn directory_copy_is_atomic_and_preserves_nested_contents() {
        let (temp, data, models) = setup();
        fs::create_dir(temp.path().join("data/tree")).unwrap();
        fs::create_dir(temp.path().join("data/tree/sub")).unwrap();
        fs::write(temp.path().join("data/tree/a.txt"), b"ABC").unwrap();
        fs::write(temp.path().join("data/tree/sub/b.txt"), b"DEF123").unwrap();
        fs::set_permissions(
            temp.path().join("data/tree/sub"),
            fs::Permissions::from_mode(0o750),
        ).unwrap();
        let mut updates = Vec::new();
        let copied = copy_directory_no_replace(&data, "tree", &models, "destination", |bytes| {
            updates.push(bytes);
            Ok(())
        }).unwrap();
        assert_eq!(copied, 9);
        assert_eq!(updates.last().copied(), Some(9));
        assert_eq!(fs::read(temp.path().join("models/destination/a.txt")).unwrap(), b"ABC");
        assert_eq!(fs::read(temp.path().join("models/destination/sub/b.txt")).unwrap(), b"DEF123");
        assert_eq!(
            fs::metadata(temp.path().join("models/destination/sub")).unwrap().permissions().mode() & 0o777,
            0o750,
        );
        assert_no_staging(&temp.path().join("models"));
    }

    #[test]
    fn directory_conflict_does_not_replace_existing_target() {
        let (temp, data, models) = setup();
        fs::create_dir(temp.path().join("data/tree")).unwrap();
        fs::write(temp.path().join("data/tree/source"), b"source").unwrap();
        fs::create_dir(temp.path().join("models/tree")).unwrap();
        fs::write(temp.path().join("models/tree/sentinel"), b"keep").unwrap();
        let err = copy_directory_no_replace(&data, "tree", &models, "tree", |_| Ok(())).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(fs::read(temp.path().join("models/tree/sentinel")).unwrap(), b"keep");
        assert!(!temp.path().join("models/tree/source").exists());
        assert_no_staging(&temp.path().join("models"));
    }

    #[test]
    fn directory_copy_rejects_nested_symlink_without_exposure() {
        let (temp, data, models) = setup();
        fs::create_dir(temp.path().join("data/tree")).unwrap();
        fs::write(temp.path().join("data/tree/ok"), b"ok").unwrap();
        fs::write(temp.path().join("outside"), b"secret").unwrap();
        symlink(temp.path().join("outside"), temp.path().join("data/tree/leak")).unwrap();
        assert!(copy_directory_no_replace(&data, "tree", &models, "target", |_| Ok(())).is_err());
        assert!(!temp.path().join("models/target").exists());
        assert_eq!(fs::read(temp.path().join("outside")).unwrap(), b"secret");
        assert_no_staging(&temp.path().join("models"));
    }

    #[test]
    fn directory_copy_cancelled_removes_all_staging() {
        let (temp, data, models) = setup();
        fs::create_dir(temp.path().join("data/tree")).unwrap();
        fs::write(temp.path().join("data/tree/file"), vec![0x42; BUFFER_SIZE + 1]).unwrap();
        let err = copy_directory_no_replace(&data, "tree", &models, "target", |bytes| {
            if bytes > 0 {
                Err(io::Error::new(io::ErrorKind::Interrupted, "stop"))
            } else {
                Ok(())
            }
        }).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::Interrupted);
        assert!(!temp.path().join("models/target").exists());
        assert_no_staging(&temp.path().join("models"));
    }

    #[test]
    fn directory_copy_rejects_self_without_modifying_source() {
        let (temp, data, _models) = setup();
        fs::create_dir(temp.path().join("data/dir")).unwrap();
        fs::write(temp.path().join("data/dir/keep"), b"keep").unwrap();
        let err = copy_directory_no_replace(&data, "dir", &data, "dir", |_| Ok(())).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
        assert_eq!(fs::read(temp.path().join("data/dir/keep")).unwrap(), b"keep");
        assert_no_staging(&temp.path().join("data"));
    }

}
