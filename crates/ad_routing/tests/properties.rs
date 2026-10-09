//! Property tests for the routing engine invariants.

use ad_routing::{evaluate, Context, Detail, Ruleset};
use proptest::prelude::*;

proptest! {
    // INVARIANT: an empty ruleset emits no actions, for any context.
    #[test]
    fn empty_ruleset_emits_nothing(keys in proptest::collection::vec("[a-z]{1,5}", 0..5)) {
        let mut ctx = Context::new();
        for k in &keys { ctx.set(k.clone(), "v"); }
        prop_assert!(evaluate(&Ruleset::default(), &ctx).is_empty());
    }

    // INVARIANT: actions in one matched group come out in ascending `order`,
    // regardless of the order they were inserted.
    #[test]
    fn actions_respect_order_field(mut orders in proptest::collection::vec(0u32..1000, 1..8)) {
        // Build details with distinct verbs tagged by their order, inserted
        // in a scrambled sequence.
        orders.sort_unstable();
        orders.dedup();
        let mut details: Vec<Detail> = orders
            .iter()
            .map(|o| Detail::action(&format!("v{o}"), "", 0, *o))
            .collect();
        // Scramble insertion order deterministically.
        details.reverse();
        let out = evaluate(&Ruleset::new(details), &Context::new());
        let got: Vec<u32> = out
            .iter()
            .map(|a| a.verb.trim_start_matches('v').parse().unwrap())
            .collect();
        let mut expected = orders.clone();
        expected.sort_unstable();
        prop_assert_eq!(got, expected);
    }

    // INVARIANT: for a single-condition group, exactly one of {action,
    // anti_action} fires — never both, never neither. action iff the condition
    // matches; anti_action iff it does not.
    #[test]
    fn anti_action_is_exact_complement(val in "[a-z]{1,6}", pat in "[a-z]{1,6}") {
        let rs = Ruleset::new(vec![
            Detail::condition("x", &format!("^{pat}$"), 0, 0),
            Detail::action("yes", "", 0, 10),
            Detail::anti_action("no", "", 0, 20),
        ]);
        let out = evaluate(&rs, &Context::from_pairs([("x", val.as_str())]));
        prop_assert_eq!(out.len(), 1);
        let fired = out[0].verb.as_str();
        if val == pat {
            prop_assert_eq!(fired, "yes");
        } else {
            prop_assert_eq!(fired, "no");
        }
    }

    // INVARIANT: a group with no condition rows (unconditional) always runs its
    // action, for any context.
    #[test]
    fn unconditional_group_always_runs(k in "[a-z]{1,5}", v in "[a-z]{1,5}") {
        let rs = Ruleset::new(vec![Detail::action("always", "", 0, 0)]);
        let out = evaluate(&rs, &Context::from_pairs([(k.as_str(), v.as_str())]));
        prop_assert_eq!(out.len(), 1);
        prop_assert_eq!(out[0].verb.as_str(), "always");
    }
}
