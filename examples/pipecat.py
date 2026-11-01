"""A pipecat bot as the agent. The dialer sends a start request to the bot's
start_url when the callee answers, and the bot joins the call's room with
pipecat's LiveKit transport. The quickstart stack runs the bot
(pipecat-bot, e2e/agents/pipecat/bot.py). It reports SALE here.

    docker compose ... run --rm examples pipecat.py
"""

from demo import PIPECAT_AGENT, Checks, connect, demo_lead, follow, number_to_call, outcome, show_attempts

api, check, to = connect(), Checks(), number_to_call()

cid = api.create_campaign("pipecat", PIPECAT_AGENT, concurrency_cap=1)["id"]
api.add_leads(cid, [demo_lead(to, "pipecat", outcome("SALE", summary="pipecat sold it"))])
api.start(cid)
follow(api, cid)
show_attempts(api, cid)

[call] = api.attempts(cid)
check.ok((call["agent_identity"] or "").startswith("agent-attempt-"), "the pipecat bot joined the room",
         call["agent_identity"])
check.ok(call["disposition"] == "SALE", "the bot reported SALE", call["disposition"])
check.ok(call["outcome"] == "completed", "the call completed", call["outcome"])
check.exit()
