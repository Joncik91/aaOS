"""Render the dashboard from a RunSnapshot.

Vertical compact list: one row per agent + a band of recent significant
events. No cost estimate in the header — token counts only.
"""

from __future__ import annotations

import time
from typing import List

from .event_model import NormalizedEvent, RunSnapshot
from .render_detail import _VERB
from .sanitize import truncate


_RESET = "\x1b[0m"
_DIM = "\x1b[2m"
_BOLD = "\x1b[1m"
_GREEN = "\x1b[32m"
_RED = "\x1b[31m"
_YELLOW = "\x1b[33m"
_CYAN = "\x1b[36m"


def clear_screen() -> str:
    return "\x1b[2J\x1b[H"


def render(snapshot: RunSnapshot, started_epoch: float, color: bool = True, name_width: int = 20) -> str:
    lines = []
    # Header
    elapsed = _format_elapsed(time.time() - started_epoch)
    agents_line = _agents_summary(snapshot)
    tok_in = f"{snapshot.stats.tokens_in:,}"
    tok_out = f"{snapshot.stats.tokens_out:,}"
    title = (
        f"{_BOLD}aaOS Run{_RESET}{_DIM}  —  {elapsed} elapsed  |  "
        f"{agents_line}  |  {snapshot.stats.tool_calls} tools  |  "
        f"{tok_in}↓ / {tok_out}↑ tokens{_RESET}"
    )
    lines.append(title)
    lines.append(_DIM + ("─" * 78) + _RESET)
    # Agent rows
    if not snapshot.agents:
        lines.append(_DIM + "  (no agents yet)" + _RESET)
    else:
        for rec in snapshot.agents.values():
            glyph, glyph_color = _status_glyph(rec.status)
            name = truncate(rec.name, name_width).ljust(name_width)
            activity = truncate(rec.last_activity or "—", 55)
            status_text = rec.status
            lines.append(
                f"  {glyph_color}{glyph}{_RESET} "
                f"{_BOLD}{name}{_RESET} "
                f"{_DIM}{status_text:<8}{_RESET} "
                f"{activity}"
            )
    # Significant-events band
    lines.append("")
    lines.append(_DIM + "─── recent significant events " + ("─" * 47) + _RESET)
    if not snapshot.significant_events:
        lines.append(_DIM + "  (none yet)" + _RESET)
    else:
        for evt in snapshot.significant_events[-8:]:
            lines.append("  " + _format_significant(evt))
    # Warnings
    if snapshot.stats.malformed_lines:
        lines.append(_DIM + f"  [warn] {snapshot.stats.malformed_lines} malformed input lines ignored" + _RESET)
    if snapshot.stats.unknown_kinds:
        kinds = ", ".join(f"{k}:{v}" for k, v in snapshot.stats.unknown_kinds.items())
        lines.append(_DIM + f"  [warn] unknown event kinds: {kinds}" + _RESET)
    return "\n".join(lines) + "\n"


def _agents_summary(snapshot: RunSnapshot) -> str:
    total = len(snapshot.agents)
    running = sum(1 for r in snapshot.agents.values() if r.status == "running")
    stopped = sum(1 for r in snapshot.agents.values() if r.status in ("stopped", "done"))
    return f"{total} agents ({running} active, {stopped} done)"


def _status_glyph(status: str):
    if status == "running":
        return "●", _GREEN
    if status == "done":
        return "○", _DIM
    if status == "stopped":
        return "○", _DIM
    if status == "failed":
        return "✗", _RED
    return "·", _DIM


def _format_significant(evt: NormalizedEvent) -> str:
    ts = evt.timestamp.strftime("%H:%M:%S") if evt.timestamp else "--:--:--"
    verb, verb_color = _VERB.get(evt.kind, _VERB["unknown"])
    name = truncate(evt.agent_name, 18)
    if evt.kind == "spawn":
        tail = f"manifest={evt.manifest_name or '?'}"
    elif evt.kind == "stop":
        tail = f"reason={evt.stop_reason or '?'}"
    elif evt.kind == "denied":
        tail = f"{evt.denied_capability or '?'} — {truncate(evt.denied_reason or '', 60)}"
    elif evt.kind == "memory_stored":
        tail = f"category={evt.memory_category or '?'}"
    elif evt.kind == "exec_completed":
        tail = f"{evt.iterations or '?'} iter, stop={evt.stop_reason or '?'}"
    elif evt.kind == "summarization_failed":
        tail = f"err: {truncate(evt.denied_reason or '', 60)}"
    else:
        tail = ""
    return f"{_DIM}{ts}{_RESET}  {verb_color}{verb}{_RESET}  {_BOLD}{name:<18}{_RESET} {tail}"


def _format_elapsed(seconds: float) -> str:
    if seconds < 60:
        return f"{int(seconds)}s"
    if seconds < 3600:
        return f"{int(seconds // 60)}m{int(seconds % 60):02d}s"
    return f"{int(seconds // 3600)}h{int((seconds % 3600) // 60):02d}m"
