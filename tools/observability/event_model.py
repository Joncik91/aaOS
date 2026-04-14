"""Normalize aaOS audit JSON events into a stable shape + maintain run state.

The daemon emits one JSON line per audit event via StdoutAuditLog. This module
consumes those raw events and produces NormalizedEvents with agent names
resolved, enums mapped to short verbs, and a running snapshot of per-agent
state for the dashboard.

What this module does NOT know: tool arguments, tool result payloads, or agent
think text. Those are on the tracing stream, not the audit stream. The detail-
log consumer parses tracing separately.
"""

from __future__ import annotations

import copy
from dataclasses import dataclass, field
from datetime import datetime
from typing import Dict, List, Optional

from .sanitize import sanitize, truncate


# ---- public shapes -------------------------------------------------------

@dataclass
class NormalizedEvent:
    timestamp: Optional[datetime]
    kind: str  # "spawn" | "stop" | "tool_invoke" | "tool_result" | "denied" | "memory_stored" | "memory_queried" | "usage" | "exec_started" | "exec_completed" | "loop_started" | "loop_stopped" | "message" | "context_summarized" | "summarization_failed" | "unknown"
    agent_name: str
    agent_id: str
    # kind-specific, all optional
    tool_name: Optional[str] = None
    tool_succeeded: Optional[bool] = None
    manifest_name: Optional[str] = None
    denied_capability: Optional[str] = None
    denied_reason: Optional[str] = None
    memory_category: Optional[str] = None
    memory_results: Optional[int] = None
    usage_in: Optional[int] = None
    usage_out: Optional[int] = None
    stop_reason: Optional[str] = None
    iterations: Optional[int] = None
    parent_from: Optional[str] = None  # short id for message sent/delivered
    parent_to: Optional[str] = None
    raw_kind: Optional[str] = None  # original audit kind string for fallbacks


@dataclass
class AgentRecord:
    agent_id: str
    name: str
    status: str = "starting"  # starting | running | stopped | done | failed
    tokens_in: int = 0
    tokens_out: int = 0
    tool_calls: int = 0
    file_reads: int = 0
    file_writes: int = 0
    last_activity: str = ""  # short human line
    iterations: Optional[int] = None
    stop_reason: Optional[str] = None


@dataclass
class RunStats:
    tokens_in: int = 0
    tokens_out: int = 0
    tool_calls: int = 0
    file_reads: int = 0
    file_writes: int = 0
    spawns: int = 0
    denials: int = 0
    memory_stores: int = 0
    malformed_lines: int = 0
    unknown_kinds: Dict[str, int] = field(default_factory=dict)


@dataclass
class RunSnapshot:
    agents: Dict[str, AgentRecord]
    stats: RunStats
    significant_events: List[NormalizedEvent]  # bounded ring


# ---- implementation ------------------------------------------------------

_SIGNIFICANT_KINDS = {"spawn", "stop", "denied", "memory_stored", "exec_completed", "summarization_failed"}


class EventModel:
    """Stateful audit-stream normalizer + run snapshot."""

    def __init__(self, significant_capacity: int = 30) -> None:
        self.agents: Dict[str, AgentRecord] = {}
        self.stats = RunStats()
        self.significant: List[NormalizedEvent] = []
        self._cap = significant_capacity

    # ---- public ----------------------------------------------------------

    def ingest(self, raw: dict) -> NormalizedEvent:
        """Convert one raw audit event dict to a NormalizedEvent and update state.

        Never raises. Malformed/unknown input returns a NormalizedEvent with
        kind='unknown'.
        """
        try:
            evt = self._normalize(raw)
        except Exception:
            self.stats.malformed_lines += 1
            return NormalizedEvent(
                timestamp=None,
                kind="unknown",
                agent_name="?",
                agent_id="?",
                raw_kind="<parse error>",
            )
        self._update_state(evt)
        return evt

    def snapshot(self) -> RunSnapshot:
        return RunSnapshot(
            agents={k: copy.copy(v) for k, v in self.agents.items()},
            stats=copy.copy(self.stats),
            significant_events=list(self.significant),
        )

    def resolve_name(self, agent_id: str) -> str:
        rec = self.agents.get(agent_id)
        if rec:
            return rec.name
        return short_id(agent_id)

    # ---- internals -------------------------------------------------------

    def _normalize(self, raw: dict) -> NormalizedEvent:
        event = raw.get("event", {}) or {}
        raw_kind = event.get("kind", "")
        agent_id = raw.get("agent_id", "") or "?"
        ts = _parse_ts(raw.get("timestamp"))
        agent_name = self.resolve_name(agent_id)

        if raw_kind == "agent_spawned":
            manifest = event.get("manifest_name", "?")
            return NormalizedEvent(
                timestamp=ts, kind="spawn",
                agent_name=manifest, agent_id=agent_id,
                manifest_name=manifest, raw_kind=raw_kind,
            )
        if raw_kind == "agent_stopped":
            reason = event.get("reason", "?")
            return NormalizedEvent(
                timestamp=ts, kind="stop",
                agent_name=agent_name, agent_id=agent_id,
                stop_reason=reason, raw_kind=raw_kind,
            )
        if raw_kind == "tool_invoked":
            tool = event.get("tool", "?")
            return NormalizedEvent(
                timestamp=ts, kind="tool_invoke",
                agent_name=agent_name, agent_id=agent_id,
                tool_name=tool, raw_kind=raw_kind,
            )
        if raw_kind == "tool_result":
            tool = event.get("tool", "?")
            ok = event.get("success", False)
            return NormalizedEvent(
                timestamp=ts, kind="tool_result",
                agent_name=agent_name, agent_id=agent_id,
                tool_name=tool, tool_succeeded=bool(ok), raw_kind=raw_kind,
            )
        if raw_kind == "capability_denied":
            cap = event.get("capability")
            reason = event.get("reason", "")
            return NormalizedEvent(
                timestamp=ts, kind="denied",
                agent_name=agent_name, agent_id=agent_id,
                denied_capability=_summarize_capability(cap),
                denied_reason=truncate(sanitize(reason), 200),
                raw_kind=raw_kind,
            )
        if raw_kind == "memory_stored":
            return NormalizedEvent(
                timestamp=ts, kind="memory_stored",
                agent_name=agent_name, agent_id=agent_id,
                memory_category=event.get("category", "?"),
                raw_kind=raw_kind,
            )
        if raw_kind == "memory_queried":
            return NormalizedEvent(
                timestamp=ts, kind="memory_queried",
                agent_name=agent_name, agent_id=agent_id,
                memory_results=event.get("results_count"),
                raw_kind=raw_kind,
            )
        if raw_kind == "usage_reported":
            return NormalizedEvent(
                timestamp=ts, kind="usage",
                agent_name=agent_name, agent_id=agent_id,
                usage_in=event.get("input_tokens", 0),
                usage_out=event.get("output_tokens", 0),
                raw_kind=raw_kind,
            )
        if raw_kind == "agent_execution_started":
            return NormalizedEvent(
                timestamp=ts, kind="exec_started",
                agent_name=agent_name, agent_id=agent_id, raw_kind=raw_kind,
            )
        if raw_kind == "agent_execution_completed":
            return NormalizedEvent(
                timestamp=ts, kind="exec_completed",
                agent_name=agent_name, agent_id=agent_id,
                iterations=event.get("total_iterations"),
                stop_reason=event.get("stop_reason"),
                raw_kind=raw_kind,
            )
        if raw_kind == "capability_granted":
            return NormalizedEvent(
                timestamp=ts, kind="cap_granted",
                agent_name=agent_name, agent_id=agent_id,
                denied_capability=_summarize_capability(event.get("capability")),
                raw_kind=raw_kind,
            )
        if raw_kind == "capability_revoked":
            return NormalizedEvent(
                timestamp=ts, kind="cap_revoked",
                agent_name=agent_name, agent_id=agent_id,
                denied_capability=_summarize_capability(event.get("capability")),
                raw_kind=raw_kind,
            )
        if raw_kind == "agent_loop_started":
            return NormalizedEvent(
                timestamp=ts, kind="loop_started",
                agent_name=agent_name, agent_id=agent_id, raw_kind=raw_kind,
            )
        if raw_kind == "agent_loop_stopped":
            return NormalizedEvent(
                timestamp=ts, kind="loop_stopped",
                agent_name=agent_name, agent_id=agent_id, raw_kind=raw_kind,
            )
        if raw_kind in ("message_sent", "message_delivered"):
            return NormalizedEvent(
                timestamp=ts, kind="message",
                agent_name=agent_name, agent_id=agent_id,
                parent_from=short_id(event.get("from", "")),
                parent_to=short_id(event.get("to", "")),
                raw_kind=raw_kind,
            )
        if raw_kind == "agent_message_received":
            return NormalizedEvent(
                timestamp=ts, kind="message",
                agent_name=agent_name, agent_id=agent_id,
                parent_from="(inbound)", parent_to=short_id(agent_id),
                raw_kind=raw_kind,
            )
        if raw_kind == "context_summarized":
            return NormalizedEvent(
                timestamp=ts, kind="context_summarized",
                agent_name=agent_name, agent_id=agent_id, raw_kind=raw_kind,
            )
        if raw_kind == "context_summarization_failed":
            reason = event.get("reason") or event.get("error") or ""
            # failure_kind is new in the runtime (added with the SummarizationFailureKind
            # enum); older event streams won't have it.
            kind_tag = event.get("failure_kind")
            label_prefix = f"[{kind_tag}] " if kind_tag else ""
            return NormalizedEvent(
                timestamp=ts, kind="summarization_failed",
                agent_name=agent_name, agent_id=agent_id,
                denied_reason=truncate(sanitize(f"{label_prefix}{reason}"), 200),
                raw_kind=raw_kind,
            )

        # Unknown kind — keep count, return as "unknown"
        self.stats.unknown_kinds[raw_kind] = self.stats.unknown_kinds.get(raw_kind, 0) + 1
        return NormalizedEvent(
            timestamp=ts, kind="unknown",
            agent_name=agent_name, agent_id=agent_id, raw_kind=raw_kind,
        )

    def _update_state(self, evt: NormalizedEvent) -> None:
        # Register agent on spawn
        if evt.kind == "spawn":
            rec = AgentRecord(agent_id=evt.agent_id, name=evt.manifest_name or evt.agent_name)
            rec.status = "running"
            rec.last_activity = "spawned"
            self.agents[evt.agent_id] = rec
            self.stats.spawns += 1
            self._push_significant(evt)
            return

        # Otherwise update in place if known; skip update if unknown agent
        rec = self.agents.get(evt.agent_id)

        if evt.kind == "stop":
            if rec:
                rec.status = "stopped"
                rec.stop_reason = evt.stop_reason
                rec.last_activity = f"stopped ({evt.stop_reason})"
            self._push_significant(evt)
            return

        if evt.kind == "tool_invoke":
            self.stats.tool_calls += 1
            if evt.tool_name == "file_read":
                self.stats.file_reads += 1
            elif evt.tool_name == "file_write":
                self.stats.file_writes += 1
            if rec:
                rec.tool_calls += 1
                if evt.tool_name == "file_read":
                    rec.file_reads += 1
                elif evt.tool_name == "file_write":
                    rec.file_writes += 1
                rec.last_activity = f"calling {evt.tool_name}"
            return

        if evt.kind == "tool_result":
            if rec:
                ok = "ok" if evt.tool_succeeded else "failed"
                rec.last_activity = f"{evt.tool_name} {ok}"
            return

        if evt.kind == "denied":
            self.stats.denials += 1
            if rec:
                rec.last_activity = f"DENIED: {evt.denied_capability or '?'}"
            self._push_significant(evt)
            return

        if evt.kind == "memory_stored":
            self.stats.memory_stores += 1
            if rec:
                rec.last_activity = f"stored memory ({evt.memory_category})"
            self._push_significant(evt)
            return

        if evt.kind == "memory_queried":
            if rec:
                results = evt.memory_results
                rec.last_activity = f"queried memory ({results} results)"
            return

        if evt.kind == "usage":
            if evt.usage_in is not None:
                self.stats.tokens_in += evt.usage_in
                if rec:
                    rec.tokens_in += evt.usage_in
            if evt.usage_out is not None:
                self.stats.tokens_out += evt.usage_out
                if rec:
                    rec.tokens_out += evt.usage_out
            return

        if evt.kind == "exec_completed":
            if rec:
                rec.iterations = evt.iterations
                # If the agent has fully exited executor, mark done only if
                # we haven't already seen a stop; otherwise keep stop state.
                if rec.status != "stopped":
                    rec.status = "done"
                rec.last_activity = f"completed ({evt.iterations} iter)"
            self._push_significant(evt)
            return

        if evt.kind == "loop_started":
            if rec:
                rec.last_activity = "loop started (persistent)"
            return

        if evt.kind == "loop_stopped":
            if rec:
                rec.last_activity = "loop stopped"
            return

        if evt.kind == "summarization_failed":
            if rec:
                rec.last_activity = "context summarization failed"
            self._push_significant(evt)
            return

        # exec_started, message, context_summarized, unknown — no state update

    def _push_significant(self, evt: NormalizedEvent) -> None:
        if evt.kind in _SIGNIFICANT_KINDS:
            self.significant.append(evt)
            if len(self.significant) > self._cap:
                self.significant.pop(0)


# ---- helpers -------------------------------------------------------------

def short_id(agent_id: str) -> str:
    if not agent_id:
        return "?"
    return agent_id[:8]


def _parse_ts(ts: Optional[str]) -> Optional[datetime]:
    if not ts:
        return None
    try:
        # Audit timestamps are RFC3339 with trailing Z. Python fromisoformat
        # only handles that on 3.11+; strip Z and parse.
        cleaned = ts.replace("Z", "+00:00")
        return datetime.fromisoformat(cleaned)
    except Exception:
        return None


def _summarize_capability(cap) -> str:
    """Condense a Capability JSON value into a short 'tool:memory_store' style string.

    Handles two serialization shapes observed in the audit stream:
      - Tagged enum: {"ToolInvoke": {"tool_name": "..."}} or {"FileRead": {"path_glob": "..."}}
      - Type-tagged: {"type": "web_search"} or {"type": "tool_invoke", "tool_name": "..."}
    """
    if isinstance(cap, str):
        return cap
    if not isinstance(cap, dict):
        return "?"

    # Type-tagged shape (audit uses this)
    if "type" in cap:
        t = cap["type"]
        if t in ("tool_invoke", "ToolInvoke"):
            return f"tool:{cap.get('tool_name', '?')}"
        if t in ("file_read", "FileRead"):
            return f"file_read:{cap.get('path_glob', '?')}"
        if t in ("file_write", "FileWrite"):
            return f"file_write:{cap.get('path_glob', '?')}"
        if t in ("spawn_child", "SpawnChild"):
            return "spawn_child"
        if t in ("web_search", "WebSearch"):
            return "web_search"
        if t in ("custom", "Custom"):
            return f"custom:{cap.get('name', '?')}"
        return str(t)

    # Tagged-enum shape (older / some paths)
    if "ToolInvoke" in cap:
        inner = cap["ToolInvoke"]
        return f"tool:{inner.get('tool_name', '?')}" if isinstance(inner, dict) else "tool:?"
    if "FileRead" in cap:
        inner = cap["FileRead"]
        return f"file_read:{inner.get('path_glob', '?')}" if isinstance(inner, dict) else "file_read:?"
    if "FileWrite" in cap:
        inner = cap["FileWrite"]
        return f"file_write:{inner.get('path_glob', '?')}" if isinstance(inner, dict) else "file_write:?"
    if "SpawnChild" in cap:
        return "spawn_child"
    if "WebSearch" in cap:
        return "web_search"
    keys = list(cap.keys())
    return keys[0] if keys else "?"
