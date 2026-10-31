---
name: compliance-engine
description: >
  Teaches how to work on the AgentDialer compliance engine (crate ad_compliance):
  safe hours (TCPA timezone windows), DNC scrubbing, and the abandonment-rate
  ledger with its jurisdiction registry. It also shows where the leader calls
  the engine before each dial, and how the DNC list in Postgres gets its
  entries (the API and the agents' DNC requests). Use this skill when you
  change calling rules, timezones, DNC, abandonment math, or add a
  jurisdiction. The engine fails closed and must stay pure. This is the
  highest-risk product subsystem.
license: AGPL-3.0-or-later
metadata:
  project: agentdialer
  tier: project
---

# Compliance engine (`ad_compliance`)

The compliance engine decides if AgentDialer may call a lead now. A wrong
decision can bring legal and money penalties. This is the part of the product
with the highest risk. Work on it with care.

`PLAN.md` §9 has the full design.

CAUTION: Do not claim that production dialing is compliant. These gaps are
open (`GAPS.md`):

- The leader passes a 24-hour window (COMP-1). No table stores call times or
  holidays.
- The DNC list is in Postgres (`dnc_numbers`). It holds only the entries of
  the API and of the agents. No national list is loaded (COMP-8).
- A lead without an IANA timezone never dials (COMP-2). The dialer does not
  find the timezone from the number or the postal code yet.
- Counsel must review the rules (decision D-5).

## Where the dialer calls the engine

The leader calls it before each dial, in `gate()` in
`crates/ad_bin/src/orchestrator/leader.rs`:

1. It reads the DNC entries of the claimed numbers
   (`PgDncRepo::set_for`). If the list does not read, it releases every
   claim, and nothing dials.
2. It calls `ad_hopper::select` with the leads, the DNC set, the windows,
   and no ruleset. `select` checks the lead status, the DNC set, and then the
   safe hours.
3. It ends each denied lead as `not_dialed`, with the reason. A DNC hit makes
   the lead `dnc`. An unknown timezone makes it `blocked`. A closed window
   holds it for 15 minutes.

The windows are `CallWindowSet::uniform(0, 2400)`, set in
`crates/ad_bin/src/orchestrator/mod.rs`. COMP-1 replaces them with data from
tables. Read how VICIdial stores this data before you add the tables
(`_research/vicidial/`; `GAPS.md` Appendix A lists the files).

The DNC list gets its entries in two ways. Both go through `ad_db::dnc`:

- `POST /v1/dnc` (`crates/ad_api/src/outcomes.rs`), with the source `api`.
- An agent's DNC request on the session stream (`ad_session`), with the
  source `agent`. It takes effect at once: every waiting lead with that
  number becomes `dnc`.

## Two rules you must never break

1. **Fail closed.** If the engine cannot prove that a call is allowed, it
   denies. An unknown timezone, a window that does not parse, and missing
   data must deny. When you add a code path, make the default branch deny.
2. **Stay pure.** A decision function takes data and a `Clock`, and it gives
   a `Decision`. It does no I/O. The caller loads the DNC data and passes it
   in. This keeps the engine the same in each run, and easy to test. In
   production, the `Clock` is `ad_db::PgClock`: the safe hours use the
   Postgres time, not the OS clock of the node (D-11).

## The modules

- `safe_hours.rs`: the calling-window gate. It finds the lead's local time
  from its IANA timezone. Then it checks the window of the weekday and the
  holidays. An unknown timezone denies with `UnknownLocalTime`.
- `dnc.rs`: DNC scrubbing, with the scopes system, campaign, and area code.
  The keys of a `DncSet` are E.164 numbers (`ad_contact::E164`).
  `DncSet::from_lists` refuses an entry that does not read. A number that
  does not read denies with `InvalidNumber`.
- `abandon.rs`: the abandonment ledger and the jurisdiction registry.

Read phone numbers only through `ad_contact`. Do not add a second parser.
Then one change in `ad_contact` changes the rule for every crate.

## The abandonment rate: use the right denominator

The rate of the regulator is not the failed calls over all dials. It is:

```
abandon_rate = answered_by_human_but_no_agent / answered_by_human
```

Only calls that a human answered are in the denominator. The ledger records
each such call, and it marks the call as abandoned or not. See
`AbandonLedger` in `abandon.rs`.

In production, `call_attempts` is the ledger.
`ad_db::PgAttemptRepo::abandon_counts` gives the `AbandonCounts` of a
campaign over a window. The leader then calls the pure
`JurisdictionRule::allows_customer_first`. Keep these rules:

- With no answered call yet, the rate is not known, and customer first is
  allowed.
- With a rate above the cap, or a ledger that does not read, the campaign
  dials agent first. Agent first cannot abandon a call, so this fails closed.
- `connect_within` (2 s) is the limit from the answer to the agent in the
  room. A customer-first call past the limit ends as `abandoned`.
- There is no answering-machine detection yet. Each answer counts as a human
  answer (`GAPS.md` COMP-10).
- The pacing window (`ad_pacing`, DIAL-9) is not a compliance gate. It lowers
  the number of abandoned calls, but the cap above stays the hard limit. The
  default target and back-off of the window come from `connect_within`. If
  you change that limit, saved pool settings can become invalid. Such a pool
  then dials one call at a time (fail closed).

## Add a jurisdiction (the registry)

A jurisdiction is a variant of `JurisdictionRule`, with its match arms for
`cap()`, `window()`, and `connect_within()`. To add one, add the variant and
its arms. Do not add a migration. Mark a rule experimental
(`is_experimental`) until you check it against primary sources. `IndiaTrai`
and `UkOfcom` are experimental now. All campaigns use `UsTsr`
(`JurisdictionRule::CAMPAIGN`) until a campaign can select one.

CAUTION: Before you ship an India rule for production, check the TRAI
Feb-2025 rules against primary TRAI sources. Do not claim compliance without
that check.

## Tests

The tests are in `crates/ad_compliance/tests/`. They cover DST, zones with a
half-hour offset (for example Asia/Kolkata at +5:30), windows that cross
midnight, holidays, closed days, the order of the DNC scopes, and the
abandonment denominator. `properties.rs` has the fail-closed and always-open
property tests.

When you change the engine:

1. Add or change the tests first.
2. Prove that each test can fail. Break the logic one time, see the test
   fail, and then undo the break.
3. Run the crate tests in Docker, and then the full tier (`vertical-slice`):

   ```
   docker compose -f docker-compose.test.yml run --rm --build tests \
       cargo test -p ad_compliance
   docker compose -f docker-compose.test.yml down
   ```

4. If you change the gate, run `e2e/run_e2e.sh`. It has a lead whose number
   is on the system DNC list in another form. The carrier must never see that
   call.

## The API

- `POST /v1/compliance/check` (`crates/ad_api/src/compliance.rs`) runs the
  engine. A window that does not parse gives HTTP 400. This also fails
  closed: the API never gives `allowed: true` for input that it cannot read.
- `GET /v1/campaigns/:id/abandonment` shows the ledger of a campaign, and if
  customer first is allowed.
- `POST /v1/dnc`, `GET /v1/dnc`, and `DELETE /v1/dnc/:id` manage the DNC
  list (the `run-campaigns` skill, "The DNC list").

Ship each API change with contract tests (the `vertical-slice` skill).
