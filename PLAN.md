# AgentDialer — Architecture & Implementation Plan

A SIP-only, AI-agent-native, Kubernetes-native outbound dialer with VICIdial
feature parity — minus all PSTN/phone-line interfacing. Written in Rust.
AI agents (LiveKit + pipecat) replace human agents entirely.

Status: planning. Nothing in `src/` is built yet. This document is the contract
for what gets built and in what order. It is grounded in direct study of the
reference codebases in `_research/` (see "Research basis").

> **Reading order note.** §0 is the current, authoritative decision on the media
> plane (added after protocol research). Where §0 conflicts with the original
> §4/§5/§8/§11, **§0 wins**; those sections are being migrated to match and are
> kept for their still-valid campaign/compliance detail. The campaign +
> compliance "brain" (§3, §6, §9) is unchanged and is the real product — it sits
> **on top of** the media plane described in §0.

---

## 0. Decision record — the media plane (CHOSEN)

### 0.1 Decision

Build a **Rust control plane that drives reused, battle-tested C media/signaling
engines** — not a from-scratch SIP/RTP stack, and not FreeSWITCH-in-front-of-
LiveKit. Concretely, learn from how **jambonz** composes its stack and reuse the
**same underlying engines**:

- **drachtio-server** (SIP signaling; wraps **Sofia-SIP**) — the SIP transaction/
  dialog engine. **Reused as-is.**
- **rtpengine** (RTP/SRTP media relay, with a **Linux kernel module** for
  in-kernel packet forwarding) — the media plane. **Reused as-is.**
- **LiveKit SIP** and **pipecat SIP** — the AI agent legs, attached as **SIP
  endpoints** that our core bridges to (not a second switch).

Our Rust code is the **control plane + B2BUA/routing logic + the entire campaign
and compliance brain on top**. We write **zero** SIP wire-stack and **zero**
RTP/media-forwarding code.

```
                              EKS
  ┌──────────────────────────────────────────────────────────────┐
  │  CONTROL PLANE (Rust, normal autoscaled Deployments)           │
  │                                                                │
  │   Campaign + Compliance brain  ← THE PRODUCT (built on top)    │
  │   (campaigns, hopper, TCPA/DNC, pacing, dispositions, …)       │
  │              │ drives                                          │
  │   call-router / B2BUA (Rust)                                   │
  │        │ drachtio control         │ rtpengine ng control       │
  │        │ (TCP len#msg)            │ (JSON/UDP cookie)           │
  └────────┼──────────────────────────┼────────────────────────────┘
           ▼                           ▼
     DATA PLANE (reused C, hostNetwork node pools)
     drachtio-server ──SIP──►  rtpengine ──RTP/SRTP──►  { customer leg (BYO SIP trunk, downstream)
     (Sofia-SIP)              (kernel mod)               agent leg → LiveKit SIP / pipecat SIP }
```

### 0.2 Why (the build-vs-reuse line, with evidence)

Verified by direct source study in `_research/` (drachtio-srf, drachtio-server,
rtpengine):

- **rtpengine ships a Linux kernel module** for in-kernel RTP forwarding plus a
  **~61,000-line C userspace daemon**. Re-implementing a media engine is a
  multi-year specialist product and the one place bugs are catastrophic
  (one-way audio, interop, security). We reuse it. Non-negotiable.
- **drachtio-server wraps Sofia-SIP**, ~20 years of SIP interop maturity. We
  reuse it.
- What jambonz actually did was **orchestrate these from a high-level app** and
  ship **EKS packaging**. That orchestration layer — in Rust, EKS-native,
  dialer-shaped, no commercial license — is the valuable, tractable thing to
  build. (Interpretation B, chosen. Interpretation A = "write our own SIP/RTP
  engine" = explicitly rejected.)

### 0.3 The two control protocols are small and confirmed tractable

**drachtio** (from `drachtio-srf/lib/wire-protocol.js`, `drachtio-agent.js`):
plain **TCP**, framing `<byteLength>#<msgId>|<message>`; handshake
`authenticate|<secret>|<tags>`; send SIP as `sip|<stackTxnId>|<stackDialogId>\r\n
<raw SIP>`; server pushes `sip`/`route`/`cdr`/`response`. A Rust client is a
`tokio-util` length-delimited-ish codec + token split + a SIP message model.
B2BUA = `createUAS` + `createUAC` + bridge — *that composition is our logic to
re-implement in Rust*; drachtio does the hard SIP transaction/dialog work.

**rtpengine ng** (from `docs/ng_control_protocol.md`,
`docs/http_websocket_support.md`): plain **UDP/TCP/HTTP/WS**, framing
`<cookie> <dict>`; the dict is **bencode on the UDP ng transport** for the
shipping rtpengine (mr9.4) — see the spike finding below; JSON is accepted on
the HTTP transport. We use a ~120-line in-house bencode codec (no crate needed).
Commands are exactly the media primitives we need:
`offer`/`answer`/`delete`/`query`/`start|stop|pause recording`/`block DTMF`/
`play media`. A Rust client is a `tokio` UDP socket + bencode + cookie
bookkeeping (~200 lines).

### 0.3.1 SPIKE VERDICT — the thesis is VALIDATED (fail-fast proof done)

Before committing to the plan we ran a throwaway spike (`spike/`) against **real
drachtio + rtpengine in Docker** and proved, from Rust, the entire risky core.
All three steps **PASS** and are reproducible:

| Spike | Result | Evidence |
|---|---|---|
| `ad_rtpengine_ctl` (ng over UDP) | **PASS** | `ping→pong`; `offer`→rewritten SDP with `c=IN IP4 127.0.0.1` + relay port `m=audio 300xx` + `a=rtcp`; `answer`; `delete→ok`. |
| `ad_sip_ctl` (drachtio control TCP) | **PASS** | connect + `authenticate`→`OK` + server hostports + version `v0.9.11`. |
| `ad_router_spike` (decisive) | **PASS** | rtpengine `offer`→relay port → drachtio **originates a real INVITE** (UAS logged `INVITE … from 127.0.0.1:5060`, answered `200 OK`) → our frame tap caught the `200 OK` SDP → rtpengine `answer`→both legs relayed → `delete`. **Rust orchestrated SIP origination + media bridging end to end.** |

Two findings corrected plan assumptions (cheaply, as intended):

1. **rtpengine UDP ng speaks bencode, not JSON** (mr9.4 returned
   `Could not decode dictionary` to JSON). Fix: a tiny in-house bencode codec
   (`spike/ad_rtpengine_ctl/src/bencode.rs`). JSON remains an option only on the
   HTTP transport. The §0.3 text above now reflects this.
2. **drachtio response-frame correlation** uses `token[2]` (the original msgId),
   not `token[0]` (a server-generated id); the leader is
   `<serverMsgId>|response|<originalMsgId>|OK|<hostports>|<version>|…`. Our
   client keys pending requests on `token[2]` for `response` frames.

Conclusion: the "**Rust control plane over reused drachtio + rtpengine**" thesis
holds. Both novel control clients are small (~100–200 lines each) and work
against the real engines. The architecture is cleared to proceed to real
implementation (Phase 0+). Spike code is throwaway; the production crates
(`ad_sip_ctl`, `ad_rtpengine_ctl`, `ad_router`) will be built properly per §0.7.

### 0.4 What we build vs. reuse

| Component | Build / Reuse | Notes |
|---|---|---|
| SIP signaling engine | **Reuse** drachtio-server (Sofia-SIP) | zero wire-stack code by us |
| RTP/SRTP media engine | **Reuse** rtpengine (+ kernel module) | zero media code by us |
| AI agent legs | **Reuse** LiveKit SIP + pipecat SIP | SIP endpoints we bridge to |
| drachtio control client (Rust) | **Build** (`ad_sip_ctl`) | new; only Node/Python clients exist today |
| rtpengine ng control client (Rust) | **Build** (`ad_rtpengine_ctl`) | new; JSON/UDP, small |
| B2BUA / call-router (Rust) | **Build** (`ad_router`) | originate customer leg, bridge to agent leg, offer/answer via rtpengine |
| EKS packaging (day zero) | **Build** | copy jambonz node-pool pattern (below) |
| Campaign + compliance brain | **Build** (the product) | §3, §6, §9 — sits on top, unchanged |

### 0.5 EKS-native from day zero (copy jambonz's proven pattern)

jambonz's EKS Helm chart already solved the real-time-media-on-Kubernetes
problem; we copy the pattern:

- **SIP node pool**: drachtio pods, `hostNetwork: true`, pinned to nodes with
  stable addressing. SIP needs real IPs because addresses live inside SIP/SDP.
- **RTP node pool**: rtpengine pods, `hostNetwork: true` + **AWS Elastic IPs** +
  a **wide UDP RTP port range** opened in the security group; kernel module
  requires a privileged/host context and a compatible node image.
- **Control-plane pool**: our Rust services (router, api, orchestrator, dispatch)
  + the campaign/compliance brain run as **ordinary autoscaled Deployments**
  (HPA/KEDA) — they hold no media and scale freely.
- Graceful drain on scale-down for SIP/RTP pods (never kill a pod mid-call):
  `preStop` + long `terminationGracePeriodSeconds` + readiness gating.

### 0.6 Honest risk statement

The **individual pieces are battle-tested** (drachtio/Sofia-SIP, rtpengine,
LiveKit SIP, pipecat). The **exact trio on EKS is not a published, turnkey
stack** — we are the integrator. The least-trodden, highest-effort part is
**real-time media networking on EKS** (hostNetwork, EIPs, RTP port ranges, call
draining, kernel-module nodes), **not** the dialer logic. The new-but-small code
risk is the two Rust control clients, de-risked by the fact that both protocols
are tiny and we have working Node/C references in `_research/` to conform to.

### 0.7 Revised crate additions (supersede the media parts of §5)

Add to the Rust workspace a **media control-plane layer** beneath the campaign
brain:

```
crates/
  ad_sip_ctl/        # drachtio control client: tokio codec, auth, SIP send/recv,
                     #   stackTxnId/stackDialogId tracking. (NEW, small)
  ad_rtpengine_ctl/  # rtpengine ng client: UDP + serde_json + cookie match.
                     #   offer/answer/delete/record/play/dtmf. (NEW, small)
  ad_sipmsg/         # SIP message + SDP model (parse/build/mutate headers).
                     #   Evaluate `rsip`/`rsipstack` first; thin in-house only if needed.
  ad_router/         # B2BUA / call-routing: on customer-leg INVITE or on
                     #   originate, run rtpengine offer, bridge to agent SIP leg
                     #   (LiveKit/pipecat), rtpengine answer, manage dialog + teardown.
                     #   Exposes initiate_call()/hangup()/transfer() to the dialer.
```

The dialer's existing `ad_telephony` provider abstraction (§5) keeps its trait,
but the first concrete provider becomes **`providers/drachtio_rtpengine`** (our
core), with `livekit`/`sip_generic` as *agent-leg* egress targets the router
bridges to. The campaign/compliance crates (`ad_compliance`, `ad_hopper`,
`ad_pacing`, `ad_concurrency`, `ad_orchestrator`, `ad_dispatch`, `ad_api`,
`ad_reports`) are **unchanged** and call the router through `initiate_call`.

### 0.8 Infra reuse for the control plane (per the "buy, don't build" directive)

Confirmed choices (replacing earlier hand-rolled bits): **Postgres as the single
coordination substrate** — `sqlx`; **`FOR UPDATE SKIP LOCKED`** for lead/run
claiming (no custom slot manager); **`pg_try_advisory_lock`** for the singleton
orchestrator leader (no custom lease); **`apalis` (apalis-sql)** or **`pgmq`**
for the durable job/retry/callback queue; `tokio-cron-scheduler` for sweeps.
Redis becomes **optional** (only for high-fanout pub/sub later). This keeps the
custom code confined to domain logic + the two tiny protocol clients.

### 0.9 Open questions this decision creates

1. `rsip`/`rsipstack` maturity for our SIP-message mutation needs vs. a minimal
   in-house model — decide in the `ad_sipmsg` phase by trying `rsip` first.
2. Do we also reuse **FreeSWITCH** (as jambonz does) for heavy media/IVR/TTS, or
   is rtpengine relay + LiveKit/pipecat agents sufficient? Default: **no
   FreeSWITCH** — rtpengine relays, agents do all media intelligence. Add
   FreeSWITCH only if a concrete IVR/conferencing need appears.
3. drachtio + rtpengine run as our own pods (full control) vs. consuming
   jambonz's charts — default: **our own minimal pods**, copying jambonz's
   networking pattern but not its Node app or license surface.

### 0.9a The SIP↔WebRTC boundary — we own the gateway (LiveKit just runs rooms)

**Decision (per explicit direction).** We do **not** depend on LiveKit's own
`livekit/sip` gateway. Our media plane owns the SIP↔WebRTC translation so
LiveKit is reduced to **pure WebRTC rooms + agents**. This keeps the telephony
intelligence (SIP interop, media, DTMF, transcoding, and crucially our
compliance-bearing call control) inside our product, with LiveKit as a
swappable agent runtime behind one internal boundary.

**Why this matters (verified).** LiveKit SIP support shipped in Dec 2023 as the
standalone **`livekit/sip`** service (livekit/livekit#1394 → `github.com/livekit/sip`):
a gateway (like Ingress/Egress) that terminates SIP + RTP, transcodes via
**libopus**, talks to livekit-server over **Redis**, and joins the SIP caller
into a room as a participant. That is exactly the box we are choosing to own
ourselves instead of adopting — otherwise call control, pacing, and compliance
would straddle a third-party service we don't control.

**What this means for the architecture.** Our core is therefore not *only* a SIP
B2BUA — it must also present a **WebRTC leg into a LiveKit room**. Concretely the
media path becomes:

```
  customer (SIP/RTP)
     │  drachtio (SIP signaling)  +  rtpengine (RTP/SRTP, DTMF, transcode)
     ▼
  ad_gateway  ──── our SIP↔WebRTC bridge ────►  LiveKit room (WebRTC)
                                                 └─ pipecat/LiveKit agent
```

- The **SIP + RTP + transcoding** half is drachtio + rtpengine (reused, proven in
  the §0.3.1 spike).
- The **WebRTC-into-LiveKit** half is new: our `ad_gateway` publishes/subscribes
  audio to a LiveKit room as a participant (via LiveKit's server API + a WebRTC
  transport), bridging rtpengine's RTP to the room's WebRTC tracks.

**New risk this introduces (and how we treat it).** The WebRTC egress leg is a
*new* unproven piece — strictly larger than "drive drachtio+rtpengine," which is
all the §0.3.1 spike proved. Honest status: **the SIP↔WebRTC bridge is NOT yet
spiked.** Options to evaluate, cheapest first, in a dedicated follow-up spike
before committing the production `ad_gateway`:

1. **rtpengine ↔ LiveKit WebRTC directly.** rtpengine speaks ICE/DTLS-SRTP and
   can bridge RTP to a WebRTC endpoint; drive LiveKit's room join via its API and
   hand rtpengine the room's WebRTC SDP (ICE/DTLS). Likely the least new code.
2. **A Rust WebRTC participant** (`webrtc` crate / `livekit` Rust SDK) that
   pulls RTP from rtpengine and publishes it as a room track. More code, more
   control.
3. **Fallback (not preferred):** run `livekit/sip` after all. Kept only as the
   escape hatch if (1)/(2) prove too costly — explicitly the thing we're trying
   to avoid, but it de-risks the schedule.

**Interim stance.** Until the bridge is spiked, the §0.3.1 result stands as "SIP
origination + media relay proven"; the LiveKit WebRTC leg is an **open
integration risk** tracked here, not a solved one. The campaign/compliance brain
(§0.10, below) is unaffected — it sits above `ad_gateway.initiate_call()`
regardless of how the WebRTC leg is implemented.

**Crate impact.** Add `ad_gateway` (the SIP↔WebRTC bridge orchestrator) to the
media-plane crates in §0.7; it owns the room-join + RTP↔WebRTC wiring and is the
concrete implementation behind the dialer's `initiate_call`. `ad_router`
(SIP-only B2BUA) remains for pure SIP-endpoint agent legs (e.g. a pipecat SIP
worker) where no WebRTC bridge is needed.

---

## 0.10 The Campaign & Dialer subsystem — THE PRODUCT (full spec)

> This is the actual product. The media plane (§0) is plumbing we reuse; **this
> section is what we are uniquely building** and it carries equal (greater, in
> truth) specification weight. It is grounded in direct study of VICIdial
> (`AST_VDhopper.pl`, the 345-table schema), dograh's campaign engine, and
> **FusionPBX's dialplan engine** (`app/dialplans`, studied in docs + code) for
> routing expressiveness. Schema detail lives in §6; call flow in §7; compliance
> in §9 — this section is the authoritative conceptual model they implement.

### 0.10.1 The three layers of the brain

```
  ┌───────────────────────────────────────────────────────────────────────┐
  │ CAMPAIGN LAYER   what to dial, when, how fast, under what rules         │
  │   campaigns · lists · hopper (lead selection) · pacing · schedule ·     │
  │   dispositions · callbacks · recycle/retry · DNC · abandonment ledger   │
  ├───────────────────────────────────────────────────────────────────────┤
  │ ROUTING LAYER    decide, per call/lead/outcome, what happens next       │
  │   a FusionPBX-grade RULE ENGINE: ordered, grouped condition/action/     │
  │   anti-action rules over a variable context, with a function library.   │
  │   Used for: lead→(trunk,caller-id,agent) · outcome→(retry/callback/DNC/ │
  │   next-list/transfer) · inbound-DID→campaign · time-of-day gating.      │
  ├───────────────────────────────────────────────────────────────────────┤
  │ EXECUTION LAYER  turn a decision into a call                            │
  │   ad_router.initiate_call() over the §0 media plane; agent leg =        │
  │   LiveKit/pipecat; outcome captured back as a disposition.              │
  └───────────────────────────────────────────────────────────────────────┘
```

### 0.10.2 Campaign model

A **campaign** is the unit of outbound work. Fields (full DDL in §6):

- Identity/tenancy: `id`, `org_id`, `name`, `state`
  (`draft→syncing→running⇄paused→completed/failed`).
- Agent binding: `agent_ref` (LiveKit/pipecat workflow) + optional **A/B
  traffic split** across agent versions (weights sum to 100; from dograh/QuickVoice).
- Lists: one or more via `campaign_lists` with **list-mix ratios**, active flag,
  and expiration (VICIdial list-mix + list expiration).
- Pacing: `dial_strategy` (default `progressive`; see 0.10.6), `concurrency_cap`.
- Scheduling: `schedule_config` (timezone + per-day/-time slots) **and** a
  `call_time_id` referencing a safe-hours window set (0.10.9 / §9).
- Outcome policy: `retry_config`, `recycle_config`, `callback_config`,
  `dnc_scope`, `abandon_jurisdiction`, and a bound **routing ruleset**
  (`outcome_ruleset_id`, `selection_ruleset_id`).
- Counters/limits: `call_count_limit`, `call_limit_24hour`, total/processed/
  failed rows.

### 0.10.3 Lead model & lists

Lead = `vicidial_list` analog (full columns in §6). The compliance-bearing core:
`phone_number`, `state`, `postal_code`, `country_code`, `gmt_offset_now`,
`called_since_last_reset` (enum `Y/N/Y1..Y10/D`), `called_count`,
`last_local_call_time`, `rank`, `status`, `owner`, plus an arbitrary `context`
JSONB (CSV extra columns → agent template variables, dograh-style). Lists carry
ordering, filters, expiration, and list-mix weight.

### 0.10.4 The hopper (lead selection) — algorithms

The **hopper** is the staging set of leads chosen *to be dialed next*, rebuilt
continuously (VICIdial's `AST_VDhopper.pl`, ~4k lines, re-expressed as the
`ad_hopper` crate). It is a **registry of hopper sources**, each a strategy that
contributes candidate leads, merged and ordered:

| Source (registry) | Behavior |
|---|---|
| `Standard` | New/eligible leads by configured status set. |
| `ScheduledCallback` | Due callbacks, **elevated priority** (agent-specific + anyone). |
| `Recycle` | Leads of configured statuses re-admitted after a cooldown. |
| `XthNew` | Nth-new ordering (last-name/phone/entry sort variants). |
| `AutoAltDial` | Alternate-phone dialing for a lead (alt_phone, address phones). |
| `ApiLoad` | Externally pushed leads (non-agent API). |
| `DropRun` / `NoHopperQueue` | Direct-insert variants. |

**Lead ordering** (composable, per campaign): `rank` (operator priority), then a
primary order (down count / up count / last-name / phone / entry / **random** /
**timezone-aware**), then a secondary tiebreak. Timezone ordering dials leads
whose local window opens/closes soonest first (compliance-aware throughput).

**Eligibility gate at selection** (Alexor-Brain's NOT-IN pattern, hardened):
exclude leads that are completed, retries-exhausted, in-progress, within
cooldown, DNC-matched, over `call_count_limit`/`call_limit_24hour`, out of the
list, or **outside their safe-hours window right now**. Re-checked again at
dispatch (§7) because the clock moves.

**Claiming** (per §0.8): workers pull hopper rows with `FOR UPDATE SKIP LOCKED`
— no custom slot manager, crash-safe, no double-dial.

### 0.10.5 The routing rule engine (FusionPBX-grade) — `ad_routing`

This is the piece that captures FusionPBX's dialplan richness. It is a **typed,
data-defined rule engine**, not XML and not Lua. A **ruleset** is an ordered list
of rules; each rule is an ordered list of **details** grouped into condition
blocks — a faithful re-expression of FusionPBX's `dialplan` / `dialplan_detail`
model (studied in `_research/fusionpbx/app/dialplans`).

Detail model (mirrors `dialplan_detail` columns exactly):

- `tag`: `condition | action | anti_action`.
- `type`: the field to test (condition) or the verb to run (action).
- `data`: a regex (condition) or an argument string (action), both subject to
  variable interpolation.
- `break`: `on_true | on_false | always | never` — flow control between groups.
- `inline`: evaluate immediately during pass (so a later condition can read a
  variable a prior action set) vs. deferred to execution.
- `group`: integer grouping rows into **OR-of-AND condition blocks**.
- `order`: evaluation order.

Semantics (exactly FusionPBX, verified from the Call-Forward / User-Record /
Outbound-Route examples):

- Within a group, all `condition` rows are **ANDed**; capture groups (`$1`,`$2`)
  from condition regexes are bound into the context.
- `action` rows in a matched group run; `anti_action` rows run when the group's
  condition is **false** (the if/else).
- `break` governs whether evaluation stops or falls through to the next group —
  letting one ruleset encode a decision matrix (FusionPBX's User-Record uses 11
  groups; Call-Forward uses 4 for `*72/*73/*74/toggle`).
- `continue` chaining across rulesets mirrors `dialplan_continue`.
- **Multi-tenant scoping**: `global` rulesets apply to all orgs; `org`-scoped
  rulesets override — exactly FusionPBX global vs domain-specific dialplans.

**Variable context + function library.** Rules evaluate against a `Context` =
a string→string variable map (lead fields, call vars, campaign settings, capture
groups). Interpolation supports `${var}`, `$${global}`, capture refs, and a
**registry of pure functions** re-expressing the FusionPBX API surface we need —
e.g. `cond(x==y ? a : b)`, `regex(...)`, `strftime(...)`, `hash(...)`, plus our
own `lead_data(...)`, `tz_now(...)`, `dnc_check(...)`. Functions are a registry
(add one = one module + one registration line; no schema change), **pure and
sandboxed** — unlike FusionPBX we do **not** embed Lua or shell-out.

**Where the engine is used (the four rulesets):**

1. **Selection/decoration ruleset** (lead → call setup): choose trunk/egress,
   set `effective_caller_id_*` (incl. CID group rotation), export call
   direction, apply per-lead overrides — the AgentDialer analog of FusionPBX's
   **Outbound Route** (`condition dest ^+?1?(\d{10})$ → set caller-id → bridge
   gateway/$1` with failover).
2. **Outcome ruleset** (disposition → next action): the heart of the dialer's
   intelligence (0.10.7).
3. **Inbound ruleset** (DID → campaign/agent) for inbound/blended later.
4. **Time-condition rulesets** (0.10.9).

**Actions are typed and call our primitives**, not FreeSWITCH verbs. The action
registry includes: `set`/`unset`/`export` (context vars), `select_trunk`,
`set_caller_id`, `bridge_agent` (→ `ad_router.initiate_call`), `transfer`
(att/blind to another agent or SIP endpoint), `schedule_callback`, `set_status`,
`add_dnc`, `recycle`, `set_record`, `limit` (concurrency/rate cap), `hangup`,
`log`. Adding an action = one module + one registration line.

### 0.10.6 Pacing / dial strategies — `ad_pacing`

A **registry of dial strategies** (re-expressing VICIdial's `dial_method` enum,
reframed for elastic AI agents — see §3.3):

- `Progressive` (**default**): place a call only when a concurrency slot is
  free. With elastic AI agents this yields **~0% abandonment** mechanically.
- `Power(ratio)`: dial `ratio × free_slots` to absorb no-answers; bounded so the
  abandonment ledger (§9) can never exceed the jurisdiction cap.
- `Predictive` (optional, later): capacity-aware pacing with the circuit breaker
  and abandonment ledger as a hard safety governor. We do **not** port
  VICIdial's `AST_VDadapt.pl` math; we re-derive a simpler capacity model.

Concurrency itself is Postgres-backed (§0.8): per-campaign cap enforced inside
the claiming transaction; the "slot" releases when the run row goes terminal.

### 0.10.7 Disposition state machine & outcome routing

Every call attempt ends in a **disposition** (system + per-campaign custom
statuses, with categories — VICIdial `vicidial_statuses` /
`vicidial_campaign_statuses` / `status_categories`). The agent layer
(LiveKit/pipecat) reports an **answer classification** (human / voicemail /
no-answer / busy / failed) and a **disposition code**; both land on the `run`.

The **outcome ruleset** (0.10.5) then decides the next action for the lead. This
is where FusionPBX's grouped condition/action model earns its keep — operators
express rich matrices like:

```
group 0: condition status ^SALE$              → set_status SALE; (done)
group 1: condition status ^(NA|B)$            → recycle after=60m max_attempts=3
group 2: condition status ^CALLBK$            → schedule_callback ${callback_at}
group 3: condition status ^DNC$               → add_dnc scope=campaign; set_status DNC
group 4: condition answer_class ^voicemail$   → set_status VM; recycle after=240m
group 5: (anti) default                       → recycle after=30m
```

The disposition→lead-status transitions form the **state machine**
(`NEW→QUEUED→INCALL→{DISPOSITIONED}→{RECYCLE|CALLBACK|DONE|DNC}`), with
`called_since_last_reset`/`called_count`/`last_local_call_time` updated per
VICIdial semantics so recycle/limit rules stay correct.

### 0.10.8 Callbacks, recycle, retry

- **Scheduled callbacks**: agent-specific or "anyone"; activate into the hopper
  at their due time with elevated priority (VICIdial `vicidial_callbacks`).
- **Recycle**: re-admit leads of configured statuses after a cooldown
  (`recycle_config`; Alexor-Brain cooldown + VICIdial `lead_recycle`).
- **Retry**: `retry_config` (max attempts, interval, per-status rules) → due-at
  re-queue, carried by the durable job queue (`apalis`/`pgmq`, §0.8).

### 0.10.9 Time conditions (first-class)

FusionPBX models time-of-day/date/day-of-week routing as **time conditions** with
match and anti-match targets. We make this first-class and distinct from
compliance safe-hours (§9), because they serve different purposes:

- **Compliance safe-hours** (§9) = legal gate, **fails closed**, per-lead local
  time. Non-negotiable.
- **Business time conditions** = operator routing ("after 5pm send inbound to
  voicemail", "run Campaign A only Mon–Fri 9–5"), expressed as time-condition
  rules in the routing engine, feeding `schedule_config`.

Both evaluate against the same `tz_now()`/`strftime()` function surface.

### 0.10.10 Named destinations & reusable targets

FusionPBX exposes dialplans with a `dialplan_destination` flag as selectable
**destinations** reusable across routes. We mirror this: a `destinations`
registry of named, reusable routing targets (an agent/workflow, a SIP endpoint,
a transfer target, a sub-ruleset) referenced by rules and campaigns — so
operators compose routing from named building blocks rather than copy-pasting.

### 0.10.11 Multi-tenancy

Org-scoped throughout (campaigns, lists, leads, rulesets, destinations,
dispositions, DNC). Rulesets and destinations additionally support a **global**
scope that an org scope overrides — the FusionPBX global/domain pattern — so a
provider ships sane defaults and tenants customize.

### 0.10.12 Crates for the brain (fits the §5 workspace)

```
crates/
  ad_campaign/    # campaign aggregate + lifecycle state machine + list-mix.
  ad_hopper/      # hopper sources registry + lead ordering + eligibility gate.
  ad_routing/     # the FusionPBX-grade rule engine: ruleset/rule/detail model,
                  #   condition/action/anti-action/break/inline/group evaluator,
                  #   variable interpolation + function registry + action registry.
  ad_pacing/      # dial-strategy registry (progressive/power/predictive).
  ad_disposition/ # status/disposition model + lead state machine + outcome apply.
  ad_compliance/  # §9 — safe-hours (fail-closed), DNC, abandonment ledger.
  ad_concurrency/ # Postgres SKIP LOCKED claim + per-campaign cap (§0.8).
  ad_orchestrator/# singleton (pg advisory lock): schedule, batch, circuit breaker.
  ad_dispatch/    # claim → compliance gate → selection ruleset → router.initiate.
  ad_reports/     # stats, funnels, exports.
```

`ad_routing` is the keystone and the most novel crate — it is how we deliver
VICIdial + FusionPBX-grade operator control **without** FreeSWITCH's XML/Lua, as
typed Rust evaluating data-defined rules stored as `routing_rules` /
`routing_rule_details` (JSONB discriminators; no migration to add a
field/verb/function).

### 0.10.13 Acceptance (what "the brain works" means)

A campaign driven end-to-end: leads load → hopper fills with correct ordering and
eligibility → selection ruleset sets trunk + caller-id → progressive pacing dials
under cap and only inside safe-hours → agent converses → disposition reported →
outcome ruleset routes to recycle/callback/DNC/done correctly → retries/callbacks
re-queue at the right time → abandonment ledger stays at ~0% → reports reconcile
with the ledger. The routing engine has property tests over group/break/inline
ordering against the FusionPBX reference examples (Call-Forward, User-Record,
Outbound-Route) re-expressed as our rules.

---

## 1. Thesis

VICIdial is three things fused into one Perl/PHP/Asterisk monolith:

1. A **campaign engine** — lead lists, hopper (lead selection), lead ordering,
   recycling, filters, callbacks, dispositions, DNC, timezone/safe-hours
   compliance, reporting.
2. A **pacing engine** — predictive/adaptive algorithms that over-dial to keep
   scarce *human* agents busy, bounded by a legally-mandated abandonment cap.
3. A **telephony stack** — Asterisk placing/bridging calls over PSTN carriers,
   with humans logging into a browser agent screen.

AgentDialer keeps (1), **deletes** (3), and **fundamentally reframes** (2):

- **Agents are AI, elastic, and spun up per answered call.** The scarcity that
  forces predictive over-dialing disappears. We dial *progressively/power* —
  only place a call when agent capacity is guaranteed — making abandonment
  mechanically ~0%. We keep the abandonment *accounting and caps* for legal
  reporting, but the pacing problem itself largely dissolves.
- **No PSTN interface.** AgentDialer speaks **SIP only**. It originates calls to
  a SIP endpoint (LiveKit SIP, a SIP PBX, or a BYO-carrier SIP trunk). Whether
  the far end is another SIP endpoint or a telephone number reached via a
  carrier's SIP trunk is the carrier's concern, downstream of us. We never
  implement a telephone-network interface.
- **The agent is LiveKit + pipecat.** AgentDialer originates the call leg and
  dispatches a LiveKit agent into a room (or routes an answered leg to a SIP
  endpoint that a pipecat/LiveKit worker owns). The conversation, barge-in,
  tools, RAG, voicemail detection, and transfer all live in the agent layer.

The result is "VICIdial's campaign brain, re-expressed for an all-AI, SIP-only,
cloud-native world," with dograh's proven AI-agent campaign architecture as the
structural template and VICIdial's compliance data model as the rulebook.

---

## 2. Research basis (what this plan is grounded in)

Cloned/downloaded under `_research/`:

| Source | What we took from it |
|---|---|
| **VICIdial** `astguiclient-2.14-960a` (`_research/vicidial/`) | Feature set, 345-table schema, dialing modes, the hopper model, the compliance data model (timezone/safe-hours/DNC), the remote-agent seam. |
| **dograh** `dograh-hq/dograh` (`_research/dograh/`) | The structural template: campaign orchestrator, Redis-backed concurrency slots, telephony provider registry, circuit breaker, scheduling, dispositions, Helm chart, per-dir `AGENTS.md` conventions. The single most relevant reference. |
| **Alexor-Brain** (`_research/Alexor-Brain/`) | Minimal eligible-contact selection SQL (NOT-IN subqueries for completed/exhausted/in-progress/cooldown), LISTEN/NOTIFY wakeups, orphaned-call cleanup on pause. |
| **outbound-caller-python** (`_research/outbound-caller-python/`) | The LiveKit agent-side reference: `CreateSIPParticipant` origination, `transfer_sip_participant`, voicemail detection, end-call playout drain. |
| **QuickVoice** (`_research/QuickVoice/`) | Campaign experiment/variant/conversion data modeling; campaign-schedule modeling. |
| **Bun** `oven-sh/bun` CLAUDE.md | Rust monorepo conventions: Cargo workspace of focused crates, RAII/Drop, registry-driven extensibility, honest status, test-next-to-code with real verification. |
| **drachtio-srf / drachtio-server** (`_research/drachtio-srf`, `_research/drachtio-server`) | The SIP control-channel wire protocol (`len#msgId|verb…\r\n<SIP>`), `authenticate` handshake, `createUAS`/`createUAC`/B2BUA primitives. Confirms a Rust drachtio control client is small. Engine wraps Sofia-SIP (reused). |
| **rtpengine** (`_research/rtpengine`) | The `ng` media-control protocol (`<cookie> <bencode-or-JSON>`; **bencode on UDP** per the spike, JSON on HTTP; offer/answer/delete/record/play). Has a **Linux kernel module** + 61k-line C daemon → definitive reason to reuse, not rebuild. Spike confirms a Rust ng client is ~200 lines (bencode/UDP). |
| **jambonz** (prior research + its EKS Helm chart) | The composition pattern (Node control plane over drachtio + rtpengine [+ FreeSWITCH]) and the **EKS media-networking packaging** (System/SIP/RTP node pools, `hostNetwork`, Elastic IPs, RTP port range) we copy from day zero. |
| **FusionPBX** (`_research/fusionpbx/app/dialplans`, docs + code) | The **routing/dialplan expressiveness** we re-express as `ad_routing` (§0.10.5): the `dialplan`/`dialplan_detail` model — ordered, grouped `condition`/`action`/`anti-action` rows with `break`/`inline`/`group`/`order`, regex capture, variable interpolation + a function library, global vs domain scoping, named destinations, time conditions. |

### 2.1 Key VICIdial facts the design must honor

- `dial_method ENUM('MANUAL','RATIO','ADAPT_HARD_LIMIT','ADAPT_TAPERED','ADAPT_AVERAGE','ADAPT_PERCENTMAX','INBOUND_MAN','SHARED_*')`.
- The hopper (`AST_VDhopper.pl`, ~4k lines) selects leads into `vicidial_hopper`
  from sources: Auto-alt-dial, Scheduled Callbacks, Xth-New ordering, API load,
  No-hopper queue, Recycled, Standard, Drop-Run.
- Compliance lives in the data model, not just code:
  - `vicidial_list`: `gmt_offset_now`, `called_since_last_reset ENUM('Y','N','Y1'..'Y10','D')`,
    `called_count`, `last_local_call_time`, `rank`, `status`, `state`, `postal_code`.
  - `vicidial_call_times`: per-day-of-week start/stop windows + `ct_state_call_times` + `ct_holidays`.
  - `vicidial_state_call_times`: per-US-state windows. This is the TCPA
    timezone-compliance core.
- Abandonment: `drop_call_seconds`, `drop_action`, `queue_log ABANDON` logging.
- The **remote-agent mechanism** (`AST_VDremote_agents.pl`) is the exact seam
  where "a human logs in" is replaced by "an external endpoint receives the
  call." That seam is where LiveKit attaches.

### 2.2 Key dograh patterns we adopt wholesale (re-implemented in Rust)

- **Campaign lifecycle**: `draft → syncing → running ⇄ paused → completed/failed`.
- **Source sync → queued_runs → batch dispatch → runs → reports**, coordinated
  by a **singleton orchestrator** over **Redis pub/sub** events
  (`SyncCompleted`, `BatchCompleted`, `BatchFailed`, `CallCompleted`,
  `RetryNeeded`, `CircuitBreakerTripped`).
- **Redis-backed concurrency slots** with an org-level cap *and* a secondary
  `campaign:<id>` scope cap; acquire → bind-to-run → release, with stale-slot
  timeout recovery and a fleet-wide count for autoscaling. **This replaces
  VICIdial pacing.**
- **Circuit breaker** = AI-native abandonment/health control: pause when the
  failure rate in a rolling window exceeds a threshold.
- **Telephony provider registry** (`providers/<name>/` self-registering spec;
  "adding a provider touches this folder + exactly two lines outside"; no DB
  migration to add one; `connectivity: "sip"` for bring-your-own-SIP).
- **Idempotent, crash-recoverable dispatch** via DB claims + claim-timeout
  recovery, so a killed worker's in-flight work is reclaimed.

---

## 3. Scope: what we keep, drop, and adapt

### 3.1 KEEP (full parity — the campaign brain)

- Campaigns (CRUD, lifecycle, pause/resume, start/stop).
- Lead lists; multi-list campaigns; list-mix with ratios; list expiration.
- The **hopper** (lead selection) with all VICIdial sources adapted (below).
- Lead ordering (Xth-new, last-name/phone sort, random, timezone ordering, rank).
- Lead recycling by status; call-count limits; 24-hour call limits.
- Lead filters (SQL-expression filters per campaign).
- Scheduled callbacks (agent-specific and anyone), with activation into hopper.
- Dispositions / statuses (system + per-campaign custom statuses, categories).
- **DNC**: campaign-level, system-level, area-code, international DNC lists.
- **TCPA timezone / safe-hours compliance**: per-lead local time, call-time
  windows per day-of-week, per-state windows, holidays. This is non-negotiable
  parity and the highest-risk subsystem.
- **Abandonment accounting & caps** (reframed — see 3.3) and reporting of it.
- Call recordings (per-run), with retention/purge and GDPR scrub/export.
- Reporting: real-time campaign stats, per-list/per-status, outcome funnels,
  export.
- Multi-tenancy (org scoping — dograh already models this; VICIdial uses
  user_groups/territories).
- API for lead load, status change, campaign control (VICIdial's non-agent API).
- A/B traffic split across agent versions (from dograh/QuickVoice).

### 3.2 DROP (PSTN / phone-line / human-agent)

- **All Asterisk/PSTN origination and bridging.** No `AST_VDauto_dial.pl`
  channel origination, no `dialplan`, no carrier/trunk config for the telephone
  network, no DAHDI/SIP-registration-to-carrier.
- **The human agent screen** and everything that serves it: the browser agent
  interface, agent login/logout, pause codes, hotkeys, agent screen webphone,
  `vicidial_live_agents` as "humans logged in."
- **CPD (call progress detection) on our side** / AMD done in Asterisk — this
  moves into the agent layer (pipecat/LiveKit voicemail detection).
- **Predictive/adaptive pacing algorithms** as the *primary* dialing strategy
  (`AST_VDadapt.pl`, 7.4k lines). We implement progressive/power as the default
  and keep a thin "predictive" strategy only as an optional, capacity-aware
  extension (see 3.3). We do **not** port the Perl adaptive math verbatim.
- **STIR/SHAKEN signing, PCI-DSS payment IVR, carrier-side concerns** — these
  are downstream of our SIP egress (carrier/PBX responsibility). We expose the
  hooks (pass headers, mark recordings, pause/resume recording) but do not
  implement the carrier crypto or a payment IVR ourselves. Explicitly flagged as
  out-of-scope-but-interfaced.

### 3.3 ADAPT (same intent, AI-native mechanism)

| VICIdial concept | AgentDialer mechanism |
|---|---|
| Human agents log into agent screen | **LiveKit agent dispatched per answered call**; agent capacity is elastic (K8s pods). The "remote agent" seam becomes "SIP egress to a LiveKit/pipecat worker." |
| `vicidial_live_agents` (who's available) | **Redis concurrency slots** = guaranteed-available agent capacity. A call is only placed when a slot is acquired. |
| Predictive/adaptive pacing to hit <3% abandonment | **Progressive/power dialing**: never dial beyond acquired capacity ⇒ abandonment is mechanically ~0%. Pacing degenerates to "dial as fast as slots free up." |
| `drop_call_seconds` / `drop_action` / abandonment rate | **Circuit breaker + abandonment ledger**: we still *record* answered-but-not-connected events and compute the regulator's rate (answered-by-human-but-dropped ÷ answered-by-human) over the correct rolling window per jurisdiction. Used for reporting and as a hard safety cap, not as the pacing driver. |
| Asterisk originate → bridge to agent | **Telephony provider `initiate_call`** over SIP → room/leg → agent attaches. One provider: SIP (with LiveKit as the first concrete egress). |
| CPD/AMD in Asterisk | **Answer supervisor in the agent** (voicemail/human/screening) — dograh's `AnswerAction` model: `start_opening/release/leave_message/drop/screen_then_rearm/wait_for_screening`. |
| GMT offset computed by `AST_DB_tz_divide.pl` | **Compliance crate** computes per-lead local time from phone/postal/state + tz database; same windows model, done in Rust. |

**Abandonment-rate math we must implement correctly** (regulator definition, not
failed÷dialed): `abandon_rate = answered_by_human_but_no_agent / answered_by_human`.
Windows: US FCC/FTC TSR 3% over rolling 30 days per campaign; UK Ofcom <3% over
24h per campaign; India TRAI network-drop penalties >2% plus the Feb-2025
amendments. The compliance crate owns per-jurisdiction window + denominator
rules behind a registry (no hardcoded single rule). **Flag:** TRAI Feb-2025
specifics (DLT registration, consent scrubbing, header/CLI registration) must be
verified against primary TRAI sources before any India-compliant claim; treat as
a research task before that jurisdiction's rule ships.

---

## 4. High-level architecture

```
                           ┌──────────────────────────────────────────────┐
                           │                 Kubernetes                     │
                           │                                                │
   Operator UI  ─────────► │  ┌───────────┐   ┌──────────────────────────┐ │
   (Next.js, later)        │  │  api       │   │ orchestrator (SINGLETON) │ │
                           │  │  (Rust,    │◄─►│  - schedule windows      │ │
   REST / gRPC  ─────────► │  │  axum)     │   │  - batch scheduling      │ │
                           │  └─────┬──────┘   │  - circuit breaker check │ │
                           │        │          │  - stale/claim recovery  │ │
                           │        │          └───────────┬──────────────┘ │
                           │        │   Redis pub/sub events │               │
                           │        ▼                        ▼               │
                           │  ┌───────────────┐     ┌──────────────────┐    │
                           │  │ Postgres      │     │ dispatch-workers │    │
                           │  │ (+ pgvector)  │◄───►│  (Rust, N replicas│    │
                           │  │  campaigns,   │     │   HPA/KEDA)       │    │
                           │  │  leads, hopper│     │  - claim queued   │    │
                           │  │  runs, dnc,   │     │    runs           │    │
                           │  │  calltimes,   │     │  - acquire slot   │    │
                           │  │  dispositions │     │  - compliance gate│    │
                           │  └───────────────┘     │  - SIP initiate   │    │
                           │        ▲                └─────────┬────────┘    │
                           │        │ Redis slots              │ SIP egress  │
                           │        │ (org + campaign scope)   ▼             │
                           │  ┌───────────────┐     ┌──────────────────┐    │
                           │  │ Redis         │     │ sip-egress /     │    │
                           │  │ slots, events,│     │ provider:livekit │────┼──► LiveKit SIP
                           │  │ LISTEN/NOTIFY │     │ (CreateSIP-      │    │    (room + agent)
                           │  └───────────────┘     │  Participant)    │    │        │
                           │                        └──────────────────┘    │        ▼
                           │  ┌───────────────┐  callback/webhook           │   pipecat/LiveKit
                           │  │ object store  │◄── run outcome, disposition  │   agent workers
                           │  │ (S3/MinIO)    │     recording ptr            │   (RAG, tools,
                           │  │  recordings   │                             │    voicemail detect)
                           │  └───────────────┘                             │
                           └──────────────────────────────────────────────┘
```

Call origination options (both supported behind the provider abstraction):

- **AgentDialer-originated (default):** dispatch-worker calls the SIP provider's
  `initiate_call` → LiveKit `CreateSIPParticipant` creates the outbound leg into
  a room where an agent is (or will be) dispatched. AgentDialer owns pacing,
  compliance, retries, DNC. This is the recommended path and the one the plan
  builds first.
- **Agent-callback (inbound-style) variant:** AgentDialer tells LiveKit to dial;
  LiveKit's own agent dispatch runs the conversation; AgentDialer receives
  outcome via webhook. Same provider interface, different `strategy`.

---

## 5. Rust workspace layout (Cargo workspace of focused crates)

Modeled on Bun's "workspace of many small crates with clear boundaries" and
dograh's service decomposition. Each crate has a single concern and a
`AGENTS.md` describing its contract.

```
agentdialer/
├── Cargo.toml                      # workspace root
├── PLAN.md                         # this document
├── AGENTS.md                       # top-level conventions (see §10)
├── REVIEW.md                       # review rules (what blocks merges)
├── .claude/                        # commands + skills (review-pr, add-provider, ...)
├── crates/
│   ├── ad_core/                    # foundation: ids, errors, time, config, telemetry
│   ├── ad_domain/                  # pure domain types: Campaign, Lead, Hopper entry,
│   │                               #   Disposition, Status, CallTime, DncEntry, Run.
│   │                               #   No I/O. The "schema in Rust types."
│   ├── ad_db/                      # sqlx (Postgres) repositories; migrations;
│   │                               #   claim/recovery queries. Async.
│   ├── ad_compliance/              # TCPA/timezone/safe-hours engine + DNC scrubbing
│   │                               #   + abandonment ledger. Registry of jurisdiction
│   │                               #   rules. PURE + deterministic + heavily tested.
│   ├── ad_hopper/                  # lead selection: sources (standard/recycle/callback/
│   │                               #   alt-dial/api/xth-new/drop-run), ordering, filters.
│   │                               #   Registry of HopperSource. Pure selection logic
│   │                               #   over ad_db queries.
│   ├── ad_pacing/                  # dial strategies: Progressive (default), Power,
│   │                               #   (optional) Predictive. Registry of DialStrategy.
│   ├── ad_concurrency/             # Redis slot manager (org + campaign scope),
│   │                               #   acquire/bind/release, stale recovery, fleet count.
│   ├── ad_events/                  # Redis pub/sub campaign event protocol (typed).
│   ├── ad_telephony/              # provider abstraction: trait TelephonyProvider,
│   │   ├── registry.rs             #   registry + factory, ProviderSpec.
│   │   └── providers/
│   │       ├── livekit/            #   SIP egress via LiveKit CreateSIPParticipant.
│   │       └── sip_generic/        #   generic static-trunk SIP egress (BYO PBX/carrier).
│   ├── ad_orchestrator/            # SINGLETON: schedule windows, batch scheduling,
│   │                               #   circuit breaker, completion + stale recovery.
│   ├── ad_dispatch/                # dispatch-worker: claim runs, compliance gate,
│   │                               #   acquire slot, initiate SIP call, record outcome.
│   ├── ad_circuit_breaker/         # rolling-window failure-rate breaker.
│   ├── ad_api/                     # axum HTTP (+ optional gRPC) surface; auth; DTOs.
│   ├── ad_reports/                 # campaign stats, funnels, exports.
│   └── ad_bin/                     # the binaries: `agentdialer api|orchestrator|
│                                   #   dispatch|migrate` subcommands (one image).
├── migrations/                     # SQL migrations (sqlx)
└── deploy/
    └── helm/agentdialer/           # Helm chart (see §8)
```

Binary strategy (Bun-style single image, subcommand dispatch): one `ad_bin`
image, run as different K8s Deployments via subcommand — `api`, `orchestrator`
(singleton), `dispatch` (N replicas), `migrate` (Job). Mirrors how dograh ships
web/orchestrator/arq-worker/ari-manager from one codebase.

### Why Rust here

- Dispatch workers hold many concurrent in-flight SIP originations and long
  outcome waits; `tokio` async + cheap tasks fit well.
- The compliance engine is pure, deterministic arithmetic over time zones — Rust
  gives us correctness, exhaustive `match`, and property-test-friendly code for
  the highest-risk subsystem.
- Single static binary ⇒ tiny images, fast cold start for HPA/KEDA scale-up.

---

## 6. Data model (Postgres)

Re-expressed from VICIdial's compliance-bearing tables and dograh's campaign
tables. Discriminators (provider name, source type, strategy, jurisdiction) live
in `VARCHAR`/`JSONB`, **never** as schema the registry has to migrate — adding a
provider/strategy/source needs no migration.

Core tables (abbreviated — full DDL is a Phase-1 deliverable):

- `organizations` — tenant root.
- `campaigns` — id, org_id, name, state(enum), workflow/agent ref,
  `dial_strategy` (varchar discriminator), `concurrency_cap`,
  `schedule_config` JSONB (timezone + per-day slots), `retry_config` JSONB,
  `circuit_breaker_config` JSONB, `call_time_id`, `drop_action`,
  `abandon_jurisdiction`, timestamps, counters (total/processed/failed rows).
- `lists` — id, campaign linkage via `campaign_lists` (list-mix ratios,
  active, expiration).
- `leads` — the `vicidial_list` analog. Carries the compliance columns:
  `phone_number`, `phone_code`, `state`, `postal_code`, `country_code`,
  `gmt_offset_now`, `called_since_last_reset`, `called_count`,
  `last_local_call_time`, `rank`, `status`, `owner`, arbitrary `context` JSONB
  (CSV extra columns → template vars).
- `hopper` — selected leads ready to dial: lead_id, campaign_id, status(enum
  READY/QUEUE/INCALL/DONE/HOLD/DNC/...), `gmt_offset_now`, `priority`,
  `source` (discriminator), claim fields (`claimed_by`, `claimed_at`).
- `queued_runs` — dograh-style: unique `source_uuid`, state
  (queued/claimed/processed/failed), retry bookkeeping, due-at.
- `runs` — one per call attempt: lead_id, campaign_id, provider, call status
  (initiated/ringing/answered/completed/failed/busy/no-answer/...), answer
  classification (human/voicemail/...), disposition code, transcript ptr,
  recording ptr, started/ended, variant (A/B).
- `statuses` / `campaign_statuses` / `status_categories` — dispositions.
- `callbacks` — scheduled callbacks (agent-specific / anyone), activation time.
- `dnc` / `campaign_dnc` — DNC lists (system/campaign/areacode/international),
  indexed on `phone_number`.
- `call_times` / `state_call_times` — safe-hours windows (per-day + per-state +
  holidays JSONB).
- `lead_filters` — per-campaign SQL-expression filters (validated/sandboxed).
- `abandon_ledger` — answered-by-human and dropped events for regulator rate.
- `recordings` — pointer + retention + GDPR scrub state.
- `api_log` / `admin_log` — audit.

Partitioning/retention: `runs`, `abandon_ledger`, logs partitioned by time;
recordings offloaded to object store with lifecycle policy. (VICIdial's
`twoday_*` archive tables are the precedent.)

---

## 7. Core control flow

### 7.1 Campaign start → sync → dispatch loop

1. **Create** campaign (`draft`), attach list(s), agent/workflow, schedule,
   concurrency cap, retry, circuit-breaker, call-time window, jurisdiction.
2. **Start** → `syncing`: source-sync job normalizes list rows into `leads` +
   seeds `queued_runs` (unique `source_uuid` ⇒ idempotent, no dup calls).
   Computes `gmt_offset_now` per lead via `ad_compliance` (phone/postal/state →
   tz). Publishes `SyncCompleted`.
3. **Orchestrator** (singleton) on `SyncCompleted`/`BatchCompleted`/
   `CallCompleted`/`RetryNeeded`:
   - checks campaign is `running`;
   - checks **schedule window** (fail-closed for compliance — *note the
     deliberate difference from dograh, which fails open*; see §9);
   - checks **circuit breaker**;
   - checks there is dispatchable work (`hopper`/`queued_runs`/due retries);
   - enqueues a batch and sets an in-progress flag (idempotent via in-memory
     flag + DB claims).
4. **Hopper fill** (`ad_hopper`): select eligible leads into `hopper` honoring
   ordering, filters, recycle rules, call-count/24h limits, callbacks priority,
   and — critically — **only leads whose local time is within the safe-hours
   window right now** (timezone gate at selection *and* re-checked at dispatch).
5. **Dispatch worker** (N replicas): claim a batch of hopper/queued rows
   (DB claim with timeout), then per lead:
   - **Compliance gate** (`ad_compliance`): safe-hours NOW, DNC scrub
     (campaign/system/areacode/international), consent, call-count/24h caps. Fail
     ⇒ mark lead/hopper accordingly, no call.
   - **Acquire concurrency slot** (`ad_concurrency`): org cap + `campaign:<id>`
     scope cap. No slot ⇒ leave work queued (this *is* the progressive pacing).
   - **Initiate SIP call** (`ad_telephony` provider): LiveKit
     `CreateSIPParticipant` (or static-trunk SIP). Pass correlation
     (campaign/lead/run ids) via SIP headers or room metadata.
   - **Bind slot to run**; create `run`.
6. **Agent layer** (LiveKit + pipecat) answers/classifies/converses. On end,
   posts outcome (disposition, transcript, recording ptr, answer class) back via
   webhook/callback → `runs` updated, slot released, `CallCompleted` published,
   abandonment ledger updated.
7. **Orchestrator** schedules the next batch on `CallCompleted`/`BatchCompleted`;
   `_monitor_completion` loop (every 60s) recovers stale claims/dispatches,
   picks up orphaned retries, and completes idle campaigns.

### 7.2 Retries, callbacks, recycling

- `retry_config` (max_attempts, interval, per-status rules) drives `RetryNeeded`
  → re-queue with due-at. Mirrors dograh retry + VICIdial recycle-by-status.
- Scheduled callbacks activate into the hopper at their time with elevated
  priority (VICIdial callback priority).
- Recycle rules re-admit leads of configured statuses after cooldown
  (Alexor-Brain cooldown subquery pattern + VICIdial `lead_recycle`).

### 7.3 Pause / stop (orphan safety)

On pause/stop/circuit-trip: stop scheduling; in-flight calls complete normally;
a cleanup pass terminates orphaned legs for non-running campaigns
(Alexor-Brain `_cleanup_paused_campaigns` pattern) via the provider's hangup.

---

## 8. Kubernetes-native deployment

Helm chart `deploy/helm/agentdialer` (structure mirrors dograh's proven chart):

- **Deployments**
  - `api` — axum, HPA (CPU + request-rate), PDB.
  - `orchestrator` — **singleton** (replicas: 1, `Recreate` strategy, leader
    lease). Not horizontally scaled by design.
  - `dispatch` — N replicas, **HPA + KEDA** scaled on the fleet concurrency
    metric / queued-runs backlog (Redis-based KEDA scaler). This is how we scale
    "agent capacity-bound" dialing.
  - `ui` (later phase) — HPA + PDB.
- **Jobs**: `migrate` (pre-install/upgrade hook).
- **Stateful deps**: Postgres (prefer managed/RDS; internal chart for dev),
  Redis (slots+events+LISTEN/NOTIFY), object store (S3/MinIO).
- **Gateway/Ingress/HTTPRoutes**: Gateway API, TLS.
- **Config/Secrets**: ConfigMap + Secret; `values.yaml` with
  `values.schema.json`; example values for single-node, k3s-prod, AWS/EKS.
- **No coturn / no SIP-RTP in this chart.** We do not run media or SIP signaling
  pods — LiveKit (its own Helm release) owns SIP/RTP. AgentDialer only speaks
  **SIP control / origination API** to LiveKit. This is the deliberate
  consequence of "SIP-only, no PSTN, agents live in LiveKit." (If a BYO static
  SIP trunk egress needs an outbound SIP signaling sidecar, it is an optional
  subchart, host-networked, added only when that provider is enabled.)

Autoscaling signal: `GET /health/autoscale-metric` on `api` exposes fleet active
calls + queued backlog (dograh precedent) for KEDA.

Singletons rationale (from dograh scaling doc): orchestrator coordinates global
scheduling state; duplicating it causes double-dispatch. Dispatch workers are
stateless and idempotent (DB claims), so they scale freely.

---

## 9. Compliance subsystem (highest risk — called out separately)

This is the subsystem most likely to carry legal/financial penalties if wrong,
so it is the most heavily specified and tested, and it deviates from dograh where
dograh's "fail open" default is unsafe for regulated dialing.

- `ad_compliance` is **pure and deterministic**: given a lead + clock + rule set,
  it returns a `ComplianceDecision { allow: bool, reason }`. No I/O inside the
  decision core (DNC data is loaded by `ad_db` and passed in).
- **Safe-hours / timezone**: compute per-lead local time from (in priority order)
  explicit tz, phone area code, postal code, state; apply `call_times` (per-day
  window), `state_call_times` (per-state), and holidays. Checked at hopper
  selection **and** re-checked at dispatch (clock moves). **Fails CLOSED** — if
  we cannot determine the lead's local time or window, we do **not** call. (This
  is the explicit, deliberate inversion of dograh's `_is_within_schedule`
  fail-open behavior; documented so a future reader doesn't "fix" it back.)
- **DNC scrubbing**: system + campaign + area-code + international lists, indexed
  lookups, checked at selection and dispatch.
- **Abandonment ledger & rate**: records answered-by-human and
  answered-but-no-agent events; computes the regulator's rate with the correct
  denominator and per-jurisdiction rolling window; feeds the circuit breaker's
  hard cap and the compliance report. Jurisdiction rules live behind a
  **registry** (`UsTsrRule`, `UkOfcomRule`, `IndiaTraiRule`, ...), each its own
  module — adding a jurisdiction is a new module + one registration line, no
  schema change.
- **GDPR**: data export + scrub (delete lead PII + recordings) as first-class
  operations with an audit trail.
- **Interfaced-but-not-implemented** (carrier-side, flagged in `AGENTS.md`):
  STIR/SHAKEN attestation (pass caller-ID + headers to the SIP trunk; signing is
  the carrier's), PCI payment capture (expose recording pause/resume + a
  "bridge-out" hook; no payment IVR here).
- **Verification debt**: TRAI Feb-2025 obligations must be confirmed against
  primary sources before `IndiaTraiRule` is claimed compliant. Until then it is
  marked `experimental` and the UI/API say so (honest-status convention).

Testing: property tests over DST boundaries, half-hour-offset zones (e.g. India
+5:30, which the +5:30 of the current clock makes a natural fixture), midnight
window wrap, holiday overrides, and the abandonment-rate denominator. These tests
must fail without the rule and pass with it (Bun convention).

---

## 10. Conventions (adopted from Bun + dograh)

Captured in top-level `AGENTS.md` + per-crate `AGENTS.md` + `REVIEW.md`:

- **Cargo workspace of focused crates**; a crate does one thing; cross-crate
  behavior lives in a shared crate, never reached into a sibling's internals
  (dograh "don't import another provider's internals").
- **Registry-driven extensibility, no hardcoded lists**: telephony providers,
  dial strategies, hopper sources, compliance jurisdictions each self-register.
  Adding one touches its own folder + a minimal, enumerated set of edits
  outside — documented per registry (dograh "exactly two lines outside"). **No
  DB migration to add a variant** — discriminator in `VARCHAR`/`JSONB`.
- **RAII/Drop over manual cleanup** (Bun). Slots, claims, and SIP legs are
  released in `Drop`/guard types so a panic or early return cannot orphan them;
  the Redis stale-timeout and DB claim-recovery are the backstop, not the
  primary path.
- **Honest status**: never overstate what works. `experimental`/`beta` flags are
  real and surfaced. (Directly from Bun's "be humble & honest.")
- **Test-next-to-code with real verification**: tests live beside the module;
  a test must fail without the change and pass with it; no flaky
  sleep-based waits (poll with a deadline); regression tests only for real
  regressions. (Bun.)
- **Comments must earn their place**; if a workaround needs a paragraph to
  justify, fix the code. (Bun.)
- **`.claude/` commands + skills** (e.g. `add-telephony-provider`,
  `add-jurisdiction-rule`, `review-pr`) and `.github/workflows` for CI
  (fmt, clippy -D warnings, test, migration check, helm lint).

---

## 11. Phased roadmap

Each phase ends with something runnable and tested. "Done" = builds + tests
pass + verified against the stated acceptance check. The media plane (§0) is the
new foundation; the campaign/compliance brain is layered on top and can be built
in parallel against a stub router until the real one lands.

**Phase 0 — Skeleton & conventions**
- Cargo workspace, `ad_core`, `ad_bin` subcommand dispatch, CI (fmt/clippy/test),
  top-level `AGENTS.md`/`REVIEW.md`, `.claude/` scaffolding.
- Accept: `agentdialer --help` lists subcommands; CI green on an empty-but-wired
  workspace.

**Phase 1 — Domain + DB + migrations**
- `ad_domain` types; `ad_db` repositories (`sqlx`); full migration set for the §6
  schema; `agentdialer migrate` applies them.
- Accept: migrations up/down clean on Postgres; repository round-trip tests pass.

**Phase 2 — Compliance engine (do this early — highest risk)**
- `ad_compliance`: timezone/safe-hours (fail-closed, `chrono-tz`), DNC scrubbing,
  abandonment rate + jurisdiction registry (US TSR first; UK + India
  `experimental`).
- Accept: property tests over DST/half-offset/holiday/window-wrap pass; rate
  denominator verified against worked examples.

**Phase 3 — Media control clients (NEW — the two small protocol clients)**
- `ad_sip_ctl` (drachtio TCP control client: codec, `authenticate`, SIP send/recv,
  stackTxnId/stackDialogId); `ad_rtpengine_ctl` (ng JSON/UDP client: cookie
  match, offer/answer/delete/ping); `ad_sipmsg` (SIP+SDP model — try `rsip` first).
- Accept: against a locally-run drachtio + rtpengine (docker), authenticate,
  round-trip a `ping`/`offer`/`answer`/`delete`, and send/receive a SIP message.
  Protocol conformance tests mirror the Node/C references in `_research/`.

**Phase 4 — Call router / B2BUA (NEW — the core origination primitive)**
- `ad_router`: originate a customer leg via drachtio, run rtpengine `offer`,
  bridge to an agent SIP leg (LiveKit SIP / pipecat SIP), rtpengine `answer`,
  manage dialog + teardown + hangup + transfer. Exposes `initiate_call()` behind
  the `ad_telephony` provider `providers/drachtio_rtpengine`.
- Accept: end-to-end on a dev box — originate a call to a test SIP UAS, bridge to
  a stub agent endpoint, confirm two-way RTP via rtpengine, tear down cleanly.
  `livekit`/`pipecat` agent-leg targets place a real agent in the loop.

**Phase 5 — Concurrency + dispatch + orchestrator (the dialer core)**
- `ad_concurrency` via **Postgres `FOR UPDATE SKIP LOCKED`** + a per-campaign
  counter (no Redis slot manager); `ad_hopper` (sources + ordering + filters +
  recycle + callbacks); `ad_pacing` (progressive default); `ad_dispatch` (claim →
  compliance gate → acquire capacity → `router.initiate_call` → run);
  `ad_orchestrator` (singleton via **`pg_try_advisory_lock`**, schedule-closed
  windows, circuit breaker, completion/stale recovery); job/retry/callback queue
  via **`apalis`**/`pgmq`.
- Accept: end-to-end test campaign — leads load, hopper fills respecting
  timezone/DNC, calls initiate only within window + under cap, abandonment ~0%,
  pause drains in-flight and stops scheduling, retries re-queue, campaign
  completes. Crash-a-dispatch-worker test shows `SKIP LOCKED` claim recovery, no
  dup calls. Two orchestrator replicas → only the advisory-lock holder is active.

**Phase 6 — API + reports**
- `ad_api` (campaign CRUD/control, lead load, status change, DNC, callbacks,
  outcome webhook ingest, autoscale metric); `ad_reports` (real-time stats,
  funnels, exports); GDPR export/scrub endpoints.
- Accept: API contract tests; a campaign driven entirely through the API;
  reports match ledger.

**Phase 7 — Kubernetes / Helm (EKS-native media plane from day zero)**
- `deploy/helm/agentdialer`:
  - **SIP node pool**: drachtio pods (`hostNetwork`, stable addressing).
  - **RTP node pool**: rtpengine pods (`hostNetwork` + Elastic IPs + UDP RTP port
    range + kernel-module-capable nodes), graceful call-draining on scale-down.
  - **Control-plane pool**: api/router/orchestrator(singleton)/dispatch(HPA+KEDA),
    migrate(Job); Postgres (managed/RDS preferred) + object store; Gateway/Ingress.
  - KEDA scaler on queued-run backlog / fleet active-calls.
- Accept: `helm lint` + `helm template` clean; deploy to a kind/k3s cluster (or
  EKS) runs a campaign through the real drachtio+rtpengine data plane to a stub
  SIP agent; SIP/RTP pods drain in-flight calls on termination; dispatch scales
  on backlog.

**Phase 8 — Operator UI (optional / later)**
- Next.js console (campaigns, lists, dispositions, DNC, real-time monitor,
  reports). Reuse dograh/QuickVoice UI patterns.

**Phase 9 — Hardening & parity closeout**
- A/B traffic split, list-mix ratios, international DNC, advanced recycle,
  recordings retention/GDPR automation (rtpengine `start/stop recording`), audit
  logs, optional capacity-aware "predictive" strategy, additional jurisdiction
  rules (after source verification), optional kernel-module RTP acceleration for
  rtpengine on EKS (userspace is the dev default; see spike),
  optional FreeSWITCH media node pool only if a concrete IVR/conferencing need
  appears.

---

## 12. Explicit non-goals / risks / open questions

**Non-goals**
- No PSTN/telephone-network interface, ever. No Asterisk dialplan, no carrier
  trunk crypto. SIP egress only; the carrier/PBX is downstream.
- No human agent screen / webphone / agent login.
- **No from-scratch SIP stack and no from-scratch RTP/media engine.** We reuse
  drachtio (Sofia-SIP) for signaling and rtpengine (incl. its kernel module) for
  media (§0). We write only the two small Rust control clients + routing logic.
- No in-house STT/TTS/LLM or payment IVR — those are LiveKit/pipecat and carrier
  concerns. We interface, we don't implement.
- No FreeSWITCH by default (jambonz uses it for heavy media/IVR; we don't need it
  unless a concrete IVR/conferencing requirement appears — §0.9).

**Risks**
- **Real-time media on EKS is the hardest, least-trodden part** (§0.6):
  hostNetwork, Elastic IPs, RTP port ranges, kernel-module nodes, graceful
  call-draining. Mitigated by copying jambonz's proven node-pool pattern, but we
  are the integrator of a trio that has no published turnkey EKS deployment.
- **Compliance correctness** is the dominant *product* risk (legal/financial).
  Mitigated by the pure, fail-closed, property-tested `ad_compliance` crate and
  jurisdiction registry — but still needs real legal review per market.
- **Two new protocol clients** (`ad_sip_ctl`, `ad_rtpengine_ctl`): new code, but
  both protocols are tiny and we have working Node/C references in `_research/`
  to conform to; de-risked by conformance tests against a live local engine.
- **LiveKit SIP coupling**: LiveKit SIP does not support SIP REGISTER; the agent
  leg uses a static trunk / `CreateSIPParticipant`. The router isolates this;
  `pipecat` SIP and `sip_generic` are alternative agent-leg targets.
- **Orchestrator singleton** availability: mitigated by `pg_try_advisory_lock`
  leader election + fast restart (single static Rust binary) + a warm standby
  replica that acquires the lock on failover.
- **Reimplementing VICIdial breadth**: 345 tables / 135 scripts is large. We
  deliberately port the *campaign brain + compliance model*, not the Perl line
  for line, and we drop the entire PSTN/human half — which is most of the bulk.

**Open questions (to resolve before/within the relevant phase)**
1. Exact TRAI Feb-2025 obligations (DLT, consent, header/CLI registration) —
   verify against primary sources (Phase 2/9 gate for India).
2. `rsip`/`rsipstack` suitability for SIP-message mutation vs. a minimal in-house
   model — decide in Phase 3 (`ad_sipmsg`) by trying `rsip` first.
3. Reuse jambonz's drachtio/rtpengine charts vs. our own minimal pods — default
   our own, copying only the networking pattern (§0.9).
4. Keep FreeSWITCH out vs. add a media node pool for IVR/conferencing (§0.9).
5. Multi-tenancy depth: full org isolation vs single-tenant first (affects schema
   indexing and API auth in Phase 1/6).
6. `apalis` vs `pgmq` for the durable queue; `apalis` leans "batteries included"
   per the buy-don't-build directive — default `apalis`, confirm in Phase 5.

---

## 13. Immediate next actions (when implementation starts)

1. Scaffold the Cargo workspace + `ad_core` + `ad_bin` + CI (Phase 0).
2. Write the §6 DDL as the first migration and `ad_domain`/`ad_db` with `sqlx`
   (Phase 1).
3. Build and property-test `ad_compliance` before any dialing code (Phase 2) —
   the product's long pole and risk pole. In parallel, stand up local drachtio +
   rtpengine (docker) and spike `ad_sip_ctl`/`ad_rtpengine_ctl` against them
   (Phase 3) to retire the only genuinely-new-code risk early.
4. Then `ad_router` (Phase 4) so the campaign core (Phase 5) has a real
   `initiate_call` to drive.

The research corpus is preserved in `_research/` (git-ignored) for reference
while implementing — including drachtio-srf, drachtio-server, and rtpengine as
the conformance references for the two control clients. `PLAN.md` + the per-crate
`AGENTS.md` are the living contract.
