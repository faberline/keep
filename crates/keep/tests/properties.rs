//! Property tests (proptest) for the parsers, the key type, the engine and the raft apply path.
use keep::backup::{read_snapshot_file, KeepSnapshot, KEEP_SNAPSHOT_VERSION};
use keep::persistence::recovery::RecoveryManager;
use keep::{KvEngine, KvKey, KvValue};
use proptest::prelude::*;
use std::collections::HashMap;

fn arb_value() -> impl Strategy<Value = KvValue> {
    prop_oneof![
        any::<i64>().prop_map(KvValue::Int),
        "[a-z0-9 ]{0,16}".prop_map(KvValue::String),
        proptest::collection::vec(any::<u8>(), 0..16).prop_map(KvValue::Bytes),
        Just(KvValue::Null),
    ]
}

fn arb_key() -> impl Strategy<Value = String> {
    "[a-d]{1,3}"
}

#[derive(Debug, Clone)]
enum Op {
    Set(String, KvValue),
    Delete(String),
}

fn arb_ops() -> impl Strategy<Value = Vec<Op>> {
    proptest::collection::vec(
        prop_oneof![
            (arb_key(), arb_value()).prop_map(|(k, v)| Op::Set(k, v)),
            arb_key().prop_map(Op::Delete),
        ],
        0..40,
    )
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    #[test]
    fn backup_reader_never_panics_on_any_bytes(
        bytes in proptest::collection::vec(any::<u8>(), 0..512)
    ) {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("backup-1.json");
        std::fs::write(&path, &bytes).unwrap();
        if let Ok(snapshot) = read_snapshot_file(&path) {
            prop_assert_eq!(snapshot.version, KEEP_SNAPSHOT_VERSION);
        }
    }

    #[test]
    fn backup_snapshot_round_trips(
        values in proptest::collection::vec((arb_key(), arb_value()), 0..20)
    ) {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("backup-1.json");
        let snapshot = KeepSnapshot { version: KEEP_SNAPSHOT_VERSION, values };
        std::fs::write(&path, serde_json::to_vec(&snapshot).unwrap()).unwrap();
        let back = read_snapshot_file(&path).unwrap();
        prop_assert_eq!(back, snapshot);
    }

    #[test]
    fn recover_on_random_snapshot_bytes_returns_result_and_keeps_file(
        bytes in proptest::collection::vec(any::<u8>(), 0..512)
    ) {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("snapshot-1.snap");
        std::fs::write(&path, &bytes).unwrap();
        let _ = RecoveryManager::recover(tmp.path(), 4);
        prop_assert_eq!(std::fs::read(&path).unwrap(), bytes);
    }

    #[test]
    fn valid_keys_round_trip(key in "[a-zA-Z0-9:_-]{1,256}") {
        let k = KvKey::new(key.clone()).unwrap();
        prop_assert_eq!(k.as_str(), key.as_str());
    }

    #[test]
    fn invalid_keys_return_err(key in prop_oneof![Just(String::new()), "[a-z]{257,300}"]) {
        prop_assert!(KvKey::new(key).is_err());
    }

    #[test]
    fn engine_matches_hashmap_model(ops in arb_ops()) {
        let engine = KvEngine::with_shards(4);
        let mut model: HashMap<String, KvValue> = HashMap::new();
        for op in &ops {
            match op {
                Op::Set(k, v) => {
                    engine.set(&KvKey::new(k.clone()).unwrap(), v.clone(), None).unwrap();
                    model.insert(k.clone(), v.clone());
                }
                Op::Delete(k) => {
                    engine.delete(&KvKey::new(k.clone()).unwrap());
                    model.remove(k);
                }
            }
        }
        let fixed = ["a", "b", "c", "d", "aa", "ab", "abc"].map(|s| s.to_string());
        for k in fixed.iter().cloned().chain(model.keys().cloned()) {
            prop_assert_eq!(
                engine.get(&KvKey::new(k.clone()).unwrap()),
                model.get(&k).cloned()
            );
        }
    }
}

#[cfg(feature = "raft")]
mod raft_apply {
    use super::*;
    use keep::persistence::format::WalOp;
    use keep::raft::KvStateMachine;
    use keep::ClusterConfig;
    use raft_runtime::RaftStateMachine;
    use std::sync::Arc;

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(256))]

        #[test]
        fn raft_apply_matches_engine_alone(ops in arb_ops()) {
            let engine_a = KvEngine::with_shards(4);
            let engine_b = Arc::new(KvEngine::with_shards(4));
            let sm = KvStateMachine::new(engine_b.clone(), ClusterConfig::new(0, 1, 1, vec![]), 0);
            for (i, op) in ops.iter().enumerate() {
                let wal = match op {
                    Op::Set(k, v) => {
                        engine_a.set(&KvKey::new(k.clone()).unwrap(), v.clone(), None).unwrap();
                        WalOp::Set { key: k.clone(), value: v.clone(), ttl: None }
                    }
                    Op::Delete(k) => {
                        engine_a.delete(&KvKey::new(k.clone()).unwrap());
                        WalOp::Delete { key: k.clone() }
                    }
                };
                sm.apply((i + 1) as u64, &serde_json::to_vec(&wal).unwrap()).unwrap();
            }
            prop_assert_eq!(sm.applied_index(), ops.len() as u64);
            let mut a = engine_a.dump_values();
            a.sort_by(|x, y| x.0.cmp(&y.0));
            let mut b = engine_b.dump_values();
            b.sort_by(|x, y| x.0.cmp(&y.0));
            prop_assert_eq!(a, b);
        }
    }
}

#[test]
fn regression_truncated_backup_json_is_err_not_panic() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("backup-1.json");
    std::fs::write(&path, br#"{"version":1,"values":[["a""#).unwrap();
    assert!(read_snapshot_file(&path).is_err());
}

#[test]
fn regression_wrong_backup_version_is_err() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("backup-1.json");
    std::fs::write(&path, br#"{"version":2,"values":[]}"#).unwrap();
    let err = read_snapshot_file(&path).unwrap_err();
    assert!(format!("{err:?}").contains("unsupported snapshot version"));
}
