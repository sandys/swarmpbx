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

### 0.0 Decision D-9 (2026-10-21) — agentdialer carries the call (BUILT + PROVEN in Docker Compose; supersedes §0.1–§0.7 and the drachtio/rtpengine half of §0.9a)

**Decision.** The dialer node carries each call itself: it is the SIP user
agent and the RTP endpoint, inside the agentdialer process. There is no
Asterisk, FreeSWITCH, drachtio, rtpengine, or `livekit/sip` under it.
Kamailio is the SIP edge: a SIP-aware load balancer for signaling only. The
SIP and media stacks are vendored libraries (decision of the user: use and
vendor libraries that already do the job; write only the glue).

**Why.**
- The dialer must own call control: the INVITE, the hangup, the audio, and
  the bridge to the agent. A switch under the dialer makes the product a
  script on top of someone else's call control.
- Production is managed Kubernetes (EKS) with no host networking.
  Asterisk, FreeSWITCH, rtpengine, `livekit/sip`, LiveKit server, and jambonz
  all use one UDP port per call from a large range, so they need
  `hostNetwork` on Kubernetes (research: `_research/scratch/media/`). Our
  design must not.

**Topology.**

```
 carrier ⇄ SIP (UDP/TCP/TLS) ⇄ Kamailio (2+ pods; an Elastic IP each, or an NLB)
                                   ⇅ SIP inside the VPC (Record-Route keeps Kamailio on the path)
                             agentdialer nodes (normal pods; the cluster of §0.10.8b)
 carrier ⇄ RTP (direct) ⇄ the node that runs the call ⇄ WebRTC ⇄ LiveKit room ⇄ agent
```

- **SIP.** The node sends its INVITE through Kamailio (the outbound proxy).
  Kamailio adds itself to `Record-Route`, so every later message of the call,
  in both directions, goes through Kamailio to the node that owns the call.
  Kamailio needs only its SIP ports (5060 UDP/TCP, 5061 TLS). Its dispatcher
  sends OPTIONS to the nodes and removes a node that does not answer. The
  carrier sees one signaling address per Kamailio pod and never sees a node
  scale up or down.
- **RTP.** Kamailio does not touch audio. The RTP goes straight between the
  carrier and the node, one UDP socket per call. A trunk selects one media
  mode:
  - `latching` (default): the node sits behind the NAT gateway like every
    pod. It sends RTP first and never stops; the carrier latches to the
    source it sees. Telnyx latches by default; Twilio latches when the
    trunk's "Symmetric RTP" is on.
  - `public`: the node has its own Elastic IP (no host networking; for
    example the `aws-pod-eip-controller` sample), for strict carriers such as
    Bandwidth, which need one static public address.
- **No webhook for the call.** The callee hangup is a SIP BYE that reaches
  our SIP stack. The agent joining or leaving is a room event on our own
  WebRTC participant. The new path does not use the LiveKit webhook; its
  code in the api is still there and is to be removed.

**Layers: vendored, copied, or ours** (pins and file lists:
`_research/scratch/media/vendor-plan.md`).

| Layer | Source | Form |
|---|---|---|
| SIP messages, transports, transactions, dialogs, digest auth, `rport`, PRACK | `rsipstack` 0.7.7 `5ba8ae89` (MIT) | `vendor/rsipstack`, whole |
| SDP offer/answer, RTP, RTCP, symmetric RTP, payload types | `rustrtc` 0.4.7 `dbf930ab` (MIT) | `vendor/rustrtc`, whole, plus patches: safe latching; an `rtp-only` feature |
| G.711, G.722, resampling, telephone-event | `audio-codec` 0.4.13 `7c531be7` (MIT; we add the LICENSE file) | `vendor/audio-codec`, whole, Opus off |
| 20 ms send clock that never stops, inbound PCM, codec choice, DTMF tap | `rustpbx` `crates/rustpbx-media` `f3330024` (MIT) | copied, trimmed, into `ad_media` |
| Session timers (RFC 4028), trunk OPTIONS health | `rustpbx` `session_timer.rs`, `trunk_health.rs` | copied |
| Opus, room-side jitter buffer, loss concealment | libwebrtc in the `livekit` 0.9.4 SDK | already used |
| DTMF to and from the agent | `livekit` `publish_dtmf`, `RoomEvent::SipDTMFReceived` | already used |
| Outbound call flow: INVITE driver, ringing timeout, failure classes, Q.850 `Reason` | ours, on `rsipstack` (~500 lines) | `ad_sip` |
| Bridge from the call audio to our room participant | ours (~300 lines), model: `rustpbx` `src/call/realtime/bridge.rs` | `ad_gateway` |
| Media timeout, keep-alive on the call's SIP flow | ours (small) | `ad_sip` / `ad_media` |

**The carrier lessons and where they live** (`_research/scratch/media/asterisk-freeswitch.md`):
- ACK again for a resent 200 OK; CANCEL only after a 1xx; ACK then BYE for a
  200 after CANCEL; no loop on a repeated digest challenge: `rsipstack`
  (tested against SIPp in the first slice).
- Latch only after 4 packets in sequence from one source, within 5 s of each
  SDP answer, then lock and drop other sources ("RTP bleed"): our `rustrtc`
  patch.
- Send with the answer's payload types, receive with ours; never hard-code
  101: `rustrtc`, tested.
- One SSRC, linear timestamps, a packet every 20 ms from answer to BYE: the
  copied egress clock.
- Answer every session refresh with a valid SDP: the copied session timer.
- Fail over to the next trunk only on a trunk fault before any 18x (503,
  5xx, no response), never on busy, no answer, or decline: ours.
- No RTP for 30 s after answer ends the call: ours.

**Failure behavior changes.** The node that runs a call now holds its media.
A node that dies drops its live calls, as a dead switch does. The repair pass
of the leader then ends those attempts (no dial again, no second call to the
consumer). Kamailio's dialog module can send BYE to the carrier for the
calls of a node that the dispatcher marks dead (open item). A node that stops
cleanly (SIGTERM) first takes no new calls and lets its calls end
(`terminationGracePeriodSeconds`).

**Proof (Docker Compose, no host networking; `e2e/docker-compose.cluster.yml`).**
One bridge network holds the nodes, Postgres, kine, LiveKit, the test
agents, and Kamailio. The carrier is `e2e/carrier-sim`, a simulator on diago
that behaves like Twilio Elastic SIP Trunking (407 digest, E.164 and
caller-ID checks, symmetric RTP, per-number busy/failure/no-answer). It sits
on the "internet" at 203.0.113.20, held by a NAT container. Traffic from the
cluster to it is source-NATed with random ports (`SNAT --random-fully`), as
by the AWS NAT gateway; one-shot sidecars add the route, as a VPC route table
does. (Docker drops routed traffic to a container IP of another bridge, so
the carrier cannot be a container on a second network.)
- `e2e/run_e2e.sh` (one node, one call per agent runtime) and
  `e2e/run_cluster_e2e.sh` (three nodes, 24 calls) pass. Each call goes node →
  Kamailio → carrier; the carrier's BYE comes back through Kamailio to the
  node that owns the call (the hangup event names the callee); the agent's
  tone goes out as RTP through the NAT, comes back from the carrier, and the
  agent hears it (`loud_in`). A killed node's calls end without a second
  dial; a killed leader is replaced; every lead is dialed exactly once.
- The cluster test runs two calls per node (six at once): above that, the
  Python test agents (one process per runtime) run out of CPU, not the nodes.

**Slices.**
- (1) Vendored crates: DONE (`vendor/VENDORED.md`).
- (2) The call driver `ad_sip`: DONE for 100/180/200 + ACK, BYE both ways,
  ring timeout → CANCEL → 487, busy, no response (Timer B), and a trunk that
  cannot be reached (`NotSent`). Not yet tested: 407 auth, a resent 200.
- (3) The RTP leg `ad_media`: DONE (G.711 both ways, the answer decides the
  codec, a latching echo carrier, silence when quiet, the media timeout). The
  latching patch to `rustrtc` (RTP bleed) is NOT done.
- (4) The bridge and the node's call task: DONE, for both runtimes. The
  participant joins after the answer; the agent leaving ends the call.
- (5) Kamailio and the NAT in the e2e tests: DONE.
- (6) `livekit/sip` and Redis are gone from the e2e tests; the LiveKit
  webhook code is still in the api, unused by the new path.

**LiveKit Cloud SIP (the primary target).** README.md "Quickstart: place
calls through LiveKit Cloud SIP" is the supported quickstart
(`docker-compose.quickstart.yml` with `docker-compose.livekit-cloud.yml`,
`e2e/livekit-cloud/run.sh setup`). The node calls the project's LiveKit
number at its SIP URI, as it calls a carrier; a dispatch rule puts each call
into a room of its own with a callee agent (GAPS.md CARRIER-3). It must never
break: `e2e/run_twilio_e2e.sh --sim` runs the quickstart stack with the
simulator in CI, and the live `e2e/run_lkcloud_e2e.sh` runs the quickstart
against LiveKit Cloud. The live test passed (2026-10-22, again 2026-10-24):
digest auth, two-way audio through Docker's NAT and a home NAT, and a clean
BYE. A reference call through a LiveKit outbound trunk reached the same
number. LiveKit numbers take incoming calls only, so no call went through the
public phone network.

**Twilio (a supported carrier).** README.md "Call through Twilio" uses the
same quickstart stack with `e2e/twilio/run.sh setup`, and it reaches real
phones. `crates/ad_sip/tests/twilio.rs` (407 digest, caller ID, E.164,
`Max-Forwards: 70`, no auth loop) runs in the test tier. The live
`e2e/run_twilio_e2e.sh` calls Twilio's free "Play" number; it needs an
account where Elastic SIP Trunking is available (Twilio refused it for the
Trial test account) and a number for the caller ID.

**Open.** The `rustrtc` latching patch; the first live Twilio run (blocked on
the account); a resent 200 against the simulator; session timers and the
trunk OPTIONS check (copy from `rustpbx`); the
`public` media mode; DTMF; Kamailio's dialog module to BYE the calls of a
dead node; a node CPU profile (a node used about half a core at two calls);
`cargo deny` on the vendored licenses; a staging test on EKS with each target
carrier.


### 0.1 Decision — SUPERSEDED by §0.0 (D-9)

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

### 0.4 What we build vs. reuse — SUPERSEDED by §0.0 (D-9)

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

### 0.5 EKS-native from day zero (copy jambonz's proven pattern) — SUPERSEDED by §0.0 (D-9)

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

### 0.7 Revised crate additions (supersede the media parts of §5) — SUPERSEDED by §0.0 (D-9)

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
claiming (no custom slot manager); **kine** (the etcd API on Postgres) for node
membership, the leader, and call assignment (decision D-8, §0.10.8b);
for the durable job/retry/callback queue; `tokio-cron-scheduler` for sweeps.
agentdialer does not use Redis (decision D-3, §0.10.8a); only LiveKit does. This keeps the
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

> **Update 2026-10-21 (D-9, §0.0):** the WebRTC half of this section stands (our participant in `ad_gateway`). The drachtio + rtpengine half is replaced: the node carries SIP and RTP itself with vendored `rsipstack`, `rustrtc`, and `audio-codec`, behind Kamailio.

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

**Spike verdict (SIP↔WebRTC path proven end to end, incl. two-way audio).**
A docker-compose spike (`spike/livekit/`) ran **LiveKit server + Redis + the
`livekit/sip` gateway**, created an inbound SIP trunk + a direct dispatch rule,
and placed a real call. Results, verified:

- **SIP tools (all containerized, no host install):** `sipsak` and `sipexer`
  (`github.com/miconda/sipexer`) OPTIONS both returned `SIP/2.0 200 OK`; `SIPp`
  placed full calls.
- **Two-way audio PROVEN.** With an agent subscriber in the room
  (`lk room join --auto-subscribe --publish agent.ogg`) and a codec-matched SIPp
  call (PCMA, matching `g711a.pcap`), the gateway call statistics showed
  `result=success`, `track_subscribes=1`, and non-zero audio in **both**
  directions: caller→agent `audio_packets=201`, `audio_rx≈7983`; agent→caller
  `audio_out_frames=300`, `audio_tx≈7944`.

This proves the "websocket/SIP nastiness" works end to end: a SIP call becomes a
live LiveKit room participant with a real bidirectional audio loop. Two findings
that will inform `ad_gateway`: a subscriber must be present or the gateway
abandons the call (`cannot-subscribe`); and codecs must match the media source.

The spike used the **`livekit/sip` gateway as the reference bridge** to prove
the path and surface the real failure modes cheaply. Owning the bridge ourselves
(`ad_gateway`, options 1/2 above) is still the §0.9a goal — now de-risked by a
working reference to conform to.

**`ad_gateway` — built, first real origination path proven.** The `ad_gateway`
crate exists and implements the real `CallInitiator` (`ad_dispatch` seam). Its
production path originates a call by creating a LiveKit room and placing a SIP
participant into it (`CreateSIPParticipant` via `livekit-api`), behind a
`GatewayClient` trait (fake for unit tests; real client `LiveKitGateway`).
This is proven end to end by `e2e/run_e2e.sh`: our orchestrator wins
leadership, claims a lead as a call attempt, and `ad_gateway` originates an outbound SIP call
that LiveKit SIP connects to a local UAS; the call lands in the room and the
agent subscriber receives audio (`frames_in > 0`).

Still open: the deeper bridge (rtpengine↔LiveKit WebRTC or a Rust WebRTC
participant) for cases where we own the media leg, not just LiveKit's SIP
origination. The campaign/compliance brain (§0.10) is unaffected — it sits above
`ad_gateway`'s `CallInitiator` regardless of the WebRTC leg's implementation.

**Option 2 — our own native WebRTC participant (BUILT + PROVEN).** We built the
bridge ourselves (PLAN §0.9a option 2): a native Rust WebRTC participant in
`ad_gateway` that joins a LiveKit room, publishes an outbound audio track, and
subscribes to the agent's track — with NO dependency on `livekit/sip`. The seam
is `RoomParticipant` + `RoomSession` + `MediaBridge` (pumps `AudioFrame` both
ways: caller RTP → room track, agent track → caller RTP), with in-memory fakes
for unit tests and a real libwebrtc-backed `LiveKitParticipant` in the normal
build. Two-way audio is proven end to end by `e2e/run_webrtc_e2e.sh`:
our participant joined room `campaign-1`, published 800 tone frames (the agent
received them: `frames_in` high, `subscribed_tracks=1`), and received 922 frames
of the agent's audio back — all through OUR participant, no `livekit/sip`.

Build notes: the livekit SDK pulls libwebrtc, whose hermetic libc++ requires
**clang 21+** (Debian bookworm ships 14) and the `glib-2.0` dev package.
`Dockerfile.test` and `Dockerfile` install LLVM 21 and set
`CC=clang-21 CXX=clang++-21`. The version skew (`livekit` 0.7 + `livekit-api`
0.8 pulled two `livekit-protocol` copies, breaking `ConnectWhatsAppCallRequest`)
is resolved by pairing `livekit = 0.9` with `livekit-api = 0.8` (one protocol).
There is no feature gate. The bridge is part of the normal build: the Docker
test tier compiles, lints, and tests it on every run, and CI runs
`e2e/run_webrtc_e2e.sh` on every change. A feature that CI never compiled could
break without any check going red; that is why the gate was removed.

Still open (product wiring): the orchestrator still originates calls through
`Gateway<LiveKitGateway>` (`CreateSIPParticipant`, which depends on
`livekit/sip`). A `CallInitiator` that carries the call's audio through
`LiveKitParticipant` + `MediaBridge` lands together with the rtpengine RTP
wiring below. Until then the bridge is proven by the e2e, not used by the dialer.

Still open (option 1): wiring real rtpengine RTP into the participant (the probe
uses a generated tone as the RTP source). The media networking (drachtio +
rtpengine) is the §0.3.1 spike; connecting its RTP to `RtpSource`/`RtpSink` is
the remaining media-plane step.

**MVP wiring verdict (real agent subscriber, automated test).** The minimum
viable product — *a real LiveKit agent wired into a room as a subscriber, then a
SIP call driven into it* — is proven as a legitimate automated test
(`spike/livekit/run_mvp_test.sh`, agent in `spike/livekit/agent/`). The agent is
a genuine LiveKit RTC participant (subscribe + publish via the `livekit` SDK);
it omits STT/LLM/TTS, which need API keys and are non-deterministic and so do
not belong in a wiring test. Order enforced by the harness: the agent is wired
and confirmed in the room **first**; the SIP call is the stimulus applied
**after**, and the harness refuses to place the call if wiring fails. Assertions
(polled with deadlines, non-zero exit on failure):

- agent `connected: true` and present in the server-side room participant list
  before any SIP call;
- after the call: `subscribed_tracks ≥ 1`, `frames_in > 0` (caller→agent audio
  received), `frames_out > 0` (agent→caller audio published).

Positive run: PASS (`subscribed_tracks=1, frames_in=102, frames_out=3446`).
Negative run (mutation: agent joins the wrong room): FAIL with exit 1
(`subscribed_tracks=0, frames_in=0`) — the test has teeth. The next layer is a
full `livekit-agents` brain (STT/LLM/TTS, needs keys); the wiring beneath it is
done and automated.
The bridge options we evaluate for the production `ad_gateway`, cheapest first:

1. **rtpengine ↔ LiveKit WebRTC directly.** rtpengine speaks ICE/DTLS-SRTP and
   can bridge RTP to a WebRTC endpoint; drive LiveKit's room join via its API and
   hand rtpengine the room's WebRTC SDP (ICE/DTLS). Likely the least new code.
2. **A Rust WebRTC participant** (`webrtc` crate / `livekit` Rust SDK) that
   pulls RTP from rtpengine and publishes it as a room track. More code, more
   control.
3. **Fallback (not preferred):** run `livekit/sip` after all. Kept only as the
   escape hatch if (1)/(2) prove too costly — explicitly the thing we're trying
   to avoid, but it de-risks the schedule.

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

**Claiming (BUILT, §0.10.7a):** there is no hopper table. The claim reads the
due leads straight from `leads` (the eligibility index, GAPS.md BETTER-3) with
`FOR UPDATE SKIP LOCKED`, within the free capacity of the campaign. Each
claimed lead becomes a call attempt. No custom slot manager, crash-safe, no
double-dial.

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

Concurrency is Postgres-backed (§0.8, BUILT): a live call attempt is the slot.
The claim counts the live attempts under the campaign row lock and takes no
more than `campaigns.concurrency_cap`. The slot frees when the attempt ends.

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

### 0.10.7a Call attempts — the statechart (BUILT + PROVEN)

Decision D-2 (2026-10-20) asks for one state machine per call attempt, saved in
Postgres and restored after a restart. Both regions are built: the customer
leg and the agent (a LiveKit agent or a pipecat bot, `ad_agent`, §0.10.8b).

**Shape.**
- **Statechart** (`ad_attempt`): plain serializable enums. `apply(attempt,
  event)` is pure. It returns the next attempt, or `Ignored` for an event that
  does not apply (a late or repeated webhook). Leg states: `pending` →
  `dialing` → `connected` → `ended`, with `uncertain` for a dial whose result
  was lost. An ended attempt carries an `Outcome`: completed, busy, no_answer,
  declined, invalid_number, failed, setup_error, uncertain, or not_dialed (with
  the gate reason). `classify_sip` maps the SIP final code to the class.
- **Persistence** (`ad_db::attempts`, migration `0006`): `call_attempts` holds
  the snapshot and its leg name. `attempt_events` holds each applied event with
  its sequence number. Both commit in one transaction under the row lock. The
  replay of the saved events gives the saved snapshot; a property test and a
  Postgres test check this.
- **Claim = capacity**: the claim locks the campaign row, counts the live
  attempts, and takes the due leads (`status` new or queued, `next_eligible_at`
  empty or past) up to `concurrency_cap`, with `SKIP LOCKED`. A partial unique
  index allows one live attempt per lead. There is no hopper table.
- **Dial**: the orchestrator re-gates each claimed lead (`ad_hopper::select`),
  saves `dial_started`, and places the call in a task of its own. A `Pacer`
  spaces the starts at `AD_CALLS_PER_SEC`. The gateway waits for the answer and
  returns a `Placement`. A transport error after the request left is "no
  response": the attempt becomes `uncertain` and keeps its capacity.
- **End** (§0.0): the node's call task holds the call until it ends. The
  callee's BYE (through Kamailio) applies `hangup`; the agent leaving the room
  applies `agent_left` (and we send BYE); no RTP for 30 s applies `hangup`.
  Every `AD_ATTEMPT_CHECK_MS` the node reconciles its live attempts that no
  task runs: it asks LiveKit whether the call's participant is in its room
  (a call whose node died is not), and it releases a pending attempt that a
  stopped node left.
- **Outbox**: the transaction that ends an attempt also writes
  `attempt_ended`. The orchestrator then settles the lead and dispatches into
  the freed capacity.
- **Settlement** (the pure `ad_orchestrator::settle`): from the outcome and the
  call count it decides the lead status (queued with a retry, dispositioned,
  done, dnc, or blocked), whether the attempt counts as a call, and the due
  time. Postgres applies it once per attempt (`settled_at`). The sweep settles
  an ended attempt that a lost event left.
- **Campaign state**: `complete_if_idle` and `pause_for_breaker` change the
  state with a predicate and write their event in the same transaction. A
  resume through the API resets the breaker (`breaker_reset_at`).

**Known limits (GAPS.md).** The pacer is one per cluster (the leader). The
agent outcome (D-6) is open. `attempt_events` has no retention.

**Proof.** The `ad_attempt` unit and property tests;
`crates/ad_db/tests/pg_attempts.rs` (capacity, disjoint parallel claims,
eligibility, replay = snapshot, outbox, settle once, completion, pause,
transitions); `crates/ad_api/tests/webhook_api.rs` (signature,
`participant_left`, `room_finished`, repeats); `e2e/run_e2e.sh` (a real call:
claim, dial, answer, callee hangup, webhook, completed, lead dispositioned,
campaign completed); `e2e/run_events_e2e.sh` (a NOTIFY chain within capacity
1, a durable pause, stored retries, a due retry dialed, a resume that resets
the breaker); and the SIPp benchmark (GAPS.md DIAL-3).

### 0.10.8 Callbacks, recycle, retry

- **Scheduled callbacks**: agent-specific or "anyone"; activate into the hopper
  at their due time with elevated priority (VICIdial `vicidial_callbacks`).
- **Recycle**: re-admit leads of configured statuses after a cooldown
  (`recycle_config`; Alexor-Brain cooldown + VICIdial `lead_recycle`).
- **Retry**: `retry_config` (max attempts, interval, per-status rules) → due-at
  re-queue. The due time is a column on the lead (`next_eligible_at`), written with
  the outcome; the claim reads a due lead directly (§0.10.7a). (This supersedes
  the earlier `apalis`/`pgmq` note and the Redis due-timers.)

### 0.10.8a Orchestrator event layer — Postgres LISTEN/NOTIFY (BUILT + PROVEN)

The orchestrator must not blind-poll. It must wake on real events: a batch
finished, a retry became due, a circuit breaker tripped.

**Decision D-3 (2026-10-20): Postgres replaces Redis.** The first build
(2026-10-10) followed dograh and used Redis pub/sub, a Redis due-timer ZSET, and
a Redis breaker window. The user decided to move the event layer entirely to
Postgres. Reasons: one store to run, back up, and reason about; events and the
state change they announce can commit in one place; retries and the breaker
window become rows that an operator can query. Redis is removed from the
product (code, Helm chart, compose files, env contract). The media e2e stacks
keep a Redis container only because LiveKit server and `livekit/sip` need it.

**Shape.**
- **Event table**: `campaign_events (id BIGSERIAL, campaign_id, kind, payload
  JSONB, created_at)`, migration `0005`. `kind` is the serde tag of
  `ad_events::CampaignEvent`.
- **Publish** (`ad_db::PgEventLog`): one transaction takes a transaction-level
  advisory lock, inserts the row, and calls `pg_notify('campaign_events', id)`.
  Postgres sends the notification only after the commit. The lock makes ids
  commit in id order, so a reader that reads "id > cursor" never skips a row
  that commits late.
- **Consume** (`ad_db::PgEventConsumer`): `LISTEN campaign_events`, then read
  rows with an id after the cursor. A task owns the `PgListener` and sends one
  wake-up into a one-slot channel, so bursts coalesce and a timeout never
  cancels a socket read. The consumer also reads on a sweep timer
  (`AD_EVENT_SWEEP_MS`, default 2000 ms). A lost notification (reconnect, no
  listener) delays an event by at most one sweep; it never loses it. Delivery
  starts after the newest event at connect; the reconcile pass finds older
  persisted work. A row that does not decode is reported once and skipped.
- **Retries on the lead row**: the pure `ad_orchestrator::retry_decision`
  (reason + attempts + policy + now) gives a due time or "exhausted". The
  settlement of the ended attempt writes it to `leads.next_eligible_at` /
  `retry_reason` (§0.10.7a). The claim reads due leads directly, and a sweep
  (`AD_RETRY_SWEEP_MS`, default 1000 ms) dispatches every running campaign, so
  a due retry dials without an event. A retry survives a restart. This follows
  dograh's durable `queued_runs.scheduled_for`.
- **Breaker window = the ended call attempts** (`ad_db::PgBreakerStore`): the
  attempt row is the record (`failure`, `ended_at`). Evaluation reads the
  window rows after `campaigns.breaker_reset_at` and replays them into the pure
  `ad_circuit_breaker` engine, which stays the only source of the trip
  decision. A trip pauses the campaign in Postgres.
- **Reactor**: `react` maps `campaign_running` → dispatch, and `attempt_ended`
  → settle the lead, then dispatch into the freed capacity. `call_completed`,
  `retry_scheduled`, `circuit_breaker_tripped`, and `campaign_completed` are
  records: the state they announce is already saved.
- **Consumer = the leader**: only the kine leader (§0.10.8b) listens. This is
  the improvement over dograh, which needs an in-memory lock and `replicas: 1`.
- **Idempotency**: claims use `SKIP LOCKED` and one live attempt per lead; the
  statechart ignores a repeated event; settlement is keyed by the attempt
  (`settled_at`). A replayed event cannot double-dial.

**Known limits (see GAPS.md).** The publish lock serializes all event inserts in
the cluster. That is fine at dialer event rates (hundreds per second), but it
is a ceiling. Postgres documents a global lock at NOTIFY commit under very high
commit rates. The event table grows without bound; a retention job is open.

**Proof.** `crates/ad_db/tests/pg_events.rs` (NOTIFY wake with a one-hour sweep,
sweep recovery of a row with no NOTIFY, start-after-newest, decode skip, kind
column), `pg_breaker.rs`, `pg_attempts.rs`, and `e2e/run_events_e2e.sh`: with
capacity 1 and a 5 s sweep, five failed dials follow each other in about
150 ms through NOTIFY, the breaker pauses the campaign once, each retry is
stored on the lead, a due retry is dialed again, and a resume through the API
resets the breaker.

### 0.10.8b The cluster — nodes, leader, and call assignment over kine (BUILT)

**Decision D-8 (2026-10-21).** Every `orchestrator` replica is a cluster
node, so a Kubernetes scale-up adds nodes. The nodes coordinate through kine:
the etcd v3 API served from Postgres (`k3s-io/kine`). kine is how calls are
assigned to the running nodes. Postgres and kine are managed services (for
example Amazon RDS for Postgres): the product reaches them only by URL and
never assumes a host, a start order, or a health check. Every role waits for
Postgres and kine with backoff (`Db::connect_ready`, `Kine::connect_ready`).

**Keys** (`ad_cluster::keys`):

| Key | Written by | Meaning |
|---|---|---|
| `/agentdialer/nodes/{node}` | each node | The node is alive; its capacity. TTL. |
| `/agentdialer/leader` | the leader | Which node schedules. TTL. |
| `/agentdialer/calls/{node}/{attempt}` | the leader | The node that runs the call, with the generation. No TTL. |

**kine is not etcd.** It serves only the subset the Kubernetes API server
uses (research in `_research/k3s-io_kine`, confirmed against v0.13.17):
- no lease KeepAlive or Revoke: a lease ID is the TTL in seconds, and a key
  expires TTL seconds after its last write. A holder keeps a key by writing it
  again every third of the TTL (`ad_cluster::Hold`);
- no Election, Lock, or plain DeleteRange service; plain Put is a racy
  upsert; `keys_only` reads are refused. `ad_cluster::Kine` uses only the
  supported transactions: create (`mod == 0`), update (`mod == N`, else get),
  and delete (`get, delete`), each compared on the key it writes;
- a delete cannot report whether the key existed; read the key to know.

**Roles.**
- *Every node* holds its node key, mirrors its own calls prefix (list +
  watch, `ad_cluster::Mirror`), and runs each call there as one task: start
  the agent, wait until it is in the room, dial, save each result. It also
  reconciles its own live calls against LiveKit.
- *The leader* holds the leader key and schedules: it consumes the campaign
  events, claims due leads within the free capacity of the whole cluster,
  re-gates them, and assigns each call to the node with the most free
  capacity, at `AD_CALLS_PER_SEC` (one queue, so each placement sees the one
  before). On `attempt_ended` it settles the lead, closes the room, and
  removes the assignment. A repair pass moves the calls of nodes that are
  gone (to the least-loaded node, even when full: those calls are live),
  removes keys of ended or moved calls, and assigns claimed calls that never
  reached a node.

**Fencing.** The leader writes the new owner and a raised `assign_gen` to
the attempt row in Postgres first, then the key to kine. A node writes an
attempt event only while the row names it with its generation
(`PgAttemptRepo::apply_owned`). A node that lost a call (it was killed, cut
off, or slow) can no longer dial it or change it. Two leaders for a moment are
safe too: claims are `SKIP LOCKED`, the campaign cap is checked under the
campaign row lock, and the generation decides each call.

**Failure behavior.** A node that stops cleanly (SIGTERM) deletes its keys,
so its calls move at once. A node that dies loses its key after
`AD_CLUSTER_TTL_S`, then its calls move. The node carries the media of its
calls (§0.0), so a connected call dies with its node: the new owner's
reconcile finds the call's participant gone from the room and ends the
attempt. A dial that was in flight on the dead node becomes `uncertain` and
is decided by the reconcile (GAPS.md DIAL-4). Neither is dialed again.

**Agents (decision D-2).** `ad_agent` starts the campaign's agent per call:
`livekit` (explicit dispatch by agent name; the agent joins as `agent-AJ_…`)
or `pipecat` (a start request with a room token to `start_url`; the bot joins
as `agent-attempt-{id}` with pipecat's LiveKit transport). Both get the same
call facts. The attempt statechart has an agent region: the dial waits until
the agent is in the room; an agent that does not join in `join_timeout_s`
ends the attempt as `not_dialed: no_agent`; an agent that leaves ends a
connected call. Rooms are one per attempt (`{prefix}-{attempt id}`).

**Proof.** `crates/ad_cluster/tests/kine.rs` runs against a real kine:
conditional writes, TTL expiry, the mirror, election and takeover, assignment
load and orphans. `crates/ad_attempt` tests the agent region (property: no
dial before the agent is ready). `crates/ad_agent/tests/pipecat.rs` tests the
start request and its token. `e2e/run_e2e.sh` runs one call with a real
LiveKit agent and one with a real pipecat bot. `e2e/run_cluster_e2e.sh`
(`docker-compose.cluster.yml`) runs three nodes with both runtimes, kills a
node and then the leader, and checks that every lead is dialed exactly once.

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
  ad_orchestrator/# schedule, retry, settlement, circuit breaker (pure).
  ad_cluster/     # kine: nodes, leader, call assignment (§0.10.8b). BUILT.
  ad_agent/       # LiveKit and pipecat agent adapters (D-2). BUILT.
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
  by a **singleton orchestrator** over events (dograh: Redis pub/sub; ours:
  Postgres LISTEN/NOTIFY, §0.10.8a)
  (`SyncCompleted`, `BatchCompleted`, `BatchFailed`, `CallCompleted`,
  `RetryNeeded`, `CircuitBreakerTripped`).
- **Concurrency slots** (dograh: Redis; ours: Postgres rows, decision D-3) with an org-level cap *and* a secondary
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
| `vicidial_live_agents` (who's available) | **Concurrency slots** (Postgres rows, decision D-3) = guaranteed-available agent capacity. A call is only placed when a slot is acquired. |
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
                           │        │   LISTEN/NOTIFY events │               │
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
                           │        │ slot rows                │ SIP egress  │
                           │        │ (org + campaign scope)   ▼             │
                           │  ┌───────────────┐     ┌──────────────────┐    │
                           │  │ Postgres:     │     │ sip-egress /     │    │
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
│   ├── ad_concurrency/             # slot manager on Postgres rows (org + campaign scope),
│   │                               #   acquire/bind/release, stale recovery, fleet count.
│   ├── ad_events/                  # campaign event contract (typed); Postgres transport in ad_db.
│   ├── ad_attempt/                 # the call-attempt statechart (pure). BUILT (§0.10.7a).
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
- `call_attempts` (BUILT, §0.10.7a) — one row per call attempt: the
  statechart snapshot and its state, the outcome class, SIP code, detail,
  failure flag, dial, answer, and end times, room, participant identity,
  worker, `checked_at`, and `settled_at`. It replaces the planned `hopper`
  claim rows and `runs`; the claim reads `leads` directly.
- `attempt_events` (BUILT) — each applied statechart event, by attempt and
  sequence number.
- `queued_runs` — dograh-style: unique `source_uuid`, state
  (queued/claimed/processed/failed), retry bookkeeping, due-at.
- `runs` — planned. `call_attempts` now holds the attempt; these columns join
  it with the agent outcome (GAPS.md AGNT-2): lead_id, campaign_id, provider, call status
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

Helm chart `deploy/helm/agentdialer` (built; `helm lint` + `helm template`
pass). Structure mirrors dograh's proven chart.

**Cluster mechanism (decided + built, §0.10.8b).** Every orchestrator replica
is a cluster node. One node leads (a key in kine with a TTL) and assigns each
call to a node; every node runs its calls. A Kubernetes Service is only a
load-balancer VIP and grants no single-instance execution, so there is
deliberately **no Service** for the orchestrator; nothing calls it. A node that
lost a call is fenced by the attempt's assignment generation in Postgres.

`e2e/run_cluster_e2e.sh` runs three nodes in Docker Compose, kills a node and
then the leader, and checks that every lead is dialed exactly once. The chart
renders exactly one Service (api), zero orchestrator Services, and
`hostNetwork: true` on the gateway pods.

- **Deployments**
  - `api` — axum, Service (ClusterIP) + HPA (CPU), `/health` probes.
  - `orchestrator` — the cluster nodes (replicas 2+, no Service). Scale it to
    add nodes; one node leads through kine (§0.10.8b).
  - `dispatch` — N replicas, HPA (optional; disabled by default). Stateless;
    claims via `SKIP LOCKED`. This is how we scale "agent capacity-bound"
    dialing.
  - `gateway` — media pods: `hostNetwork: true`, a fixed RTP port range, a media
    node selector. SIP/RTP carry IPs inside the packets, so these cannot sit
    behind a ClusterIP Service (jambonz pattern, §0.5).
  - `ui` (later phase) — HPA + PDB.
- **Jobs**: `migrate` (pre-install/upgrade hook, weight -5).
- **Stateful deps**: Postgres (prefer managed/RDS; internal chart for dev),
  Redis (LiveKit SIP), object store (S3/MinIO) later.
- **Config/Secrets**: `database.existingSecret` / `livekit.existingSecret`
  (DATABASE_URL, LIVEKIT_API_KEY/SECRET). Inline values for dev only.

Singletons rationale (from dograh scaling doc): the orchestrator coordinates
global scheduling state; duplicating it causes double-dispatch. Dispatch workers
are stateless and idempotent (DB claims), so they scale freely.

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
  the stale-slot timeout and DB claim-recovery are the backstop, not the
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
  `ad_orchestrator` (a leader elected in kine, §0.10.8b, schedule-closed
  windows, circuit breaker, completion/stale recovery); job/retry/callback queue
  via **`apalis`**/`pgmq`.
- Accept: end-to-end test campaign — leads load, hopper fills respecting
  timezone/DNC, calls initiate only within window + under cap, abandonment ~0%,
  pause drains in-flight and stops scheduling, retries re-queue, campaign
  completes. Crash-a-dispatch-worker test shows `SKIP LOCKED` claim recovery, no
  dup calls. Several orchestrator replicas run calls; one leader schedules.

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
- **Orchestrator** availability: every replica is a node, and a node that dies
  loses its calls to the others after the kine TTL (§0.10.8b). kine itself
  must be highly available (several kine replicas on one managed Postgres).
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
