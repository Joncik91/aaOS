# Security

aaOS is a capability-enforced agent runtime. Capability bypasses, sandbox escapes, secret-handling regressions, and audit-trail tampering are the classes of issue I most want to hear about.

## Security model — what aaOS defends against

Agents run as processes under a runtime (`agentd`) that mediates all sensitive operations through capability tokens. Tokens are issued at spawn, narrowable-only on delegation, revocable at runtime, and audited on every use. The security posture today, by reachable threat class:

### Capability token forgery

aaOS's capability claim is that an agent cannot use a capability it wasn't granted — either by construction, fabrication, or delegation. That claim decomposes into four threat classes:

1. **In-process forgery by tool code — closed.** Tools in `aaos-tools` and any external tool crate receive a `CapabilityHandle`, never a `CapabilityToken`. The handle's inner field is `aaos-core`-private, so tool crates cannot fabricate a handle from a raw integer. The runtime checks handle-to-agent ownership on every resolve, and the `requesting_agent` parameter comes from the runtime-owned `InvocationContext` rather than tool input. Shipped in commits `14a8eae`, `18d14f0`, `884125a`.
2. **Worker-side forgery on `NamespacedBackend` — closed by design, wiring partial.** The namespaced backend runs agent workers in Linux user/mount/IPC namespaces with Landlock + seccomp applied before the agent loop begins. Workers receive no handles in the launch protocol and the handle-field privacy prevents local fabrication. The broker socket is peer-creds-authenticated (`SO_PEERCRED` exact match on pid+uid+gid). Production tool-brokering through the namespaced-worker path is not yet wired — workers today exchange launch + handshake messages only. The claim describes design, not current exercise; when the broker↔worker tool-invocation stream lands, forgery at this layer becomes structurally impossible.
3. **Registry memory tampering by an attacker already executing inside `agentd` — open.** If an attacker gains Rust-level code execution inside the daemon process (memory-safety bug, compromised dep), they can write directly to the capability registry. HMAC signing with the key held in `agentd`'s memory does not fix this — the attacker reads the key from the same address space. Real fixes are OS-level (Landlock ptrace denial and seccomp applied to `agentd` itself) and hardware isolation (MicroVM-per-agent keeping the registry on the host). Both are tracked for Phase F-b / Phase G.
4. **Cross-process / cross-host transport — not applicable today.** No mechanism exists for tokens to leave `agentd`'s address space. `NamespacedBackend` keeps all handles in the host daemon; multi-host swarms and MicroVM-per-agent are not implemented. When such a transport lands, HMAC-signed `(agent_id, capability, constraints, issued_at)` with external key storage (TPM2, memfd_secret, external signer subprocess) is the target.

### Path capability checks

`file_read` / `file_write` / `file_edit` / `grep` check paths against the agent's granted globs via filesystem-resolved canonicalization, which closes symlink-bypass and lexical-only-resolution attacks (run 9 of the self-reflection log surfaced and fixed the original bypass). A residual TOCTOU window exists between the capability check and the actual open — if an agent gains the ability to create symlinks inside a granted writable prefix (no `file_symlink` tool today), this widens; `openat2(RESOLVE_BENEATH)` or `O_NOFOLLOW` tightening is tracked as a hardening item.

### Secret handling

`DEEPSEEK_API_KEY` / `ANTHROPIC_API_KEY` are read from `/etc/default/aaos` (enforced `0600 root:root` by `.deb` postinst), scrubbed from `/proc/<pid>/environ` at daemon startup (libc `getenv` + byte-zero + `remove_var` — plain `remove_var` alone does not clear `env_start..env_end`), and the `aaos` group cannot read the env file. The audit stream does not log request bodies. Key rotation is operator-driven; `systemctl restart agentd` after rotation.

### Audit trail

Every tool invocation, capability grant, denial, revocation, and agent lifecycle event becomes a typed audit event. Events are broadcast via an in-process `BroadcastAuditLog` to any number of subscribers (the operator CLI's `submit` / `logs` streams each hold one) and persisted to stdout in the Docker path / journald in the `.deb` path. Log-sink tampering requires write access to journald or the subscriber's sink, which is the same bar as writing to `agentd`'s own memory. An external signing audit sink is a separate hardening item.

### What we explicitly do not defend against

- Attackers with kernel-level compromise, `CAP_SYS_PTRACE`, or `/proc/<pid>/mem` write access to `agentd`. Those already win.
- LLM hallucination or fabrication. Mitigated at the prompt layer, but not a security boundary.
- Prompt-injected goal text that manipulates *within* the agent's granted capabilities. If a grant is too wide, injection widens the blast radius up to that grant — not beyond it.
- Denial of service. Single-operator system; DoS is easy and not the threat model.

## Reporting a vulnerability

Email **jounes.ds@gmail.com** or use GitHub's private security advisory flow on this repo (Security → Report a vulnerability). Do **not** open a public issue for anything exploitable.

Please include:
- Affected commit SHA (or release tag).
- Repro steps that demonstrate the issue. Minimal failing case > long write-up.
- Suggested fix if you have one — appreciated but not required.

I aim to acknowledge within 72 hours and ship a fix or timeline within two weeks for confirmed issues. Published advisories go out via GitHub Security Advisories with CVE IDs where appropriate.

## Supported versions

Pre-1.0. Only the tip of `main` is supported today. When a tagged release cuts (Phase F-b or later), this section will name the support window.

## Scope

In scope — these are the classes of issue I actively want reports on:
- Capability bypass (parent⊆child, revocation, handle forgery — see threat-class #1 and #2 above).
- Tool/path capability check bypass (traversal, TOCTOU, symlink).
- Agent-loop sandbox escape (`NamespacedBackend` worker breaking out of Landlock/seccomp).
- LLM-API-key leakage through logs, audit stream, memory, or ambient channels.
- Audit-trail tampering or silent event suppression.
- Prompt-injection paths that cross a capability boundary.

Out of scope — see "What we explicitly do not defend against" above. Report if you like, but triage will be lower-priority.

## Operating aaOS securely

- Keep `/etc/default/aaos` at `0600 root:root`. The `.deb` postinst enforces this on install/upgrade.
- Do not grant capability globs that include `/etc/default/aaos` or `/proc/*/environ`.
- On `NamespacedBackend` hosts, ensure unprivileged user namespaces are enabled and Landlock is compiled in (kernel 5.13+).
- Rotate your LLM API key periodically; `systemctl restart agentd` after rotating. The daemon scrubs `/proc/<pid>/environ` at startup so the old key stops appearing there; the rotation itself still depends on the operator writing the new value to `/etc/default/aaos`.
- If you're writing tools outside the `aaos-tools` crate, use `ctx.capability_registry.permits(...)` for checks — never try to construct a `CapabilityHandle` from a raw integer (the field is private and there is no public constructor; attempts won't compile). Never inspect handles for internal structure — they're opaque by contract.
