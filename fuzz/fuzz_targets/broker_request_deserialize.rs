#![no_main]
//! Fuzz broker-protocol deserialization.
//!
//! `WireRequest` is what the worker reads off its broker stream and
//! deserializes via `serde_json::from_slice`.  A malformed frame from
//! a buggy or compromised daemon side could deserialize to something
//! the worker mishandles — panics, arithmetic-overflow, or the
//! tagged-enum dispatch landing in an unhandled arm.
//!
//! v0.2.0 added `Request::RevokeToken` (push-revocation), v0.2.x
//! kept extending `InvokeTool` to carry capability tokens.  Each
//! addition is a new shape for serde to handle.  This fuzzer hammers
//! arbitrary bytes through the deserialize path looking for panics.
//!
//! We don't expect to find correctness bugs (serde is heavily
//! battle-tested) — what we look for is panics in custom Deserialize
//! impls or in arithmetic that runs on deserialized values.

use libfuzzer_sys::fuzz_target;

use aaos_backend_linux::broker_protocol::WireRequest;

fuzz_target!(|data: &[u8]| {
    // Try as-is.
    let _ = serde_json::from_slice::<WireRequest>(data);

    // Try wrapped — the wire protocol is one JSON object per line, so
    // exercise the trailing-newline + multi-frame paths a real reader
    // would hit.
    if !data.is_empty() {
        let mut buf = data.to_vec();
        buf.push(b'\n');
        let _ = serde_json::from_slice::<WireRequest>(&buf);
    }
});
