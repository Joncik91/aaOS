#!/usr/bin/env python3
"""aaOS detail log viewer.

Reads from stdin. The daemon emits a mix of:
  - JSON audit events (one per line, starts with '{')
  - Tracing output (human-readable, with ANSI color codes, including
    AGENT THINKS / TOOL CALL / TOOL RESULT / TOOL ERROR blocks)

This consumer renders a unified stream:
  - audit events go through event_model → render_detail (one line each)
  - tracing blocks are framed with the agent name (resolved from audit
    state), truncated if very long, and the verb word is always present
    so output remains scannable.
"""

from __future__ import annotations

import json
import os
import re
import sys
from typing import Optional

# Allow running as a script (python tools/observability/detail_log.py)
if __package__ is None or __package__ == "":
    import os
    sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
    from observability.event_model import EventModel
    from observability.render_detail import render
    from observability.sanitize import sanitize, truncate
else:
    from .event_model import EventModel
    from .render_detail import render
    from .sanitize import sanitize, truncate


_AGENT_ID_RE = re.compile(r"agent_id=([a-f0-9-]+)")
_TRACE_TS_RE = re.compile(r"(\d{4}-\d{2}-\d{2}T)(\d{2}:\d{2}:\d{2})")
_ITER_RE = re.compile(r"iter=(\d+)")
_TOOL_RE = re.compile(r"tool=(\S+)")
_BLOCK_HEAD_THINK = "--- AGENT THINKS ---"
_BLOCK_HEAD_CALL = "--- TOOL CALL:"
_BLOCK_HEAD_RESULT = "--- TOOL RESULT:"
_BLOCK_HEAD_ERROR = "--- TOOL ERROR:"
_BLOCK_END = "--- END ---"


def main() -> int:
    model = EventModel()
    in_block = False
    block_kind = ""
    block_agent = "?"
    block_iter: Optional[str] = None
    block_tool: Optional[str] = None
    block_ts: Optional[str] = None
    last_trace_ts: Optional[str] = None
    block_lines = []
    block_lines_max = 20  # lines of a tool result/think shown inline; more truncated
    verbose = bool(int(os.environ.get("AAOS_OBS_VERBOSE", "0") or "0"))
    # Noisy kinds hidden by default; set AAOS_OBS_VERBOSE=1 to see them.
    # tool_invoke / tool_result audit events are suppressed because the tracing
    # blocks already frame them with argument/result content inline.
    quiet_kinds = set() if verbose else {
        "cap_granted", "cap_revoked", "message", "usage",
        "tool_invoke", "tool_result",
    }

    for line in sys.stdin:
        raw_line = line.rstrip("\n")
        clean = sanitize(raw_line, keep_newlines=False)

        # Note the latest trace timestamp; the block headers come on the next line.
        ts_m = _TRACE_TS_RE.search(clean)
        if ts_m:
            last_trace_ts = ts_m.group(2)

        # ---- JSON audit event (must have top-level "event" field) ----
        if clean.startswith("{") and '"event":' in clean and not in_block:
            try:
                raw = json.loads(clean)
            except Exception:
                # Not a real audit event; treat as an in-block payload if possible
                continue
            if not isinstance(raw.get("event"), dict):
                continue
            evt = model.ingest(raw)
            if evt.kind not in quiet_kinds:
                print(render(evt))
            continue

        # ---- tracing block boundaries ---------------------------------
        # Headers: "--- TOOL CALL: foo ---" or "--- AGENT THINKS ---"
        # No agent_id on the header line; agent_id + iter arrive on the END line.
        if _BLOCK_HEAD_THINK in clean:
            _flush_block_if_open(in_block, block_kind, block_agent, block_iter, block_tool, block_ts, block_lines)
            in_block, block_kind = True, "think"
            block_agent, block_iter, block_tool = "?", None, None
            block_ts = last_trace_ts
            block_lines = []
            continue
        if _BLOCK_HEAD_CALL in clean:
            _flush_block_if_open(in_block, block_kind, block_agent, block_iter, block_tool, block_ts, block_lines)
            in_block, block_kind = True, "call"
            block_agent, block_iter = "?", None
            block_tool = _extract_header_tool(clean, _BLOCK_HEAD_CALL)
            block_ts = last_trace_ts
            block_lines = []
            continue
        if _BLOCK_HEAD_RESULT in clean:
            _flush_block_if_open(in_block, block_kind, block_agent, block_iter, block_tool, block_ts, block_lines)
            in_block, block_kind = True, "result"
            block_agent, block_iter = "?", None
            block_tool = _extract_header_tool(clean, _BLOCK_HEAD_RESULT)
            block_ts = last_trace_ts
            block_lines = []
            continue
        if _BLOCK_HEAD_ERROR in clean:
            _flush_block_if_open(in_block, block_kind, block_agent, block_iter, block_tool, block_ts, block_lines)
            in_block, block_kind = True, "error"
            block_agent, block_iter = "?", None
            block_tool = _extract_header_tool(clean, _BLOCK_HEAD_ERROR)
            block_ts = last_trace_ts
            block_lines = []
            continue
        if _BLOCK_END in clean:
            if in_block:
                agent, it, tool = _extract_agent_iter_tool(clean, model)
                if agent != "?":
                    block_agent = agent
                if it is not None:
                    block_iter = it
                if tool is not None and block_tool is None:
                    block_tool = tool
                _flush_block(block_kind, block_agent, block_iter, block_tool, block_ts, block_lines)
            in_block, block_kind, block_lines = False, "", []
            continue

        if in_block:
            block_lines.append(clean)
            continue

        # Non-block tracing line: drop silently (INFO spam, module paths, etc.)

    # EOF: flush any remaining open block
    if in_block:
        _flush_block(block_kind, block_agent, block_iter, block_tool, block_ts, block_lines)

    return 0


# ---- helpers -------------------------------------------------------------

def _extract_agent_iter_tool(line: str, model: EventModel):
    m = _AGENT_ID_RE.search(line)
    agent = model.resolve_name(m.group(1)) if m else "?"
    mi = _ITER_RE.search(line)
    mi_val = mi.group(1) if mi else None
    mt = _TOOL_RE.search(line)
    mt_val = mt.group(1) if mt else None
    return agent, mi_val, mt_val


_HEADER_TOOL_RE = re.compile(r"TOOL (?:CALL|RESULT|ERROR):\s*(\w+)")


def _extract_header_tool(header_line: str, marker: str):
    m = _HEADER_TOOL_RE.search(header_line)
    return m.group(1) if m else None


def _flush_block_if_open(in_block, block_kind, agent, it, tool, ts, lines):
    if in_block:
        _flush_block(block_kind, agent, it, tool, ts, lines)


_DIM = "\x1b[2m"
_BOLD = "\x1b[1m"
_RESET = "\x1b[0m"
_CYAN = "\x1b[36m"
_YELLOW = "\x1b[33m"
_GREEN = "\x1b[32m"
_RED = "\x1b[31m"


def _flush_block(kind: str, agent: str, it: Optional[str], tool: Optional[str], ts: Optional[str], lines):
    header_color = {
        "think": _CYAN,
        "call": _YELLOW,
        "result": _GREEN,
        "error": _RED,
    }.get(kind, _DIM)
    verb = {
        "think": "THINK ",
        "call": "CALL  ",
        "result": "RESULT",
        "error": "ERROR ",
    }.get(kind, "OTHER ")
    tag_parts = []
    if tool:
        tag_parts.append(f"tool={tool}")
    if it:
        tag_parts.append(f"iter={it}")
    tag = "  " + "  ".join(tag_parts) if tag_parts else ""
    ts_text = ts or "--:--:--"

    print(f"{_DIM}{ts_text}{_RESET}  {_BOLD}{agent:<16}{_RESET}  {header_color}{verb}{_RESET}{_DIM}{tag}{_RESET}")
    # Collapse consecutive blank lines and cap total
    shown = 0
    for ln in lines:
        stripped = ln.strip()
        if not stripped:
            continue
        # Indent & truncate long inline content so skill_read / file_read bodies
        # don't flood the terminal
        body = truncate(stripped, 240)
        print(f"  {_DIM}│{_RESET} {body}")
        shown += 1
        if shown >= 20:
            total = sum(1 for x in lines if x.strip())
            remaining = total - shown
            if remaining > 0:
                print(f"  {_DIM}│ … ({remaining} more lines, see docker logs for full output){_RESET}")
            break
    print()  # blank separator after block


if __name__ == "__main__":
    sys.exit(main())
