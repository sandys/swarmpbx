"""Agent first: the dialer starts the agent, waits until it is in the room,
and then dials. No call is abandoned, but each call that rings holds an
agent. Use it when your agent cannot join within 2 s of the answer.

    docker compose ... run --rm examples agent_first.py
"""

from demo import LIVEKIT_AGENT, Checks, connect, demo_lead, follow, number_to_call, show_attempts
from agentdialer import seconds_between

api, check, to = connect(), Checks(), number_to_call()

agent = {**LIVEKIT_AGENT, "dial_order": "agent_first", "join_timeout_s": 20}
campaign = api.create_campaign("agent-first", agent, concurrency_cap=1)
api.add_leads(campaign["id"], [demo_lead(to, "agent-first")])
api.start(campaign["id"])
follow(api, campaign["id"])
show_attempts(api, campaign["id"])

[call] = api.attempts(campaign["id"])
before = seconds_between(call["agent_ready_at"], call["dialed_at"])
print(f"The agent was in the room {before:.2f} s before the dial." if before is not None else "")
check.ok(call["dial_order"] == "agent_first", "the dialer started the agent first", call["dial_order"])
check.ok(before is not None and before >= 0, "the agent was in the room before the dial", before)
check.ok(call["disposition"] == "DONE", "the agent reported DONE", call["disposition"])
check.exit()
