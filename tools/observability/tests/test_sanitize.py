import os, sys
sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__)))))

from observability.sanitize import sanitize, truncate


def test_strips_csi_color_codes():
    assert sanitize("\x1b[31mred\x1b[0m text") == "red text"


def test_strips_csi_cursor_codes():
    assert sanitize("hello\x1b[2J\x1b[Hworld") == "helloworld"


def test_strips_osc_window_title():
    assert sanitize("before\x1b]0;title\x07after") == "beforeafter"


def test_strips_bel_and_del():
    assert sanitize("a\x07b\x7fc") == "abc"


def test_strips_cr_and_backspace():
    assert sanitize("a\rb\x08c") == "abc"


def test_keeps_tabs_by_default():
    assert sanitize("a\tb") == "a\tb"


def test_keeps_newlines_when_requested():
    assert sanitize("a\nb", keep_newlines=True) == "a\nb"


def test_drops_newlines_by_default():
    out = sanitize("a\nb")
    assert out == "ab"


def test_handles_empty_string():
    assert sanitize("") == ""
    assert sanitize(None) == ""


def test_strips_c1_controls():
    # 0x9b is CSI in C1
    assert sanitize("a\x9bb") == "ab"


def test_truncate_leaves_short_alone():
    assert truncate("short", 10) == "short"


def test_truncate_cuts_with_suffix():
    out = truncate("a" * 50, 10)
    assert out.endswith("…")
    assert len(out) == 10
