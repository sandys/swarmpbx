"""A client for the AgentDialer API, with the Python standard library only.

Use it as a module in your own flows:

    from agentdialer import Dialer

    api = Dialer()  # AD_API_URL, else http://127.0.0.1:8080
    c = api.create_campaign("my-calls", {"runtime": "livekit", "agent_name": "demo"})
    api.add_leads(c["id"], [{"phone_number": "+12025550123", "timezone": "America/New_York"}])
    api.start(c["id"])
    api.wait_until_done(c["id"])
    print(api.attempts(c["id"]))

Or send one request from the command line:

    python agentdialer.py GET /v1/campaigns/1
    python agentdialer.py PATCH /v1/campaigns/1 '{"concurrency_cap": 4}'

Each method sends one request and returns the decoded JSON. A response that
is not 2xx raises ApiError with the status and the API's error.
"""

from __future__ import annotations

import json
import os
import sys
import time
import urllib.error
import urllib.parse
import urllib.request
from datetime import datetime, timezone
from typing import Callable, Iterable


class ApiError(Exception):
    def __init__(self, method: str, path: str, status: int, body):
        self.status, self.body = status, body
        super().__init__(f"{method} {path}: HTTP {status}: {body}")


class Dialer:
    def __init__(self, url: str | None = None, timeout_s: float = 30):
        self.url = (url or os.environ.get("AD_API_URL") or "http://127.0.0.1:8080").rstrip("/")
        self.timeout_s = timeout_s

    def request(self, method: str, path: str, body=None, **query):
        query = {k: v for k, v in query.items() if v is not None}
        if query:
            path += "?" + urllib.parse.urlencode(query)
        data = None if body is None else json.dumps(body).encode()
        req = urllib.request.Request(
            self.url + path, data=data, method=method, headers={"content-type": "application/json"}
        )
        try:
            with urllib.request.urlopen(req, timeout=self.timeout_s) as resp:
                raw = resp.read()
        except urllib.error.HTTPError as e:
            raw = e.read()
            try:
                detail = json.loads(raw)
            except ValueError:
                detail = raw.decode(errors="replace")
            raise ApiError(method, path, e.code, detail) from None
        return json.loads(raw) if raw else None

    # ---- the API is up ----

    def health(self) -> dict:
        return self.request("GET", "/health")

    def wait_until_ready(self, timeout_s: float = 120) -> None:
        """Wait until the API answers, for example right after the stack starts."""
        deadline = time.monotonic() + timeout_s
        while True:
            try:
                self.health()
                return
            except (OSError, ApiError):
                if time.monotonic() > deadline:
                    raise
                time.sleep(1)

    # ---- campaigns ----

    def create_campaign(
        self,
        name: str,
        agent: dict | None = None,
        concurrency_cap: int = 1,
        org_id: int = 1,
        retry: dict | None = None,
    ) -> dict:
        """A campaign in the `draft` state. `agent` is
        {"runtime": "livekit", "agent_name": ...} or
        {"runtime": "pipecat", "start_url": ...}, with the optional keys
        dial_order, join_timeout_s, outcome_wait_s, and metadata. `retry`
        holds the retry settings that differ from the defaults: busy_s,
        no_answer_s, voicemail_s, failed_s, error_s, abandoned_s, max_calls,
        and callback_min_s."""
        body = {"org_id": org_id, "name": name, "concurrency_cap": concurrency_cap}
        if agent is not None:
            body["agent"] = agent
        if retry is not None:
            body["retry"] = retry
        return self.request("POST", "/v1/campaigns", body)

    def campaign(self, campaign_id: int) -> dict:
        return self.request("GET", f"/v1/campaigns/{campaign_id}")

    def campaigns(self, org_id: int = 1) -> list:
        return self.request("GET", "/v1/campaigns", org_id=org_id)

    def update_campaign(self, campaign_id: int, **settings) -> dict:
        """Change `name`, `concurrency_cap`, `agent`, or `retry` (only the
        retry keys that you give), in any state. The next claim and the next
        settlement use the new values."""
        return self.request("PATCH", f"/v1/campaigns/{campaign_id}", settings)

    def transition(self, campaign_id: int, transition: str) -> dict:
        return self.request(
            "POST", f"/v1/campaigns/{campaign_id}/transition", {"transition": transition}
        )

    def start(self, campaign_id: int) -> dict:
        """Start a draft campaign: it is `running` and dials."""
        self.transition(campaign_id, "start")
        return self.transition(campaign_id, "sync_complete")

    def pause(self, campaign_id: int) -> dict:
        return self.transition(campaign_id, "pause")

    def resume(self, campaign_id: int) -> dict:
        return self.transition(campaign_id, "resume")

    # ---- leads ----

    def add_leads(self, campaign_id: int, leads: list, default_region: str | None = None) -> dict:
        """Load leads. Each lead is {"phone_number", "timezone", "context", ...}.
        The API refuses the whole list if one number is bad."""
        body = {"leads": leads}
        if default_region:
            body["default_region"] = default_region
        return self.request("POST", f"/v1/campaigns/{campaign_id}/leads", body)

    def leads(self, campaign_id: int) -> list:
        return self.request("GET", f"/v1/campaigns/{campaign_id}/leads")

    # ---- the calls and their outcomes ----

    def attempts(self, campaign_id: int, lead_id: int | None = None) -> list:
        """Every call attempt of the campaign (or of one lead), oldest first."""
        out, after = [], 0
        while True:
            page = self.request(
                "GET", f"/v1/campaigns/{campaign_id}/attempts", lead_id=lead_id, after=after, limit=1000
            )
            out += page
            if len(page) < 1000:
                return out
            after = page[-1]["id"]

    def events(self, campaign_id: int, after: int = 0, limit: int = 100) -> list:
        return self.request("GET", f"/v1/campaigns/{campaign_id}/events", after=after, limit=limit)

    def dispositions(self, campaign_id: int) -> dict:
        """{"system": [...], "campaign": [...]}: the codes that an agent can report."""
        return self.request("GET", f"/v1/campaigns/{campaign_id}/dispositions")

    def set_dispositions(self, campaign_id: int, codes: list) -> dict:
        """Replace the campaign's own codes: [{"code", "name", "flags"}]."""
        return self.request("PUT", f"/v1/campaigns/{campaign_id}/dispositions", codes)

    def abandonment(self, campaign_id: int) -> dict:
        return self.request("GET", f"/v1/campaigns/{campaign_id}/abandonment")

    # ---- the DNC list ----

    def add_dnc(
        self,
        phone_number: str,
        scope: str = "system",
        campaign_id: int | None = None,
        note: str | None = None,
    ) -> dict:
        """scope "system" stops every campaign; "campaign" stops one campaign."""
        body = {"phone_number": phone_number, "scope": scope}
        if campaign_id is not None:
            body["campaign_id"] = campaign_id
        if note:
            body["note"] = note
        return self.request("POST", "/v1/dnc", body)

    def dnc(self, campaign_id: int | None = None) -> list:
        """The system list, and the list of `campaign_id` when you give it."""
        return self.request("GET", "/v1/dnc", campaign_id=campaign_id)

    def remove_dnc(self, entry_id: int) -> None:
        self.request("DELETE", f"/v1/dnc/{entry_id}")

    # ---- pacing ----

    def pacing(self, campaign_id: int) -> dict:
        return self.request("GET", f"/v1/campaigns/{campaign_id}/pacing")

    def set_pacing(self, campaign_id: int, **settings) -> dict:
        """mode="adaptive"|"strict", pool={...}, or reset=True."""
        return self.request("PUT", f"/v1/campaigns/{campaign_id}/pacing", settings)

    def agent_pools(self) -> list:
        return self.request("GET", "/v1/agent-pools")

    # ---- waiting ----

    def wait_until_done(
        self,
        campaign_id: int,
        timeout_s: float = 600,
        on_attempt: Callable[[dict], None] | None = None,
    ) -> dict:
        """Wait until the campaign is `completed`: no lead waits for a call now
        or later. `on_attempt` gets each attempt once, when it settles."""
        deadline, seen = time.monotonic() + timeout_s, set()
        while True:
            campaign = self.campaign(campaign_id)
            if on_attempt:
                for a in self.attempts(campaign_id):
                    if a["settled_at"] and a["id"] not in seen:
                        seen.add(a["id"])
                        on_attempt(a)
            if campaign["state"] == "completed":
                return campaign
            if time.monotonic() > deadline:
                raise TimeoutError(
                    f"campaign {campaign_id} is still {campaign['state']} after {timeout_s:.0f} s"
                )
            time.sleep(2)


# ---- times ----


def parse_time(value: str | None) -> datetime | None:
    """An API time (RFC 3339) as an aware datetime, or None."""
    if not value:
        return None
    value = value.replace("Z", "+00:00")
    head, sep, tail = value.partition(".")
    if sep:  # Python before 3.11 reads only 3 or 6 digits of a fraction.
        digits = "".join(ch for ch in tail if ch.isdigit())
        tail = digits[:6].ljust(6, "0") + tail[len(digits):]
        value = f"{head}.{tail}"
    return datetime.fromisoformat(value).astimezone(timezone.utc)


def seconds_between(start: str | None, end: str | None) -> float | None:
    a, b = parse_time(start), parse_time(end)
    return None if a is None or b is None else (b - a).total_seconds()


# ---- printing ----


def table(rows: Iterable[Iterable], headers: list) -> None:
    rows = [["-" if v is None else str(v) for v in row] for row in rows]
    widths = [max([len(h)] + [len(r[i]) for r in rows]) for i, h in enumerate(headers)]
    for row in [headers, ["-" * w for w in widths]] + rows:
        print("  ".join(v.ljust(w) for v, w in zip(row, widths)).rstrip())


def main(argv: list) -> int:
    if len(argv) not in (3, 4):
        print(__doc__.split("Or send one request from the command line:")[1].split("Each method")[0].strip())
        return 2
    method, path = argv[1].upper(), argv[2]
    body = json.loads(argv[3]) if len(argv) == 4 else None
    try:
        out = Dialer().request(method, path, body)
    except ApiError as e:
        print(json.dumps({"status": e.status, "error": e.body}), file=sys.stderr)
        return 1
    if out is not None:
        # Indented on a terminal, one line in a pipe (for scripts).
        print(json.dumps(out, indent=2) if sys.stdout.isatty() else json.dumps(out, separators=(",", ":")))
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
