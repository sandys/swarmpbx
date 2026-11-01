"""One call: the dialer calls the number, and the demo agent joins when the
callee answers (customer first, the default). The agent reports DONE.

    docker compose ... run --rm examples first_call.py
"""

from demo import LIVEKIT_AGENT, Checks, connect, demo_lead, follow, number_to_call, show_attempts, show_leads

api, check, to = connect(), Checks(), number_to_call()

campaign = api.create_campaign("first-call", LIVEKIT_AGENT, concurrency_cap=1)
api.add_leads(campaign["id"], [demo_lead(to, "first")])
api.start(campaign["id"])
follow(api, campaign["id"])
show_attempts(api, campaign["id"])
show_leads(api, campaign["id"])

[call] = api.attempts(campaign["id"])
[lead] = api.leads(campaign["id"])
check.ok(call["dial_order"] == "customer_first", "the dialer called the customer first", call["dial_order"])
check.ok(call["outcome"] == "completed", "the call connected and ended", call["outcome"])
check.ok(call["disposition"] == "DONE", "the agent reported DONE", call["disposition"])
check.ok(lead["status"] == "dispositioned", "the lead is dispositioned", lead["status"])
check.exit()
