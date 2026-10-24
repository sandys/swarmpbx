# AgentDialer

AgentDialer is a SIP-only, AI-agent-native, Kubernetes-native outbound dialer.
It is written in Rust. AI agents (LiveKit and pipecat) replace human agents. The
campaign and compliance "brain" is the product. Each dialer node carries its
own calls (SIP and RTP) with vendored libraries, behind Kamailio.

The primary target is LiveKit Cloud SIP. The quickstart below connects the
dialer to LiveKit Cloud SIP and a LiveKit phone number, and places calls.
Twilio Elastic SIP Trunking is a second supported carrier ("Call through
Twilio").

`PLAN.md` holds the authoritative architecture. `.agents/REPO_GUIDE.md` holds
the working conventions. Read both before you change code.

## What it does

AgentDialer runs outbound dial campaigns:

- It selects the due leads and paces calls per campaign, within a concurrency
  cap.
- It gates every call on compliance (safe-hours, DNC). Compliance fails closed.
- It originates each call into a LiveKit room where an AI agent answers.
- It records outcomes, trips a circuit breaker on failure, and schedules
  retries.

Parts of this are not complete yet. See "Known limits" under "Status".

It speaks SIP only. It reaches phone numbers through a SIP trunk. The AI agent
is the "human".

## Quickstart: place calls through LiveKit Cloud SIP

These steps stand up the whole stack in Docker Compose, connect it to LiveKit
Cloud SIP with a phone number, and place calls. This is the primary supported
use, and it must never break: `e2e/run_lkcloud_e2e.sh` runs the same steps
against LiveKit Cloud. The host needs only bash, git, and the Docker CLI.

```
 your agent ⇄ room (the stack's LiveKit) ⇄ WebRTC ⇄ agentdialer node
 node ⇄ SIP ⇄ Kamailio ⇄ SIP (internet) ⇄ LiveKit Cloud SIP ⇄ room ⇄ callee agent
 node ⇄ RTP (internet) ⇄ LiveKit Cloud SIP
```

A call goes like this:

1. The node dials your LiveKit phone number at your project's SIP URI, as it
   dials a carrier. LiveKit Cloud is the trunk.
2. LiveKit Cloud asks for the trunk password (a 407 digest challenge). The
   node answers with the credentials that setup made.
3. A dispatch rule puts the call into a room of its own in LiveKit Cloud. A
   callee agent joins that room and answers.
4. The audio goes straight between the node and LiveKit Cloud. The stack's own
   LiveKit server holds the dialer's side of the call, where your agent talks.

LiveKit phone numbers take incoming calls only. So these calls stay in
LiveKit Cloud: they do not go through the public phone network, and no phone
rings. To call a real phone, use a carrier trunk ("Call through Twilio").

### What you need

1. A LiveKit Cloud project. Each LiveKit Cloud plan includes one free US phone
   number.
2. An API key and its secret (LiveKit Cloud, Settings, API keys).
3. The project's SIP URI (LiveKit Cloud, Project Settings). The project URL
   does not give it.
4. A `.env` file at the repository root. It is git-ignored:

   ```
   LIVEKIT_URL=wss://<project>.livekit.cloud
   LIVEKIT_API_KEY=...
   LIVEKIT_API_SECRET=...
   LIVEKIT_SIP_URI=sip:<id>.sip.livekit.cloud
   ```

### 1. Connect LiveKit Cloud SIP

```
e2e/livekit-cloud/run.sh setup
```

The script runs in a container, with LiveKit's SDK and the `lk` CLI. It never
prints a secret. It does these steps, and it reuses each part that exists:

- It rents a LiveKit phone number, or it reuses the number of the project. It
  prints the number.
- It creates the inbound trunk `agentdialer-e2e`, with the user `agentdialer`
  and a generated digest password. The dialer uses this trunk.
- It creates the dispatch rule `agentdialer-e2e`. The rule serves the trunk and
  the number. It gives each call a room of its own, and it sends the callee
  agent `agentdialer-e2e-callee` into the room.
- It creates the outbound trunk `agentdialer-e2e-outbound` for a reference call
  (step 4). The trunk points at the project's own SIP URI, because LiveKit
  numbers take incoming calls only.
- It writes `.env.lkcloud` (git-ignored) for the stack: the SIP URI host as
  `AD_SIP_TRUNK`, the trunk user and password, the caller ID, and the number.

You can run it again at any time. `e2e/livekit-cloud/run.sh inspect` shows the
number, the trunks, and the rule.

### 2. Start the stack

```
QS="docker compose -f docker-compose.quickstart.yml -f docker-compose.livekit-cloud.yml --env-file .env --env-file .env.lkcloud"
$QS up -d --build
```

The stack is Postgres, kine, LiveKit (the dialer's rooms), Kamailio (the SIP
edge), the migrate job, the API (on `127.0.0.1:8080`), one dialer node, a demo
agent, and the callee agent.
- The demo agent joins each call on the dialer's side. It plays a tone and
  leaves after `DEMO_AGENT_MAX_CALL_S` (default 30 s). The node then ends the
  call.
- The callee agent connects to LiveKit Cloud and joins each call's room there.
  It also plays a tone.

### 3. Place calls

`scripts/api.sh` calls the API from a container on the stack's network. Put
your LiveKit number in the lead:

```
NUMBER=$(sed -n 's/^LKCLOUD_NUMBER=//p' .env.lkcloud)
scripts/api.sh POST /v1/campaigns \
  '{"org_id": 1, "name": "first-calls", "concurrency_cap": 1,
    "agent": {"runtime": "livekit", "agent_name": "demo"}}'
# -> {"id":1, ... "state":"draft"}
scripts/api.sh POST /v1/campaigns/1/leads \
  "{\"leads\": [{\"phone_number\": \"$NUMBER\", \"timezone\": \"America/New_York\"}]}"
scripts/api.sh POST /v1/campaigns/1/transition '{"transition": "start"}'
scripts/api.sh POST /v1/campaigns/1/transition '{"transition": "sync_complete"}'
```

The campaign starts to dial when it is `running`.

- Write each phone number in E.164 (`+` and the country code).
- Give each lead an IANA timezone. A lead without one never dials, because
  compliance fails closed.

### 4. Watch

```
$QS logs -f node
scripts/api.sh GET /v1/campaigns/1
e2e/livekit-cloud/run.sh call
```

- The LiveKit Cloud dashboard shows a room for each call. The room name starts
  with `agentdialer-e2e-`.
- `e2e/livekit-cloud/run.sh call` is a reference call. LiveKit's own SIP
  client calls your number through the outbound trunk, and the callee agent
  joins. Use it to check the LiveKit Cloud side alone. The stack must be up,
  because the callee agent runs in it.

### 5. Stop

```
$QS down -v
e2e/livekit-cloud/run.sh teardown   # optional: delete the trunks and the rule
```

Teardown keeps the number. If you do not need the number, release it in the
LiveKit Cloud dashboard.

### Your own agents

- The dialer's side: put your agent in the campaign:
  `{"runtime": "livekit", "agent_name": "<name>"}` for a LiveKit agent, or
  `{"runtime": "pipecat", "start_url": "<url>"}` for a pipecat bot. The agent
  must reach the stack's LiveKit server: run it on the stack's network
  (`agentdialer-quickstart_default`) with `LIVEKIT_URL=ws://livekit:7880`.
- The LiveKit Cloud side: the dispatch rule sends the agent
  `agentdialer-e2e-callee` into each call's room. To answer with your own
  agent, register it in LiveKit Cloud with that agent name, and start the
  stack without `-f docker-compose.livekit-cloud.yml`.

### If a call fails

| Symptom | Cause |
|---|---|
| setup: `LIVEKIT_SIP_URI is not set` | Copy the SIP URI from LiveKit Cloud, Project Settings, into `.env`. |
| setup: `LiveKit refused the request` | LiveKit Cloud refused the key, the secret, or the request. The message gives LiveKit's reason. |
| setup: `LiveKit has no US number for rent now` | LiveKit has no free number at this time. Try again later. |
| The campaign does not dial | The campaign is not `running`, or the lead has no timezone. |
| The call connects, but the demo agent hears nothing | The callee agent did not join the room in LiveKit Cloud. Read `$QS logs callee`. |

## Call through Twilio

Twilio Elastic SIP Trunking is a second supported carrier, and it reaches real
phones. The quickstart stack places the calls; only the trunk changes.
`e2e/run_twilio_e2e.sh` runs these steps against a real trunk, and
`e2e/run_twilio_e2e.sh --sim` runs them on every change with the carrier
simulator in Twilio's place (CI).

### What you need

1. A Twilio account where Elastic SIP Trunking is available. An upgraded
   account has it. Twilio's documents do not agree about Trial accounts. On
   2026-10-22, Twilio's API refused trunks for a Trial account with HTTP 401.
2. A caller ID: a Twilio number of the account, or a verified caller ID
   (Twilio Console, Phone Numbers, Verified Caller IDs). Twilio requires one
   of them in From.
   - CAUTION: On a Trial account, Twilio calls verified numbers only. Put a
     verified number in the lead. That phone rings.
3. An API key (Twilio Console, Account, API keys & tokens). A Main key works
   alone. A Standard key also needs the account SID.
4. A `.env` file at the repository root. It is git-ignored:

   ```
   TWILIO_SID=SK...
   TWILIO_SECRET=...
   # With a Standard key:   TWILIO_ACCOUNT_SID=AC...
   # A second pair, used with TWILIO_CREDS=2 (for example the account SID
   # and the auth token):   TWILIO_SID2=AC...  TWILIO_SECRET2=...
   # To choose the caller ID (default: the first number of the account,
   # else the first verified caller ID):
   # TWILIO_CALLER_ID=+15551234567
   ```

### 1. Set up the trunk

```
e2e/twilio/run.sh setup
```

The script runs in a container. It never prints a secret. It creates the trunk
`agentdialer-e2e` (termination URI `agentdialer-<id>.pstn.twilio.com`), or it
reuses the trunk when it exists. It also does these steps:

- It creates a credential list with a generated digest password.
- It turns on Symmetric RTP.
- It selects the caller ID.
- It writes `.env.twilio` (git-ignored) for the stack.

You can run it again at any time. `e2e/twilio/run.sh inspect` shows the
account, its numbers, its verified caller IDs, and its trunks.

If setup prints `ACTION` for Symmetric RTP, turn on "Symmetric RTP" for the
trunk in the Twilio Console. The node is behind NAT: its SDP names a private
address, so Twilio must send the audio to the address that the audio comes
from.

### 2. Start the stack

```
docker compose -f docker-compose.quickstart.yml --env-file .env.twilio up -d --build
```

This is the quickstart stack without the callee agent.

### 3. Place calls

Do step 3 of the quickstart, but set `NUMBER` to the number to call, for
example `NUMBER=+16504894546`. That is Twilio's "Play" test number. Calls to
it are free, and you can reach it only through a trunk.

### 4. Watch

```
docker compose -f docker-compose.quickstart.yml logs -f node
scripts/api.sh GET /v1/campaigns/1
e2e/twilio/run.sh calls +16504894546 2026-10-21T00:00:00Z   # Twilio's record
```

### 5. Stop

```
docker compose -f docker-compose.quickstart.yml down -v
e2e/twilio/run.sh teardown   # optional: delete the trunk and its credentials
```

### If a call fails

| Symptom | Cause |
|---|---|
| setup: "not available on a Trial account" | Twilio refuses trunks for the account. With an API key, try the account SID and the auth token: put them in `.env` as `TWILIO_SID2` and `TWILIO_SECRET2`, then run `TWILIO_CREDS=2 e2e/twilio/run.sh setup`. If Twilio refuses both, upgrade the account. |
| setup: no caller ID | Get a Twilio number, or verify a caller ID in the Twilio Console. You can also set `TWILIO_CALLER_ID` in `.env`. |
| A Trial account: outcome `failed` with SIP 403 (Twilio error 32100) | The destination or the caller ID is not verified. |
| outcome `failed` with SIP 403 | The trunk credentials or the caller ID. Run setup again. |
| outcome `failed` with SIP 400 | The number is not E.164. |
| The call connects, but the agent hears nothing | Symmetric RTP is off on the trunk. |
| The callee hangs up, but the call ends only about 30 s later | Behind NAT, Twilio's BYE may not reach Kamailio; the node's media timeout ends the call. Give Kamailio a public address to fix this. |

## Architecture

Two halves (see `PLAN.md` §0 and §0.10):

1. The media plane (`PLAN.md` §0.0, decision D-9). agentdialer carries each
   call itself, with vendored libraries:
   - SIP: `rsipstack` (`ad_sip`). RTP and G.711: `rustrtc` and `audio-codec`
     (`ad_media`). The bridge into the LiveKit room: our own WebRTC
     participant (`ad_gateway`).
   - Kamailio is the SIP edge: a SIP-aware load balancer that keeps itself on
     each call's path (`Record-Route`). It carries no audio.
   - Every part is a normal Kubernetes pod: no host networking, no RTP port
     range per call. The RTP goes straight between the trunk (LiveKit Cloud
     SIP or a carrier) and the node, through the NAT gateway (the trunk
     latches).

2. The campaign brain. The product:
   - campaigns, leads/lists, the hopper, pacing, dispositions, retries, DNC.
   - a FusionPBX-grade routing rule engine.
   - a Postgres event layer (an event table plus LISTEN/NOTIFY) that drives the
     orchestrator (no blind polling). There is no Redis in the product.

```
 trunk ⇄ SIP ⇄ Kamailio ⇄ SIP ⇄ agentdialer node (ad_sip)
 trunk ⇄ RTP (through the NAT gateway) ⇄ node (ad_media)
                          node ⇄ WebRTC ⇄ LiveKit room ⇄ LiveKit / pipecat agent
```

The orchestrator runs as a cluster (`PLAN.md` §0.10.8b). Every orchestrator
replica is a node, and the nodes coordinate through kine, the etcd API on
Postgres. One node holds the leader key and schedules: it claims due leads
straight from the `leads` table (`SELECT ... FOR UPDATE SKIP LOCKED`, within
the campaign's `concurrency_cap` and the free capacity of the cluster), and it
assigns each call to the node with the most free capacity, paced by
`AD_CALLS_PER_SEC`. Each node runs its calls: it starts the campaign's agent (a
LiveKit agent or a pipecat bot) in the call's room, waits until the agent is
there, and sends the INVITE through Kamailio. On the answer, the node joins the
room and carries the audio until the call ends: the callee's BYE, the agent
leaving the room, or no RTP for 30 s. The leader then updates the lead once (a
retry is stored on the lead row, `next_eligible_at`), closes the room, and
dials into the freed capacity. When a node dies, its live calls drop (it
carried their media); the leader moves their attempts, the reconcile ends
them, and none is dialed again. Postgres fences the old owner.
Postgres and kine are managed services to the product: it reaches them only by
URL and waits for them with backoff.

## Crates

One crate = one concern.

- `ad_core` — typed ids, errors, a `Clock` trait for deterministic time.
- `ad_compliance` — safe-hours gate, DNC, abandonment ledger. Pure, fails closed.
- `ad_campaign` — the campaign aggregate and its lifecycle state machine.
- `ad_lead` — the lead aggregate and the lead repository trait.
- `ad_routing` — a FusionPBX-grade routing rule engine (conditions, actions,
  anti-actions, groups, break semantics, `${}`/`$N` interpolation).
- `ad_hopper` — lead selection: status, call-count, DNC, safe-hours, routing.
- `ad_circuit_breaker` — a pure rolling-window failure-rate breaker.
- `ad_attempt` — the call-attempt statechart, with a customer leg and an agent
  region. Pure: it takes an attempt and an event and returns the next attempt.
  It maps SIP codes to outcome classes.
- `ad_agent` — the agent adapters: `livekit` (explicit dispatch by agent name)
  and `pipecat` (a start request with a room token). Both get the same call
  facts.
- `ad_cluster` — cluster coordination over kine: node membership, the leader,
  call assignment, and placement.
- `ad_dispatch` — the dial seam (`CallInitiator`, `Placement`) and the launch
  pacer. It also holds the older dial loop that the experimental
  `dispatch/run-batch` endpoint uses.
- `ad_sip` — the node's SIP user agent on the vendored `rsipstack`: one
  outbound call through the SIP edge, the ring timeout (CANCEL), and the end of
  the call.
- `ad_media` — one call's RTP leg on the vendored `rustrtc` and `audio-codec`:
  the SDP offer and answer, G.711 both ways, a 20 ms send clock that never
  stops, and the media timeout.
- `ad_gateway` — the call path: `SipCaller` places a call with `ad_sip` and
  `ad_media` and bridges its audio into the LiveKit room with our own WebRTC
  participant. It also does the room operations and the presence check of the
  reconcile.
- `ad_orchestrator` — the pure scheduling gate, the run loop, the event reactor
  (dispatch, or settle then dispatch), the pure retry decision, and the pure
  settlement (what an ended attempt does to its lead).
- `ad_events` — the event contract: typed campaign events, the publisher and
  consumer traits, the breaker-store trait, and in-memory fakes for tests.
- `ad_db` — the Postgres data layer (sqlx), migrations,
  the call attempts (the claim, the saved statechart, settlement), and the
  event layer: the `campaign_events` table with LISTEN/NOTIFY, and the
  circuit-breaker window over the ended attempts.
- `ad_api` — the HTTP API (axum), with a `router()` factory for in-process
  tests. It still has the LiveKit webhook endpoint (`POST
  /v1/webhooks/livekit`); the call path no longer needs it.

`vendor/` holds the vendored libraries, exact upstream copies at pinned
commits (`vendor/VENDORED.md`). The workspace excludes them, so the test tier
does not format or lint them.
- `ad_bin` — the single binary `agentdialer`. Roles by subcommand: `api`,
  `orchestrator`, `dispatch`, `migrate`. One image, many roles.

## Build and test (Docker-first)

Everything builds and runs in containers, locally and in CI, identically. Do not
assume a host toolchain.

Run the full test tier (fmt + clippy + the suite, including the Postgres
integration tests for the data layer and the event layer):

```
docker compose -f docker-compose.test.yml up --build \
    --abort-on-container-exit --exit-code-from tests
docker compose -f docker-compose.test.yml down
```

Always stop the stack after tests.

To run the tests of one crate, run one command in the `tests` service. Compose
starts Postgres first:

```
docker compose -f docker-compose.test.yml run --rm --build tests \
    cargo test -p ad_orchestrator
docker compose -f docker-compose.test.yml down
```

The `tests` service mounts the source read-only. To format the source, run
`cargo fmt` as your own user in the test image (the compose build makes
`agentdialer-tests`):

```
docker run --rm --user "$(id -u):$(id -g)" -e HOME=/tmp \
    -v "$PWD":/app -w /app agentdialer-tests:latest cargo fmt
```

Caution: do not add `--all`. It also formats path dependencies, and that
rewrites the vendored crates in `vendor/`, which must stay exact upstream
copies.

The host needs only bash, git, and the Docker CLI. Do not install Rust, Python,
`psql`, or other tools on the host.

## Configuration

Each role reads its settings from environment variables. The contract is in
`crates/ad_bin/src/config.rs`. At startup a role reports every missing or invalid
variable in one message and exits. A blank value counts as missing. A test in
`ad_bin` checks the Helm chart, the e2e scripts, and this section against the
contract, so they cannot drift.

### `orchestrator`

| Variable | Need | Default | Meaning |
|---|---|---|---|
| `DATABASE_URL` | required | — | Postgres URL. Postgres also carries the events, retries, and breaker window. |
| `LIVEKIT_URL` | required | — | LiveKit server URL. |
| `LIVEKIT_API_KEY` | required | — | LiveKit API key. |
| `LIVEKIT_API_SECRET` | required | — | LiveKit API secret. |
| `AD_SIP_TRUNK` | required | — | The carrier's `host:port`. The node calls `sip:<number>@<trunk>`. |
| `AD_SIP_PROXY` | optional | — | The SIP edge (Kamailio), `host:port`. Every INVITE goes through it, and Kamailio stays on the call path. Without it, the node sends straight to the trunk. |
| `AD_SIP_BIND` | optional | `0.0.0.0:5060` | The node's SIP socket. |
| `AD_SIP_ADVERTISE_IP` | optional | the local address of the route to the proxy (else to the trunk) | The address in Via, Contact, and the SDP: the pod IP. Helm sets it from `status.podIP`. |
| `AD_SIP_TRUNK_USER` | optional | — | The digest user for the trunk. Set it with `AD_SIP_TRUNK_PASSWORD`, or neither. |
| `AD_SIP_TRUNK_PASSWORD` | optional | — | The digest password for the trunk. |
| `AD_SIP_CALLER_ID` | optional | — | The caller ID in our From, in E.164 (`+15551234567`). Twilio requires a number of the account or a verified caller ID. Without it, From is `agentdialer`. |
| `AD_MEDIA_PUBLIC_IP` | optional | — | The `public` media mode: the SDP names this address (an Elastic IP of the pod) instead of the pod IP. |
| `AD_ETCD_ENDPOINTS` | required | — | kine (the etcd API on Postgres) endpoints, comma-separated. The nodes find each other, elect the leader, and receive their calls there. |
| `AD_ROOM_PREFIX` | optional | `campaign` | Room name prefix. The room of a call is `<prefix>-<attempt id>`. |
| `AD_BATCH_LIMIT` | optional | `20` | Leads claimed per claim. A claim takes no more than the free capacity of the campaign. |
| `AD_CALLS_PER_SEC` | optional | `10` | Call starts per second. Calls run at the same time; this limits how fast they start. |
| `AD_RETRY_SWEEP_MS` | optional | `1000` | Interval of the sweep that dials the leads that became due (a retry or a hold), updates leads from ended attempts, and completes idle campaigns. |
| `AD_EVENT_SWEEP_MS` | optional | `2000` | The longest wait before the event table is read again, also when a notification is lost. |
| `AD_ATTEMPT_CHECK_MS` | optional | `15000` | Interval of the check of live calls against LiveKit (is the call's participant still in its room). It ends a call whose node died, and it decides a dial whose result was lost. |
| `AD_NODE_CAPACITY` | optional | `50` | The most calls this node runs at one time. The leader gives each new call to the node with the most free capacity. |
| `AD_CLUSTER_TTL_S` | optional | `10` | The TTL of the node and leader keys in kine. When a node stops without a clean exit, its calls move after this time. A clean stop (SIGTERM) moves them at once. |
| `HOSTNAME` | optional | `orchestrator` | The node name: the node key in kine, and the node of each call attempt. Letters, digits, `-`, `_`, `.`. |
| `RUST_LOG` | optional | `info` | Log filter (`tracing` `EnvFilter` syntax). |

The retry policy is fixed in code for now (`crates/ad_orchestrator/src/retry.rs`).
A lead gets at most 3 attempts. The wait before the next attempt is 5 minutes
after busy, 30 minutes after no answer, 60 minutes after voicemail, and 15
minutes after a failure or an error. No variable changes the policy yet.

### `api`

| Variable | Need | Default | Meaning |
|---|---|---|---|
| `AD_API_ADDR` | optional | `0.0.0.0:8080` | Listen address. |
| `DATABASE_URL` | optional | in-memory store | Postgres URL. Migrations run at start. |
| `LIVEKIT_API_KEY` | optional | — | LiveKit API key. With `LIVEKIT_API_SECRET` and `DATABASE_URL`, it turns on the LiveKit webhook endpoint. Set both keys or neither. |
| `LIVEKIT_API_SECRET` | optional | — | LiveKit API secret. It verifies the signature of each LiveKit webhook. |
| `RUST_LOG` | optional | `info` | Log filter. |

### `migrate`

| Variable | Need | Default | Meaning |
|---|---|---|---|
| `DATABASE_URL` | required | — | Postgres URL. Postgres also carries the events, retries, and breaker window. |
| `RUST_LOG` | optional | `info` | Log filter. |

The `dispatch` role is a stub. It prints a message and exits. It has no
contract yet.

### Logs

Every role writes its logs to stdout and its errors to stderr. It writes no log
files. In a container, read them with `docker logs <container>`; in Kubernetes,
use `kubectl logs`. Set `RUST_LOG=debug` for more detail, or
`RUST_LOG=info,agentdialer=debug` for the binary only.

### Run locally (all in Docker)

```
docker compose up -d        # Postgres on 127.0.0.1:5433, kine on 127.0.0.1:2379
docker build -t agentdialer:dev .
DB=postgres://agentdialer:agentdialer@127.0.0.1:5433/agentdialer
docker run --rm --network host -e DATABASE_URL=$DB agentdialer:dev migrate
docker run --rm --network host \
    -e DATABASE_URL=$DB \
    -e LIVEKIT_URL=ws://127.0.0.1:7880 -e LIVEKIT_API_KEY=... -e LIVEKIT_API_SECRET=... \
    -e AD_SIP_TRUNK=carrier.example:5060 -e AD_SIP_PROXY=127.0.0.1:5060 \
    -e AD_ETCD_ENDPOINTS=http://127.0.0.1:2379 \
    agentdialer:dev orchestrator
docker compose down
```

For a complete stack (Kamailio, a NAT, the carrier simulator, LiveKit, and both
test agents), use `e2e/docker-compose.cluster.yml`, which `e2e/run_e2e.sh`
drives.

## End-to-end tests

The `e2e/` directory holds automated end-to-end tests. Each brings up a real
stack, asserts behavior, and tears down. The scripts need only bash and the
Docker CLI on the host: every other tool (HTTP polling, JSON, `psql`, `etcdctl`,
SIPp) runs in a container (`e2e/lib.sh`). Each script keeps the
orchestrator or probe log in `e2e/.out/` (git-ignored). The checks treat
Postgres and kine as managed services: they use the database URL and the kine
endpoint, never a shell in a container.

The carrier in the tests is `e2e/carrier-sim`, a SIP carrier simulator built
on diago (`github.com/emiago/diago`). It behaves like Twilio Elastic SIP
Trunking termination:
- a 407 digest challenge for each new INVITE;
- E.164 and caller-ID checks;
- symmetric RTP (it sends the audio back to the address it comes from);
- per-number behaviors: echo, tone, busy, decline, unavailable, failure,
  and no answer.

Its API (`/calls`) reports what it saw on each call, and the tests assert on
that API.

The cluster call tests run on `e2e/docker-compose.cluster.yml`: one bridge
network, no host networking, with Kamailio as the SIP edge. The carrier is on
the "internet" behind a NAT container that source-NATs with random ports, as
the AWS NAT gateway does.

- `e2e/run_lkcloud_e2e.sh` — the quickstart above, as a live test against
  LiveKit Cloud: setup, the quickstart stack, a campaign and a lead through
  the API, and a real call over the internet to the project's LiveKit number.
  It checks the digest auth, that each agent hears the other's tone, and that
  our BYE ends the call. It also places a reference call from LiveKit's
  outbound trunk. It needs the LiveKit Cloud keys and the SIP URI in `.env`.
- `e2e/run_twilio_e2e.sh` — "Call through Twilio", as a test: the quickstart
  stack, a campaign and a lead through the API, a real call through Twilio to
  its free "Play" number, Twilio's audio at the agent, and Twilio's own record
  of the call. It needs `.env` and an account where Elastic SIP Trunking is
  available. On a Trial account, set `TWILIO_E2E_TO` to a verified number.
  `--sim` runs the same stack and steps with the simulator in Twilio's place,
  with no credentials.
- `e2e/run_e2e.sh` — one node carries two calls itself: one with a real
  LiveKit agent (explicit dispatch) and one with a real pipecat bot (a start
  request). Each agent joins its call room before the dial.
  - The INVITE goes through Kamailio. The carrier challenges it with 407, and
    the node answers with the trunk credentials and the caller ID.
  - The agent's tone goes out as RTP through the NAT, comes back from the
    carrier, and the agent hears it.
  - The carrier's BYE comes back through Kamailio and ends the attempt. The
    leader settles each lead, closes the room, and completes both campaigns.
  - A third campaign checks that the carrier's busy, failure, and no-answer
    replies become the matching outcomes.
- `e2e/run_cluster_e2e.sh` — three nodes share 24 calls through kine, with both
  agent runtimes, the same way. The test kills a node and then the leader. It
  checks that the dead node's calls end without a second dial, that a new
  leader takes over, that the audio crossed both ways on every call that the
  carrier ended, and that every lead is dialed exactly once.
- `e2e/run_events_e2e.sh` — over a real Postgres, with a trunk that the node
  cannot reach (every dial fails before the INVITE leaves): dials
  follow each other through LISTEN/NOTIFY within the capacity, the breaker
  pauses the campaign, each retry is stored on the lead, a due retry is dialed
  again, and a resume through the API resets the breaker.
- `e2e/run_webrtc_e2e.sh` — our native WebRTC participant carries audio both
  ways into a LiveKit room with the agent, with no `livekit/sip`.
- `spike/livekit/run_mvp_test.sh` — a real LiveKit agent wired into a room as a
  subscriber, then a SIP call driven into it (two-way audio proven).

CI runs the `e2e/` scripts on every change (`.github/workflows/ci.yml`).
Among them, `run_twilio_e2e.sh --sim` runs the quickstart stack with the
carrier simulator, so the stack keeps working also without credentials. The
LiveKit Cloud quickstart test (`run_lkcloud_e2e.sh`) runs in CI when the
repository has the secrets `LIVEKIT_URL`, `LIVEKIT_API_KEY`,
`LIVEKIT_API_SECRET`, and `LIVEKIT_SIP_URI`. The live Twilio test runs in CI
when the repository has the secrets `TWILIO_SID` and `TWILIO_SECRET` (and, for
a Standard key, `TWILIO_ACCOUNT_SID`). CI does not run the spike script.

The test tier also runs `crates/ad_sip/tests/twilio.rs` on every change: our
SIP user agent against an in-process callee that answers like Twilio. It
checks the 407 digest, the caller ID in From, the E.164 Request-URI, and
`Max-Forwards: 70`, and that bad credentials end the call after one retry.

## Deployment

`deploy/helm/agentdialer` is the Helm chart. The orchestrator replicas are the
cluster nodes (no Service); each node carries its own calls as a normal pod (no
host networking). The API has a Service and an HPA. A migrate Job runs as a
pre-upgrade hook. Set `database`, `livekit`, and `sip` (`sip.trunk`, and
`sip.proxy` for Kamailio) in the values; the database and LiveKit values take
an existing Secret or an inline value for development. Each node advertises
its pod IP (`AD_SIP_ADVERTISE_IP` from `status.podIP`). The values
`orchestrator.retrySweepMs`, `orchestrator.eventSweepMs`, and
`orchestrator.attemptCheckMs` set `AD_RETRY_SWEEP_MS`, `AD_EVENT_SWEEP_MS`, and
`AD_ATTEMPT_CHECK_MS`. The chart does not deploy Kamailio or kine.

## Status

Built and proven (Docker-first tests + e2e):

- campaign lifecycle, leads/lists, routing engine, hopper selection, compliance.
- the call path (decision D-9, `PLAN.md` §0.0): each node carries its calls
  itself, SIP (`ad_sip` on `rsipstack`) and RTP (`ad_media` on `rustrtc`),
  behind Kamailio, and bridges the audio into the room with our own WebRTC
  participant. No `livekit/sip`, no host networking. Proven through a NAT with
  a latching carrier (`e2e/run_e2e.sh`, `e2e/run_cluster_e2e.sh`).
- the quickstart, the primary target: the quickstart stack placed real calls
  over the internet to a LiveKit number at LiveKit Cloud SIP, through Docker's
  NAT and a home NAT. LiveKit's 407 digest challenge passed, the audio crossed
  both ways, and our BYE ended the call (`e2e/run_lkcloud_e2e.sh`, 2026-10-24).
  These calls did not go through the public phone network. A live call through
  Twilio is not proven yet: Twilio refused trunks for the test account
  (`GAPS.md` CARRIER-1).
- the cluster (`PLAN.md` §0.10.8b): orchestrator replicas are nodes that share
  the calls through kine. One node leads and assigns calls; a dead node's calls
  end without a second dial; Postgres fences a node that lost a call. Proven
  with three nodes in Docker Compose (`e2e/run_cluster_e2e.sh`).
- agents (decision D-2): a LiveKit agent (explicit dispatch) or a pipecat bot
  (a start request) joins each call room before the dial. Proven with a real
  agent of each kind (`e2e/run_e2e.sh`).
- call attempts (`ad_attempt`, `PLAN.md` §0.10.7a): a statechart saved in
  Postgres with its event log. The claim reads due leads directly and is bound
  by the campaign's `concurrency_cap`. Calls run at the same time, paced by
  `AD_CALLS_PER_SEC`. SIP codes map to busy, no answer, declined, invalid
  number, and failed. A lost dial result holds its capacity as `uncertain`
  until a check against LiveKit decides it. The callee's BYE, the agent
  leaving, or the media timeout ends each call on its node, and the ended
  attempt updates its lead once.
- the Postgres event layer: event-driven orchestrator (LISTEN/NOTIFY with a
  sweep), events written in the same transaction as the state they announce,
  a circuit breaker over the ended attempts that pauses the campaign in
  Postgres (a resume resets it), retries stored on the lead row, and campaign
  completion in Postgres.
- the native WebRTC participant in `ad_gateway` (SIP↔WebRTC bridge, option 2 in
  `PLAN.md` §0.9a). Two-way audio is proven by `e2e/run_webrtc_e2e.sh`: our
  participant carries audio both ways into a LiveKit room with the agent, with no
  `livekit/sip`. The bridge is part of the normal build (no feature gate): the
  Docker test tier compiles, lints, and tests it, and CI runs the audio e2e.

Known limits of the call path (`PLAN.md` §0.0, "Open"):

- RTP latching follows `rustrtc`'s rules, which one marked packet can win; the
  patch against RTP bleed is not done.
- Not tested yet: a resent 200. Not built:
  session timers, the trunk OPTIONS check, the `public` media mode, DTMF.
- A node that dies drops its live calls; nothing sends BYE to the carrier for
  them yet.
- A node used about half a core at two calls in the cluster test; profile it
  before a capacity plan (OPS-8).

Known limits (`GAPS.md` has the full list, by ID):

- Compliance data: production dialing uses a 24-hour call window and an empty
  DNC list. No table stores call times, holidays, or DNC entries yet (COMP-1).
  A lead without an explicit IANA timezone never dials (COMP-2).
- Dial rate: calls run at the same time. A SIPp benchmark (2026-10-21) measured
  50.0 attempts per second at a configured 50 cps, with 84 live calls at the
  peak (one node, before the cluster). The leader paces the whole cluster;
  there is no rate per campaign or per trunk yet (DIAL-3, DIAL-7).
- Outcome: a completed call has no disposition yet, because no agent outcome
  reaches the dialer (DIAL-1, AGNT-2).
- Holds: a lead that the calling window holds is checked again after 15
  minutes. The next window opening is not computed yet (BETTER-3).
- Events: the publish lock serializes event inserts, and `campaign_events` and
  `attempt_events` have no retention yet (ORCH-2).
- Cluster: kine is not etcd (no lease KeepAlive, no Election, no keys-only
  reads), and one kine replica is a single point of failure. A dead node's
  attempts move after the TTL (default 10 s). Agent readiness is a LiveKit poll
  every 250 ms (OPS-8).
- `POST /v1/campaigns/:id/dispatch/run-batch` is experimental. It uses a stub
  initiator and its response says so (`experimental: true`).

Not built yet:

- dispositions UI and reports, A/B traffic split, a full `livekit-agents` brain
  (STT/LLM/TTS). There is no authentication yet.
- one gRPC session stream per agent session for outcomes, events, and logs,
  with OpenTelemetry on the same endpoint (decision D-6). `GAPS.md` lists every
  open gap.
