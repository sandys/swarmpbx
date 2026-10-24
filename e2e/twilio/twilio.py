"""Twilio Elastic SIP Trunking for the agentdialer e2e tests.

Run it through e2e/twilio/run.sh, which runs it in a Python container with
the repository's .env (TWILIO_SID, TWILIO_SECRET, and optionally
TWILIO_ACCOUNT_SID). It uses only the standard library.

Commands:
  whoami    the key type and the account behind the key
  inspect   the account's numbers, verified caller IDs, and trunks (read only)
  setup     create or reuse the e2e trunk, its credential list and credential,
            and write .env.twilio (git-ignored) for the dialer
  calls     Twilio's record of the e2e calls to a number since a time
  teardown  delete the e2e trunk and credential list, and .env.twilio

It never prints a secret. SIDs and phone numbers are shown masked.
"""

import base64
import json
import os
import re
import secrets
import string
import sys
import urllib.error
import urllib.parse
import urllib.request

API = "https://api.twilio.com/2010-04-01"
TRUNKING = "https://trunking.twilio.com/v1"
NAME = "agentdialer-e2e"
SIP_USER = "agentdialer"
OUT = os.environ.get("TWILIO_E2E_OUT", "/repo/.env.twilio")


class TwilioError(Exception):
    pass


def mask(value):
    """Show the kind and the last four characters only."""
    if not value:
        return "-"
    return f"{value[:2]}…{value[-4:]}" if len(value) > 8 else "…"


def credentials():
    """TWILIO_SID and TWILIO_SECRET, or with TWILIO_CREDS=2, TWILIO_SID2 and
    TWILIO_SECRET2 (for example an API key in one pair and the account SID
    with the auth token in the other)."""
    n = os.environ.get("TWILIO_CREDS", "").strip()
    sid = os.environ.get(f"TWILIO_SID{n}", "").strip().strip("'\"")
    secret = os.environ.get(f"TWILIO_SECRET{n}", "").strip().strip("'\"")
    if not sid or not secret:
        raise TwilioError(f"TWILIO_SID{n} and TWILIO_SECRET{n} must be set in .env")
    return sid, secret


def call(method, url, form=None, ok=(200, 201, 204)):
    sid, secret = credentials()
    data = urllib.parse.urlencode(form).encode() if form is not None else None
    req = urllib.request.Request(url, data=data, method=method)
    token = base64.b64encode(f"{sid}:{secret}".encode()).decode()
    req.add_header("Authorization", f"Basic {token}")
    req.add_header("Accept", "application/json")
    if data is not None:
        req.add_header("Content-Type", "application/x-www-form-urlencoded")
    try:
        with urllib.request.urlopen(req, timeout=20) as resp:
            body = resp.read().decode()
            return json.loads(body) if body.strip() else {}
    except urllib.error.HTTPError as e:
        if e.code in ok:
            return {}
        detail = e.read().decode(errors="replace")
        try:
            detail = json.loads(detail).get("message", detail)
        except ValueError:
            pass
        raise TwilioError(f"{method} {url.split('?')[0]}: HTTP {e.code}: {detail}") from None


def pages(url, key):
    """Every item of a list endpoint, across pages."""
    items = []
    while url:
        page = call("GET", url)
        items.extend(page.get(key, []))
        nxt = page.get("next_page_uri") or (page.get("meta") or {}).get("next_page_url")
        if nxt and nxt.startswith("/"):
            nxt = "https://api.twilio.com" + nxt
        url = nxt
    return items


def account_sid():
    """The account behind the key. A Standard API key cannot read the
    Accounts resource, so try the other ways too."""
    explicit = os.environ.get("TWILIO_ACCOUNT_SID", "").strip().strip("'\"")
    if explicit:
        return explicit, "TWILIO_ACCOUNT_SID in .env"
    sid, _ = credentials()
    if sid.startswith("AC"):
        return sid, "TWILIO_SID is the account SID"
    try:
        accounts = call("GET", f"{API}/Accounts.json").get("accounts", [])
        if accounts:
            return accounts[0]["sid"], "the Accounts resource (a Main key)"
    except TwilioError:
        pass
    try:
        for trunk in pages(f"{TRUNKING}/Trunks", "trunks"):
            if trunk.get("account_sid"):
                return trunk["account_sid"], "an existing trunk"
    except TwilioError:
        pass
    raise TwilioError(
        "cannot find the account SID with this API key (a Standard key cannot "
        "read the Accounts resource). Add TWILIO_ACCOUNT_SID=AC... to .env "
        "(Twilio Console, Account Info)."
    )


TRIAL = (
    "A Trial account can have one trunk and one number (Twilio Help Center). "
    "It can call only verified numbers, and the caller ID must be the "
    "account's number or a verified caller ID."
)
TRIAL_REFUSED = (
    "Twilio refused Elastic SIP Trunking for this Trial account. If you used "
    "an API key, try the account SID and the auth token (TWILIO_SID2 and "
    "TWILIO_SECRET2 in .env, then TWILIO_CREDS=2). If Twilio refuses both, "
    "upgrade the account (Twilio Console, Billing > Upgrade)."
)


def trunks_or_explain(kind):
    try:
        return pages(f"{TRUNKING}/Trunks", "trunks")
    except TwilioError as e:
        if kind == "Trial" and "HTTP 401" in str(e):
            raise TwilioError(f"{e}\n{TRIAL_REFUSED}") from None
        raise


def whoami():
    sid, _ = credentials()
    kind = {"SK": "API key", "AC": "account SID + auth token"}.get(sid[:2], "unknown")
    acct, how = account_sid()
    print(f"key: {mask(sid)} ({kind})")
    print(f"account: {mask(acct)} (from {how})")
    kind = "unknown"
    try:
        info = call("GET", f"{API}/Accounts/{acct}.json")
        kind = info.get("type") or "unknown"
        print(f"account type: {kind}, status: {info.get('status')}")
    except TwilioError as e:
        print(f"account type: unknown ({e})")
    return acct, kind


def numbers(acct):
    return [
        n["phone_number"]
        for n in pages(f"{API}/Accounts/{acct}/IncomingPhoneNumbers.json?PageSize=100", "incoming_phone_numbers")
    ]


def verified(acct):
    """The account's verified caller IDs (Outgoing Caller IDs)."""
    return [
        n["phone_number"]
        for n in pages(f"{API}/Accounts/{acct}/OutgoingCallerIds.json?PageSize=100", "outgoing_caller_ids")
    ]


def find_trunk(kind="unknown"):
    for t in trunks_or_explain(kind):
        if t.get("friendly_name") == NAME:
            return t
    return None


def find_credential_list(acct):
    for cl in pages(f"{API}/Accounts/{acct}/SIP/CredentialLists.json", "credential_lists"):
        if cl.get("friendly_name") == NAME:
            return cl
    return None


def inspect():
    acct, kind = whoami()
    nums = numbers(acct)
    print(f"phone numbers: {len(nums)} ({', '.join(mask(n) for n in nums) or 'none'})")
    ids = verified(acct)
    print(f"verified caller IDs: {len(ids)} ({', '.join(mask(n) for n in ids) or 'none'})")
    if kind == "Trial":
        print(f"note: {TRIAL}")
    try:
        trunks = trunks_or_explain(kind)
    except TwilioError as e:
        print(f"trunks: refused: {e}")
        return
    print(f"trunks: {len(trunks)}")
    for t in trunks:
        lists = pages(f"{TRUNKING}/Trunks/{t['sid']}/CredentialLists", "credential_lists")
        print(
            f"  {t.get('friendly_name')!r} {t.get('domain_name')} "
            f"symmetric_rtp={t.get('symmetric_rtp_enabled')} secure={t.get('secure')} "
            f"credential_lists={len(lists)}"
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
        "# Written by e2e/twilio/run.sh setup. Git-ignored. Holds the e2e trunk's",
        "# SIP digest password: do not commit or share it.",
    ] + [f"{k}={v}" for k, v in values.items()]
    tmp = OUT + ".tmp"
    with open(tmp, "w") as f:
        f.write("\n".join(lines) + "\n")
    os.chmod(tmp, 0o600)
    os.replace(tmp, OUT)


def new_password():
    """At least 12 characters, a digit, and mixed case (Twilio's rule)."""
    alphabet = string.ascii_letters + string.digits
    while True:
        p = "".join(secrets.choice(alphabet) for _ in range(24))
        if any(c.isdigit() for c in p) and any(c.islower() for c in p) and any(c.isupper() for c in p):
            return p


def setup():
    acct, kind = whoami()
    if kind == "Trial":
        print(f"note: {TRIAL}")
    saved = read_out()

    trunk = find_trunk(kind)
    if trunk is None:
        domain = f"agentdialer-{acct[-8:].lower()}.pstn.twilio.com"
        trunk = call("POST", f"{TRUNKING}/Trunks", {"FriendlyName": NAME, "DomainName": domain})
        print(f"trunk: created {mask(trunk['sid'])} {trunk['domain_name']}")
    else:
        print(f"trunk: reused {mask(trunk['sid'])} {trunk['domain_name']}")

    # The dialer sits behind NAT (PLAN.md section 0.0, the latching media
    # mode): Twilio must send RTP to the address it sees, not to the SDP.
    if not trunk.get("symmetric_rtp_enabled"):
        try:
            call("POST", f"{TRUNKING}/Trunks/{trunk['sid']}", {"SymmetricRtpEnabled": "true"})
        except TwilioError as e:
            print(f"symmetric RTP: the API did not take the change ({e})")
        trunk = call("GET", f"{TRUNKING}/Trunks/{trunk['sid']}")
    symmetric = bool(trunk.get("symmetric_rtp_enabled"))
    print(f"symmetric RTP: {'on' if symmetric else 'OFF'}")
    if not symmetric:
        print(
            "  ACTION: turn on 'Symmetric RTP' for the trunk "
            f"{NAME!r} in the Twilio Console (Elastic SIP Trunking > Trunks > "
            f"{NAME} > General). Without it, Twilio sends audio to the private "
            "address in the dialer's SDP, and the call has no inbound audio."
        )

    cl = find_credential_list(acct)
    if cl is None:
        cl = call("POST", f"{API}/Accounts/{acct}/SIP/CredentialLists.json", {"FriendlyName": NAME})
        print(f"credential list: created {mask(cl['sid'])}")
    else:
        print(f"credential list: reused {mask(cl['sid'])}")
    creds = pages(f"{API}/Accounts/{acct}/SIP/CredentialLists/{cl['sid']}/Credentials.json", "credentials")
    mine = next((c for c in creds if c.get("username") == SIP_USER), None)
    password = saved.get("AD_SIP_TRUNK_PASSWORD")
    if mine is None or not password:
        # Twilio never returns a password, so a lost one means a new one.
        password = new_password()
        if mine is None:
            call(
                "POST",
                f"{API}/Accounts/{acct}/SIP/CredentialLists/{cl['sid']}/Credentials.json",
                {"Username": SIP_USER, "Password": password},
            )
            print(f"credential: created user {SIP_USER!r}")
        else:
            call(
                "POST",
                f"{API}/Accounts/{acct}/SIP/CredentialLists/{cl['sid']}/Credentials/{mine['sid']}.json",
                {"Password": password},
            )
            print(f"credential: new password for user {SIP_USER!r}")
    else:
        print(f"credential: reused user {SIP_USER!r}")

    linked = pages(f"{TRUNKING}/Trunks/{trunk['sid']}/CredentialLists", "credential_lists")
    if not any(x.get("sid") == cl["sid"] for x in linked):
        call("POST", f"{TRUNKING}/Trunks/{trunk['sid']}/CredentialLists", {"CredentialListSid": cl["sid"]})
        print("credential list: linked to the trunk")

    # Twilio takes a number of the account or a verified caller ID.
    nums = numbers(acct) or verified(acct)
    caller = os.environ.get("TWILIO_CALLER_ID", "").strip() or (nums[0] if nums else "")
    print(f"caller ID: {mask(caller) if caller else 'NONE (no Twilio number and no verified caller ID)'}")

    write_out(
        {
            "TWILIO_ACCOUNT_SID": acct,
            "TWILIO_ACCOUNT_TYPE": kind,
            "TWILIO_TRUNK_SID": trunk["sid"],
            "TWILIO_SYMMETRIC_RTP": "on" if symmetric else "off",
            "AD_SIP_TRUNK": trunk["domain_name"],
            "AD_SIP_TRUNK_USER": SIP_USER,
            "AD_SIP_TRUNK_PASSWORD": password,
            "AD_SIP_CALLER_ID": caller,
        }
    )
    print(f"wrote {os.path.basename(OUT)} (git-ignored)")


def calls(to, since):
    acct, _ = account_sid()
    q = urllib.parse.urlencode({"To": to, "StartTime>": since, "PageSize": 20})
    found = pages(f"{API}/Accounts/{acct}/Calls.json?{q}", "calls")
    print(json.dumps(
        [
            {
                "sid": mask(c.get("sid")),
                "status": c.get("status"),
                "direction": c.get("direction"),
                "duration": c.get("duration"),
                "trunk": mask(c.get("trunk_sid")),
                "start_time": c.get("start_time"),
            }
            for c in found
        ]
    ))


def teardown():
    acct, _ = account_sid()
    trunk = find_trunk()
    if trunk:
        call("DELETE", f"{TRUNKING}/Trunks/{trunk['sid']}")
        print(f"trunk: deleted {mask(trunk['sid'])}")
    cl = find_credential_list(acct)
    if cl:
        call("DELETE", f"{API}/Accounts/{acct}/SIP/CredentialLists/{cl['sid']}.json")
        print(f"credential list: deleted {mask(cl['sid'])}")
    if os.path.exists(OUT):
        os.remove(OUT)
        print(f"removed {os.path.basename(OUT)}")


def main(argv):
    cmd = argv[1] if len(argv) > 1 else "whoami"
    try:
        if cmd == "whoami":
            whoami()
        elif cmd == "inspect":
            inspect()
        elif cmd == "setup":
            setup()
        elif cmd == "calls":
            calls(argv[2], argv[3])
        elif cmd == "teardown":
            teardown()
        else:
            print(__doc__)
            return 2
    except TwilioError as e:
        print(f"twilio: {e}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
