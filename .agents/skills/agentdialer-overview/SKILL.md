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
     (`PLAN.md` §0.9a). This bridge is not spiked yet. It is an open risk.

2. The campaign brain (`PLAN.md` §0.10). This is the actual product:
   - campaigns, lists, the hopper (lead selection), pacing, dispositions,
     callbacks, recycle/retry, DNC, and the abandonment ledger.
   - a routing rule engine (`ad_routing`) modeled on the FusionPBX dialplan.
   - compliance: safe-hours, DNC, abandonment rate.

## The crates

The workspace is a set of focused crates under `crates/`. One crate = one
concern. Current crates:

- `ad_core` — foundation. Typed ids, errors, and a `Clock` trait. The `Clock`
  trait makes time-dependent logic deterministic under test.
- `ad_compliance` — safe-hours gate, DNC scrubbing, abandonment ledger. Pure.
  Fails closed. See the `compliance-engine` skill.
- `ad_api` — the HTTP API (axum). A `router()` factory makes the API testable
  in process. See the `vertical-slice` skill.
- `ad_bin` — the single binary `agentdialer`. It runs roles by subcommand:
  `api`, `orchestrator`, `dispatch`, `migrate`. One image, many roles.

Planned crates (see `PLAN.md` §0.10.12 and §0.7): `ad_campaign`, `ad_hopper`,
`ad_routing`, `ad_pacing`, `ad_disposition`, `ad_concurrency`,
`ad_orchestrator`, `ad_dispatch`, `ad_reports`, and the media crates
`ad_sip_ctl`, `ad_rtpengine_ctl`, `ad_router`, `ad_gateway`, `ad_sipmsg`.

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

Run these commands. They must pass:

```
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
cargo test --all
```

If you did not run the tests, the work is not done.

## Where to go next

- To change the dialer core, read `PLAN.md` §0.10 and the `routing-engine` skill.
- To change compliance, read the `compliance-engine` skill.
- To add an API endpoint, read the `vertical-slice` skill.
- To work on the media plane, read `PLAN.md` §0 and the spike in `spike/`.
