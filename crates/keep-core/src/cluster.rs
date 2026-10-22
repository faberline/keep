//! Cluster topology & sharding (HA phase A: horizontal scale-out).
//!
//! keep runs as a StatefulSet of independent stores (`keep-0..keep-N`). The
//! keyspace is split into `shard_count` virtual shards mapped onto `node_count`
//! nodes; a client routes a key with `crc32(key) % shard_count -> shard ->
//! node`. This module is the addressing/membership substrate that both the
//! client-routing story and a future raft consensus layer build on. It does NOT
//! itself replicate or coordinate — that's phase C (see the keep HA / Raft
//! capability and TD roadmap).
//!
//! Single-node default (node_count = 1): one node owns every shard.

use std::sync::Arc;

use serde::Serialize;
use utoipa::ToSchema;

/// Cluster membership + sharding parameters. Install-time constants (changing
/// `shard_count` on a live cluster reshuffles ownership — treat as fixed).
#[derive(Debug, Clone)]
pub struct ClusterConfig {
    /// This node's ordinal (k8s StatefulSet pod index `keep-<id>`).
    pub node_id: usize,
    /// Total nodes in the cluster.
    pub node_count: usize,
    /// Virtual shard count (>= node_count; the client-side routing fan-out).
    pub shard_count: u32,
    /// Peer base URLs. Without a [`ReplicaPlacement`] the index is the node
    /// ordinal; with one it is the replica id inside this pod's shard group
    /// (own slot empty).
    pub peers: Vec<String>,
    /// Raft replica placement (HA phase C). `Some` when the StatefulSet runs
    /// `replicas_per_shard > 1`: this pod hosts exactly one shard group.
    pub replica: Option<ReplicaPlacement>,
}

/// Where this pod sits in the `shard x replica` StatefulSet grid
/// (`ordinal = replica_index * shard_count + shard_index`), mirroring
/// raft-runtime's `ClusterDims`. Voters are replica ids `0..voter_count`; the
/// rest are learners.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReplicaPlacement {
    pub shard_index: u32,
    pub replica_index: u32,
    pub replicas_per_shard: u32,
    pub voter_count: u32,
}

impl ReplicaPlacement {
    /// Whether this replica is a voting member of its shard group.
    pub fn is_voter(&self) -> bool {
        self.replica_index < self.voter_count
    }
}

impl Default for ClusterConfig {
    fn default() -> Self {
        Self {
            node_id: 0,
            node_count: 1,
            shard_count: 1,
            peers: Vec::new(),
            replica: None,
        }
    }
}

impl ClusterConfig {
    /// Build from a node id / count / shard count, deriving k8s peer DNS names
    /// (`keep-<i>.<svc>`) when no explicit peers are given.
    pub fn new(node_id: usize, node_count: usize, shard_count: u32, peers: Vec<String>) -> Self {
        let node_count = node_count.max(1);
        let shard_count = shard_count.max(node_count as u32).max(1);
        Self {
            node_id: node_id.min(node_count - 1),
            node_count,
            shard_count,
            peers,
            replica: None,
        }
    }

    /// Build from the shared k8s topology helper. Unlike the legacy local
    /// sharding constructor, this preserves the StatefulSet's shard count
    /// literally: one shard with three replicas is valid in HA mode.
    pub fn from_shared_topology(
        node_id: usize,
        node_count: usize,
        shard_count: u32,
        peers: Vec<String>,
    ) -> Self {
        let node_count = node_count.max(1);
        Self {
            node_id: node_id.min(node_count - 1),
            node_count,
            shard_count: shard_count.max(1),
            peers,
            replica: None,
        }
    }

    /// Build a raft replica-mode config: this pod is replica `replica_index` of
    /// shard group `shard_index`. `peers` is indexed by replica id.
    pub fn replicated(shard_count: u32, placement: ReplicaPlacement, peers: Vec<String>) -> Self {
        let shard_count = shard_count.max(1);
        let replicas = placement.replicas_per_shard.max(1);
        Self {
            node_id: (placement.replica_index * shard_count + placement.shard_index) as usize,
            node_count: (shard_count * replicas) as usize,
            shard_count,
            peers,
            replica: Some(placement),
        }
    }

    /// Shard a key falls in: `crc32(key) % shard_count`.
    pub fn shard_for(&self, key: &str) -> u32 {
        crc32fast::hash(key.as_bytes()) % self.shard_count
    }

    /// Node that owns a shard: `shard % node_count`. In replica mode this is
    /// the ordinal of the shard's replica 0 (`ordinal = replica * S + shard`).
    pub fn owner_of_shard(&self, shard: u32) -> usize {
        if self.replica.is_some() {
            return (shard % self.shard_count) as usize;
        }
        (shard % self.node_count as u32) as usize
    }

    /// Whether this node owns `key` (in replica mode: whether its shard group
    /// replicates `key`).
    pub fn owns(&self, key: &str) -> bool {
        match self.replica {
            Some(p) => self.shard_for(key) == p.shard_index,
            None => self.owner_of_shard(self.shard_for(key)) == self.node_id,
        }
    }

    /// Shard ids this node owns.
    pub fn owned_shards(&self) -> Vec<u32> {
        if let Some(p) = self.replica {
            return vec![p.shard_index];
        }
        (0..self.shard_count)
            .filter(|s| self.owner_of_shard(*s) == self.node_id)
            .collect()
    }

    pub fn snapshot(&self) -> ClusterState {
        ClusterState {
            node_id: self.node_id,
            node_count: self.node_count,
            shard_count: self.shard_count,
            owned_shards: self.owned_shards().len(),
            peers: self.peers.clone(),
            mode: if self.replica.is_some() {
                "raft"
            } else if self.node_count > 1 {
                "sharded"
            } else {
                "single"
            }
            .to_string(),
            shard_index: self.replica.map(|p| p.shard_index),
            replica_index: self.replica.map(|p| p.replica_index),
            replicas_per_shard: self.replica.map_or(1, |p| p.replicas_per_shard),
            voter_count: self.replica.map_or(1, |p| p.voter_count),
        }
    }
}

/// Public cluster-state view (served at `/cluster`).
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct ClusterState {
    pub node_id: usize,
    pub node_count: usize,
    pub shard_count: u32,
    pub owned_shards: usize,
    pub peers: Vec<String>,
    /// `single`, `sharded` (phase A, no replication) or `raft` (replicated
    /// shard groups).
    pub mode: String,
    /// This pod's shard group (raft mode only).
    pub shard_index: Option<u32>,
    /// This pod's raft id inside its shard group (raft mode only).
    pub replica_index: Option<u32>,
    pub replicas_per_shard: u32,
    /// Voting replicas per shard; replicas beyond it are learners.
    pub voter_count: u32,
}

/// Shared cluster handle in [`crate::http::AppState`].
pub type Cluster = Arc<ClusterConfig>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn single_node_owns_everything() {
        let c = ClusterConfig::default();
        assert!(c.owns("anything"));
        assert_eq!(c.owned_shards(), vec![0]);
    }

    #[test]
    fn sharding_partitions_keys_across_nodes() {
        // 3 nodes, 12 shards. Each node owns a disjoint subset; union = all keys.
        let nodes: Vec<ClusterConfig> = (0..3)
            .map(|i| ClusterConfig::new(i, 3, 12, vec![]))
            .collect();
        for k in ["a", "b", "user:1", "result:job-42", "x"] {
            let owners: Vec<bool> = nodes.iter().map(|n| n.owns(k)).collect();
            assert_eq!(
                owners.iter().filter(|&&o| o).count(),
                1,
                "key {k} must have exactly one owner"
            );
        }
        // Ownership is stable + disjoint.
        let total: usize = nodes.iter().map(|n| n.owned_shards().len()).sum();
        assert_eq!(total, 12);
    }

    #[test]
    fn shard_count_floored_to_node_count() {
        let c = ClusterConfig::new(0, 5, 2, vec![]);
        assert!(c.shard_count >= 5);
    }

    #[test]
    fn shared_topology_preserves_shard_count_below_replica_count() {
        let c = ClusterConfig::from_shared_topology(2, 3, 1, vec![]);
        assert_eq!(c.node_id, 2);
        assert_eq!(c.node_count, 3);
        assert_eq!(c.shard_count, 1);
    }

    #[test]
    fn replicated_pod_owns_only_its_shard_group() {
        // S=2, R=3: ordinal 5 = replica 2 of shard 1.
        let place = |shard, replica| ReplicaPlacement {
            shard_index: shard,
            replica_index: replica,
            replicas_per_shard: 3,
            voter_count: 2,
        };
        let c = ClusterConfig::replicated(2, place(1, 2), vec![]);
        assert_eq!(c.node_id, 5);
        assert_eq!(c.node_count, 6);
        assert_eq!(c.owned_shards(), vec![1]);
        assert!(
            !c.replica.unwrap().is_voter(),
            "replica 2 >= V=2 is a learner"
        );
        assert_eq!(c.owner_of_shard(1), 1, "replica 0 of shard 1 is ordinal 1");
        // Every replica of a shard group owns the same keys; groups are disjoint.
        for k in ["a", "b", "user:1", "result:job-42", "x"] {
            let groups: Vec<bool> = (0..2)
                .map(|s| ClusterConfig::replicated(2, place(s, 0), vec![]).owns(k))
                .collect();
            assert_eq!(groups.iter().filter(|&&o| o).count(), 1, "key {k}");
            let s = c.shard_for(k);
            for r in 0..3 {
                assert!(ClusterConfig::replicated(2, place(s, r), vec![]).owns(k));
            }
        }
        let st = c.snapshot();
        assert_eq!(st.mode, "raft");
        assert_eq!(st.shard_index, Some(1));
        assert_eq!(st.replica_index, Some(2));
        assert_eq!(st.voter_count, 2);
    }
}
