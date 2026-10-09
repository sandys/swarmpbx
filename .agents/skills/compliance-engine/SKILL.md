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

## Two rules you must never break

1. Fail closed. If the engine cannot prove a call is permitted, it denies.
   Examples that must deny: unknown timezone, unparseable window, missing data.
   When you add a code path, the default branch denies.

2. Stay pure. A decision function takes data plus a `Clock` and returns a
   `Decision`. It does no I/O. The caller loads DNC data and passes it in. This
   keeps the engine deterministic and testable.

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
3. Run `cargo test -p ad_compliance` and `cargo clippy --all-targets -- -D warnings`.

## The API

The engine is exposed at `POST /v1/compliance/check` in
`crates/ad_api/src/compliance.rs`. A malformed window returns HTTP 400. That is
also fail-closed: the API never returns `allowed: true` for input it cannot
interpret. Ship API changes with contract tests. See the `vertical-slice` skill.
