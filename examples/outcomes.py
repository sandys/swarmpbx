"""What the agent reports decides the next step for the lead:

- sale: the agent reports SALE with a summary. The lead is done.
- callback: the agent asks for a call back in 20 s. The lead waits,
  before the other leads, and the dialer calls it again at that time. On
  that call the agent reports DONE. The campaign's retry setting
  callback_min_s (5 s here, 60 s by default) is the soonest callback that
  the dialer takes.
- silent: the agent reports nothing. The dialer waits outcome_wait_s (5 s
  here), and then settles the lead by the call result.
- no-thanks: the agent reports NI (not interested). The lead is done.

    docker compose ... run --rm examples outcomes.py
"""

from demo import (
    DONE, LIVEKIT_AGENT, Checks, connect, demo_lead, follow, number_to_call, outcome,
    show_attempts, show_leads,
)
from agentdialer import seconds_between

api, check, to = connect(), Checks(), number_to_call()

agent = {**LIVEKIT_AGENT, "outcome_wait_s": 5}
campaign = api.create_campaign("outcomes", agent, concurrency_cap=2, retry={"callback_min_s": 5})
cid = campaign["id"]
api.add_leads(cid, [
    demo_lead(to, "sale", outcome("SALE", summary="bought the plan")),
    # At 20 s, the other calls have ended, so only the due time of the lead
    # can start the second call.
    demo_lead(to, "callback", [outcome("CALLBK", callback_in_s=20), DONE]),
    demo_lead(to, "silent", None),
    demo_lead(to, "no-thanks", outcome("NI")),
])
api.start(cid)
follow(api, cid)
show_attempts(api, cid)
show_leads(api, cid)

leads = {l["context"]["name"]: l for l in api.leads(cid)}
calls = {name: api.attempts(cid, lead_id=l["id"]) for name, l in leads.items()}

sale = calls["sale"][-1]
check.ok(
    (sale["disposition"], (sale["agent_outcome"] or {}).get("summary")) == ("SALE", "bought the plan"),
    "the sale call has the disposition SALE and the agent's summary",
    (sale["disposition"], sale["agent_outcome"]),
)
check.ok([a["disposition"] for a in calls["callback"]] == ["CALLBK", "DONE"],
         "the dialer called the callback lead again, at its time", [a["disposition"] for a in calls["callback"]])
first, again = (calls["callback"] + [None, None])[:2]
due = (first or {}).get("agent_outcome", {}) or {}
due = (due.get("callback") or {}).get("at_utc")
gap = seconds_between(due, again["dialed_at"]) if again and due else None
check.ok(gap is not None and 0 <= gap < 3, f"the second call came at the callback time (+{gap:.1f} s)" if gap is not None else "the second call came at the callback time", gap)
silent = calls["silent"][-1]
# Postgres saves the end event and the settlement, so both times come from one
# clock. A step of that clock during the wait (up to about 2 s on a WSL2
# machine, GAPS.md OPS-9) moves the result, so the check allows 2 s each way.
[ended] = [e for e in api.events(cid, limit=1000)
           if e["kind"] == "attempt_ended" and e["event"]["attempt_id"] == silent["id"]]
waited = seconds_between(ended["at"], silent["settled_at"])
check.ok(silent["disposition"] is None and leads["silent"]["status"] == "dispositioned",
         "the silent call has no disposition, and the lead is settled", (silent["disposition"], leads["silent"]["status"]))
check.ok(waited is not None and 3 <= waited < 7, f"the dialer waited outcome_wait_s for an outcome ({waited:.1f} s)" if waited else "the dialer waited for an outcome", waited)
check.ok(calls["no-thanks"][-1]["disposition"] == "NI", "the no-thanks call has the disposition NI")
check.ok(all(l["status"] == "dispositioned" for l in leads.values()), "every lead is dispositioned",
         {n: l["status"] for n, l in leads.items()})
check.exit()
