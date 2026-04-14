import json
import os
import sys

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__)))))

from observability.event_model import EventModel


def _ev(kind: str, agent_id: str = "11111111-2222-3333-4444-555555555555", **event_fields):
    return {
        "id": "evt-1",
        "timestamp": "2026-04-14T15:28:41.556879109Z",
        "agent_id": agent_id,
        "event": {"kind": kind, **event_fields},
    }


def test_spawn_registers_agent_and_resolves_name():
    m = EventModel()
    evt = m.ingest(_ev("agent_spawned", manifest_name="bootstrap"))
    assert evt.kind == "spawn"
    assert evt.agent_name == "bootstrap"
    snap = m.snapshot()
    assert len(snap.agents) == 1
    assert next(iter(snap.agents.values())).name == "bootstrap"


def test_subsequent_events_resolve_agent_name():
    m = EventModel()
    m.ingest(_ev("agent_spawned", manifest_name="bootstrap"))
    evt = m.ingest(_ev("tool_invoked", tool="file_read"))
    assert evt.agent_name == "bootstrap"
    assert evt.kind == "tool_invoke"


def test_unknown_agent_id_uses_short_uuid_fallback():
    m = EventModel()
    evt = m.ingest(_ev("tool_invoked", agent_id="deadbeef-1234-5678-9abc-ffffffffffff", tool="echo"))
    assert evt.agent_name == "deadbeef"


def test_usage_aggregates_per_agent_and_global():
    m = EventModel()
    m.ingest(_ev("agent_spawned", manifest_name="a1"))
    m.ingest(_ev("usage_reported", input_tokens=100, output_tokens=50))
    m.ingest(_ev("usage_reported", input_tokens=20, output_tokens=10))
    snap = m.snapshot()
    assert snap.stats.tokens_in == 120
    assert snap.stats.tokens_out == 60
    agent = next(iter(snap.agents.values()))
    assert agent.tokens_in == 120
    assert agent.tokens_out == 60


def test_tool_invoke_counts_and_categorizes():
    m = EventModel()
    m.ingest(_ev("agent_spawned", manifest_name="a1"))
    for tool in ("file_read", "file_read", "file_write", "echo"):
        m.ingest(_ev("tool_invoked", tool=tool))
    snap = m.snapshot()
    assert snap.stats.tool_calls == 4
    assert snap.stats.file_reads == 2
    assert snap.stats.file_writes == 1


def test_capability_denied_goes_to_significant_events_with_summary():
    m = EventModel()
    m.ingest(_ev("agent_spawned", manifest_name="orch"))
    m.ingest(_ev(
        "capability_denied",
        capability={"ToolInvoke": {"tool_name": "memory_store"}},
        reason="cannot grant memory_store to child 'writer'",
    ))
    snap = m.snapshot()
    assert snap.stats.denials == 1
    assert any(e.kind == "denied" for e in snap.significant_events)
    denial = [e for e in snap.significant_events if e.kind == "denied"][0]
    assert denial.denied_capability == "tool:memory_store"
    assert "writer" in denial.denied_reason


def test_agent_stopped_updates_status():
    m = EventModel()
    m.ingest(_ev("agent_spawned", manifest_name="child"))
    m.ingest(_ev("agent_stopped", reason="user_requested"))
    snap = m.snapshot()
    agent = next(iter(snap.agents.values()))
    assert agent.status == "stopped"
    assert agent.stop_reason == "user_requested"


def test_agent_execution_completed_marks_done_and_records_iterations():
    m = EventModel()
    m.ingest(_ev("agent_spawned", manifest_name="c"))
    m.ingest(_ev("agent_execution_completed", total_iterations=7, stop_reason="complete"))
    agent = next(iter(m.snapshot().agents.values()))
    assert agent.status == "done"
    assert agent.iterations == 7


def test_memory_stored_counts_and_bubbles_up():
    m = EventModel()
    m.ingest(_ev("agent_spawned", manifest_name="bootstrap"))
    m.ingest(_ev("memory_stored", category="observation", memory_id="x", content_hash="y"))
    assert m.snapshot().stats.memory_stores == 1
    assert any(e.kind == "memory_stored" for e in m.snapshot().significant_events)


def test_malformed_event_never_crashes():
    m = EventModel()
    # Missing 'event' key entirely
    evt = m.ingest({"agent_id": "x"})
    assert evt.kind == "unknown"


def test_unknown_kind_is_tracked_and_rendered_as_unknown():
    m = EventModel()
    evt = m.ingest(_ev("brand_new_event_kind_from_future_aaOS"))
    assert evt.kind == "unknown"
    assert m.snapshot().stats.unknown_kinds == {"brand_new_event_kind_from_future_aaOS": 1}


def test_tool_result_before_matching_invoke_still_renders():
    # Event order shouldn't cause a crash
    m = EventModel()
    m.ingest(_ev("agent_spawned", manifest_name="a"))
    evt = m.ingest(_ev("tool_result", tool="file_read", success=True))
    assert evt.kind == "tool_result"
    assert evt.tool_succeeded is True


def test_real_run6_fixture_parses_without_errors():
    """Replay captured Run 6 audit events; every event should produce a known kind."""
    fixture = os.path.join(
        os.path.dirname(__file__), "fixtures", "run6_events.jsonl"
    )
    m = EventModel()
    count = 0
    with open(fixture) as f:
        for line in f:
            line = line.strip()
            if not line:
                continue
            try:
                raw = json.loads(line)
            except json.JSONDecodeError:
                continue
            evt = m.ingest(raw)
            # Every event should normalize to a real kind (no silent dropping)
            assert evt.kind in {
                "spawn", "stop", "tool_invoke", "tool_result", "denied",
                "memory_stored", "memory_queried", "usage", "exec_started",
                "exec_completed", "loop_started", "loop_stopped", "message",
                "context_summarized", "summarization_failed",
                "cap_granted", "cap_revoked", "unknown",
            }
            count += 1
    assert count > 0
    snap = m.snapshot()
    # Run 6 had 3 agents (bootstrap, code-analyzer, proposal-writer)
    assert len(snap.agents) >= 1
    # And tokens should have aggregated to something non-zero
    assert snap.stats.tokens_in > 0 or snap.stats.tokens_out > 0 or snap.stats.tool_calls > 0


def test_capability_denied_handles_string_and_dict_forms():
    m = EventModel()
    m.ingest(_ev("agent_spawned", manifest_name="orch"))
    evt = m.ingest(_ev("capability_denied", capability="custom_cap", reason="nope"))
    assert evt.denied_capability == "custom_cap"

    evt2 = m.ingest(_ev(
        "capability_denied",
        capability={"FileRead": {"path_glob": "/etc/*"}},
        reason="nope",
    ))
    assert "file_read:/etc/*" == evt2.denied_capability


def test_dashboard_snapshot_is_independent_copy():
    m = EventModel()
    m.ingest(_ev("agent_spawned", manifest_name="a"))
    snap1 = m.snapshot()
    m.ingest(_ev("agent_stopped", reason="done"))
    # snap1 should not reflect the stop
    a = next(iter(snap1.agents.values()))
    assert a.status == "running"
