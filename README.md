# AgentDialer

AgentDialer is a SIP-only, AI-agent-native, Kubernetes-native outbound dialer.
It is written in Rust. AI agents (LiveKit and pipecat) replace human agents. The
campaign and compliance "brain" is the product; the media plane reuses proven
engines.

`PLAN.md` holds the authoritative architecture. `.agents/REPO_GUIDE.md` holds
the working conventions. Read both before you change code.

## What it does

AgentDialer runs outbound dial campaigns:

- It selects leads from a hopper and paces calls per campaign.
- It gates every call on compliance (safe-hours, DNC). Compliance fails closed.
- It originates each call into a LiveKit room where an AI agent answers.
- It records outcomes, trips a circuit breaker on failure, and schedules
  retries.

It does NOT interface with the PSTN. It speaks SIP only. The AI agent is the
"human".

## Architecture

Two halves (see `PLAN.md` §0 and §0.10):

1. The media plane. Plumbing we reuse, not build:
   - `drachtio` for SIP signaling, `rtpengine` for RTP/SRTP media.
   - Our own SIP↔WebRTC bridge so LiveKit runs pure WebRTC rooms (`ad_gateway`).
   - A thin Rust control plane over these engines.

2. The campaign brain. The product:
   - campaigns, leads/lists, the hopper, pacing, dispositions, retries, DNC.
   - a FusionPBX-grade routing rule engine.
   - a Redis event layer that drives the orchestrator (no blind polling).

```
  customer (SIP/RTP)
     │  drachtio (SIP)  +  rtpengine (RTP/SRTP, transcode)
     ▼
  ad_gateway  ── SIP↔WebRTC bridge ──►  LiveKit room (WebRTC)
                                         └─ LiveKit / pipecat agent
```

The orchestrator is a singleton by Postgres advisory-lock leader election (not a
Kubernetes Service, not `replicas: 1`). It reacts to Redis events, claims the
hopper with `SELECT ... FOR UPDATE SKIP LOCKED`, dispatches through `ad_gateway`,
writes outcomes back, and schedules retries on Redis due-timers.

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
- `ad_dispatch` — the dial loop. A `CallInitiator` seam, RAII capacity slots,
  compliance re-gate at dispatch.
- `ad_gateway` — the real `CallInitiator`. It originates calls into LiveKit
  rooms, and owns the SIP↔WebRTC bridge (a native WebRTC participant).
- `ad_orchestrator` — the pure scheduling gate, the run loop, and the event
  reactor (schedule next batch / retry / pause / stop).
- `ad_events` — the Redis event layer: typed campaign events, a pub/sub
  transport, due-timers, and the persistent circuit-breaker store.
- `ad_db` — the Postgres data layer (sqlx), migrations, advisory-lock leader.
- `ad_api` — the HTTP API (axum), with a `router()` factory for in-process tests.
- `ad_bin` — the single binary `agentdialer`. Roles by subcommand: `api`,
  `orchestrator`, `dispatch`, `migrate`. One image, many roles.

## Build and test (Docker-first)

Everything builds and runs in containers, locally and in CI, identically. Do not
assume a host toolchain.

Run the full test tier (fmt + clippy + the suite, including the Postgres and
Redis integration tests):

```
docker compose -f docker-compose.test.yml up --build \
    --abort-on-container-exit --exit-code-from tests
docker compose -f docker-compose.test.yml down
```

Always stop the stack after tests.

## End-to-end tests

The `e2e/` directory holds automated end-to-end tests. Each brings up a real
stack, asserts behavior, and tears down.

- `e2e/run_e2e.sh` — the orchestrator wins leadership, claims the hopper, and
  `ad_gateway` originates a real call that lands in a LiveKit room with an agent
  subscriber receiving audio.
- `e2e/run_events_e2e.sh` — over real Redis + Postgres, failures trip the
  persistent circuit breaker (dialing pauses) and a retry is scheduled on the
  due-timer.
- `e2e/run_webrtc_e2e.sh` — our native WebRTC participant carries audio both
  ways into a LiveKit room with the agent, with no `livekit/sip`. CI runs this
  on every change.
- `spike/livekit/run_mvp_test.sh` — a real LiveKit agent wired into a room as a
  subscriber, then a SIP call driven into it (two-way audio proven).

## Deployment

`deploy/helm/agentdialer` is the Helm chart. The orchestrator is a leader-elected
Deployment (no Service). The gateway runs as `hostNetwork` media pods with an RTP
port range. The API has a Service and an HPA. A migrate Job runs as a pre-upgrade
hook.

## Status

Built and proven (Docker-first tests + e2e):

- campaign lifecycle, leads/lists, routing engine, hopper selection, compliance.
- the dial loop, the singleton orchestrator (leader election), call origination
  into a LiveKit room with a real agent subscriber.
- the Redis event layer: event-driven orchestrator, a persistent circuit breaker
  that trips, idempotent outcome write-back, and retry due-timers.
- the native WebRTC participant in `ad_gateway` (SIP↔WebRTC bridge, option 2 in
  `PLAN.md` §0.9a). Two-way audio is proven by `e2e/run_webrtc_e2e.sh`: our
  participant carries audio both ways into a LiveKit room with the agent, with no
  `livekit/sip`. The bridge is part of the normal build (no feature gate): the
  Docker test tier compiles, lints, and tests it, and CI runs the audio e2e.

In progress:

- wiring real rtpengine RTP into the WebRTC participant (the e2e uses a generated
  tone as the RTP source; connecting drachtio + rtpengine is the next media step).
- the orchestrator still originates calls through `CreateSIPParticipant`
  (`livekit/sip`). A `CallInitiator` over our bridge lands with the rtpengine
  wiring. Until then the bridge is proven by the e2e, not used by the dialer.

Not built yet:

- dispositions UI and reports, A/B traffic split, a full `livekit-agents` brain
  (STT/LLM/TTS). There is no authentication yet.
