"""Render NormalizedEvent as one detail-log line.

One line per event. Consistent columns. No ambiguity: timestamp, agent,
6-char verb, payload. Color is a visual aid but the verb word is always
present so screen readers / grep still work.
"""

from __future__ import annotations

from typing import Optional

from .event_model import NormalizedEvent
from .sanitize import sanitize, truncate


# ANSI codes (no dependency on a color library)
_DIM = "\x1b[2m"
_BOLD = "\x1b[1m"
_RESET = "\x1b[0m"
_RED = "\x1b[31m"
_GREEN = "\x1b[32m"
_YELLOW = "\x1b[33m"
_BLUE = "\x1b[34m"
_MAGENTA = "\x1b[35m"
_CYAN = "\x1b[36m"


_VERB = {
    "spawn":            ("SPAWN ", _GREEN + _BOLD),
    "stop":             ("STOP  ", _YELLOW),
    "tool_invoke":      ("CALL  ", _CYAN),
    "tool_result":      ("RESULT", _DIM),
    "denied":           ("DENIED", _RED + _BOLD),
    "memory_stored":    ("MEMORY", _MAGENTA),
    "memory_queried":   ("QUERY ", _MAGENTA + _DIM),
    "usage":            ("TOKENS", _DIM),
    "exec_started":     ("START ", _DIM),
    "exec_completed":   ("DONE  ", _GREEN),
    "loop_started":     ("LOOP+ ", _DIM),
    "loop_stopped":     ("LOOP- ", _DIM),
    "message":          ("MSG   ", _BLUE + _DIM),
    "cap_granted":      ("GRANT ", _DIM),
    "cap_revoked":      ("REVOKE", _YELLOW),
    "context_summarized":   ("SUMM  ", _DIM),
    "summarization_failed": ("SUMM! ", _RED),
    "unknown":          ("OTHER ", _DIM),
}


def render(evt: NormalizedEvent, color: bool = True, agent_col_width: int = 16) -> str:
    """Return a single-line string (no trailing newline) representing evt."""
    verb, verb_color = _VERB.get(evt.kind, _VERB["unknown"])
    ts = _format_ts(evt.timestamp)
    name = truncate(evt.agent_name or "?", agent_col_width)
    name_padded = name.ljust(agent_col_width)
    body = _body(evt)

    if color:
        return (
            f"{_DIM}{ts}{_RESET}  "
            f"{_BOLD}{name_padded}{_RESET}  "
            f"{verb_color}{verb}{_RESET}  "
            f"{body}"
        )
    return f"{ts}  {name_padded}  {verb}  {body}"


def _format_ts(ts) -> str:
    if ts is None:
        return "--:--:--"
    return ts.strftime("%H:%M:%S")


def _body(evt: NormalizedEvent) -> str:
    k = evt.kind
    if k == "spawn":
        return f"manifest={evt.manifest_name or '?'}"
    if k == "stop":
        return f"reason={evt.stop_reason or '?'}"
    if k == "tool_invoke":
        return evt.tool_name or "?"
    if k == "tool_result":
        outcome = "ok" if evt.tool_succeeded else "failed"
        return f"{evt.tool_name or '?'} {outcome}"
    if k == "denied":
        cap = evt.denied_capability or "?"
        reason = evt.denied_reason or ""
        if reason:
            return f"{cap} — {truncate(sanitize(reason), 120)}"
        return cap
    if k == "memory_stored":
        return f"category={evt.memory_category or '?'}"
    if k == "memory_queried":
        return f"results={evt.memory_results if evt.memory_results is not None else '?'}"
    if k == "usage":
        return f"in={evt.usage_in or 0}  out={evt.usage_out or 0}"
    if k == "exec_completed":
        return f"{evt.iterations or '?'} iterations, stop={evt.stop_reason or '?'}"
    if k == "message":
        return f"{evt.parent_from or '?'} → {evt.parent_to or '?'}"
    if k == "cap_granted" or k == "cap_revoked":
        return evt.denied_capability or "?"
    if k == "summarization_failed":
        return f"error: {evt.denied_reason or '?'}"
    if k == "unknown":
        return f"(raw_kind={evt.raw_kind or '?'})"
    # exec_started, loop_started, loop_stopped, context_summarized — no body
    return ""
