//! Tests for the routing engine. They re-express FusionPBX-style patterns to
//! prove the semantics: anti-action if/else, OR-of-AND groups, break-flow,
//! inline set feeding later conditions, and capture-group-driven routing.

use super::*;

fn verbs(actions: &[Action]) -> Vec<&str> {
    actions.iter().map(|a| a.verb.as_str()).collect()
}

#[test]
fn action_runs_on_match_anti_action_on_mismatch() {
    // group 0: if call_direction == inbound -> action "answer"; else "reject".
    let rs = Ruleset::new(vec![
        Detail::condition("call_direction", "^inbound$", 0, 0),
        Detail::action("answer", "", 0, 10),
        Detail::anti_action("reject", "", 0, 20),
    ]);

    let inbound = evaluate(&rs, &Context::from_pairs([("call_direction", "inbound")]));
    assert_eq!(verbs(&inbound), vec!["answer"]);

    let outbound = evaluate(&rs, &Context::from_pairs([("call_direction", "outbound")]));
    assert_eq!(verbs(&outbound), vec!["reject"]);
}

#[test]
fn capture_group_drives_action_args() {
    // destination ^5(\d{4})$ -> set target=$1 (the 4 captured digits).
    let rs = Ruleset::new(vec![
        Detail::condition("destination", r"^5(\d{4})$", 0, 0),
        Detail::action("set", "target=$1", 0, 10),
    ]);
    let out = evaluate(&rs, &Context::from_pairs([("destination", "51234")]));
    assert_eq!(out.len(), 1);
    assert_eq!(out[0].args, "target=1234");
}

#[test]
fn unconditional_group_always_matches() {
    // A group with no condition rows runs its actions (FusionPBX unconditional).
    let rs = Ruleset::new(vec![Detail::action("log", "hello", 0, 0)]);
    let out = evaluate(&rs, &Context::new());
    assert_eq!(verbs(&out), vec!["log"]);
}

#[test]
fn break_on_true_stops_later_groups() {
    // group 0 matches and breaks on-true; group 1 must not run.
    let rs = Ruleset::new(vec![
        Detail::condition("x", "^1$", 0, 0).with_break(Break::OnTrue),
        Detail::action("first", "", 0, 10),
        Detail::condition("x", "^1$", 1, 0),
        Detail::action("second", "", 1, 10),
    ]);
    let out = evaluate(&rs, &Context::from_pairs([("x", "1")]));
    assert_eq!(verbs(&out), vec!["first"]);
}

#[test]
fn break_on_true_falls_through_when_false() {
    // group 0's condition is false, so on-true does not stop; group 1 runs.
    let rs = Ruleset::new(vec![
        Detail::condition("x", "^1$", 0, 0).with_break(Break::OnTrue),
        Detail::action("first", "", 0, 10),
        Detail::action("second", "", 1, 10),
    ]);
    let out = evaluate(&rs, &Context::from_pairs([("x", "0")]));
    assert_eq!(verbs(&out), vec!["second"]);
}

#[test]
fn inline_set_feeds_a_later_group_condition() {
    // group 0 inline-sets user_exists=true; group 1 conditions on it.
    let rs = Ruleset::new(vec![
        Detail::action("set", "user_exists=true", 0, 0).with_inline(true),
        Detail::condition("user_exists", "^true$", 1, 0),
        Detail::action("route_to_user", "", 1, 10),
    ]);
    let out = evaluate(&rs, &Context::new());
    assert_eq!(verbs(&out), vec!["set", "route_to_user"]);
}

#[test]
fn ordering_within_group_follows_order_field() {
    let rs = Ruleset::new(vec![
        Detail::action("c", "", 0, 30),
        Detail::action("a", "", 0, 10),
        Detail::action("b", "", 0, 20),
    ]);
    let out = evaluate(&rs, &Context::new());
    assert_eq!(verbs(&out), vec!["a", "b", "c"]);
}

#[test]
fn multi_condition_group_is_anded() {
    // Both conditions must hold for the action to run.
    let rs = Ruleset::new(vec![
        Detail::condition("a", "^1$", 0, 0),
        Detail::condition("b", "^2$", 0, 1),
        Detail::action("go", "", 0, 10),
    ]);
    assert_eq!(
        verbs(&evaluate(
            &rs,
            &Context::from_pairs([("a", "1"), ("b", "2")])
        )),
        vec!["go"]
    );
    assert!(evaluate(&rs, &Context::from_pairs([("a", "1"), ("b", "9")])).is_empty());
}

#[test]
fn bad_regex_fails_closed_no_match() {
    // An invalid pattern must not match (and must not panic).
    let rs = Ruleset::new(vec![
        Detail::condition("x", "(", 0, 0),
        Detail::action("should_not_run", "", 0, 10),
    ]);
    assert!(evaluate(&rs, &Context::from_pairs([("x", "anything")])).is_empty());
}
