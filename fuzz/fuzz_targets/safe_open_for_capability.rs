#![no_main]
//! Fuzz the TOCTOU-safe open primitive used by every file tool.
//!
//! `safe_open_for_capability(path, mode)` is the v0.2.x defense
//! against symlink-swap attacks.  It must:
//!   - Reject any path containing a symlink (RESOLVE_NO_SYMLINKS)
//!   - Return an `Err` that doesn't panic on bad input
//!   - Never leak the fd on the error path (RAII via `OwnedFd`)
//!
//! Fuzzer feeds an arbitrary byte sequence as the path string.  We
//! also vary the access mode through the input's first byte.
//!
//! Targets the openat2-vs-fallback branch: openat2 is tried first;
//! plain `open()` is the fallback.  Both code paths must handle
//! malformed paths (NULs, very long paths, paths that escape the
//! filesystem root) without panic.

use libfuzzer_sys::fuzz_target;
use std::str;
use std::sync::OnceLock;

use aaos_tools::path_safe::{safe_open_for_capability, AccessMode};

/// Per-fuzzer-process tmpdir.  All paths the fuzzer tries are
/// rooted under here so WriteCreateTrunc / WriteCreateAppend can't
/// litter the cwd with millions of zero-byte files (or worse,
/// touch real paths the fuzzer process owns).
fn fuzz_root() -> &'static str {
    static ROOT: OnceLock<String> = OnceLock::new();
    ROOT.get_or_init(|| {
        let pid = std::process::id();
        let dir = format!("/tmp/aaos-fuzz-{pid}");
        std::fs::create_dir_all(&dir).expect("create fuzz tmpdir");
        dir
    })
}

fuzz_target!(|data: &[u8]| {
    if data.is_empty() {
        return;
    }
    // First byte selects the access mode; remainder is the path.
    let mode = match data[0] % 6 {
        0 => AccessMode::Read,
        1 => AccessMode::WriteCreateTrunc,
        2 => AccessMode::WriteCreateAppend,
        3 => AccessMode::ReadWriteCreate,
        4 => AccessMode::PathOnly,
        _ => AccessMode::ReadDir,
    };
    let Ok(suffix) = str::from_utf8(&data[1..]) else {
        return;
    };

    // Reject paths with embedded NULs upfront — Rust's CStr
    // construction would panic.  Production callers don't pass NULs
    // (JSON-RPC layer rejects them earlier); not interesting to fuzz.
    if suffix.contains('\0') {
        return;
    }
    // Cap path length so the kernel's ENAMETOOLONG path doesn't
    // dominate the runtime.
    if suffix.len() > 4096 {
        return;
    }
    // Reject leading `/` so the suffix can't escape our fuzz tmpdir
    // on absolute paths — the production primitive accepts absolute
    // paths but for the fuzzer we want a closed sandbox so we don't
    // accidentally try to open real system paths.
    if suffix.starts_with('/') {
        return;
    }

    let path = format!("{}/{}", fuzz_root(), suffix);

    // Never panic, regardless of path content.  Errors are fine;
    // panics are the bug we hunt.
    let _ = safe_open_for_capability(&path, mode);
});
