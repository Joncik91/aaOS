# Run 5 — First Persistent-Memory Run *(2026-04-14)*

**Integration commit:** `548188b` "run 5: 12 artifacts + manifest tuning from observed failures" (16:51).

First run with `AAOS_PERSISTENT_MEMORY=1`. Stable Bootstrap ID was `f3042f07-751a-4141-a73c-36e1687aff46`, persisted to `/var/lib/aaos/bootstrap_id`. Host's `./memory/` bind-mounted into the container's `/var/lib/aaos/memory/` so SQLite state survives restarts.

## What Worked

- **Protocol fully exercised end-to-end.** Bootstrap called `memory_query` *before* decomposing (4 queries, empty results as expected on first run), then `memory_store` at completion with a goal-level summary under its stable ID. That summary is now retrievable by future Bootstrap runs.
- **`file_list` eliminated the path-guessing problem.** Zero failures of the "file_read on a directory" class that dominated Run 4. Children listed directories before reading.
- **Capability system caught a real mistake in real time.** Bootstrap drafted a `pattern-implementer` child with `file_write: /src/*`. `spawn_agent` refused: "agent f3042f07 lacks FileWrite { path_glob: /src/\* }; cannot delegate to child." Bootstrap recovered by spawning with `/data/workspace/…/*` instead.
- **Behavioral-adaptation-layer pivot.** After the `/src/*` denial, a later child reasoned explicitly: *"Since we cannot modify the Rust codebase directly (read-only /src/), we implement the evolution as a behavioral adaptation layer using existing capabilities."* That's the "prompts first, code second" path the reviews had pushed for, arrived at by the system itself after hitting the constraint.
- **Independent convergence on the same direction.** Run 4 and Run 5 were given the same prompt. Mock embeddings meant Run 5 couldn't retrieve Run 4's outputs effectively. Both independently converged on "Meta-Cognitive Coordinator for Bootstrap cross-run learning" — two fresh runs landing on the same feature is a real signal.

## What the Run Exposed

Three issues, all fixed as manifest-only changes (no runtime code):

1. **Skill over-adherence.** Bootstrap loaded `planning-and-task-breakdown` and followed every step mechanically, ignoring the skill's own explicit "When NOT to use: single-file changes with obvious scope." Runtime roughly doubled compared to Run 4 (~30 minutes vs ~12) without a proportional quality gain. Fix: manifest now instructs Bootstrap to honor each skill's "When to use / When NOT to use" sections — "a skill loaded and correctly skipped is better than a skill applied to the wrong task."

2. **Child memory writes are orphaned.** Of 14 records in the SQLite store at run end, only 1 was tagged with Bootstrap's stable ID. The other 13 were under ephemeral child `agent_id`s that no future Bootstrap can retrieve (memory queries are filtered per-agent by design). Classic asymmetry: only the persistent agent benefits from persistent memory. Fix: removed `tool: memory_store` from all child manifest examples; children now return findings in their reply, Bootstrap persists only what's worth keeping.

3. **Workspace `file_list` denied for children.** Children were granted `file_write: /data/workspace/X/*` but not the matching `file_read: /data/workspace/X/*`. `file_list` is gated on `FileRead` capability and correctly refused. The capability model being strict is the whole point. Fix: manifest examples now grant both `file_read` and `file_write` for workspace dirs.

## What the Run Over-Built

The pattern-builder child produced the same pattern-storage logic in **JavaScript** (`pattern-storage.js`, 22 KB) and then again in **Python** (`pattern-storage.py`, 24 KB). Neither language has a path into the aaOS runtime. The correct target would have been an updated `manifests/bootstrap.yaml` plus a short markdown spec. The builder noticed it couldn't write to `/src/` (correct) and pivoted to "behavioral layer" (correct), then chose languages that still can't execute anywhere (incorrect). New heuristic added to the manifest: "Don't spawn children to produce the same artifact in different languages — pick one representation and move on."

## Artifacts

12 workspace files were produced. They are not committed (agent output is gitignored under `/output/`); the evidence of what happened lives in this log. The shape of the output was: one up-front decomposition README, eight design artifacts covering current state / evolution plan / pattern-storage design / implementation / adaptation algorithm / bootstrap-upgrade guide / schemas, two over-built parallel implementations (`pattern-storage.js` and `pattern-storage.py` — the symptom described above), and a `memory-dump.json` export of the 14 stored memories as a human-readable paper trail.

## Cost

Estimated in earlier notes as "~$0.55 for run 5 alone." That estimate was wrong — it was computed from token counts × flat DeepSeek rate, ignoring context caching. The authoritative cumulative figure from the DeepSeek dashboard — covering runs 1, 2, 3, 4, **and** 5 in aggregate — is **~$0.54**, not per-run.
