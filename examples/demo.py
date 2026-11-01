"""The shared parts of the recipes: the demo agents' controls, the number to
call, the checks, and the tables.

The demo agents of the quickstart stack do not talk. They take two controls
from the lead's context, so that a recipe can show each outcome:

- "outcome": the outcome that the agent reports, as JSON text. A list gives
  one outcome for each call to the lead, in order. JSON null reports none.
- "talk_s": the agent leaves the call after this many seconds.

A real agent decides the outcome from the talk, and it ignores these keys.
"""

from __future__ import annotations

import json
import os
import sys
import time

from agentdialer import Dialer, seconds_between, table

# The agents of the quickstart stack.
LIVEKIT_AGENT = {"runtime": "livekit", "agent_name": "demo"}
PIPECAT_AGENT = {"runtime": "pipecat", "start_url": "http://pipecat-bot:8089/start"}

DONE = {"disposition": "DONE", "answered_by": "human"}


def outcome(
    disposition: str | None,
    summary: str | None = None,
    callback_in_s: int | None = None,
    dnc: str | None = None,
) -> dict:
    """An outcome for a demo agent to report. `callback_in_s` asks for a call
    back that many seconds later (the dialer refuses less than the campaign's
    retry setting `callback_min_s`, 60 s by default). `dnc` is
    "campaign" or "system": the callee asked not to be called again."""
    o = {"disposition": disposition, "answered_by": "human"}
    if summary:
        o["summary"] = summary
    if callback_in_s is not None:
        o["callback"] = {"in_s": callback_in_s}
    if dnc:
        o["dnc"] = {"scope": dnc}
    return o


def demo_lead(phone_number: str, name: str, reports=DONE, talk_s: float = 5) -> dict:
    """A lead for a demo agent. `reports` is an outcome, a list with one
    outcome for each call, or None for no outcome."""
    return {
        "phone_number": phone_number,
        "timezone": "America/New_York",
        "context": {"name": name, "outcome": json.dumps(reports), "talk_s": str(talk_s)},
    }


def number_to_call() -> str:
    """The number of each lead: `--to <number>`, else QUICKSTART_NUMBER (the
    stack sets it from LKCLOUD_NUMBER in .env.lkcloud)."""
    args = sys.argv[1:]
    if "--to" in args and args.index("--to") + 1 < len(args):
        return args[args.index("--to") + 1]
    number = os.environ.get("QUICKSTART_NUMBER", "")
    if not number:
        sys.exit("No number to call: give --to <number>, or run setup (e2e/livekit-cloud/run.sh setup).")
    return number


def connect() -> Dialer:
    api = Dialer()
    api.wait_until_ready()
    return api


def name_of(campaign_id: int, api: Dialer) -> dict:
    """Lead id to the lead's name in its context."""
    return {l["id"]: l["context"].get("name", str(l["id"])) for l in api.leads(campaign_id)}


def join_s(a: dict) -> float | None:
    """How long after the answer the agent was in the room (customer first)."""
    return seconds_between(a["answered_at"], a["agent_ready_at"])


def follow(api: Dialer, campaign_id: int, timeout_s: float = 600, each=None) -> dict:
    """Wait for the campaign to complete. Print each call when it settles.
    `each(call, settled)` runs after each print; `settled` counts the calls."""
    names = name_of(campaign_id, api)
    started = time.monotonic()
    settled = 0

    def show(a: dict) -> None:
        nonlocal settled
        settled += 1
        if a["lead_id"] not in names:  # a lead added during the run
            names.update(name_of(campaign_id, api))
        j = join_s(a)
        print(
            f"  {time.monotonic() - started:5.0f} s  call {a['id']} to {names.get(a['lead_id'])}: "
            f"{a['outcome']}, disposition {a['disposition'] or '-'}"
            + (f", agent in the room {j:.2f} s after the answer" if j is not None and j >= 0 else "")
        )
        if each:
            each(a, settled)

    print(f"Campaign {campaign_id} runs. Each line is a call when it settles:")
    return api.wait_until_done(campaign_id, timeout_s, on_attempt=show)


def show_leads(api: Dialer, campaign_id: int) -> None:
    rows = [
        (
            l["id"],
            l["context"].get("name"),
            l["status"],
            l["called_count"],
            l["retry_reason"],
            l["next_eligible_at"],
        )
        for l in api.leads(campaign_id)
    ]
    table(rows, ["lead", "name", "status", "calls", "waits for", "next call at"])


def show_attempts(api: Dialer, campaign_id: int) -> None:
    names = name_of(campaign_id, api)
    rows = []
    for a in api.attempts(campaign_id):
        j = join_s(a)
        rows.append(
            (
                a["id"],
                names.get(a["lead_id"]),
                a["dial_order"],
                a["outcome"],
                a["disposition"],
                f"{j:.2f}" if j is not None else None,
                f"{a['pool_window']}/{a['pool_limit']}" if a["pool_limit"] else None,
            )
        )
    table(rows, ["call", "lead", "dial order", "result", "disposition", "join s", "pool"])


class Checks:
    """The recipe's checks. Each prints one line; `exit()` fails on any FAIL."""

    def __init__(self):
        self.failed = 0

    def ok(self, passed: bool, what: str, got=None) -> bool:
        print(("ok:   " if passed else "FAIL: ") + what + ("" if passed or got is None else f" (got {got})"))
        self.failed += 0 if passed else 1
        return passed

    def exit(self) -> None:
        sys.exit(1 if self.failed else 0)
