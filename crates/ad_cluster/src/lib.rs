//! Cluster coordination for agentdialer nodes over kine, the etcd API on
//! Postgres (PLAN.md §0.10.8b). Postgres stays the truth for calls. kine says
//! which nodes are alive, which node leads, and which node runs which call:
//!
//! - every node holds `/agentdialer/nodes/{node}` (`Hold::own`);
//! - one node holds `/agentdialer/leader` (`Hold::claim`) and schedules: it
//!   writes `/agentdialer/calls/{node}/{attempt}` for the node that must run
//!   the call;
//! - each node mirrors its own calls prefix and runs what appears there
//!   (`Mirror`);
//! - the leader mirrors the nodes and the calls, and moves the calls of a node
//!   that is gone (`orphans`).
//!
//! The `assign_gen` of the attempt row in Postgres fences a node that lost a
//! call: it can no longer change it.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

pub mod hold;
pub mod keys;
pub mod kine;
pub mod mirror;
pub mod placement;

pub use hold::Hold;
pub use kine::{ClusterError, Entry, Kine, Write};
pub use mirror::{Mirror, Snapshot};
pub use placement::NodeLoad;

/// The value of a node key.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NodeInfo {
    pub node: String,
    /// The most calls the node runs at one time.
    pub capacity: u32,
}

/// The value of a call key.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Assignment {
    /// `call_attempts.assign_gen` of this assignment.
    pub gen: i32,
}

/// The value of the leader key.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LeaderInfo {
    pub node: String,
}

pub fn encode<T: Serialize>(value: &T) -> Vec<u8> {
    serde_json::to_vec(value).expect("cluster records serialize")
}

pub fn decode<'a, T: Deserialize<'a>>(bytes: &'a [u8]) -> Option<T> {
    serde_json::from_slice(bytes).ok()
}

/// The live nodes and the calls assigned to each. Pure.
pub fn loads(nodes: &Snapshot, calls: &Snapshot) -> BTreeMap<String, NodeLoad> {
    let mut out: BTreeMap<String, NodeLoad> = nodes
        .values()
        .filter_map(|e| decode::<NodeInfo>(&e.value))
        .map(|n| {
            (
                n.node,
                NodeLoad {
                    capacity: n.capacity,
                    assigned: 0,
                },
            )
        })
        .collect();
    for key in calls.keys() {
        if let Some((node, _)) = keys::parse_call_key(key) {
            if let Some(load) = out.get_mut(node) {
                load.assigned += 1;
            }
        }
    }
    out
}

/// The calls assigned to nodes that are no longer alive, as (node, attempt).
/// Pure.
pub fn orphans(nodes: &Snapshot, calls: &Snapshot) -> Vec<(String, i64)> {
    let live: std::collections::BTreeSet<String> = nodes
        .values()
        .filter_map(|e| decode::<NodeInfo>(&e.value))
        .map(|n| n.node)
        .collect();
    calls
        .keys()
        .filter_map(|k| keys::parse_call_key(k))
        .filter(|(node, _)| !live.contains(*node))
        .map(|(node, attempt)| (node.to_string(), attempt))
        .collect()
}

/// Write the assignment of `attempt_id` to `node`.
pub async fn assign(
    kine: &Kine,
    node: &str,
    attempt_id: i64,
    gen: i32,
) -> Result<(), ClusterError> {
    let key = keys::call_key(node, attempt_id);
    let value = encode(&Assignment { gen });
    match kine.create(&key, &value, None).await? {
        Write::Done { .. } => Ok(()),
        // The same node got the call back: overwrite the older generation.
        Write::Conflict { current: Some(e) } => {
            match kine.update(&key, &value, None, e.mod_revision).await? {
                Write::Done { .. } => Ok(()),
                Write::Conflict { .. } => Err(ClusterError::Protocol(format!(
                    "{key} changed during assign"
                ))),
            }
        }
        Write::Conflict { current: None } => Err(ClusterError::Protocol(format!(
            "{key} changed during assign"
        ))),
    }
}

/// Remove the assignment of `attempt_id` from `node`.
pub async fn unassign(kine: &Kine, node: &str, attempt_id: i64) -> Result<(), ClusterError> {
    kine.delete(&keys::call_key(node, attempt_id)).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn snapshot(entries: &[(String, Vec<u8>)]) -> Snapshot {
        Arc::new(
            entries
                .iter()
                .map(|(k, v)| {
                    (
                        k.clone(),
                        Entry {
                            key: k.clone(),
                            value: v.clone(),
                            mod_revision: 1,
                        },
                    )
                })
                .collect(),
        )
    }

    fn node(name: &str, capacity: u32) -> (String, Vec<u8>) {
        (
            keys::node_key(name),
            encode(&NodeInfo {
                node: name.into(),
                capacity,
            }),
        )
    }

    fn call(node: &str, attempt: i64) -> (String, Vec<u8>) {
        (
            keys::call_key(node, attempt),
            encode(&Assignment { gen: 1 }),
        )
    }

    #[test]
    fn loads_count_the_calls_of_live_nodes() {
        let nodes = snapshot(&[node("a", 5), node("b", 3)]);
        let calls = snapshot(&[call("a", 1), call("a", 2), call("b", 3), call("gone", 4)]);
        let l = loads(&nodes, &calls);
        assert_eq!(
            l["a"],
            NodeLoad {
                capacity: 5,
                assigned: 2
            }
        );
        assert_eq!(
            l["b"],
            NodeLoad {
                capacity: 3,
                assigned: 1
            }
        );
        assert_eq!(l.len(), 2);
    }

    #[test]
    fn calls_of_a_gone_node_are_orphans() {
        let nodes = snapshot(&[node("a", 5)]);
        let calls = snapshot(&[call("a", 1), call("gone", 4), call("gone", 7)]);
        assert_eq!(
            orphans(&nodes, &calls),
            vec![("gone".to_string(), 4), ("gone".to_string(), 7)]
        );
    }

    #[test]
    fn an_unreadable_node_record_is_not_a_live_node() {
        let nodes = snapshot(&[(keys::node_key("x"), b"not json".to_vec())]);
        let calls = snapshot(&[call("x", 1)]);
        assert!(loads(&nodes, &calls).is_empty());
        assert_eq!(orphans(&nodes, &calls), vec![("x".to_string(), 1)]);
    }
}
