---
name: agentdialer-overview
description: >
  Explains the AgentDialer framework: a SIP-only, AI-agent-native, Kubernetes-native
  outbound dialer written in Rust. Its primary target is LiveKit Cloud SIP: the
  README quickstart connects it to LiveKit Cloud SIP and a phone number and places
  calls. Use this skill when you start work on AgentDialer, when you change the
  quickstart or a trunk, when you need to know which crate owns a concern, or when
  you need the map of the media plane and the campaign brain. Read this first
  before you change code.
license: AGPL-3.0-or-later
metadata:
  project: agentdialer
  tier: project
---

# AgentDialer — framework overview

AgentDialer is a SIP-only, AI-agent-native, Kubernetes-native outbound dialer.
It is written in Rust. AI agents (LiveKit and pipecat) replace human agents.
There are no human agents and no PSTN interface.

Read these files in this order before you change code:

1. `PLAN.md` at the repository root: the full architecture.
2. `README.md`: what is built, the configuration, and the known limits.
3. `GAPS.md`: the gaps against the open-source references, the order of work,
   and the decisions.
4. `.agents/REPO_GUIDE.md`: the working conventions.

Then read the reference code in `_research/` for the area you change.
`GAPS.md` Appendix A maps each area to its reference files. Much of the design
comes from these projects (VICIdial, dograh, FusionPBX, FreeSWITCH, drachtio,
rtpengine). Do not implement a feature before you read how they do it.

## The primary target

LiveKit Cloud SIP is the primary target. The goal: a new user stands up the
stack in Docker Compose, connects it to LiveKit Cloud SIP with a LiveKit phone
number, and starts to place calls. README "Quickstart: place calls through
LiveKit Cloud SIP" teaches it, and it must never break.

- The node dials the LiveKit number at the project's SIP URI, as it dials a
  carrier. LiveKit Cloud challenges with 407, and a dispatch rule puts each
  call into a room of its own with a callee agent.
- The stack is `docker-compose.quickstart.yml` with
  `docker-compose.livekit-cloud.yml` (the callee agent).
  `e2e/livekit-cloud/run.sh setup` makes the number, the trunks, and the rule,
  and writes `.env.lkcloud`.
- Use `e2e/livekit-cloud/run.sh` for every LiveKit Cloud API call. Never run
  a one-off command against LiveKit Cloud.
- Keep `e2e/run_twilio_e2e.sh --sim` green: it runs the quickstart stack with
  the carrier simulator on every change. When you have the LiveKit Cloud
  keys, also run `e2e/run_lkcloud_e2e.sh`.
- Limit: LiveKit numbers take incoming calls only, so these calls do not go
  through the public phone network. Do not claim that they do.

Twilio Elastic SIP Trunking is a second supported carrier (README "Call
through Twilio"), and it reaches real phones. The same quickstart stack
places its calls with `.env.twilio`. Keep `crates/ad_sip/tests/twilio.rs`
green. Use `e2e/twilio/run.sh` for every call to the Twilio API.

## The two halves

1. The media plane (`PLAN.md` §0.0, decision D-9). agentdialer carries the
   call itself:
   - The node that runs a call is its SIP user agent and its RTP endpoint.
     There is no Asterisk, FreeSWITCH, drachtio, rtpengine, or `livekit/sip`
     under it.
   - The SIP and media stacks are vendored libraries: `rsipstack` (SIP),
     `rustrtc` (SDP, RTP), `audio-codec` (G.711, G.722), and copied parts of
     `rustpbx`. Use a library that already does the job; write only the glue.
   - Kamailio is the SIP edge. It routes each call's SIP to the node that owns
     it (`Record-Route`). It does not carry audio.
   - The RTP goes straight between the carrier and the node, one socket per
     call, in `latching` or `public` mode.
   - Caution: never use `hostNetwork` or a port range per call on Kubernetes.
     Production is EKS with normal pods.
   - We own the SIP↔WebRTC bridge so LiveKit runs pure WebRTC rooms
     (`PLAN.md` §0.9a). `ad_gateway` owns the room participant
     (`LiveKitParticipant`) and the audio pump (`MediaBridge`).
   - Status: built and proven in Docker Compose (`e2e/run_e2e.sh`,
     `e2e/run_cluster_e2e.sh`). `ad_sip` is the SIP user agent, `ad_media`
     the RTP leg, and `ad_gateway::SipCaller` joins them to the room. The
     open items are in `PLAN.md` §0.0 ("Open").
   - Caution: never run `cargo fmt --all`. It also formats path dependencies
     and so rewrites the vendored crates in `vendor/`. Run `cargo fmt`.

2. The campaign brain (`PLAN.md` §0.10). This is the actual product:
   - campaigns, lists, the hopper (lead selection), pacing, dispositions,
     callbacks, recycle/retry, DNC, and the abandonment ledger.
   - a routing rule engine (`ad_routing`) modeled on the FusionPBX dialplan.
   - compliance: safe-hours, DNC, abandonment rate.
   - a Postgres event layer (an event table plus LISTEN/NOTIFY) that drives the
     orchestrator on events, not a blind poll. There is no Redis (decision D-3).

## The crates

The workspace is a set of focused crates under `crates/`. One crate = one
concern. Current crates (all built):

- `ad_core` — foundation. Typed ids, errors, and a `Clock` trait. The `Clock`
  trait makes time-dependent logic deterministic under test.
- `ad_compliance` — safe-hours gate, DNC scrubbing, abandonment ledger. Pure.
  Fails closed. See the `compliance-engine` skill.
- `ad_campaign` — the campaign aggregate and its lifecycle state machine.
- `ad_lead` — the lead aggregate and the lead repository trait.
- `ad_routing` — the routing rule engine. See the `routing-engine` skill.
- `ad_hopper` — lead selection: status, call-count, DNC, safe-hours, routing.
- `ad_circuit_breaker` — a pure rolling-window failure-rate breaker.
- `ad_dispatch` — the dial seam (`CallInitiator`, `Placement`) and the pacer.
- `ad_sip` — the node's SIP user agent on the vendored `rsipstack`: one
  outbound call through the SIP edge, the ring timeout, the end of the call.
- `ad_media` — one call's RTP leg on the vendored `rustrtc` and
  `audio-codec`: SDP, G.711, a 20 ms send clock that never stops, the media
  timeout.
- `ad_gateway` — the call path (`SipCaller`: `ad_sip` + `ad_media` + our
  native WebRTC participant in the room), the room operations, and the
  presence check of the reconcile.
- `ad_attempt` — the call-attempt statechart (customer leg and agent region).
  Pure. Read `PLAN.md` §0.10.7a.
- `ad_agent` — the agent adapters: LiveKit (explicit dispatch) and pipecat (a
  start request with a room token).
- `ad_cluster` — cluster coordination over kine: nodes, the leader, call
  assignment, and placement. Read `PLAN.md` §0.10.8b.
- `ad_orchestrator` — the pure scheduling gate, the run loop, the event
  reactor, the pure retry decision (`retry.rs`), and the pure settlement
  (`settle.rs`).
- `ad_events` — the event contract: typed events, the publisher and consumer
  traits, the breaker-store trait, and in-memory fakes.
- `ad_db` — the Postgres data layer (sqlx), migrations,
  the call attempts (`attempts`: claim, saved statechart, settlement),
  and the event layer (`events`: the event table with LISTEN/NOTIFY; `breaker`:
  the breaker window over the ended attempts).
- `ad_api` — the HTTP API (axum). A `router()` factory makes the API testable
  in process. See the `vertical-slice` skill.
- `ad_bin` — the single binary `agentdialer`. It runs roles by subcommand:
  `api`, `orchestrator`, `dispatch`, `migrate`. One image, many roles.
  `config.rs` holds the environment contract of each role.

## How the orchestrator runs

The orchestrator runs as a cluster (`PLAN.md` §0.10.8b). Every replica is a
node; the nodes coordinate through kine, the etcd API on Postgres.

1. Each node waits for Postgres and kine (with backoff), then holds its key
   `/agentdialer/nodes/{node}` by writing it again every third of the TTL.
2. One node holds `/agentdialer/leader` and schedules. It consumes the
   campaign events (`LISTEN campaign_events`), claims due leads straight from
   `leads` within the campaign cap and the free capacity of the cluster, and
   assigns each call to the node with the most free capacity
   (`/agentdialer/calls/{node}/{attempt}`), paced by `AD_CALLS_PER_SEC`.
3. Each node runs its calls: it starts the campaign's agent (`ad_agent`:
   LiveKit dispatch or pipecat start request), waits until the agent is in
   the room, and sends the INVITE through Kamailio (`ad_gateway::SipCaller`).
   On the answer it joins the room and carries the audio. It saves each
   result with its node and assignment generation (`apply_owned`). A node
   that lost the call is fenced.
4. The call ends on its node: the callee's BYE, the agent leaving the room,
   or no RTP for 30 s. The node's check against LiveKit
   (`AD_ATTEMPT_CHECK_MS`) ends a call whose node died.
5. The transaction that ends an attempt writes `attempt_ended`. The leader
   settles the lead (the pure `ad_orchestrator::settle`), closes the room,
   removes the assignment, and dials into the freed capacity.
6. The leader's sweep (`AD_RETRY_SWEEP_MS`) settles ended attempts, repairs
   the assignments (moves the calls of a dead node), dials the leads whose
   retry or hold became due, and completes idle campaigns.

Every state change and its event commit in one transaction
(`ad_db::events::publish_in`).

Caution: kine is not etcd. Use only `ad_cluster::Kine`, which offers the
transactions kine supports. Do not use lease KeepAlive, Election, plain Put,
plain delete, or keys-only reads (`PLAN.md` §0.10.8b).

## Decisions in force

`GAPS.md` §8 records each decision. These change how you build:

- D-2 (decided and built): the dialer places the call. The agent never
  dials. Agents join through two adapters: LiveKit (a room plus agent
  dispatch) and pipecat (a bot per call). The attempt statechart drives both,
  and the dial waits until the agent is in the room.
- D-3 (decided and built): Postgres is the event layer. Do not add Redis.
- D-6 (decided, not built): each agent session opens one gRPC stream to the
  dialer for the outcome, the session events, and the logs. OpenTelemetry
  (OTLP) uses the same gRPC endpoint.
- D-8 (decided and built): the orchestrator replicas form a cluster through
  kine. Postgres and kine are managed services (for example RDS): reach them
  only by URL, wait for them with backoff, and never depend on a host check or
  a start order.
- D-9 (decided and built): agentdialer carries the call with vendored SIP
  and RTP libraries, behind Kamailio, with no host networking (`PLAN.md`
  §0.0).

## The rules you must follow

Read `.agents/REPO_GUIDE.md` for the full list. The rules that block merges:

- Vertical slices, not phases. Ship logic + API + tests together, all green.
  See the `vertical-slice` skill.
- Fail closed in compliance. See the `compliance-engine` skill.
- Pure engines are pure. Decision functions take data + a `Clock`. No I/O.
- Registry-driven extensibility. Add a variant with its own module and a small,
  enumerated set of edits. Do not run a migration to add a variant.
- Run every tool in a container. The host runs only bash, git, the Docker CLI,
  and basic shell utilities. `README.md` "Build and test" has the recipes.
- Keep the environment contract in step. When you add, rename, or remove a
  variable, change `config.rs`, the README "Configuration" table, the Helm
  chart, and every e2e script that starts the role. A test in `ad_bin` fails
  if one of them is out of date.
- Do not edit an applied migration. sqlx stores a checksum of each migration.
  Add a new numbered file in `crates/ad_db/migrations/`.
- Put scratch work in `_research/scratch/<topic>/`. Do not use `/tmp`.
- Write all text in Simplified Technical English (ASD-STE100). Aim for 80%.

## Before you say "done"

Run the Docker-first test tier. It runs fmt + clippy + the full suite (including
the Postgres integration tests) in containers, identically to CI:

```
docker compose -f docker-compose.test.yml up --build \
    --abort-on-container-exit --exit-code-from tests
docker compose -f docker-compose.test.yml down
```

If you change how a role behaves at runtime, also run the e2e script that
covers it (`e2e/run_e2e.sh`, `e2e/run_events_e2e.sh`, or
`e2e/run_webrtc_e2e.sh`). If you change the call path or the quickstart, also
run `e2e/run_twilio_e2e.sh --sim`, and `e2e/run_lkcloud_e2e.sh` when you have
the LiveKit Cloud keys. Always stop the stack after tests. If you did not run
the tier, the work is not done.

## Where to go next

- To pick the next work, read `GAPS.md` §7 (the order of work).
- To change the dialer core, read `PLAN.md` §0.10 and the `routing-engine` skill.
- To change compliance, read the `compliance-engine` skill.
- To add an API endpoint, read the `vertical-slice` skill.
- To work on the call path (SIP, RTP, the bridge into the room), read
  `PLAN.md` §0.0, `vendor/VENDORED.md`, `crates/ad_sip/`, `crates/ad_media/`,
  and `crates/ad_gateway/src/sip_call.rs`. The research notes are in
  `_research/scratch/media/` (the carrier lessons are in
  `asterisk-freeswitch.md`). Test with `e2e/run_e2e.sh` (Kamailio, a NAT,
  the carrier simulator `e2e/carrier-sim`, both agent runtimes).
- To change the quickstart or a trunk, read "The primary target" above and
  the README sections "Quickstart: place calls through LiveKit Cloud SIP" and
  "Call through Twilio". Put new carrier behavior into the simulator (diago),
  not into ad hoc scripts.
- To change the orchestrator, the cluster, the call attempts, or the event
  layer, read `PLAN.md` §0.10.7a, §0.10.8a, and §0.10.8b,
  `crates/ad_bin/src/orchestrator/` (`mod.rs`, `calls.rs`, `leader.rs`),
  `crates/ad_cluster/`, `crates/ad_attempt/`, `crates/ad_db/src/attempts.rs`,
  `crates/ad_events/`, and `crates/ad_db/src/events.rs`.
- To add or change an environment variable, read `crates/ad_bin/src/config.rs`.
- The `e2e/` directory holds the automated end-to-end proofs. Run them with
  their scripts; each brings up a real stack and tears it down.
