//! Linux Landlock confinement for configured local DATA/MODELS roots.
//!
//! This is defense in depth, NOT a replacement for mount namespaces,
//! descriptor-relative file operations and HTTP authorization.
//! Install the ruleset BEFORE constructing the multithreaded Tokio runtime:
//! Landlock applies to the calling thread and threads created afterwards.

use std::fs::{self, File, OpenOptions};
use std::io;
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};

const LL_CREATE_RULESET_VERSION: u32 = 1;
const LL_RULE_PATH_BENEATH: u32 = 1;

const FS_EXECUTE: u64 = 1 << 0;
const FS_WRITE_FILE: u64 = 1 << 1;
const FS_READ_FILE: u64 = 1 << 2;
const FS_READ_DIR: u64 = 1 << 3;
const FS_REMOVE_DIR: u64 = 1 << 4;
const FS_REMOVE_FILE: u64 = 1 << 5;
const FS_MAKE_CHAR: u64 = 1 << 6;
const FS_MAKE_DIR: u64 = 1 << 7;
const FS_MAKE_REG: u64 = 1 << 8;
const FS_MAKE_SOCK: u64 = 1 << 9;
const FS_MAKE_FIFO: u64 = 1 << 10;
const FS_MAKE_BLOCK: u64 = 1 << 11;
const FS_MAKE_SYM: u64 = 1 << 12;
const FS_REFER: u64 = 1 << 13;
const FS_TRUNCATE: u64 = 1 << 14;

// ABI v3 (Linux 6.2+) is required to handle TRUNCATE. Request ALL
// available v3 file rights; rights not explicitly granted stay denied.
const HANDLED: u64 = FS_EXECUTE | FS_WRITE_FILE | FS_READ_FILE | FS_READ_DIR
    | FS_REMOVE_DIR | FS_REMOVE_FILE | FS_MAKE_CHAR | FS_MAKE_DIR
    | FS_MAKE_REG | FS_MAKE_SOCK | FS_MAKE_FIFO | FS_MAKE_BLOCK
    | FS_MAKE_SYM | FS_REFER | FS_TRUNCATE;

// No EXECUTE or special-file creation is granted to either tree.
const ALLOWED: u64 = FS_WRITE_FILE | FS_READ_FILE | FS_READ_DIR
    | FS_REMOVE_DIR | FS_REMOVE_FILE | FS_MAKE_DIR | FS_MAKE_REG
    | FS_MAKE_SYM | FS_REFER | FS_TRUNCATE;

#[repr(C)]
struct PathBeneathAttr {
    allowed_access: u64,
    parent_fd: libc::c_int,
}

// Auto-closes the ruleset fd on error paths as well as success.
struct RulesetFd(libc::c_int);
impl Drop for RulesetFd {
    fn drop(&mut self) {
        // SAFETY: a successful Landlock create_ruleset returns an owned fd.
        unsafe { libc::close(self.0); }
    }
}

pub(super) fn enter(data: &File, models: &File) -> Result<(), String> {
    // SAFETY: Version query takes NULL/0 and is independent of struct layout.
    let abi = unsafe {
        libc::syscall(libc::SYS_landlock_create_ruleset, std::ptr::null::<u8>(),
                      0usize, LL_CREATE_RULESET_VERSION)
    };
    if abi < 0 {
        return Err(format!("Landlock ABI query failed: {}", io::Error::last_os_error()));
    }
    if abi < 3 {
        return Err(format!("Landlock ABI v3 is required (kernel supports v{abi})."));
    }

    // Pass the 8-byte prefix for handled_access_fs. Later kernel ABI
    // extensions default to zero; we do not depend on them.
    // SAFETY: the pointer is initialized and remains valid for this syscall.
    let ruleset_fd = unsafe {
        libc::syscall(libc::SYS_landlock_create_ruleset, &HANDLED as *const u64,
                      std::mem::size_of::<u64>(), 0u32)
    };
    if ruleset_fd < 0 {
        return Err(format!("Landlock ruleset failed: {}", io::Error::last_os_error()));
    }
    let ruleset = RulesetFd(ruleset_fd as libc::c_int);

    for root in [data, models] {
        let rule = PathBeneathAttr { allowed_access: ALLOWED, parent_fd: root.as_raw_fd() };
        // SAFETY: rule points to an initialized C-compatible struct and
        // the root directory descriptor stays open throughout this call.
        let result = unsafe {
            libc::syscall(libc::SYS_landlock_add_rule, ruleset.0,
                          LL_RULE_PATH_BENEATH, &rule as *const PathBeneathAttr, 0u32)
        };
        if result < 0 {
            return Err(format!("Landlock path rule failed: {}", io::Error::last_os_error()));
        }
    }

    // Unprivileged callers must set no_new_privs BEFORE restrict_self.
    // SAFETY: documented prctl operation with constant arguments.
    if unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) } != 0 {
        return Err(format!("PR_SET_NO_NEW_PRIVS failed: {}", io::Error::last_os_error()));
    }
    // SAFETY: ruleset is a valid fd and all rules were installed.
    if unsafe { libc::syscall(libc::SYS_landlock_restrict_self, ruleset.0, 0u32) } < 0 {
        return Err(format!("Landlock restrict_self failed: {}", io::Error::last_os_error()));
    }
    Ok(())
}

/// Readable file and writable temporary path known to be OUTSIDE both roots.
/// Prepared before Landlock is entered; the latter is irreversible.
pub(super) struct VerificationProbes {
    outside_read: PathBuf,
    outside_write: PathBuf,
}

fn inside_any_root(path: &Path, data: &Path, models: &Path) -> bool {
    path.starts_with(data) || path.starts_with(models)
}

/// Select reliable outside probes without assuming /etc and /tmp are outside.
/// Check readability/writability first to avoid false-positive EACCES from
/// ordinary Unix permissions after installing Landlock.
pub(super) fn prepare_verification(
    data: &Path, models: &Path,
) -> Result<VerificationProbes, String> {
    let outside_read = ["/etc/passwd", "/etc/hosts", "/usr/bin/env", "/bin/sh", "/proc/version"]
        .iter()
        .filter_map(|name| fs::canonicalize(name).ok())
        .find(|path| !inside_any_root(path, data, models) && File::open(path).is_ok())
        .ok_or("Nelze najít čitelný soubor mimo DATA/MODELS pro ověření Landlock.")?;

    let mut outside_write = None;
    for (index, name) in ["/tmp", "/var/tmp", "/dev/shm"].iter().enumerate() {
        let Ok(directory) = fs::canonicalize(name) else { continue };
        if inside_any_root(&directory, data, models) { continue; }
        let candidate = directory.join(format!(
            ".salamander-landlock-probe-{}-{index}", std::process::id()
        ));
        // O_EXCL: never open or remove an existing file from another process.
        if let Ok(file) = OpenOptions::new().create_new(true).write(true).open(&candidate) {
            drop(file);
            fs::remove_file(&candidate)
                .map_err(|e| format!("Nelze uklidit předběžný test {}: {e}", candidate.display()))?;
            outside_write = Some(candidate);
            break;
        }
    }
    let outside_write = outside_write
        .ok_or("Nelze najít zapisovatelný testovací adresář mimo DATA/MODELS pro ověření Landlock.")?;

    Ok(VerificationProbes { outside_read, outside_write })
}

/// Runs after entering Landlock and before starting the HTTP runtime.
/// Must be invoked in a NEW PROCESS: the restrictions cannot be reverted.
pub(super) fn verify(
    data: &File, models: &File, data_path: &Path, models_path: &Path,
    probes: &VerificationProbes,
) -> Result<(), String> {
    for (label, handle, directory) in [
        ("DATA", data, data_path), ("MODELS", models, models_path)
    ] {
        handle.metadata().map_err(|e| format!("{label} root is not readable: {e}"))?;
        let inside = directory.join(format!(
            ".salamander-landlock-verify-{}", std::process::id()
        ));
        OpenOptions::new().create_new(true).write(true).open(&inside)
            .map_err(|e| format!("Landlock: nelze zapsat do {label}: {e}"))?;
        fs::remove_file(&inside)
            .map_err(|e| format!("Landlock: nelze smazat testovací soubor v {label}: {e}"))?;
    }
    match File::open(&probes.outside_read) {
        Ok(_) => return Err("Landlock failed: read outside the roots is allowed".into()),
        Err(e) if e.kind() == io::ErrorKind::PermissionDenied => {}
        Err(e) => return Err(format!("Unexpected outside read error: {e}")),
    }
    match OpenOptions::new().create_new(true).write(true).open(&probes.outside_write) {
        Ok(_) => {
            let _ = fs::remove_file(&probes.outside_write);
            Err("Landlock failed: write access outside the roots is still allowed".into())
        }
        Err(e) if e.kind() == io::ErrorKind::PermissionDenied => Ok(()),
        Err(e) => Err(format!("Landlock write probe did not return EACCES/EPERM: {e}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn outside_probe_detection_is_component_aware() {
        let data = Path::new("/tmp/data");
        let models = Path::new("/tmp/models");
        assert!(inside_any_root(Path::new("/tmp/data/photos"), data, models));
        assert!(inside_any_root(Path::new("/tmp/models"), data, models));
        assert!(!inside_any_root(Path::new("/tmp/databases"), data, models));
        assert!(!inside_any_root(Path::new("/etc/passwd"), data, models));
    }
}
