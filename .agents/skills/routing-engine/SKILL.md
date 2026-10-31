---
name: routing-engine
description: >
  Teaches the AgentDialer routing rule engine (crate ad_routing). It holds
  the dial logic of the campaigns as data, on the FusionPBX dialplan model,
  in typed Rust rules. Use this skill when you work on dial logic, lead-to-trunk selection,
  outcome routing (disposition to retry/callback/DNC), agent selection, or time
  conditions. Do not use XML or Lua. Rules are data; actions call our primitives.
license: AGPL-3.0-or-later
metadata:
  project: agentdialer
  tier: project
---

# Routing rule engine (`ad_routing`)

`ad_routing` lets an operator control the dial logic with data, not code. It
follows the FusionPBX dialplan model. It is typed Rust, not XML and not Lua.
`PLAN.md` §0.10.5 has the target.

The model comes from two places. Read both before you change how rules work:

- the FusionPBX dialplan code in `_research/fusionpbx/app/dialplans`;
- the FreeSWITCH evaluator in
  `_research/freeswitch/src/mod/dialplans/mod_dialplan_xml/mod_dialplan_xml.c`.

CAUTION: A FusionPBX rule that you port can give a different result. The
engine does not follow the FreeSWITCH rules yet (`GAPS.md` ROUT-1). Decision
D-1 is open. The advice is to match FreeSWITCH.

## What is built

A `Ruleset` is a list of `Detail` rows in order. `evaluate(&Ruleset,
&Context)` gives the actions that apply, as a `Vec<Action>` (`verb`, `args`).
It is pure. A `Detail` has the fields of a FusionPBX `dialplan_detail`:

- `tag`: `Condition`, `Action`, or `AntiAction`.
- `field_or_verb`: the field to test (a condition) or the verb to run (an
  action).
- `data`: a regex (a condition) or an argument string (an action). Both can
  hold variables.
- `group`: a number. The details of one group form one block.
- `order`: the order of the checks.
- `break_`: `OnTrue`, `OnFalse`, `Always`, or `Never` (the default).
- `inline`: when true, `set`, `export`, and `unset` change the context at
  once. A later condition then reads the new value.

How the engine works now:

- All conditions of a group must match. The `$N` captures of a match go into
  the context, and they stay there.
- If the group matches, its actions run. If a condition of the group fails,
  the anti-actions of the group run.
- `break_` decides, for each group, if the check stops.

A string can hold `${var}`, `$N`, and function calls such as
`${name:arg1,arg2}`. The function registry (`builtin` in `context.rs`) has
`cond_eq`, `regex_match`, `upper`, and `lower`. The functions are pure. Do not
embed Lua. Do not run a shell.

Actions are free strings. The engine runs only the inline `set`, `export`,
and `unset`. The hopper obeys `skip`, `hangup`, and `add_dnc`.

## Where it is not wired yet

- **The dial path.** Production passes no ruleset (`GAPS.md` ROUT-2). The
  place to wire it is the leader's `gate()` in
  `crates/ad_bin/src/orchestrator/leader.rs`. It calls `ad_hopper::select`
  with `ruleset: None`. To try a ruleset now, use
  `POST /v1/campaigns/:id/hopper/preview`.
- **The outcome.** The step from a call's result to the lead's next state is
  code now: `ad_orchestrator::settle_with` in
  `crates/ad_orchestrator/src/settle.rs`. A DNC request wins. A callback
  queues the lead at its time. A machine answer gives a voicemail retry. Then
  the call result decides. An outcome ruleset will make this data. Keep the
  same order, and keep the DNC request first.

## What is not built (the target)

`GAPS.md` ROUT-1 to ROUT-4 list the work:

- The FreeSWITCH rules: break and anti-actions for each condition, `on-false`
  as the default break, `$N` from the condition's own match, and extension
  `continue`.
- Rulesets in storage, with versions and an API. A campaign binds a
  selection ruleset and an outcome ruleset.
- A typed action registry that calls AgentDialer's own steps, not FreeSWITCH
  verbs: `set_trunk`, `set_caller_id`, `schedule_callback`, `recycle`,
  `add_dnc`, `set_status`, `transfer`, `limit`, `hangup`.
- More functions: `strftime`, `tz_now`, `lead_data`, `dnc_check`, `hash`.
- Time conditions, `regex="all|any|xor"`, and a global or an org scope.
- A trace that explains each check.

The four rulesets of the target:

1. Selection: lead to trunk, caller ID, and agent.
2. Outcome: disposition to the next step (retry, callback, DNC, done). Read
   the disposition flags in `ad_disposition` first.
3. Inbound: DID to campaign and agent.
4. Time conditions: routing by business hours.

NOTE: Time conditions are business routing. They are not the safe-hours gate.
The safe-hours gate is in `ad_compliance`, and it fails closed (the
`compliance-engine` skill).

## Registry rules

- To add a function, add one match arm in `builtin` (`context.rs`).
- To add an action, add one module and one line to register it, when the
  typed action registry exists.
- Do not add a migration for a function or an action. The kind is in the
  data.

## Tests

The engine is pure, so test it like the compliance engine. Write unit tests
for each verb, and property tests for groups, breaks, and the inline order.
Write the FusionPBX examples (Call-Forward, User-Record, Outbound-Route) as
rules, and check that the engine gives the same actions in the same order.
Ship the engine API with its tests (the `vertical-slice` skill).
