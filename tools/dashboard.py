#!/usr/bin/env python3
"""aaOS Live Agent Dashboard"""
import sys, json

agents = {}
stats = {"ti": 0, "to": 0, "tc": 0, "fr": 0, "fw": 0, "sa": 0}

def clear():
    print("\033[2J\033[H", end="")

def c(text, code):
    return "\033[" + code + "m" + str(text) + "\033[0m"

def render():
    clear()
    print(c("=== aaOS Agent Dashboard ===", "1;36"))
    print("  Tokens: " + c("in=" + format(stats["ti"], ","), "33") + " / " + c("out=" + format(stats["to"], ","), "32"))
    print("  Tools: " + str(stats["tc"]) + " | Files: " + str(stats["fr"]) + "R/" + str(stats["fw"]) + "W | Agents: " + str(stats["sa"]))
    print(c("-" * 60, "90"))
    for aid, info in agents.items():
        sc = "32" if info["st"] == "running" else "90"
        nm = c("[" + info["nm"] + "]", "1;" + sc)
        st = c(info["st"], sc)
        print("  " + nm + " " + st + "  in=" + format(info["ti"], ",") + " out=" + format(info["to"], ","))
        la = info.get("la", "")
        if la:
            print("    > " + c(la, "90"))
    print(c("-" * 60, "90"))
    print(c("Live:", "1;37"))

for line in sys.stdin:
    line = line.strip()
    if not line or not line.startswith("{"):
        continue
    try:
        d = json.loads(line)
        e = d.get("event", {})
        k = e.get("kind", "")
        aid = d.get("agent_id", "")[:8]
        ts = d.get("timestamp", "")[:19].replace("T", " ")
        if k == "agent_spawned":
            nm = e.get("manifest_name", "?")
            agents[aid] = {"nm": nm, "st": "running", "ti": 0, "to": 0, "la": "spawned"}
            stats["sa"] += 1
            render()
            print("  " + c(ts, "90") + " " + c("SPAWN", "1;32") + " [" + nm + "]")
        elif k == "agent_stopped":
            if aid in agents:
                agents[aid]["st"] = "stopped"
                agents[aid]["la"] = "stopped"
            nm = agents.get(aid, {}).get("nm", aid)
            render()
            print("  " + c(ts, "90") + " " + c("STOP", "1;31") + " [" + nm + "]")
        elif k == "usage_reported":
            ti = e.get("input_tokens", 0)
            to = e.get("output_tokens", 0)
            stats["ti"] += ti
            stats["to"] += to
            if aid in agents:
                agents[aid]["ti"] += ti
                agents[aid]["to"] += to
            nm = agents.get(aid, {}).get("nm", aid)
            render()
            print("  " + c(ts, "90") + " " + nm + ": thinking (" + str(ti) + "->" + str(to) + ")")
        elif k == "tool_invoked":
            tool = e.get("tool", "?")
            stats["tc"] += 1
            if tool == "file_read": stats["fr"] += 1
            if tool == "file_write": stats["fw"] += 1
            if aid in agents:
                agents[aid]["la"] = tool + "..."
            nm = agents.get(aid, {}).get("nm", aid)
            render()
            print("  " + c(ts, "90") + " " + nm + ": " + c(tool, "33") + "...")
        elif k == "tool_result":
            tool = e.get("tool", "?")
            ok = e.get("success", False)
            mark = c("OK", "32") if ok else c("FAIL", "31")
            if aid in agents:
                agents[aid]["la"] = tool + " -> " + ("OK" if ok else "FAIL")
            nm = agents.get(aid, {}).get("nm", aid)
            render()
            print("  " + c(ts, "90") + " " + nm + ": " + tool + " " + mark)
        elif k == "agent_execution_completed":
            it = e.get("total_iterations", "?")
            if aid in agents:
                agents[aid]["st"] = "done"
                agents[aid]["la"] = "completed (" + str(it) + " iters)"
            nm = agents.get(aid, {}).get("nm", aid)
            render()
            print("  " + c(ts, "90") + " " + c("DONE", "1;32") + " [" + nm + "] " + str(it) + " iterations")
        elif k == "memory_stored":
            cat = e.get("category", "?")
            if aid in agents:
                agents[aid]["la"] = "memory (" + cat + ")"
            nm = agents.get(aid, {}).get("nm", aid)
            render()
            print("  " + c(ts, "90") + " " + nm + ": " + c("memory_store", "35") + " (" + cat + ")")
        sys.stdout.flush()
    except:
        pass
