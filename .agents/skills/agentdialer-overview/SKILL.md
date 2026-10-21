---
name: agentdialer-overview
description: >
  Explains the AgentDialer framework: a SIP-only, AI-agent-native, Kubernetes-native
  outbound dialer written in Rust. Use this skill when you start work on AgentDialer,
  when you need to know which crate owns a concern, or when you need the map of the
  media plane and the campaign brain. Read this first before you change code.
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

## The two halves

1. The media plane (`PLAN.md` §0). It is plumbing we reuse, not build:
   - `drachtio` does SIP signaling (it wraps Sofia-SIP).
   - `rtpengine` does RTP/SRTP media (it has a Linux kernel module).
   - Our Rust code is a thin control plane over these engines.
   - We do NOT write a SIP stack or a media engine from scratch.
   - A spike proved this works (`PLAN.md` §0.3.1). The spike code is in `spike/`.
   - We own the SIP↔WebRTC bridge so LiveKit runs pure WebRTC rooms
     (`PLAN.md` §0.9a). `ad_gateway` owns it. The bridge seam (`RoomParticipant`,
     `MediaBridge`, `AudioFrame`) and its in-memory tests are built; the real
     libwebrtc-backed participant (`LiveKitParticipant`) is part of the normal
     build. The orchestrator does not use it yet. It still dials through
     `CreateSIPParticipant` (`livekit/sip`).

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
- `ad_dispatch` — the dial loop. A `CallInitiator` seam + RAII capacity slots.
- `ad_gateway` — the real `CallInitiator`. It originates calls into LiveKit
  rooms, and owns the SIP↔WebRTC bridge (a native WebRTC participant, part of
  the normal build).
- `ad_orchestrator` — the pure scheduling gate, the run loop, the event
  reactor, and the pure retry decision (`retry.rs`).
- `ad_events` — the event contract: typed events, the publisher and consumer
  traits, the breaker-store trait, and in-memory fakes.
- `ad_db` — the Postgres data layer (sqlx), migrations, advisory-lock leader,
  and the event layer (`events`: the event table with LISTEN/NOTIFY; `breaker`:
  the breaker window over `call_outcomes`).
- `ad_api` — the HTTP API (axum). A `router()` factory makes the API testable
  in process. See the `vertical-slice` skill.
- `ad_bin` — the single binary `agentdialer`. It runs roles by subcommand:
  `api`, `orchestrator`, `dispatch`, `migrate`. One image, many roles.
  `config.rs` holds the environment contract of each role.

## How the orchestrator runs

1. It waits for the Postgres advisory lock. Only the leader continues.
2. It connects the event consumer (`LISTEN campaign_events`).
3. It reconciles: it dispatches a batch for each running campaign.
4. It consumes events. A `NOTIFY` wakes it. A sweep timer
   (`AD_EVENT_SWEEP_MS`) reads the event table again if a notification is
   lost.
5. For each call, it writes the outcome and the retry (`next_eligible_at` on
   the lead row) in one transaction.
6. A retry sweep (`AD_RETRY_SWEEP_MS`) puts due leads back in the hopper and
   publishes an event that schedules the next batch.

## Decisions in force

`GAPS.md` §8 records each decision. These change how you build:

- D-2 (decided, not built): the dialer places the call. The agent never dials.
  Agents join through two adapters: LiveKit (a room plus agent dispatch) and
  pipecat (a bot per call). A statechart per attempt drives both. Its state is
  saved in Postgres (an event log plus a snapshot) and restored after a
  restart.
- D-3 (decided and built): Postgres is the event layer. Do not add Redis.
- D-6 (decided, not built): each agent session opens one gRPC stream to the
  dialer for the outcome, the session events, and the logs. OpenTelemetry
  (OTLP) uses the same gRPC endpoint.

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
`e2e/run_webrtc_e2e.sh`). Always stop the stack after tests. If you did not run
the tier, the work is not done.

## Where to go next

- To pick the next work, read `GAPS.md` §7 (the order of work).
- To change the dialer core, read `PLAN.md` §0.10 and the `routing-engine` skill.
- To change compliance, read the `compliance-engine` skill.
- To add an API endpoint, read the `vertical-slice` skill.
- To work on the media plane or the SIP↔WebRTC bridge, read `PLAN.md` §0 and
  §0.9a, the spike in `spike/`, and `crates/ad_gateway/src/bridge.rs`.
- To change the orchestrator or the event layer, read `PLAN.md` §0.10.8a,
  `crates/ad_bin/src/orchestrator.rs`, `crates/ad_events/`, and
  `crates/ad_db/src/events.rs`.
- To add or change an environment variable, read `crates/ad_bin/src/config.rs`.
- The `e2e/` directory holds the automated end-to-end proofs. Run them with
  their scripts; each brings up a real stack and tears it down.
