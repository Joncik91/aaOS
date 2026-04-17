# Namespaced backend re-verification *(2026-04-17)*

Not a reflection run — a verification pass to confirm the `NamespacedBackend` kernel launch path still works on current `main` and clear the stale "not yet functional" note in `architecture.md` before Phase F-b (Debian-derivative image) starts leaning on "namespaced-by-default."

## Why

`architecture.md:134` said *"The `clone() + uid_map + pivot_root + exec` launch path is pinned by a unit test but not yet functional; completion requires manual verification on a Linux 5.13+ host."* The 2026-04-15 reflection log (`2026-04-15-namespaced-backend-and-droplet-prep.md`) documented the exact opposite: commits `1d6ec97` and `67c7fc3` completed the child function, `/proc/<pid>/status` showed `Seccomp: 2`, and all four `--ignored` integration tests passed. That drift had been sitting for two days. Before F-b's namespaced-by-default claim becomes load-bearing, re-confirm nothing regressed across the ~20 commits since the 04-15 baseline and correct the doc.

## Setup

Fresh DigitalOcean droplet (user-provisioned per CLAUDE.md cloud runbook). 2 vCPU / 4 GB / Debian 13 / kernel 6.12.43+deb13-amd64 — same kernel as the 04-15 verification host. Kernel features confirmed:

- `CONFIG_USER_NS=y`
- `CONFIG_SECURITY_LANDLOCK=y`
- `kernel.unprivileged_userns_clone=1`
- `user.max_user_namespaces=15519`

Rust 1.95.0 installed via rustup (minimal profile). Source rsynced from A8 at commit `3e1b207`. `cargo build -p aaos-backend-linux --bin aaos-agent-worker` compiles clean in 53 s.

## What held

### Integration tests

```
$ cargo test -p aaos-backend-linux --test namespaced_backend -- --ignored --test-threads=1
running 4 tests
test health_detects_exit ... ok
test launch_reaches_sandboxed_ready ... ok
test stop_is_idempotent ... ok
test worker_cannot_execve ... ok

test result: ok. 4 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.25s
```

Same four tests that pass in the 04-15 run, still green. No code change needed. The replan commit `54cf501`, the scaffold commit `2b8ed6d`, and every commit since 04-15 left the kernel launch path alone.

### Live worker confinement

Added a one-off probe test (`tests/proc_probe.rs`, not committed — reverts below) that launches a worker, reads its `/proc/<pid>/status`, then stops. Output:

```
=== WORKER /proc/7584/status ===
  Name:         aaos-agent-work
  Pid:          7584
  Uid:          0       0       0       0
  Gid:          0       0       0       0
  CapEff:       000001ffffffffff
  NoNewPrivs:   1
  Seccomp:      2
  Seccomp_filters: 2
=== END ===
```

Four things worth calling out:

1. **`NoNewPrivs: 1`** — `prctl(PR_SET_NO_NEW_PRIVS)` fired before Landlock + seccomp. Without this, neither unprivileged Landlock nor unprivileged seccomp would take effect.
2. **`Seccomp: 2`** — `SECCOMP_MODE_FILTER` is active. The worker is in filter mode, not disabled (0) or strict (1).
3. **`Seccomp_filters: 2`** — both stacked filters installed. The "allow-then-deny" ordering lesson from patterns.md ("Stacked seccomp filters require the first filter to allow `SYS_seccomp`") holds. If the first filter hadn't explicitly allowed `seccomp()`, only one filter would be in place here.
4. **`Uid: 0 0 0 0`, `CapEff: 000001...`** — in-namespace root with full user-namespace capabilities. From the parent's perspective the process is unprivileged (that's what `CLONE_NEWUSER` buys). The capabilities listed here are clamped by the seccomp filter — they only grant syscalls the filter allows.

### No regression

20 commits landed between the 04-15 baseline (`67c7fc3`) and today's verification (`3e1b207`). None touched `crates/aaos-backend-linux/src/`. The replan-on-failure work, the deterministic fetcher scaffold, the planner prompt tightening, the CLI `tool FAILED` surfacing — all in the runtime / role / CLI layers. The kernel-boundary code sat untouched, which is exactly the signal you want: substrate code should be stable while higher layers iterate.

## What shipped

- Doc fix: `architecture.md:128-141` rewritten. Replaced *"not yet functional"* with the actual state (verified end-to-end on kernel 6.12.43, 4 tests pass, `NoNewPrivs:1 / Seccomp:2 / Seccomp_filters:2`). Added the 2026-04-17 re-verification date and the commit (`3e1b207`) it was re-checked against.
- This reflection entry, linked from `reflection/README.md`.

The probe test file (`tests/proc_probe.rs`) was created on the droplet only and not copied back to the repo — it's a one-shot evidence-capture test, not a regression. The four canonical integration tests already pin the behavior.

## What did NOT ship (deliberate)

- Not enabling `AAOS_DEFAULT_BACKEND=namespaced` on the existing `.deb` install default. That's explicitly an F-b cloud-image change, not a base-package change — operators installing the `.deb` on their own Debian 13 host don't necessarily have the kernel features (unprivileged user namespaces can be disabled by distro policy, Landlock not universal). F-b's image bakes the prerequisites and can flip the default; the base `.deb` stays conservative.
- Not wiring the broker-side `TryExecve` poke op. `worker_cannot_execve` is still a launch+stop placeholder test. Not on the F-b critical path; deferred.

## Droplet lifecycle

Provisioned 2026-04-17 ~07:30 UTC. Session work <15 min actual compute (most of the wall-clock was cloud-init + apt + rustup). Destroyed by operator after verification. Total spend well under $0.50 per the CLAUDE.md runbook. No secrets placed on the droplet — DeepSeek was not needed for this verification since the tests don't invoke an LLM.

## Phase F-b now unblocked

The remaining scope for F-b is the Packer pipeline itself:

1. Start from upstream Debian 13 cloud image.
2. Preinstall the `.deb` (already built by `cargo deb -p agentd --no-build`).
3. Enable `agentd.service` at first boot.
4. Set `AAOS_DEFAULT_BACKEND=namespaced` in `/etc/default/aaos`.
5. Strip desktop meta-packages.
6. Publish the snapshot to DigitalOcean as the first cloud target.

That's the next session.
