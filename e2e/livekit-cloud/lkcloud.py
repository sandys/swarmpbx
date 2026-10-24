"""Connect agentdialer to LiveKit Cloud SIP (README.md, "Quickstart").

LiveKit Cloud SIP is the dialer's primary target. The project gets a LiveKit
phone number (the free US number of each plan). The node dials that number at
the project's SIP URI, as it dials a carrier. An inbound trunk challenges the
INVITE (407 digest), and a dispatch rule puts the call into a room of its own
with a callee agent. So a call proves our SIP and RTP over the real internet.

For a reference, LiveKit's own SIP client calls the same number through a
LiveKit outbound trunk (`call`). LiveKit numbers are inbound only, so the
outbound trunk points at the project's SIP URI.

Run it through e2e/livekit-cloud/run.sh, which runs it in its tool image (the
official livekit-api SDK and lk CLI) with the repository's .env: LIVEKIT_URL,
LIVEKIT_API_KEY, LIVEKIT_API_SECRET, and LIVEKIT_SIP_URI (the project's SIP
URI: LiveKit Cloud, Project Settings).

Commands:
  whoami        the project URL, the key, and the SIP URI; checks the key
  inspect       the e2e number, trunks, and dispatch rules (read only)
  setup         rent or reuse the number; create or reuse the inbound trunk
                (with a generated digest password), the dispatch rules, and
                the outbound trunk; write .env.lkcloud (git-ignored)
  call [HOLD_S] LiveKit's own SIP client calls the number through the outbound
                trunk, into a room with the callee agent; it ends the call
                after HOLD_S seconds (default 15)
  teardown      delete the trunks and the dispatch rules, and .env.lkcloud;
                the number stays (lk number release gives it back)

It never prints a secret.
"""

import asyncio
import json
import os
import re
import secrets
import string
import subprocess
import sys

from livekit import api

NAME = "agentdialer-e2e"
OUTBOUND = f"{NAME}-outbound"
CALLEE_AGENT = "agentdialer-e2e-callee"
SIP_USER = "agentdialer"
CALLER_ID = "+15550001000"
OUT = os.environ.get("LKCLOUD_E2E_OUT", "/repo/.env.lkcloud")


class ToolError(Exception):
    pass


def mask(value):
    if not value:
        return "-"
    return f"{value[:3]}…{value[-4:]}" if len(value) > 8 else "…"


def env(name):
    return os.environ.get(name, "").strip().strip("'\"")


def project():
    url, key, secret = env("LIVEKIT_URL"), env("LIVEKIT_API_KEY"), env("LIVEKIT_API_SECRET")
    if not (url and key and secret):
        raise ToolError("LIVEKIT_URL, LIVEKIT_API_KEY, and LIVEKIT_API_SECRET must be set in .env")
    return url, key, secret


def sip_host():
    """The host of LIVEKIT_SIP_URI (sip:<id>.sip.livekit.cloud), or None."""
    uri = env("LIVEKIT_SIP_URI")
    if not uri:
        return None
    host = re.sub(r"^sips?:", "", uri).split(";")[0].split("@")[-1].strip("/")
    if not re.fullmatch(r"[A-Za-z0-9.-]+(:\d+)?", host):
        raise ToolError(f"LIVEKIT_SIP_URI is not a SIP URI: {uri!r}")
    return host


NO_SIP_URI = (
    "LIVEKIT_SIP_URI is not set. Add it to .env from LiveKit Cloud, Project "
    "Settings (for example LIVEKIT_SIP_URI=sip:abc123xyz.sip.livekit.cloud). "
    "The project URL does not give it."
)


def lk(*args):
    """Run the lk CLI (the phone-number API) and return its JSON output."""
    done = subprocess.run(["lk", "--quiet", *args, "--json"], capture_output=True, text=True)
    if done.returncode != 0:
        raise ToolError(f"lk {' '.join(args[:2])}: {done.stderr.strip() or done.stdout.strip()}")
    return json.loads(done.stdout or "{}")


def numbers():
    return lk("number", "list").get("items", [])


async def find_trunk(lk_api):
    for t in (await lk_api.sip.list_inbound_trunk(api.ListSIPInboundTrunkRequest())).items:
        if t.name == NAME:
            return t
    return None


async def find_outbound(lk_api):
    for t in (await lk_api.sip.list_outbound_trunk(api.ListSIPOutboundTrunkRequest())).items:
        if t.name == OUTBOUND:
            return t
    return None


async def find_rule(lk_api, name):
    for r in (await lk_api.sip.list_dispatch_rule(api.ListSIPDispatchRuleRequest())).items:
        if r.name == name:
            return r
    return None


async def whoami(lk_api):
    url, key, _ = project()
    print(f"project: {url}")
    print(f"key: {mask(key)}")
    rooms = await lk_api.room.list_rooms(api.ListRoomsRequest())
    print(f"key: valid (rooms now: {len(rooms.rooms)})")
    trunks = await lk_api.sip.list_inbound_trunk(api.ListSIPInboundTrunkRequest())
    print(f"SIP API: available (inbound trunks: {len(trunks.items)})")
    host = sip_host()
    print(f"SIP URI: {'sip:' + host if host else 'NOT SET'}")


async def inspect(lk_api):
    await whoami(lk_api)
    for n in numbers():
        print(f"number: {n.get('e164Format')} status={n.get('status')} rules={[mask(r) for r in n.get('sipDispatchRuleIds', [])]}")
    trunk = await find_trunk(lk_api)
    print(
        f"inbound trunk {NAME!r}: "
        + (f"{mask(trunk.sip_trunk_id)} numbers={list(trunk.numbers) or 'any'} auth_user={trunk.auth_username or '-'}" if trunk else "none")
    )
    out = await find_outbound(lk_api)
    print(
        f"outbound trunk {OUTBOUND!r}: "
        + (f"{mask(out.sip_trunk_id)} address={out.address} numbers={list(out.numbers)}" if out else "none")
    )
    rule = await find_rule(lk_api, NAME)
    if rule is None:
        print(f"dispatch rule {NAME!r}: none")
    else:
        agents = [a.agent_name for a in rule.room_config.agents]
        print(
            f"dispatch rule {NAME!r}: {mask(rule.sip_dispatch_rule_id)} "
            f"trunks={[mask(t) for t in rule.trunk_ids]} agents={agents}"
        )


def read_out():
    values = {}
    if os.path.exists(OUT):
        with open(OUT) as f:
            for line in f:
                m = re.match(r"^([A-Z0-9_]+)=(.*)$", line.strip())
                if m:
                    values[m.group(1)] = m.group(2)
    return values


def write_out(values):
    lines = [
        "# Written by e2e/livekit-cloud/run.sh setup. Git-ignored. Holds the e2e",
        "# trunk's SIP digest password: do not commit or share it.",
    ] + [f"{k}={v}" for k, v in values.items()]
    tmp = OUT + ".tmp"
    with open(tmp, "w") as f:
        f.write("\n".join(lines) + "\n")
    os.chmod(tmp, 0o600)
    os.replace(tmp, OUT)


def new_password():
    alphabet = string.ascii_letters + string.digits
    return "".join(secrets.choice(alphabet) for _ in range(24))


def callee_room_config():
    return api.RoomConfiguration(agents=[api.RoomAgentDispatch(agent_name=CALLEE_AGENT)])


async def ensure_rule(lk_api, name, trunk_ids):
    """A dispatch rule that gives each call a room of its own with the
    callee agent. An out-of-date rule is replaced. A number routes to a rule
    as its id (PN_...) in the rule's trunk_ids."""
    rule = await find_rule(lk_api, name)
    agents = [a.agent_name for a in rule.room_config.agents] if rule else []
    trunks = [t for t in rule.trunk_ids if not t.startswith("PN_")] if rule else []
    if rule is not None and (trunks != trunk_ids or agents != [CALLEE_AGENT]):
        await lk_api.sip.delete_dispatch_rule(
            api.DeleteSIPDispatchRuleRequest(sip_dispatch_rule_id=rule.sip_dispatch_rule_id)
        )
        print(f"dispatch rule {name!r}: deleted {mask(rule.sip_dispatch_rule_id)} (out of date)")
        rule = None
    if rule is None:
        rule = await lk_api.sip.create_dispatch_rule(
            api.CreateSIPDispatchRuleRequest(
                dispatch_rule=api.SIPDispatchRuleInfo(
                    name=name,
                    trunk_ids=trunk_ids,
                    rule=api.SIPDispatchRule(
                        dispatch_rule_individual=api.SIPDispatchRuleIndividual(room_prefix=f"{name}-")
                    ),
                    room_config=callee_room_config(),
                )
            )
        )
        print(f"dispatch rule {name!r}: created {mask(rule.sip_dispatch_rule_id)}")
    else:
        print(f"dispatch rule {name!r}: reused {mask(rule.sip_dispatch_rule_id)}")
    return rule


def ensure_number(rule_id):
    """The project's first active number, else the first US number for rent
    (each plan includes one free US number). It routes to rule_id."""
    owned = [n for n in numbers() if n.get("status") in ("PHONE_NUMBER_STATUS_ACTIVE", None)]
    if owned:
        number = owned[0]["e164Format"]
        print(f"number: reused {number}")
    else:
        found = lk("number", "search", "--country-code", "US", "--limit", "1").get("items", [])
        if not found:
            raise ToolError("LiveKit has no US number for rent now")
        number = found[0]["e164Format"]
        lk("number", "purchase", "--numbers", number)
        print(f"number: rented {number}")
    if not owned or rule_id not in owned[0].get("sipDispatchRuleIds", []):
        lk("number", "update", "--number", number, "--sip-dispatch-rule-id", rule_id)
        print(f"number: routes to dispatch rule {mask(rule_id)}")
    return number


async def ensure_outbound(lk_api, host, number, password):
    """LiveKit numbers are inbound only, so the outbound trunk points at the
    project's own SIP URI, with the inbound trunk's credentials."""
    out = await find_outbound(lk_api)
    if out is not None and (out.address != host or list(out.numbers) != [number] or out.auth_username != SIP_USER):
        await lk_api.sip.delete_trunk(api.DeleteSIPTrunkRequest(sip_trunk_id=out.sip_trunk_id))
        print(f"outbound trunk: deleted {mask(out.sip_trunk_id)} (out of date)")
        out = None
    if out is None:
        out = await lk_api.sip.create_outbound_trunk(
            api.CreateSIPOutboundTrunkRequest(
                trunk=api.SIPOutboundTrunkInfo(
                    name=OUTBOUND,
                    address=host,
                    numbers=[number],
                    auth_username=SIP_USER,
                    auth_password=password,
                )
            )
        )
        print(f"outbound trunk: created {mask(out.sip_trunk_id)} -> {host}")
    else:
        # LiveKit does not return the password: set it, so it matches.
        await lk_api.sip.update_outbound_trunk_fields(out.sip_trunk_id, auth_password=password)
        print(f"outbound trunk: reused {mask(out.sip_trunk_id)} -> {host}")
    return out


async def setup(lk_api):
    await whoami(lk_api)
    host = sip_host()
    if host is None:
        raise ToolError(NO_SIP_URI)
    saved = read_out()
    password = saved.get("AD_SIP_TRUNK_PASSWORD")

    trunk = await find_trunk(lk_api)
    if trunk is None:
        password = password or new_password()
        # No numbers: the trunk takes any number, so it must authenticate.
        trunk = await lk_api.sip.create_inbound_trunk(
            api.CreateSIPInboundTrunkRequest(
                trunk=api.SIPInboundTrunkInfo(
                    name=NAME, numbers=[], auth_username=SIP_USER, auth_password=password
                )
            )
        )
        print(f"inbound trunk: created {mask(trunk.sip_trunk_id)}")
    elif not password:
        # LiveKit does not return the password, so a lost one means a new one.
        password = new_password()
        trunk = await lk_api.sip.update_inbound_trunk_fields(
            trunk.sip_trunk_id, auth_username=SIP_USER, auth_password=password
        )
        print(f"inbound trunk: reused {mask(trunk.sip_trunk_id)}, new password")
    else:
        print(f"inbound trunk: reused {mask(trunk.sip_trunk_id)}")

    rule = await ensure_rule(lk_api, NAME, [trunk.sip_trunk_id])
    number = ensure_number(rule.sip_dispatch_rule_id)
    out = await ensure_outbound(lk_api, host, number, password)

    write_out(
        {
            "LKCLOUD_TRUNK_ID": trunk.sip_trunk_id,
            "LKCLOUD_RULE_ID": rule.sip_dispatch_rule_id,
            "LKCLOUD_OUTBOUND_TRUNK_ID": out.sip_trunk_id,
            "LKCLOUD_CALLEE_AGENT": CALLEE_AGENT,
            "LKCLOUD_NUMBER": number,
            "LKCLOUD_TO": number,
            "AD_SIP_TRUNK": host,
            "AD_SIP_TRUNK_USER": SIP_USER,
            "AD_SIP_TRUNK_PASSWORD": password,
            "AD_SIP_CALLER_ID": CALLER_ID,
        }
    )
    print(f"wrote {os.path.basename(OUT)} (git-ignored)")


async def call(lk_api, hold_s="15"):
    """LiveKit's SIP client calls the number. The callee agent joins the
    calling room too, so each side has a tone to hear."""
    saved = read_out()
    trunk_id, number = saved.get("LKCLOUD_OUTBOUND_TRUNK_ID"), saved.get("LKCLOUD_NUMBER")
    if not (trunk_id and number):
        raise ToolError("run setup first")
    room = f"{NAME}-lkout-{secrets.token_hex(3)}"
    await lk_api.room.create_room(api.CreateRoomRequest(name=room, empty_timeout=60))
    await lk_api.agent_dispatch.create_dispatch(
        api.CreateAgentDispatchRequest(agent_name=CALLEE_AGENT, room=room)
    )
    try:
        p = await lk_api.sip.create_sip_participant(
            api.CreateSIPParticipantRequest(
                sip_trunk_id=trunk_id,
                sip_call_to=number,
                room_name=room,
                participant_identity="lk-outbound",
                wait_until_answered=True,
            ),
            timeout=60,
        )
        print(json.dumps({"room": room, "identity": p.participant_identity, "sip_call_id": p.sip_call_id, "answered": True}))
        sys.stdout.flush()
        await asyncio.sleep(float(hold_s))
    finally:
        # Closing the room hangs up the SIP leg (BYE).
        await lk_api.room.delete_room(api.DeleteRoomRequest(room=room))
    print(json.dumps({"room": room, "ended": True}))


async def teardown(lk_api):
    rule = await find_rule(lk_api, NAME)
    if rule:
        await lk_api.sip.delete_dispatch_rule(
            api.DeleteSIPDispatchRuleRequest(sip_dispatch_rule_id=rule.sip_dispatch_rule_id)
        )
        print(f"dispatch rule {NAME!r}: deleted {mask(rule.sip_dispatch_rule_id)}")
    for find in (find_outbound, find_trunk):
        trunk = await find(lk_api)
        if trunk:
            await lk_api.sip.delete_trunk(api.DeleteSIPTrunkRequest(sip_trunk_id=trunk.sip_trunk_id))
            print(f"trunk {trunk.name!r}: deleted {mask(trunk.sip_trunk_id)}")
    for n in numbers():
        print(f"number: {n.get('e164Format')} stays (lk number release --numbers {n.get('e164Format')})")
    if os.path.exists(OUT):
        os.remove(OUT)
        print(f"removed {os.path.basename(OUT)}")


COMMANDS = {"whoami": whoami, "inspect": inspect, "setup": setup, "call": call, "teardown": teardown}


async def run(cmd, args):
    url, key, secret = project()
    lk_api = api.LiveKitAPI(url, key, secret)
    try:
        await COMMANDS[cmd](lk_api, *args)
    finally:
        await lk_api.aclose()


def main(argv):
    cmd = argv[1] if len(argv) > 1 else "whoami"
    if cmd not in COMMANDS:
        print(__doc__)
        return 2
    try:
        asyncio.run(run(cmd, argv[2:]))
    except ToolError as e:
        print(f"lkcloud: {e}", file=sys.stderr)
        return 1
    except api.SipCallError as e:
        print(f"lkcloud: the call failed: {e}", file=sys.stderr)
        return 1
    except api.TwirpError as e:
        print(f"lkcloud: LiveKit refused the request: {e.code}: {e.message}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
