# AgentDialer — Gap Analysis

| | |
|---|---|
| Status | Assessment. Not a plan of record. `PLAN.md` stays authoritative. |
| Date | 2026-10-20 |
| Last update | 2026-10-24: dial order, abandonment ledger, DIAL-9 built |
| Code baseline | commit `b19ad3a` |
| Method | Full read of `PLAN.md`, `README.md`, and every crate. Targeted read of the nine sources in `_research/` that `PLAN.md` cites, plus the FreeSWITCH dialplan evaluator (`_research/freeswitch/`). |
| Legal note | This document is not legal advice. Verify every legal item with counsel before you claim compliance. |

`_research/` is git-ignored. The reference paths in this document point to the
local research corpus.

---

## 1. Purpose

This document records where AgentDialer is missing features, where it is
incorrect, and where it can be better than the open-source tools we studied.
Each gap has an ID, a severity, the current state in our code, the reference
behavior, and a direction. Use the IDs in commits and PRs.

## 2. The goal, and what it changes

We build a world-class outbound dialer with full dialer features. It has these
constraints:

- **Agents only.** Every call goes to an AI agent. LiveKit agents are the first
  agent runtime. There are no human agents.
- **SIP only.** We never interface with the PSTN. The carrier or PBX is
  downstream of our SIP egress.
- **Rust, and new.** We re-express the proven ideas. We do not port the Perl,
  PHP, or Python line for line.

These constraints remove large parts of the references and change other parts:

| Reference concept | Status for AgentDialer | Reason |
|---|---|---|
| Agent screen, login, pause codes, hotkeys, manual dial | Not needed | No humans. |
| Adaptive pacing (`AST_VDadapt.pl`) | Keep the idea, change the signal | VICIdial tunes the dial level to the idle humans. Our agents start for each call, so the pace follows the measured time that an agent takes to join (DIAL-9). |
| AMD in the switch | Moves to the agent | The agent hears the answer and classifies it. |
| IVR, call menus, prerecorded broadcast | Moves to the agent | The agent speaks and listens. |
| Abandonment control | Keep, and make it provable | The rules still apply. Agent first makes abandonment zero by construction. Customer first (the default) abandons a call when the agent joins late; the 2 s limit, the join-paced controller (DIAL-9), and the 3% governor (COMP-10) keep it under the cap. |
| Compliance data model (call times, DNC, limits) | Keep in full | This is legal exposure. AI voice adds rules (see COMP-6). |
| Campaign brain (lists, hopper, recycle, callbacks, dispositions) | Keep in full | This is the product. |

## 3. How to read this document

Severity:

- **P0** — A real campaign cannot dial correctly, safely, or legally. Fix before any production call.
- **P1** — Needed for feature parity with a production dialer.
- **P2** — Improvement or differentiator.

Each gap uses this form:

- **Now:** what our code does, with the file.
- **Reference:** what an open-source tool does, with the file.
- **Gap:** the difference that matters.
- **Do:** the recommended direction.

---

## 4. Summary

### 4.1 What is strong now (keep it)

- `ad_compliance` is pure, deterministic, and fails closed. Property tests cover
  DST, half-hour zones, midnight wrap, and holidays.
- One leader schedules for the cluster: a key in kine with a TTL, and Postgres
  fences a node that lost a call (D-8). It is not `replicas: 1`. (Until
  2026-10-21 this was a Postgres advisory lock.)
- The hopper claim uses `FOR UPDATE SKIP LOCKED`.
- Outcome write-back is idempotent (`UNIQUE (lead_id, attempt_at)`).
- The SIP↔WebRTC bridge (`ad_gateway` `LiveKitParticipant` + `MediaBridge`) carries
  two-way audio in a real LiveKit room with no `livekit/sip`. The CI workflow
  includes that e2e. It has not run in CI yet.
- The Docker-first test tier builds, lints, and tests everything, including
  libwebrtc.

### 4.2 The gaps that block a real campaign

| ID | Gap |
|---|---|
| COMP-1 | Production dialing uses a 24-hour window and an empty DNC list. |
| COMP-2 | A lead without an explicit IANA timezone never dials. There is no derivation. |
| COMP-6 | No consent record exists. AI voice calls need consent (verify). |
| COMP-7 | An opt-out in the call cannot reach the DNC list. |
| DIAL-1 | The gateway reported every call end as "answered". Partly fixed 2026-10-21: SIP codes map to classes and the webhook ends the call; the agent outcome is open. |
| DIAL-2 | Capacity was not held for the length of the call. Fixed 2026-10-21: a live call attempt is the unit of capacity. |
| DIAL-3 | The orchestrator dialed one call at a time. Fixed 2026-10-21: calls run in parallel, paced by `AD_CALLS_PER_SEC`. |
| DIAL-4 | A timeout after the INVITE could cause a second dial. Fixed 2026-10-21: the attempt becomes `uncertain` and is reconciled first. |
| DIAL-9 | Customer first (the default dial order) has no pacing from the agents' join time. A burst of answers starts more agents than the workers can bring into the rooms in 2 s, and those calls are abandoned. New 2026-10-24. Built 2026-10-24: the claim stays within a window for each agent pool, learned from the join times; item 7 (the agent breaker) is open. |
| LEAD-1 | Nothing filled the hopper. Fixed 2026-10-21: the claim reads the due leads directly. |
| LEAD-2 | Retries lived only in Redis. Fixed 2026-10-20: the due time is on the lead row. |
| LEAD-3 | Claims never released. Fixed 2026-10-21: the call attempt is the claim, and settlement updates the lead once. |
| ORCH-1 | Campaign state never became paused or completed. Fixed 2026-10-21: both are saved with their events. |
| AGNT-1 | No agent was dispatched. Fixed 2026-10-21: a LiveKit agent or a pipecat bot joins each attempt room. Since 2026-10-24 the campaign's `dial_order` decides when: on the answer (`customer_first`, the default) or before the dial (`agent_first`). |
| AGNT-2 | There is no outcome contract from the agent to the dialer. |
| API-1 | The API has no authentication and no tenant isolation. |
| OPS-1 | The orchestrator cannot start from Helm or `e2e/run_e2e.sh` (no `REDIS_URL`). Fixed 2026-10-20. |

### 4.3 Feature coverage

"—" means we did not find the feature in our reading of that reference.

| Feature | VICIdial | dograh | AgentDialer now | Gap |
|---|---|---|---|---|
| Campaign lifecycle | Yes | Yes | Built | — |
| Lists, list mix, expiration | Yes | Sources only | Missing | LEAD-6 |
| Lead import and dedupe | Yes | Yes | Partial | LEAD-6 |
| Hopper / lead selection | Yes | Yes | Engine only, no fill | LEAD-1 |
| Lead ordering options | Many | Random | Two | LEAD-7 |
| Recycle by disposition | Yes | — | Missing | LEAD-4 |
| Retries by reason | Partial | Yes | Durable on the lead row; one global policy | LEAD-2 |
| Scheduled callbacks | Yes | — | Missing | LEAD-5 |
| Alternate numbers | Yes | — | Missing | LEAD-8 |
| Lead filters | Yes (raw SQL) | — | Missing | LEAD-9 |
| Disposition catalog with flags | Yes | Yes | Minimal | LEAD-10 |
| Safe-hours in lead timezone | Yes | No (campaign tz, fails open) | Engine built, not wired | COMP-1 |
| Timezone derivation | Yes (fails open) | — | Missing | COMP-2 |
| Per-state windows | Yes | — | Missing | COMP-3 |
| Holidays | Yes (narrowing) | — | Full-day block only | COMP-4 |
| Call-frequency limits | Yes | — | Missing | COMP-5 |
| Consent records | — | — | Missing | COMP-6 |
| In-call opt-out to DNC | Human action | VICIdial adapter | Missing | COMP-7 |
| DNC lists | Yes | Via VICIdial | Engine only | COMP-8 |
| Abandonment ledger | Yes | Breaker only | Built for customer first, one jurisdiction | COMP-10 |
| Capacity-bound pacing | Human-based | Yes (slots) | Built (a live attempt is the slot) | DIAL-2 |
| Adaptive pacing | Yes (idle humans, drop rate) | No (fixed rate, offline load test) | Built (by agent join time) | DIAL-9 |
| Dial-rate limit (CPS) | — | Yes | One rate for the cluster | DIAL-3 |
| Caller-ID pools | Yes | Yes | Missing | DIAL-6 |
| Trunk selection and failover | Partial | Yes | Missing | DIAL-7 |
| Circuit breaker | — | Yes | Partial | DIAL-8 |
| Voicemail / machine policy | Asterisk AMD | Answer supervisor | Missing | AGNT-5 |
| Agent dispatch per call | n/a | Pipeline per run | Built (LiveKit, pipecat) | AGNT-1 |
| Agent outcome contract | Human UI | Status + LLM disposition | Missing | AGNT-2 |
| Transfer | Yes | Yes | Missing | AGNT-4 |
| A/B agent split | — | Yes | Missing | AGNT-6 |
| Routing rule engine | Asterisk dialplan | — | Engine, not wired | ROUT-1..4 |
| Recordings | Yes | Yes | Missing | OBS-3 |
| Reports | Yes | Yes | Missing | OBS-2 |
| Management API | Non-agent API | REST | Partial | API-2 |
| Auth and tenancy | User groups | Org scoping | Missing | API-1 |
| Outbound webhooks | — | Yes | Missing | API-4 |
| Own SIP and media leg | n/a | n/a | Built: the node carries the call (D-9) | MEDIA-1 |
| Singleton leader | One cron server | `replicas: 1` | Built, fenced (kine, D-8) | ORCH-5 |

---

## 5. Gaps by area

### 5.1 Compliance (COMP)

#### COMP-1 Production dialing has no real compliance data — P0
- **Now:** `crates/ad_bin/src/orchestrator.rs` builds `CallWindowSet::uniform(0, 2400)` and `DncSet::default()`. No table stores call times, holidays, or DNC entries.
- **Reference:** VICIdial binds a call-time set to each campaign (`vicidial_campaigns.local_call_time`). It stores `vicidial_call_times`, `vicidial_state_call_times`, `vicidial_call_time_holidays`, `vicidial_dnc`, and `vicidial_campaign_dnc` (`extras/MySQL_AST_CREATE_tables.sql`).
- **Gap:** The engine is correct, but production never gives it real inputs. A lead with a timezone can be called at any hour.
- **Do:** Persist call-time sets, holiday sets, DNC lists, and campaign compliance config. Load them for each batch. Deny every call of a campaign that has no call-time set.

#### COMP-2 No timezone derivation — P0
- **Now:** `Lead.timezone` must be an explicit IANA name. Otherwise the engine denies the call. This is correct fail-closed behavior, but most lead lists have no timezone column.
- **Reference:** VICIdial `lookup_gmt` (`www/vicidial/functions.php` line 597) tries postal code, owner tz code, NANPA prefix, area code, state, then a country default. `vicidial_phone_codes.php_tz` holds IANA names. The country default fails open.
- **Gap:** Without derivation, most real leads never dial.
- **Do:** Derive the IANA zone at load time from explicit tz, postal code, NANPA area code and prefix, and state. Store the zone and its source. Do not fall back to a country default when the country has more than one zone. Deny when no source resolves.

#### COMP-3 No per-state calling windows — P1
- **Reference:** VICIdial state rules replace the default window for leads in that state (`bin/AST_VDhopper.pl` ~lines 1800–2124).
- **Gap:** We cannot express state rules that are stricter than the federal window.
- **Do:** Model state windows. Use the intersection of the legal window, the state window, and the campaign window. An intersection can never widen the legal window, even if an operator misconfigures a state rule. This is stricter than VICIdial, which replaces the window.

#### COMP-4 Holiday semantics — P1
- **Now:** A holiday blocks the full day in the lead's local date.
- **Reference:** A VICIdial holiday narrows the window (start = max, stop = min). It has an ACTIVE/INACTIVE status. State rules have their own holidays.
- **Do:** Support "closed" and "narrowed" holidays per call-time set and per state. Keep "closed" as the default.

#### COMP-5 Call-frequency limits — P1
- **Now:** `SelectionInput.call_count_limit` exists, but production passes `None`.
- **Reference:** VICIdial `call_count_limit`, `daily_call_count_limit`, `daily_phone_number_call_limit`, `call_limit_24hour` (per lead or per phone number, with a per-state override container), and `drop_lockout_time`. Some state laws cap calls per 24 hours (verify per state).
- **Do:** Keep rolling-window counters per E.164 number and per lead, per campaign and global, with per-state overrides. Check them at selection and again at dispatch.

#### COMP-6 Consent and AI-voice rules — P0 (legal)
- **Now:** No consent record exists.
- **Context:** In February 2024 the FCC ruled that AI-generated voices are "artificial" voices under the TCPA. Such calls need the consent that prerecorded calls need. Verify the required consent level with counsel.
- **Gap:** The dialer cannot prove consent for a call.
- **Do:** Store a consent record per lead: type, source, timestamp, scope, and an evidence reference. Each campaign declares the consent level it needs. Deny a call when the record is absent or does not cover the campaign.

#### COMP-7 In-call opt-out — P0 (legal)
- **Now:** There is no path from the agent to the DNC list.
- **Reference:** dograh `providers/ari/external_pbx/vicidial.py` `apply_do_not_call`. VICIdial statuses carry a `dnc` flag. `PLAN.md` §0.10.5 lists an `add_dnc` action.
- **Do:** Add a `dnc_request` with a scope to the agent outcome contract (AGNT-2). Write the DNC entry at once, with evidence. Cancel all pending retries and callbacks for that number. Verify the required timing per jurisdiction.

#### COMP-8 DNC data management — P1
- **Now:** A `DncSet` is built in memory for each request. There is no storage, no version, and no import.
- **Reference:** VICIdial has internal, campaign, area-code (`NPAXXXXXXX`), other-campaign, and international DNC. A hit at hopper time rewrites the lead status to DNCL or DNCC.
- **Do:** Persist DNC lists (system, org, campaign, area code, national-registry import with its scrub date). Index them by E.164. Record the list version that each decision used. Set the lead status on a hit. Track the national-registry re-scrub interval (verify).

#### COMP-9 Phone number normalization — P1
- **Now:** `dnc::normalize` keeps digits only. Area codes work only for NANP numbers of 10 or 11 digits. The API accepts any non-empty phone string.
- **Do:** Normalize every number to E.164 at load time, with a default country per list. Reject invalid numbers with a report. Use E.164 for DNC keys, frequency counters, and timezone lookup.

#### COMP-10 The abandonment ledger — partly done (2026-10-24) — P1
- **Done (2026-10-24), with customer first:** a customer-first call whose agent is not in the room 2 s after the 200 OK ends as `abandoned` (`ad_attempt`; the pure statechart also turns a late `agent_ready` into `abandoned`). `call_attempts` is the ledger: `PgAttemptRepo::abandon_counts` counts the answered and abandoned calls that ended in the rule's window. The leader reads it before each claim: above the cap (`JurisdictionRule::allows_customer_first`), or when it cannot read it, the claim dials agent first. The retry of an abandoned lead dials agent first. `GET /v1/campaigns/:id/abandonment` reports the ledger.
- **Still open:**
  - Every campaign uses `us_tsr` (`JurisdictionRule::CAMPAIGN`); a campaign cannot name its jurisdiction. `UkOfcom` uses the US formula.
  - Every answer counts as a human answer: there is no answering-machine detection (AGNT-5). VICIdial removes machine answers from the denominator.
  - The TSR safe harbor also needs a recorded message (the seller's name and a phone number) within 2 s on an abandoned call (16 CFR 310.4(b)(4)(iii)). The dialer hangs up and plays nothing.
  - The limit counts from the 200 OK, not from the end of the callee's greeting. This is stricter than the rule.
  - The ledger query counts 30 days of attempts on each claim of a customer-first campaign. A large campaign needs a rolling counter.
  - "In the room" is the agent's join, not its first audio (`agent_first_audio_at`).
  - Click-to-call: the limit counts from the node's answer of Twilio's INVITE. The phone answered earlier, at Twilio.
  - The margin is small: a burst of answers on a cold agent pool abandoned calls. DIAL-9 has the measurements and the fix (a window for each agent pool, learned from the join times, built 2026-10-24).
  - The governor counts only ended calls. Without DIAL-9, every call of a burst could abandon before the governor saw the first one. With DIAL-9, at most the window `K` can.
- **Reference:** VICIdial counts drops (DROP, XDROP) over human-answered statuses. Its UK mode removes the estimated answering-machine share of drops (`bin/AST_VDadapt.pl` ~line 3946). `drop_call_seconds` defines a drop.
- **Open question:** Does an AI agent count as a "live sales representative" for the 2-second rule? Ask counsel (D-5).

#### COMP-11 Deny reasons and experimental status — P2
- **Now:** An empty or invalid number returns `Dnc(System)`. `JurisdictionRule::is_experimental` appears in no API, although the repo guide requires it.
- **Do:** Add an `InvalidNumber` deny reason. Show the experimental status in every API response that uses a jurisdiction.

#### COMP-12 Recording and identity disclosures — P1
- **Do:** Store per campaign the disclosures the agent must speak (caller identity, purpose, recording notice for all-party-consent jurisdictions). Pass them to the agent. Record in the attempt that the agent spoke them.

#### COMP-13 Reassigned numbers — P2
- **Do:** Check consented numbers against a reassigned-numbers source before dialing (for example the FCC database in the US). Record the check in the decision record.

### 5.2 Dial loop (DIAL)

#### DIAL-1 The call outcome comes from the wrong place — P0
- **Now:** `ad_gateway` `Gateway::initiate` returns `Answered` for every created SIP participant. `LiveKitGateway` maps every error to `Failed(String)` and drops the SIP status.
- **Reference:** dograh `services/telephony/status_processor.py` takes the outcome from a terminal provider status. Alexor-Brain `app.py` `/api/livekit/webhook` (line 1423) uses `room_finished`. The LiveKit sample `outbound-caller-python/agent.py` reads `sip_status_code` from `TwirpError`.
- **Gap:** Busy and no-answer never occur in production. Every non-answer counts as a breaker failure. No real disposition reaches the lead.
- **Do:** Separate placement from outcome. `initiate` returns "placed" or "rejected with a SIP code". The outcome arrives from a terminal event: the LiveKit webhook, the SIP final response, and the agent outcome (AGNT-2). Map SIP codes to a typed taxonomy (BETTER-7).
- **Status (2026-10-21), partial:** placement and outcome are separate. The gateway waits for the answer in a task of its own and returns a `Placement`: answered, rejected (with the SIP code from the LiveKit error metadata), not sent, or no response. `ad_attempt::classify_sip` maps the code to busy, no answer, declined, invalid number, or failed. The end of the call comes from the LiveKit webhook (`POST /v1/webhooks/livekit` on the api role). A reconcile against LiveKit is the backstop. Open: the agent outcome (AGNT-2). A completed call has no disposition yet.

#### DIAL-2 Capacity is not held for the call — P0
- **Now:** The orchestrator skips `ad_dispatch::Dispatcher`. `Dispatcher` releases its slot when `initiate` returns. `ad_db::leader::in_call_count` is never called.
- **Reference:** dograh binds a slot to the run and releases it on the terminal status (`call_concurrency/service.py`, 20-minute stale cutoff, org cap and campaign cap). VICIdial dials `goal − live calls` (`bin/AST_VDauto_dial.pl` lines 1213, 1434). Alexor-Brain waits while live calls ≥ cap.
- **Do:** Make a live attempt the slot. Count attempts in dialing, ringing, or connected state per campaign and per org. Acquire before the dial. Release on the terminal transition. Recover stale attempts.
- **Status (2026-10-21):** fixed. A live call attempt is the unit of capacity. The claim locks the campaign row, counts the live attempts, and claims no more than `campaigns.concurrency_cap` (API field `concurrency_cap`, default 10). The capacity frees only when the attempt ends. `crates/ad_db/tests/pg_attempts.rs` proves the cap and that parallel claims are disjoint. Open: an org cap and a trunk cap (DIAL-7).

#### DIAL-3 Serial dialing — P0
- **Now:** `dispatch_batch` awaits each `initiate` with `wait_until_answered: true`, then sleeps `per_call_gap`. One unanswered call blocks the batch for the full ring time.
- **Reference:** dograh runs up to 10 setups in parallel with a 240-second batch deadline and a per-campaign token bucket (`campaign_call_dispatcher.py`). Alexor-Brain dials with `wait_until_answered=False`.
- **Do:** Launch attempts at the same time, bounded by free capacity. Pace launches with a token bucket per campaign and per trunk. Never block the scheduler on ring time.
- **Measured (SIPp benchmark, 2026-10-20, all in Docker):** the dial rate does not follow `AD_CALLS_PER_SEC`. With a 300 ms ring, 30 calls at a configured 10/s ran at 1.29 attempts/s, and 100 calls at 50/s ran at 1.08 attempts/s. With a 3 s ring the rate fell to 0.34/s: each call waits for its answer. Answered setup took 328–386 ms (p50 333 ms). Each failed setup stalled the loop for 5.7–8.1 s. Peak live calls: 11. Harness: `_research/scratch/sip-bench/`.
- **Measured again after OPS-6, OPS-7, and D-3 (2026-10-20):** every call of the three tiers was answered (140 of 140). The failed-setup stalls are gone. The rate is still serial: 2.12 attempts/s at a configured 10/s, and 2.71 attempts/s at a configured 50/s. Each attempt waits for its answer (about 310 ms) and then for the spacer. With a 3 s ring the rate stays at 0.34/s.
- **Status (2026-10-21):** fixed. Each dial runs in a task of its own, so the scheduler never waits for a ring. A `Pacer` (`ad_dispatch`) spaces the call starts at `AD_CALLS_PER_SEC`. Open: the pacer is one for the cluster (in the leader, since D-8), not one per campaign or per trunk (DIAL-7). The window of DIAL-9 (built 2026-10-24) also limits the claims by the agents' join time.
- **Measured after the fix (SIPp benchmark, 2026-10-21, all in Docker):** the dial rate follows `AD_CALLS_PER_SEC`. 30 calls at a configured 10/s ran at 10.0 attempts/s (before: 2.12). 100 calls at 50/s ran at 50.0 attempts/s (before: 2.71), with 84 live calls at the peak. With a 3 s ring, 10 calls at 50/s ran at 50.0 attempts/s (before: 0.34): the ring no longer stops the dialing. With `concurrency_cap` 5, 40 calls never had more than 5 live, and the rate fell to 1.6 attempts/s, as the cap requires. All 180 calls completed. The LiveKit webhook ended every call, and every campaign completed in Postgres.

#### DIAL-4 An uncertain initiation can dial twice — P0
- **Now:** Every error becomes `Failed`, and `Failed` schedules a retry.
- **Reference:** dograh `_finish_interrupted_dispatch` marks a timeout or a lost connection after the request was sent as "uncertain". It never requeues the row, keeps the slot, and reconciles later.
- **Do:** Classify errors as "rejected" (a SIP final response arrived) or "uncertain" (no response). For an uncertain attempt, hold the slot and reconcile from the provider before any retry.
- **Status (2026-10-21):** fixed. A connect error before the request leaves is a setup error: no call exists. Any other transport error is "no response": the attempt becomes `uncertain` and keeps its capacity. The reconcile (`AD_ATTEMPT_CHECK_MS`) then asks LiveKit whether the call is in its room. If it is, the attempt connects. If it is not, the attempt ends as `uncertain`, and only then can a retry follow. The dial start is saved before the request leaves, so a stopped dialer leaves a `dialing` attempt, and the reconcile decides it after the longest dial time.

#### DIAL-5 No call timers — P1
- **Now:** one fixed ring timeout of 30 s for all campaigns (`ad_gateway::RING_TIMEOUT`). No maximum call duration and no silence timeout.
- **Reference:** VICIdial `dial_timeout` (default 60 seconds).
- **Do:** Add a ring timeout, a maximum call duration, and a silence timeout per campaign. With DIAL-9, a ringing call holds a place in the window of its agent pool, so the ring timeout also sets the throughput of the pool.

#### DIAL-6 No caller-ID management — P1
- **Reference:** dograh selects a from-number and derives the trunk from the number (`services/telephony/base.py` `select_trunk`). VICIdial has `campaign_cid` and CID groups.
- **Do:** Add caller-ID pools per campaign with rotation and optional local presence by area code. Track number health (answer rate). Bind each number to its trunk. Pass identity headers so the carrier can attest.

#### DIAL-7 No trunk registry or failover — P1
- **Now:** One `AD_SIP_TRUNK_ID` environment variable.
- **Reference:** dograh stores trunks as rows. VICIdial limits calls per trunk.
- **Do:** Add a trunk registry with concurrency and CPS limits, a breaker per trunk, and a failover order. Let the selection ruleset choose a trunk.

#### DIAL-8 Breaker semantics — P1
- **Now:** A trip only blocks dialing until the window passes. The campaign then resumes by itself. The trip compares with `>`.
- **Reference:** dograh sets the campaign to `paused`, logs the failing run ids, and resets the breaker on resume (`circuit_breaker.py`). It compares with `>=`.
- **Do:** On a trip, set the campaign to `paused` with the reason and evidence. Reset on operator resume. Document the comparison. Keep the campaign breaker separate from the trunk breaker (DIAL-7).
- **Also:** When the orchestrator cannot read the breaker (a database error), it treats the breaker as closed and keeps dialing. Since 2026-10-20 it logs this as a warning. Decide whether this path must fail closed.
- **Seen in the SIPp benchmark (2026-10-20):** 19 calls that never sent an INVITE (a local media error) counted as call failures and tripped the breaker. Our own misconfiguration paused the campaign. BETTER-7 separates these classes.
- **Status (2026-10-21), partial:** a trip pauses the campaign in Postgres, with one `circuit_breaker_tripped` event in the same transaction. A resume through the API sets `campaigns.breaker_reset_at`, and the window counts only attempts that ended after it. `e2e/run_events_e2e.sh` proves both. The breaker counts only attempts that started a dial; `failed`, `setup_error`, and `uncertain` are failures, and busy and no answer are normal results. Open: the comparison, a trunk breaker, and the read-error path.

#### DIAL-9 The pace does not follow the agents' join time — built 2026-10-24 (item 7 open)
The user's goal for the product: the dialer sets the call rate from the pace at which the agents join. This is the main difference from other dialers (BETTER-6).

- **Now:**
  - The leader starts calls at one fixed rate for the cluster (`AD_CALLS_PER_SEC`, default 10, `ad_dispatch::Pacer`). The only other limit is `campaigns.concurrency_cap`. No limit knows how fast the agents join.
  - Customer first is the default dial order. The node starts the agent at the 200 OK, and the agent must be in the room 2 s after it. If it is not, the node hangs up and the call ends as `abandoned` (COMP-10).
  - The governor (COMP-10) decides at the claim, before the answer, and it counts only ended calls. So the leader can claim every call of a burst customer first before the first abandoned call is in the ledger.
  - Each attempt keeps its join time: `answered_at` and `snapshot.agent_ready_at`. An agent-first attempt keeps its start time only in `attempt_events` (`agent_starting`). The attempt does not keep its agent pool.
- **Measured (2026-10-24, one machine, all in Docker):**
  - One call at a time: a LiveKit agent joined 332–620 ms after the answer, and a pipecat bot 147–233 ms after it.
  - `e2e/run_cluster_e2e.sh` (3 nodes, 24 calls, one Python LiveKit worker, no warm processes): the first 6 LiveKit customer-first calls answered at almost the same time. Their agents joined 810, 1256, 1390, 1497, and 1822 ms after the answer. The sixth agent did not join in 2 s, and that call was abandoned. Each extra agent that started at the same time added about 0.1–0.5 s.
  - After that abandoned call, the governor dialed campaign 1 agent first. The abandoned lead waits 15 min for its retry, so the script fails at its deadline. The cluster e2e fails now because of this gap.
  - Before the start request moved to the 200 OK, all 6 agents missed 2 s by about 0.2 s.
- **Cause:** the join time depends on the load of the agent pool. A burst of answers starts many agents at once, and each start waits for the ones before it. A fixed rate cannot know the speed of the pool: the same rate is safe for a warm pool and abandons calls on a cold pool.
- **Reference:**
  - VICIdial tunes its pace by itself. `bin/AST_VDadapt.pl` `calculate_dial_level` (~lines 6813–7090) sets the dial level (lines for each agent) from the last minute: the average number of READY agents minus the average number of calls that wait for an agent (~line 2763). `adaptive_intensity` scales the change, `adaptive_maximum_level` is the top, and 1.0 is the bottom. Drop rules come on top: more than 20 calls and more than 50% drops in the last minute halve the level. When the drop rate of the day is above `adaptive_dropped_percentage`, the level goes to 1.0 (`HARD_LIMIT`), halves (`AVERAGE`), or tapers to the end of the day (`TAPERED`). Its agents are humans who stay connected, so it never measures a join time.
  - dograh does not tune at run time. `api/services/call_concurrency/rate_limiter.py` is a fixed token bucket for each campaign (`rate_limit_per_second`, a 1 s window in Redis, fails closed). Org and campaign slots limit the live calls. KEDA scales the pods on the active calls of each pod, and an offline load test finds the calls for each pod (`api/services/observability/loop_lag.py`). Its bot runs in the process of the call, so it has no join step.
  - LiveKit agents (`livekit_agents/livekit-agents/livekit/agents/worker.py`): a worker above `load_threshold` (0.7 in production) gets no new jobs, and `num_idle_processes` keeps warm processes (the CPU count in production, 0 in development). This back-pressure stays inside LiveKit. The dialer gets no signal from it.
  - Commercial predictive dialers (Genesys, Five9, NICE) adjust the pace from the answer rate and the handle times, with a target abandonment rate as the limit. This comes from their public product descriptions, not from code that we read.
  - TCP congestion control learns a capacity that nobody announces. Slow start and AIMD (RFC 5681) react to loss. Delay-based control (TCP Vegas, BBR) reacts to the time of each reply before a loss occurs. The join time is our "time of each reply", and an abandoned call is our "loss".
- **Gap:** no reference paces by the agents' join time, because their agents are connected before the call. Our agents start for each call. The join time tells how many answers the agent pool can take at once. We measured it for each call, and we did not use it.
- **Status (2026-10-24): built**, as items 1–6 and 8–10 below, with the decisions of D-10 (both modes for each campaign, every limit value settable at run time). Item 7 is open.
  - `ad_pacing` is the pure controller: `observe` (one sample in, the next state out) and `limit` (the window limit for a mode). Unit tests and property tests cover it.
  - `ad_db`: migration 0008 adds `agent_pools` (settings and state for each pool), `campaigns.pacing` (the mode), and on `call_attempts` the pool, the window and limit at the claim, `agent_started_at`, `agent_ready_at`, and `paced_at`. `claim_paced` locks the pool row, reads the limit, counts the window, and inserts in one transaction. Settings that do not read give a limit of 1 (fail closed). `ingest` learns from each ended or joined attempt once.
  - The leader learns before each claim and on each sweep. An agent in the room publishes `agent_joined`, so the next claim does not wait for the sweep.
  - The node takes the time of the start request before it sends it, so the join time includes the request.
  - The node takes the join time from the monotonic clock. In one `e2e/run_e2e.sh` run, the wall clock of the WSL2 machine stepped back about 0.56 s during a call, and a join of 781 ms read as 207 ms from the wall-clock times. The abandonment test on the join uses the same time. Since OPS-9, every time of a call comes from one read of the Postgres clock plus the monotonic clock.
  - API: `GET` and `PUT /v1/campaigns/:id/pacing`, `GET /v1/agent-pools`.
  - Proven: `e2e/run_e2e.sh` (a slow pool goes from a limit of 2 to 1, with no abandoned call and no claim above its limit) and `e2e/run_cluster_e2e.sh` (the burst on the cold LiveKit worker ends with no abandoned call; each pool started at 1 and grew to 3 and 4). The mutation check (no window limit in the claim) failed 5 of the 9 `pg_pacing` tests and both e2e runs.

**Do: a join-paced controller.** Built 2026-10-24 as below; the notes "Built:" give the differences.

1. **One controller for each agent pool.**
   - A pool is a LiveKit `agent_name` (on the dialer's LiveKit server), or a pipecat `start_url`. All the campaigns that use the pool share its controller. Built: the pool keeps the query of the `start_url` (`AgentSpec::pool()`), because a query can select a different bot.
   - The leader owns all the controllers, so the nodes need no coordination. Built: the claim locks the pool's row, so two claims of one pool cannot both take its last place.
   - Save the pool key on each attempt at the claim (OBS-1). Then a change of the campaign's agent does not move live attempts to a different pool.
2. **A window, not a rate.**
   - The window of a pool counts its attempts that can still start an agent or are starting one:
     - customer-first attempts that are dialing or ringing,
     - customer-first attempts that are answered and have no agent in the room yet,
     - agent-first attempts whose agent is starting.
   - The leader claims a call for the pool only while the window is below the pool's limit `K`.
   - A ringing call counts, because it can answer at any time. So at most `K` agents start at once, even when all ringing calls answer at the same instant.
   - A rate cannot give this bound: at any rate, a burst of answers can come.
   - The throughput of a pool is about `K` ÷ the average time in the window (ring time plus join time). Example: `K` = 10 and 15 s of ring give about 0.67 dials per second. The controller raises `K` until the joins get slow, so it finds this throughput by itself. With a low answer rate, few ringing calls answer at once, the joins stay fast, and `K` grows.
3. **One sample for each agent start.**
   - A sample is the time from the start request to the agent in the room. For customer first, the start is the 200 OK. For agent first, it is `agent_starting`. Add `agent_started_at` to the snapshot.
   - A sample also records the window at its start.
   - These samples are slow samples: a start that the runtime refuses, an agent that does not join in time, and an abandoned call. Built: an abandoned call is a slow sample only when no agent was in the room at the limit. A call that a stopped node abandoned, or that the callee ended before the limit, says nothing about the pool.
   - A pipecat pool and a LiveKit pool have different speeds. Each pool learns only from its own samples.
4. **Learn `K` from the samples (AIMD).**
   - The two limits come from the rule's limit (`JurisdictionRule::connect_within`, 2 s for `us_tsr`). The target is half of it (1.0 s). The back-off is three quarters of it (1.5 s). Both are below the hang-up, so the controller slows down before a call is abandoned.
   - Slow start: start at `K` = 1. After each round of `K` fast samples, double `K`. The first sample above the target ends the slow start.
   - After the slow start, each sample at or below the target adds 1/`K`, so `K` grows by 1 for each round.
   - A sample between the target and the back-off does not change `K`.
   - A slow sample sets `K` to `max(1, K ÷ 2)`. A second slow sample from the same round does not halve `K` again. A round is the samples that started before the last halving.
   - Raise `K` only when the window was full (at least `K` − 1) at the start of the sample. A quiet hour must not raise `K`, because a later burst then finds a limit that nobody proved.
   - `K` never goes above the sum of the concurrency caps of the pool's campaigns. Built differently: `K` grows only when a claim found the window full (window + 1 ≥ `K`), and the caps and the cluster capacity limit the window, so `K` stops near them (at most one doubling above). `max_k` is a hard top that the operator sets at run time.
5. **The other limits stay.** `AD_CALLS_PER_SEC` still spaces the call starts, and `concurrency_cap` still limits each campaign. The leader uses the smallest limit (BETTER-6). Trunk limits come with DIAL-7.
6. **Keep `K` across a leader change.** Save `K`, the phase (slow start or not), and the time of the last halving in Postgres, one row for each pool. The leader writes the row when `K` changes, and a new leader reads it. Without the row, a new leader starts again at `K` = 1. That is safe, but the ramp can take minutes when few calls answer. Built: `agent_pools.state`, written by each `ingest`.
7. **Agent failures (from AGNT-3). Open.** A start that the runtime refuses is a slow sample (built). If `K` = 1 and 5 starts in a row fail, pause the pool's campaigns with the reason and the attempt ids, as the campaign breaker does (DIAL-8). Count these failures apart from the call failures.
8. **The governor stays.** COMP-10 is still the hard net above the cap. The window also limits a burst: at most `K` calls can abandon before the governor sees the first one.
9. **Show it.** `GET /v1/campaigns/:id/pacing` shows the pool key, `K`, the phase, the window now, and the join times of the last 50 samples (p50, p95, and the number of slow samples). Export the join times and `K` as metrics (OBS-4). Built, without the metrics. Also built (D-10): `PUT` changes the mode and the pool's settings (target, back-off, first `K`, largest `K`) at run time, and `reset` starts the pool again; `GET /v1/agent-pools` lists the pools. The leader also logs each learning step.
10. **Put it where `PLAN.md` puts pacing.** Built. `PLAN.md` §0.10.6 names a registry of dial strategies in a crate `ad_pacing`. That crate does not exist yet. Make the controller a pure engine in `ad_pacing`: it takes the samples and returns `K`, with no I/O. Make it the strategy for customer first. Correct §0.10.6 in the same change. It says now that `Progressive` gives about 0% abandonment, which is not true for customer first.

**Tests (all in the same change):**
- Unit tests of the pure controller:
  - the slow start doubles `K` after each round,
  - the first sample above the target ends the slow start,
  - a fast sample adds 1/`K`,
  - a sample between the target and the back-off changes nothing,
  - a refused start, a missing agent, and an abandoned call halve `K`,
  - one round halves `K` one time only,
  - `K` does not grow when the window was not full,
  - `K` stays between 1 and the ceiling.
- Property tests: `K` is always 1 or more. A slow sample never raises `K`. Fast samples only never lower `K`. The same samples always give the same `K`.
- Postgres tests (`ad_db`):
  - the window counts the four kinds of attempts in item 2,
  - it does not count an attempt with an agent in the room or an ended attempt,
  - two campaigns of one pool share one window,
  - the claim takes no call for a full pool,
  - a new leader reads `K` back.
- API contract test of `/pacing`: 503 without Postgres, 404 for an unknown campaign, and the values from Postgres.
- `e2e/run_e2e.sh`: add a slow pipecat pool through the bot's test control (`start_url` with `hold_ms=1600`, so each bot joins in about 1.8 s: slower than the back-off, faster than the hang-up). Give it 4 leads. The test passes when no call is abandoned and no two attempts of the pool overlap in the window, from their times in Postgres. Built differently: `hold_ms=700` and 6 leads, with the target (300 ms), the back-off (600 ms), and the first `K` (2) set through the API at run time. This also tests the run-time settings, and the joins stay far from the 2 s hang-up.
- `e2e/run_cluster_e2e.sh`: the burst on the cold LiveKit worker must end with no abandoned call. Check that `K` started at 1 and grew.
- Mutation check: remove the window check from the claim. Then the slow-pool test and the cluster test must fail. After the mutant run, touch the changed files in the real tree (the build cache shares `/src/target`).
- Later, with BETTER-8: a simulator with a pool model (join time grows with the starts in progress) and a carrier with an answer rate. Assert that almost no call is abandoned and that `K` settles near the capacity of the pool.

**Alternatives, and why they are not the default:**
- Start the agent at 180 Ringing (a third dial order). This gives the agent the ring time as a head start. But it starts an agent for each call that does not answer, and the user decided that an agent starts only for an answered call.
- Warm agent workers (LiveKit `num_idle_processes`, a pool of started pipecat processes). They make the joins faster and `K` larger. They are the operator's setup, and they do not tell the dialer the pace. Document them in the README for operators.
- A fixed cap for each pool, set by the operator. This needs a load test for each pool, as dograh does. It cannot follow a pool that scales or gets slower.
- A predictive mode that counts a ringing call at its measured chance to answer, tuned by the ledger as VICIdial does. The controller above already lets `K` grow when few calls answer, so this mode is not necessary to start.

**Open (decide before the build, D-10):**
- The target and the back-off (1.0 s and 1.5 s, from the 2 s rule).
- A strict mode for each campaign: `K` never goes above the largest number of starts at once that all joined fast. Then no call is abandoned by construction, but the throughput is lower.
- A first `K` for each pool in the agent JSON (data, no migration), so a known pool does not start at 1.
- The slow-start round for a pool with few answers.

### 5.3 Lead lifecycle and selection (LEAD)

#### LEAD-1 Nothing fills the hopper — P0
- **Now:** Only tests call `PgHopperRepo::insert_leads`. The e2e scripts seed the hopper with SQL. Leads from `POST /v1/campaigns/:id/leads` never dial.
- **Reference:** VICIdial fills `vicidial_hopper` every minute from cron (`bin/AST_VDhopper.pl`). dograh syncs the source into `queued_runs`.
- **Do:** Replace the fill step with an eligibility index (BETTER-3). Each lead carries `next_eligible_at`, and the claim reads it directly.
- **Status (2026-10-21):** fixed with BETTER-3. The claim reads the due leads straight from `leads` (`status` new or queued, `next_eligible_at` empty or past). Migration 0006 drops the `hopper` table. Leads that `POST /v1/campaigns/:id/leads` loads now dial.

#### LEAD-2 Retries are not durable — P0 (fixed)
- **Now:** Retries live only in the Redis sorted set `ad:timers:{campaign}`. A Redis loss leaves leads in `queued` with a claimed hopper row. This breaks the rule in `PLAN.md` §0.10.8a.
- **Reference:** dograh writes a new `queued_runs` row with `scheduled_for`, `retry_count`, and `parent_queued_run_id`. It persists the decision before the run turns terminal (`db/campaign_client.py` `record_campaign_retry_decision`).
- **Do:** Store the retry due time in Postgres. Redis may wake the orchestrator, but it must never hold the only copy.
- **Status (2026-10-20):** fixed with decision D-3. The pure `retry_decision` gives a due time or "exhausted". `record_outcome` writes `leads.next_eligible_at` and `retry_reason` in the same transaction as the `call_outcomes` row. A sweep (`readmit_due`) clears the due time and makes the hopper row claimable in one statement. `e2e/run_events_e2e.sh` proves that a due retry is re-admitted and dialed again. Still open: one global policy (no per-campaign `retry_config`), and the retry does not check the safe-hours window of the due time (LEAD-4).

#### LEAD-3 Claims never release — P0
- **Now:** `hopper.claimed_by` stays set after the outcome. `in_call_count` would grow without limit. `ad_hopper` treats `Dispositioned` as dialable at once, so the hopper preview lists a lead as eligible again right after its call. No rule controls when it may dial again.
- **Reference:** VICIdial deletes the hopper row at dial (`bin/AST_VDauto_dial.pl` line 2084) and uses `called_since_last_reset` with recycle rules. dograh moves rows through queued, processing, processed, failed, and recovers claims after 360 seconds.
- **Do:** Give each attempt explicit states. Release the claim at the terminal state. Give each claim a lease with expiry and recovery. Make a dispositioned lead eligible only through a recycle, retry, or callback rule.
- **Also (found 2026-10-20):** A `no_capacity` or `compliance_denied` result at dispatch sets the lead to `dispositioned`, and its hopper row stays claimed. A lead that was never dialed leaves the queue. The code comment says the lead stays claimable; it does not. Fix this with the attempt states.
- **Status (2026-10-21):** fixed. The claim creates a call attempt and sets the lead to `in_call`. When the attempt ends, the settlement (the pure `ad_orchestrator::settle`) updates the lead once, keyed by `call_attempts.settled_at`: a retry (queued with a due time), `dispositioned`, `done`, `dnc`, or `blocked` (no timezone). A dispositioned lead is not dialable (`ad_hopper`). A gate denial no longer drops the lead: outside the window it waits 15 minutes, a DNC hit sets `dnc`, an unknown timezone sets `blocked`, and the attempt row keeps the reason. A pending attempt without a dial task is released after 60 s. Open: recycle and callback rules (LEAD-4, LEAD-5), and the next window opening as the due time (BETTER-3).

#### LEAD-4 Recycle rules — P1
- **Reference:** VICIdial `vicidial_lead_recycle` (status, `attempt_delay` default 1800 s, `attempt_maximum` default 2). It counts the delay in calling time: it subtracts the overnight closed gap (`bin/AST_VDhopper.pl` ~lines 2126–2215).
- **Do:** Add recycle rules per campaign by disposition. Count the delay in open calling time. Limit the number of recycles.

#### LEAD-5 Scheduled callbacks — P1
- **Reference:** VICIdial `vicidial_callbacks` (USERONLY or ANYONE, `callback_time`, customer timezone, hopper priority 50). QuickVoice `campaign-time.ts` resolves DST gaps and overlaps on purpose.
- **Do:** Let the agent request a callback time and zone (AGNT-2). Store it. Make the lead eligible at that time with a higher priority. Re-gate it on compliance. Use an explicit DST gap and overlap rule. "USERONLY" becomes "same agent version".

#### LEAD-6 Lists, list mix, import, dedupe — P1
- **Now:** Leads belong directly to a campaign. Import is a JSON array.
- **Reference:** VICIdial `vicidial_lists` (active, expiration, per-list call time) and `list_order_mix`. dograh source sync with a unique `source_uuid`. VICIdial vendor-lead-code dedupe.
- **Do:** Make lists first-class and shareable across campaigns. Add list-mix ratios and expiration. Dedupe on import by E.164 and external id. Map CSV columns to fields and context. Return a validation report.

#### LEAD-7 Lead ordering — P1
- **Now:** `UpCount` and `Oldest`.
- **Reference:** VICIdial orders by lead id, last name, phone, count, last call time, rank, owner, timezone, random, and "Nth new", with a secondary order and an optional random tiebreak (`bin/AST_VDhopper.pl` lines 3080–3096).
- **Do:** Compose order keys: priority or rank, recycle versus fresh, timezone-aware (dial the zones whose window closes first), seeded random.

#### LEAD-8 Alternate numbers — P1
- **Reference:** VICIdial `auto_alt_dial` tries `alt_phone`, `address3`, and extended numbers with priorities 25, 20, 15.
- **Do:** Give a lead several numbers with a type and an order. After a no-answer, try the next number by rule. Check each number against DNC and frequency limits on its own.

#### LEAD-9 Lead filters — P1
- **Reference:** VICIdial `lead_filter_sql` injects raw SQL into the hopper query.
- **Do:** Express filters as typed conditions over lead fields, with the routing-engine conditions. Never accept raw SQL.

#### LEAD-10 Disposition catalog — P1
- **Now:** `LeadStatus` has six lifecycle states. `call_outcomes.outcome` holds a free string.
- **Reference:** VICIdial statuses carry the flags `human_answered`, `sale`, `dnc`, `customer_contact`, `not_interested`, `unworkable`, `scheduled_callback`, `completed`, `answering_machine`, and a category, at system and campaign level. dograh separates `call_status` (how the call ended) from `call_disposition` (the business result).
- **Do:** Keep the lead lifecycle state separate from the disposition. Add a disposition catalog per org and campaign with these flags. Let the flags drive abandonment math, DNC, completion, and reports.

#### LEAD-11 Quotas — P2
- **Reference:** VICIdial `demographic_quotas`.
- **Do:** Stop dialing a segment when its target count is reached.

### 5.4 Routing engine (ROUT)

#### ROUT-1 The semantics differ from FreeSWITCH — P1 (decision D-1)
- **Now:** `ad_routing` ANDs the conditions of a group, runs anti-actions when any condition of the group fails, decides `break` per group, and defaults to `Break::Never`. A capture `$N` stays in the context across conditions.
- **Reference:** FusionPBX turns each group into stacked `<condition>` elements and attaches the actions to the last one (`app/dialplans/resources/classes/dialplan.php` `xml()`). FreeSWITCH evaluates each condition on its own. The default break is `on-false` (`mod_dialplan_xml.c` line 175). It exits on pass with `on-true`, on fail with `on-false`, or always (line 584). `never` never exits and never skips ahead. Substitution of `$N` uses the condition's own match (line 543). An extension stops the hunt unless it has `continue="true"` (line 712).
- **Gap:** `PLAN.md` §0.10.5 says "exactly FusionPBX". The engine is a different model. Rules ported from FusionPBX will behave differently.
- **Do:** Decide D-1. If we match FreeSWITCH, make break and anti-actions per condition and default to `on-false`.

#### ROUT-2 The engine is not wired — P1
- **Now:** No ruleset storage and no ruleset API exist. Production passes `ruleset: None`. Actions are free strings. The hopper honors only `skip`, `hangup`, and `add_dnc`.
- **Do:** Store versioned rulesets. Bind a selection ruleset and an outcome ruleset to each campaign. Add a typed action registry (`set_trunk`, `set_caller_id`, `schedule_callback`, `recycle`, `add_dnc`, `set_status`, `transfer`, `limit`, `hangup`).

#### ROUT-3 Missing constructs — P1
- **Reference:** FreeSWITCH time conditions (`wday`, `hour`, `minute-of-day`, `time-of-day`, `date-time`, `mday`, `mon`, `year`, `yday`, `week`, `mweek`), `regex="all|any|xor"`, extension `continue`, global versus domain scope, and FusionPBX destinations.
- **Do:** Add these constructs. Add the functions that `PLAN.md` lists (`strftime`, `tz_now`, `lead_data`, `dnc_check`, `hash`).

#### ROUT-4 Explain and reference tests — P2
- **Do:** Return a trace of each evaluation (which condition matched, which break fired). Port the Call-Forward, User-Record, and Outbound-Route reference dialplans as tests (`PLAN.md` §0.10.13).

### 5.5 Orchestrator and events (ORCH)

#### ORCH-1 Campaign state never settles — P0
- **Now:** A breaker trip and a completion only write a log line. The orchestrator publishes `campaign_completed` when one claim returns empty, even when calls are live or retries are pending.
- **Reference:** dograh `complete_campaign_if_idle` sets `completed` only when sync is done, no queued or processing rows remain, and no call is active.
- **Do:** Make completion a database predicate: no work due now or later, no live attempt, sync done. Set the state in the same transaction. Make the breaker pause durable (DIAL-8).
- **Status (2026-10-21):** fixed. `complete_if_idle` sets `completed` only when no lead waits for a dial now or later and every attempt has ended and settled. The `campaign_completed` event commits with the state. `e2e/run_e2e.sh` proves it after a real call. The breaker pause is durable (DIAL-8).

#### ORCH-2 Events can be lost — P1
- **Now:** Events are published after the commit, and errors are ignored (`.ok()`). Redis pub/sub delivers at most once. Reconciliation runs only on reconnect.
- **Reference:** dograh runs a 60-second sweep that recovers claims and orphaned work. Alexor-Brain polls every 180 seconds as a backstop.
- **Do:** Write events to an outbox table in the same transaction and relay them. Run a periodic sweep for stale claims, stuck attempts, orphaned work, and completion.
- **Status (2026-10-20), partial:** Decision D-3 moved the events to Postgres. Each event is a row in `campaign_events`, and the publisher sends `NOTIFY` in the same transaction. The consumer reads rows after its cursor on each notification and on a sweep (`AD_EVENT_SWEEP_MS`), so a lost notification only delays an event. Publish errors are logged. Still open:
  - The event row and the state change it announces commit in two transactions. If the process stops between them, the event is lost. Retries recover through the retry sweep, but a lost `batch_completed` stalls the campaign until the next reconnect or restart. Put the event insert into the outcome transaction, and add a periodic reconcile.
  - The publisher takes one advisory lock for the cluster, so event inserts are serialized. This is fine at dialer rates, but it is a ceiling.
  - `campaign_events` grows without a limit. Add retention (OBS-5).
- **Update (2026-10-21):** the attempt end, the settlement, the completion, the breaker pause, and the API campaign transitions now write their events in the same transaction as the state (`ad_db::events::publish_in`, an outbox). The sweep (`AD_RETRY_SWEEP_MS`) settles ended attempts and dispatches every running campaign, so a lost event delays work by one sweep. Still open: the publish lock ceiling, and retention of `campaign_events` and `attempt_events` (OBS-5).

#### ORCH-3 Two orchestration models — P1
- **Now:** `run_loop.rs` and `gate()` are used only by tests and the API. The binary uses the reactor. `Schedule` fails open, but `PLAN.md` §7.1 says the schedule window fails closed. Production never applies a business schedule.
- **Do:** Keep one model. Apply the business schedule in production. Decide its failure mode in `PLAN.md`; the compliance gate stays fail-closed in all cases.

#### ORCH-4 A finished call does not trigger dialing — P1
- **Now:** The reactor treats `call_completed` as no-op. Only `batch_completed` starts the next batch.
- **Reference:** dograh schedules the next batch on `CallCompletedEvent`.
- **Do:** Treat freed capacity as a trigger for new launches.
- **Status (2026-10-21):** fixed. `attempt_ended` settles the lead and dispatches into the freed capacity. In `e2e/run_events_e2e.sh` (capacity 1, a 5 s sweep), five dials follow each other in about 150 ms through NOTIFY.

#### ORCH-5 No leader fencing — P1
- **Now:** The leader holds the advisory lock on one pooled connection and never checks it. If that connection drops, Postgres releases the lock, a standby takes it, and the old leader keeps dialing.
- **Do:** Check the lock connection on a short interval and stop dialing at once when it fails. Add a fencing token (leader epoch) to the attempt writes so a stale leader cannot commit.
- **Also (2026-10-21):** dials now run in parallel tasks. A deposed leader keeps its running tasks until it stops, and they can still save results. The attempt row lock keeps each attempt consistent, but a deposed leader can still start new dials until it notices.
- **Status (2026-10-21):** fixed with decision D-8 (`PLAN.md` §0.10.8b). The leader is a key in kine with a TTL; there is no advisory lock. A node that loses the key stops scheduling at once. Each attempt row carries the node and an assignment generation, and a node writes an event only while the row names it with its generation (`apply_owned`), so a node or a leader that lost a call cannot dial it or change it. `e2e/run_cluster_e2e.sh` kills a node and then the leader and checks that every lead is dialed exactly once.

### 5.6 Agent integration (AGNT)

#### AGNT-1 No agent dispatch — P0
- **Now:** Rooms are `{prefix}-{lead_id}`. `e2e/run_e2e.sh` passes because a test agent waits in `campaign-1` and the test lead has id 1. In production no agent joins these rooms.
- **Reference:** Alexor-Brain `services/call_service.py` calls `create_dispatch(agent_name, metadata)` and then `CreateSIPParticipant` in room `call-{call_id}`. The LiveKit sample starts the agent session before it dials.
- **Do:** For each attempt, create room `attempt-{id}`, dispatch the named agent with metadata (lead context, campaign, attempt id, variant, disclosures), and confirm the agent joined. Then originate the call.
- **Also (2026-10-21):** rooms are still `{prefix}-{lead_id}`. Each call attempt stores its `room` and `participant_identity` (`sip-attempt-{id}`), so a room per attempt is one change in the claim.
- **Status (2026-10-21):** fixed for both runtimes (`ad_agent`, decision D-2). Each attempt has its own room (`{prefix}-{attempt id}`). A `livekit` campaign agent is dispatched by name with the call facts as job metadata; a `pipecat` agent gets a start request with a room token. The dial waits until the agent is in the room; an agent that does not join in `join_timeout_s` ends the attempt as `not_dialed: no_agent`, and the lead comes back after a minute. `e2e/run_e2e.sh` runs one call with a real LiveKit agent and one with a real pipecat bot.

#### AGNT-2 No outcome contract — P0
- **Reference:** dograh answer actions (`START_OPENING`, `RELEASE`, `LEAVE_MESSAGE`, `DROP`, `SCREEN_THEN_REARM`, `WAIT_FOR_SCREENING`), machine subtypes, and post-call disposition extraction. The LiveKit sample tools `end_call`, `transfer_call`, `detected_answering_machine`.
- **Do:** Define a versioned outcome schema: answer class, disposition code, callback time and zone, DNC request and scope, transfer request, summary, disclosures spoken. Carry it on the gRPC session stream (decision D-6). Make it idempotent by attempt id. Validate it against the campaign catalog.

#### AGNT-3 No agent-capacity signal — merged into DIAL-9 (2026-10-24)
- LiveKit gives the dialer no worker availability; it keeps the worker load inside its dispatch. The measured join time is the capacity signal, and DIAL-9 uses it before each claim. The breaker on agent failures is DIAL-9 item 7. Still true from 2026-10-21: a start that the runtime refuses, or an agent that does not join in time, ends the attempt without a dial and does not count as a call.

#### AGNT-4 Hangup, transfer, and orphan cleanup — P1
- **Reference:** `delete_room` for hangup and `transfer_sip_participant` for transfer (LiveKit sample). Alexor-Brain `_cleanup_paused_campaigns` ends live calls of a stopped campaign.
- **Do:** Add API actions to end or transfer an attempt. On pause or stop, apply a policy: let live calls finish, or end them. Transfer targets are other agents or SIP URIs. There are no human queues.

#### AGNT-5 Voicemail and machine policy — P1
- **Reference:** dograh answer supervisor (`services/pipecat/processors/answer_supervisor.py`, `answer_classification.py`). VICIdial `cpd_amd_action` and `amd_send_to_vmx`.
- **Do:** Pass a per-campaign answer policy to the agent (leave a message, drop, screen). The agent reports the class. The dialer applies the retry rule for that class.

#### AGNT-6 Agent variants (A/B) — P2
- **Reference:** dograh `traffic_split.py` (weights sum to 100, `sha256(seed:phone) % 10000`). QuickVoice experiments with basis-point allocation, hashed assignment, goals, conversions, attribution.
- **Do:** Add variants with deterministic assignment per contact. Store the assignment on each attempt. Report per variant. Add goals and conversion events after that.

#### AGNT-7 Other agent runtimes — P2
- **Do:** Keep the agent target behind a registry, so a pipecat SIP worker can be added later without a schema change.

### 5.7 Media plane (MEDIA)

#### MEDIA-1 Production still depends on `livekit/sip` — FIXED 2026-10-21 (D-9)
- **Now:** The orchestrator dials through `CreateSIPParticipant`. `PLAN.md` §0.9a says we own the SIP↔WebRTC boundary.
- **Do:** Add a `CallInitiator` over drachtio, rtpengine, and our bridge. Keep `LiveKitGateway` only as a fallback until the new path passes the same e2e tests.
- **Update (2026-10-21, D-9):** the new path is not drachtio + rtpengine. The node carries SIP and RTP with vendored `rsipstack` and `rustrtc`, behind Kamailio, with no host networking (`PLAN.md` §0.0, slices 1–6).
- **Status (2026-10-21):** fixed. The node's call task dials through `ad_sip` (via Kamailio), carries the audio with `ad_media`, and bridges it into the room with our participant; `CreateSIPParticipant` and `livekit/sip` are gone from the code and from the e2e tests. `e2e/run_e2e.sh` (one node, both runtimes) and `e2e/run_cluster_e2e.sh` (three nodes, 24 calls) pass through a NAT that stands in for the AWS NAT gateway. Open (`PLAN.md` §0.0): the `rustrtc` latching patch (RTP bleed), 407 auth and a resent 200 against SIPp, session timers, the trunk OPTIONS check, the `public` media mode, DTMF, and removal of the LiveKit webhook code.

#### MEDIA-2 drachtio client is a spike — CLOSED by D-9 (2026-10-21)
- **Now:** `spike/ad_sip_ctl` authenticates and sends a raw INVITE. It never sends ACK, BYE, or CANCEL. It does not handle provisional responses, digest auth, keepalive, reconnect, or CDR events.
- **Reference:** `drachtio-srf/lib/drachtio-agent.js` (frame tokens, ACK with the dialog id, PRACK, `route|invite`, `cdr:*`) and `srf.js` `createUAC` (1xx, 3xx, final response, digest via `digest-client.js`, missing Contact).
- **Do:** Build `ad_sip_ctl` with these flows and conformance tests against a real drachtio container.
- **Status (2026-10-21):** closed. D-9 drops drachtio. The SIP flows listed here (ACK, BYE, CANCEL, 1xx, digest auth, keep-alive) come from vendored `rsipstack` and our call driver in `ad_sip` instead.

#### MEDIA-3 rtpengine client is a spike — P1
- **Now:** `spike/ad_rtpengine_ctl` sends ping, offer, answer, and delete with no flags.
- **Reference:** `rtpengine/docs/ng_control_protocol.md` (`codec` transcode/mask/strip, `transport-protocol`, `ICE`, `replace`, `query`, recording, `play media`, `play DTMF`) and `transcoding.md`.
- **Do:** Build `ad_rtpengine_ctl` with typed flags, `query` statistics, and a guaranteed `delete` on every exit path (RAII).

#### MEDIA-4 No RTP leg into the bridge — P1
- **Now:** The bridge e2e feeds a generated tone.
- **Do:** Add a UDP RTP source and sink. Decode and encode G.711. Resample between 8 kHz and 48 kHz, or open the LiveKit audio source at 8 kHz. Add a jitter buffer and RTP sequence and timestamp handling. Pass RFC 4733 DTMF to the agent as events.

#### MEDIA-5 No call router (`ad_router`) — P1
- **Do:** Compose the leg: rtpengine offer, drachtio INVITE, early media, final response, rtpengine answer, bridge start, and teardown on BYE from either side. Map SIP codes for DIAL-1.

#### MEDIA-6 Media pods on Kubernetes — P2
- **Do:** Follow `PLAN.md` §0.5: `hostNetwork`, RTP port ranges, graceful drain, optional rtpengine kernel module.

### 5.8 API, tenancy, and security (API)

#### API-1 No authentication or tenant isolation — P0
- **Now:** Any caller can read or change any campaign by id. `org_id` is a query parameter.
- **Reference:** dograh `api/AGENTS.md` requires an `organization_id` filter on every read and write of tenant data.
- **Do:** Add authentication (API keys first). Take the org id from the credential. Scope every query by org. Return 404 for a cross-org id.

#### API-2 Missing management endpoints — P1
- **Do:** Add endpoints for campaign config (schedule, pacing, retry, recycle, call-time set, DNC scope, jurisdiction, agent), lists, DNC, call times, rulesets, dispositions, callbacks, attempts, outcome ingest, hangup and transfer, and reports. VICIdial `non_agent_api.php` (add_lead, update_lead, add DNC) shows the minimum set.

#### API-3 `dispatch/run-batch` gives misleading results — P1
- **Now:** It uses a stub initiator that always answers and a fresh breaker on every request.
- **Do:** Remove it, or mark it `experimental` in the response.

#### API-4 Outbound webhooks — P2
- **Reference:** dograh `tasks/webhook_delivery.py`, `tasks/run_integrations.py`.
- **Do:** Send signed post-call events to customer systems with retries.

#### API-5 Idempotent import and pagination — P2
- **Do:** Accept an idempotency key on lead import. Paginate every list endpoint.

### 5.9 Data, reports, and observability (OBS)

#### OBS-1 The attempt record is too thin — P1
- **Now:** `call_outcomes` stores outcome, answered, and attempt time.
- **Do:** Store per attempt: SIP code and reason, trunk, caller ID, ring, answer, and end times, answer class, disposition, agent and variant, room, recording reference, and the decision record (BETTER-2).
- **Status (2026-10-21), partial:** `call_attempts` stores the SIP code and detail, the dial, answer, and end times, the room, the participant identity, and the worker. `attempt_events` keeps every state change in order. Open: trunk, caller ID, answer class, disposition, agent, variant, recording, and the decision record (BETTER-2).
- **Update (2026-10-24):** the snapshot also keeps `agent_ready_at`, and the row keeps the agent identity and state. DIAL-9 added (2026-10-24) the agent pool key (`call_attempts.pool`), the agent start time (`agent_started_at`), and the window and limit at the claim.

#### OBS-2 No reports — P1
- **Do:** Add real-time campaign stats, per list, status, and disposition, funnels, an abandonment report per jurisdiction, a carrier report (answer rate, average duration, post-dial delay, SIP code mix), and per-variant results.

#### OBS-3 No recordings — P1
- **Do:** Record through rtpengine or LiveKit egress. Store consent marks. Apply retention and erasure.

#### OBS-4 No tracing or metrics — P1
- **Do:** Add one trace per attempt across dialer, SIP, media, room, and agent. Expose Prometheus metrics. Keep phone numbers out of logs (`ad_core::NonSensitive` exists but nothing uses it).
- **Status (2026-10-20), partial:** The orchestrator now logs one `call attempt` line per call (campaign, lead, outcome, error detail, setup time), one `batch finished` line per batch, and every exclusion at dispatch. Errors it used to drop with `.ok()` are now warnings. All logs go to stdout and stderr. Traces and metrics are still missing.

#### OBS-5 No data lifecycle — P1
- **Do:** Add GDPR export and erasure of lead data and recordings, an audit log, and time partitions for attempts.

### 5.10 Operations and tests (OPS)

#### OPS-1 The orchestrator cannot start from Helm or `run_e2e.sh` — P0
- **Now:** `orchestrator.rs` line 394 exits when `REDIS_URL` is not set. The Helm chart does not set it. `e2e/run_e2e.sh` last changed in `3ac01d9`, before the Redis commit `58fe190`, and does not set it. We found this by reading. We did not run it.
- **Do:** Set `REDIS_URL` in the chart and the script. Run `run_e2e.sh`. Correct the README status until it passes.
- **Confirmed (2026-10-20):** an unmodified run of `run_e2e.sh` failed with `Error: REDIS_URL must be set`.
- **Status (2026-10-20): fixed as a class.** `crates/ad_bin/src/config.rs` defines the variables of each role. Each role reports every missing, blank, or invalid variable at startup. The chart (`redis.url` / `redis.existingSecret`), `run_e2e.sh`, and `docker-compose.yml` now provide `REDIS_URL`. Tests in `ad_bin` fail when the Helm chart, an e2e script, or the README "Configuration" table drifts from the contract. Later the same day, decision D-3 removed `REDIS_URL` from the contract. The drift tests made the chart, the scripts, and the README follow in the same change.

#### OPS-2 Chart drift — P1
- **Now:** The gateway Deployment runs the `dispatch` role, which prints a message and exits. The chart sets `AD_TICK_MS` and `AD_MAX_CONCURRENT`, which nothing reads. The chart has no Redis.
- **Do:** Align the chart with the binary. Add a test that renders the chart and checks every env var against what the binary reads.
- **Status (2026-10-20), partial:** The orchestrator, API, and migrate templates now match the contract, and the drift test covers them. The chart wires the LiveKit key and secret in both modes. The gateway Deployment still runs the stub `dispatch` role.

#### OPS-3 Two e2e suites are not in CI — P1 (fixed)
- **Now:** The CI workflow runs the test tier and the WebRTC e2e. `run_e2e.sh` and `run_events_e2e.sh` are manual, which is how OPS-1 went unseen.
- **Do:** Run all e2e suites in CI.
- **Status (2026-10-20):** fixed in `.github/workflows/ci.yml`: the jobs `e2e` and `events-e2e` run the two scripts. The scripts pass locally in Docker. The jobs have not yet run on GitHub, because the change is not pushed.

#### OPS-4 The e2e LiveKit SIP config allowed one live call — P1 (fixed)
- **Found by:** the SIPp benchmark (2026-10-20). 19 of 20 calls failed with `media failed: failed to listen on udp port` while the first call held its port.
- **Cause:** `spike/livekit/sip.yaml` set `rtp_port: 20000` and `rtp_port_range_end: 20050`. livekit/sip reads `rtp_port` as a range `start-end` and has no `rtp_port_range_end` key, so the range was one port. `run_e2e.sh` places one call, so it never showed.
- **Status:** fixed (`rtp_port: 20000-20100`). OPS-7 later widened the range to 20000-20999.

#### OPS-5 The e2e scripts used host tools — P1 (fixed)
- **Now:** The scripts called host `curl` and `python3` and wrote logs to `/tmp`, against the Docker-first rule.
- **Status (2026-10-20):** fixed. `e2e/lib.sh` runs HTTP polling and JSON parsing in a container. The scripts keep their logs in `e2e/.out/` (git-ignored). The host needs only bash and the Docker CLI.

#### OPS-6 About 8% of benchmark calls fail to join their room — P1 (fixed)
- **Found by:** the SIPp benchmark (2026-10-20): 2 of 30 and 8 of 100 calls failed before the INVITE, about every eleventh call.
- **Evidence:** livekit/sip logs "waiting for connection establishment failed: could not connect after timeout"; livekit-server logs "cannot create sip participant: update room failed: could not connect after timeout". The WebRTC join from livekit/sip to the room times out.
- **Suspected cause (not confirmed):** the e2e LiveKit server has 51 WebRTC ports (`spike/livekit/livekit.yaml`, 50000–50050), and every container shares the host network, so client sockets can take ports in that range.
- **Do:** Widen the range outside the host ephemeral range, re-run the benchmark, and confirm. Production sizing follows from the result.
- **Status (2026-10-20):** fixed and confirmed. The range is now 21000–21999, outside the host ephemeral range 32768–60999. In the benchmark re-run, no call failed to join its room:

  | Tier 1 (30 calls at 10 cps) | Before | After |
  |---|---|---|
  | Answered | 28 of 30 | 30 of 30 |
  | Attempts per second | 1.29 | 2.46 |
  | Longest gap between attempts | 5149 ms | 495 ms |

#### OPS-7 The e2e LiveKit SIP RTP range runs out at 50 cps — P1 (fixed in e2e; sizing rule open for production)
- **Found by:** the same benchmark re-run (2026-10-20). After OPS-6, the loop is faster, and tier 2 (100 calls at 50 cps) placed about 6 attempts per second. 39 of the first 60 attempts failed with `media failed: failed to listen on udp port`.
- **Measured (2026-10-20):** the benchmark now samples the UDP sockets in both LiveKit ranges every 0.5 s (`_research/scratch/sip-bench/udp_sampler.py`). It also reads the RTP ports that livekit/sip offers in its SDP.
  - livekit/sip takes only even ports. It offered 61 ports in three tiers (distinct per tier), and none was odd. The range 20000–20100 has 51 even ports, and the socket count stopped at exactly 51.
  - livekit/sip keeps each RTP port about 30 s after the BYE. Tier 2 started with the 30 ports of tier 1 still bound. They closed 13–42 s into tier 2.
  - The room participant of livekit/sip opens about 28 UDP sockets per live call (tier 3: 3 live calls, peak 85), on ports outside the RTP range. They close at the end of the call. The earlier guess that these sockets take ports from `rtp_port` was wrong. Size the file-descriptor limit of livekit/sip for this.
- **Cause:** at about 6 attempts per second, ports in use = attempts per second × (call length + 30 s), which is more than 51.
- **Status:** fixed for the e2e stacks: `spike/livekit/sip.yaml` now has `rtp_port: 20000-20999` (500 even ports), below the LiveKit RTC range 21000–21999. The file states the sizing rule.
- **Confirmed (2026-10-20):** with the wider range, tier 2 answered 100 of 100 calls, and up to 93 RTP ports were bound at the same time (the old limit was 51).
- **Update (2026-10-21):** with parallel dials (DIAL-3) at up to 50 calls per second, up to 140 RTP ports were bound at the same time (this count includes the ports of the previous tier that had not closed yet). The e2e range is now `rtp_port: 10000-19999` (5000 even ports). The same run had up to 1602 sockets in the LiveKit RTC range (21000-21999) at 84 live calls, and livekit/sip had about 21 UDP sockets per live call. Size the RTC range with the same care. No call failed.
- **Do:** Apply the rule to production: ports in use = calls per second × (average call length + 30 s), and the range holds twice that number. At 50 cps and 3-minute calls this is about 10,500 ports in use, so one livekit/sip node is not enough. This also applies to the rtpengine port range of our gateway (MEDIA-1). Measure the linger time on the production version.
- **Also:** the Helm chart default gateway RTP range (`gateway.rtpPortStart` 30000 to `rtpPortEnd` 40000) overlaps the host ephemeral range 32768–60999. This is the cause class of OPS-6. Move the range before production.


#### OPS-8 Cluster coordination limits — P1 (new 2026-10-21)
- **Now:** the nodes coordinate through kine (decision D-8, `PLAN.md` §0.10.8b).
- **Limits found:**
  - kine v0.13.17 is not etcd: no lease KeepAlive (keys stay alive by rewrite), no Election or Lock, a racy plain Put, no `keys_only` reads, and a delete that cannot say whether the key existed. `ad_cluster` uses only the supported transactions, and the tests run against kine itself.
  - One kine replica is a single point of failure. Several kine replicas can share one Postgres, but a write through one replica reaches the watchers of another up to 1 s later.
  - kine adds a row per key write (node and leader rewrites every third of the TTL) to its `kine` table in the app database. Compaction is on (`--compact-interval`), but the table load on a managed Postgres is not measured yet.
  - The leader paces all call starts (`AD_CALLS_PER_SEC`); there is no rate per campaign or per trunk (DIAL-7), The DIAL-9 window (built 2026-10-24) limits the claims by the agents' join time. Its controller runs in the leader, and its state is in Postgres.
  - With agent first, a node finds the agent in the room by asking LiveKit every 250 ms (`AGENT_POLL`) until it joins. At high call rates a room event would cost less. With customer first, the node joins the room at the answer, and its own participant sees the agent join, with no poll.
  - A node carries the media of its calls (D-9), so a node that dies drops its live calls. They end without a second dial, but the consumer hears the call drop. Kamailio's dialog module could send BYE to the carrier for them.
  - Measured in the cluster e2e (2026-10-21, 8 cores for the whole stack): a node used about half a core with two calls, and about the same with four. Most of that cost does not grow with the calls; profile it before a capacity plan. The Python test agents, one process per runtime, ran out of CPU above about six calls at once. On 2026-10-24 the same limit made the agents of a burst join late (DIAL-9).
  - A failover takes up to the TTL plus a third of it (default 10 s + 3.3 s) before the attempts of a dead node move; its connected calls dropped with it.
- **Do:** run kine with two or more replicas against the managed Postgres and measure the failover; measure the kine table load; replace the readiness poll with a push signal on the node that owns the call (no webhook).

#### OPS-9 The roles trusted the OS clock — P1 (fixed 2026-10-24)
- **Was:** each role took the time from the OS clock of its node (`SystemClock`). Two nodes compared times that came from two clocks: the takeover of an agent start, the reconcile of a live call (`created_at` is the Postgres `now()`), and the claim's due time against `now()` in SQL. A node with a wrong clock decided the safe hours from a wrong local time. Inside one call, a step of the wall clock changed the measured join (see DIAL-9: 781 ms read as 207 ms).
- **Now (decision D-11):** the roles with Postgres take the time from `ad_db::PgClock`: the Postgres time (`clock_timestamp()`, the read with the shortest round trip of three, at most 500 ms) plus the monotonic time since that read. A role waits for its first read before it does work, and reads again every 30 s. Each call takes one read at its start and then only the monotonic clock (`CallTimes` in the node). The API without Postgres keeps the OS clock (no other node shares its data).
- **Still on the OS clock:** the log lines and the LiveKit tokens (`livekit-api` dates the `nbf` and `exp` claims with `SystemTime`). LiveKit refuses a token from a node whose clock is ahead by more than its leeway, so that node's calls fail; they do not dial wrong. A node logs `the OS clock is off` when its OS clock is more than 250 ms from Postgres.
- **Proven:** `ad_db` unit tests (the anchor arithmetic, the read choice) and `tests/pg_clock.rs` (the clock tells the Postgres time; a sync corrects a clock that is 1 h off); the node's `CallTimes` test (a step of the node clock after the first read changes no time of the call); `e2e/run_cluster_e2e.sh` assert (h): node-2 and node-3 run with an OS clock 3 min and 11 min behind (an `LD_PRELOAD` shim that moves only the wall clock, `e2e/clock-offset`; libfaketime did the same, but it made a dial take 0.7–7 s instead of 0.1–0.6 s, and the slow nodes abandoned calls), each logs its offset, and every saved event time is within 30 s before and 5 s after the time at which Postgres saved it.
- **Found on the WSL2 dev machine (2026-10-24):** the wall clock steps back about 0.6 s every 30 s against the monotonic clock (a probe in a plain container: -603 ms at 20 s, -1189 ms at 50 s, -1796 ms at 80 s). Postgres uses that wall clock. So a role's clock runs up to about one step ahead of `now()` until its next read, and the times of one call keep the drift of the call. Before this change, the e2e checks that compared a node time (`ended_at`, `agent_ready_at`) with `created_at` were exact only because every container read the same kernel clock. They now compare only Postgres times (the save times of the events, `e2e/lib.sh` `saved_end` and `left_window`). The same machine once paused for about 3 min during a run (node-3's clock stepped +173 s at its next read).
- **Mutation checks:** a `PgClock` that tells the OS time fails `a_sync_corrects_a_clock_that_is_one_hour_off` (the other `pg_clock` test passes, because the OS clock and Postgres share one kernel clock in Docker); calls that take their times from the OS clock fail assert (h) of `e2e/run_cluster_e2e.sh` (the events of node-2 and node-3 show -180 s and -660 s), and no other assert.
- **Do:** date the LiveKit tokens with the role's clock (a patch to `livekit-api`, or our own token signer), so that the OS clock dates only the logs.

#### CARRIER-1 Twilio Elastic SIP Trunking, a supported carrier — P1 (new 2026-10-21; it was the primary carrier until 2026-10-24)
- **Now:** README "Call through Twilio" uses the quickstart stack (`docker-compose.quickstart.yml`) with `e2e/twilio/run.sh setup`. `crates/ad_sip/tests/twilio.rs` and `e2e/run_twilio_e2e.sh --sim` (with the diago carrier simulator) guard it on every change. Twilio reaches real phones; LiveKit Cloud SIP (CARRIER-3) does not.
- **Not yet proven against Twilio itself:** the live `e2e/run_twilio_e2e.sh` stops at setup. The account is a Trial account, and Twilio's trunks API refuses it ("HTTP 401: This feature is not available on a Trial account", again on 2026-10-22). Twilio's documents do not agree: the Help Center allows one trunk on Trial, and the SIP Trunking limits page requires an upgrade. Twilio gave the same 401 to a Main API key and to the account SID with the auth token, so the account causes it, not the key. Not tested: the Console. The account also has no number and no verified caller ID. On Trial, Twilio calls verified destinations only.
- **Found by the carrier simulator (2026-10-21), fixed:** (1) Kamailio's Via had no `rport`. Behind a NAT that changes the source port (the AWS NAT gateway), a carrier that follows RFC 3261 sent its 407 to a port with no NAT mapping, and the call timed out. Kamailio now adds `rport` (`add_local_rport()`). (2) rsipstack sends a CANCEL to the Request-URI host, so the ring-timeout CANCEL skipped Kamailio. `ad_sip` now preloads a Route to the SIP edge, and Kamailio removes it. SIPp hid the first fault, because it replies to the source address. The SIPp tests had no ring timeout through Kamailio, so they did not find the second.
- **Behind NAT (Docker Compose):** Twilio's in-dialog requests (its BYE) go to Kamailio's Record-Route address, which is private; the node's media timeout then ends the call. Give Kamailio a public address (a second listen socket with `advertise`) for production.
- **Do:** upgrade the account and buy one number, then run `e2e/run_twilio_e2e.sh`; add the repository secrets so CI runs it; add a per-campaign caller ID.

#### CARRIER-3 LiveKit Cloud SIP, the primary target — done (2026-10-22; primary since 2026-10-24)
- **Now:** README "Quickstart: place calls through LiveKit Cloud SIP" is the supported quickstart, and it must never break. `e2e/run_lkcloud_e2e.sh` runs it live (the tool is `e2e/livekit-cloud/run.sh`); it passed against a real LiveKit Cloud project:
  - Our node called the project's LiveKit number (+12402319284) at its SIP URI over the internet, through Kamailio, Docker's NAT, and a home NAT.
  - LiveKit challenged with 407 (realm `LiveKit`). Our digest passed, and LiveKit matched our inbound trunk and the number.
  - Each agent heard the other's tone, and our BYE ended the call.
  - LiveKit's own SIP client called the same number through a LiveKit outbound trunk, with the same results.
- **Learned:**
  - LiveKit Cloud latches RTP (it sends to the source of our RTP, not to the private address in our SDP).
  - A LiveKit number routes to a dispatch rule as its id (`PN_...`) in the rule's `trunk_ids`. A rule with no trunks cannot take a number.
  - LiveKit numbers are inbound only, so the outbound trunk points at the project's own SIP URI.
- **Not tested:** a call through the public phone network. No call left LiveKit. A call from a phone to the number uses the `PN_...` route of the same rule.
- **Do:** add the repository secrets so CI runs it (`lkcloud-e2e`).

#### CARRIER-2 Twilio Programmable Voice paths — click-to-call built (2026-10-24); SIP Domain outbound P2
Elastic SIP Trunking (CARRIER-1) is the trunk path. Programmable Voice gives two more SIP paths. On 2026-10-24, the SIP Domains API answered for the Trial account (no 401), where the trunking API refuses it. But no Twilio SIP path works on a Trial account: Twilio's "Custom TwiML during trial" (docs, Try out Voice) blocks `<Dial><Sip>` and `<Dial><Number>`, and a Trial Calls API request takes only `To`, a Twilio template URL, and `StatusCallback`. Every path below needs an upgraded account.
- **`<Dial><Sip>`, inbound to us ("click-to-call"): built, proven with the simulator only.** `AD_DIAL_PROVIDER=twilio_click_to_call` (`ad_gateway::click_to_call`, a variant of the `Telephony` registry):
  - The node asks the Calls API to call the lead, with inline TwiML `<Dial><Sip>{AD_TWILIO_SIP_URI}?x-ad-call=<token>&x-ad-node=<node></Sip></Dial>`. No webhook: the node reads the call's status (busy, no answer, failed) until the INVITE arrives, and it cancels the call at the ring timeout or when the dial task stops (RAII).
  - Kamailio routes an INVITE with `x-ad-call` to the node in `x-ad-node`, only inside the cluster network (`route[INBOUND]`), over UDP or TCP.
  - The node answers only an INVITE with the token of one of its dials (`ad_sip::SipUa::expect`, 128 random bits); any other INVITE gets 404, and an INVITE without the header gets 403.
  - Proven: `e2e/run_twilio_c2c_e2e.sh --sim` (the simulator is the Calls API and the phones; the INVITE comes over TCP; both BYE directions; busy and no answer) and `crates/ad_sip/tests/inbound.rs`.
  - **Not tested against Twilio:** the live mode of the same script needs an upgraded Twilio account. On 2026-10-24 the ngrok TCP tunnel came up (after a card was added to the ngrok account), and Twilio refused the Trial account's request: "Invalid or disallowed parameters provided - trial accounts have limited parameter access" (code 0, HTTP 400). Twilio's magic numbers (`+15005550006`) work only with test credentials, which place no call, so no INVITE comes from them. Unknown until a live run: whether Twilio sends the RTP of `<Dial><Sip>` to the address that our RTP comes from (we are behind NAT; ngrok carries no UDP).
  - **Do:** the live run; Kamailio's public address on EKS (an NLB, `advertise` on the carrier side); store the Twilio call SID on the attempt so that the reconcile can end a call whose node died; answering-machine detection (`MachineDetection`); a per-campaign caller ID.
- **SIP Domain, outbound (needs an upgraded account; a Trial account blocks `<Dial><Number>`):** the node sends its INVITE to `<name>.sip.twilio.com` with digest credentials, as it does to a trunk. Twilio then runs the domain's TwiML, for example `<Dial><Number>` to the phone, and the phone talks to our agent. It needs no public address, because we start the call. Do: a reusable `setup-domain` command in `e2e/twilio/run.sh` and a `--domain` mode of `e2e/run_twilio_e2e.sh`.
---

## 6. Where AgentDialer can be better than the references

The references solve these problems for humans, for one server, or with
cron. A fresh Rust dialer for agents can do better.

#### BETTER-1 Zero abandonment by construction, with proof
- **Weakness in references:** VICIdial over-dials and then limits drops with statistics. We did not find an abandonment model in dograh.
- **Build:** Reserve an agent before the dial. Start the agent session before the INVITE, as the LiveKit sample does. Record `answered_at` and `agent_first_audio_at` for every call. The abandonment rate is then measured, not estimated.
- **Now (2026-10-24):** this is `"dial_order": "agent_first"`. The default is `customer_first` (the user's choice: an agent starts only for an answered call). Customer first keeps the cap by its 2 s limit and by the fall back to agent first above the cap (COMP-10). It is not zero by construction. The join-paced window (DIAL-9, built 2026-10-24) limits a burst to `K` agent starts.

#### BETTER-2 A decision record for every dial
- **Weakness:** We did not find a reference that keeps evidence of why a call was allowed.
- **Build:** Before each INVITE, write an immutable record of the inputs and the result: lead zone and its source, window version, DNC list versions and scrub dates, consent record id, frequency counts, rule versions, decision. This is evidence for a legal defense, and it makes every compliance bug reproducible from data.

#### BETTER-3 An eligibility index instead of a hopper
- **Weakness:** VICIdial fills a staging table from cron every minute. dograh keeps a queue table plus in-memory locks.
- **Build:** Store `next_eligible_at` on each lead number. It is the latest of: retry due, recycle delay, callback time, next opening of the lead's calling window, and frequency-limit expiry. The claim is `WHERE campaign = $1 AND next_eligible_at <= now() ORDER BY priority ... FOR UPDATE SKIP LOCKED`. The pure engine re-gates each claimed lead. Retries, recycles, callbacks, and window openings become one mechanism. No cron, no Redis timers.
- **Status (2026-10-21), partial:** built for retries and window holds: `leads.next_eligible_at` and a claim with `SKIP LOCKED`, and the pure engine re-gates each claimed lead. Open: callbacks, recycle, frequency limits, and the next window opening.

#### BETTER-4 One durable attempt state machine with an outbox
- **Weakness:** dograh relies on in-memory locks and `replicas: 1`. VICIdial spreads state over MyISAM tables and many daemons.
- **Build:** An attempt row with typed states and idempotent transitions keyed by attempt id. Each transition writes its event to an outbox in the same transaction. A relay publishes the outbox. Handlers are idempotent against Postgres.
- **Status (2026-10-21), partial:** built for the customer leg: the `ad_attempt` statechart, the `call_attempts` snapshot and the `attempt_events` log in one transaction, and `attempt_ended` in the same transaction. Open: the agent region (decision D-2) and a leader epoch on writes (ORCH-5).

#### BETTER-5 A typed agent-to-dialer protocol
- **Weakness:** VICIdial relies on a human click. dograh extracts the disposition with an LLM after the call.
- **Build:** A versioned schema that the agent fills during the call (answer class, disposition from the campaign catalog, callback, opt-out, transfer). The dialer validates it. An agent SDK helper makes the correct use the easy use.

#### BETTER-6 Pacing from the agents' join time
- **Weakness:** VICIdial tunes its pace to the idle humans and the drop rate. dograh uses a fixed rate and a cap from an offline load test, and it ignores carriers. LiveKit keeps its worker load inside its dispatch. No reference measures how fast an agent joins, because their agents are connected before the call.
- **Build:** Learn the capacity of each agent pool from the join time of each call, and claim calls only inside that window (DIAL-9). Then use the smallest of these limits: the window of the agent pool, the campaign cap (built, DIAL-2), the org quota (API-1), the trunk concurrency and CPS (DIAL-7), `AD_CALLS_PER_SEC`, and the abandonment governor (COMP-10).
- **Status (2026-10-24):** the window of the agent pool (DIAL-9) is built, with the campaign cap, `AD_CALLS_PER_SEC`, and the governor. Open: the org quota (API-1) and the trunk limits (DIAL-7).

#### BETTER-7 SIP intelligence
- **Weakness:** dograh maps a small set of normalized statuses (completed, failed, busy, no-answer, canceled, error). VICIdial maps a fixed status list.
- **Build:** Map every SIP final code (and Q.850 cause when present) to a typed class with a default policy. For example: 486 retries later, 404 and 484 suppress the number, 503 feeds the trunk breaker, 603 lowers the priority. Operators can override the policy per campaign.
- **Status (2026-10-21), partial:** `ad_attempt::classify_sip` maps the common final codes to classes with a default policy: busy and no answer retry, an invalid number is done, a decline is dispositioned, and other codes are failures. Open: Q.850 causes, per-campaign overrides, and a trunk breaker for 503.

#### BETTER-8 Deterministic simulation tests
- **Weakness:** We did not find a reference that can test a full campaign without a real switch.
- **Build:** Every engine already takes a `Clock`. Add a campaign simulator with a virtual clock, a fake carrier with answer, busy, and no-answer rates, and fake agents. Assert invariants over thousands of runs: no call outside the window, no cap overrun, abandonment under the cap, retries at the right time, correct behavior across DST changes.

#### BETTER-9 Correct time handling
- **Weakness:** VICIdial uses fixed offsets in quarter hours, DST range codes, and a country fallback that fails open.
- **Build:** Use IANA zones, store the derivation source, fail closed, and test DST gaps and overlaps explicitly.

#### BETTER-10 Routing rules with explain and versions
- **Weakness:** FusionPBX renders XML for FreeSWITCH and has no dry run.
- **Build:** Typed rules with exact FreeSWITCH semantics (if D-1 says so), stored with versions, with a dry-run endpoint that returns the evaluation trace.

#### BETTER-11 Our own media bridge
- **Weakness:** LiveKit's own SIP gateway puts call control in a service we do not control.
- **Build:** drachtio for SIP, rtpengine for RTP, and our Rust bridge into a pure WebRTC room. Call control, compliance, and media stay in one product.

#### BETTER-12 Experiments as a first-class feature
- **Build:** QuickVoice's experiment model plus dograh's deterministic split: variants, hashed assignment, goals, conversions, attribution, and per-variant reports.

---

## 7. Recommended order of work

Each step is one vertical slice: logic, API, and tests together, green in the
Docker test tier.

Steps 3 and 4 run before step 2 (agreed 2026-10-20). The compliance data sits
on the lead and attempt tables that steps 3 and 4 change, and nothing dials
real numbers yet. Do not dial real numbers before step 2 is done.

1. **Make the existing path honest and runnable.** OPS-1, OPS-2, OPS-3, API-3. Correct the README status.
2. **Make dialing legal.** COMP-1, COMP-2, COMP-6, COMP-7, COMP-9, BETTER-2.
3. **Make the attempt real.** DIAL-1, DIAL-2, DIAL-3, DIAL-4, AGNT-1, AGNT-2, BETTER-4. The gateway stops returning "answered". Capacity is held for the call.
   **Progress (2026-10-21):** DIAL-2, DIAL-3, and DIAL-4 are done. DIAL-1 and BETTER-4 are partial. AGNT-1 and AGNT-2 are open: the agent adapters (D-2) and the session stream (D-6).
   **Progress (2026-10-24):** AGNT-1 is done, with both dial orders. DIAL-9 is done (item 7 open), and `e2e/run_cluster_e2e.sh` passes with it.
4. **Make scheduling durable.** LEAD-1, LEAD-3, ORCH-1, ORCH-2, ORCH-4, ORCH-5, BETTER-3. Postgres holds all state; there is no Redis (D-3). LEAD-2 is done.
   **Progress (2026-10-21):** LEAD-1, LEAD-3, ORCH-1, and ORCH-4 are done. ORCH-2 and BETTER-3 are partial. ORCH-5 is open.
5. **Secure the API.** API-1, API-2.
6. **Complete the campaign brain.** LEAD-4 to LEAD-10, COMP-3, COMP-4, COMP-5, COMP-8, COMP-10, DIAL-5 to DIAL-8, AGNT-4, AGNT-5.
7. **Wire the routing engine.** D-1, then ROUT-1 to ROUT-4.
8. **Own the media leg.** MEDIA-2, MEDIA-3, MEDIA-4, MEDIA-5, then MEDIA-1.
9. **Close parity and differentiate.** OBS-1 to OBS-5, AGNT-6, API-4, API-5, BETTER-6 (after DIAL-9 and DIAL-7), BETTER-7, BETTER-8.

---

## 8. Decisions and open questions

| ID | Question | Recommendation |
|---|---|---|
| D-1 | Match FreeSWITCH dialplan semantics exactly, or keep the group model and correct `PLAN.md`? | Match FreeSWITCH. Operators will port FusionPBX rules, and `PLAN.md` §0.10.13 tests against the FusionPBX examples. |
| D-2 | Does the dialer or the agent originate the call? | The dialer. It owns the compliance gate at the moment of the dial and the attempt state. The agent never dials. **Decided 2026-10-20:** agents join through two adapters, one for LiveKit (a room plus agent dispatch) and one for pipecat (a bot per call). A statechart per attempt drives both. Its state is saved in Postgres (an event log plus a snapshot) and restored after a restart. **Built 2026-10-21:** both regions of the statechart (`ad_attempt`) and both adapters (`ad_agent`), proven with a real LiveKit agent and a real pipecat bot (`PLAN.md` §0.10.7a, §0.10.8b). |
| D-3 | What is the role of Redis? | **Decided and built 2026-10-20:** none. Postgres is the event layer: an event table with `LISTEN/NOTIFY` and a sweep, retries on the lead row, and the breaker window over `call_outcomes` (`PLAN.md` §0.10.8a). Redis is removed from the product. The media e2e stacks keep Redis only for LiveKit. |
| D-4 | `apalis` or `pgmq` for the durable queue (`PLAN.md` §0.8)? | Neither. Use the eligibility index (BETTER-3). |
| D-5 | Legal: AI-voice consent level, the 2-second rule for AI agents, the state laws in scope, recording disclosures, UK and India rules. | Get a counsel review before we claim compliance. Keep jurisdictions `experimental` until then. |
| D-6 | Outcome transport: signed webhook or LiveKit participant attributes and data? | **Decided 2026-10-20:** neither. Each agent session opens one gRPC stream to the dialer. It carries the outcome, the session events, and the logs. OpenTelemetry (OTLP) uses the same gRPC endpoint, so there is no second connection. Open: OTLP as its own service on that endpoint (recommended), or OTLP messages inside the session stream. Not built yet. |
| D-7 | When do we remove `livekit/sip`? | **Superseded by D-9 (2026-10-21):** remove it when the node carries the call through Kamailio and passes the same e2e tests. Old answer: After the drachtio and rtpengine path passes the same e2e tests (MEDIA-1). |
| D-8 | How do the dialer nodes share the calls when Kubernetes runs several? | **Decided and built 2026-10-21:** through kine, the etcd API on Postgres. Each orchestrator replica is a node with a key in kine; one node holds the leader key and assigns each call to a node with free capacity; Postgres fences a node that lost a call (`PLAN.md` §0.10.8b). Postgres and kine are managed services (for example RDS): the product reaches them only by URL and waits for them with backoff, with no host checks. |
| D-9 | Who carries the call (SIP and RTP)? | **Decided and built 2026-10-21 (proven in Docker Compose, see MEDIA-1):** agentdialer itself, inside the node process, with vendored libraries (`rsipstack`, `rustrtc`, `audio-codec`, parts of `rustpbx`). No Asterisk, FreeSWITCH, drachtio, rtpengine, or `livekit/sip` under it. Kamailio is the SIP edge (signaling load balancer, `Record-Route`). RTP goes straight between the carrier and the node, one socket per call, in `latching` or `public` (Elastic IP per pod) mode. No host networking (`PLAN.md` §0.0). Supersedes D-7. |
| D-10 | How does the dialer pace customer-first calls, so that the agents join in time? | **Decided 2026-10-24:** a join-paced window for each agent pool, learned with AIMD from the join time of each call (DIAL-9). Both modes exist, as a setting of each campaign: `adaptive` (the default) and `strict`. Every limit value can change at run time through the API, with no restart: the target, the back-off, the first `K`, the largest `K`, and the mode. The defaults come from the 2 s rule: a target of 1.0 s and a back-off of 1.5 s. Built 2026-10-24 (DIAL-9 status). |
| D-11 | Do we run a time server (chrony, ntpd) for the dialer? | **Decided and built 2026-10-24:** no. A pod cannot set the clock without `CAP_SYS_TIME`, and a change moves the clock of the whole node. On EKS the node keeps its clock (chrony, Amazon Time Sync), and the managed Postgres keeps its own. The roles take the time from Postgres plus the monotonic clock and do not depend on the OS clock (OPS-9). |

---

## Appendix A. Reference map

| Source | Path in `_research/` | Read for |
|---|---|---|
| dograh | `dograh/api/services/campaign/` (`campaign_orchestrator.py`, `campaign_call_dispatcher.py`, `circuit_breaker.py`, `traffic_split.py`), `dograh/api/db/campaign_client.py`, `dograh/api/services/telephony/status_processor.py`, `dograh/api/services/call_concurrency/`, `dograh/api/services/telephony/providers/AGENTS.md` | Orchestrator, dispatch, slots, durable retries, terminal status, breaker, A/B, provider registry |
| dograh agent side | `dograh/api/services/pipecat/answer_classification.py`, `processors/answer_supervisor.py`, `dograh/api/services/workflow/answer_handling.py`, `disposition_extraction.py` | Answer classes and actions, disposition extraction |
| VICIdial | `vicidial/2026-06-15/bin/AST_VDhopper.pl`, `AST_VDauto_dial.pl`, `AST_VDadapt.pl` (`calculate_dial_level`), `www/vicidial/functions.php` (`lookup_gmt`), `extras/MySQL_AST_CREATE_tables.sql` | Hopper, call times, holidays, recycle, DNC, timezone lookup, adaptive pacing, drop rate |
| dograh pacing | `dograh/api/services/call_concurrency/rate_limiter.py`, `dograh/api/services/observability/loop_lag.py`, `active_calls.py` | Fixed rate, slots, KEDA scaling from an offline load test |
| LiveKit agents | `livekit_agents/livekit-agents/livekit/agents/worker.py` | Worker load threshold, warm processes |
| FusionPBX | `fusionpbx/app/dialplans/resources/classes/dialplan.php`, `app/dialplans/resources/switch/conf/dialplan/*.xml`, `app/time_conditions/` | Dialplan model, reference dialplans, time conditions |
| FreeSWITCH | `freeswitch/src/mod/dialplans/mod_dialplan_xml/mod_dialplan_xml.c` | Exact condition, break, anti-action, and continue semantics |
| Alexor-Brain | `Alexor-Brain/campaign_worker.py`, `services/call_service.py`, `app.py` (LiveKit webhook), `postgresql_notifier.sql` | Eligibility, orphan cleanup, agent dispatch, terminal webhook |
| LiveKit outbound caller | `outbound-caller-python/agent.py` | Agent-side dialing, SIP status, transfer, hangup |
| QuickVoice | `QuickVoice/apps/server/prisma/migrations/20260731000000_add_campaign_personalization_experiments/migration.sql`, `apps/server/src/modules/outbound/campaign-time.ts` | Experiments, attribution, DST-safe local time |
| drachtio | `drachtio-srf/lib/drachtio-agent.js`, `srf.js`, `digest-client.js` | Wire protocol, UAC flow, ACK, auth. Ignore `drachtio-server/docs/messages.md` (old JSON protocol). |
| rtpengine | `rtpengine/docs/ng_control_protocol.md`, `transcoding.md` | ng commands and flags, transcoding |
| Our spike | `spike/` (in the repo) | What the media proof covered, and what it skipped |
