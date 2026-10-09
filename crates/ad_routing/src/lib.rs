//! A typed, data-defined routing rule engine (PLAN.md §0.10.5), modeled on the
//! FusionPBX dialplan: ordered rules, each a list of details grouped into
//! OR-of-AND condition blocks, with condition / action / anti-action rows and
//! break-flow control. No XML, no Lua — the evaluator produces a list of
//! resolved `Action`s that the caller (dialer / hopper) interprets.

pub mod context;

pub use context::Context;

use regex::Regex;

/// What a detail row is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Tag {
    Condition,
    Action,
    AntiAction,
}

/// Break-flow control between condition groups (FusionPBX semantics).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Break {
    /// Stop evaluating further groups once this group's condition is true.
    OnTrue,
    /// Stop once false.
    OnFalse,
    /// Always stop after this group.
    Always,
    /// Never stop (default).
    #[default]
    Never,
}

/// One detail row.
#[derive(Debug, Clone)]
pub struct Detail {
    pub tag: Tag,
    /// For a condition: the field to test (a variable name, resolved from the
    /// context). For an action/anti-action: the verb.
    pub field_or_verb: String,
    /// For a condition: a regex matched against the field value. For an
    /// action: the argument string (subject to interpolation at resolve time).
    pub data: String,
    pub group: u32,
    pub order: u32,
    pub break_: Break,
    /// Inline evaluation: an action marked inline is applied to the context
    /// immediately during the pass (so later conditions can read a var it set).
    /// Only `set`/`unset`/`export` are honored inline; others are deferred.
    pub inline: bool,
}

impl Detail {
    pub fn condition(field: &str, data: &str, group: u32, order: u32) -> Self {
        Detail {
            tag: Tag::Condition,
            field_or_verb: field.into(),
            data: data.into(),
            group,
            order,
            break_: Break::Never,
            inline: false,
        }
    }
    pub fn action(verb: &str, data: &str, group: u32, order: u32) -> Self {
        Detail {
            tag: Tag::Action,
            field_or_verb: verb.into(),
            data: data.into(),
            group,
            order,
            break_: Break::Never,
            inline: false,
        }
    }
    pub fn anti_action(verb: &str, data: &str, group: u32, order: u32) -> Self {
        Detail {
            tag: Tag::AntiAction,
            ..Detail::action(verb, data, group, order)
        }
    }
    pub fn with_break(mut self, b: Break) -> Self {
        self.break_ = b;
        self
    }
    pub fn with_inline(mut self, inline: bool) -> Self {
        self.inline = inline;
        self
    }
}

/// A ruleset is an ordered list of details (one logical "extension").
#[derive(Debug, Clone, Default)]
pub struct Ruleset {
    pub details: Vec<Detail>,
}

impl Ruleset {
    pub fn new(details: Vec<Detail>) -> Self {
        Ruleset { details }
    }
}

/// A resolved action the evaluator emits (verb + interpolated args).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Action {
    pub verb: String,
    pub args: String,
}

/// Evaluate a ruleset against a context. Returns the ordered list of resolved
/// actions. `set`/`unset`/`export` mutate the working context when `inline`.
pub fn evaluate(ruleset: &Ruleset, ctx: &Context) -> Vec<Action> {
    let mut ctx = ctx.clone();
    let mut actions = Vec::new();

    // Group details by `group`, preserving ascending group order.
    let mut groups: Vec<u32> = ruleset
        .details
        .iter()
        .map(|d| d.group)
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect();
    groups.sort_unstable();

    for g in groups {
        let mut rows: Vec<&Detail> = ruleset.details.iter().filter(|d| d.group == g).collect();
        rows.sort_by_key(|d| d.order);

        // All conditions in the group are ANDed. Capture groups from the first
        // matching condition regex are bound into the context.
        let conditions: Vec<&&Detail> = rows.iter().filter(|d| d.tag == Tag::Condition).collect();
        let matched = eval_conditions(&conditions, &mut ctx);

        for d in &rows {
            match d.tag {
                Tag::Condition => {}
                Tag::Action if matched => apply(d, &mut ctx, &mut actions),
                Tag::AntiAction if !matched => apply(d, &mut ctx, &mut actions),
                _ => {}
            }
        }

        // Break-flow control decides whether to evaluate further groups.
        let stop = match rows.iter().map(|d| d.break_).find(|b| *b != Break::Never) {
            Some(Break::OnTrue) => matched,
            Some(Break::OnFalse) => !matched,
            Some(Break::Always) => true,
            _ => false,
        };
        if stop {
            break;
        }
    }

    actions
}

/// Evaluate the ANDed conditions of a group. Empty condition set = match
/// (an unconditional group, FusionPBX style). Binds capture groups.
fn eval_conditions(conditions: &[&&Detail], ctx: &mut Context) -> bool {
    for cond in conditions {
        let field_val = resolve_field(&cond.field_or_verb, ctx);
        let pattern = ctx.interpolate(&cond.data);
        let re = match Regex::new(&pattern) {
            Ok(re) => re,
            // Fail closed on a bad pattern: treat as non-match.
            Err(_) => return false,
        };
        match re.captures(&field_val) {
            Some(caps) => {
                let groups: Vec<Option<String>> = caps
                    .iter()
                    .skip(1)
                    .map(|m| m.map(|mm| mm.as_str().to_string()))
                    .collect();
                ctx.set_captures(&groups);
            }
            None => return false,
        }
    }
    true
}

/// A condition field names a context variable; resolve it (interpolating any
/// `${...}` so e.g. `${call_direction}` works, as does a bare `destination`).
fn resolve_field(field: &str, ctx: &Context) -> String {
    if field.contains("${") || field.contains('$') {
        ctx.interpolate(field)
    } else {
        ctx.get(field).map(|s| s.to_string()).unwrap_or_default()
    }
}

/// Apply an action: interpolate args; mutate context for inline set/unset/
/// export; always record the resolved action for the caller.
fn apply(d: &Detail, ctx: &mut Context, actions: &mut Vec<Action>) {
    let args = ctx.interpolate(&d.data);
    if d.inline {
        match d.field_or_verb.as_str() {
            "set" | "export" => {
                if let Some((k, v)) = args.split_once('=') {
                    ctx.set(k.trim(), v.trim());
                }
            }
            "unset" => ctx.unset(args.trim()),
            _ => {}
        }
    }
    actions.push(Action {
        verb: d.field_or_verb.clone(),
        args,
    });
}

#[cfg(test)]
mod tests;
