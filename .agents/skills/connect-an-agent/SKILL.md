---
name: connect-an-agent
description: >
  Teaches how to connect an AI voice agent to AgentDialer: a LiveKit Agents
  worker or a pipecat bot. It gives the contract between the dialer and the
  agent: what the agent receives for each call, which participant is the
  callee, how a call ends, the 2-second limit after the answer, and how the
  agent reports the outcome of each call (a disposition, a callback, or a DNC
  request) on the session stream. Use this skill when you build, change, or
  debug the agent that talks on the calls, when calls end as abandoned because
  the agent joins late, or when the dialer does not get the agent's outcome.
license: AGPL-3.0-or-later
metadata:
  project: agentdialer
  tier: project
---

# Connect an agent to AgentDialer

The dialer places each call. The agent does not dial. For each call, the dialer
makes a LiveKit room, puts the callee into it, and starts one agent in it. The
agent talks to the callee in the room. Then the agent tells the dialer how the
call went, on a gRPC stream to the `api` role.

```
the node starts the agent (LiveKit dispatch or pipecat start request)
  -> the agent joins the room and publishes its audio (2 s limit)
  -> the agent opens its session stream, in the background
  -> the talk -> the call ends
  -> the agent sends one Outcome -> the dialer answers Ack
```

Read `README.md` "Architecture" for the call path. The reference agents are
`e2e/agents/livekit/agent.py` and `e2e/agents/pipecat/bot.py`. They play a
tone, use no AI service, and report an outcome with the client
`e2e/agents/session/agent_session.py`. Start from them.

## The two runtimes

Put the agent in the campaign's `agent` object (the `run-campaigns` skill has
all the fields):

```json
{"runtime": "livekit", "agent_name": "sales", "metadata": {"script": "renewal-v1"}}
{"runtime": "pipecat", "start_url": "http://bots:7860/start", "dial_order": "agent_first"}
```

**`livekit`: a LiveKit Agents worker.**

- Register the worker with an `agent_name`. The dialer starts it with an
  explicit dispatch (`CreateDispatch`) into the call's room.
- The job metadata (`ctx.job.metadata`) is the call facts as a JSON string.
- The agent joins as `agent-AJ_...`, with the kind AGENT.

**`pipecat`: a pipecat bot server.**

- The dialer sends `POST <start_url>` with this body:

  ```json
  {"url": "<LiveKit URL>", "room_name": "<room>", "token": "<JWT>",
   "identity": "agent-attempt-42", "call": {"...": "the call facts"}}
  ```

- Answer 2xx to accept the call. Any other answer, or no answer in 10 s,
  fails the agent start.
- Join with pipecat's `LiveKitTransport(url=..., token=..., room_name=...)`.
  The token lets the bot join only that room, as an agent, for 1 hour.

## The call facts

Both runtimes get the same JSON, so one agent design serves both:

```json
{
  "attempt_id": 42,
  "campaign_id": 7,
  "room": "campaign-42",
  "callee_identity": "sip-attempt-42",
  "lead": {"id": 9, "phone_number": "+12025550123",
           "context": {"first_name": "Ada", "plan": "gold"}},
  "metadata": {"script": "renewal-v1"},
  "session": {"url": "http://api:9090", "token": "9f2c...e1"}
}
```

- `lead.context` is the `context` map of the lead (the `run-campaigns`
  skill, "Load leads").
- `metadata` is the `metadata` of the campaign's agent. Use it to select a
  script or a prompt.
- `callee_identity` names the callee's participant in the room.
- `session` is the agent's session stream for this call ("Report the
  outcome"). It is absent when the node has no `AD_SESSION_URL`.

## In the room

- The callee is the participant `callee_identity`. It publishes one audio
  track with the callee's voice, and it plays the agent's audio to the phone.
- The phone side is G.711 at 8 kHz. Sound above 4 kHz does not reach the
  phone.
- The dialer counts every participant other than the callee as the agent.
  Do not add other participants to the call room.
- Customer first: the callee is in the room when the agent joins, or it joins
  at the same time. Greet the callee when its audio track comes.
- Agent first: the agent joins first. The callee joins only when the phone
  answers. The phone can ring for up to 30 s. If the call fails (busy, no
  answer), the dialer closes the room. Wait for the callee, and handle the
  close of the room.

## How a call ends

| Event | What happens |
|---|---|
| The agent leaves the room | The node sends BYE. The outcome is `completed`. To hang up, leave the room. |
| The callee hangs up | The callee's participant leaves the room. Leave the room, and end the job. |
| No RTP comes for 30 s | The node ends the call. |
| The attempt ends | The leader closes the room. Your agent gets a disconnect. |

## Report the outcome

Only the agent knows how the call went: a sale, "not interested", "call me
tomorrow", or "do not call me again". The agent tells the dialer on the
session stream. The agent never writes to the database.

The stream is gRPC, `agentdialer.session.v1.Session/Open`, in
`proto/agentdialer/session/v1/session.proto`. The `api` role serves it
(`AD_SESSION_ADDR`, default port 9090). Open one stream for each call:

1. Join the room and publish your audio first. Then connect to `session.url`
   in the background, and send `Hello {attempt_id, token, agent}`.
2. The dialer answers `Welcome`. It lists the disposition codes of the
   campaign, each with its flags. A wrong token closes the stream with
   `UNAUTHENTICATED`.
3. When you know the result, send one `Outcome`. The dialer answers `Ack`, or
   `Rejected {reason}`. After `Rejected`, fix the outcome and send it again.
4. Optional: send `Event {name, json}` or `Log {level, message}`. The dialer
   writes them to its log and does not answer.

The fields of `Outcome`:

| Field | Meaning |
|---|---|
| `schema_version` | `1`. |
| `disposition` | A code from `Welcome`. The system codes are `SALE`, `NI` (not interested), `CALLBK`, `DNC`, `AM` (answering machine), `WN` (wrong number), and `DONE`. A campaign can add its own codes (`PUT /v1/campaigns/:id/dispositions`). |
| `answered_by` | `HUMAN`, `MACHINE`, or `UNKNOWN`. |
| `callback` | `{local_time: "2026-03-02T09:30", zone}`. Needed with `CALLBK`. The zone is IANA; without it, the zone of the lead. It must be 1 minute to 90 days from now. |
| `dnc` | `{scope: "system" or "campaign"}`. The callee asked not to be called again. An empty scope means `system`. `DNC` as the disposition asks for the same. |
| `summary`, `transfer_to`, `disclosures` | Kept with the attempt. The dialer does not transfer calls yet. |

What the dialer does with the outcome:

- A DNC request takes effect at once. The number goes on the DNC list
  (source `agent`), and every waiting lead with that number stops.
- A callback queues the lead at that time, before the other leads.
- `AM`, or `answered_by: MACHINE`, schedules a voicemail retry.
- Any other code settles the lead as `dispositioned`.
- The first outcome stays. The same outcome again gets `Ack` with
  `duplicate`. A different second outcome gets `Rejected`.

You can report after the hang-up. After an answered call that your agent
joined, the dialer waits `outcome_wait_s` (in the campaign's `agent`; default
30, maximum 600) for the outcome. With no outcome in that time, it settles the
lead by the call result.

With Python, use the reference client:

```python
import asyncio
from agent_session import Session

# After the join and the track publish: open the session in the background.
opening = asyncio.create_task(Session.open(call_facts, "my-agent"))

# ... the talk ...

session = await opening                                 # None without a session
if session:
    print(session.codes())                              # ["SALE", "NI", ...]
    answer = await session.report({"disposition": "CALLBK", "answered_by": "human",
                                   "callback": {"local_time": "2026-03-02T09:30"}})
    await session.close()                               # answer: "ack" or "rejected: ..."
```

Build the stubs from the proto with `grpcio-tools`, as the reference
Dockerfiles do. For another language, generate a client from the same proto.

CAUTION: Do not open the session before the join. With customer first, the
callee waits at most 2 s for the agent. A slow connect before the join
abandons the call.

CAUTION: In Docker, set `GRPC_DNS_RESOLVER=native` for Python gRPC. The
default resolver took 3 to 6 s for each connect on the Docker DNS
(2026-10-30). The reference Dockerfiles set it.

## The 2-second limit

CAUTION: With `customer_first` (the default), the agent must be in the room
2 s after the answer. The 2 s include the start request. If the agent is not
in the room, the node hangs up, and the outcome is `abandoned`. The
abandonment ledger counts each one. Above 3%, the campaign dials agent first.

The node sees the agent when LiveKit tells it that the agent joined. In our
runs, that came with the agent's first audio track (2026-10-30: the node saw
the agent 3 ms after its track publish). An agent that connects and publishes
nothing for 2 s abandons the call.

Do these steps to join in time:

- Publish the audio track at once after you connect. Do slow work (the
  session stream, a greeting from an LLM) after the publish or in the
  background.
- LiveKit Agents (the reference uses `livekit-agents==1.8.8`):
  - Run the worker with `start`, not `dev`. In `dev` mode,
    `num_idle_processes` is 0, so each job starts a new process.
  - Keep idle processes ready (`num_idle_processes`). Load the models in
    `setup_fnc`, not in the job.
  - Do not let the worker refuse a job for load. The dialer controls the
    number of calls. The reference sets `load_fnc=lambda: 0.0` and
    `load_threshold=1.0`.
- pipecat:
  - Answer the start request at once. Start the bot in a background task.
  - Load the models when the server starts, not for each call.
- Put the agent near the LiveKit server of the dialer's rooms.

If the agent cannot join in 2 s, use `"dial_order": "agent_first"`. Set
`join_timeout_s` (1 to 300, default 20) to the longest start time of the
agent.

## The agent pool and the pacing

The agents of one `agent_name` (LiveKit) or one `start_url` (pipecat) are one
pool. The dialer learns how many calls the pool can take at one time from the
join times. Fast joins give the pool more calls at one time. A slow join, a
failed start, or no agent at 2 s halves the number. Campaigns with the same
agent share the pool.

`GET /v1/campaigns/:id/pacing` shows the join times of the pool
(`recent.p95_ms`). Use it to measure your agent.

## Connect to the right LiveKit server

The agent must use the LiveKit server of the dialer's rooms (`LIVEKIT_URL` of
the orchestrator). In the quickstart, this is not the LiveKit Cloud project
of the trunk. That project holds the callee's side of the call.

- In the quickstart, that server is `ws://livekit:7880`, with the key
  `devkey` and the secret `devsecret_devsecret_devsecret_32b`. Run your agent
  in a container on the network `agentdialer-quickstart_default`.
- Set `room.auto_create: false` on that server (`e2e/livekit-cluster.yaml`
  does this). Without it, an agent that starts late opens the closed room
  again.

## Test the agent

1. Start the quickstart stack (the `run-campaigns` skill).
2. Start your agent in a container on the stack network.
3. Create a campaign with your `agent_name` or `start_url`, load one lead,
   and start the campaign.
4. Read the result and the join time of each call:

   ```bash
   docker run --rm --network agentdialer-quickstart_default postgres:16-alpine \
     psql postgres://ad:ad@postgres:5432/ad -c \
     "SELECT id, outcome, agent_state, agent_identity,
             agent_ready_at - answered_at AS join_after_answer
        FROM call_attempts WHERE campaign_id = 1 ORDER BY id"
   ```

5. Read the outcome that the agent reported:

   ```bash
   scripts/api.sh GET /v1/campaigns/1/attempts   # disposition and agent_outcome of each attempt
   ```

`e2e/run_e2e.sh` tests both reference agents with both dial orders. It also
tests a disposition, a DNC request, a callback, and a call with no outcome.
Run it when you change how the dialer starts an agent or reads an outcome.

## Not built yet

- No DTMF, no transfer, and no recording. The dialer keeps `transfer_to`,
  but it does not transfer the call.
- The session stream has no traces (OTLP) yet.
- No answering-machine detection. Each answer counts as a human answer.

## Change the dialer side

The adapters are in `crates/ad_agent/` (`lib.rs`: `Agents::start`,
`call_metadata`; `spec.rs`: `AgentSpec`). To add a runtime, add one
`AgentSpec` variant and one match arm in `Agents::start`. The spec is data
(`campaigns.agent`), so a new runtime needs no migration. Ship it with tests
(the `vertical-slice` skill).

The outcome path: `proto/agentdialer/session/v1/session.proto` (the
contract), `crates/ad_session/` (the gRPC server), `crates/ad_disposition/`
(the catalog and the check of a report, pure), `crates/ad_db/src/outcomes.rs`
(the store), and `ad_orchestrator::settle_with` (the lead's next state, pure).
A change to the proto must keep old agents working: add fields, never
renumber them.
