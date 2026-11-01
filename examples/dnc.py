"""The DNC list stops calls before the dial, in two ways:

1. Through the API: you put the number on a campaign's DNC list. The lead of
   that number never dials.
2. From the agent: the callee asks not to be called again, and the agent
   reports a DNC request. The number goes on the campaign's DNC list, and
   the next lead with that number stops at once, before its dial.

The recipe uses the campaign scope, and it removes its entries at the end,
so the number stays callable. The scope "system" stops every campaign.

    docker compose ... run --rm examples dnc.py
"""

from demo import LIVEKIT_AGENT, Checks, connect, demo_lead, follow, number_to_call, outcome, show_leads

api, check, to = connect(), Checks(), number_to_call()

print("1. A DNC entry from the API")
listed = api.create_campaign("dnc-from-the-api", LIVEKIT_AGENT, concurrency_cap=1)["id"]
api.add_leads(listed, [demo_lead(to, "listed")])
# The same number waits in another campaign, which this recipe does not start.
other = api.create_campaign("dnc-other-campaign", LIVEKIT_AGENT)["id"]
api.add_leads(other, [demo_lead(to, "other")])
entry = api.add_dnc(to, scope="campaign", campaign_id=listed, note="the dnc recipe")
try:
    api.start(listed)
    follow(api, listed)
    show_leads(api, listed)
finally:
    api.remove_dnc(entry["id"])
[lead] = api.leads(listed)
check.ok(lead["status"] == "dnc", "the listed lead is dnc", lead["status"])
check.ok(not any(a["dialed_at"] for a in api.attempts(listed)), "the dialer did not dial it")
[waiting] = api.leads(other)
check.ok(waiting["status"] == "new", "the entry did not stop the number in another campaign", waiting["status"])

print("\n2. A DNC request from the agent")
asked = api.create_campaign("dnc-from-the-agent", LIVEKIT_AGENT, concurrency_cap=1)["id"]
api.add_leads(asked, [
    demo_lead(to, "stop", outcome("NI", dnc="campaign")),
    demo_lead(to, "next", outcome("SALE")),
])
try:
    api.start(asked)
    follow(api, asked)
    show_leads(api, asked)
    entries = [e for e in api.dnc(campaign_id=asked) if e["campaign_id"] == asked]
finally:
    for e in api.dnc(campaign_id=asked):
        if e["campaign_id"] == asked:
            api.remove_dnc(e["id"])
leads = {l["context"]["name"]: l for l in api.leads(asked)}
check.ok(leads["stop"]["status"] == "dnc", "the agent's DNC request made the lead dnc", leads["stop"]["status"])
check.ok([(e["scope"], e["source"]) for e in entries] == [("campaign", "agent")],
         "the number went on the campaign's DNC list, from the agent", entries)
check.ok((leads["next"]["status"], leads["next"]["called_count"]) == ("dnc", 0),
         "the next lead with that number stopped before a dial", (leads["next"]["status"], leads["next"]["called_count"]))
check.exit()
