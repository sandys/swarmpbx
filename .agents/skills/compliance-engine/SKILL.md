---
name: compliance-engine
description: >
  Teaches how to work on the AgentDialer compliance engine (crate ad_compliance):
  safe-hours (TCPA timezone windows), DNC scrubbing, and the abandonment-rate
  ledger with its jurisdiction registry. Use this skill when you change calling
  rules, timezones, DNC, abandonment math, or add a jurisdiction. The engine
  fails closed and must stay pure. This is the highest-risk product subsystem.
license: AGPL-3.0-or-later
metadata:
  project: agentdialer
  tier: project
---

# Compliance engine (`ad_compliance`)

The compliance engine decides if AgentDialer may call a lead right now. It is
the highest-risk product subsystem. A wrong decision carries legal and financial
penalties. Treat every change here with care.

Read `PLAN.md` §9 for the full design.

Warning: until COMP-1 and COMP-2 in `GAPS.md` are done, do not claim that
production dialing is compliant. The orchestrator passes a 24-hour window and
an empty DNC list (COMP-1). No table stores call times, holidays, or DNC
entries. A lead without an explicit IANA timezone never dials (COMP-2). Counsel
must also review the rules (decision D-5).

Where production calls the engine: the leader's `gate()` in
`crates/ad_bin/src/orchestrator/leader.rs`. It re-gates each claimed lead with
`ad_hopper::select`. It passes `CallWindowSet::uniform(0, 2400)` (set in
`crates/ad_bin/src/orchestrator/mod.rs`), `DncSet::default()`, and no
ruleset. COMP-1 replaces these with data from tables.

The VICIdial reference shows how a mature dialer stores this data
(`_research/vicidial/`, see `GAPS.md` Appendix A). Read it before you add the
tables.

## Two rules you must never break

1. Fail closed. If the engine cannot prove a call is permitted, it denies.
   Examples that must deny: unknown timezone, unparseable window, missing data.
   When you add a code path, the default branch denies.

2. Stay pure. A decision function takes data plus a `Clock` and returns a
   `Decision`. It does no I/O. The caller loads DNC data and passes it in. This
   keeps the engine deterministic and testable. In production the `Clock` is
   `ad_db::PgClock`: the safe hours use the Postgres time, not the OS clock
   of the node (`GAPS.md` OPS-9).

## The modules

- `safe_hours.rs` — the calling-window gate. It resolves the lead local time
  from an IANA timezone, then checks per-weekday windows and holidays. Unknown
  or unparseable timezone denies with `UnknownLocalTime`.
- `dnc.rs` — do-not-call scrubbing. Scopes: system, campaign, area code. An
  empty or unusable number denies.
- `abandon.rs` — the abandonment ledger and the jurisdiction registry.

## The abandonment rate — use the correct denominator

The regulator rate is NOT failed calls divided by total dials. It is:

```
abandon_rate = answered_by_human_but_no_agent / answered_by_human
```

Only human-answered calls are in the denominator. The ledger records
human-answered events and marks each as abandoned or not. See
`abandon.rs::AbandonLedger`.

In production, `call_attempts` is the ledger. `ad_db::PgAttemptRepo::abandon_counts`
returns `AbandonCounts` for a campaign and a window. The leader then calls the
pure `JurisdictionRule::allows_customer_first`. Keep these rules:
- No answered call yet: the rate is undefined, and customer first is allowed.
- A rate above the cap, or a ledger that the leader cannot read: dial agent
  first. Agent first cannot abandon a call, so this fails closed.
- `connect_within` (2 s) is the limit from the answer to the agent in the
  room. A customer-first call past the limit ends as `abandoned`.
- There is no answering-machine detection yet. Every answer counts as a human
  answer (`GAPS.md` COMP-10).
- The join-paced window (`ad_pacing`, `GAPS.md` DIAL-9) is not a compliance
  gate. It lowers the number of abandoned calls. The governor above stays the
  hard limit. The window's default target and back-off come from
  `connect_within`; if you change that limit, saved pool settings can become
  invalid, and their pools then dial one call at a time (fail closed).

## Add a jurisdiction (registry pattern)

A jurisdiction rule is a variant of `JurisdictionRule` plus match arms for
`cap()` and `window()`. Add one with a new variant and the arms. Do not run a
migration. Mark a rule `experimental` until you verify it against primary
sources. The `IndiaTrai` and `UkOfcom` rules are experimental now.

Warning: before you ship an India rule for production, confirm the TRAI
Feb-2025 obligations against primary TRAI sources. Do not claim compliance
without that check.

## Tests

Tests live in `crates/ad_compliance/tests/`. They cover DST, half-hour-offset
zones (for example Asia/Kolkata at +5:30), midnight-wrap windows, holidays,
closed days, DNC scope precedence, and the abandonment denominator.
`properties.rs` holds the fail-closed and always-open property tests.

When you change the engine:

1. Add or update tests first.
2. Prove the test has teeth: break the logic once, confirm the test fails, then
   revert.
3. Run the crate tests in Docker. Then run the full tier (`vertical-slice` skill):
   ```
   docker compose -f docker-compose.test.yml run --rm --build tests \
       cargo test -p ad_compliance
   docker compose -f docker-compose.test.yml down
   ```

## The API

The engine is exposed at `POST /v1/compliance/check` in
`crates/ad_api/src/compliance.rs`. `GET /v1/campaigns/:id/abandonment` shows
the ledger of a campaign and whether customer first is allowed. A malformed window returns HTTP 400. That is
also fail-closed: the API never returns `allowed: true` for input it cannot
interpret. Ship API changes with contract tests. See the `vertical-slice` skill.
