# Security

aaOS is a capability-enforced agent runtime. Capability bypasses, sandbox escapes, secret-handling regressions, and audit-trail tampering are the classes of issue I most want to hear about.

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

In scope:
- Capability bypass (parent⊆child, revocation, handle forgery).
- Tool/path capability check bypass (traversal, TOCTOU, symlink).
- Agent-loop sandbox escape (NamespacedBackend worker breaking out of Landlock/seccomp).
- LLM-API-key leakage through logs, audit stream, memory, or ambient channels.
- Audit-trail tampering or silent event suppression.
- Prompt-injection paths that cross a capability boundary.

Out of scope (report if you like, but lower priority):
- Denial of service (single-operator tool; DoS is easy and not the threat model).
- Issues that require `CAP_SYS_PTRACE` or kernel-level compromise — those already win.
- LLM hallucination / fabrication — mitigated at the prompt level, not a security boundary.

## Operating aaOS securely

- Keep `/etc/default/aaos` at `0600 root:root`. The `.deb` postinst enforces this on install/upgrade.
- Do not grant capability globs that include `/etc/default/aaos` or `/proc/*/environ`.
- On `NamespacedBackend` hosts, ensure unprivileged user namespaces are enabled and Landlock is compiled in (kernel 5.13+).
- Rotate your LLM API key periodically; `systemctl restart agentd` after rotating.
