//! TOCTOU-safe path opening for capability-checked filesystem tools.
//!
//! The naive flow that previous versions of the file tools used is:
//!
//! 1. Capability check: `canonical_for_match(path_str)` — calls
//!    `fs::canonicalize` and matches the returned string against the
//!    permitted glob.
//! 2. I/O: `tokio::fs::open(path_str)` (or `read_to_string`, etc.).
//!
//! Between steps 1 and 2 an attacker with write access to any directory in
//! the path can swap a regular file for a symlink to a forbidden target.
//! The capability check passes against the original file's canonical form;
//! the I/O follows the symlink and reads the forbidden target. Round 4 + 5
//! self-reflection runs filed this as a real capability bypass.
//!
//! The fix in this module is the standard Linux pattern:
//!
//! - Open with `O_NOFOLLOW | O_CLOEXEC` so symlinks at the *final*
//!   component are rejected before they can redirect us.
//! - Resolve the resulting fd to a canonical path via
//!   `/proc/self/fd/<fd>` — this read happens *after* the open, so
//!   the kernel has already pinned the inode and a subsequent
//!   filesystem mutation cannot change the canonical we get back.
//! - The capability check is run against this fd-derived canonical,
//!   then I/O proceeds on the same fd. There is no second open.
//!
//! `O_NOFOLLOW` only protects the *last* component, but the
//! `/proc/self/fd/<fd>` readlink reflects the kernel's view of the
//! file's full path — including any symlinks crossed by intermediate
//! components — so the capability decision is made against the real
//! target, not the requested path string.

#![cfg(target_os = "linux")]

use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::path::Path;

use nix::fcntl::{open, openat2, OFlag, OpenHow, ResolveFlag};
use nix::sys::stat::Mode;

use aaos_core::{CoreError, Result};

/// Access mode for [`safe_open_for_capability`]. We do **not** support
/// fully-arbitrary `OpenOptions` here — the capability tooling only needs
/// read, create-truncate, and append. Keeping the surface narrow makes
/// flag mistakes harder.
#[derive(Debug, Clone, Copy)]
pub enum AccessMode {
    /// `O_RDONLY` — for tools that only read.
    Read,
    /// `O_WRONLY | O_CREAT | O_TRUNC` — for full-file writes.
    /// The file's mode bits are 0o600 if newly created.
    WriteCreateTrunc,
    /// `O_WRONLY | O_CREAT | O_APPEND` — for append-mode writes.
    /// The file's mode bits are 0o600 if newly created.
    WriteCreateAppend,
    /// `O_RDWR | O_CREAT` — for tools that read-then-write the
    /// same fd (used by file_edit). The file's mode bits are
    /// 0o600 if newly created.
    ReadWriteCreate,
    /// `O_PATH | O_NOFOLLOW` — fd suitable only for path
    /// resolution (`/proc/self/fd/<fd>` readlink) and metadata.
    /// Cannot be used for I/O. Used by tools that hand off to
    /// an external syscall (e.g. ripgrep) and don't need to read
    /// directly through the fd.
    PathOnly,
    /// `O_RDONLY | O_DIRECTORY | O_NOFOLLOW` — opens a directory
    /// for reading. Used by tools that want a TOCTOU-safe
    /// `Dir::from_fd` listing path: the same fd that powered the
    /// capability check is the one passed to `fdopendir`. ENOTDIR
    /// is returned by the kernel if the path resolves to a non-
    /// directory.
    ReadDir,
}

/// Open a path safely for a capability-checked tool.
///
/// Returns the [`OwnedFd`] (RAII: closed on drop) and the canonical path
/// string the kernel sees for that fd, suitable for capability matching
/// against the agent's allowed globs.
///
/// The fd refers to the inode that was open at the time of the call; later
/// renames or unlinks of the path do not invalidate the fd. Callers should
/// pass the fd directly to I/O (`File::from`, `tokio::fs::File::from_std`,
/// etc.) rather than re-opening by path.
pub fn safe_open_for_capability(path: &str, mode: AccessMode) -> Result<(OwnedFd, String)> {
    let flags = match mode {
        AccessMode::Read => OFlag::O_RDONLY | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC,
        AccessMode::WriteCreateTrunc => {
            OFlag::O_WRONLY | OFlag::O_CREAT | OFlag::O_TRUNC | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC
        }
        AccessMode::WriteCreateAppend => {
            OFlag::O_WRONLY
                | OFlag::O_CREAT
                | OFlag::O_APPEND
                | OFlag::O_NOFOLLOW
                | OFlag::O_CLOEXEC
        }
        AccessMode::ReadWriteCreate => {
            OFlag::O_RDWR | OFlag::O_CREAT | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC
        }
        AccessMode::PathOnly => OFlag::O_PATH | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC,
        AccessMode::ReadDir => {
            OFlag::O_RDONLY | OFlag::O_DIRECTORY | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC
        }
    };
    // 0o600 — only meaningful when O_CREAT is set; ignored on read.
    let create_mode = Mode::S_IRUSR | Mode::S_IWUSR;

    // Bug 32 (v0.2.3): try `openat2(RESOLVE_NO_SYMLINKS)` first.  This
    // rejects symlinks at *every* path component, not just the leaf —
    // closing the intermediate-component swap window that O_NOFOLLOW
    // alone leaves open.  Available since Linux 5.6.  On older kernels
    // (or syscalls denied by an LSM), fall back to plain `open()` with
    // O_NOFOLLOW so the build still works on hosts that don't support
    // openat2; the leaf-only protection is the same as v0.2.2.
    //
    // Strip O_NOFOLLOW from the flags when calling openat2 — the kernel
    // returns EINVAL if O_NOFOLLOW is set together with
    // RESOLVE_NO_SYMLINKS (the latter strictly subsumes the former).
    let openat2_flags = flags & !OFlag::O_NOFOLLOW;
    // open_how::mode MUST be 0 unless O_CREAT or O_TMPFILE is set; the
    // kernel returns EINVAL otherwise.
    let how = if openat2_flags.contains(OFlag::O_CREAT) {
        OpenHow::new()
            .flags(openat2_flags)
            .mode(create_mode)
            .resolve(ResolveFlag::RESOLVE_NO_SYMLINKS)
    } else {
        OpenHow::new()
            .flags(openat2_flags)
            .resolve(ResolveFlag::RESOLVE_NO_SYMLINKS)
    };
    let raw_fd = match openat2(libc::AT_FDCWD, Path::new(path), how) {
        Ok(fd) => fd,
        Err(nix::errno::Errno::ENOSYS) | Err(nix::errno::Errno::EPERM) => {
            // openat2 unavailable — fall back to plain open + O_NOFOLLOW.
            // EPERM here typically means seccomp denied the syscall (the
            // worker's allowlist may not include SYS_openat2 yet).
            open(Path::new(path), flags, create_mode).map_err(|errno| match errno {
                nix::errno::Errno::ELOOP => CoreError::Ipc(format!(
                    "refusing to open symlink at {path}: O_NOFOLLOW (capability TOCTOU guard)"
                )),
                nix::errno::Errno::ENOENT => CoreError::Ipc(format!("file not found: {path}")),
                nix::errno::Errno::EACCES => CoreError::Ipc(format!("permission denied: {path}")),
                e => CoreError::Ipc(format!("open({path}) failed: {e}")),
            })?
        }
        Err(nix::errno::Errno::ELOOP) => {
            return Err(CoreError::Ipc(format!(
                "refusing to open path containing symlink: {path} \
                 (RESOLVE_NO_SYMLINKS — capability TOCTOU guard)"
            )));
        }
        Err(nix::errno::Errno::ENOENT) => {
            return Err(CoreError::Ipc(format!("file not found: {path}")));
        }
        Err(nix::errno::Errno::EACCES) => {
            return Err(CoreError::Ipc(format!("permission denied: {path}")));
        }
        Err(e) => {
            return Err(CoreError::Ipc(format!("openat2({path}) failed: {e}")));
        }
    };
    // SAFETY: openat2/open returned `Ok` so `raw_fd` is a valid,
    // exclusive-ownership file descriptor we are responsible for closing.
    // Wrap it in `OwnedFd` so RAII handles cleanup.
    let fd: OwnedFd = unsafe { OwnedFd::from_raw_fd(raw_fd) };

    let canonical = canonical_path_for_fd(fd.as_raw_fd())
        .map_err(|e| CoreError::Ipc(format!("canonicalize fd {}: {}", fd.as_raw_fd(), e)))?;
    Ok((fd, canonical))
}

/// Resolve `/proc/self/fd/<fd>` to the path the kernel currently sees
/// for that file descriptor. This reflects the post-open canonical form
/// — symlinks crossed by intermediate components have already been
/// resolved by the open syscall, and the inode is pinned, so a
/// subsequent rename/symlink-swap cannot change the answer.
///
/// Uses `readlinkat(AT_FDCWD, ...)` directly via nix — the namespaced
/// worker's seccomp policy permits `readlinkat` but not the older bare
/// `readlink` syscall, and Rust's `std::fs::read_link` calls the bare
/// syscall on x86_64 glibc, returning EPERM under seccomp. Calling
/// `readlinkat` ensures we go through the syscall the worker is
/// actually allowed to make.
pub fn canonical_path_for_fd(fd: i32) -> std::io::Result<String> {
    let link = format!("/proc/self/fd/{fd}");
    let target = nix::fcntl::readlinkat(None, Path::new(&link))
        .map_err(|errno| std::io::Error::from_raw_os_error(errno as i32))?;
    Ok(target.to_string_lossy().into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::os::fd::{FromRawFd, IntoRawFd};

    #[test]
    fn canonical_for_fd_matches_path() {
        let mut tmp = tempfile::NamedTempFile::new().unwrap();
        write!(tmp, "hello").unwrap();
        let path = tmp.path().to_path_buf();

        let (fd, canonical) =
            safe_open_for_capability(path.to_str().unwrap(), AccessMode::Read).unwrap();
        // canonical comes back as the realpath of the file.
        assert_eq!(
            canonical,
            std::fs::canonicalize(&path).unwrap().to_string_lossy()
        );
        // fd is real — check we can read from it.
        let mut buf = String::new();
        use std::io::Read;
        let mut f = unsafe { std::fs::File::from_raw_fd(fd.into_raw_fd()) };
        f.read_to_string(&mut buf).unwrap();
        assert_eq!(buf, "hello");
    }

    #[test]
    fn nofollow_rejects_symlink() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("target.txt");
        std::fs::write(&target, "secret").unwrap();
        let link = dir.path().join("link.txt");
        std::os::unix::fs::symlink(&target, &link).unwrap();

        let result = safe_open_for_capability(link.to_str().unwrap(), AccessMode::Read);
        assert!(result.is_err(), "O_NOFOLLOW should reject symlink");
        let err = result.unwrap_err().to_string();
        assert!(err.contains("symlink") || err.contains("ELOOP") || err.contains("loop"));
    }

    #[test]
    fn intermediate_component_symlink_rejected() {
        // Bug 32 (v0.2.3): O_NOFOLLOW alone only rejects symlinks at the
        // *leaf* component.  RESOLVE_NO_SYMLINKS via openat2 rejects them
        // at every component.  Plant a symlink as the parent dir and
        // assert the open is refused, even though the leaf itself is a
        // regular filename.
        let dir = tempfile::tempdir().unwrap();
        let real_subdir = dir.path().join("real");
        std::fs::create_dir(&real_subdir).unwrap();
        std::fs::write(real_subdir.join("file.txt"), b"safe").unwrap();
        // /<dir>/link -> /<dir>/real
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&real_subdir, &link).unwrap();

        // Try to open via the symlink intermediate. O_NOFOLLOW alone
        // would accept this (the leaf "file.txt" is not a symlink).
        // openat2 with RESOLVE_NO_SYMLINKS rejects it.
        let result =
            safe_open_for_capability(link.join("file.txt").to_str().unwrap(), AccessMode::Read);
        assert!(
            result.is_err(),
            "RESOLVE_NO_SYMLINKS must reject symlink in any path component"
        );
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("symlink") || err.contains("RESOLVE_NO_SYMLINKS") || err.contains("ELOOP"),
            "expected symlink-rejection error, got: {err}"
        );
    }

    #[test]
    fn write_create_trunc_creates_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("new.txt");
        let path_str = path.to_str().unwrap();

        let (fd, canonical) =
            safe_open_for_capability(path_str, AccessMode::WriteCreateTrunc).unwrap();
        assert!(canonical.ends_with("new.txt"));

        let mut f = unsafe { std::fs::File::from_raw_fd(fd.into_raw_fd()) };
        f.write_all(b"hi").unwrap();
        drop(f);

        let read_back = std::fs::read_to_string(&path).unwrap();
        assert_eq!(read_back, "hi");
    }

    #[test]
    fn symlink_swap_after_open_does_not_affect_fd() {
        // Plant a regular file, open it, then swap it for a symlink to
        // a forbidden target. The fd is bound to the original inode;
        // I/O on it must still see the original content.
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("legit.txt");
        std::fs::write(&target, "legit").unwrap();
        let other = dir.path().join("forbidden.txt");
        std::fs::write(&other, "forbidden").unwrap();

        let (fd, _) = safe_open_for_capability(target.to_str().unwrap(), AccessMode::Read).unwrap();

        // Swap the path for a symlink to the forbidden file.
        std::fs::remove_file(&target).unwrap();
        std::os::unix::fs::symlink(&other, &target).unwrap();

        // The pre-open fd still sees the original contents.
        let mut buf = String::new();
        use std::io::Read;
        let mut f = unsafe { std::fs::File::from_raw_fd(fd.into_raw_fd()) };
        f.read_to_string(&mut buf).unwrap();
        assert_eq!(buf, "legit");
    }
}
