# Run 7 Follow-up: acted on the error-handling finding *(2026-04-14, same day)*

Two commits derived from Run 7b's proposal, scoped minimally rather than implementing the system's 6-week `AaosError` super-enum plan:

- **`ba0904a`** — renamed the `MemoryResult2` alias in `aaos-memory` to `MemoryStoreResult`. Round-1 of Copilot's review caught that the system's proposed "rename to `MemoryResult`" would collide with the existing `MemoryResult` struct (a query-result data type). `MemoryStoreResult` is the accurate, unambiguous name.

- **`51db7b5`** — added `SummarizationFailureKind` enum to `aaos-core::audit`, extended `ContextSummarizationFailed` audit variant with a `failure_kind` field, plumbed a typed `SummarizationFailure` through `PreparedContext.summarization_failure` so `persistent.rs` can emit the structured audit event. **Discovery during review**: the existing `ContextSummarizationFailed` audit variant was silently dropped on the fallback path — `prepare_context()` caught summarization errors with `tracing::warn` and returned `Ok(uncompressed_context)`, so the caller never saw the failure and never emitted the audit event the variant was designed for. Commit B fixes that without changing the outward contract (fallback stays non-fatal).

**Commit C (cross-crate `From<LlmError> for CoreError` impls) was gated on "≥2 real sites that would benefit" and skipped** — after Commit B there are zero remaining `.map_err(|e| e.to_string())` calls at cross-crate boundaries, so adding generic wrappers now would be abstraction without call sites. Reconsider when ≥2 appear naturally.

**Process notes for this follow-up:**
- Two rounds of Copilot peer review before implementation (Round 1 caught the name collision and the hidden behavior change; Round 2 caught a `String` vs `&'static str` ambiguity and refined the YAGNI gate).
- Total time: ~30 min including both review rounds + implementation + tests + commits. Roughly the same wall-clock time as Run 7b itself took to *design* the proposal — a useful calibration on the shape-vs-size distinction ("80% of the work is spec; 20% is coding").
