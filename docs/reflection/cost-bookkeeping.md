# Cost Bookkeeping (historical — discontinued 2026-04-25)

This file is preserved for context.  Per-run cost figures stopped being tracked from round 5 onwards (v0.1.6, 2026-04-25) — recent reflection entries either omit cost or mark it `TBD`.  Reasons for stopping:

- **Token-math estimates were unreliable** for DeepSeek runs because the provider's context-caching discount drops cache-hit input tokens to roughly 10% of the normal rate.  A persistent Bootstrap Agent re-sends a growing conversation on every iteration; cache hits dominate input tokens very quickly, and naive `tokens × flat-rate` overestimates by 5–10×.
- **Dashboard figures lagged behind runs.**  The authoritative cumulative across all runs through 2026-04-14 was ~$0.70 per the DeepSeek + Anthropic dashboards.  Reading the dashboard required logging into the provider, copying numbers, and updating prose — a friction the project couldn't afford to maintain at one release per day.
- **Per-run cost stopped being load-bearing.**  Once the cumulative was confirmed at well under $1 across the entire build history, granular per-run accounting added no decision signal — the "pennies per run" framing was already enough to know the loop was economically viable.

## Earlier per-run figures

Older reflection entries (rounds 1–4, the Phase A–E retrospective work) include figures like "~$0.02", "~$0.05", "~$0.11 total across three runs", "~$0.48 for run 4".  Those were all token-count × flat-rate estimates from `docker logs` token output.  **They are not reliable for DeepSeek runs** for the reason above.  They are kept as recorded — corrections live next to the original text per the docs-style rule "transparency beats tidiness."

## What the running cumulative was, last we checked

As of 2026-04-14: ~$0.54 total since the Anthropic → DeepSeek switch (Phase E1).  Add a small additional amount for earlier Anthropic-only runs (the Phase D fetch-HN demo + the security self-audit).  Rough all-in cumulative across everything in this log: **~$0.70**.

That number has not been updated since.  Each subsequent round (5–9, the fuzz pass, the stress probe) likely added a few cents to a few tens of cents at most.  If a future run needs an actual cost figure, query the provider dashboard directly — don't extrapolate token counts.

## What replaced cost tracking

Each release ships with a wall-clock and a "what changed" entry in the CHANGELOG.  That's the operational signal — minutes, not dollars.  Time is the bottleneck for solo-maintainer iteration, not money.
