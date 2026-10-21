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

Parts of this are not complete yet. See "Known limits" under "Status".

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
   - a Postgres event layer (an event table plus LISTEN/NOTIFY) that drives the
     orchestrator (no blind polling). There is no Redis in the product.

```
  customer (SIP/RTP)
     │  drachtio (SIP)  +  rtpengine (RTP/SRTP, transcode)
     ▼
  ad_gateway  ── SIP↔WebRTC bridge ──►  LiveKit room (WebRTC)
                                         └─ LiveKit / pipecat agent
```

The orchestrator is a singleton by Postgres advisory-lock leader election (not a
Kubernetes Service, not `replicas: 1`). It reacts to Postgres events, claims the
hopper with `SELECT ... FOR UPDATE SKIP LOCKED`, dispatches through `ad_gateway`,
writes outcomes back, and stores each retry on the lead row (`next_eligible_at`)
in the same transaction.

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
  reactor (schedule next batch / pause / stop), and the pure retry decision.
- `ad_events` — the event contract: typed campaign events, the publisher and
  consumer traits, the breaker-store trait, and in-memory fakes for tests.
- `ad_db` — the Postgres data layer (sqlx), migrations, advisory-lock leader,
  and the event layer: the `campaign_events` table with LISTEN/NOTIFY, and the
  circuit-breaker window over `call_outcomes`.
- `ad_api` — the HTTP API (axum), with a `router()` factory for in-process tests.
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
    -v "$PWD":/app -w /app agentdialer-tests:latest cargo fmt --all
```

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
| `AD_SIP_TRUNK_ID` | required | — | LiveKit outbound SIP trunk id. |
| `AD_ROOM_PREFIX` | optional | `campaign` | Room name prefix. |
| `AD_BATCH_LIMIT` | optional | `20` | Hopper rows claimed per batch. |
| `AD_CALLS_PER_SEC` | optional | `10` | Origination rate limit. |
| `AD_RETRY_SWEEP_MS` | optional | `1000` | Interval of the sweep that re-admits leads whose retry is due. |
| `AD_EVENT_SWEEP_MS` | optional | `2000` | The longest wait before the event table is read again, also when a notification is lost. |
| `HOSTNAME` | optional | `orchestrator` | Worker id written into hopper claims. |
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
docker compose up -d        # Postgres on 127.0.0.1:5433
docker build -t agentdialer:dev .
DB=postgres://agentdialer:agentdialer@127.0.0.1:5433/agentdialer
docker run --rm --network host -e DATABASE_URL=$DB agentdialer:dev migrate
docker run --rm --network host \
    -e DATABASE_URL=$DB \
    -e LIVEKIT_URL=ws://127.0.0.1:7880 -e LIVEKIT_API_KEY=... -e LIVEKIT_API_SECRET=... \
    -e AD_SIP_TRUNK_ID=ST_... \
    agentdialer:dev orchestrator
docker compose down
```

## End-to-end tests

The `e2e/` directory holds automated end-to-end tests. Each brings up a real
stack, asserts behavior, and tears down. The scripts need only bash and the
Docker CLI on the host: every other tool (HTTP polling, JSON, `psql`,
SIPp) runs in a container (`e2e/lib.sh`). Each script keeps the
orchestrator or probe log in `e2e/.out/` (git-ignored).

- `e2e/run_e2e.sh` — the orchestrator wins leadership, claims the hopper, and
  `ad_gateway` originates a real call that lands in a LiveKit room with an agent
  subscriber receiving audio.
- `e2e/run_events_e2e.sh` — over a real Postgres, failures trip the circuit
  breaker, each retry is stored on the lead, and a due retry is re-admitted and
  dialed again through LISTEN/NOTIFY.
- `e2e/run_webrtc_e2e.sh` — our native WebRTC participant carries audio both
  ways into a LiveKit room with the agent, with no `livekit/sip`.
- `spike/livekit/run_mvp_test.sh` — a real LiveKit agent wired into a room as a
  subscriber, then a SIP call driven into it (two-way audio proven).

CI runs the three `e2e/` scripts on every change (`.github/workflows/ci.yml`).
CI does not run the spike script.

## Deployment

`deploy/helm/agentdialer` is the Helm chart. The orchestrator is a leader-elected
Deployment (no Service). The gateway runs as `hostNetwork` media pods with an RTP
port range. The API has a Service and an HPA. A migrate Job runs as a pre-upgrade
hook. Set `database` and `livekit` (including `sipTrunkId`) in the
values; each takes an existing Secret or an inline value for development.
The values `orchestrator.retrySweepMs` and `orchestrator.eventSweepMs` set
`AD_RETRY_SWEEP_MS` and `AD_EVENT_SWEEP_MS`.

Caution: before you dial at volume, size the RTP port range. Each call takes one
even port. On `livekit/sip`, a port stays bound about 30 s after the call ends.
Ports in use = calls per second × (average call length + 30 s). Make the range
hold twice that number. Keep the range outside the host ephemeral range (the
Linux default is 32768–60999). The chart default `gateway.rtpPortStart` to
`gateway.rtpPortEnd` (30000–40000) overlaps it. `GAPS.md` OPS-6 and OPS-7 have
the measurements.

## Status

Built and proven (Docker-first tests + e2e):

- campaign lifecycle, leads/lists, routing engine, hopper selection, compliance.
- the dial loop, the singleton orchestrator (leader election), call origination
  into a LiveKit room with a real agent subscriber.
- the Postgres event layer: event-driven orchestrator (LISTEN/NOTIFY with a
  sweep), a circuit breaker over `call_outcomes` that trips, idempotent outcome
  write-back, and retries stored on the lead row.
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

Known limits (`GAPS.md` has the full list, by ID):

- Compliance data: production dialing uses a 24-hour call window and an empty
  DNC list. No table stores call times, holidays, or DNC entries yet (COMP-1).
  A lead without an explicit IANA timezone never dials (COMP-2).
- Dial rate: the orchestrator places one call at a time and waits for each
  answer. A SIPp benchmark (2026-10-20) measured 2.7 attempts per second at a
  configured 50 cps (DIAL-3).
- Answer: the gateway reports "answered" when LiveKit accepts the SIP
  participant, not when the callee answers (DIAL-1).
- Hopper: nothing fills the hopper from the API (LEAD-1). Claims never release
  (LEAD-3).
- Events: an event commits in a transaction apart from the state change that it
  announces. If the process stops between the two, a campaign can stall until
  the next restart. The event table has no retention yet (ORCH-2).
- Agents: the dialer dispatches no agent into the per-lead rooms yet (AGNT-1).
- `POST /v1/campaigns/:id/dispatch/run-batch` is experimental. It uses a stub
  initiator and its response says so (`experimental: true`).

Not built yet:

- dispositions UI and reports, A/B traffic split, a full `livekit-agents` brain
  (STT/LLM/TTS). There is no authentication yet.
- agent adapters for LiveKit and pipecat, driven by a per-attempt statechart
  that is saved in Postgres (decision D-2), and one gRPC session stream per
  agent session for outcomes, events, and logs, with OpenTelemetry on the same
  endpoint (decision D-6). `GAPS.md` lists every open gap.
