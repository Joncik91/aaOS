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
use std::path::{Path, PathBuf};

use nix::fcntl::{open, OFlag};
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
    /// Cannot be used for I/O. Used by tools that need a
    /// TOCTOU-safe canonical for capability checking but then
    /// perform their I/O via a different syscall (e.g.
    /// `tokio::fs::read_dir`, `ripgrep`).
    PathOnly,
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
    };
    // 0o600 — only meaningful when O_CREAT is set; ignored on read.
    let create_mode = Mode::S_IRUSR | Mode::S_IWUSR;

    let raw_fd = open(Path::new(path), flags, create_mode).map_err(|errno| match errno {
        nix::errno::Errno::ELOOP => CoreError::Ipc(format!(
            "refusing to open symlink at {path}: O_NOFOLLOW (capability TOCTOU guard)"
        )),
        nix::errno::Errno::ENOENT => CoreError::Ipc(format!("file not found: {path}")),
        nix::errno::Errno::EACCES => CoreError::Ipc(format!("permission denied: {path}")),
        e => CoreError::Ipc(format!("open({path}) failed: {e}")),
    })?;
    // SAFETY: `nix::fcntl::open` returned `Ok` so `raw_fd` is a valid,
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
pub fn canonical_path_for_fd(fd: i32) -> std::io::Result<String> {
    let link = format!("/proc/self/fd/{fd}");
    let target: PathBuf = std::fs::read_link(&link)?;
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
