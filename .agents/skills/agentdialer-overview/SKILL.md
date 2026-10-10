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

Read `PLAN.md` at the repository root for the full architecture. Read
`.agents/REPO_GUIDE.md` for the working conventions. This skill gives you the
map and tells you where to go next.

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
     libwebrtc-backed participant is behind the `webrtc` feature.

2. The campaign brain (`PLAN.md` §0.10). This is the actual product:
   - campaigns, lists, the hopper (lead selection), pacing, dispositions,
     callbacks, recycle/retry, DNC, and the abandonment ledger.
   - a routing rule engine (`ad_routing`) modeled on the FusionPBX dialplan.
   - compliance: safe-hours, DNC, abandonment rate.
   - a Redis event layer (`ad_events`) that drives the orchestrator on events,
     not a blind poll.

## The crates

The workspace is a set of focused crates under `crates/`. One crate = one
concern. Current crates (all built):

- `ad_core` — foundation. Typed ids, errors, and a `Clock` trait. The `Clock`
  trait makes time-dependent logic deterministic under test.
- `ad_compliance` — safe-hours gate, DNC scrubbing, abandonment ledger. Pure.
  Fails closed. See the `compliance-engine` skill.
- `ad_campaign` — the campaign aggregate and its lifecycle state machine.
- `ad_lead` — the lead aggregate and the lead repository trait.
- `ad_routing` — the FusionPBX-grade routing rule engine. See the
  `routing-engine` skill.
- `ad_hopper` — lead selection: status, call-count, DNC, safe-hours, routing.
- `ad_circuit_breaker` — a pure rolling-window failure-rate breaker.
- `ad_dispatch` — the dial loop. A `CallInitiator` seam + RAII capacity slots.
- `ad_gateway` — the real `CallInitiator`. It originates calls into LiveKit
  rooms, and owns the SIP↔WebRTC bridge (a native WebRTC participant, `webrtc`
  feature).
- `ad_orchestrator` — the pure scheduling gate, the run loop, and the event
  reactor.
- `ad_events` — the Redis event layer: typed events, pub/sub, due-timers, and
  the persistent circuit-breaker store.
- `ad_db` — the Postgres data layer (sqlx), migrations, advisory-lock leader.
- `ad_api` — the HTTP API (axum). A `router()` factory makes the API testable
  in process. See the `vertical-slice` skill.
- `ad_bin` — the single binary `agentdialer`. It runs roles by subcommand:
  `api`, `orchestrator`, `dispatch`, `migrate`. One image, many roles.

## The rules you must follow

Read `.agents/REPO_GUIDE.md` for the full list. The rules that block merges:

- Vertical slices, not phases. Ship logic + API + tests together, all green.
  See the `vertical-slice` skill.
- Fail closed in compliance. See the `compliance-engine` skill.
- Pure engines are pure. Decision functions take data + a `Clock`. No I/O.
- Registry-driven extensibility. Add a variant with its own module and a small,
  enumerated set of edits. Do not run a migration to add a variant.
- Write all text in Simplified Technical English (ASD-STE100). Aim for 80%.

## Before you say "done"

Run the Docker-first test tier. It runs fmt + clippy + the full suite (including
the Postgres and Redis integration tests) in containers, identically to CI:

```
docker compose -f docker-compose.test.yml up --build \
    --abort-on-container-exit --exit-code-from tests
docker compose -f docker-compose.test.yml down
```

Always stop the stack after tests. If you did not run the tier, the work is not
done.

## Where to go next

- To change the dialer core, read `PLAN.md` §0.10 and the `routing-engine` skill.
- To change compliance, read the `compliance-engine` skill.
- To add an API endpoint, read the `vertical-slice` skill.
- To work on the media plane or the SIP↔WebRTC bridge, read `PLAN.md` §0 and
  §0.9a, the spike in `spike/`, and `crates/ad_gateway/src/bridge.rs`.
- To change the orchestrator or the event layer, read `PLAN.md` §0.10.8a and
  `crates/ad_events/`.
- The `e2e/` directory holds the automated end-to-end proofs. Run them with
  their scripts; each brings up a real stack and tears it down.
