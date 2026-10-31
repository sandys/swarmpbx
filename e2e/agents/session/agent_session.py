"""The client of the dialer's agent session stream, for the reference agents.

The node gives each agent a session in its call facts:

  {"attempt_id": 5, "session": {"url": "http://api:9090", "token": "<hex>"}, ...}

The agent opens the stream with Hello. The dialer answers with Welcome, which
lists the disposition codes of the campaign. When the agent knows the result
of the call, it sends one Outcome, and the dialer answers Ack or Rejected.
The contract is proto/agentdialer/session/v1/session.proto.
"""

from datetime import datetime, timedelta, timezone

import grpc

from agentdialer.session.v1 import session_pb2 as pb
from agentdialer.session.v1 import session_pb2_grpc as rpc

SCHEMA_VERSION = 1
ANSWERED_BY = {
    "unknown": pb.ANSWERED_BY_UNKNOWN,
    "human": pb.ANSWERED_BY_HUMAN,
    "machine": pb.ANSWERED_BY_MACHINE,
}


def _channel(url: str) -> grpc.aio.Channel:
    if url.startswith("https://"):
        return grpc.aio.secure_channel(url[len("https://"):], grpc.ssl_channel_credentials())
    return grpc.aio.insecure_channel(url.removeprefix("http://"))


def outcome_message(outcome: dict) -> pb.Outcome:
    """An Outcome from its JSON form, with the field names of the proto:

    {"disposition": "CALLBK", "answered_by": "human", "summary": "...",
     "callback": {"local_time": "2026-03-02T09:30", "zone": "America/New_York"},
     "dnc": {"scope": "campaign"}, "transfer_to": "...", "disclosures": [...]}
    """
    msg = pb.Outcome(
        schema_version=SCHEMA_VERSION,
        disposition=outcome.get("disposition", ""),
        answered_by=ANSWERED_BY[outcome.get("answered_by", "unknown")],
        transfer_to=outcome.get("transfer_to", ""),
        summary=outcome.get("summary", ""),
        disclosures=outcome.get("disclosures", []),
    )
    if "callback" in outcome:
        cb = outcome["callback"]
        msg.callback.CopyFrom(pb.Callback(local_time=cb["local_time"], zone=cb.get("zone", "")))
    if "dnc" in outcome:
        msg.dnc.CopyFrom(pb.Dnc(scope=(outcome["dnc"] or {}).get("scope", "")))
    return msg


class Session:
    """An open session of one call."""

    def __init__(self, channel, call, welcome):
        self._channel, self._call = channel, call
        self.welcome = welcome

    @classmethod
    async def open(cls, call_facts: dict, agent: str) -> "Session | None":
        """Open the session of the call. None when the facts have no session.
        A refused token raises grpc.aio.AioRpcError."""
        facts = call_facts.get("session")
        if not facts:
            return None
        channel = _channel(facts["url"])
        call = rpc.SessionStub(channel).Open()
        hello = pb.Hello(attempt_id=call_facts["attempt_id"], token=facts["token"], agent=agent)
        await call.write(pb.AgentMessage(hello=hello))
        first = await call.read()
        if first is grpc.aio.EOF or first.WhichOneof("body") != "welcome":
            await channel.close()
            raise RuntimeError(f"no welcome: {first}")
        return cls(channel, call, first.welcome)

    def codes(self) -> list:
        return [d.code for d in self.welcome.dispositions]

    async def report(self, outcome: dict) -> str:
        """Send the outcome. Returns "ack", "duplicate", or "rejected: <reason>"."""
        await self._call.write(pb.AgentMessage(outcome=outcome_message(outcome)))
        reply = await self._call.read()
        if reply is grpc.aio.EOF:
            return "closed"
        kind = reply.WhichOneof("body")
        if kind == "ack":
            return "duplicate" if reply.ack.duplicate else "ack"
        if kind == "rejected":
            return f"rejected: {reply.rejected.reason}"
        return f"unexpected: {kind}"

    async def close(self) -> None:
        try:
            await self._call.done_writing()
        finally:
            await self._channel.close()


# The test control of the reference agents: the outcome that they report.
DEFAULT_OUTCOME = {"disposition": "DONE", "answered_by": "human"}


def planned_outcome(call_facts: dict) -> "dict | None":
    """The lead context's "outcome", else DONE. "outcome": null reports none.
    A callback {"in_s": N} becomes the UTC time N seconds after now."""
    context = (call_facts.get("lead") or {}).get("context") or {}
    outcome = context.get("outcome", DEFAULT_OUTCOME)
    if outcome and "in_s" in outcome.get("callback", {}):
        at = datetime.now(timezone.utc) + timedelta(seconds=outcome["callback"]["in_s"])
        outcome = {**outcome, "callback": {"local_time": at.strftime("%Y-%m-%dT%H:%M"), "zone": "UTC"}}
    return outcome
