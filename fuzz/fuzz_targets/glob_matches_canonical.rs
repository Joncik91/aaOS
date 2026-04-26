#![no_main]
//! Fuzz the path-traversal surface that gates every file capability.
//!
//! `glob_matches_canonical(pattern, canonical)` is the function the
//! fd-based file tools (file_read, file_write, file_edit, file_list,
//! file_read_many, grep) call to decide whether the kernel-pinned
//! canonical path satisfies the agent's grant glob.  A bug here
//! (e.g., a pattern like `/data/*` accepting `/data-foo/...`,
//! repeated in v0.1.x and fixed) is a capability bypass.
//!
//! Fuzzer feeds two slices interpreted as UTF-8 strings.  We exercise
//! both `glob_matches_canonical` directly AND the higher-level
//! `Capability::FileRead { path_glob }.permits(&FileRead { path_glob })`
//! check via a token, since the higher-level wrapper has its own
//! canonicalize-and-match path that has historically diverged from
//! the bare glob matcher.
//!
//! Invariant we'd like to assert (impossible to verify universally —
//! we just look for panics here): no pattern that lexically refuses
//! to match a string under glob semantics should match it under the
//! capability semantics.

use libfuzzer_sys::fuzz_target;
use std::str;

use aaos_core::{
    glob_matches_canonical, AgentId, Capability, CapabilityToken, Constraints,
};

fuzz_target!(|data: &[u8]| {
    // Split the input into two roughly-equal halves: pattern + path.
    if data.len() < 2 {
        return;
    }
    let mid = data.len() / 2;
    let (Ok(pattern), Ok(path)) = (str::from_utf8(&data[..mid]), str::from_utf8(&data[mid..]))
    else {
        return;
    };

    // Direct call — should never panic, regardless of input.
    let _ = glob_matches_canonical(pattern, path);

    // Higher-level path: build a token with FileRead grant, ask if it
    // permits a FileRead request on the same path.  Should never
    // panic.  Differential is the interesting part: if the bare
    // matcher denies but the token permits (or vice versa) on a
    // canonical-form path, that's a divergence worth knowing.
    let token = CapabilityToken::issue(
        AgentId::new(),
        Capability::FileRead {
            path_glob: pattern.to_string(),
        },
        Constraints::default(),
    );
    let request = Capability::FileRead {
        path_glob: path.to_string(),
    };
    let _ = token.permits(&request);
});
