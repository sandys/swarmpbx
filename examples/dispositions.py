"""Your own disposition codes. The system codes (SALE, NI, CALLBK, DNC, and
others) work in every campaign. A campaign can add its own codes, each with
the flags that tell the dialer what the code means.

- booked: the agent reports the campaign's code DEMO_BOOKED. The dialer
  accepts it.
- unknown: the agent reports a code that the campaign does not have. The
  dialer refuses the report, and it settles the lead by the call result.

    docker compose ... run --rm examples dispositions.py
"""

from demo import LIVEKIT_AGENT, Checks, connect, demo_lead, follow, number_to_call, outcome, show_attempts

api, check, to = connect(), Checks(), number_to_call()

cid = api.create_campaign("dispositions", {**LIVEKIT_AGENT, "outcome_wait_s": 5}, concurrency_cap=1)["id"]
codes = api.set_dispositions(cid, [{
    "code": "DEMO_BOOKED",
    "name": "Demo booked",
    "flags": {"human_answered": True, "sale": True, "customer_contact": True, "completed": True},
}])
print("The campaign's own codes:", [c["code"] for c in codes["campaign"]])
print("The system codes:", [c["code"] for c in codes["system"]])
api.add_leads(cid, [
    demo_lead(to, "booked", outcome("DEMO_BOOKED", summary="a demo on Tuesday")),
    demo_lead(to, "unknown", outcome("NOT_A_CODE")),
])
api.start(cid)
follow(api, cid)
show_attempts(api, cid)

names = {l["id"]: l["context"]["name"] for l in api.leads(cid)}
calls = {names[a["lead_id"]]: a for a in api.attempts(cid)}
booked = calls["booked"]
check.ok(booked["disposition"] == "DEMO_BOOKED", "the dialer took the campaign's code", booked["disposition"])
flags = ((booked["agent_outcome"] or {}).get("disposition") or {}).get("flags", {})
check.ok(flags.get("sale") is True, "the outcome keeps the code's flags", flags)
check.ok(calls["unknown"]["disposition"] is None, "the dialer refused the unknown code", calls["unknown"]["disposition"])
check.ok(calls["unknown"]["settled_at"] is not None, "the dialer settled that lead by the call result")
check.exit()
