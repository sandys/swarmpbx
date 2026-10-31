# AgentDialer

AgentDialer is an outbound dialer for AI agents. It runs call campaigns, and
it puts an AI agent on each answered call. It is written in Rust. It is made
for Kubernetes, and the tests run it in Docker Compose.

The dialer carries each call itself. A dialer node is the SIP user agent and
the RTP endpoint of its calls. Kamailio is the SIP edge in front of the nodes.
Each call gets a LiveKit room, and a LiveKit agent or a pipecat bot joins that
room to talk to the callee. When the talk ends, the agent tells the dialer how
it went. The dialer then decides what to do with the lead.

The main target is LiveKit Cloud SIP. The quickstart below connects the
dialer to LiveKit Cloud SIP and a LiveKit phone number, and places calls.
Twilio is a second supported carrier, in two ways:

- as an Elastic SIP Trunk ("Call through Twilio");
- as click-to-call: Twilio calls the phone, and then it calls the dialer
  ("Click-to-call through Twilio").

`PLAN.md` holds the full design. `GAPS.md` lists what is missing, by ID.
`.agents/REPO_GUIDE.md` holds the rules for a change. Read them before you
change the code.

`.agents/skills/` teaches an AI assistant to use and change AgentDialer:
`run-campaigns` (place calls), `connect-an-agent` (build the agent), and
`agentdialer-overview` (change the code). Claude Code, Gemini CLI, and Codex
find them through `.claude/skills`, `.gemini/skills`, and `.codex/skills`.

## What it does

Before the call:

- It runs campaigns. You load leads, start a campaign, and pause it. The
  dialer completes a campaign when no work is left.
- It keeps each phone number in E.164. It refuses a number that the
  numbering plan does not assign.
- It picks the due leads of each campaign, within the campaign's
  concurrency cap.
- It checks each call against compliance: the safe hours of the lead's
  timezone, and the DNC list. When the check cannot prove that a call is
  allowed, the call does not go out.
- It paces the calls by the time that the agents take to join. Agents that
  join fast get more calls at the same time.

During the call:

- It places each call through a SIP trunk, and it carries the audio between
  the callee and the agent's room.
- It starts the campaign's agent for each call: after the answer (customer
  first, the default) or before the dial (agent first).
- It counts the abandoned calls of each campaign. A call with no agent 2 s
  after the answer is `abandoned`. When the rate goes above 3%, the campaign
  dials agent first.

After the call:

- It takes the outcome of each call from the agent: a disposition, a
  callback time, or a DNC request. A DNC request stops the calls to that
  number at once.
- It saves the result of each call, and it schedules retries and callbacks.
- It pauses a campaign whose calls fail too often, or whose agents do not
  start (circuit breakers).

It speaks SIP only. The AI agent takes the place of a human agent. Some parts
are not complete. See "Status".

## Architecture

AgentDialer has three paths:

- **The call path** carries the call. SIP and RTP go between the trunk and a
  node. WebRTC audio goes between the node and the call's LiveKit room.
- **The control path** decides which lead to call, when, and on which node.
  The leader node does this through Postgres and kine.
- **The outcome path** brings the result of the talk back from the agent. The
  agent sends it to the `api` role on a gRPC stream.

### The parts

```mermaid
flowchart LR
    you(["you<br/>scripts/api.sh"]) -->|HTTP| api["api role"]
    agent["AI agent<br/>LiveKit agent or pipecat bot"] -->|"gRPC: the outcome"| api
    api -->|"campaigns, leads,<br/>outcomes, DNC"| pg[("Postgres")]
    orch["orchestrator nodes<br/>one is the leader"] <-->|"claims, attempts,<br/>events, time"| pg
    orch <-->|"members, leader,<br/>call keys"| kine["kine<br/>etcd API"]
    kine -->|"its keys"| pg
    orch -->|"start the agent"| agent
    orch <-->|SIP| kam["Kamailio<br/>SIP edge"]
    kam <-->|SIP| trunk["trunk<br/>LiveKit Cloud SIP or a carrier"]
    orch <-.->|RTP| trunk
    orch <-->|"WebRTC audio"| lk["LiveKit server<br/>a room for each call"]
    agent <-->|"WebRTC audio"| lk
```

- **The `api` role** serves the HTTP API (port 8080) and the agent session
  stream (gRPC, port 9090).
- **The `orchestrator` role.** Each replica is a node. One node is also the
  leader. Each node runs the calls that the leader gives it.
- **Postgres** is the record of every campaign, lead, call, outcome, and DNC
  entry. It also carries the events and the time.
- **kine** gives the nodes the etcd API on top of Postgres. The nodes use it
  to find each other, to elect the leader, and to receive their calls.
- **Kamailio** is the SIP edge between the nodes and the trunk.
- **The LiveKit server** holds one room for each call. The node and the agent
  meet there.
- **The agent** is yours: a LiveKit Agents worker or a pipecat bot. It never
  reaches the database. It reports to the `api` role.

### The call path

```mermaid
flowchart LR
    phone(["callee<br/>phone or callee agent"])
    trunk["trunk<br/>LiveKit Cloud SIP or a carrier"]
    kam["Kamailio<br/>SIP edge"]
    node["agentdialer node<br/>SIP user agent and RTP"]
    room["LiveKit room<br/>one for each call"]
    agent["AI agent<br/>LiveKit agent or pipecat bot"]
    phone <--> trunk
    trunk <-->|SIP| kam
    kam <-->|SIP| node
    trunk <-.->|"RTP, no hop between"| node
    node <-->|"WebRTC audio"| room
    agent <-->|"WebRTC audio"| room
```

- **The trunk** is the far end: LiveKit Cloud SIP in the quickstart, or a
  carrier such as Twilio.
- **Kamailio** sends each INVITE to the trunk, and it adds itself to
  `Record-Route`. So every later SIP message of the call, in both directions,
  goes through Kamailio to the node that owns the call. Kamailio does not
  touch the audio.
- **The node** is the SIP user agent (`ad_sip`, on the vendored `rsipstack`)
  and the RTP endpoint (`ad_media`, on the vendored `rustrtc` and
  `audio-codec`). The RTP goes between the trunk and the node. The node sends
  RTP first, through the NAT gateway. The trunk sends its audio back to the
  address that it sees ("latching").
- **The room.** The node joins the call's LiveKit room as its own WebRTC
  participant (`ad_gateway`), and it bridges the call audio into the room.
  The agent hears the callee there and talks back.
- Every part is a normal pod. There is no host networking and no port range
  for each call. No Asterisk, FreeSWITCH, rtpengine, or `livekit/sip` runs
  under the dialer (decision D-9, `PLAN.md` §0.0).

### The control path

```mermaid
flowchart TB
    you(["you<br/>scripts/api.sh"]) -->|HTTP| api["api role"]
    api -->|"campaigns, leads, pacing"| pg[("Postgres")]
    subgraph orch["orchestrator pods"]
        leader["node 1, the leader<br/>claims, gates, assigns, settles"]
        node2["node 2"]
        node3["node 3"]
    end
    leader <-->|"claims, settlements,<br/>LISTEN/NOTIFY"| pg
    leader -->|"a key for each call"| kine["kine<br/>etcd API on Postgres"]
    kine -->|"watch: my calls"| node2
    kine -->|"watch: my calls"| node3
    node2 -->|"attempt events, fenced"| pg
    node3 -->|"attempt events, fenced"| pg
    kine -->|"stores its keys"| pg
```

- **The leader.** One node holds the leader key in kine. The leader claims
  due leads from Postgres, and it gates each one on compliance. It gives each
  call to the node with the most free capacity (a key in kine), and it
  settles each ended call. Every node, the leader too, runs the calls that
  kine gives it.
- **Scale.** A Kubernetes scale-up adds nodes. A new node writes its key in
  kine, and the leader gives it calls.
- **Fencing.** Postgres is the record of every call. A node changes an
  attempt only while the attempt row names that node and its assignment
  generation. A node that lost a call cannot dial it or change it.
- **Events** go through Postgres: an event table with LISTEN/NOTIFY, and a
  sweep that finds what a lost notification left. There is no Redis.
- **Managed services.** The dialer treats Postgres and kine as services that
  it reaches by URL. A role waits for them with backoff. It does not depend
  on a host, a start order, or a health check.

### Before the dial: the claim and the gate

The leader does these checks for each due lead:

```mermaid
flowchart TD
    due(["a due lead"]) --> claim{"Can the campaign claim it?<br/>running, breakers closed, under its cap,<br/>free node capacity, a place in the pool window"}
    claim -->|no| later(["the lead waits for the next event"])
    claim -->|yes| order{"dial_order is customer_first,<br/>and the abandonment rate<br/>is at or under 3%?"}
    order -->|yes| cf["customer first"]
    order -->|"no, or the ledger does not read"| af["agent first"]
    cf --> read{"Does the DNC list read?"}
    af --> read
    read -->|no| release(["release the claim: no dial"])
    read -->|yes| listed{"Is the number on the list?"}
    listed -->|yes| dnc(["not dialed: the lead is dnc"])
    listed -->|no| tz{"Does the lead have a timezone?"}
    tz -->|no| blocked(["not dialed: the lead is blocked"])
    tz -->|yes| window{"Is the lead's local time<br/>in the calling window?"}
    window -->|no| hold(["not dialed: try again in 15 min"])
    window -->|yes| assign(["give the call to the node<br/>with the most free capacity"])
```

Each "no" that the leader cannot prove ends in no dial. This is the
fail-closed rule of the compliance engine (`ad_compliance`). The DNC check
compares E.164 numbers, so `+1 202 555 0123` and `(202) 555-0123` are the same
number.

### One call, step by step

This is a customer-first call, the default:

```mermaid
sequenceDiagram
    participant L as Leader
    participant PG as Postgres
    participant KI as kine
    participant N as Node
    participant T as Trunk
    participant R as LiveKit room
    participant A as Agent
    participant API as api role
    L->>PG: claim a due lead, and gate it
    L->>KI: write the call key for the node
    KI-->>N: watch: a new call
    N->>PG: save dial_started
    Note over N,T: every SIP message goes through Kamailio
    N->>T: INVITE
    T-->>N: 407, a digest challenge
    N->>T: INVITE with the digest
    T-->>N: 200 OK
    par at the 200 OK
        N->>R: join the room
    and
        N->>A: start request, with a session token
    end
    A->>R: join, within 2 s of the answer
    N->>PG: save answered and agent_ready
    A->>API: open the session: Hello with the token
    API-->>A: Welcome: the disposition codes
    Note over T,A: audio: trunk, RTP, node, WebRTC, room, agent
    T->>N: BYE
    N->>PG: save hangup: the attempt ends, with attempt_ended
    A->>API: Outcome, for example CALLBK
    API->>PG: save the outcome, with agent_outcome
    API-->>A: Ack
    PG-->>L: NOTIFY
    L->>PG: settle the lead by the outcome, claim the next lead
    L->>R: close the room
```

1. The leader claims a due lead and gates it ("Before the dial"). The claim
   locks the campaign row.
2. The leader writes the call's key to kine, at most `AD_CALLS_PER_SEC` keys
   each second. The node that owns the key sees it and starts the call.
3. The node saves `dial_started`, and then it sends the INVITE through
   Kamailio. The trunk asks for the password (407). The node answers with the
   digest credentials.
4. At the 200 OK, the node sends the agent's start request and joins the
   room. It saves `answered` with the time of the 200 OK. The agent must be
   in the room 2 s after the answer. If it is not, the node hangs up, and the
   outcome is `abandoned`.
5. The agent opens its session stream to the `api` role with the token of
   the call. The dialer answers with the disposition codes of the campaign.
6. The audio flows until the callee sends BYE, the agent leaves, or no RTP
   comes for 30 s.
7. The transaction that ends the attempt also writes the event
   `attempt_ended`. The leader wakes on the NOTIFY.
8. The agent sends the outcome of the call, during the call or after it. The
   `api` role checks the outcome against the campaign's dispositions, saves
   it, and writes the event `agent_outcome`.
9. The leader settles the lead one time ("After the call"). Then it closes
   the room, and it dials into the free capacity.

With `"dial_order": "agent_first"` in the campaign's `agent`, the node starts
the agent first. It sends the INVITE when the agent is in the room.

### After the call: the settlement

The leader updates the lead one time for each ended attempt:

```mermaid
flowchart TD
    ended(["the attempt ended"]) --> waits{"Answered, the agent joined,<br/>no outcome yet, and less than<br/>outcome_wait_s since the end?"}
    waits -->|yes| wait(["wait: the agent can still report"])
    wait -.->|"agent_outcome, or the sweep"| waits
    waits -->|no| dncq{"Did the agent ask for DNC?"}
    dncq -->|yes| dnc(["dnc: the number goes on the DNC list"])
    dncq -->|no| answered{"Did the callee answer,<br/>and did the agent report?"}
    answered -->|no| result{"the call result"}
    answered -->|yes| cb{"a callback time?"}
    cb -->|yes| queued(["queued at that time, priority 50"])
    cb -->|no| am{"a machine answer?"}
    am -->|yes| vm(["voicemail retry in 60 min"])
    am -->|no| result
    result -->|"completed, declined"| disp(["dispositioned"])
    result -->|busy| r5(["retry in 5 min"])
    result -->|"no answer"| r30(["retry in 30 min"])
    result -->|"failed, abandoned, error"| r15(["retry in 15 min"])
    result -->|"invalid number"| done(["done"])
```

- A DNC request takes effect when the agent sends it, not at the settlement.
  The `api` role puts the number on the DNC list, and every waiting lead with
  that number stops at once.
- A lead gets at most 3 calls. After the third call, a retry becomes
  `dispositioned`.
- A retry or a callback goes on the lead row as `next_eligible_at`. The claim
  takes a lead with a higher priority first.
- The retry of an `abandoned` lead dials agent first.
- While the leader waits for an outcome, the call frees its place on the
  node. The room stays open for the agent.

### Call attempts

Each call is an attempt with a state machine (`ad_attempt`). The state machine
is pure: it takes an attempt and an event, and it gives the next attempt.
Postgres saves the attempt and each event in one transaction. A node that
starts again continues from the saved state.

```mermaid
stateDiagram-v2
    [*] --> pending: the leader claims a lead
    pending --> dialing: dial_started
    pending --> ended: not dialed (gate, no agent)
    dialing --> connected: answered
    dialing --> uncertain: no result came back
    dialing --> ended: busy, no answer, declined, failed
    uncertain --> connected: the check finds the call
    uncertain --> ended: the check finds no call
    connected --> ended: BYE, agent left, no RTP for 30 s, no agent at 2 s
    ended --> settled: the leader updates the lead
    settled --> [*]
```

An ended attempt has an outcome: `completed`, `busy`, `no_answer`,
`declined`, `invalid_number`, `failed`, `abandoned`, `setup_error`,
`uncertain`, or `not_dialed`. The node maps the SIP final code to the outcome.
An `uncertain` attempt keeps its capacity until the node's check finds the
call in its room, or not.

### The data

Postgres holds all the state. Each role reaches it by URL.

| Table | What it holds | Who writes it |
|---|---|---|
| `campaigns` | Each campaign: its state, its agent, its disposition codes. | the `api` role, the leader |
| `leads` | Each lead: its number, timezone, status, priority, and next due time. | the `api` role, the leader |
| `call_attempts` | Each call: the saved state machine, the node and generation that own it, the outcome, and the agent's outcome. | the nodes, the leader, the `api` role |
| `attempt_events` | The events of each call, in order. | the nodes, the leader |
| `campaign_events` | The event layer, with LISTEN/NOTIFY. | all roles |
| `agent_pools` | The pacing state and settings of each agent pool. | the leader, the `api` role |
| `dnc_numbers` | The DNC list: a system scope and a campaign scope. | the `api` role, for you and for the agents |

kine keeps its keys in a table of its own in the same database.

### When a node stops

- A node that gets SIGTERM deletes its keys. Its calls move at once.
- A node that dies loses its key after `AD_CLUSTER_TTL_S` (default 10 s). Its
  live calls drop, because the node carried their audio. The leader moves the
  attempts. The new owner's check finds each call gone from its room, and the
  attempt ends. No lead is dialed two times.
- When the leader dies, another node takes the leader key. It continues from
  the state in Postgres.

### Pacing by join time

The dialer learns how many calls an agent pool can take at the same time
(`ad_pacing`, `GAPS.md` DIAL-9). A pool is the agents of one agent name
(LiveKit) or of one start URL (pipecat).

- The window of a pool is its calls whose agent is not in the room yet. A
  call that rings counts.
- The leader claims a call only while the window is under the pool's limit
  `k`.
- Fast joins raise `k`. A slow join, a failed start, or no agent at 2 s
  halves it.
- Five failed starts in a row trip the pool's breaker, and the leader pauses
  the campaign.

The state of each pool is in Postgres, so a new leader continues from it.
"Your own agents" shows the settings.

### Time

The roles take the time from Postgres, not from the OS clock of their node
(decision D-11).

- Each role reads the Postgres clock at start and every 30 s. It adds the
  monotonic time since the last read.
- Each call reads that clock one time at its start. After that, it uses only
  the monotonic clock. A clock step cannot change a ring time, a join time,
  or the 2 s test.
- The LiveKit tokens get their times from the same clock. Only the log lines
  use the OS clock.

## Quickstart: place calls through LiveKit Cloud SIP

These steps start the whole stack in Docker Compose, connect it to LiveKit
Cloud SIP with a phone number, and place calls. This is the main supported
use, and it must not break: `e2e/run_lkcloud_e2e.sh` runs the same steps
against LiveKit Cloud. The host needs only bash, git, and the Docker CLI.

```mermaid
flowchart LR
    subgraph local["your machine: the Docker Compose stack"]
        demo["demo agent"]
        lk["LiveKit server<br/>the dialer's rooms"]
        node["agentdialer node"]
        kam["Kamailio"]
        callee["callee agent"]
    end
    subgraph cloud["LiveKit Cloud"]
        sip["LiveKit Cloud SIP<br/>your number"]
        croom["a room for each call"]
    end
    demo <-->|WebRTC| lk
    lk <-->|WebRTC| node
    node <-->|SIP| kam
    kam <-->|"SIP over the internet"| sip
    node <-.->|"RTP over the internet"| sip
    sip <--> croom
    callee <-->|WebRTC| croom
```

A call goes like this:

1. The node dials your LiveKit phone number at your project's SIP URI, as it
   dials a carrier. LiveKit Cloud is the trunk.
2. LiveKit Cloud asks for the trunk password (a 407 digest challenge). The
   node answers with the credentials that setup made.
3. A dispatch rule puts the call into a room of its own in LiveKit Cloud. The
   callee agent joins that room and answers.
4. The audio goes between the node and LiveKit Cloud. The stack's own
   LiveKit server holds the dialer's side of the call, where your agent talks.

LiveKit phone numbers take incoming calls only. So these calls stay in
LiveKit Cloud. They do not go through the public phone network, and no phone
rings. To call a real phone, use a carrier trunk ("Call through Twilio").

### What you need

1. A LiveKit Cloud project. Each LiveKit Cloud plan includes one free US phone
   number.
2. An API key and its secret (LiveKit Cloud, Settings, API keys).
3. The project's SIP URI (LiveKit Cloud, Project Settings). The project URL
   does not give it.
4. A `.env` file at the repository root. Git ignores it:

   ```
   LIVEKIT_URL=wss://<project>.livekit.cloud
   LIVEKIT_API_KEY=...
   LIVEKIT_API_SECRET=...
   LIVEKIT_SIP_URI=sip:<id>.sip.livekit.cloud
   ```

### 1. Connect LiveKit Cloud SIP

```bash
e2e/livekit-cloud/run.sh setup
```

The script runs in a container, with LiveKit's SDK and the `lk` CLI. It does
not print a secret. It does these steps, and it uses again each part that
exists:

- It rents a LiveKit phone number, or it uses the number of the project. It
  prints the number.
- It creates the inbound trunk `agentdialer-e2e`, with the user `agentdialer`
  and a generated digest password. The dialer uses this trunk.
- It creates the dispatch rule `agentdialer-e2e`. The rule serves the trunk and
  the number. It gives each call a room of its own, and it sends the callee
  agent `agentdialer-e2e-callee` into the room.
- It creates the outbound trunk `agentdialer-e2e-outbound` for a reference
  call (step 4). The trunk points at the project's own SIP URI, because
  LiveKit numbers take incoming calls only.
- It writes `.env.lkcloud` (git ignores it) for the stack: the SIP URI host
  as `AD_SIP_TRUNK`, the trunk user and password, the caller ID, and the
  number.

You can run it again at any time. `e2e/livekit-cloud/run.sh inspect` shows the
number, the trunks, and the rule.

### 2. Start the stack

```bash
QS="docker compose -f docker-compose.quickstart.yml -f docker-compose.livekit-cloud.yml --env-file .env --env-file .env.lkcloud"
$QS up -d --build
```

The stack has Postgres, kine, LiveKit (the dialer's rooms), Kamailio (the SIP
edge), the migrate job, the API (on `127.0.0.1:8080`), one dialer node, a demo
agent, and the callee agent.

- The demo agent joins each call on the dialer's side. It plays a tone, and
  it leaves after `DEMO_AGENT_MAX_CALL_S` (default 30 s). The node then ends
  the call.
- The callee agent connects to LiveKit Cloud and joins each call's room
  there. It plays a tone too.

### 3. Place calls

`scripts/api.sh` calls the API from a container on the stack's network. Put
your LiveKit number in the lead:

```bash
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

- Write each phone number with its country code, or add `"default_region":
  "US"` next to `"leads"`. The dialer keeps the number in E.164. It refuses
  the whole list if one number is bad, and it tells you which leads to
  correct.
- Give each lead an IANA timezone. A lead without one does not dial, because
  compliance fails closed.
- To keep a number from every call, add it to the DNC list:
  `scripts/api.sh POST /v1/dnc '{"phone_number": "+12025550123"}'`.

### 4. Watch

```bash
$QS logs -f node
scripts/api.sh GET /v1/campaigns/1
scripts/api.sh GET /v1/campaigns/1/attempts   # each call, with the agent's disposition
e2e/livekit-cloud/run.sh call
```

- The LiveKit Cloud dashboard shows a room for each call. The room name starts
  with `agentdialer-e2e-`.
- `e2e/livekit-cloud/run.sh call` makes a reference call. LiveKit's own SIP
  client calls your number through the outbound trunk, and the callee agent
  joins. Use it to check the LiveKit Cloud side alone. The stack must be up,
  because the callee agent runs in it.

### 5. Stop

```bash
$QS down -v
e2e/livekit-cloud/run.sh teardown   # optional: delete the trunks and the rule
```

Teardown keeps the number. If you do not need the number, release it in the
LiveKit Cloud dashboard.

### Your own agents

**The agent.** Put your agent in the campaign:

- a LiveKit agent: `{"runtime": "livekit", "agent_name": "<name>"}`;
- a pipecat bot: `{"runtime": "pipecat", "start_url": "<url>"}`.

The agent must reach the stack's LiveKit server. Run it on the stack's
network (`agentdialer-quickstart_default`) with `LIVEKIT_URL=ws://livekit:7880`.

**The dial order.** `"dial_order"` in the campaign's `agent` sets when the
agent starts. For example:
`{"runtime": "livekit", "agent_name": "demo", "dial_order": "agent_first"}`.

- `"customer_first"` (the default): the dialer dials the customer. When the
  customer answers, the dialer starts the agent in the call's room. An agent
  starts only for an answered call.
- `"agent_first"`: the dialer starts the agent, waits until it is in the room
  (`join_timeout_s`, default 20), and then dials. No call is abandoned, but
  each call that rings uses an agent.

CAUTION: With `customer_first`, the agent must be in the room 2 s after the
answer (the US TSR rule). If it is not, the dialer hangs up, and the outcome
is `abandoned`. The 2 s include the agent's start request. Keep the agent's
workers ready for a job.

The dialer sends the start request at the answer, while it joins the room.
An agent that starts late must not open the closed room again. Set
`room.auto_create: false` on the LiveKit server of the dialer's rooms
(`e2e/livekit-cluster.yaml` does this).

**The outcome.** Your agent tells the dialer how each call went: a
disposition (`SALE`, `NI`, `CALLBK`, `DNC`, and others), a callback time, or
a DNC request. It sends the outcome on the session stream, a gRPC stream to
the `api` role (`proto/agentdialer/session/v1/session.proto`). The call facts
of each call give the URL of the stream and a token for that call.

- A DNC request puts the number on the DNC list. Every waiting lead with that
  number stops at once.
- A callback queues the lead at that time, before the other leads.
- After an answered call, the dialer waits for the outcome, at most
  `outcome_wait_s` in the campaign's `agent` (default 30). With no outcome,
  it settles the lead by the call result.
- Open the session after the agent joins, in the background. With customer
  first, the agent has only 2 s to join.
- The demo agent reports `DONE`. `e2e/agents/session/agent_session.py` is a
  Python client. The `connect-an-agent` skill has the full contract.
- The node gives each agent the URL `http://api:9090`. If your agent runs in
  another place, set `AD_SESSION_URL` to a URL that it reaches before you
  start the stack.
- `scripts/api.sh GET /v1/campaigns/1/dispositions` lists the codes. To add
  codes for a campaign, `PUT` a list of `{"code", "name", "flags"}` there.

**Abandonment.** The ledger counts the answered and abandoned calls of the
campaign for 30 days: `scripts/api.sh GET /v1/campaigns/1/abandonment`. When
the rate is above 3%, the campaign dials agent first until the rate is at or
under 3%. The retry of an abandoned lead dials agent first.

**Pacing.** The dialer paces each agent pool by the time its agents take to
join ("Pacing by join time"). Campaigns with the same agent share one pool.

- `k` starts at 1. A round of fast joins (at or under the target, default
  1.0 s) raises `k`. It doubles until the first join above the target, and
  then it grows by 1. A join above the back-off (default 1.5 s), a start that
  fails, or no agent at the 2 s limit halves `k`.
- The mode of a campaign: `adaptive` (the default) uses `k`. `strict` also
  lets only one agent start more than the pool proved at the same time.
- You can change each value at run time, with no restart. The next claim
  uses it:

  ```bash
  scripts/api.sh GET /v1/campaigns/1/pacing
  scripts/api.sh PUT /v1/campaigns/1/pacing \
    '{"mode": "strict", "pool": {"target_ms": 800, "backoff_ms": 1200, "first_k": 2, "max_k": 8}}'
  scripts/api.sh PUT /v1/campaigns/1/pacing '{"reset": true}'
  scripts/api.sh GET /v1/agent-pools
  ```

  `pool` replaces the settings of the pool, for all its campaigns. A value
  that you do not give takes its default. The back-off must be above the
  target and at most 2 s. `reset` starts the pool again at `first_k`.

**The LiveKit Cloud side.** The dispatch rule sends the agent
`agentdialer-e2e-callee` into each call's room. To answer with your own
agent, register it in LiveKit Cloud with that agent name, and start the stack
without `-f docker-compose.livekit-cloud.yml`.

### If a call fails

| Symptom | Cause |
|---|---|
| setup: `LIVEKIT_SIP_URI is not set` | Copy the SIP URI from LiveKit Cloud, Project Settings, into `.env`. |
| setup: `LiveKit refused the request` | LiveKit Cloud refused the key, the secret, or the request. The message gives LiveKit's reason. |
| setup: `LiveKit has no US number for rent now` | LiveKit has no free number at this time. Try again later. |
| The campaign does not dial | The campaign is not `running`, or the lead has no timezone. |
| The call connects, but the demo agent hears nothing | The callee agent did not join the room in LiveKit Cloud. Read `$QS logs callee`. |
| The attempt's outcome is `abandoned` | The agent was not in the room 2 s after the answer. Check that the agent worker runs and has a free process (`$QS logs agent`), or set `"dial_order": "agent_first"` in the campaign's `agent`. |
| The attempt has no `disposition` | The agent did not report in `outcome_wait_s`. Check that the agent reaches `AD_SESSION_URL` (default `http://api:9090`), and read `$QS logs api` for `agent session open`. |
| The agent's outcome comes back `Rejected` | The code is not in the campaign's catalog, or the callback time is wrong. The reason says which. |
| The campaign dials one call at a time | The pool's limit `k` is 1, because its agents are slow to join. Read `scripts/api.sh GET /v1/campaigns/1/pacing` (`recent.p95_ms`, `slow`). Keep the agent's workers ready for a job. |
| `GET /v1/campaigns/1/pacing` answers 409 "invalid pacing settings" | The saved settings of the pool do not agree with the 2 s limit. The pool dials one call at a time. Send new settings, or `{"reset": true}`. |

## Call through Twilio

Twilio Elastic SIP Trunking is a second supported carrier, and it reaches real
phones. The quickstart stack places the calls. Only the trunk changes.
`e2e/run_twilio_e2e.sh` runs these steps against a real trunk.
`e2e/run_twilio_e2e.sh --sim` runs them on every change, with the carrier
simulator in Twilio's place (CI).

### What you need

1. A Twilio account with Elastic SIP Trunking. An upgraded account has it.
   Twilio's documents do not agree about Trial accounts. On 2026-10-22,
   Twilio's API refused trunks for a Trial account with HTTP 401.
2. A caller ID: a Twilio number of the account, or a verified caller ID
   (Twilio Console, Phone Numbers, Verified Caller IDs). Twilio requires one
   of them in From.

   CAUTION: On a Trial account, Twilio calls verified numbers only. Put a
   verified number in the lead. That phone rings.
3. An API key (Twilio Console, Account, API keys & tokens). A Main key works
   alone. A Standard key also needs the account SID.
4. A `.env` file at the repository root. Git ignores it:

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

```bash
e2e/twilio/run.sh setup
```

The script runs in a container. It does not print a secret. It creates the
trunk `agentdialer-e2e` (termination URI `agentdialer-<id>.pstn.twilio.com`),
or it uses the trunk again when it exists. It also does these steps:

- It creates a credential list with a generated digest password.
- It turns on Symmetric RTP.
- It selects the caller ID.
- It writes `.env.twilio` (git ignores it) for the stack.

You can run it again at any time. `e2e/twilio/run.sh inspect` shows the
account, its numbers, its verified caller IDs, and its trunks.

If setup prints `ACTION` for Symmetric RTP, turn on "Symmetric RTP" for the
trunk in the Twilio Console. The node is behind NAT. Its SDP names a private
address, so Twilio must send the audio to the address that the audio comes
from.

### 2. Start the stack

```bash
docker compose -f docker-compose.quickstart.yml --env-file .env.twilio up -d --build
```

This is the quickstart stack without the callee agent.

### 3. Place calls

Do step 3 of the quickstart, but set `NUMBER` to the number to call. For
example, `NUMBER=+16504894546` is Twilio's "Play" test number. Calls to it are
free, and you can reach it only through a trunk.

### 4. Watch

```bash
docker compose -f docker-compose.quickstart.yml logs -f node
scripts/api.sh GET /v1/campaigns/1
e2e/twilio/run.sh calls +16504894546 2026-10-21T00:00:00Z   # Twilio's record
```

### 5. Stop

```bash
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
| The callee hangs up, but the call ends about 30 s later | Behind NAT, Twilio's BYE may not reach Kamailio. The node's media timeout ends the call. Give Kamailio a public address to fix this. |

## Click-to-call through Twilio

In click-to-call, Twilio calls the phone first. When the phone answers,
Twilio calls the dialer and joins the two calls. Twilio Programmable Voice
places the call, so the dialer needs no SIP trunk. The quickstart stack does
it with `AD_DIAL_PROVIDER=twilio_click_to_call`.

```mermaid
sequenceDiagram
    participant N as Node
    participant KA as Kamailio
    participant TW as Twilio
    participant P as Phone
    N->>TW: Calls API: call the lead, with inline TwiML (Dial, Sip) and a token
    TW->>P: call
    loop until the INVITE comes
        N->>TW: read the call status
    end
    P-->>TW: answer
    TW->>KA: INVITE with x-ad-call (token) and x-ad-node
    KA->>N: INVITE to the node in x-ad-node
    N-->>TW: 200 OK through Kamailio, only for the token of one of its dials
    Note over N,P: audio: node, RTP, Twilio, phone. The agent is in the room.
    N->>TW: BYE through Kamailio ends the Dial
    TW->>P: hang up
```

A call goes like this:

1. The node asks Twilio's Calls API to call the lead. The request carries the
   TwiML inline, so the node needs no webhook and no public HTTP address.
2. The phone answers. Twilio runs `<Dial><Sip>`. It sends an INVITE to
   `AD_TWILIO_SIP_URI`, with the call's token (`x-ad-call`) and the node's
   address (`x-ad-node`) as headers.
3. Kamailio routes the INVITE to the node in `x-ad-node`. The node answers
   only an INVITE with the token of one of its dials. Any other INVITE gets
   404. With `"dial_order": "customer_first"`, the node starts the agent when
   it answers this INVITE, because the phone has answered.
4. Until the INVITE comes, the node reads the call's status at the Calls API.
   Busy, no answer, and failed end the attempt with that outcome. At the ring
   timeout (30 s), the node cancels the call.
5. A BYE from either side ends the call. Our BYE ends Twilio's `<Dial>`, and
   Twilio then hangs up the phone.

`e2e/run_twilio_c2c_e2e.sh --sim` proves these steps on every change, with the
carrier simulator as the Calls API and the phones. We did not test it against
Twilio yet: our Twilio account is a Trial account, and a Trial account blocks
`<Dial><Sip>`.

### What you need

1. An upgraded Twilio account (Twilio Console, Billing > Upgrade).

   CAUTION: A Trial account cannot do click-to-call. Twilio blocks
   `<Dial><Sip>` on a Trial account. Its Calls API accepts only `To`, a Twilio
   template URL, and `StatusCallback`. It refuses our request with "Invalid or
   disallowed parameters provided - trial accounts have limited parameter
   access". Twilio's test credentials and magic numbers (`+15005550006`) do
   not place a call, so no INVITE comes from them.
2. The Twilio API key in `.env`, as in "Call through Twilio".
3. A caller ID: a Twilio number of the account, or a verified caller ID.
4. A SIP address that Twilio can reach. In production, that is Kamailio's
   public address. On a laptop behind NAT, use an ngrok TCP tunnel to
   Kamailio: put `NGROK_AUTHTOKEN` in `.env`. An ngrok TCP endpoint needs a
   payment method on the ngrok account. ngrok carries no UDP. The RTP goes
   through the NAT, so Twilio must send it to the address that it comes from.

### Run it

```bash
e2e/run_twilio_c2c_e2e.sh            # live: ngrok, Twilio, and a call to TWILIO_C2C_TO
```

The script does these steps:

1. It starts ngrok (`e2e/docker-compose.ngrok.yml`) and reads the tunnel's
   address from the ngrok API.
2. `e2e/twilio/run.sh setup-c2c` finds the account and the caller ID, and
   writes `.env.twilio-c2c` (git ignores it). It creates nothing at Twilio.
   It stops on a Trial account.
3. It starts the quickstart stack with these settings: the provider, the
   `.env.twilio-c2c` values, `AD_TWILIO_SIP_URI=sip:agentdialer@<tunnel>;transport=tcp`,
   and `AD_EDGE_TCP_HOST` and `AD_EDGE_TCP_PORT`. Kamailio puts the tunnel's
   address in its Record-Route, so Twilio's later requests come back through
   the tunnel.
4. It places one call to `TWILIO_C2C_TO` (default: Twilio's "Play" number,
   `+16504894546`), as in step 3 of the quickstart.

## Crates

The workspace is a set of crates under `crates/`. One crate has one concern.

**Decisions (pure: data in, a decision out, no I/O):**

- `ad_core`: typed ids, errors, and a `Clock` trait for time in tests.
- `ad_compliance`: the safe-hours gate, DNC, and the abandonment ledger. It
  fails closed.
- `ad_campaign`: the campaign and its lifecycle state machine.
- `ad_lead`: the lead and the lead repository trait.
- `ad_routing`: a routing rule engine on the FusionPBX dialplan model
  (conditions, actions, anti-actions, groups, break, `${}` and `$N`
  interpolation).
- `ad_hopper`: lead selection by status, call count, DNC, safe hours, and
  routing.
- `ad_circuit_breaker`: a failure-rate breaker over a rolling window.
- `ad_attempt`: the call-attempt state machine, with a customer leg and an
  agent part. It maps SIP codes to outcomes.
- `ad_pacing`: the controller of the window of each agent pool. It learns
  from the agents' join times (slow start, a halving on a slow join), with
  the modes `adaptive` and `strict`.
- `ad_orchestrator`: the scheduling gate, the run loop, the event reactor
  (dispatch, or settle then dispatch), the retry decision, and the settlement
  (what an ended attempt and its agent outcome do to the lead).
- `ad_contact`: phone numbers (E.164, on the vendored `phonenumber`, the
  libphonenumber data) and postal codes. Every crate reads numbers through
  it, so one change there replaces the library for all of them.
- `ad_disposition`: the disposition catalog (system codes, campaign codes,
  and their flags) and the check of an agent's report: the code, the
  callback time in the lead's zone, and the DNC scope.

**The call:**

- `ad_sip`: the node's SIP user agent on the vendored `rsipstack`. It places
  an outbound call through the SIP edge, with the ring timeout (CANCEL) and
  the end of the call. It also takes an inbound call that the node expects
  (click-to-call: the INVITE carries the token of one of its dials).
- `ad_media`: the RTP leg of one call, on the vendored `rustrtc` and
  `audio-codec`. It does the SDP offer and answer, G.711 both ways, a 20 ms
  send clock that does not stop, and the media timeout.
- `ad_gateway`: the call path. `SipCaller` places a call with `ad_sip` and
  `ad_media`, and it bridges the audio into the LiveKit room with our own
  WebRTC participant. It also does the room operations and the presence
  check of the reconcile. `Telephony` is the provider registry: a SIP trunk,
  or Twilio click-to-call (`click_to_call`).
- `ad_agent`: the agent adapters. `livekit` uses explicit dispatch by agent
  name. `pipecat` sends a start request with a room token. Both get the same
  call facts, with the session URL and token.
- `ad_livekit`: LiveKit access tokens and the room service client. It dates
  each token by the role's clock, not by the OS clock.
- `ad_dispatch`: the dial seam (`CallInitiator`, `Placement`) and the launch
  pacer. It also holds the older dial loop of the experimental
  `dispatch/run-batch` endpoint.

**State and coordination:**

- `ad_db`: the Postgres data layer (sqlx) and its migrations. It holds the
  call attempts (the claim, the saved state machine, the settlement), the
  agent pools (`agent_pools`), the event layer (`campaign_events` with
  LISTEN/NOTIFY), the breaker window over the ended attempts, the DNC list
  (`dnc`), the agent sessions and their outcomes (`outcomes`), and `PgClock`
  (the Postgres time plus the monotonic time).
- `ad_events`: the event contract: typed campaign events, the publisher and
  consumer traits, the breaker-store trait, and in-memory fakes for tests.
- `ad_cluster`: coordination over kine: node membership, the leader, call
  assignment, and placement.

**The roles:**

- `ad_api`: the HTTP API (axum): campaigns, leads, pacing, dispositions,
  attempts, events, and the DNC list. A `router()` factory makes the API
  testable in process. It still has the LiveKit webhook endpoint
  (`POST /v1/webhooks/livekit`). The call path does not use it.
- `ad_session`: the agent session stream (gRPC, tonic), from
  `proto/agentdialer/session/v1/session.proto`. The `api` role serves it. It
  checks the token of the call, sends the catalog, and saves the outcome.
- `ad_bin`: the one binary, `agentdialer`. The subcommand selects the role:
  `api`, `orchestrator`, `dispatch`, or `migrate`. One image runs all roles.

`vendor/` holds the vendored libraries: exact upstream copies at pinned
commits (`vendor/VENDORED.md`). The workspace excludes them, so the test tier
does not format or lint them.

## Build and test (Docker-first)

Everything builds and runs in containers, the same way on a laptop and in CI.
Do not use a toolchain on the host.

Run the full test tier (fmt, clippy, and the suite, with the Postgres
integration tests of the data layer and the event layer):

```bash
docker compose -f docker-compose.test.yml up --build \
    --abort-on-container-exit --exit-code-from tests
docker compose -f docker-compose.test.yml down
```

Stop the stack after each test run.

To run the tests of one crate, run one command in the `tests` service. Compose
starts Postgres first:

```bash
docker compose -f docker-compose.test.yml run --rm --build tests \
    cargo test -p ad_orchestrator
docker compose -f docker-compose.test.yml down
```

The `tests` service mounts the source read-only. To format the source, run
`cargo fmt` as your own user in the test image (the compose build makes
`agentdialer-tests`):

```bash
docker run --rm --user "$(id -u):$(id -g)" -e HOME=/tmp \
    -v "$PWD":/app -w /app agentdialer-tests:latest cargo fmt
```

CAUTION: Do not add `--all`. It also formats the path dependencies. That
changes the vendored crates in `vendor/`, which must stay exact upstream
copies.

The host needs only bash, git, and the Docker CLI. Do not install Rust,
Python, `psql`, or other tools on the host.

## Configuration

Each role reads its settings from environment variables. The contract is in
`crates/ad_bin/src/config.rs`. At start, a role reports every missing or
bad variable in one message and exits. A blank value counts as missing. A
test in `ad_bin` checks the Helm chart, the e2e scripts, and this section
against the contract, so they cannot drift.

### `orchestrator`

| Variable | Need | Default | Meaning |
|---|---|---|---|
| `DATABASE_URL` | required | — | Postgres URL. Postgres also carries the events, the retries, the breaker window, and the time. |
| `LIVEKIT_URL` | required | — | LiveKit server URL. |
| `LIVEKIT_API_KEY` | required | — | LiveKit API key. |
| `LIVEKIT_API_SECRET` | required | — | LiveKit API secret. |
| `AD_DIAL_PROVIDER` | optional | `sip_trunk` | The telephony provider. `sip_trunk`: the node sends the INVITE to `AD_SIP_TRUNK`. `twilio_click_to_call`: Twilio calls the lead, then sends an INVITE to the node ("Click-to-call through Twilio"). |
| `AD_SIP_TRUNK` | optional | — | The carrier's `host:port`. The node calls `sip:<number>@<trunk>`. Required with `sip_trunk`. |
| `AD_SIP_PROXY` | optional | — | The SIP edge (Kamailio), `host:port`. Every INVITE goes through it, and Kamailio stays on the call path. Without it, the node sends to the trunk. |
| `AD_SIP_BIND` | optional | `0.0.0.0:5060` | The node's SIP socket. |
| `AD_SIP_ADVERTISE_IP` | optional | the local address of the route to the proxy (else to the trunk) | The address in Via, Contact, and the SDP: the pod IP. Helm sets it from `status.podIP`. |
| `AD_SIP_TRUNK_USER` | optional | — | The digest user for the trunk. Set it with `AD_SIP_TRUNK_PASSWORD`, or set neither. |
| `AD_SIP_TRUNK_PASSWORD` | optional | — | The digest password for the trunk. |
| `AD_SIP_CALLER_ID` | optional | — | The caller ID in our From, in E.164 (`+15551234567`). Twilio requires a number of the account or a verified caller ID. Without it, From is `agentdialer`. Required with `twilio_click_to_call`: the From of Twilio's call to the lead. |
| `AD_MEDIA_PUBLIC_IP` | optional | — | The `public` media mode: the SDP names this address (an Elastic IP of the pod) instead of the pod IP. |
| `AD_TWILIO_API_URL` | optional | `https://api.twilio.com` | The Twilio API. The tests point it at the carrier simulator. |
| `AD_TWILIO_ACCOUNT_SID` | optional | — | The Twilio account SID (`AC...`). Required with `twilio_click_to_call`. |
| `AD_TWILIO_API_KEY` | optional | — | A Twilio API key SID (`SK...`), or the account SID. Required with `twilio_click_to_call`. |
| `AD_TWILIO_API_SECRET` | optional | — | The secret of the API key, or the auth token. Required with `twilio_click_to_call`. |
| `AD_TWILIO_SIP_URI` | optional | — | The SIP URI that Twilio dials to reach the SIP edge, for example `sip:agentdialer@edge.example.com;transport=tcp`. The node adds the call's token and its own address as headers. Required with `twilio_click_to_call`. |
| `AD_ETCD_ENDPOINTS` | required | — | kine (the etcd API on Postgres) endpoints, separated by commas. The nodes find each other, elect the leader, and receive their calls there. |
| `AD_ROOM_PREFIX` | optional | `campaign` | Room name prefix. The room of a call is `<prefix>-<attempt id>`. |
| `AD_BATCH_LIMIT` | optional | `20` | Leads claimed per claim. A claim takes no more than the free capacity of the campaign. |
| `AD_CALLS_PER_SEC` | optional | `10` | Call starts per second. Calls run at the same time; this limits how fast they start. |
| `AD_RETRY_SWEEP_MS` | optional | `1000` | Interval of the sweep that dials the leads that became due (a retry or a hold), updates leads from ended attempts, and completes idle campaigns. |
| `AD_EVENT_SWEEP_MS` | optional | `2000` | The longest wait before the event table is read again, also when a notification is lost. |
| `AD_ATTEMPT_CHECK_MS` | optional | `15000` | Interval of the check of live calls against LiveKit (is the call's participant still in its room). It ends a call whose node died, and it decides a dial whose result was lost. |
| `AD_NODE_CAPACITY` | optional | `50` | The most calls that this node runs at one time. The leader gives each new call to the node with the most free capacity. |
| `AD_CLUSTER_TTL_S` | optional | `10` | The TTL of the node and leader keys in kine. When a node stops without a clean exit, its calls move after this time. A clean stop (SIGTERM) moves them at once. |
| `AD_SESSION_URL` | optional | — | The URL of the agent session stream that the agents reach, for example `http://api:9090`. The node gives it to each agent with a token. Without it, the agents cannot report an outcome, and the dialer settles each call without one. |
| `HOSTNAME` | optional | `orchestrator` | The node name: the node key in kine, and the node of each call attempt. Letters, digits, `-`, `_`, `.`. |
| `RUST_LOG` | optional | `info` | Log filter (`tracing` `EnvFilter` syntax). |

The retry policy is in the code for now (`crates/ad_orchestrator/src/retry.rs`).
A lead gets at most 3 attempts. The wait before the next attempt is 5 minutes
after busy, 30 minutes after no answer, 60 minutes after voicemail, and 15
minutes after a failure, an abandoned call, or an error. No variable changes
the policy yet.

### `api`

| Variable | Need | Default | Meaning |
|---|---|---|---|
| `AD_API_ADDR` | optional | `0.0.0.0:8080` | Listen address. |
| `AD_SESSION_ADDR` | optional | `0.0.0.0:9090` | The listen address of the agent session stream (gRPC). The role serves it only with `DATABASE_URL`. |
| `DATABASE_URL` | optional | in-memory store | Postgres URL. Migrations run at start. |
| `LIVEKIT_API_KEY` | optional | — | LiveKit API key. With `LIVEKIT_API_SECRET` and `DATABASE_URL`, it turns on the LiveKit webhook endpoint. Set both keys or neither. |
| `LIVEKIT_API_SECRET` | optional | — | LiveKit API secret. It verifies the signature of each LiveKit webhook. |
| `RUST_LOG` | optional | `info` | Log filter. |

### `migrate`

| Variable | Need | Default | Meaning |
|---|---|---|---|
| `DATABASE_URL` | required | — | Postgres URL. Postgres also carries the events, the retries, and the breaker window. |
| `RUST_LOG` | optional | `info` | Log filter. |

The `dispatch` role is a stub. It prints a message and exits. It has no
contract yet.

### Logs

Every role writes its logs to stdout and its errors to stderr. It writes no
log files. In a container, read them with `docker logs <container>`. In
Kubernetes, use `kubectl logs`. Set `RUST_LOG=debug` for more detail, or
`RUST_LOG=info,agentdialer=debug` for the binary only.

### Run locally (all in Docker)

```bash
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

For a full stack (Kamailio, a NAT, the carrier simulator, LiveKit, and both
test agents), use `e2e/docker-compose.cluster.yml`. `e2e/run_e2e.sh` drives
it.

## End-to-end tests

The `e2e/` directory holds the end-to-end tests. Each test starts a real
stack, checks its behavior, and stops the stack. The scripts need only bash
and the Docker CLI on the host. Every other tool (HTTP polls, JSON, `psql`,
`etcdctl`, SIPp) runs in a container (`e2e/lib.sh`). Each script keeps its
logs in `e2e/.out/` (git ignores it). The checks treat Postgres and kine as
managed services: they use the database URL and the kine endpoint, not a
shell in a container.

**The carrier simulator.** The carrier in the tests is `e2e/carrier-sim`, a
SIP carrier simulator on diago (`github.com/emiago/diago`). It acts like
Twilio Elastic SIP Trunking termination:

- a 407 digest challenge for each new INVITE;
- E.164 and caller-ID checks;
- symmetric RTP (it sends the audio back to the address that it comes from);
- behaviors for each number: echo, tone, busy, decline, unavailable, failure,
  and no answer.

Its API (`/calls`) reports what it saw on each call, and the tests check that
API.

**The cluster stack.** The cluster call tests run on
`e2e/docker-compose.cluster.yml`: one bridge network, no host networking, with
Kamailio as the SIP edge. The carrier is on the "internet", behind a NAT
container that changes the source port at random, as the AWS NAT gateway does.

**The tests:**

- `e2e/run_lkcloud_e2e.sh`: the quickstart, as a live test against LiveKit
  Cloud. It runs setup, starts the quickstart stack, makes a campaign and a
  lead through the API, and places a real call over the internet to the
  project's LiveKit number. It checks the digest auth, that each agent hears
  the other's tone, and that our BYE ends the call. It also places a
  reference call from LiveKit's outbound trunk. It needs the LiveKit Cloud
  keys and the SIP URI in `.env`.
- `e2e/run_twilio_e2e.sh`: "Call through Twilio", as a test. It starts the
  quickstart stack, makes a campaign and a lead through the API, and places a
  real call through Twilio to its free "Play" number. It checks Twilio's
  audio at the agent and Twilio's own record of the call. It needs `.env` and
  an account with Elastic SIP Trunking. On a Trial account, set
  `TWILIO_E2E_TO` to a verified number. `--sim` runs the same stack and steps
  with the simulator in Twilio's place, with no credentials.
- `e2e/run_twilio_c2c_e2e.sh`: "Click-to-call through Twilio", as a test.
  With `--sim`, the simulator is Twilio's Calls API and the phones. It has
  four leads:
  - two phones answer, and the INVITEs reach the node through Kamailio over
    TCP;
  - the audio crosses both ways;
  - one phone hangs up, and the agent leaves the other call;
  - a busy lead and a lead that does not answer get their outcomes.

  Without `--sim`, the script calls through Twilio and an ngrok tunnel (not
  run yet).
- `e2e/run_e2e.sh`: one node carries the calls itself. A real LiveKit agent
  (explicit dispatch) and a real pipecat bot (a start request) each take a
  call with each dial order. Customer first, the agent joins after the
  answer. Agent first, it joins before the dial.
  - A fifth campaign has a bot whose start request takes 4 s and that does
    not join. Its call answers, and the node hangs up 2 s later as
    `abandoned`. A late join cannot open the closed room again. The
    campaign's next lead then dials agent first, so no customer waits.
  - The INVITE goes through Kamailio. The carrier challenges it with 407, and
    the node answers with the trunk credentials and the caller ID.
  - The agent's tone goes out as RTP through the NAT and comes back from the
    carrier, and the agent hears it.
  - The carrier's BYE comes back through Kamailio and ends the attempt. The
    leader settles each lead, closes the room, and completes the campaigns.
  - Each agent reports an outcome on the session stream. The test checks a
    `SALE` with a summary, a DNC request (the number goes on the campaign's
    DNC list, from the agent), a callback in one day (the lead waits with
    priority 50), and a call with no outcome (the dialer waits 3 s, then
    settles it).
  - A third campaign checks that the carrier's busy, failure, and no-answer
    replies give the matching outcomes. Its fourth lead has a number on the
    system DNC list, in another form. The leader's gate stops it before the
    dial, and the carrier never sees it.
  - A pacing campaign has a bot whose start request takes 700 ms. Through
    the API, the test sets the pool's target to 300 ms, its back-off to
    600 ms, and its first limit to 2. The slow joins halve the limit to 1. No
    claim goes above the limit, no call is abandoned, and the faster pools
    grow their limit above 1.
  - A pool whose bot server refuses each start (HTTP 503) trips its breaker
    after five failures in a row, and the leader pauses the campaign.
- `e2e/run_cluster_e2e.sh`: three nodes share 24 calls through kine, with both
  agent runtimes. The LiveKit campaign dials customer first, and the pipecat
  campaign agent first. The test kills a node, and then the leader. It checks
  these points:
  - the dead node's calls end without a second dial, and a new leader takes
    over;
  - the audio crossed both ways on every call that the carrier ended;
  - every lead is dialed one time;
  - each agent pool starts at a limit of 1 and grows it, no claim goes above
    the limit, and no call is abandoned because an agent was slow;
  - the OS clocks of two nodes are wrong (11 min behind and 3 min ahead, with
    a preloaded shim, `e2e/clock-offset`). Each node logs its offset, and
    every event time that a node saves is within seconds of the time at which
    Postgres saved it. The node that is ahead still connects calls, because
    it dates its LiveKit tokens by the Postgres time.
- `e2e/run_events_e2e.sh`: a real Postgres, with a trunk that the node cannot
  reach (every dial fails before the INVITE leaves). Dials follow each other
  through LISTEN/NOTIFY within the capacity. The breaker pauses the campaign.
  Each retry is stored on the lead, and a due retry is dialed again. A resume
  through the API resets the breaker.
- `e2e/run_webrtc_e2e.sh`: our WebRTC participant carries audio both ways
  into a LiveKit room with the agent, with no `livekit/sip`.
- `spike/livekit/run_mvp_test.sh`: a real LiveKit agent in a room as a
  subscriber, and then a SIP call into the room (two-way audio).

**CI.** CI runs the `e2e/` scripts on every change
(`.github/workflows/ci.yml`):

- `run_twilio_e2e.sh --sim` runs the quickstart stack with the carrier
  simulator, so the stack keeps working without credentials.
- `run_twilio_c2c_e2e.sh --sim` runs click-to-call with the simulator.
- `run_lkcloud_e2e.sh` (the LiveKit Cloud quickstart) runs when the
  repository has the secrets `LIVEKIT_URL`, `LIVEKIT_API_KEY`,
  `LIVEKIT_API_SECRET`, and `LIVEKIT_SIP_URI`.
- The live Twilio test runs when the repository has the secrets `TWILIO_SID`
  and `TWILIO_SECRET` (and, for a Standard key, `TWILIO_ACCOUNT_SID`).
- CI does not run the spike script.

The test tier also runs `crates/ad_sip/tests/twilio.rs` on every change: our
SIP user agent against an in-process callee that answers like Twilio. It
checks the 407 digest, the caller ID in From, the E.164 Request-URI, and
`Max-Forwards: 70`. It also checks that bad credentials end the call after
one retry.

## Deployment

`deploy/helm/agentdialer` is the Helm chart.

- The orchestrator replicas are the cluster nodes (no Service). Each node
  carries its own calls as a normal pod (no host networking). Each node
  advertises its pod IP (`AD_SIP_ADVERTISE_IP` from `status.podIP`).
- The API has a Service and an HPA. The Service has the HTTP port and the
  session port (`api.sessionPort`, default 9090). The nodes give the agents
  `http://<release>-api:9090`. Set `api.sessionUrl` when your agents reach
  the API at another URL.
- A migrate Job runs as a pre-upgrade hook.
- Set `database`, `livekit`, and `sip` (`sip.trunk`, and `sip.proxy` for
  Kamailio) in the values. The database and LiveKit values take an existing
  Secret, or an inline value for development.
- The values `orchestrator.retrySweepMs`, `orchestrator.eventSweepMs`, and
  `orchestrator.attemptCheckMs` set `AD_RETRY_SWEEP_MS`, `AD_EVENT_SWEEP_MS`,
  and `AD_ATTEMPT_CHECK_MS`.
- The chart does not deploy Kamailio or kine.

The roles take the time from Postgres, so the managed Postgres must keep a
correct clock (RDS does). Do not run a time daemon in a pod. A node logs
`the OS clock is off` when its OS clock is more than 250 ms from Postgres.
The calls and the LiveKit tokens do not use the OS clock, but the log lines
do. Fix the clock of the node, so that its log times agree with the
database.

## Status

What works, and how we know:

| Part | State | Proof |
|---|---|---|
| The call path: the node carries SIP and RTP behind Kamailio, and bridges the audio into the room (D-9) | Works through a NAT with a latching carrier | `e2e/run_e2e.sh`, `e2e/run_cluster_e2e.sh` |
| The quickstart on LiveKit Cloud SIP | Works over the internet, through Docker's NAT and a home NAT (2026-10-24). No call went through the public phone network. | `e2e/run_lkcloud_e2e.sh` |
| Twilio Elastic SIP Trunking | Works with the simulator. Not run against Twilio: Twilio refused trunks for the test account (`GAPS.md` CARRIER-1). | `e2e/run_twilio_e2e.sh --sim`, `crates/ad_sip/tests/twilio.rs` |
| Twilio click-to-call | Works with the simulator. The live run needs an upgraded account (`GAPS.md` CARRIER-2). | `e2e/run_twilio_c2c_e2e.sh --sim` |
| The cluster: nodes, leader, call assignment over kine, fencing in Postgres | Works with three nodes in Docker Compose | `e2e/run_cluster_e2e.sh` |
| Agents: a LiveKit agent or a pipecat bot, both dial orders (D-2) | Works with a real agent of each kind | `e2e/run_e2e.sh` |
| The abandonment ledger and the 3% cap (`GAPS.md` COMP-10) | Works. No answering-machine detection: every answer counts as a human answer. | `e2e/run_e2e.sh` |
| Pacing by join time (`GAPS.md` DIAL-9, D-10) | Works, with every limit changeable at run time. The pool breaker pauses a campaign after five failed agent starts in a row. | `e2e/run_e2e.sh`, `e2e/run_cluster_e2e.sh` |
| Time from Postgres (D-11) | Works, the LiveKit tokens too (`GAPS.md` OPS-9) | `e2e/run_cluster_e2e.sh` |
| The agent's outcome on the session stream (`GAPS.md` AGNT-2, D-6): dispositions, callbacks, DNC requests | Works with both reference agents | `e2e/run_e2e.sh`, `crates/ad_session`, `crates/ad_db/tests/pg_outcomes.rs` |
| Phone numbers in E.164 at the lead load, and the DNC list in Postgres (`GAPS.md` COMP-9, COMP-7) | Works | `crates/ad_contact`, `crates/ad_api/tests/outcomes_api.rs`, `e2e/run_e2e.sh` |
| The event layer: LISTEN/NOTIFY, the breaker, retries on the lead row, campaign completion | Works | `e2e/run_events_e2e.sh` |
| Call attempts: the state machine saved with its events | Works | `crates/ad_db/tests/pg_attempts.rs`, the e2e tests |
| The WebRTC participant | Works: two-way audio | `e2e/run_webrtc_e2e.sh` |
| Campaigns, leads, the hopper, the compliance engine | The engines work. Production passes a 24-hour window (`GAPS.md` COMP-1). | crate tests, API contract tests |
| The routing engine | Built as a library. The dial path does not use it yet. | `ad_routing` tests |

Known limits of the call path (`PLAN.md` §0.0, "Open"):

- RTP latching follows the rules of `rustrtc`, and one marked packet can win.
  The patch against RTP bleed is not done.
- We did not test a resent 200 yet. Session timers, the trunk OPTIONS check,
  the `public` media mode, and DTMF are not built.
- A node that dies drops its live calls. Nothing sends BYE to the carrier for
  them yet.
- A node used about half a core at two calls in the cluster test. Profile it
  before a capacity plan (OPS-8).

Other known limits (`GAPS.md` has the full list, by ID):

- **Abandonment.** There is no answering-machine detection, so every answer
  counts as a human answer. All campaigns use the US TSR rule; a campaign
  cannot select a jurisdiction yet. An abandoned call gets no recorded
  message yet.
- **Compliance data.** Production dialing uses a 24-hour call window. No
  table stores call times or holidays yet (COMP-1). The DNC list holds the
  entries of the API and of the agents; no national list is loaded (COMP-4).
  A lead without an explicit IANA timezone does not dial (COMP-2).
- **Dial rate.** A SIPp benchmark (2026-10-21) measured 50.0 attempts per
  second at a set rate of 50 cps, with 84 live calls at the peak (one node,
  before the cluster). The leader paces the whole cluster. There is no rate
  for each campaign or each trunk yet (DIAL-3, DIAL-7).
- **Outcome.** The agent's outcome decides the lead. The dialer does not
  transfer a call yet (`transfer_to` is only kept), and the session stream
  has no traces (OTLP) yet (D-6).
- **Holds.** The dialer checks a lead that the calling window holds again
  after 15 minutes. It does not compute the next window opening yet
  (BETTER-3).
- **Events.** The publish lock puts event inserts in a queue, and
  `campaign_events` and `attempt_events` have no retention yet (ORCH-2).
- **Cluster.** kine is not etcd (no lease KeepAlive, no Election, no
  keys-only reads), and one kine replica is a single point of failure. A dead
  node's attempts move after the TTL (default 10 s). With agent first, the
  node polls LiveKit every 250 ms to find the agent (OPS-8).
- **Experimental endpoint.** `POST /v1/campaigns/:id/dispatch/run-batch` is
  experimental. It uses a stub initiator, and its response says so
  (`experimental: true`).

Not built yet:

- a dispositions UI and reports, an A/B traffic split, and a full
  `livekit-agents` brain (STT, LLM, TTS);
- authentication of the HTTP API (API-1);
- OpenTelemetry on the session stream (decision D-6).
