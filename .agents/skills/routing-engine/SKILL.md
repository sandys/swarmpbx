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
XML and not Lua. Read `PLAN.md` §0.10.5 for the full specification.

The engine was studied from the real FusionPBX dialplan code in
`_research/fusionpbx/app/dialplans`. Conform to that model.

## The model (mirrors FusionPBX `dialplan_detail`)

A ruleset is an ordered list of rules. Each rule is an ordered list of details.
Each detail has these fields:

- `tag`: `condition`, `action`, or `anti_action`.
- `type`: the field to test (condition) or the verb to run (action).
- `data`: a regex (condition) or an argument string (action). Both support
  variable interpolation.
- `break`: `on_true`, `on_false`, `always`, or `never`. It controls flow between
  groups.
- `inline`: evaluate now (so a later condition reads a variable a prior action
  set) or defer to execution.
- `group`: an integer. It groups details into OR-of-AND condition blocks.
- `order`: the evaluation order.

## The semantics (exactly FusionPBX)

- In a group, all `condition` details are ANDed. Capture groups from condition
  regexes (`$1`, `$2`) bind into the context.
- `action` details in a matched group run. `anti_action` details run when the
  group condition is false. This is the if/else.
- `break` decides whether evaluation stops or falls through to the next group.
- `continue` chaining across rulesets mirrors `dialplan_continue`.
- Global rulesets apply to all orgs. An org ruleset overrides. This mirrors
  FusionPBX global vs domain-specific dialplans.

## Context and functions

Rules evaluate against a `Context`. The context is a string-to-string map: lead
fields, call variables, campaign settings, and capture groups. Interpolation
supports `${var}`, `$${global}`, capture references, and a registry of pure
functions (for example `cond(x==y?a:b)`, `regex(...)`, `strftime(...)`,
`tz_now(...)`, `dnc_check(...)`). Functions are pure and sandboxed. Do not embed
Lua. Do not shell out.

## Actions call our primitives

Actions are typed. They call AgentDialer primitives, not FreeSWITCH verbs.
Examples: `set`, `unset`, `export`, `select_trunk`, `set_caller_id`,
`bridge_agent`, `transfer`, `schedule_callback`, `set_status`, `add_dnc`,
`recycle`, `set_record`, `limit`, `hangup`, `log`.

## Registry rules

- Add a function: one module plus one registration line.
- Add an action: one module plus one registration line.
- Do not run a migration to add a function or an action. The discriminator
  lives in data.

## The four rulesets

1. Selection ruleset: lead to trunk, caller id, and agent.
2. Outcome ruleset: disposition to next action (retry, callback, DNC, done).
3. Inbound ruleset: DID to campaign and agent.
4. Time-condition rulesets: business-hours routing.

Note: time conditions are business routing. They are not the compliance
safe-hours gate. The safe-hours gate is in `ad_compliance` and fails closed.
See the `compliance-engine` skill.

## Tests

Keep the engine pure, so test it like the compliance engine: unit tests for
each verb and property tests for group, break, and inline ordering. Re-express
the FusionPBX reference examples (Call-Forward, User-Record, Outbound-Route) as
rules and assert the engine produces the same action sequence. Ship the engine
API and its tests together. See the `vertical-slice` skill.
