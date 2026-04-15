# Interlude — Skill Loading Bug Found in Runs 2-3 *(2026-04-13 22:37)*

**Fix commit:** `66542bf` "fix: teach Bootstrap to use skills, not just name agents after them."

Observed during runs 2 and 3: Bootstrap saw the skill catalog in its system prompt but spawned children *named after* skills (e.g. `security-and-hardening`) without ever calling `skill_read` to load the actual instructions. It was using skill names as naming inspiration, not as executable knowledge.

Fix was manifest-only: explicit `skill_read` examples in the Bootstrap prompt, plus a rule: "Before starting work, load relevant skills with `skill_read` — follow their workflows." Shipped at 22:37 — 3 minutes after Run 3's fix.

This set up Run 4.
