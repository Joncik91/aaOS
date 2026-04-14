#!/usr/bin/env python3
"""aaOS live dashboard.

Reads audit JSON events from stdin (ignores non-JSON tracing lines) and
renders a vertical-list dashboard that refreshes on every audit event.
Throttled to ≤10 Hz to avoid flicker on bursty runs.
"""

from __future__ import annotations

import json
import sys
import time

if __package__ is None or __package__ == "":
    import os
    sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
    from observability.event_model import EventModel
    from observability.render_dashboard import render, clear_screen
else:
    from .event_model import EventModel
    from .render_dashboard import render, clear_screen


_MIN_REFRESH_INTERVAL = 0.1  # seconds — 10 Hz cap


def main() -> int:
    model = EventModel()
    started = time.time()
    last_draw = 0.0

    # Initial empty screen so the user sees something immediately
    sys.stdout.write(clear_screen())
    sys.stdout.write(render(model.snapshot(), started))
    sys.stdout.flush()

    for line in sys.stdin:
        line = line.strip()
        if not line or not line.startswith("{") or '"event":' not in line:
            continue
        try:
            raw = json.loads(line)
        except Exception:
            continue
        if not isinstance(raw.get("event"), dict):
            continue
        model.ingest(raw)
        now = time.time()
        if now - last_draw < _MIN_REFRESH_INTERVAL:
            continue
        sys.stdout.write(clear_screen())
        sys.stdout.write(render(model.snapshot(), started))
        sys.stdout.flush()
        last_draw = now

    # Final draw on EOF
    sys.stdout.write(clear_screen())
    sys.stdout.write(render(model.snapshot(), started))
    sys.stdout.flush()
    return 0


if __name__ == "__main__":
    sys.exit(main())
