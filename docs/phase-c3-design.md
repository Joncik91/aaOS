# Phase C3: Shared Knowledge Graph

> **Sub-project of Phase C: Agent Memory System**
> Builds on Phase C2 (episodic store) and Phase C1 (managed context windows).
> **Status: Deferred** — design documented for future alignment, not for immediate implementation.

## Goal

A shared knowledge layer where agents can contribute to and query from a collective understanding. Content is indexed by meaning, not location. An agent asks "what do we know about the authentication module?" and gets relevant facts from any agent that has studied it — governed by capability tokens.

## Peer Review Notes (Qwen + Copilot)

Reviewed by Qwen CLI and GitHub Copilot CLI. Key feedback incorporated:
- Deferral confirmed as sound by both reviewers
- Added trust/provenance model and topic taxonomy governance to open questions (Qwen)
- Added agent lifecycle (decommissioning) concern (Qwen)
- Noted roadmap/spec divergence: roadmap describes semantic filesystem, spec describes inter-agent fact store — needs reconciliation before C3 activates (Copilot)
- C2 `MemoryScope` field pulled forward for forward-compatibility (both)
- `supersedes` flow needs explicit tool interface design (Copilot)
- C2's single-table topology is now compatible with C3 shared queries (Copilot)
- Rate limiting/quotas on contributions added to open questions (Qwen)

## Why Deferred

Per peer review (Qwen + Copilot), C3 should not be built until:

1. **C1 and C2 are battle-tested** — the per-agent memory patterns need to prove themselves under real workloads before cross-agent sharing adds complexity.
2. **Cross-agent access control is designed** — the current capability system is per-agent. Sharing memories across agents requires a new access model (who can see what, under what conditions).
3. **Real multi-agent usage validates the need** — we need at least 2 agents that demonstrably need to share knowledge, with a concrete articulation of what they'd share and why.
4. **Embedding pipeline is stable** — C2 will validate the embedding model, LanceDB integration, and query patterns. C3 inherits all of that.
5. **Agent lifecycle management is solved** — when an agent is retired, what happens to its contributions? This affects the data model fundamentally.

## Design Direction (Not a Spec)

The following captures the intended direction so future work stays aligned. This is NOT a buildable spec — details will be refined when C3 is activated.

### Roadmap Reconciliation Required

The roadmap describes C3 as a **semantic filesystem abstraction** — "Find all files related to capability enforcement" returning code, docs, and prior analysis without knowing paths. This spec describes an **inter-agent fact store**. These are materially different things. Before C3 is activated, this divergence must be explicitly reconciled: are we building a semantic filesystem, a fact store, or both?

### Shared vs. Private Memory

Two scopes (C2 already includes the `MemoryScope` enum with `Private` as default):
- **Private** (C2): Per-agent episodic store. Only the owning agent can read/write.
- **Shared** (C3): A common knowledge store that multiple agents can contribute to and query from. Uses `MemoryScope::Shared { topics: Vec<String> }`.

C2's single-table LanceDB topology (all agents in one table with `agent_id` column) is already compatible with C3's cross-agent queries — just widen the WHERE clause.

### Access Model (Sketch)

**Decision needed before C3:** Should shared knowledge use dedicated `Capability` variants (`KnowledgeContribute`, `KnowledgeQuery`) or go through the existing `ToolInvoke` path (like C2's memory tools)? C2 chose `ToolInvoke` for consistency. C3 should follow the same pattern unless there's a compelling reason for dedicated variants. This decision should be documented in C2's out-of-scope section when made.

```rust
// If dedicated variants (TBD):
pub enum Capability {
    // existing...
    KnowledgeContribute { topics: Vec<String> },
    KnowledgeQuery { topics: Vec<String> },
}
```

Topics act as namespaces. An agent with `KnowledgeContribute { topics: ["auth", "security"] }` can share facts about auth and security. Another agent with `KnowledgeQuery { topics: ["auth"] }` can read those facts. Topics are declared in manifests and enforced by the kernel.

**Topic taxonomy governance:** Topics are flat strings today. Without normalization or a controlled vocabulary, `["auth"]` and `["authentication"]` silently fragment the shared graph. Options: require a topic registry, enforce lowercase/kebab-case normalization, or accept fragmentation as a v1 trade-off.

### Data Model (Sketch)

Shared knowledge records extend `MemoryRecord` (from C2) with:
- `contributed_by: AgentId` — provenance tracking
- `topics: Vec<String>` — for access control filtering and topic-scoped queries
- `confidence: f32` — how sure the contributing agent is (self-assessed — see trust concern below)

### Query Interface (Sketch)

A `knowledge_query` tool (distinct from `memory_query`) that searches the shared graph:

```json
{
  "name": "knowledge_query",
  "input_schema": {
    "properties": {
      "query": { "type": "string" },
      "topics": { "type": "array", "items": { "type": "string" } },
      "limit": { "type": "integer", "default": 10 }
    }
  }
}
```

Results include provenance (which agent contributed the fact) and confidence scores.

**`supersedes` flow:** To supersede a fact, an agent must first `knowledge_query` to find the record, extract its UUID, then store with `supersedes: Some(id)`. This query-then-update flow must be explicit in the tool interface design.

### Open Questions

1. **Conflict resolution:** When two agents store contradictory facts about the same topic, which wins? Options: newest, highest confidence, human arbiter, keep both with provenance.
2. **Garbage collection:** Shared knowledge can grow without bound. What's the pruning policy? Unused facts? Low confidence? Age-based decay?
3. **Consistency model:** Is the shared graph eventually consistent (agents may see stale data) or strongly consistent (all agents see the same state)?
4. **Graph structure:** Is this a flat vector store with topic filtering (simple, like C2 with broader scope), or a real property graph with edges between facts (powerful, much more complex)?
5. **Topic taxonomy governance:** Controlled vocabulary, normalization rules, or accept fragmentation?
6. **Trust/provenance model:** Self-reported `confidence` is unreliable — a low-quality agent can confidently contribute garbage. Is there consumer-side trust weighting? Agent reputation? Human curation?
7. **Rate limiting:** Should there be quotas on contributions per agent to prevent one chatty agent from flooding the shared graph?
8. **Agent decommissioning:** When an agent is retired, what happens to its contributions? Remove? Mark as orphaned? Transfer ownership?
9. **Schema evolution:** How does the shared knowledge format change over time? Versioned records? Migration strategy?

### Prerequisites Before Building

- [ ] C1 in production use for 2+ months
- [ ] C2 in production use for 1+ month
- [ ] At least 2 agents demonstrably needing shared knowledge
- [ ] Cross-agent capability model designed and reviewed
- [ ] Embedding pipeline stable (model choice, dimensionality, etc.)
- [ ] Agent lifecycle management designed (decommissioning story)
- [ ] Roadmap/spec divergence reconciled (semantic filesystem vs. fact store)

## What You'll See (Future)

1. Agent A (code reviewer) stores: "The auth module uses bcrypt for password hashing, but the salt rounds are set to 4 which is too low."
2. Agent B (security auditor) queries: "What do we know about password hashing?"
3. Agent B gets Agent A's finding, with provenance, and can act on it.
4. Both agents' knowledge compounds — the system knows more than any individual agent.
