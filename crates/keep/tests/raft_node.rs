//! raft-runtime-backed consensus (HA phase C). Only compiles/runs with
//! `--features raft`. Proves a write goes through the shared [`RaftHost`]
//! (propose → commit → apply) onto the engine with read-your-write, that the
//! per-shard hosts route by key, and that a shard's snapshot round-trips
//! (`dump_values`/`load_values`).
//!
//! Multi-replica failover (SIGKILL, SIGSTOP, SIGTERM handoff, InstallSnapshot
//! catch-up, uncommitted-tail truncation) runs against real processes in
//! `raft_cluster_process.rs`.

#![cfg(feature = "raft")]

use std::sync::Arc;

use keep::apply::ApplyOutcome;
use keep::persistence::format::WalOp;
use keep::raft::{KvStateMachine, ShardHosts};
use keep::{ClusterConfig, KvEngine, KvKey, KvValue};
use raft_runtime::RaftStateMachine;

/// A fresh, empty temp dir for a test's raft hard-state stores.
fn tmp_dir(tag: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("keep-raft-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// Single node owning every shard, one sole-voter host per shard: a SET
/// proposed through its shard host commits + applies, and the engine reflects it
/// the moment `propose` returns (read-your-write).
#[tokio::test]
async fn single_node_per_shard_read_your_write() {
    let dir = tmp_dir("ryw");
    let engine = Arc::new(KvEngine::with_shards(4));
    let cluster = ClusterConfig::new(0, 1, 1, vec![]);
    let hosts = ShardHosts::new(cluster, engine.clone(), &dir)
        .await
        .unwrap();
    assert!(hosts.host_count() >= 1, "one host per owned shard");

    let outcome = hosts
        .write(
            "rk",
            WalOp::Set {
                key: "rk".to_string(),
                value: KvValue::String("via-raft".to_string()),
                ttl: None,
            },
        )
        .await
        .unwrap();
    assert_eq!(
        outcome.unwrap(),
        ApplyOutcome::Done,
        "propose returns the replicated apply outcome"
    );
    assert_eq!(
        engine.get(&KvKey::new("rk").unwrap()),
        Some(KvValue::String("via-raft".to_string())),
        "raft-committed write is applied to the engine before propose returns"
    );

    // A delete routes to the same shard host, applies, and reports that the key
    // existed; a second delete reports that it did not.
    let delete = || WalOp::Delete {
        key: "rk".to_string(),
    };
    let first = hosts.write("rk", delete()).await.unwrap().unwrap();
    assert_eq!(first, ApplyOutcome::Bool(true));
    let second = hosts.write("rk", delete()).await.unwrap().unwrap();
    assert_eq!(second, ApplyOutcome::Bool(false));
    assert_eq!(engine.get(&KvKey::new("rk").unwrap()), None);

    let _ = std::fs::remove_dir_all(&dir);
}

/// A single node owning every shard, but as one host per shard: writes route to
/// their key's shard host and every committed write lands in the engine.
#[tokio::test]
async fn per_shard_hosts_route_writes_by_key() {
    let dir = tmp_dir("route");
    let engine = Arc::new(KvEngine::with_shards(4));
    // 8 virtual shards on one node ⇒ up to 8 per-shard hosts.
    let cluster = ClusterConfig::new(0, 1, 8, vec![]);
    let hosts = ShardHosts::new(cluster, engine.clone(), &dir)
        .await
        .unwrap();
    assert!(hosts.host_count() >= 1, "one host per owned shard");

    for i in 0..20 {
        let key = format!("key-{i}");
        hosts
            .write(
                &key,
                WalOp::Set {
                    key: key.clone(),
                    value: KvValue::String(format!("v{i}")),
                    ttl: None,
                },
            )
            .await
            .unwrap()
            .unwrap();
    }
    for i in 0..20 {
        let key = format!("key-{i}");
        assert_eq!(
            engine.get(&KvKey::new(key).unwrap()),
            Some(KvValue::String(format!("v{i}"))),
            "key-{i} committed through its shard host + applied"
        );
    }

    let _ = std::fs::remove_dir_all(&dir);
}

/// A shard's state machine snapshot keeps only that shard's keys and round-trips
/// through `restore` into a fresh engine, restoring `applied_index` — the bytes
/// the host ships via InstallSnapshot for catch-up.
#[tokio::test]
async fn shard_state_machine_snapshot_round_trips() {
    let cluster = ClusterConfig::new(0, 1, 8, vec![]);
    let src = Arc::new(KvEngine::with_shards(4));
    // Seed values across the keyspace; only shard-0 keys belong in shard 0's snapshot.
    for i in 0..50 {
        let k = format!("snap-{i}");
        let _ = src.set(
            &KvKey::new(&k).unwrap(),
            KvValue::String(format!("v{i}")),
            None,
        );
    }
    let sm = KvStateMachine::new(src.clone(), cluster.clone(), 0);
    // Advance the applied head, then capture the snapshot.
    let cmd = serde_json::to_vec(&WalOp::Set {
        key: "applied-marker".to_string(),
        value: KvValue::String("m".to_string()),
        ttl: None,
    })
    .unwrap();
    sm.apply(7, &cmd).unwrap();
    assert_eq!(sm.applied_index(), 7);
    let mut snap = Vec::new();
    sm.snapshot(&mut snap).unwrap();

    // Restore into a fresh engine through a fresh state machine.
    let dst = Arc::new(KvEngine::with_shards(4));
    let sm2 = KvStateMachine::new(dst.clone(), cluster.clone(), 0);
    sm2.restore(&mut std::io::Cursor::new(&snap)).unwrap();
    assert_eq!(
        sm2.applied_index(),
        7,
        "restore sets applied_index to the snapshot head"
    );
    for i in 0..50 {
        let k = format!("snap-{i}");
        let got = dst.get(&KvKey::new(&k).unwrap());
        if cluster.shard_for(&k) == 0 {
            assert_eq!(
                got,
                Some(KvValue::String(format!("v{i}"))),
                "shard-0 key {k} restored"
            );
        } else {
            assert_eq!(
                got, None,
                "off-shard key {k} excluded from shard-0 snapshot"
            );
        }
    }
}
