# Cost Bookkeeping

Earlier docs contained per-run cost figures like "~$0.02", "~$0.05", "~$0.11 total across three runs", "~$0.48 for run 4". Those were all token-count × flat-rate estimates from `docker logs` output. **They are not reliable for DeepSeek runs**, because DeepSeek's context caching discounts cache-hit input tokens to roughly 10% of the normal rate. A persistent Bootstrap Agent re-sends a growing conversation on every iteration — cache hits dominate the input tokens very quickly.

**The authoritative cumulative figure is the DeepSeek dashboard:** as of 2026-04-14, ~$0.54 total since the Anthropic → DeepSeek switch. Add a small additional amount for earlier Anthropic-only runs (the Phase D fetch-HN demo and the security self-audit ran against Anthropic). Rough all-in cumulative across everything in this log: **~$0.70**.

Per-run numbers in individual entries are kept as they were recorded at the time, but annotated `[token-math estimate, not dashboard-verified]` where relevant. The "pennies per run" framing holds — the exact per-run breakdown doesn't.
