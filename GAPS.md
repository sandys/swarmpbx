# AgentDialer — Gap Analysis

| | |
|---|---|
| Status | Assessment. Not a plan of record. `PLAN.md` stays authoritative. |
| Date | 2026-10-20 |
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
| Predictive pacing to keep humans busy (`AST_VDadapt.pl`) | Not needed as the primary mode | Agent capacity is elastic. We dial against reserved capacity. |
| AMD in the switch | Moves to the agent | The agent hears the answer and classifies it. |
| IVR, call menus, prerecorded broadcast | Moves to the agent | The agent speaks and listens. |
| Abandonment control | Keep, and make it provable | The rules still apply. Elastic agents make it near zero by construction. |
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
- The orchestrator is a singleton by Postgres advisory lock, not by
  `replicas: 1`.
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
| DIAL-1 | The gateway reports "answered" when LiveKit accepts the SIP participant. |
| DIAL-2 | Capacity is not held for the length of the call. |
| DIAL-3 | The orchestrator dials one call at a time and waits for each answer. |
| DIAL-4 | A timeout after the INVITE is sent can cause a second dial. |
| LEAD-1 | Nothing fills the hopper. Leads loaded through the API never dial. |
| LEAD-2 | Retries live only in Redis. |
| LEAD-3 | Claims never release. A dispositioned lead can re-dial at once. |
| ORCH-1 | Campaign state never becomes paused or completed in Postgres. |
| AGNT-1 | No agent is dispatched into the per-lead rooms. |
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
| Retries by reason | Partial | Yes | Redis only | LEAD-2 |
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
| Abandonment ledger | Yes | Breaker only | Engine only | COMP-10 |
| Capacity-bound pacing | Human-based | Yes (slots) | Missing in production | DIAL-2, DIAL-3 |
| Dial-rate limit (CPS) | — | Yes | One global gap | DIAL-3 |
| Caller-ID pools | Yes | Yes | Missing | DIAL-6 |
| Trunk selection and failover | Partial | Yes | Missing | DIAL-7 |
| Circuit breaker | — | Yes | Partial | DIAL-8 |
| Voicemail / machine policy | Asterisk AMD | Answer supervisor | Missing | AGNT-5 |
| Agent dispatch per call | n/a | Pipeline per run | Missing | AGNT-1 |
| Agent outcome contract | Human UI | Status + LLM disposition | Missing | AGNT-2 |
| Transfer | Yes | Yes | Missing | AGNT-4 |
| A/B agent split | — | Yes | Missing | AGNT-6 |
| Routing rule engine | Asterisk dialplan | — | Engine, not wired | ROUT-1..4 |
| Recordings | Yes | Yes | Missing | OBS-3 |
| Reports | Yes | Yes | Missing | OBS-2 |
| Management API | Non-agent API | REST | Partial | API-2 |
| Auth and tenancy | User groups | Org scoping | Missing | API-1 |
| Outbound webhooks | — | Yes | Missing | API-4 |
| Own SIP↔WebRTC bridge | n/a | n/a | Proven, not used | MEDIA-1..5 |
| Singleton leader | One cron server | `replicas: 1` | Built, no fencing | ORCH-5 |

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

#### COMP-10 The abandonment ledger has no source — P1
- **Now:** `ad_compliance::abandon::AbandonLedger` is in memory, and nothing records into it. `UkOfcom` uses the US formula.
- **Reference:** VICIdial counts drops (DROP, XDROP) over human-answered statuses. Its UK mode removes the estimated answering-machine share of drops (`bin/AST_VDadapt.pl` ~line 3946). `drop_call_seconds` defines a drop.
- **Do:** Persist an answer event per attempt with `human_answered`, `answered_at`, and `agent_first_audio_at`. Compute the rate per jurisdiction. Feed the pacing governor and the reports.
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

#### DIAL-2 Capacity is not held for the call — P0
- **Now:** The orchestrator skips `ad_dispatch::Dispatcher`. `Dispatcher` releases its slot when `initiate` returns. `ad_db::leader::in_call_count` is never called.
- **Reference:** dograh binds a slot to the run and releases it on the terminal status (`call_concurrency/service.py`, 20-minute stale cutoff, org cap and campaign cap). VICIdial dials `goal − live calls` (`bin/AST_VDauto_dial.pl` lines 1213, 1434). Alexor-Brain waits while live calls ≥ cap.
- **Do:** Make a live attempt the slot. Count attempts in dialing, ringing, or connected state per campaign and per org. Acquire before the dial. Release on the terminal transition. Recover stale attempts.

#### DIAL-3 Serial dialing — P0
- **Now:** `dispatch_batch` awaits each `initiate` with `wait_until_answered: true`, then sleeps `per_call_gap`. One unanswered call blocks the batch for the full ring time.
- **Reference:** dograh runs up to 10 setups in parallel with a 240-second batch deadline and a per-campaign token bucket (`campaign_call_dispatcher.py`). Alexor-Brain dials with `wait_until_answered=False`.
- **Do:** Launch attempts at the same time, bounded by free capacity. Pace launches with a token bucket per campaign and per trunk. Never block the scheduler on ring time.
- **Measured (SIPp benchmark, 2026-10-20, all in Docker):** the dial rate does not follow `AD_CALLS_PER_SEC`. With a 300 ms ring, 30 calls at a configured 10/s ran at 1.29 attempts/s, and 100 calls at 50/s ran at 1.08 attempts/s. With a 3 s ring the rate fell to 0.34/s: each call waits for its answer. Answered setup took 328–386 ms (p50 333 ms). Each failed setup stalled the loop for 5.7–8.1 s. Peak live calls: 11. Harness: `_research/scratch/sip-bench/`.

#### DIAL-4 An uncertain initiation can dial twice — P0
- **Now:** Every error becomes `Failed`, and `Failed` schedules a retry.
- **Reference:** dograh `_finish_interrupted_dispatch` marks a timeout or a lost connection after the request was sent as "uncertain". It never requeues the row, keeps the slot, and reconciles later.
- **Do:** Classify errors as "rejected" (a SIP final response arrived) or "uncertain" (no response). For an uncertain attempt, hold the slot and reconcile from the provider before any retry.

#### DIAL-5 No call timers — P1
- **Reference:** VICIdial `dial_timeout` (default 60 seconds).
- **Do:** Add a ring timeout, a maximum call duration, and a silence timeout per campaign.

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
- **Also:** When the orchestrator cannot read the breaker (a Redis error), it treats the breaker as closed and keeps dialing. Since 2026-10-20 it logs this as a warning. Decide whether this path must fail closed.
- **Seen in the SIPp benchmark (2026-10-20):** 19 calls that never sent an INVITE (a local media error) counted as call failures and tripped the breaker. Our own misconfiguration paused the campaign. BETTER-7 separates these classes.

### 5.3 Lead lifecycle and selection (LEAD)

#### LEAD-1 Nothing fills the hopper — P0
- **Now:** Only tests call `PgHopperRepo::insert_leads`. The e2e scripts seed the hopper with SQL. Leads from `POST /v1/campaigns/:id/leads` never dial.
- **Reference:** VICIdial fills `vicidial_hopper` every minute from cron (`bin/AST_VDhopper.pl`). dograh syncs the source into `queued_runs`.
- **Do:** Replace the fill step with an eligibility index (BETTER-3). Each lead carries `next_eligible_at`, and the claim reads it directly.

#### LEAD-2 Retries are not durable — P0
- **Now:** Retries live only in the Redis sorted set `ad:timers:{campaign}`. A Redis loss leaves leads in `queued` with a claimed hopper row. This breaks the rule in `PLAN.md` §0.10.8a.
- **Reference:** dograh writes a new `queued_runs` row with `scheduled_for`, `retry_count`, and `parent_queued_run_id`. It persists the decision before the run turns terminal (`db/campaign_client.py` `record_campaign_retry_decision`).
- **Do:** Store the retry due time in Postgres. Redis may wake the orchestrator, but it must never hold the only copy.

#### LEAD-3 Claims never release — P0
- **Now:** `hopper.claimed_by` stays set after the outcome. `in_call_count` would grow without limit. `ad_hopper` treats `Dispositioned` as dialable at once, so the hopper preview lists a lead as eligible again right after its call. No rule controls when it may dial again.
- **Reference:** VICIdial deletes the hopper row at dial (`bin/AST_VDauto_dial.pl` line 2084) and uses `called_since_last_reset` with recycle rules. dograh moves rows through queued, processing, processed, failed, and recovers claims after 360 seconds.
- **Do:** Give each attempt explicit states. Release the claim at the terminal state. Give each claim a lease with expiry and recovery. Make a dispositioned lead eligible only through a recycle, retry, or callback rule.

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

#### ORCH-2 Events can be lost — P1
- **Now:** Events are published after the commit, and errors are ignored (`.ok()`). Redis pub/sub delivers at most once. Reconciliation runs only on reconnect.
- **Reference:** dograh runs a 60-second sweep that recovers claims and orphaned work. Alexor-Brain polls every 180 seconds as a backstop.
- **Do:** Write events to an outbox table in the same transaction and relay them. Run a periodic sweep for stale claims, stuck attempts, orphaned work, and completion.

#### ORCH-3 Two orchestration models — P1
- **Now:** `run_loop.rs` and `gate()` are used only by tests and the API. The binary uses the reactor. `Schedule` fails open, but `PLAN.md` §7.1 says the schedule window fails closed. Production never applies a business schedule.
- **Do:** Keep one model. Apply the business schedule in production. Decide its failure mode in `PLAN.md`; the compliance gate stays fail-closed in all cases.

#### ORCH-4 A finished call does not trigger dialing — P1
- **Now:** The reactor treats `call_completed` as no-op. Only `batch_completed` starts the next batch.
- **Reference:** dograh schedules the next batch on `CallCompletedEvent`.
- **Do:** Treat freed capacity as a trigger for new launches.

#### ORCH-5 No leader fencing — P1
- **Now:** The leader holds the advisory lock on one pooled connection and never checks it. If that connection drops, Postgres releases the lock, a standby takes it, and the old leader keeps dialing.
- **Do:** Check the lock connection on a short interval and stop dialing at once when it fails. Add a fencing token (leader epoch) to the attempt writes so a stale leader cannot commit.

### 5.6 Agent integration (AGNT)

#### AGNT-1 No agent dispatch — P0
- **Now:** Rooms are `{prefix}-{lead_id}`. `e2e/run_e2e.sh` passes because a test agent waits in `campaign-1` and the test lead has id 1. In production no agent joins these rooms.
- **Reference:** Alexor-Brain `services/call_service.py` calls `create_dispatch(agent_name, metadata)` and then `CreateSIPParticipant` in room `call-{call_id}`. The LiveKit sample starts the agent session before it dials.
- **Do:** For each attempt, create room `attempt-{id}`, dispatch the named agent with metadata (lead context, campaign, attempt id, variant, disclosures), and confirm the agent joined. Then originate the call.

#### AGNT-2 No outcome contract — P0
- **Reference:** dograh answer actions (`START_OPENING`, `RELEASE`, `LEAVE_MESSAGE`, `DROP`, `SCREEN_THEN_REARM`, `WAIT_FOR_SCREENING`), machine subtypes, and post-call disposition extraction. The LiveKit sample tools `end_call`, `transfer_call`, `detected_answering_machine`.
- **Do:** Define a versioned outcome schema: answer class, disposition code, callback time and zone, DNC request and scope, transfer request, summary, disclosures spoken. Carry it in a signed webhook or LiveKit participant attributes (decision D-6). Make it idempotent by attempt id. Validate it against the campaign catalog.

#### AGNT-3 No agent-capacity signal — P1
- **Do:** Use agent capacity as a pacing input: worker availability from LiveKit or a configured pool cap. Fail fast when no worker accepts a dispatch.

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

#### MEDIA-1 Production still depends on `livekit/sip` — P1
- **Now:** The orchestrator dials through `CreateSIPParticipant`. `PLAN.md` §0.9a says we own the SIP↔WebRTC boundary.
- **Do:** Add a `CallInitiator` over drachtio, rtpengine, and our bridge. Keep `LiveKitGateway` only as a fallback until the new path passes the same e2e tests.

#### MEDIA-2 drachtio client is a spike — P1
- **Now:** `spike/ad_sip_ctl` authenticates and sends a raw INVITE. It never sends ACK, BYE, or CANCEL. It does not handle provisional responses, digest auth, keepalive, reconnect, or CDR events.
- **Reference:** `drachtio-srf/lib/drachtio-agent.js` (frame tokens, ACK with the dialog id, PRACK, `route|invite`, `cdr:*`) and `srf.js` `createUAC` (1xx, 3xx, final response, digest via `digest-client.js`, missing Contact).
- **Do:** Build `ad_sip_ctl` with these flows and conformance tests against a real drachtio container.

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
- **Status (2026-10-20): fixed as a class.** `crates/ad_bin/src/config.rs` defines the variables of each role. Each role reports every missing, blank, or invalid variable at startup. The chart (`redis.url` / `redis.existingSecret`), `run_e2e.sh`, and `docker-compose.yml` now provide `REDIS_URL`. Tests in `ad_bin` fail when the Helm chart, an e2e script, or the README "Configuration" table drifts from the contract.

#### OPS-2 Chart drift — P1
- **Now:** The gateway Deployment runs the `dispatch` role, which prints a message and exits. The chart sets `AD_TICK_MS` and `AD_MAX_CONCURRENT`, which nothing reads. The chart has no Redis.
- **Do:** Align the chart with the binary. Add a test that renders the chart and checks every env var against what the binary reads.
- **Status (2026-10-20), partial:** The orchestrator, API, and migrate templates now match the contract, and the drift test covers them. The chart wires the LiveKit key and secret in both modes. The gateway Deployment still runs the stub `dispatch` role.

#### OPS-3 Two e2e suites are not in CI — P1
- **Now:** The CI workflow runs the test tier and the WebRTC e2e. `run_e2e.sh` and `run_events_e2e.sh` are manual, which is how OPS-1 went unseen.
- **Do:** Run all e2e suites in CI.

#### OPS-4 The e2e LiveKit SIP config allowed one live call — P1 (fixed)
- **Found by:** the SIPp benchmark (2026-10-20). 19 of 20 calls failed with `media failed: failed to listen on udp port` while the first call held its port.
- **Cause:** `spike/livekit/sip.yaml` set `rtp_port: 20000` and `rtp_port_range_end: 20050`. livekit/sip reads `rtp_port` as a range `start-end` and has no `rtp_port_range_end` key, so the range was one port. `run_e2e.sh` places one call, so it never showed.
- **Status:** fixed (`rtp_port: 20000-20100`).

#### OPS-5 The e2e scripts used host tools — P1 (fixed)
- **Now:** The scripts called host `curl` and `python3` and wrote logs to `/tmp`, against the Docker-first rule.
- **Status (2026-10-20):** fixed. `e2e/lib.sh` runs HTTP polling and JSON parsing in a container. The scripts keep their logs in `e2e/.out/` (git-ignored). The host needs only bash and the Docker CLI.

#### OPS-6 About 8% of benchmark calls fail to join their room — P1 (open)
- **Found by:** the SIPp benchmark (2026-10-20): 2 of 30 and 8 of 100 calls failed before the INVITE, about every eleventh call.
- **Evidence:** livekit/sip logs "waiting for connection establishment failed: could not connect after timeout"; livekit-server logs "cannot create sip participant: update room failed: could not connect after timeout". The WebRTC join from livekit/sip to the room times out.
- **Suspected cause (not confirmed):** the e2e LiveKit server has 51 WebRTC ports (`spike/livekit/livekit.yaml`, 50000–50050), and every container shares the host network, so client sockets can take ports in that range.
- **Do:** Widen the range outside the host ephemeral range, re-run the benchmark, and confirm. Production sizing follows from the result.

---

## 6. Where AgentDialer can be better than the references

The references solve these problems for humans, for one server, or with
cron. A fresh Rust dialer for agents can do better.

#### BETTER-1 Zero abandonment by construction, with proof
- **Weakness in references:** VICIdial over-dials and then limits drops with statistics. We did not find an abandonment model in dograh.
- **Build:** Reserve an agent before the dial. Start the agent session before the INVITE, as the LiveKit sample does. Record `answered_at` and `agent_first_audio_at` for every call. The abandonment rate is then measured, not estimated.

#### BETTER-2 A decision record for every dial
- **Weakness:** We did not find a reference that keeps evidence of why a call was allowed.
- **Build:** Before each INVITE, write an immutable record of the inputs and the result: lead zone and its source, window version, DNC list versions and scrub dates, consent record id, frequency counts, rule versions, decision. This is evidence for a legal defense, and it makes every compliance bug reproducible from data.

#### BETTER-3 An eligibility index instead of a hopper
- **Weakness:** VICIdial fills a staging table from cron every minute. dograh keeps a queue table plus in-memory locks.
- **Build:** Store `next_eligible_at` on each lead number. It is the latest of: retry due, recycle delay, callback time, next opening of the lead's calling window, and frequency-limit expiry. The claim is `WHERE campaign = $1 AND next_eligible_at <= now() ORDER BY priority ... FOR UPDATE SKIP LOCKED`. The pure engine re-gates each claimed lead. Retries, recycles, callbacks, and window openings become one mechanism. No cron, no Redis timers.

#### BETTER-4 One durable attempt state machine with an outbox
- **Weakness:** dograh relies on in-memory locks and `replicas: 1`. VICIdial spreads state over MyISAM tables and many daemons.
- **Build:** An attempt row with typed states and idempotent transitions keyed by attempt id. Each transition writes its event to an outbox in the same transaction. A relay publishes the outbox. Handlers are idempotent against Postgres.

#### BETTER-5 A typed agent-to-dialer protocol
- **Weakness:** VICIdial relies on a human click. dograh extracts the disposition with an LLM after the call.
- **Build:** A versioned schema that the agent fills during the call (answer class, disposition from the campaign catalog, callback, opt-out, transfer). The dialer validates it. An agent SDK helper makes the correct use the easy use.

#### BETTER-6 Capacity-aware pacing for elastic agents
- **Weakness:** VICIdial pacing models human idle time. dograh caps concurrency but ignores carriers.
- **Build:** Compute the launch rate from agent capacity, campaign cap, org quota, trunk concurrency and CPS, and the abandonment governor. Use the smallest limit.

#### BETTER-7 SIP intelligence
- **Weakness:** dograh maps a small set of normalized statuses (completed, failed, busy, no-answer, canceled, error). VICIdial maps a fixed status list.
- **Build:** Map every SIP final code (and Q.850 cause when present) to a typed class with a default policy. For example: 486 retries later, 404 and 484 suppress the number, 503 feeds the trunk breaker, 603 lowers the priority. Operators can override the policy per campaign.

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

1. **Make the existing path honest and runnable.** OPS-1, OPS-2, OPS-3, API-3. Correct the README status.
2. **Make dialing legal.** COMP-1, COMP-2, COMP-6, COMP-7, COMP-9, BETTER-2.
3. **Make the attempt real.** DIAL-1, DIAL-2, DIAL-3, DIAL-4, AGNT-1, AGNT-2, BETTER-4. The gateway stops returning "answered". Capacity is held for the call.
4. **Make scheduling durable.** LEAD-1, LEAD-2, LEAD-3, ORCH-1, ORCH-2, ORCH-4, ORCH-5, BETTER-3. Redis holds no only-copy state.
5. **Secure the API.** API-1, API-2.
6. **Complete the campaign brain.** LEAD-4 to LEAD-10, COMP-3, COMP-4, COMP-5, COMP-8, COMP-10, DIAL-5 to DIAL-8, AGNT-4, AGNT-5.
7. **Wire the routing engine.** D-1, then ROUT-1 to ROUT-4.
8. **Own the media leg.** MEDIA-2, MEDIA-3, MEDIA-4, MEDIA-5, then MEDIA-1.
9. **Close parity and differentiate.** OBS-1 to OBS-5, AGNT-6, API-4, API-5, BETTER-6 to BETTER-8.

---

## 8. Decisions and open questions

| ID | Question | Recommendation |
|---|---|---|
| D-1 | Match FreeSWITCH dialplan semantics exactly, or keep the group model and correct `PLAN.md`? | Match FreeSWITCH. Operators will port FusionPBX rules, and `PLAN.md` §0.10.13 tests against the FusionPBX examples. |
| D-2 | Does the dialer or the agent originate the call? | The dialer. It owns the compliance gate at the moment of the dial and the attempt state. The agent never dials. |
| D-3 | What is the role of Redis? | Postgres holds all durable state with an outbox. Redis only wakes the orchestrator. Consider `LISTEN/NOTIFY` to remove the dependency. |
| D-4 | `apalis` or `pgmq` for the durable queue (`PLAN.md` §0.8)? | Neither. Use the eligibility index (BETTER-3). |
| D-5 | Legal: AI-voice consent level, the 2-second rule for AI agents, the state laws in scope, recording disclosures, UK and India rules. | Get a counsel review before we claim compliance. Keep jurisdictions `experimental` until then. |
| D-6 | Outcome transport: signed webhook or LiveKit participant attributes and data? | Both, with the webhook as the durable path. |
| D-7 | When do we remove `livekit/sip`? | After the drachtio and rtpengine path passes the same e2e tests (MEDIA-1). |

---

## Appendix A. Reference map

| Source | Path in `_research/` | Read for |
|---|---|---|
| dograh | `dograh/api/services/campaign/` (`campaign_orchestrator.py`, `campaign_call_dispatcher.py`, `circuit_breaker.py`, `traffic_split.py`), `dograh/api/db/campaign_client.py`, `dograh/api/services/telephony/status_processor.py`, `dograh/api/services/call_concurrency/`, `dograh/api/services/telephony/providers/AGENTS.md` | Orchestrator, dispatch, slots, durable retries, terminal status, breaker, A/B, provider registry |
| dograh agent side | `dograh/api/services/pipecat/answer_classification.py`, `processors/answer_supervisor.py`, `dograh/api/services/workflow/answer_handling.py`, `disposition_extraction.py` | Answer classes and actions, disposition extraction |
| VICIdial | `vicidial/2026-06-15/bin/AST_VDhopper.pl`, `AST_VDauto_dial.pl`, `AST_VDadapt.pl`, `www/vicidial/functions.php` (`lookup_gmt`), `extras/MySQL_AST_CREATE_tables.sql` | Hopper, call times, holidays, recycle, DNC, timezone lookup, pacing, drop rate |
| FusionPBX | `fusionpbx/app/dialplans/resources/classes/dialplan.php`, `app/dialplans/resources/switch/conf/dialplan/*.xml`, `app/time_conditions/` | Dialplan model, reference dialplans, time conditions |
| FreeSWITCH | `freeswitch/src/mod/dialplans/mod_dialplan_xml/mod_dialplan_xml.c` | Exact condition, break, anti-action, and continue semantics |
| Alexor-Brain | `Alexor-Brain/campaign_worker.py`, `services/call_service.py`, `app.py` (LiveKit webhook), `postgresql_notifier.sql` | Eligibility, orphan cleanup, agent dispatch, terminal webhook |
| LiveKit outbound caller | `outbound-caller-python/agent.py` | Agent-side dialing, SIP status, transfer, hangup |
| QuickVoice | `QuickVoice/apps/server/prisma/migrations/20260731000000_add_campaign_personalization_experiments/migration.sql`, `apps/server/src/modules/outbound/campaign-time.ts` | Experiments, attribution, DST-safe local time |
| drachtio | `drachtio-srf/lib/drachtio-agent.js`, `srf.js`, `digest-client.js` | Wire protocol, UAC flow, ACK, auth. Ignore `drachtio-server/docs/messages.md` (old JSON protocol). |
| rtpengine | `rtpengine/docs/ng_control_protocol.md`, `transcoding.md` | ng commands and flags, transcoding |
| Our spike | `spike/` (in the repo) | What the media proof covered, and what it skipped |
