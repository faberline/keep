//! Pure scale logic for raft membership: which membership calls a
//! `replicasPerShard` change needs, and how many replicas per shard the
//! StatefulSet may have in this pass. No cluster and no network: the calls go
//! through [`MembershipClient`].

/// Port of the raft client endpoint, as `render.rs` `CLIENT_PORT`.
const PEER_PORT: u16 = 7117;

/// A refused membership call: the HTTP status and the stable error code.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refusal {
    /// The HTTP status of the refusal (a 4xx).
    pub status: u16,
    /// The stable error code, for example `not_caught_up`.
    pub code: String,
}

/// The membership admin API of keep (`/admin/raft/{shard}/members...`).
#[allow(async_fn_in_trait)]
pub trait MembershipClient {
    /// `POST /admin/raft/{shard}/members` with `{"node_id", "url"}`.
    async fn add_learner(&self, shard: u32, node_id: u64, url: &str) -> Result<(), Refusal>;
    /// `POST /admin/raft/{shard}/members/{node_id}/promote`.
    async fn promote(&self, shard: u32, node_id: u64) -> Result<(), Refusal>;
    /// `DELETE /admin/raft/{shard}/members/{node_id}`.
    async fn remove(&self, shard: u32, node_id: u64) -> Result<(), Refusal>;
}

/// The peer url of one replica of one shard: `http://<sts>-<ordinal>.<sts>-headless:7117/shard/<shard>`,
/// where the ordinal is `replica * shard_count + shard`.
pub fn peer_url(sts: &str, shard_count: u32, shard: u32, replica: u32) -> String {
    let ordinal = replica * shard_count + shard;
    format!("http://{sts}-{ordinal}.{sts}-headless:{PEER_PORT}/shard/{shard}")
}

/// The result of one [`drive`] pass.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScaleOutcome {
    /// Replicas per shard that are confirmed members after this pass.
    pub members_per_shard: u32,
    /// Replicas per shard the StatefulSet may have in this pass.
    pub replicas_per_shard_allowed: u32,
    /// One line for the refused call that stopped the pass: `shard <s> replica <r>: <code>`.
    pub pending: Vec<String>,
}

/// Make the membership calls that move the shards from `members_per_shard`
/// to `wanted_per_shard` replicas, one replica level at a time.
pub async fn drive<C: MembershipClient>(
    client: &C,
    sts: &str,
    shard_count: u32,
    members_per_shard: u32,
    wanted_per_shard: u32,
) -> ScaleOutcome {
    if wanted_per_shard > members_per_shard {
        scale_up(
            client,
            sts,
            shard_count,
            members_per_shard,
            wanted_per_shard,
        )
        .await
    } else if wanted_per_shard < members_per_shard {
        scale_down(client, shard_count, members_per_shard, wanted_per_shard).await
    } else {
        ScaleOutcome {
            members_per_shard,
            replicas_per_shard_allowed: wanted_per_shard,
            pending: Vec::new(),
        }
    }
}

/// Add a learner and promote it for every shard, one replica level at a time.
/// The pods must exist first, so the StatefulSet may already have `wanted`.
async fn scale_up<C: MembershipClient>(
    client: &C,
    sts: &str,
    shard_count: u32,
    members_per_shard: u32,
    wanted_per_shard: u32,
) -> ScaleOutcome {
    let mut members = members_per_shard;
    for replica in members_per_shard..wanted_per_shard {
        let node_id = u64::from(replica);
        for shard in 0..shard_count {
            let url = peer_url(sts, shard_count, shard, replica);
            let added = client.add_learner(shard, node_id, &url).await;
            if let Err(refusal) = added {
                if refusal.code != "transition_in_flight" {
                    return stopped(members, wanted_per_shard, shard, replica, &refusal.code);
                }
            }
            let promoted = client.promote(shard, node_id).await;
            if let Err(refusal) = promoted {
                if refusal.code != "transition_in_flight" {
                    return stopped(members, wanted_per_shard, shard, replica, &refusal.code);
                }
            }
        }
        members = replica + 1;
    }
    ScaleOutcome {
        members_per_shard: members,
        replicas_per_shard_allowed: wanted_per_shard,
        pending: Vec::new(),
    }
}

/// Remove the highest replica level first, for every shard. A pod is released
/// only after its member is gone, so the allowed value never drops below a
/// member that is still there.
async fn scale_down<C: MembershipClient>(
    client: &C,
    shard_count: u32,
    members_per_shard: u32,
    wanted_per_shard: u32,
) -> ScaleOutcome {
    let mut members = members_per_shard;
    for replica in (wanted_per_shard..members_per_shard).rev() {
        let node_id = u64::from(replica);
        for shard in 0..shard_count {
            if let Err(refusal) = client.remove(shard, node_id).await {
                if refusal.code != "unknown_node" {
                    let allowed = wanted_per_shard.max(members);
                    return ScaleOutcome {
                        members_per_shard: members,
                        replicas_per_shard_allowed: allowed,
                        pending: vec![format!("shard {shard} replica {replica}: {}", refusal.code)],
                    };
                }
            }
        }
        members = replica;
    }
    ScaleOutcome {
        members_per_shard: members,
        replicas_per_shard_allowed: wanted_per_shard.max(members),
        pending: Vec::new(),
    }
}

/// The outcome of a scale-up pass that a refused call stopped: the level that
/// is complete is kept, the StatefulSet keeps `wanted` pods, and the refusal
/// is one pending line for the next reconcile.
fn stopped(
    members: u32,
    wanted_per_shard: u32,
    shard: u32,
    replica: u32,
    code: &str,
) -> ScaleOutcome {
    ScaleOutcome {
        members_per_shard: members,
        replicas_per_shard_allowed: wanted_per_shard,
        pending: vec![format!("shard {shard} replica {replica}: {code}")],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::Mutex;

    /// A recording fake: each call is logged as `<op> <shard> <node> [url]`,
    /// and `refuse` maps `<op> <shard> <node>` to the refusal to return.
    struct Fake {
        calls: Mutex<Vec<String>>,
        refuse: HashMap<String, Refusal>,
    }

    impl Fake {
        fn new(refuse: &[(&str, u16, &str)]) -> Self {
            Self {
                calls: Mutex::new(Vec::new()),
                refuse: refuse
                    .iter()
                    .map(|(key, status, code)| {
                        (
                            key.to_string(),
                            Refusal {
                                status: *status,
                                code: code.to_string(),
                            },
                        )
                    })
                    .collect(),
            }
        }

        fn calls(&self) -> Vec<String> {
            self.calls.lock().expect("calls lock").clone()
        }

        fn answer(&self, line: String, key: String) -> Result<(), Refusal> {
            self.calls.lock().expect("calls lock").push(line);
            match self.refuse.get(&key) {
                Some(refusal) => Err(refusal.clone()),
                None => Ok(()),
            }
        }
    }

    impl MembershipClient for Fake {
        async fn add_learner(&self, shard: u32, node_id: u64, url: &str) -> Result<(), Refusal> {
            self.answer(
                format!("add {shard} {node_id} {url}"),
                format!("add {shard} {node_id}"),
            )
        }

        async fn promote(&self, shard: u32, node_id: u64) -> Result<(), Refusal> {
            self.answer(
                format!("promote {shard} {node_id}"),
                format!("promote {shard} {node_id}"),
            )
        }

        async fn remove(&self, shard: u32, node_id: u64) -> Result<(), Refusal> {
            self.answer(
                format!("remove {shard} {node_id}"),
                format!("remove {shard} {node_id}"),
            )
        }
    }

    fn outcome(members: u32, allowed: u32, pending: &[&str]) -> ScaleOutcome {
        ScaleOutcome {
            members_per_shard: members,
            replicas_per_shard_allowed: allowed,
            pending: pending.iter().map(|line| line.to_string()).collect(),
        }
    }

    #[test]
    fn peer_url_follows_the_pod_math() {
        // ordinal = replica * shard_count + shard = 2 * 3 + 1 = 7
        assert_eq!(peer_url("k", 3, 1, 2), "http://k-7.k-headless:7117/shard/1");
    }

    #[tokio::test]
    async fn scale_up_adds_then_promotes_each_shard() {
        let fake = Fake::new(&[]);
        let result = drive(&fake, "k", 2, 1, 2).await;
        assert_eq!(
            fake.calls(),
            vec![
                "add 0 1 http://k-2.k-headless:7117/shard/0",
                "promote 0 1",
                "add 1 1 http://k-3.k-headless:7117/shard/1",
                "promote 1 1",
            ]
        );
        assert_eq!(result, outcome(2, 2, &[]));
    }

    #[tokio::test]
    async fn scale_up_not_caught_up_stops_and_keeps_the_pods() {
        let fake = Fake::new(&[("promote 1 1", 409, "not_caught_up")]);
        let result = drive(&fake, "k", 2, 1, 2).await;
        assert_eq!(fake.calls().last().map(String::as_str), Some("promote 1 1"));
        assert_eq!(result, outcome(1, 2, &["shard 1 replica 1: not_caught_up"]));
    }

    #[tokio::test]
    async fn scale_up_treats_transition_in_flight_as_done() {
        let fake = Fake::new(&[("add 0 1", 409, "transition_in_flight")]);
        let result = drive(&fake, "k", 2, 1, 2).await;
        assert!(
            fake.calls().contains(&"promote 0 1".to_string()),
            "the pass must go on to promote after an add that is already in flight"
        );
        assert_eq!(result, outcome(2, 2, &[]));
    }

    #[tokio::test]
    async fn scale_down_removes_highest_replica_of_every_shard_first() {
        let fake = Fake::new(&[]);
        let result = drive(&fake, "k", 2, 3, 2).await;
        assert_eq!(fake.calls(), vec!["remove 0 2", "remove 1 2"]);
        assert_eq!(result, outcome(2, 2, &[]));
    }

    #[tokio::test]
    async fn scale_down_refusal_keeps_the_voter_pod() {
        let fake = Fake::new(&[("remove 0 2", 409, "would_lose_tolerance")]);
        let result = drive(&fake, "k", 2, 3, 2).await;
        assert_eq!(fake.calls(), vec!["remove 0 2"]);
        assert_eq!(
            result,
            outcome(3, 3, &["shard 0 replica 2: would_lose_tolerance"])
        );
    }

    #[tokio::test]
    async fn scale_down_unknown_node_counts_as_done() {
        let fake = Fake::new(&[("remove 1 2", 404, "unknown_node")]);
        let result = drive(&fake, "k", 2, 3, 2).await;
        assert_eq!(result, outcome(2, 2, &[]));
    }

    #[tokio::test]
    async fn equal_levels_make_no_call() {
        let fake = Fake::new(&[]);
        let result = drive(&fake, "k", 2, 2, 2).await;
        assert!(fake.calls().is_empty());
        assert_eq!(result, outcome(2, 2, &[]));
    }

    #[tokio::test]
    async fn scale_down_by_two_stops_at_the_first_failed_level() {
        let fake = Fake::new(&[("remove 0 2", 409, "is_leader")]);
        let result = drive(&fake, "k", 1, 4, 2).await;
        assert_eq!(fake.calls(), vec!["remove 0 3", "remove 0 2"]);
        assert_eq!(result, outcome(3, 3, &["shard 0 replica 2: is_leader"]));
    }
}
