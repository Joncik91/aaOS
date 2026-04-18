# Phase F-b sub-project 1 — bug-fix re-verification on a fresh droplet *(2026-04-18)*

**Integration commits under test:** `4201ed1` through `8b5bb36` (7 bug-fix commits from the 2026-04-18 QA run), plus `b24139b` and `ce4582a` shipped during this re-verification. Fresh DO droplet (Debian 13 / kernel 6.12.43) because the previous one had been destroyed. Scope: confirm the 4 fixes that have user-visible effects (BUGs #1, #2, #5, #7) actually behave on a real host; the 3 code-internal fixes (#3 tracing, #4 docs, #6 scaffold wall-clock) are observable in-tree and don't need re-exercising.

## What the run exposed

**BUG #1's fix was itself broken.** The shipped fix (`80497ac`) added `pre-build-command = "cargo build --release -p aaos-backend-linux --bin aaos-agent-worker"` to `[package.metadata.deb]`. cargo-deb 3.6.3 does not recognize `pre-build-command` — it's not in the accepted-fields list. Running `cargo deb -p agentd` fails with a TOML parse error, not the original missing-asset error. The fix was invented from memory and never exercised on a real host. **Fixed for real in `b24139b`:** added a `packaging/build-deb.sh` wrapper that builds both binaries in sequence, with a comment in the Cargo.toml pointing to it.

**BUG #2's fix was partially broken.** The shipped fix (`9f7a324`) flipped `systemd-units = { enable = true, start = true }` in `[package.metadata.deb]`, assuming cargo-deb would append `deb-systemd-helper` directives to `postinst`. cargo-deb does append those — **unless** `maintainer-scripts` is also set, in which case it copies the maintainer scripts verbatim and silently skips systemd-units injection. Result on a clean `apt install`: daemon installed but `inactive (dead)` and `disabled`. **Fixed for real in `ce4582a`:** do the systemd wiring inline in our own `postinst` + add a matching `prerm` for the stop path, with a comment documenting the cargo-deb gotcha.

**Both regressions were caused by the same meta-pattern.** The original QA reflection lifted "First on-production exercise finds what unit + CI never will" to `patterns.md`. The bug-fix sweep that followed didn't apply that rule to itself — neither `cargo check` nor the test suite parses the `[package.metadata.deb]` table, and no CI job runs a fresh-install smoke test. Both fixes looked correct and green, and both failed on the first production install.

## What shipped (this session)

- `b24139b` — `packaging/build-deb.sh` wrapper; remove bogus `pre-build-command`.
- `ce4582a` — inline systemd enable/start in postinst + add prerm.

Both push'd; CI green.

## What re-verified cleanly

- `packaging/build-deb.sh --features mcp --no-default-features` produced a 4.4 MB `.deb` in one command on the fresh droplet, ~3 min total release build time. **BUG #1 ✅.**
- `apt install .../aaos_0.0.0-1_amd64.deb` on a clean host resulted in `systemctl is-active agentd = active` + `is-enabled = enabled` + socket at `/run/agentd/agentd.sock` mode 0660 aaos:aaos — no manual `systemctl enable --now` needed. **BUG #2 ✅.**
- Baseline computed-orchestration goal (fetch HN + lobsters → /data/compare.md) ran in 100s; workspace HTML files 34+50 KB; `compare.md` contains live story titles from today's HN homepage. No regression from the 7-fix sweep. **Baseline ✅.**
- With `AAOS_DEFAULT_TASK_TTL_HOPS=1`, the hop chain fired as designed. Client-side stream shows:
  ```
  [22:18:47] aad602c0    spawned fetcher
  [22:18:47] aad602c0    tool: web_fetch {"url":"https://example.com"}
  [22:18:47] 00000000    TTL expired (hops_exhausted): analyze
  ```
  Depth-0 fetcher launched; depth-1 `analyze` subtask refused before launch with `SubtaskTtlExpired{hops_exhausted}` visible in the default CLI view (not just verbose). **BUGs #5 + #7 verified end-to-end ✅.**

## What didn't need re-verification on the droplet

- **BUG #3 (SchedulerView tracing)** — code change is self-evident; unit tests unchanged. Would be visible in `journalctl -u agentd` under `RUST_LOG=debug` if needed. Not exercised this run.
- **BUG #4 (scheduler-coverage docs)** — pure doc change.
- **BUG #6 (wall-clock wraps scaffolds)** — new inline unit test `wall_clock_expiry_kills_scaffold_subtask` covers it; replicating on a droplet adds no new information because scaffold wall-clock kill is a tokio::select! race, well-contained.

## Cost

<1 hour of droplet time (<$0.05). Not measured on the provider dashboard. One DeepSeek key used for the baseline goal + the hop-TTL goal; shredded on exit, user rotates provider-side.

## Lessons worth lifting

- **Your bug-fix commit is a production change.** Treat it as such. The original QA reflection shipped with "next actions: fix 7 bugs" and the bug-fix sweep followed that list without re-running the on-host e2e that surfaced the bugs in the first place. Two of the seven fixes were wrong; both would have been caught by a 10-minute re-verification pass on a single `apt install`. Add "re-verify bug fixes on a fresh droplet" to the same Definition-of-Done rule that `patterns.md` now names for feature ships. Already generalized — no new patterns entry.
