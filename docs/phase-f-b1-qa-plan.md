# Phase F-b sub-project 1 — End-to-End QA Plan

> **For agentic workers:** this is a QA exercise plan, not a code-change plan. Each task is a scenario to run on a fresh DigitalOcean droplet with exact commands, expected outputs, and pass/fail assertions. Work the tasks sequentially — later tasks build on earlier ones (droplet state, installed `.deb`, provisioned secrets). After each task, note the observed result; at the end, roll up into a reflection entry.

**Goal:** Verify that Phase F-b sub-project 1 (reasoning-slot scheduler + per-task TTL) ships correctly on a real Debian 13 host, and that the Phase F-a features it built on (computed orchestration, MCP integration, `.deb` install flow) still work end-to-end after the scheduler + TTL refactor.

**Environment:** Ephemeral DigitalOcean droplet (user-provisioned, IP supplied after plan approval). Debian 13, 2vCPU / 4GB RAM. Passwordless root from A8. Destroyed after the run.

**Tech stack exercised:** `agentd` daemon (v0.0.0), `.deb` packaging, Unix socket JSON-RPC, MCP HTTP+SSE endpoint, DeepSeek LLM API, namespaced backend (dark — just confirmed not-broken), computed orchestration (Planner → PlanExecutor → roles), reasoning-slot scheduler, per-task TTL with hop + wall-clock enforcement.

**Out of scope:** Namespaced backend full exercise (LSM-blocked on DO, documented in `docs/patterns.md`); CI-only guardrails; unit-test-level assertions (those already ran on A8 + GitHub Actions).

---

## Success criteria (the "did we ship" checklist)

By end of run, all of the following must hold. Paste each line into the reflection entry verbatim with ✅ / ❌:

- [ ] Clean Debian 13 droplet accepts the `.deb` via `apt install`, starts `agentd`, socket appears at `/run/agentd/agentd.sock`.
- [ ] `agentd list` from a non-root operator user returns an empty list (no agents yet) without permission errors.
- [ ] `agentd submit "fetch HN and lobste.rs..."` drives a computed-orchestration plan through roles; `fetcher` scaffold writes workspace files; `/data/compare.md` contains real prose from fetched HTML (not training data).
- [ ] **No** capability-denied audit events appear for legitimate role boundaries (roles read only their declared inputs).
- [ ] MCP server endpoint on `127.0.0.1:3781` answers `tools/call` with `submit_goal`, returns a run id, and the SSE stream delivers real audit events.
- [ ] **Scheduler in action:** with `AAOS_MAX_CONCURRENT_INFERENCE=1` and two concurrent goals, the second subtask waits for the first to finish its `complete()` call before starting its own.
- [ ] **TTL hop enforcement:** a goal with `AAOS_DEFAULT_TASK_TTL_HOPS=1` submitted against a multi-subtask plan fails the second-hop subtask with `SubtaskTtlExpired{reason:"hops_exhausted"}` and does not launch the child agent.
- [ ] **TTL wall-clock enforcement:** a goal with `AAOS_DEFAULT_TASK_TTL_WALL_CLOCK_S=5` submitted against a canonical multi-subtask benchmark gets one subtask killed with `SubtaskTtlExpired{reason:"wall_clock_exceeded"}` within ~5s; the dependent does not launch.
- [ ] Journald audit trail shows the full event sequence for the above three runs; no unexpected panics or error lines in `journalctl -u agentd`.

If any row is ❌, fix before reporting "QA passed."

---

## File structure

**No code files produced.** This plan references existing files and produces exactly one artifact at the end:

- `docs/reflection/<today>-f-b1-e2e-qa.md` — reflection entry capturing the outcome, any bugs surfaced, any fixes shipped during the run.

---

## Task 1: Bring the droplet up to a clean baseline

**Prerequisites:** user has pasted the droplet IP. Export it as `$DROPLET` in this session before running any command.

- [ ] **Step 1: Capture IP + sanity SSH**

```bash
export DROPLET=<ip-user-pasted>
ssh -o StrictHostKeyChecking=accept-new root@$DROPLET "uname -a; cat /etc/os-release | head -3"
```

Expected: `Linux ... Debian` and `PRETTY_NAME="Debian GNU/Linux 13 (trixie)"`. If the OS is not Debian 13, stop and ask the user to rebuild the droplet.

- [ ] **Step 2: Update apt + install prerequisites**

```bash
ssh root@$DROPLET "apt-get update -qq && apt-get install -y -qq adduser systemd netcat-openbsd curl jq python3 ca-certificates"
```

Expected: exits 0. `netcat-openbsd` gives us `nc -U` for socket probing; `jq` for parsing JSON responses.

- [ ] **Step 3: Create a non-root operator account**

```bash
ssh root@$DROPLET "adduser --disabled-password --gecos '' testop && usermod -aG sudo testop"
```

Expected: `adduser` reports `Adding user 'testop'`.

- [ ] **Step 4: Note baseline**

```bash
ssh root@$DROPLET "dpkg -l | grep -i aaos || echo 'aaos not installed yet'; systemctl status agentd 2>&1 | head -3"
```

Expected: `aaos not installed yet` and `Unit agentd.service could not be found`. If either diverges, stop — droplet isn't pristine.

---

## Task 2: Build the `.deb` on the droplet (release, with `mcp` feature)

We build on the droplet rather than shipping a pre-built `.deb` because (a) A8 doesn't have `cargo-deb` plumbing set up, (b) building on target matches the real release flow, (c) any build-time divergence gets surfaced here.

- [ ] **Step 1: Install Rust + build deps**

```bash
ssh root@$DROPLET "apt-get install -y -qq build-essential pkg-config libssl-dev git"
ssh root@$DROPLET "curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --default-toolchain stable --profile minimal"
ssh root@$DROPLET "source ~/.cargo/env && cargo install cargo-deb --locked"
```

Expected: `cargo-deb v2.x.x` in the output of the last command. If `cargo install` runs >15min, something is wrong — check disk / network.

- [ ] **Step 2: Ship the repo to the droplet**

```bash
rsync -az --delete \
  --exclude='target/' --exclude='.git/' --exclude='node_modules/' \
  /root/apps/aaOS/ root@$DROPLET:/root/aaOS/
```

Expected: transfer completes, no errors. Under 30s on a typical connection; if slow, the exclude list may have missed a big dir.

- [ ] **Step 3: Build the release binary + `.deb` with the `mcp` feature**

```bash
ssh root@$DROPLET "cd /root/aaOS && source ~/.cargo/env && cargo deb -p agentd -- --features mcp 2>&1 | tail -5"
```

Expected: final line is something like `target/debian/aaos_0.0.0-1_amd64.deb`. Build takes ~5 minutes on a 2vCPU droplet. If it fails with a `#[cfg(debug_assertions)]` leak or missing dep, stop and fix in-tree before retrying.

- [ ] **Step 4: Capture build metadata for the reflection entry**

```bash
ssh root@$DROPLET "ls -la /root/aaOS/target/debian/*.deb && /root/aaOS/target/debian/../release/agentd --version 2>&1 || echo 'version flag missing'"
```

Record the `.deb` filename + size. `agentd --version` may not be wired (not required); fine either way.

---

## Task 3: Install the `.deb` and verify daemon health

- [ ] **Step 1: Install via `apt install`**

```bash
ssh root@$DROPLET "apt install -y /root/aaOS/target/debian/aaos_*.deb 2>&1 | tail -5"
```

Expected: last few lines include `Setting up aaos` and no `ERROR` substrings. If `apt install` fails with a missing dep, capture the error and stop — that's a packaging bug.

- [ ] **Step 2: Systemd service health**

```bash
ssh root@$DROPLET "systemctl status agentd 2>&1 | head -12"
```

Expected: `Active: active (running)` within 2 seconds. If `Active: failed`, immediately run `ssh root@$DROPLET "journalctl -u agentd -n 50 --no-pager"` and stop for triage.

- [ ] **Step 3: Socket present with correct permissions**

```bash
ssh root@$DROPLET "ls -la /run/agentd/agentd.sock && stat -c '%a %U:%G' /run/agentd/agentd.sock"
```

Expected: socket exists, mode `0660`, owner `aaos:aaos`. If mode is `0755`-ish, the umask-fix regression is back (see `docs/patterns.md` "End-to-end verification as an unprivileged user").

- [ ] **Step 4: Operator user in `aaos` group**

```bash
ssh root@$DROPLET "usermod -aG aaos testop && id testop"
```

Expected: `testop` gains the `aaos` supplementary group. Without this, no non-root CLI call can reach the socket.

- [ ] **Step 5: Operator CLI against the socket**

```bash
ssh root@$DROPLET "sudo -u testop -i bash -c 'newgrp aaos <<< \"agentd list\"' 2>&1 | tail -5"
```

Expected: empty list (no agents yet) or `No agents running`. No permission denied. If that fails, confirm the socket mode + group in step 3.

---

## Task 4: Provision the DeepSeek key (operator will paste it in-session)

The key should NOT be committed anywhere. Operator pastes it into the terminal; we write it mode 0600 to `/etc/default/aaos`; shred + rotate after the run.

- [ ] **Step 1: Prompt the user for the key**

Ask the user (in chat) to paste a fresh DeepSeek key, OR confirm reuse of a rotating one. Do NOT proceed without explicit acknowledgement.

- [ ] **Step 2: Install at `/etc/default/aaos` mode 0600**

Use the pasted value as `<KEY>` inline:

```bash
ssh root@$DROPLET "cat > /etc/default/aaos <<EOF
DEEPSEEK_API_KEY=<KEY>
AAOS_LOG_LEVEL=info
EOF
chmod 0600 /etc/default/aaos && chown root:root /etc/default/aaos"
```

Expected: no output from the command; then:

```bash
ssh root@$DROPLET "stat -c '%a %U:%G %n' /etc/default/aaos"
```

Expected: `600 root:root /etc/default/aaos`.

- [ ] **Step 3: Restart the daemon so it picks up the env**

```bash
ssh root@$DROPLET "systemctl restart agentd && sleep 2 && systemctl status agentd | head -5"
```

Expected: `Active: active (running)` and the `journalctl` line showing DeepSeek client construction (look for `llm_client configured` or similar).

---

## Task 5: Baseline goal — confirm Phase F-a (computed orchestration) still works

Runs the same canonical benchmark from the 2026-04-17 role-wiring work. If this regresses, the scheduler/TTL refactor broke something.

- [ ] **Step 1: Submit the canonical "fetch HN + lobste.rs" goal**

```bash
ssh root@$DROPLET "sudo -u testop -i bash -c 'newgrp aaos <<< \"mkdir -p /data && chmod 777 /data && agentd submit \\\"fetch HN and lobste.rs, compare the top 3 stories on each, write to /data/compare.md\\\" 2>&1 | tee /tmp/submit-5.log\"'"
```

Expected: command returns within ~60s (generous upper bound). `tee`'d log ends with a terminal `{"kind":"end",...}` frame showing aggregated token usage.

- [ ] **Step 2: Workspace files exist, non-empty**

```bash
ssh root@$DROPLET "ls -la /var/lib/aaos/workspace/ | head -10; for f in /var/lib/aaos/workspace/*/hn.html /var/lib/aaos/workspace/*/lobsters.html; do echo -n \"\$f: \"; wc -c \"\$f\" 2>/dev/null || echo MISSING; done"
```

Expected: both html files exist, each > 5 KB. If `MISSING`, the fetcher scaffold is broken.

- [ ] **Step 3: Output contains real, non-training prose**

```bash
ssh root@$DROPLET "wc -c /data/compare.md && head -30 /data/compare.md"
```

Expected: file exists, > 2 KB, mentions at least one specific story title visible in the live HN/Lobste.rs homepages at submission time (any operator-visible recent keyword).

- [ ] **Step 4: No unexpected capability denials**

```bash
ssh root@$DROPLET "grep -i 'capability.*denied\|ToolInvocationDenied' /var/log/daemon.log 2>/dev/null | head -20 || journalctl -u agentd --since '5 minutes ago' | grep -i 'denied' | head -20"
```

Expected: no output, OR a very short list that all match declared role boundaries (e.g. writer trying to `file_list /data`, which is expected-ish). Flag anything unexpected in the reflection.

---

## Task 6: Scheduler observation — concurrent goal serialization

Verifies that `ReasoningScheduler` actually throttles LLM calls. With `AAOS_MAX_CONCURRENT_INFERENCE=1` and two concurrent submissions, the second should wait for the first's active `complete()` call to finish before starting its own.

- [ ] **Step 1: Reconfigure daemon with slot=1**

```bash
ssh root@$DROPLET "sed -i '/AAOS_MAX_CONCURRENT_INFERENCE/d' /etc/default/aaos && echo 'AAOS_MAX_CONCURRENT_INFERENCE=1' >> /etc/default/aaos && systemctl restart agentd && sleep 2"
```

- [ ] **Step 2: Fire two submissions in parallel, capture timestamps**

```bash
ssh root@$DROPLET "sudo -u testop -i bash -c 'newgrp aaos <<< \"(agentd submit \\\"summarize the word hello in one sentence\\\" > /tmp/goal-a.log 2>&1 & echo \$! > /tmp/pid-a) ; (agentd submit \\\"summarize the word world in one sentence\\\" > /tmp/goal-b.log 2>&1 & echo \$! > /tmp/pid-b) ; wait\"'"
```

Expected: both return 0. Tee'd logs are separate.

- [ ] **Step 3: Check timestamps on the first audit event per run**

```bash
ssh root@$DROPLET "journalctl -u agentd --since '2 minutes ago' | grep -E 'AgentExecutionStarted|AgentExecutionCompleted' | head -20"
```

Expected: the second goal's `AgentExecutionStarted` is AFTER the first goal's `AgentExecutionCompleted`, OR the two interleave at a per-turn granularity but never with two concurrent `complete()` calls in flight.

**How to really prove serialization**: look at two audit-log entries back-to-back; if B's `AgentExecutionStarted` comes before A's `AgentExecutionCompleted` AND there are two distinct agent_ids, the test is inconclusive (they may have overlapped between turns). That's acceptable behavior — one slot = one `complete()` call, not one slot = one agent. Document the observed interleave in the reflection.

- [ ] **Step 4: Reset for next task**

```bash
ssh root@$DROPLET "sed -i '/AAOS_MAX_CONCURRENT_INFERENCE/d' /etc/default/aaos && systemctl restart agentd && sleep 2"
```

---

## Task 7: TTL hop exhaustion — goal refused before deep recursion

- [ ] **Step 1: Set `max_hops=1` in env**

```bash
ssh root@$DROPLET "echo 'AAOS_DEFAULT_TASK_TTL_HOPS=1' >> /etc/default/aaos && systemctl restart agentd && sleep 2"
```

- [ ] **Step 2: Submit a multi-subtask goal (forces hops > 1)**

```bash
ssh root@$DROPLET "sudo -u testop -i bash -c 'newgrp aaos <<< \"agentd submit \\\"fetch https://example.com and then analyze what you found\\\" 2>&1 | tee /tmp/submit-7.log\"'"
```

Expected: terminal `{"kind":"end",...}` frame arrives. The run completes quickly (seconds, not a minute) because downstream subtasks get refused.

- [ ] **Step 3: Verify `SubtaskTtlExpired` in the audit trail**

```bash
ssh root@$DROPLET "grep -i 'SubtaskTtlExpired\|hops_exhausted' /tmp/submit-7.log | head -10"
```

Expected: at least one `SubtaskTtlExpired` event with `reason: "hops_exhausted"`. If the LLM happened to produce a single-subtask plan (fetcher only), this task is inconclusive — re-submit with a more complex goal like `"fetch https://example.com, analyze the content, then write a summary to /tmp/out.md"` to force multiple subtasks.

- [ ] **Step 4: Reset for next task**

```bash
ssh root@$DROPLET "sed -i '/AAOS_DEFAULT_TASK_TTL_HOPS/d' /etc/default/aaos && systemctl restart agentd && sleep 2"
```

---

## Task 8: TTL wall-clock expiry — subtask killed mid-run

- [ ] **Step 1: Set a tight wall-clock in env**

```bash
ssh root@$DROPLET "echo 'AAOS_DEFAULT_TASK_TTL_WALL_CLOCK_S=5' >> /etc/default/aaos && systemctl restart agentd && sleep 2"
```

- [ ] **Step 2: Submit a goal that takes > 5s per subtask**

```bash
ssh root@$DROPLET "sudo -u testop -i bash -c 'newgrp aaos <<< \"agentd submit \\\"read https://en.wikipedia.org/wiki/Operating_system and produce a 500-word summary to /tmp/os-summary.md\\\" 2>&1 | tee /tmp/submit-8.log\"'"
```

Expected: the run ends within ~30s with a TTL expiry (any subtask that itself takes > 5s of LLM time gets cancelled).

- [ ] **Step 3: Verify `wall_clock_exceeded` event**

```bash
ssh root@$DROPLET "grep -i 'SubtaskTtlExpired\|wall_clock_exceeded' /tmp/submit-8.log | head -10"
```

Expected: at least one `SubtaskTtlExpired { reason: "wall_clock_exceeded" }`. If the LLM call just happened to complete fast (cached, easy), re-run with an even harder prompt (e.g. `"compare 5 Linux distributions in detail, write to /tmp/out.md"`) and `AAOS_DEFAULT_TASK_TTL_WALL_CLOCK_S=2`.

- [ ] **Step 4: Verify dependent cascade — dependent subtasks don't launch**

```bash
ssh root@$DROPLET "grep -E 'SubtaskStarted|SubtaskTtlExpired' /tmp/submit-8.log | head -20"
```

Expected: for every `SubtaskTtlExpired`, the subtask that depended on it has NO matching `SubtaskStarted` line. If a dependent started anyway, the cascade is broken — stop for triage.

- [ ] **Step 5: Reset**

```bash
ssh root@$DROPLET "sed -i '/AAOS_DEFAULT_TASK_TTL_WALL_CLOCK_S/d' /etc/default/aaos && systemctl restart agentd && sleep 2"
```

---

## Task 9: MCP server API — smoke test over loopback

Enables the built-in MCP server, which exposes `submit_goal`, `get_agent_status`, `cancel_agent` via HTTP JSON-RPC on `127.0.0.1:3781`. SSE stream for audit events.

- [ ] **Step 1: Enable server in `mcp-servers.yaml`**

```bash
ssh root@$DROPLET "cat > /etc/aaos/mcp-servers.yaml <<EOF
client:
  servers: []

server:
  enabled: true
  bind: \"127.0.0.1:3781\"
EOF
systemctl restart agentd && sleep 3"
```

- [ ] **Step 2: Submit a goal via MCP**

```bash
ssh root@$DROPLET "curl -s -X POST http://127.0.0.1:3781/mcp \
  -H 'Content-Type: application/json' \
  -d '{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"tools/call\",\"params\":{\"name\":\"submit_goal\",\"arguments\":{\"goal\":\"say hello in one word\"}}}' | jq ."
```

Expected: response is a JSON-RPC success with `result.content[0].text` containing `{"run_id":"<uuid>"}`. Capture the run_id for step 3.

- [ ] **Step 3: SSE stream of audit events**

```bash
# Replace <UUID> with the run_id from step 2.
ssh root@$DROPLET "timeout 20 curl -s -N http://127.0.0.1:3781/mcp/events?run_id=<UUID> | head -30"
```

Expected: at least one `data: {"event":"...","agent_id":"...",...}` SSE frame within 20s. If the stream hangs with no events, the SSE filter-by-agent-id is broken (commit `9d12206` territory).

- [ ] **Step 4: `get_agent_status` reports terminal state**

```bash
ssh root@$DROPLET "curl -s -X POST http://127.0.0.1:3781/mcp -H 'Content-Type: application/json' -d '{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"tools/call\",\"params\":{\"name\":\"get_agent_status\",\"arguments\":{\"run_id\":\"<UUID>\"}}}' | jq ."
```

Expected: `content[0].text` is one of `\"completed\"`, `\"running\"`, or `\"failed\"`.

---

## Task 10: Audit trail + logs — no silent panics, no surprises

- [ ] **Step 1: Full journal since install**

```bash
ssh root@$DROPLET "journalctl -u agentd --since '30 minutes ago' --no-pager | tee /tmp/agentd-run.log | tail -40"
```

Expected: no `panic`, no `RUST_BACKTRACE` lines, no repeated `ERROR` bursts. Warnings (e.g. "role parameter is not yet supported") are OK.

- [ ] **Step 2: Grep for red flags**

```bash
ssh root@$DROPLET "grep -iE 'panic|backtrace|unwrap.*None|index out of bounds' /tmp/agentd-run.log | head -20"
```

Expected: empty. Any hit is a real bug — capture and stop.

- [ ] **Step 3: Collect the session logs for the reflection entry**

```bash
mkdir -p /tmp/qa-collect
scp root@$DROPLET:/tmp/submit-*.log /tmp/qa-collect/
scp root@$DROPLET:/tmp/agentd-run.log /tmp/qa-collect/
scp root@$DROPLET:/tmp/goal-*.log /tmp/qa-collect/ 2>/dev/null || true
ls -la /tmp/qa-collect/
```

Expected: all task logs copied to A8 for analysis. These feed into the reflection entry but do NOT get committed to the repo (CLAUDE.md's rule: no agent output in git).

---

## Task 11: Wrap-up — key rotation + droplet destroy hand-off

- [ ] **Step 1: Shred the API key on the droplet**

```bash
ssh root@$DROPLET "shred -u /etc/default/aaos"
```

Expected: file gone. Agentd will likely log an error on next restart — we're about to destroy the droplet so fine.

- [ ] **Step 2: Tell the user to destroy the droplet**

Explicit chat message to the user: "QA complete. Please destroy the droplet at <IP> and rotate the DeepSeek key." Do NOT attempt to destroy via `doctl` or similar — destruction is the user's call per the cloud-compute rules in CLAUDE.md.

- [ ] **Step 3: Write the reflection entry**

Create `docs/reflection/<today>-f-b1-e2e-qa.md` using the template from `docs/reflection/README.md`. Populate:

- **Setup:** droplet spec (2vCPU/4GB Debian 13), install method (cargo deb + apt install), DeepSeek key provisioning.
- **What worked:** every ✅ from the success criteria checklist at the top of this plan, paraphrased with the actual command-line evidence.
- **What the run exposed:** any ❌, surprises, audit events outside expectations. Flag bugs explicitly with "FOUND BUG: ..." so future greppers can find them.
- **What shipped:** commits produced during the run if any fixes were made mid-QA (usually none — a clean run produces no new commits).
- **Cost:** droplet billed hours, a few cents. DeepSeek token math is NOT a cost figure (see `docs/CLAUDE.md` rule 2) — only quote the provider dashboard number if the user looks it up post-run.
- **Lessons worth lifting:** anything that generalizes goes into `docs/patterns.md`; single-run specifics stay here.

Commit the reflection + any patterns update in one commit:

```bash
cd /root/apps/aaOS
git add docs/reflection/<today>-f-b1-e2e-qa.md docs/reflection/README.md docs/patterns.md
git commit -m "docs: reflection — Phase F-b sub-project 1 e2e QA on DO droplet"
git push
```

- [ ] **Step 4: Final checklist**

Paste each line from "Success criteria" at the top of this plan into the reflection entry with its observed ✅ / ❌. If all rows are ✅, declare QA passed. If any ❌, the next session's first task is to diagnose.

---

## Self-review

**Task coverage vs success criteria:**

- Droplet baseline + `.deb` install + socket + operator access → Tasks 1, 2, 3.
- Baseline computed orchestration → Task 5.
- MCP endpoint → Task 9.
- Scheduler serialization → Task 6.
- TTL hops → Task 7.
- TTL wall-clock + cascade → Task 8.
- Audit trail clean → Task 10.
- Reflection + wrap-up → Task 11.

All 9 rows in the success-criteria checklist have a dedicated task.

**Placeholder scan:**

- `<IP>` and `<UUID>` and `<KEY>` are real runtime variables the operator fills in; not placeholders in the "write real code later" sense.
- `<today>` in reflection filenames needs to be replaced with the actual date. That's explicit.

**Type consistency:**

- `AAOS_MAX_CONCURRENT_INFERENCE`, `AAOS_DEFAULT_TASK_TTL_HOPS`, `AAOS_DEFAULT_TASK_TTL_WALL_CLOCK_S` appear in the same form across all tasks — matches the names the code reads in `crates/aaos-llm/src/scheduled.rs` and `crates/aaos-runtime/src/plan/planner.rs` (`default_task_ttl`).
- Audit event variants `SubtaskTtlExpired`, reasons `"hops_exhausted"` / `"wall_clock_exceeded"` match the spec-compliant forms from `crates/aaos-core/src/audit.rs` + Tasks 7/8 of the implementation plan.

Plan is internally consistent. Ready to execute once the user provides the droplet IP.
