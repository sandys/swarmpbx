"""Change a running campaign through the API:

- add a lead while the campaign runs: the dialer calls it at once.
- change the retry settings while the campaign runs: the next outcome uses
  them.

The campaign starts with two leads and calls them one at a time. The agent
of the first lead asks for a call back in 40 s. After the first call, the
recipe raises callback_min_s from 5 s to 60 s. After the second call, while
the first lead waits, the recipe adds a third lead. Its agent asks for a call
back in 20 s. The dialer refuses it, because 20 s is now under the floor.

    docker compose ... run --rm examples changes.py
"""

from demo import (
    DONE, LIVEKIT_AGENT, Checks, connect, demo_lead, follow, number_to_call, outcome,
    show_attempts, show_leads,
)
from agentdialer import seconds_between

api, check, to = connect(), Checks(), number_to_call()

agent = {**LIVEKIT_AGENT, "outcome_wait_s": 5}
campaign = api.create_campaign("changes", agent, concurrency_cap=1, retry={"callback_min_s": 5})
cid = campaign["id"]
api.add_leads(cid, [
    demo_lead(to, "waits", [outcome("CALLBK", callback_in_s=40), DONE]),
    demo_lead(to, "second"),
])
api.start(cid)


def change(_call: dict, settled: int) -> None:
    if settled == 1:
        api.update_campaign(cid, retry={"callback_min_s": 60})
        print("  changed callback_min_s to 60 s")
    # The first lead waits for its callback, and no other lead is left. So
    # only the new lead's event can start the next call.
    if settled == 2:
        api.add_leads(cid, [demo_lead(to, "added", outcome("CALLBK", callback_in_s=20))])
        print("  added the lead 'added'")


follow(api, cid, each=change)
show_attempts(api, cid)
show_leads(api, cid)

leads = {l["context"]["name"]: l for l in api.leads(cid)}
calls = {name: api.attempts(cid, lead_id=l["id"]) for name, l in leads.items()}
retry = api.campaign(cid)["retry"]
check.ok((retry["callback_min_s"], retry["busy_s"]) == (60, 300),
         "PATCH changed callback_min_s, and the other retry settings kept their values",
         retry)

new, first = leads.get("added"), (calls.get("waits") or [None])[0]
gap = seconds_between(new["created_at"], calls["added"][0]["dialed_at"]) if new and calls.get("added") else None
due = ((first or {}).get("agent_outcome") or {}).get("callback", {}).get("at_utc")
check.ok(gap is not None and 0 <= gap < 4,
         f"the dialer called the added lead {gap:.1f} s after the add" if gap is not None
         else "the dialer called the added lead", gap)
check.ok(bool(due and calls.get("added")) and seconds_between(calls["added"][0]["dialed_at"], due) > 0,
         "the added lead was called before the callback of the first lead was due", due)
check.ok([a["disposition"] for a in calls.get("added", [])] == [None]
         and leads["added"]["status"] == "dispositioned",
         "the dialer refused the 20 s callback under the new 60 s floor (one call, no disposition)",
         ([a["disposition"] for a in calls.get("added", [])], leads.get("added", {}).get("status")))
check.ok([a["disposition"] for a in calls.get("waits", [])] == ["CALLBK", "DONE"],
         "the first lead's callback, asked before the change, came", [a["disposition"] for a in calls.get("waits", [])])
check.exit()
