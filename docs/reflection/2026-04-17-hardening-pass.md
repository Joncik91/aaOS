# Post-run-12 hardening pass *(2026-04-17)*

**Integration commits:** `648e4c7`, `20878da`, `086f381`, `7d8b694`, `69c780b`

Not a self-build run. After run 12 shipped I asked myself what was loose in the existing feature set and went through it systematically. Six items landed; five survived verification against the actual code. Worth a short entry because one of them is a latent bug that had been in production for weeks and one of them is a real enforcement change.

## What shipped

1. **Executor-level output contract (`086f381`).** A role whose capabilities include a `file_write: {<param>}` grant *without* a trailing `/*` is now treated as declaring a single-path output. After a subtask returns `Ok`, the executor resolves those grants and checks each path exists. If any is missing, the subtask is converted to `SubtaskCompleted{success:false}` + `Correctable` so the replan loop can retry. Closes the run-12 failure mode where the builder said "complete" with the report unwritten and the executor believed it.

2. **Orphan-agent abort (`20878da`).** `handle_submit_streaming` now calls `exec_task.abort()` on client disconnect (write failure or broadcast close). Previously the spawned `executor.run` task was detached and kept driving its agent subtree after the CLI disappeared — run 12's trace had a ~10-minute zombie builder writing placeholder files in a scratch dir. Bootstrap branch unchanged — Bootstrap is intentionally persistent across submits.

3. **`web_fetch` streams to cap (`7d8b694`).** Prior implementation called `response.bytes().await` which buffers the whole body before applying `max_bytes`. A 100 MB response allocated 100 MB to return a 50 KB slice; a large-enough body could OOM agentd. Now: early-reject if Content-Length > 10× cap, otherwise stream chunks via `response.chunk().await` and stop at `max_bytes`. Result JSON gains `truncated: bool` and `bytes_read: u64`. Three new tests against a tiny TcpListener mock verify the cap, the early reject, and the under-cap path.

4. **Bounded `repeat_counts` map (`69c780b`).** `ToolInvocation.repeat_counts` grew unbounded over daemon lifetime. Cap at 1024 entries; evict a quarter in bulk when full. Eviction only costs repeat-guard accuracy on the evicted keys — a re-invoked evicted key just has to cross the threshold again.

5. **Two papercuts (`648e4c7`, `69c780b`).** `server.rs:1203` switched from the deprecated `get_tokens` alias to `get_token_handles` so `clippy -- -D warnings` has nothing to flag. The `rejects_message_starting_with_dash` git_commit test dropped its unnecessary `scaffold_git_repo` call — the message check fires before the `.git` existence check, so the test runs on fresh machines without git. And `max_iterations = max_attempts + 10` formula is now documented on `RoleRetry` and at the call site.

## The one I got wrong

An earlier pass of this audit flagged an "output-guard race" — a claim that the executor's `check_declared_outputs_exist` could run before the agent's `file_write` IPC had finished flushing. Reading the actual `file_write` implementation refuted this: `tokio::fs::write` completes synchronously with respect to the calling await point, the tool's `Ok` return waits on that await, and the agent loop is strictly sequential within a subtask. There is no in-flight write for the guard to race against. **Lesson:** speculate, then open the file. The guard I shipped is not racing anything.

## Signal to lift

The audit prompt ("find bugs in aaOS") returned 9 items on the first pass. Three were real (web_fetch, repeat_counts, undocumented formula). Six were plausible-sounding fabrications that dissolved on verification — imagined future-refactor mistakes, imagined proxy-layer concerns, imagined test-parallelism races. This matches prior observations about unconstrained-generation audit workflows: **the yield on "find bugs" is positive but the false-positive rate is high enough that every claim needs code-reading before it's reported, let alone acted on.** The calibrated form is "I see pattern X that *might* imply bug Y — let me read file Z to check." Not "bug Y exists."

## What this doesn't change

- The plan-complete checklist in the builder role prompt is now redundant with the executor-level guard, but I left it in. The prompt costs ~50 tokens; the executor guard is the enforcement, the prompt is the hint. Together they cover belt + suspenders.
- The scope of the output contract is narrow by design: only `file_write: {param}` with no `/*` suffix. If a future role writes output via a *directory* grant and a filename convention (e.g., `file_write: {workspace}/*` and "always writes `summary.md`"), the guard won't catch it. That's fine — making the rule more general would over-fire on legitimate directory-writable roles like the fetcher.

## Cost

~90 minutes of Opus work, self-directed. No droplet. All fixes verified against unit tests on A8 (490 passed, 0 failed, 15 ignored). No user review before commit — the fixes are small, well-scoped, and individually tested.
