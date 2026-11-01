"""Pacing by join time, and changes at run time.

The dialer paces each agent pool by the time its agents take to join. The
pool's limit k starts at 1. Each round of fast joins raises it, so more calls
run at the same time. A slow join, a start that fails, or no agent at the
2 s limit lowers it.

The recipe starts the pool again (reset), and it loads 6 leads with a cap of
3 calls. After 3 calls, it raises the cap to 4 and sets the strict mode, with
no restart. The next claim uses the new values.

    docker compose ... run --rm examples pacing.py
"""

from demo import LIVEKIT_AGENT, Checks, connect, demo_lead, follow, number_to_call, show_attempts
from agentdialer import parse_time

api, check, to = connect(), Checks(), number_to_call()

cid = api.create_campaign("pacing", LIVEKIT_AGENT, concurrency_cap=3)["id"]
api.add_leads(cid, [demo_lead(to, f"lead-{i}", talk_s=8) for i in range(1, 7)])
p = api.set_pacing(cid, reset=True)
print(f"The pool {p['pool']['pool']} starts again: k={p['pool']['k']}, limit {p['limit']}.")
api.start(cid)


def change_at_run_time(_call: dict, settled: int) -> None:
    if settled == 3:
        api.update_campaign(cid, concurrency_cap=4)
        p = api.set_pacing(cid, mode="strict")
        print(f"  -> cap 4, mode {p['mode']}; the pool learned k={p['pool']['k']} "
              f"from {p['pool']['samples']} joins (p95 {p['pool']['recent'].get('p95_ms')} ms)")


follow(api, cid, each=change_at_run_time)
show_attempts(api, cid)
p = api.pacing(cid)
print(f"At the end: mode {p['mode']}, k={p['pool']['k']}, limit {p['limit']}.")

calls = api.attempts(cid)
limits = [a["pool_limit"] for a in calls if a["pool_limit"]]
check.ok(all(a["pool_window"] <= a["pool_limit"] for a in calls if a["pool_limit"]),
         "no call started above the pool's limit", [(a["pool_window"], a["pool_limit"]) for a in calls])
check.ok(max(limits, default=0) >= 2, "fast joins raised the limit above 1", limits)
spans = [(parse_time(a["answered_at"]), parse_time(a["ended_at"])) for a in calls if a["answered_at"] and a["ended_at"]]
overlap = any(a0 < b1 and b0 < a1 for i, (a0, a1) in enumerate(spans) for b0, b1 in spans[i + 1:])
check.ok(overlap, "two calls ran at the same time")
c = api.campaign(cid)
check.ok((c["concurrency_cap"], p["mode"]) == (4, "strict"), "the cap and the mode changed at run time",
         (c["concurrency_cap"], p["mode"]))
check.ok(all(a["outcome"] == "completed" for a in calls), "every call completed", [a["outcome"] for a in calls])
check.exit()
