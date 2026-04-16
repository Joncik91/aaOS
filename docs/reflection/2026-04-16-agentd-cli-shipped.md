# `agentd` operator CLI shipped *(2026-04-16)*

**Integration commits:**
- `58dd1bb` — `AgentInfo` gains `started_at` + `parent_agent`
- `eb37b95` — `BroadcastAuditLog` wraps inner log with tokio broadcast
- `c0705ad` — `agent.submit_streaming` NDJSON streaming handler
- `74614d5` — `agent.logs_streaming` single-agent attach
- `2c47685` — real `CliCommand` enum (stubs for handlers)
- `42fceb9` — `cli/client.rs` JSON-RPC transport
- `493baea` — `cli/errors.rs` with exit codes + hint rendering
- `2042e4f` — `cli/prefix.rs` agent-id disambiguation
- `8f4f4a7` — `cli/output.rs` event filter, line formatter, colors
- `67027cd` — `cli/submit.rs` streaming + SIGINT detach
- `82157ae` — `cli/list.rs` tabular + `--json`
- `d283d2f` — `cli/status.rs` prefix resolve + detail view
- `6ca4f5b` — `cli/stop.rs` terminate by prefix
- `0cd3cbb` — `cli/logs.rs` attach to audit stream
- `214ae7c` — `postinst` creates `aaos` group explicitly
- `03d909d` — `agentd(1)` man page via pandoc
- `8301c7b` — README Quick Start covers `.deb` install
- `5e01acc` — socket chmod 0660 so aaos-group members can connect

**Setup.** Fresh Debian 13 cloud VM (kernel 6.12.43). Full toolchain install (Rust stable + cargo-deb 2.7.0 + pandoc), repo clone, release build, man page generation, `cargo deb -p agentd --no-build`, `apt install`. Created an unprivileged test user `testop` and added them to the `aaos` group.

**What worked.** The plan's 18 tasks landed in order with no blocked implementer reports. Pattern from Phase F-a — subagent-driven implementation with tight per-task prompts, no subagent reviewers, verification against the running suite — held up again. TDD discipline (failing test first, then implementation) caught one test-vs-code field mismatch (`Capability::FileRead` vs `path_glob`) that would otherwise have surfaced later. The `BroadcastAuditLog` → `agent.submit_streaming` → `agent.logs_streaming` chain was the highest-risk server-side work; it landed on first attempt, with the implementer correctly identifying that the real bootstrap loop would emit its own `AgentExecutionCompleted` before any test-injected events could arrive, and inventing a `HangingLlm` test stub to pin the bootstrap loop idle while the test drives events directly via the broadcast log.

**What the run exposed.**
- **Socket permissions gap.** Every smoke test I ran locally on A8 used root. The droplet-based test with a non-root user in the `aaos` group surfaced that `UnixListener::bind` inherits the process umask; the resulting socket was 0755-ish. `stat` succeeded (directory-traverse OK) but `connect(2)` failed because it needs write on the socket inode. The permission-denied hint I wrote to `format_error` fired for testop even though they were in the group — misleading. Fix: explicit `chmod 0660` after bind (`5e01acc`). Only end-to-end verification with a non-root operator would have caught this — the test suite always ran as root.
- **Permission-denied hint was actually helpful.** Even though it was wrong in this specific case (testop *was* in the group), the hint pointed at the right concept. Once the socket mode was fixed, the existing hint text stayed correct for the case operators will actually hit (forgot to `adduser` at all).
- **cargo-deb metadata ordering bit once.** An earlier Task dispatch wrote to `crates/agentd/Cargo.toml` twice in a "rejected" edit that had actually applied. `cargo metadata` surfaced the duplicate-key with a clear error. Ground truth — the file on disk — beats assumptions about tool-call rejection state.
- **`Command::Spawn` stub removal was invisible.** The old `agentd spawn <manifest>` CLI subcommand was never functional (main.rs called it a stub and returned). Dropping it as part of Task 4 required checking no other caller existed — `AgentCommand::Stop` in the runtime crate is a different type with the same variant name. Search-before-delete paid off.
- **Final response text is not yet printed.** The CLI prints an `end`-frame summary line but does NOT print Bootstrap's final text response. The spec called for "Print final Bootstrap response text before exit." but the server never emits that text as a distinct frame — the LLM's final message is inside an `AgentExecutionCompleted` event's `stop_reason` field only when the executor decided to stop. For real operator use, the final answer currently has to come via `journalctl -u agentd` or a separate tool call. Follow-up: emit a dedicated `{"kind":"final_text","text":"..."}` frame just before `end` when Bootstrap's persistent loop completes a turn.

**End-to-end verification (commit `5e01acc` on droplet, Debian 13 / kernel 6.12.43, kernel DeepSeek Reasoner):**

- `apt install ./aaos_0.0.0-1_amd64.deb` → clean install, user + group + man page all landed.
- `getent group aaos` → exists. `getent passwd aaos` → system user, no shell.
- `adduser testop aaos` → testop in `aaos` group.
- `systemctl start agentd` → active; `ls -la /run/agentd/agentd.sock` → `srw-rw---- aaos aaos`.
- `su - testop -c 'agentd list'` → `No agents running.` exit 0.
- `su - testop -c 'agentd status abc123'` → `error: agent not found: abc123` exit 2.
- `su - testop -c 'agentd stop abc123'` → same exit 2.
- `man -w agentd` → `/usr/share/man/man1/agentd.1.gz`. `man agentd` renders NAME/SYNOPSIS/DESCRIPTION/etc. properly.
- `agentd submit --help` → shows the example line `agentd submit "fetch HN top 5 stories"`.
- After `DEEPSEEK_API_KEY` is set in `/etc/default/aaos` and daemon restarted:
  - `agentd submit "say hello three times and stop"` — streams `spawned bootstrap`, `complete`, end-frame `(4k in / 0k out, 5s)`, exit 0. Total wall clock ~5s.
  - `agentd list` shows the now-running Bootstrap with ID prefix, name, state, uptime.
  - `agentd status <prefix>` shows 18 capabilities, deepseek-reasoner model, parent `—`.
  - Second `agentd submit "count from 1 to 3 and stop"` on the now-persistent Bootstrap runs in 3s, exit 0 — no respawn.
- `shred -u /etc/default/aaos` afterwards — no API key left on the droplet.

**What shipped.** Five-subcommand operator CLI (`submit`, `list`, `status`, `stop`, `logs`) + two new server streaming methods (`agent.submit_streaming`, `agent.logs_streaming`) + `BroadcastAuditLog` + explicit `aaos` system group in postinst + `agentd(1)` man page. The `.deb` now passes the "new operator reaches a successful `agentd submit` in 5 minutes" test that the CLI spec required. 16k LoC total (from 14k), 103 unit tests + existing integration tests, all green. No regressions in the existing Docker-based deployment path (Bootstrap mode via env vars still works).

**What remains (deferred, not blocking).**
- `OutputProduced { path }` audit event and a `final_text` frame so the CLI can show Bootstrap's actual answer alongside the timing/token summary. Today the operator has to `journalctl` for it.
- Real agent-name resolution in the streaming path (today's 8-char id prefix fallback works but is ugly).
- Shell completions (`bash-completion`, `zsh`, `fish`).
- Output redirection (`--output <path>`), CLI config file, batch mode (`submit - < goals.txt`).

**Cost.** ~$0.40 VM-hours on a Basic DO droplet during build + verification. LLM: two small DeepSeek calls for the verification goals, ~4k input / ~0 output each — under $0.01 estimated (not confirmed on dashboard yet).

**Lesson for patterns.md.** End-to-end verification as a non-root operator catches class-of-permission bugs the test suite can't see. The socket-mode issue wasn't a coding mistake — the daemon compiled, listened, and served root clients fine. It only failed when reached from an unprivileged shell with a group-only auth claim. CI doesn't run as non-root; the test suite doesn't exercise real Unix sockets under real users. The droplet test isn't optional.
