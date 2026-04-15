# Run 1 — Security Self-Audit *(2026-04-13)*

**Integration commit:** `82d19e9` "security: fix 4 vulnerabilities found by self-audit" (20:52).

This was the first time the runtime read its own source and produced actionable output. Not framed as "self-reflection" at the time; called an "audit." Later numbered Run 1 retroactively.

## Setup

Bootstrap Agent (Anthropic Haiku at this point in the day; DeepSeek Reasoner came later with `f6b62a6` at 19:40 but the audit predates it per commit ordering — the audit actually ran just after `f6b62a6` made DeepSeek available and `82d19e9` integrated the findings). Two children:

- `code-reader` — 464 K tokens of source read
- `security-auditor` — 474 K tokens of adversarial review

Total context used: 1.37 M tokens. Cost recorded at the time as **~$0.05** `[token-math estimate]`.

## What the Agents Found

13 findings across 6 components. The human reviewed each against actual code, confirmed 4, flagged 4 as overstated or wrong, deferred 5 as low-risk in Docker.

**The four confirmed and fixed:**

1. **Path traversal in `glob_matches`** (CRITICAL). `"/data/../etc/passwd".starts_with("/data/")` returns true. An agent with `file_read: /data/*` could read any file via `..` sequences. Fixed by adding lexical path normalization before matching.
2. **Unknown tools receive all capability tokens** (MEDIUM). `matches_tool_capability` returned `true` for unknown tools, leaking `FileRead`/`FileWrite`/`SpawnChild` tokens. Fixed to only pass `ToolInvoke` tokens to unknown tools.
3. **Child tokens ignore parent constraints** (HIGH). `CapabilityToken::issue()` used `Constraints::default()` for child tokens. A parent with rate limits could spawn children without those limits. Fixed so children inherit the granting parent's constraints.
4. **No path canonicalization in file tools** (CRITICAL). Same root cause as #1, at the tool level. Fixed by the same `normalize_path()` function.

## What the Agents Got Wrong

- **V6.1 "Capability checker injection."** Described the router accepting a closure as a vulnerability. The closure is constructed by the server, not by agents. Not exploitable.
- **CVSS scores inflated.** Assigned network attack vectors (AV:N) to a system running in Docker with no network listener.
- **V2.1 overstated.** The parent-subset enforcement was correct; the real risk was path traversal in the glob matcher it delegates to, which was already V1.1.

## Why This Mattered

The path traversal vulnerability had been present since Phase A and would have allowed any agent to read any file on the system by using `..` sequences. No human had caught it in four phases of development. A run of the runtime's own agents found it.
