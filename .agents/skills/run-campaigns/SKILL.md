---
name: run-campaigns
description: >
  Teaches how to use AgentDialer to place calls. Start the stack in Docker
  Compose, connect a trunk (LiveKit Cloud SIP, Twilio Elastic SIP Trunking, or
  Twilio click-to-call), and run a campaign through the API: with a ready
  Python recipe (examples/), or with your own flow on the Python client. Then
  change the pacing, read the result of each call, and find why a call failed.
  Use this skill when someone wants to run AgentDialer, place calls, or operate
  a campaign. To build the agent that talks on the calls, use the
  connect-an-agent skill. To change the code, use agentdialer-overview.
license: AGPL-3.0-or-later
metadata:
  project: agentdialer
  tier: project
---

# Run campaigns with AgentDialer

This skill teaches you to place calls with AgentDialer. You use the system
through its HTTP API only. `README.md` has each step in full ("The API" lists
each endpoint). This skill puts the API, the decisions, and the checks in one
place.

## Safety rules

- Run every tool in a container. The host needs only bash, git, and the
  Docker CLI. The `examples` service runs the Python recipes and the client
  (`examples/agentdialer.py`) in a container on the stack network.
- Read and change the data only through the API. Do not write to Postgres.
- Do not print a value from `.env` or from a `.env.*` file. These files hold
  keys and passwords. Show only a prefix or a length.
- A call to a real number rings a real phone and costs money. Before you put a
  real number in a lead, get the approval of the person who asked for the
  calls. Use a free test target when you can:
  - the LiveKit number of the project (no phone rings);
  - Twilio's "Play" number `+16504894546` (only through a trunk);
  - the carrier simulator (`e2e/carrier-sim`).
- Use `e2e/livekit-cloud/run.sh` for LiveKit Cloud, and `e2e/twilio/run.sh`
  for Twilio. Do not send one-off requests to these services.
- Stop each stack when you finish (`down -v`).

CAUTION: Do not use AgentDialer for production calls to real people yet.
Production dialing uses a 24-hour call window (`GAPS.md` COMP-1). The DNC list
holds only the entries that you and the agents add. No national list is
loaded (COMP-8). The API has no authentication (API-1), so do not put it on a
public network.

## How a lead moves

```
new or queued -> the leader claims it (in_call)
  -> the gate: the DNC list, the timezone, the calling window
  -> the dial -> the call ends -> the agent reports, or not
  -> the leader settles it: dispositioned, done, dnc, blocked,
     or queued for a retry or a callback
```

README "Architecture" has a diagram for each step: "How the leader wakes",
"Before the dial: the claim and the gate", "One call, step by step", and
"After the call: the settlement".

The dialer does not poll. Each change through the API wakes the leader: a
start, new leads, a new cap, agent, `retry` setting, or pacing. A waiting
lead dials at its due time. On LiveKit Cloud, a lead that was added to a
running campaign dialed 1.7 s after the add.

## 1. Select the trunk

| You want | Use | README section |
|---|---|---|
| The first call in a few minutes, with no phone that rings | LiveKit Cloud SIP and a LiveKit phone number | "Quickstart: place calls through LiveKit Cloud SIP" |
| Calls to real phones through a SIP trunk | Twilio Elastic SIP Trunking (an upgraded account) | "Call through Twilio" |
| Twilio calls the phone first, and then it calls the dialer | Twilio click-to-call (an upgraded account and a SIP address that Twilio can reach) | "Click-to-call through Twilio" |

Run the setup of the trunk one time. It writes an env file for the stack. Git
ignores the env files.

```bash
e2e/livekit-cloud/run.sh setup    # writes .env.lkcloud
e2e/twilio/run.sh setup           # writes .env.twilio
```

Click-to-call has its own script, `e2e/run_twilio_c2c_e2e.sh`. Read its README
section first.

## 2. Start the stack

```bash
# LiveKit Cloud SIP:
QS="docker compose -f docker-compose.quickstart.yml -f docker-compose.livekit-cloud.yml --env-file .env --env-file .env.lkcloud"
# Or Twilio Elastic SIP Trunking:
QS="docker compose -f docker-compose.quickstart.yml --env-file .env.twilio"

$QS up -d --build
RUN="$QS run --rm examples"
$RUN agentdialer.py GET /health   # {"status":"ok", ...} when the API is ready
```

The API is `http://api:8080` on the stack network, and `127.0.0.1:8080` on
the host. The stack has two demo agents: the LiveKit agent `demo`, and the
pipecat bot at `http://pipecat-bot:8089/start`. Each plays a tone and leaves
after 30 s.

## 3. Run a recipe, or write a flow

Each recipe in `examples/` runs one kind of campaign through the API, and
checks the result. Run one with `$RUN <recipe>.py`. Add `--to <number>` to
call a number that is not `LKCLOUD_NUMBER` (the Twilio stack needs it).

| Recipe | It shows |
|---|---|
| `first_call.py` | One customer-first call. The agent reports `DONE`. |
| `agent_first.py` | The agent joins, and then the dialer dials. |
| `outcomes.py` | `SALE` with a summary, a callback that calls again in 20 s, no outcome, `NI`. |
| `dnc.py` | A DNC entry from the API, and a DNC request from the agent. |
| `dispositions.py` | The campaign's own codes. An unknown code is refused. |
| `pacing.py` | 6 leads, cap 3, `k` grows. The cap and the mode change while it runs. |
| `pipecat.py` | A pipecat bot as the agent. |
| `changes.py` | A lead added while the campaign runs is called at once. A `retry` change applies to the next outcome. |

The demo agents take two controls from the lead's `context`: `outcome` (the
JSON text of the outcome to report: an object, a list with one outcome for
each call, or `null`) and `talk_s` (the agent leaves after this many seconds).
`examples/demo.py` builds them (`demo_lead`, `outcome`). A real agent ignores
these keys.

To write a flow, use the client. It has one method for each endpoint:

```python
from agentdialer import Dialer
api = Dialer()                       # AD_API_URL, else http://127.0.0.1:8080
c = api.create_campaign("renewals", {"runtime": "livekit", "agent_name": "demo"}, concurrency_cap=2)
api.add_leads(c["id"], [{"phone_number": "+12025550123", "timezone": "America/New_York"}])
api.start(c["id"])                   # start, then sync_complete
api.wait_until_done(c["id"], on_attempt=print)
```

Put the file in `examples/`, and run it with `$RUN my_flow.py`. For one
request, give the method, the path, and the body:
`$RUN agentdialer.py PATCH /v1/campaigns/1 '{"concurrency_cap": 4}'`.

CAUTION: When you change a recipe, run `e2e/run_twilio_e2e.sh --sim`. It runs
every recipe. A recipe that fails breaks the quickstart.

## 4. Create or change a campaign

`POST /v1/campaigns`:

| Field | Need | Meaning |
|---|---|---|
| `org_id` | required | The organization, as an integer. Use `1`. |
| `name` | required | Must not be empty. |
| `concurrency_cap` | optional | The most live calls of the campaign at one time. Default 10. |
| `agent` | optional | The agent of each call (see below). Give each campaign an agent. |
| `retry` | optional | The retry settings that differ from the defaults (section 9). |

The `agent` object (the `connect-an-agent` skill has the full contract):

| Field | Meaning |
|---|---|
| `runtime` | `livekit` or `pipecat`. |
| `agent_name` | `livekit` only: the agent name of your LiveKit Agents worker. |
| `start_url` | `pipecat` only: the `http` or `https` URL of your bot server's start endpoint. |
| `dial_order` | `customer_first` (the default) or `agent_first`. |
| `join_timeout_s` | `agent_first`: how long the dialer waits for the agent before it dials. 1 to 300, default 20. |
| `outcome_wait_s` | How long the dialer waits for the agent's outcome after an answered call ends. 0 to 600, default 30. |
| `metadata` | Any JSON. The agent gets it with each call. |

The API refuses an unknown field in `agent` (HTTP 400).

```python
c = api.create_campaign("first-calls", {"runtime": "livekit", "agent_name": "demo",
                                        "metadata": {"script": "renewal-v1"}}, concurrency_cap=2)
api.update_campaign(c["id"], concurrency_cap=4)   # PATCH /v1/campaigns/:id
```

`PATCH /v1/campaigns/:id` changes `name`, `concurrency_cap`, `agent` (the
whole object), or `retry` (only the keys that you give), in any state. It
does not change the state. The next claim and the next settlement use the
new values.

## 5. Load leads

`POST /v1/campaigns/:id/leads` with `{"leads": [...], "default_region": "US"}`.
`default_region` (ISO 3166) is the country of a number without a country code.

| Field | Need | Meaning |
|---|---|---|
| `phone_number` | required | Any common form: `+1 202 555 0123`, or `(202) 555-0123` with a `default_region`. The dialer keeps it in E.164 (`+12025550123`). The number must be one that the numbering plan assigns. |
| `timezone` | required to dial | An IANA name, for example `America/New_York`. A lead without one does not dial, because compliance fails closed (COMP-2). |
| `state`, `postal_code` | optional | Saved with the lead. A US or Canadian postal code gets its standard form (`12345-6789`, `K1A 0B1`). |
| `context` | optional | A map of strings. The agent gets it as `lead.context`. Put the name of the person and the facts that the agent needs here. |

```python
api.add_leads(c["id"], [{"phone_number": "+12025550123", "timezone": "America/New_York",
                         "context": {"first_name": "Ada", "plan": "gold"}}])
```

If one lead is bad, the API loads no lead. It answers HTTP 400 with a report
of each bad lead: `{"error", "rejected": [{"index", "phone_number",
"reason"}]}`. Correct those leads, and send the list again.

The phone numbers and the postal codes go through one module,
`crates/ad_contact`. To use another phone library, change it there.

## 6. Start, pause, and resume

`POST /v1/campaigns/:id/transition` with `{"transition": "<name>"}`:

| Transition | From | To |
|---|---|---|
| `start` | `draft` | `syncing` |
| `sync_complete` | `syncing` | `running`. The dialer starts to dial. |
| `pause` | `running` | `paused`. The live calls continue. No new call starts. |
| `resume` | `paused` | `running`. It also resets the breaker. |
| `complete` | any state that is not terminal | `completed` |
| `fail` | any state that is not terminal | `failed` |

A transition from a wrong state answers HTTP 409. The dialer completes a
running campaign when no lead has work left. `api.start(id)` sends `start`
and `sync_complete`. `api.wait_until_done(id)` waits for `completed`.

## 7. Watch the calls and read the results

```bash
$QS logs -f node                              # the node and the leader
$RUN agentdialer.py GET /v1/campaigns/1        # the state of the campaign
$RUN agentdialer.py GET /v1/campaigns/1/leads  # the status of each lead
```

Useful log lines of the node: `call assigned`, `lead not dialed`,
`a participant joined the room`, `agent in the room after the answer`,
`call ended`, `the call waits for the agent's outcome`, `attempt settled`,
`abandonment rate above the cap; agent
first`, `breaker open; campaign paused`, `campaign completed`, and `the OS
clock is off`. The `api` log shows `agent session open` and `agent outcome`
for each agent that reports.

The status of a lead:

| Status | Meaning |
|---|---|
| `new` | Not dialed yet. |
| `queued` | Waits for a dial. `next_eligible_at` holds the time of a retry or a hold, and `retry_reason` says why (`busy`, `callback`, ...). A callback has `priority` 50. |
| `in_call` | Has a live call. |
| `dispositioned`, `done` | Dialed. No more calls. |
| `dnc` | On a do-not-call list. |
| `blocked` | Cannot dial until its data changes, for example a lead with no timezone. |

The calls and the events of a campaign, in pages (`?after=<id>&limit=<n>`):

```bash
$RUN agentdialer.py GET /v1/campaigns/1/attempts             # each call
$RUN agentdialer.py GET '/v1/campaigns/1/attempts?lead_id=7'  # the calls of one lead
$RUN agentdialer.py GET /v1/campaigns/1/events               # for example agent_pool_tripped
```

Each call has its `outcome`, `sip_code`, `detail`, `disposition`,
`agent_outcome`, `dial_order` (the order that this call used), `room`,
`agent_identity`, `agent_state`, the pool window and limit (`pool_window`,
`pool_limit`), and the times: `dialed_at`, `answered_at`, `agent_started_at`,
`agent_ready_at`, `ended_at`, `outcome_at`, and `settled_at`.
`api.attempts(id)` reads every page.

The outcome of a call:

| Outcome | Meaning | Next step |
|---|---|---|
| `completed` | The callee answered, and the call ended. | The agent's outcome decides (below). |
| `busy`, `no_answer` | The SIP code of the callee. | The dialer retries (see below). |
| `declined` | The callee refused the call. | None. The dialer does not call the lead again. |
| `invalid_number` | The trunk refused the number. | Correct the number. The lead is `done`. |
| `failed` | The trunk or the carrier failed the call. `sip_code` and `detail` tell why. | Read "If a call fails" in the README section of the trunk. |
| `abandoned` | The callee answered, and no agent was in the room 2 s later. | Make the agent faster, or use `agent_first`. See section 10. |
| `setup_error` | The dial request did not reach the trunk. No call exists. | Read the node log. The dialer retries. |
| `uncertain` | The dialer did not learn the result. | Read the node log. The dialer retries. |
| `not_dialed` | The dialer did not dial. `detail` gives the reason. | See the next table. |

The reason of a `not_dialed` call:

| Reason | What the dialer does |
|---|---|
| The calling window | Tries the lead again after 15 min. |
| No agent (the agent did not start or join before the dial) | Tries the lead again after 1 min. |
| No timezone | Sets the lead to `blocked`. |
| DNC | Sets the lead to `dnc`. |
| A rule or a limit excludes the lead | Sets the lead to `done`. |

The agent reports the result of the talk on the session stream (the
`connect-an-agent` skill, "Report the outcome"). The dialer keeps it in
`disposition` and `agent_outcome`:

| The agent reports | The lead |
|---|---|
| A DNC request, or the code `DNC` | `dnc`. The number goes on the DNC list. |
| `CALLBK` with a time | `queued` until that time, before the other leads. |
| `AM`, or a machine answer | A voicemail retry. |
| Any other code | `dispositioned`. |
| Nothing in `outcome_wait_s` | Settled by the call result. |

If an attempt has no `disposition`, read the `api` log. No `agent session
open` line means that the agent did not reach `AD_SESSION_URL`.

The disposition codes: `api.dispositions(id)`. To add campaign codes, send
a list of `{"code", "name", "flags"}` with `api.set_dispositions(id, codes)`
(`dispositions.py`). The flags
are `human_answered`, `sale`, `customer_contact`, `not_interested`,
`unworkable`, `scheduled_callback`, `completed`, `answering_machine`, and
`dnc`. A campaign code must not use a system code.

## 8. The DNC list

| Request | What it does |
|---|---|
| `POST /v1/dnc` `{"phone_number", "scope", "campaign_id", "region", "note"}` | Adds a number. `scope` is `system` (the default, every campaign) or `campaign` (with `campaign_id`). Every waiting lead with that number stops at once. The reply gives the entry's `id`. |
| `GET /v1/dnc?campaign_id=1` | The system list and the list of campaign 1, newest first. |
| `DELETE /v1/dnc/:id` | Removes an entry. A lead that stopped stays `dnc`. |

The dialer checks the list again before each dial. If it cannot read the
list, it does not dial (fail closed).

## 9. Retries and the breaker

- The campaign's `retry` settings give the wait before the next call:
  `busy_s` (300), `no_answer_s` (1800), `voicemail_s` (3600, after a machine
  answer), `failed_s` (900), `error_s` (900, after `setup_error` or
  `uncertain`), and `abandoned_s` (900). After `max_calls` calls (3), the lead
  is `dispositioned`. `callback_min_s` (60) is the soonest callback that an
  agent can ask for.
- Set them at the create, or change them at any time. Give only the keys to
  change; the others keep their values. The next settlement uses them:

  ```python
  api.create_campaign("renewals", agent, retry={"busy_s": 120, "max_calls": 5})
  api.update_campaign(c["id"], retry={"no_answer_s": 3600})
  ```

- The leader dials a waiting lead at its due time (`next_eligible_at`), and
  it settles a call when its outcome wait ends. It does not poll.
- A change of `retry` applies to the next settlement. A lead that waits
  already keeps its due time.
- `completed` and `declined` end the lead (`dispositioned`), unless the agent
  asked for a callback or a voicemail retry. There is no recycle rule yet.
- The retry of an `abandoned` lead dials agent first.
- The breaker pauses a campaign when at least 5 calls ended in the last
  2 min, and more than half of them are `failed`, `setup_error`, or
  `uncertain`. Correct the cause, and then send `resume`.

## 10. Select the dial order and the pacing

**The dial order** (`agent.dial_order`):

- Use `customer_first` (the default) when your agent can be in the room 2 s
  after the answer. An agent starts only for an answered call.
- Use `agent_first` when your agent needs more than 2 s to join, or when you
  cannot accept an abandoned call. Each call that rings then holds an agent.
  An agent that leaves before the answer makes the call `abandoned`.
- `GET /v1/campaigns/:id/abandonment` shows the answered and abandoned calls
  of the last 30 days. When the rate is above 3%, the campaign dials agent
  first until the rate is at or under 3%.

**The pacing.** The dialer learns how many calls each agent pool can take at
one time (`k`). A pool is the agents of one `agent_name` or one `start_url`.
Fast joins raise `k`. A slow join, a failed start, or no agent at 2 s halves
`k`.

```python
api.pacing(c["id"])            # mode, k, recent.p95_ms, slow
api.set_pacing(c["id"], mode="adaptive",
               pool={"target_ms": 1000, "backoff_ms": 1500, "first_k": 1, "max_k": 20})
api.set_pacing(c["id"], reset=True)
api.agent_pools()
```

- `mode`: `adaptive` (the default) uses `k`. `strict` also lets only one
  agent start more than the pool proved at one time.
- `pool` replaces the settings of the pool for all its campaigns. A value
  that you do not give takes its default. `backoff_ms` must be above
  `target_ms` and at most 2000.
- `reset` starts the pool again at `first_k`.
- A change has effect at the next claim. No restart is necessary.

If the campaign dials one call at a time, read `recent.p95_ms` in the pacing
answer. The agents take too long to join. Keep the agent workers ready for a
job (the `connect-an-agent` skill).

## 11. Stop

```bash
$QS down -v
e2e/livekit-cloud/run.sh teardown   # optional: delete the trunks and the rule
e2e/twilio/run.sh teardown          # optional: delete the trunk and its credentials
```

## Limits to tell the user

- Compliance data: a 24-hour window (COMP-1). The DNC list holds only what
  the API and the agents put in it. No national DNC list is loaded.
- No authentication on the API (API-1).
- No answering-machine detection. Each answer counts as a human answer.
- No reports or dashboard.
- No call rate for each campaign or each trunk. `AD_CALLS_PER_SEC` limits the
  whole cluster.
- A LiveKit phone number takes incoming calls only. The quickstart calls stay
  in LiveKit Cloud, and no phone rings.
- Twilio Elastic SIP Trunking and click-to-call work with the simulator. They
  did not run against Twilio yet (CARRIER-1, CARRIER-2).
