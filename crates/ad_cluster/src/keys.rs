//! The key layout in kine. Postgres stays the source of truth for calls; these
//! keys say which node is alive, which node leads, and which node runs which
//! call.
//!
//! ```text
//! /agentdialer/nodes/{node}            node record, rewritten before its TTL ends
//! /agentdialer/leader                  the leader record, rewritten likewise
//! /agentdialer/calls/{node}/{attempt}  an assignment (no TTL): {"gen": N}
//! ```

pub const ROOT: &str = "/agentdialer/";

pub fn nodes_prefix() -> String {
    format!("{ROOT}nodes/")
}

pub fn node_key(node: &str) -> String {
    format!("{ROOT}nodes/{node}")
}

pub fn leader_key() -> String {
    format!("{ROOT}leader")
}

pub fn calls_prefix() -> String {
    format!("{ROOT}calls/")
}

pub fn node_calls_prefix(node: &str) -> String {
    format!("{ROOT}calls/{node}/")
}

pub fn call_key(node: &str, attempt_id: i64) -> String {
    format!("{ROOT}calls/{node}/{attempt_id}")
}

/// The node of a node key.
pub fn parse_node_key(key: &str) -> Option<&str> {
    let node = key.strip_prefix(ROOT)?.strip_prefix("nodes/")?;
    (!node.is_empty() && !node.contains('/')).then_some(node)
}

/// The node and the attempt of a call key.
pub fn parse_call_key(key: &str) -> Option<(&str, i64)> {
    let rest = key.strip_prefix(ROOT)?.strip_prefix("calls/")?;
    let (node, attempt) = rest.split_once('/')?;
    if node.is_empty() || attempt.contains('/') {
        return None;
    }
    Some((node, attempt.parse().ok()?))
}

/// A node name must be one key segment.
pub fn valid_node_name(node: &str) -> bool {
    !node.is_empty()
        && node.len() <= 128
        && node
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_round_trip() {
        assert_eq!(parse_node_key(&node_key("node-1")), Some("node-1"));
        assert_eq!(
            parse_call_key(&call_key("node-1", 42)),
            Some(("node-1", 42))
        );
        assert!(node_calls_prefix("node-1").starts_with(&calls_prefix()));
        assert!(call_key("node-1", 42).starts_with(&node_calls_prefix("node-1")));
        // One node's prefix never matches another node with a longer name.
        assert!(!call_key("node-10", 1).starts_with(&node_calls_prefix("node-1")));
    }

    #[test]
    fn foreign_keys_do_not_parse() {
        assert_eq!(parse_node_key("/agentdialer/nodes/"), None);
        assert_eq!(parse_node_key("/agentdialer/nodes/a/b"), None);
        assert_eq!(parse_node_key("/other/nodes/a"), None);
        assert_eq!(parse_call_key("/agentdialer/calls/n/x"), None);
        assert_eq!(parse_call_key("/agentdialer/calls/n/1/2"), None);
        assert_eq!(parse_call_key("/agentdialer/calls//1"), None);
    }

    #[test]
    fn node_names_are_one_segment() {
        assert!(valid_node_name("ad-node-1.cluster_x"));
        for bad in ["", "a/b", "a b", "ä"] {
            assert!(!valid_node_name(bad), "{bad:?}");
        }
    }
}
