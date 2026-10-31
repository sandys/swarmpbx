---
name: run-campaigns
description: >
  Teaches how to use AgentDialer to place calls. Start the stack in Docker
  Compose, connect a trunk (LiveKit Cloud SIP, Twilio Elastic SIP Trunking, or
  Twilio click-to-call), create a campaign, load leads, and start it. Then
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

This skill teaches you to place calls with AgentDialer. `README.md` has each
step in full. This skill puts the API, the decisions, and the checks in one
place.

## Safety rules

- Run every tool in a container. The host needs only bash, git, and the
  Docker CLI. `scripts/api.sh` sends API requests from a container.
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

README "Architecture" has a diagram for each step: "Before the dial: the
claim and the gate", "One call, step by step", and "After the call: the
settlement".

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
scripts/api.sh GET /health        # {"status":"ok", ...} when the API is ready
```

The API is `http://api:8080` on the stack network, and `127.0.0.1:8080` on
the host. The stack has a demo agent with the agent name `demo`. It plays a
tone and leaves after 30 s.

## 3. Create a campaign

`POST /v1/campaigns`:

| Field | Need | Meaning |
|---|---|---|
| `org_id` | required | The organization, as an integer. Use `1`. |
| `name` | required | Must not be empty. |
| `concurrency_cap` | optional | The most live calls of the campaign at one time. Default 10. |
| `agent` | optional | The agent of each call (see below). Give each campaign an agent. |

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

```bash
scripts/api.sh POST /v1/campaigns \
  '{"org_id": 1, "name": "first-calls", "concurrency_cap": 2,
    "agent": {"runtime": "livekit", "agent_name": "demo",
              "metadata": {"script": "renewal-v1"}}}'
```

## 4. Load leads

`POST /v1/campaigns/:id/leads` with `{"leads": [...], "default_region": "US"}`.
`default_region` (ISO 3166) is the country of a number without a country code.

| Field | Need | Meaning |
|---|---|---|
| `phone_number` | required | Any common form: `+1 202 555 0123`, or `(202) 555-0123` with a `default_region`. The dialer keeps it in E.164 (`+12025550123`). The number must be one that the numbering plan assigns. |
| `timezone` | required to dial | An IANA name, for example `America/New_York`. A lead without one does not dial, because compliance fails closed (COMP-2). |
| `state`, `postal_code` | optional | Saved with the lead. A US or Canadian postal code gets its standard form (`12345-6789`, `K1A 0B1`). |
| `context` | optional | A map of strings. The agent gets it as `lead.context`. Put the name of the person and the facts that the agent needs here. |

```bash
scripts/api.sh POST /v1/campaigns/1/leads \
  '{"leads": [{"phone_number": "+12025550123", "timezone": "America/New_York",
               "context": {"first_name": "Ada", "plan": "gold"}}]}'
```

If one lead is bad, the API loads no lead. It answers HTTP 400 with a report
of each bad lead: `{"error", "rejected": [{"index", "phone_number",
"reason"}]}`. Correct those leads, and send the list again.

The phone numbers and the postal codes go through one module,
`crates/ad_contact`. To use another phone library, change it there.

## 5. Start, pause, and resume

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
running campaign when no lead has work left.

## 6. Watch the calls and read the results

```bash
$QS logs -f node                          # the node and the leader
scripts/api.sh GET /v1/campaigns/1        # the state of the campaign
scripts/api.sh GET /v1/campaigns/1/leads  # the status and called_count of each lead
```

Useful log lines of the node: `call assigned`, `lead not dialed`,
`a participant joined the room`, `agent in the room after the answer`,
`call ended`, `attempt settled`, `abandonment rate above the cap; agent
first`, `breaker open; campaign paused`, `campaign completed`, and `the OS
clock is off`. The `api` log shows `agent session open` and `agent outcome`
for each agent that reports.

The status of a lead:

| Status | Meaning |
|---|---|
| `new` | Not dialed yet. |
| `queued` | Waits for a dial. `next_eligible_at` holds the time of a retry or a hold. |
| `in_call` | Has a live call. |
| `dispositioned`, `done` | Dialed. No more calls. |
| `dnc` | On a do-not-call list. |
| `blocked` | Cannot dial until its data changes, for example a lead with no timezone. |

The calls and the events of a campaign, in pages (`?after=<id>&limit=<n>`):

```bash
scripts/api.sh GET /v1/campaigns/1/attempts   # each call: outcome, sip_code, disposition, agent_outcome
scripts/api.sh GET /v1/campaigns/1/events     # campaign events, for example agent_pool_tripped
```

Each call is a row in `call_attempts`. To read more columns, use `psql` in a
container on the stack network:

```bash
docker run --rm --network agentdialer-quickstart_default postgres:16-alpine \
  psql postgres://ad:ad@postgres:5432/ad -c \
  "SELECT id, lead_id, state, outcome, sip_code, detail, answered_at, ended_at
     FROM call_attempts WHERE campaign_id = 1 ORDER BY id"
```

`attempt_events` holds the events of each call in order (`attempt_id`,
`seq`, `kind`, `payload`).

The outcome of a call:

| Outcome | Meaning | Next step |
|---|---|---|
| `completed` | The callee answered, and the call ended. | The agent's outcome decides (below). |
| `busy`, `no_answer` | The SIP code of the callee. | The dialer retries (see below). |
| `declined` | The callee refused the call. | None. The dialer does not call the lead again. |
| `invalid_number` | The trunk refused the number. | Correct the number. The lead is `done`. |
| `failed` | The trunk or the carrier failed the call. `sip_code` and `detail` tell why. | Read "If a call fails" in the README section of the trunk. |
| `abandoned` | The callee answered, and no agent was in the room 2 s later. | Make the agent faster, or use `agent_first`. See section 9. |
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

The disposition codes: `scripts/api.sh GET /v1/campaigns/1/dispositions`. To
add campaign codes, `PUT` a list of `{"code", "name", "flags"}`. The flags
are `human_answered`, `sale`, `customer_contact`, `not_interested`,
`unworkable`, `scheduled_callback`, `completed`, `answering_machine`, and
`dnc`. A campaign code must not use a system code.

## 7. The DNC list

| Request | What it does |
|---|---|
| `POST /v1/dnc` `{"phone_number", "scope", "campaign_id", "region", "note"}` | Adds a number. `scope` is `system` (the default, every campaign) or `campaign` (with `campaign_id`). Every waiting lead with that number stops at once. |
| `GET /v1/dnc?campaign_id=1` | The system list and the list of campaign 1, newest first. |
| `DELETE /v1/dnc/:id` | Removes an entry. A lead that stopped stays `dnc`. |

The dialer checks the list again before each dial. If it cannot read the
list, it does not dial (fail closed).

## 8. Retries and the breaker

- A lead gets at most 3 calls. The wait before the next call is 5 min after
  `busy`, 30 min after `no_answer`, 60 min after a machine answer, and 15 min
  after `failed`, `abandoned`, `setup_error`, or `uncertain`. After the third
  call, the lead is `dispositioned`. No setting changes this yet
  (`crates/ad_orchestrator/src/retry.rs`).
- `completed` and `declined` end the lead (`dispositioned`), unless the agent
  asked for a callback or a voicemail retry. There is no recycle rule yet.
- The retry of an `abandoned` lead dials agent first.
- The breaker pauses a campaign when at least 5 calls ended in the last
  2 min, and more than half of them are `failed`, `setup_error`, or
  `uncertain`. Correct the cause, and then send `resume`.

## 9. Select the dial order and the pacing

**The dial order** (`agent.dial_order`):

- Use `customer_first` (the default) when your agent can be in the room 2 s
  after the answer. An agent starts only for an answered call.
- Use `agent_first` when your agent needs more than 2 s to join, or when you
  cannot accept an abandoned call. Each call that rings then holds an agent.
- `GET /v1/campaigns/:id/abandonment` shows the answered and abandoned calls
  of the last 30 days. When the rate is above 3%, the campaign dials agent
  first until the rate is at or under 3%.

**The pacing.** The dialer learns how many calls each agent pool can take at
one time (`k`). A pool is the agents of one `agent_name` or one `start_url`.
Fast joins raise `k`. A slow join, a failed start, or no agent at 2 s halves
`k`.

```bash
scripts/api.sh GET /v1/campaigns/1/pacing     # mode, k, recent.p95_ms, slow
scripts/api.sh PUT /v1/campaigns/1/pacing \
  '{"mode": "adaptive", "pool": {"target_ms": 1000, "backoff_ms": 1500, "first_k": 1, "max_k": 20}}'
scripts/api.sh PUT /v1/campaigns/1/pacing '{"reset": true}'
scripts/api.sh GET /v1/agent-pools
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

## 10. Stop

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
