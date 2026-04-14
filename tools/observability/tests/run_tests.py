#!/usr/bin/env python3
"""Tiny test runner (no pytest dependency).

Discovers every top-level function named `test_*` inside the two test modules
and runs them. Fails noisily on the first assertion error, continues to
collect pass/fail totals otherwise.
"""
from __future__ import annotations

import importlib
import os
import sys
import traceback


def run_module(mod_name: str) -> tuple[int, int, list[str]]:
    mod = importlib.import_module(mod_name)
    passed, failed, failures = 0, 0, []
    for name in sorted(dir(mod)):
        if not name.startswith("test_"):
            continue
        fn = getattr(mod, name)
        if not callable(fn):
            continue
        try:
            fn()
        except Exception:
            failed += 1
            failures.append(f"{mod_name}.{name}\n{traceback.format_exc()}")
        else:
            passed += 1
    return passed, failed, failures


def main() -> int:
    # Ensure tools/ is on the path so `observability.*` imports work
    here = os.path.dirname(os.path.abspath(__file__))
    tools_dir = os.path.dirname(os.path.dirname(here))
    sys.path.insert(0, tools_dir)
    # Also add observability/tests/ itself for the local imports
    sys.path.insert(0, here)

    totals = [0, 0]
    all_failures: list[str] = []
    for mod_name in ("test_sanitize", "test_event_model"):
        p, f, fails = run_module(mod_name)
        totals[0] += p
        totals[1] += f
        all_failures.extend(fails)

    if all_failures:
        for f in all_failures:
            print("FAIL:", f)
        print(f"\n{totals[0]} passed, {totals[1]} failed")
        return 1
    print(f"OK — {totals[0]} tests passed")
    return 0


if __name__ == "__main__":
    sys.exit(main())
