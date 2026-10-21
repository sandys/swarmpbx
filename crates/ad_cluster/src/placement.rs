//! Placement: which live node runs the next call. Pure.

use std::collections::BTreeMap;

/// A live node, as the leader sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeLoad {
    /// The most calls the node runs at one time.
    pub capacity: u32,
    /// The calls assigned to it now.
    pub assigned: u32,
}

impl NodeLoad {
    pub fn free(&self) -> u32 {
        self.capacity.saturating_sub(self.assigned)
    }
}

/// The node with the most free capacity; ties go to the first name. `None`
/// when every node is full.
pub fn pick(nodes: &BTreeMap<String, NodeLoad>) -> Option<&str> {
    nodes
        .iter()
        .filter(|(_, l)| l.free() > 0)
        .max_by(|(an, a), (bn, b)| a.free().cmp(&b.free()).then(bn.cmp(an)))
        .map(|(n, _)| n.as_str())
}

/// The least-loaded live node, even when it is full. For a call that moves
/// from a node that is gone: the call is live already, so it must not wait for
/// free capacity. `None` only when no node is alive.
pub fn pick_any(nodes: &BTreeMap<String, NodeLoad>) -> Option<&str> {
    nodes
        .iter()
        .min_by(|(an, a), (bn, b)| a.assigned.cmp(&b.assigned).then(an.cmp(bn)))
        .map(|(n, _)| n.as_str())
}

/// The free capacity of the whole cluster.
pub fn free(nodes: &BTreeMap<String, NodeLoad>) -> u32 {
    nodes.values().map(NodeLoad::free).sum()
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn cluster(loads: &[(u32, u32)]) -> BTreeMap<String, NodeLoad> {
        loads
            .iter()
            .enumerate()
            .map(|(i, (capacity, assigned))| {
                (
                    format!("node-{i}"),
                    NodeLoad {
                        capacity: *capacity,
                        assigned: *assigned,
                    },
                )
            })
            .collect()
    }

    #[test]
    fn the_least_loaded_node_gets_the_call() {
        let c = cluster(&[(10, 7), (10, 2), (10, 5)]);
        assert_eq!(pick(&c), Some("node-1"));
        assert_eq!(free(&c), 16);
    }

    #[test]
    fn ties_go_to_the_first_name_and_full_nodes_get_nothing() {
        assert_eq!(pick(&cluster(&[(5, 1), (5, 1)])), Some("node-0"));
        assert_eq!(pick(&cluster(&[(5, 5), (3, 3)])), None);
        assert_eq!(pick(&cluster(&[])), None);
        // A node over its capacity (it shrank) is full, not negative.
        assert_eq!(free(&cluster(&[(2, 4)])), 0);
    }

    #[test]
    fn a_moved_call_goes_to_the_least_loaded_node_even_when_full() {
        let full = cluster(&[(2, 3), (2, 2)]);
        assert_eq!(pick(&full), None);
        assert_eq!(pick_any(&full), Some("node-1"));
        assert_eq!(pick_any(&cluster(&[])), None);
    }

    proptest! {
        // Placing calls one by one fills the cluster evenly and never
        // overfills a node.
        #[test]
        fn placement_is_even_and_bounded(
            caps in proptest::collection::vec(1u32..20, 1..6),
            calls in 0u32..150
        ) {
            let mut c = cluster(&caps.iter().map(|c| (*c, 0)).collect::<Vec<_>>());
            let total: u32 = caps.iter().sum();
            for _ in 0..calls {
                match pick(&c).map(str::to_string) {
                    Some(n) => c.get_mut(&n).unwrap().assigned += 1,
                    None => prop_assert_eq!(free(&c), 0),
                }
            }
            for l in c.values() {
                prop_assert!(l.assigned <= l.capacity);
            }
            prop_assert_eq!(c.values().map(|l| l.assigned).sum::<u32>(), calls.min(total));
        }
    }
}
