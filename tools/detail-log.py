#!/usr/bin/env python3
"""aaOS Detail Log — shows agent reasoning and tool calls in plain English."""
import sys, json, re

agents = {}
in_block = False
block_type = ""

for line in sys.stdin:
    line = line.rstrip()
    
    # Strip ANSI codes
    clean = re.sub(r'\x1b\[[0-9;]*m', '', line)
    
    # Track agent names from audit events
    if clean.startswith('{'):
        try:
            d = json.loads(clean)
            e = d.get('event', {})
            aid = d.get('agent_id', '')[:8]
            if e.get('kind') == 'agent_spawned':
                agents[aid] = e.get('manifest_name', '?')
        except:
            pass
        continue
    
    # Parse tracing lines for agent thoughts and tool calls
    if '--- AGENT THINKS ---' in clean:
        # Extract agent_id from the line
        aid_match = re.search(r'agent_id=([a-f0-9-]+)', clean)
        aid = aid_match.group(1)[:8] if aid_match else '?'
        name = agents.get(aid, aid)
        iter_match = re.search(r'iter=(\d+)', clean)
        it = iter_match.group(1) if iter_match else '?'
        print(f'\n\033[1;36m[{name}] thinking (iter {it}):\033[0m', flush=True)
        in_block = True
        block_type = "think"
        continue
    
    if '--- TOOL CALL:' in clean:
        aid_match = re.search(r'agent_id=([a-f0-9-]+)', clean)
        aid = aid_match.group(1)[:8] if aid_match else '?'
        name = agents.get(aid, aid)
        tool_match = re.search(r'TOOL CALL: (\w+)', clean)
        tool = tool_match.group(1) if tool_match else '?'
        print(f'\n\033[1;33m[{name}] calling {tool}:\033[0m', flush=True)
        in_block = True
        block_type = "call"
        continue
    
    if '--- TOOL RESULT:' in clean:
        aid_match = re.search(r'agent_id=([a-f0-9-]+)', clean)
        aid = aid_match.group(1)[:8] if aid_match else '?'
        name = agents.get(aid, aid)
        tool_match = re.search(r'TOOL RESULT: (\w+)', clean)
        tool = tool_match.group(1) if tool_match else '?'
        print(f'\n\033[1;32m[{name}] {tool} result:\033[0m', flush=True)
        in_block = True
        block_type = "result"
        continue
    
    if '--- TOOL ERROR:' in clean:
        aid_match = re.search(r'agent_id=([a-f0-9-]+)', clean)
        aid = aid_match.group(1)[:8] if aid_match else '?'
        name = agents.get(aid, aid)
        tool_match = re.search(r'TOOL ERROR: (\w+)', clean)
        tool = tool_match.group(1) if tool_match else '?'
        print(f'\n\033[1;31m[{name}] {tool} ERROR:\033[0m', flush=True)
        in_block = True
        block_type = "error"
        continue
    
    if '--- END ---' in clean:
        in_block = False
        block_type = ""
        continue
    
    if in_block:
        # For tool results with file content, truncate long lines
        if block_type == "result" and len(clean) > 500:
            print(f'  {clean[:200]}... ({len(clean)} chars)', flush=True)
        else:
            print(f'  {clean}', flush=True)
    
    # Also catch spawn/stop INFO lines
    if 'agent spawned' in clean and 'name=' in clean:
        name_match = re.search(r'name=(\S+)', clean)
        if name_match:
            print(f'\033[1;32m>>> SPAWNED: {name_match.group(1)}\033[0m', flush=True)
    
    if 'agent stopped' in clean:
        print(f'\033[1;31m>>> STOPPED\033[0m', flush=True)
    
    sys.stdout.flush()
