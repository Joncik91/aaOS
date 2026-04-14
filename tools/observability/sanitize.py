"""Strip terminal control characters from strings before display.

The daemon's tracing output contains ANSI CSI/OSC sequences for color, and
agents' think text and tool I/O can contain arbitrary bytes. Sanitize before
rendering so output can't take over the terminal (BEL, cursor moves, clears).
"""

import re

# CSI: ESC [ <params> <final>  —  common ANSI color/cursor codes
_CSI_RE = re.compile(r"\x1b\[[0-?]*[ -/]*[@-~]")
# OSC: ESC ] <params> (BEL | ST)  —  window titles, hyperlinks
_OSC_RE = re.compile(r"\x1b\].*?(?:\x07|\x1b\\)", re.DOTALL)
# Other ESC sequences (single-char)
_ESC_RE = re.compile(r"\x1b[@-Z\\-_]")
# C0 controls we keep unconditionally (tabs always; LF only when requested)
_C0_KEEP_ALWAYS = {0x09}
_C1_RE = re.compile(r"[\x80-\x9f]")


def sanitize(text: str, keep_newlines: bool = False) -> str:
    """Return a terminal-safe version of text.

    - Strips ANSI CSI/OSC and other ESC sequences.
    - Replaces C0 control chars (< 0x20) with a space, except \\t and (optionally) \\n.
    - Strips C1 control chars (0x80-0x9f).
    - Collapses CR to nothing to avoid cursor resets.
    """
    if not text:
        return ""
    t = _OSC_RE.sub("", text)
    t = _CSI_RE.sub("", t)
    t = _ESC_RE.sub("", t)
    t = _C1_RE.sub("", t)
    # C0: rebuild character by character
    keep = set(_C0_KEEP_ALWAYS)
    if keep_newlines:
        keep.add(0x0A)
    out = []
    for ch in t:
        code = ord(ch)
        if code < 0x20:
            if code in keep:
                out.append(ch)
            # else drop (CR, BS, DEL, BEL, etc.)
        elif code == 0x7f:
            continue  # DEL
        else:
            out.append(ch)
    return "".join(out)


def truncate(text: str, limit: int, suffix: str = "…") -> str:
    """Truncate text to limit characters, appending suffix if it was cut."""
    if len(text) <= limit:
        return text
    cut = max(0, limit - len(suffix))
    return text[:cut] + suffix
