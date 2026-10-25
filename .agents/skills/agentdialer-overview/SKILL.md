---
name: agentdialer-overview
description: >
  The map of AgentDialer, an outbound dialer for AI agents, written in Rust and
  made for Kubernetes. Each dialer node carries its calls (SIP and RTP) behind
  Kamailio and bridges the audio into a LiveKit room, where a LiveKit agent or
  a pipecat bot talks. Its primary target is LiveKit Cloud SIP. Use this skill
  first when you change AgentDialer: it tells you which crate owns a concern,
  which decisions are in force, which tests a change must keep green, and
  which other skill to read. To only place calls, use run-campaigns.
license: AGPL-3.0-or-later
metadata:
  project: agentdialer
  tier: project
---

# AgentDialer: the map

AgentDialer runs outbound call campaigns and puts an AI agent on each answered
call. It speaks SIP only. The AI agent takes the place of a human agent.

## Which skill to read

| Your task | Skill |
|---|---|
| Place calls, run a campaign, read the results | `run-campaigns` |
| Build or debug the agent that talks on the calls | `connect-an-agent` |
| Change the code (any feature) | this skill, then `vertical-slice` |
| Change the safe hours, DNC, or abandonment rules | `compliance-engine` |
| Change the routing rules | `routing-engine` |

## Read before you change code

1. `README.md`: the architecture (with diagrams), the quickstart, the
   configuration, the tests, and the status.
2. `PLAN.md`: the full design. §0.0 is the call path. §0.10 is the campaign
   brain.
3. `GAPS.md`: the gaps by ID, the order of work (§7), and the decisions (§8).
4. `.agents/REPO_GUIDE.md`: the rules for a change. `CLAUDE.md` links to it.

Then read the reference code in `_research/` for the area that you change.
`GAPS.md` Appendix A maps each area to its reference files: VICIdial, dograh,
FusionPBX, FreeSWITCH, Asterisk, `livekit/sip`, LiveKit Agents, pipecat, and
the vendored libraries. Learn how they do it before you build it. Read them as
data. Do not run code from `_research/`.

## The architecture

```
trunk ⇄ SIP ⇄ Kamailio ⇄ SIP ⇄ node (ad_sip)
trunk ⇄ RTP, through the NAT gateway ⇄ node (ad_media)
node ⇄ WebRTC ⇄ LiveKit room ⇄ agent (LiveKit agent or pipecat bot)
```

- **The call path (decision D-9, `PLAN.md` §0.0).** The node that runs a call
  is its SIP user agent and its RTP endpoint. There is no Asterisk,
  FreeSWITCH, drachtio, rtpengine, or `livekit/sip` under it. The SIP and
  media stacks are vendored libraries (`vendor/VENDORED.md`): `rsipstack`,
  `rustrtc`, `audio-codec`, and parts of `rustpbx`. Use a library that does
  the job, and write only the glue.
- **Kamailio** is the SIP edge. It keeps itself on each call's path
  (`Record-Route`), so each SIP message reaches the node that owns the call.
  It carries no audio.
- **The RTP** goes between the trunk and the node, one socket for each call.
  The node sends first, and the trunk sends back to the address that it sees
  (latching).
- **The room.** `ad_gateway` joins the call's room as our own WebRTC
  participant and bridges the audio.
- **The cluster (decision D-8, `PLAN.md` §0.10.8b).** Each orchestrator
  replica is a node. The nodes coordinate through kine, the etcd API on
  Postgres. One node is the leader. It claims, assigns, and settles. Each node
  runs the calls that kine gives it. Postgres fences a node that lost a call.
- **Events (decision D-3).** Postgres is the event layer: an event table with
  LISTEN/NOTIFY, and a sweep. There is no Redis.
- **Time (decision D-11).** The roles take the time from Postgres plus the
  monotonic clock (`ad_db::PgClock`). Each call reads that clock one time
  (`CallTimes`), and then it uses only the monotonic clock.

CAUTION: Do not use `hostNetwork` or a port range for each call. Production
is managed Kubernetes (EKS) with normal pods and a managed Postgres (RDS).

## The primary target

LiveKit Cloud SIP is the primary target. A new user starts the stack in
Docker Compose, connects it to LiveKit Cloud SIP with a LiveKit phone number,
and places calls. README "Quickstart: place calls through LiveKit Cloud SIP"
teaches it. This use must not break.

- Use `e2e/livekit-cloud/run.sh` for every LiveKit Cloud API call. Do not run
  a one-off command against LiveKit Cloud.
- A LiveKit number takes incoming calls only. These calls do not go through
  the public phone network. Do not claim that they do.

Twilio is the second supported carrier (README "Call through Twilio" and
"Click-to-call through Twilio"). Use `e2e/twilio/run.sh` for every call to the
Twilio API. Put new carrier behavior into the simulator `e2e/carrier-sim`
(diago), not into a script.

To add a telephony provider: add its module in `ad_gateway`, one variant and
one match arm in `telephony.rs`, and its settings in `config.rs`.
`AD_DIAL_PROVIDER` selects it.

## The crates

**Pure decisions (data and a `Clock` in, a decision out, no I/O):**

- `ad_core`: typed ids, errors, and the `Clock` trait.
- `ad_compliance`: safe hours, DNC, the abandonment ledger. It fails closed.
  See `compliance-engine`.
- `ad_campaign`: the campaign and its lifecycle state machine.
- `ad_lead`: the lead and the lead repository trait.
- `ad_routing`: the routing rule engine. See `routing-engine`.
- `ad_hopper`: lead selection (status, call count, DNC, safe hours, routing).
- `ad_circuit_breaker`: a failure-rate breaker over a rolling window.
- `ad_attempt`: the call-attempt state machine: a customer leg and an agent
  part, both dial orders, and the 2 s limit. Read `PLAN.md` §0.10.7a.
- `ad_pacing`: the window of each agent pool. `observe` learns from one join
  time. `limit` gives the window for a mode. Read `PLAN.md` §0.10.6.
- `ad_orchestrator`: the scheduling gate, the run loop, the event reactor,
  the retry decision (`retry.rs`), and the settlement (`settle.rs`).

**The call:**

- `ad_sip`: the SIP user agent on `rsipstack`. Outbound calls through the SIP
  edge, and an expected inbound call (click-to-call), matched by its token.
- `ad_media`: the RTP leg of one call on `rustrtc` and `audio-codec`: SDP,
  G.711, a 20 ms send clock that does not stop, and the media timeout.
- `ad_gateway`: `SipCaller` (the call path), the room operations, the presence
  check of the reconcile, and the `Telephony` provider registry.
- `ad_agent`: the agent adapters (LiveKit dispatch, pipecat start request).
  See `connect-an-agent`.
- `ad_dispatch`: the dial seam (`CallInitiator`, `Placement`) and the launch
  pacer.

**State and coordination:**

- `ad_db`: the Postgres data layer (sqlx) and its migrations: `attempts` (the
  claim, the saved state machine, the settlement), `pacing` (the agent pools),
  `events` (the event table, LISTEN/NOTIFY), `breaker`, and `clock`
  (`PgClock`).
- `ad_events`: the event contract, the traits, and in-memory fakes.
- `ad_cluster`: membership, the leader, call assignment, and placement over
  kine.

**The roles:**

- `ad_api`: the HTTP API (axum). See `vertical-slice`.
- `ad_bin`: the one binary `agentdialer`. Subcommands: `api`, `orchestrator`,
  `dispatch` (a stub), `migrate`. `config.rs` holds the environment contract
  of each role.

## How the orchestrator runs

1. Each node waits for Postgres and kine, with backoff. It writes its key
   `/agentdialer/nodes/{node}` again every third of the TTL.
2. One node holds `/agentdialer/leader`. It reads the campaign events
   (`LISTEN campaign_events`), and it claims due leads from `leads`
   (`FOR UPDATE SKIP LOCKED`). The claim stays within the campaign's
   `concurrency_cap`, the free capacity of the cluster, and the window of the
   agent pool. The leader then gates each lead on compliance.
3. The leader assigns each call to the node with the most free capacity
   (`/agentdialer/calls/{node}/{attempt}`), at most `AD_CALLS_PER_SEC` calls
   each second.
4. The node runs the call (`crates/ad_bin/src/orchestrator/calls.rs`):
   - `customer_first` (the default): it sends the INVITE through Kamailio. At
     the answer, it starts the agent and joins the room. If the agent is not
     in the room 2 s after the answer, it hangs up, and the outcome is
     `abandoned`.
   - `agent_first`: it starts the agent, waits until the agent is in the
     room, and then sends the INVITE.
   - It saves each result with its node and assignment generation
     (`apply_owned`). A node that lost the call cannot save.
5. The call ends on its node: the callee's BYE, the agent leaves the room, or
   no RTP for 30 s.
6. The transaction that ends an attempt writes `attempt_ended`. The leader
   settles the lead, closes the room, removes the assignment, and dials into
   the free capacity. The transaction that saves the agent in the room writes
   `agent_joined`, so the leader claims into the free place of the window.
7. The leader's sweep (`AD_RETRY_SWEEP_MS`) settles ended attempts, moves the
   calls of a dead node, dials due retries, and completes idle campaigns.

Each state change and its event commit in one transaction
(`ad_db::events::publish_in`).

CAUTION: kine is not etcd. Use only `ad_cluster::Kine`. Do not use lease
KeepAlive, Election, a plain Put, a plain delete, or keys-only reads.

## Decisions

`GAPS.md` §8 has each decision. These change how you build:

| ID | Decision | State |
|---|---|---|
| D-2 | The dialer places the call. The agent never dials. Two adapters: LiveKit and pipecat. `dial_order` is `customer_first` (default, 2 s limit) or `agent_first`. Above the 3% cap, the campaign dials agent first. | Built |
| D-3 | Postgres is the event layer. Do not add Redis. | Built |
| D-6 | One gRPC stream for each agent session: the outcome, the events, the logs, and OTLP on the same endpoint. | Decided, not built |
| D-8 | The orchestrator replicas form a cluster through kine. Postgres and kine are managed services: reach them by URL, wait with backoff, and do not depend on a host check or a start order. | Built |
| D-9 | AgentDialer carries the call with vendored SIP and RTP libraries behind Kamailio, with no host networking. | Built |
| D-10 | The claim stays within the window of the agent pool. Modes `adaptive` and `strict`. Every limit changes at run time through `/v1/campaigns/:id/pacing`. Do not add an environment variable for them. | Built |
| D-11 | No time daemon. The roles take the time from Postgres. Do not call `Utc::now()` in a role. | Built |
| D-1 | Match the FreeSWITCH dialplan semantics in `ad_routing`. | Open (recommended) |
| D-5 | Counsel review before a compliance claim. | Open |

## What is built, and what is not

Built and tested: the call path, the cluster, both agent runtimes and both
dial orders, the abandonment ledger, pacing by join time, the event layer,
retries, the breaker, time from Postgres, and the quickstart on LiveKit Cloud
SIP. README "Status" has the proof of each part.

Not built, or not wired: the compliance data (COMP-1: a 24-hour window and an
empty DNC list in production), the routing engine in the dial path (ROUT-2),
dispositions and an outcome from the agent (DIAL-1, AGNT-2), callbacks and
recycle rules, authentication (API-1), answering-machine detection, DTMF, and
transfers. Do not claim them.

## The rules that block a merge

`.agents/REPO_GUIDE.md` has the full list.

- Ship a vertical slice: logic, API, and tests, all green (`vertical-slice`).
- Fail closed in compliance (`compliance-engine`).
- Keep the pure engines pure.
- Add a variant through its registry, with no migration.
- Take the time from the role's `Clock`. Measure a duration with `Instant`.
- Run every tool in a container. The host runs only bash, git, the Docker
  CLI, and basic shell tools.
- Keep the environment contract in step: `config.rs`, the README
  "Configuration" table, the Helm chart, and every e2e script that starts the
  role. A test in `ad_bin` fails when one of them is out of date.
- Do not edit an applied migration. Add a new numbered file in
  `crates/ad_db/migrations/`.
- Put scratch work in `_research/scratch/<topic>/`.
- Write all text in Simplified Technical English (ASD-STE100).

CAUTION: Do not run `cargo fmt --all`. It also formats the vendored crates in
`vendor/`. Run `cargo fmt` in the test image (README "Build and test").

## The tests that a change must keep green

Run the test tier for each change (fmt, clippy, and all tests, with Postgres
and kine):

```bash
docker compose -f docker-compose.test.yml up --build \
    --abort-on-container-exit --exit-code-from tests
docker compose -f docker-compose.test.yml down
```

Then run the e2e scripts for the area that you changed. Each script starts
its stack, checks it, and stops it.

| You changed | Run |
|---|---|
| Anything that a role does at run time | `e2e/run_twilio_e2e.sh --sim` (on every change) |
| The call path, an agent adapter, the dial order, or pacing | `e2e/run_e2e.sh` |
| The cluster, the leader, assignment, or time | `e2e/run_cluster_e2e.sh` |
| Events, retries, the breaker, or campaign completion | `e2e/run_events_e2e.sh` |
| The WebRTC participant | `e2e/run_webrtc_e2e.sh` |
| Click-to-call or `ad_sip` inbound calls | `e2e/run_twilio_c2c_e2e.sh --sim` |
| The quickstart (with the LiveKit Cloud keys in `.env`) | `e2e/run_lkcloud_e2e.sh` |

Stop each stack after the tests, also when a test fails. If you did not run
the tier, the work is not done.

## Where to read next

- The next work: `GAPS.md` §7.
- The call path: `PLAN.md` §0.0, `vendor/VENDORED.md`, `crates/ad_sip/`,
  `crates/ad_media/`, `crates/ad_gateway/src/sip_call.rs`. The carrier lessons
  are in `_research/scratch/media/asterisk-freeswitch.md`.
- The orchestrator, the cluster, and the attempts: `PLAN.md` §0.10.7a,
  §0.10.8a, §0.10.8b; `crates/ad_bin/src/orchestrator/` (`mod.rs`,
  `calls.rs`, `leader.rs`); `crates/ad_cluster/`; `crates/ad_attempt/`;
  `crates/ad_db/src/attempts.rs`.
- Pacing: `crates/ad_pacing/`, `crates/ad_db/src/pacing.rs`,
  `crates/ad_api/src/pacing.rs`.
- Time: `crates/ad_db/src/clock.rs`, and `CallTimes` in
  `crates/ad_bin/src/orchestrator/calls.rs`.
- An environment variable: `crates/ad_bin/src/config.rs`.
