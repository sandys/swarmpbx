---
name: routing-engine
description: >
  Teaches the AgentDialer routing rule engine (crate ad_routing), the keystone of
  the campaign brain. It re-expresses the FusionPBX dialplan model as typed Rust
  rules. Use this skill when you work on dial logic, lead-to-trunk selection,
  outcome routing (disposition to retry/callback/DNC), agent selection, or time
  conditions. Do not use XML or Lua. Rules are data; actions call our primitives.
license: AGPL-3.0-or-later
metadata:
  project: agentdialer
  tier: project
---

# Routing rule engine (`ad_routing`)

`ad_routing` is the keystone of the campaign brain. It gives operators
FusionPBX-grade control over dial logic as data, not code. It is typed Rust, not
XML and not Lua. Read `PLAN.md` §0.10.5 for the target specification.

The model comes from the real FusionPBX dialplan code in
`_research/fusionpbx/app/dialplans` and from the FreeSWITCH evaluator in
`_research/freeswitch/src/mod/dialplans/mod_dialplan_xml/mod_dialplan_xml.c`.
Read both before you change the semantics.

Caution: if you port a FusionPBX rule, expect a different result. The engine
semantics are not the FreeSWITCH semantics yet (`GAPS.md` ROUT-1). Decision
D-1 is open. The recommendation is to match FreeSWITCH.

## What is built

A `Ruleset` is an ordered list of `Detail` rows. `evaluate(&Ruleset,
&Context)` returns the resolved actions as a `Vec<Action>` (`verb`, `args`).
It is pure. A `Detail` has these fields (they mirror FusionPBX
`dialplan_detail`):

- `tag`: `Condition`, `Action`, or `AntiAction`.
- `field_or_verb`: the field to test (condition) or the verb to run (action).
- `data`: a regex (condition) or an argument string (action). Both support
  interpolation.
- `group`: an integer. Details with the same group form one block.
- `order`: the evaluation order.
- `break_`: `OnTrue`, `OnFalse`, `Always`, or `Never` (the default).
- `inline`: if true, `set`, `export`, and `unset` change the context at once,
  so a later condition reads the new value.

The current semantics:

- In a group, all conditions are ANDed. The `$N` captures of a match go into
  the context and stay there.
- If the group matches, its actions run. If a condition of the group fails,
  the anti-actions of the group run.
- `break_` decides per group if evaluation stops.

Interpolation supports `${var}`, `$N`, and function calls in the form
`${name:arg1,arg2}`. The function registry (`builtin` in `context.rs`) has
`cond_eq`, `regex_match`, `upper`, and `lower`. Functions are pure. Do not
embed Lua. Do not shell out.

Actions are free strings. The engine itself runs only the inline `set`,
`export`, and `unset`. The hopper honors `skip`, `hangup`, and `add_dnc`.
Production passes no ruleset yet (`GAPS.md` ROUT-2).

## What is not built (the target)

`GAPS.md` ROUT-1 to ROUT-4 list the work. In short:

- FreeSWITCH semantics: break and anti-actions per condition, `on-false` as
  the default break, `$N` from the condition's own match, and extension
  `continue`.
- Versioned ruleset storage and an API. A campaign binds a selection ruleset
  and an outcome ruleset.
- A typed action registry that calls AgentDialer primitives, not FreeSWITCH
  verbs: `set_trunk`, `set_caller_id`, `schedule_callback`, `recycle`,
  `add_dnc`, `set_status`, `transfer`, `limit`, `hangup`.
- More functions: `strftime`, `tz_now`, `lead_data`, `dnc_check`, `hash`.
- Time conditions, `regex="all|any|xor"`, and global versus org scope.
- An explain trace of each evaluation.

The four rulesets of the target:

1. Selection ruleset: lead to trunk, caller id, and agent.
2. Outcome ruleset: disposition to next action (retry, callback, DNC, done).
3. Inbound ruleset: DID to campaign and agent.
4. Time-condition rulesets: business-hours routing.

Note: time conditions are business routing. They are not the compliance
safe-hours gate. The safe-hours gate is in `ad_compliance` and fails closed.
See the `compliance-engine` skill.

## Registry rules

- Add a function: one match arm in `builtin` (`context.rs`).
- Add an action: one module plus one registration line, when the typed action
  registry exists.
- Do not run a migration to add a function or an action. The discriminator
  lives in data.

## Tests

Keep the engine pure, so test it like the compliance engine: unit tests for
each verb and property tests for group, break, and inline ordering. Re-express
the FusionPBX reference examples (Call-Forward, User-Record, Outbound-Route) as
rules and assert the engine produces the same action sequence. Ship the engine
API and its tests together. See the `vertical-slice` skill.
