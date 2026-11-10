//! Judging logic of the competitor ratchet gate (issue #2): keep vs RESP KV servers.
//!
//! Every ratio is oriented so higher is better for keep: throughput is
//! `keep / peer`, p99 latency is `peer / keep`. A `ratchet` cell fails when
//! `ratio < max(min_ratio, ratchet × baseline)`; a `report` cell never fails.
//! The gate fails closed on malformed cells.

#![allow(dead_code)] // the evaluate/record/live-runner slices (keep#34, #35, #37) use the rest of these items

use serde_json::{json, Value};

const BASELINE_PATH: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/perf-baseline.json");
const BASELINE_SCHEMA: &str = "keep.perf_baseline/1";
const REPORT_SCHEMA: &str = "keep.bench_compare/1";
const TIERS: [&str; 2] = ["ram", "tiered"];
const CORES: [u32; 2] = [1, 4];
const PHASES: [&str; 3] = ["SET", "GET", "CAS"];
const TARGETS: [&str; 5] = ["keep", "redis", "dragonfly", "keydb", "garnet"];
/// Peers whose cells are hard ratchets.
const RATCHET_PEERS: [&str; 2] = ["redis", "dragonfly"];
/// keep 4-core vs keep 1-core throughput (report-only scaling cells).
const SCALING_PEER: &str = "keep-1c";
const SERVER_NAME: &str = "keep-perf-srv";

// ===========================================================================
// Judging logic (pure)
// ===========================================================================

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Metric {
    Ops,
    P99,
}

impl Metric {
    fn name(self) -> &'static str {
        match self {
            Self::Ops => "ops",
            Self::P99 => "p99",
        }
    }
}

fn cell_key(tier: &str, cores: u32, phase: &str, metric: Metric, peer: &str) -> String {
    format!("{tier}/{cores}c/{phase}/{}/{peer}", metric.name())
}

#[derive(Debug, PartialEq)]
struct CellId<'a> {
    tier: &'a str,
    cores: u32,
    phase: &'a str,
    metric: Metric,
    peer: &'a str,
}

fn parse_cell_key(key: &str) -> Result<CellId<'_>, String> {
    let bad = || format!("malformed cell key `{key}` (want tier/Nc/PHASE/metric/peer)");
    let parts: Vec<&str> = key.split('/').collect();
    let [tier, cores, phase, metric, peer] = parts[..] else {
        return Err(bad());
    };
    let cores = cores
        .strip_suffix('c')
        .and_then(|c| c.parse().ok())
        .ok_or_else(bad)?;
    let metric = match metric {
        "ops" => Metric::Ops,
        "p99" => Metric::P99,
        _ => return Err(bad()),
    };
    let peer_ok = TARGETS[1..].contains(&peer) || peer == SCALING_PEER;
    if !TIERS.contains(&tier) || !PHASES.contains(&phase) || !peer_ok {
        return Err(bad());
    }
    Ok(CellId {
        tier,
        cores,
        phase,
        metric,
        peer,
    })
}

/// The gate a cell for `peer` gets when it is first recorded.
fn role_for(peer: &str) -> &'static str {
    if RATCHET_PEERS.contains(&peer) {
        "ratchet"
    } else {
        "report"
    }
}

/// keep-vs-peer ratio, oriented so higher is always better for keep.
fn ratio(metric: Metric, keep: f64, peer: f64) -> Option<f64> {
    let r = match metric {
        Metric::Ops => keep / peer,
        Metric::P99 => peer / keep,
    };
    (r.is_finite() && r > 0.0).then_some(r)
}

#[derive(Debug, Clone, Copy)]
struct Policy {
    throughput_ratchet: f64,
    p99_ratchet: f64,
    max_cv: f64,
    ops_floor: f64,
    p99_floor: f64,
}

impl Policy {
    fn from_baseline(baseline: &Value) -> Result<Self, String> {
        let p = &baseline["policy"];
        let get = |k: &str| {
            p[k].as_f64()
                .filter(|v| v.is_finite() && *v > 0.0)
                .ok_or_else(|| format!("baseline policy.{k} missing or not a positive number"))
        };
        Ok(Self {
            throughput_ratchet: get("throughput_ratchet")?,
            p99_ratchet: get("p99_ratchet")?,
            max_cv: get("max_cv")?,
            ops_floor: get("ops_min_ratio_factor")?,
            p99_floor: get("p99_min_ratio_factor")?,
        })
    }
    fn ratchet(&self, metric: Metric) -> f64 {
        match metric {
            Metric::Ops => self.throughput_ratchet,
            Metric::P99 => self.p99_ratchet,
        }
    }
    fn floor(&self, metric: Metric) -> f64 {
        match metric {
            Metric::Ops => self.ops_floor,
            Metric::P99 => self.p99_floor,
        }
    }
}

fn positive(cell: &Value, k: &str) -> Result<f64, String> {
    cell[k]
        .as_f64()
        .filter(|v| v.is_finite() && *v > 0.0)
        .ok_or_else(|| format!("`{k}` missing or not a positive number"))
}

#[derive(Debug, PartialEq)]
enum Verdict {
    Pass { threshold: f64 },
    Fail { threshold: f64 },
    Report,
}

fn judge(cell: &Value, metric: Metric, ratio: f64, policy: &Policy) -> Result<Verdict, String> {
    match cell["gate"].as_str() {
        Some("ratchet") => {
            let baseline = positive(cell, "baseline")?;
            let min_ratio = positive(cell, "min_ratio")?;
            let threshold = min_ratio.max(policy.ratchet(metric) * baseline);
            Ok(if ratio + 1e-9 >= threshold {
                Verdict::Pass { threshold }
            } else {
                Verdict::Fail { threshold }
            })
        }
        Some("report") => Ok(Verdict::Report),
        other => Err(format!("unknown gate {other:?} (want ratchet|report)")),
    }
}

fn median(xs: &[f64]) -> f64 {
    if xs.is_empty() {
        return f64::NAN;
    }
    let mut v = xs.to_vec();
    v.sort_by(f64::total_cmp);
    let m = v.len() / 2;
    if v.len() % 2 == 1 {
        v[m]
    } else {
        (v[m - 1] + v[m]) / 2.0
    }
}

/// Coefficient of variation (population stddev / mean); 0 for < 2 samples.
fn cv(xs: &[f64]) -> f64 {
    if xs.len() < 2 {
        return 0.0;
    }
    let mean = xs.iter().sum::<f64>() / xs.len() as f64;
    let var = xs.iter().map(|x| (x - mean).powi(2)).sum::<f64>() / xs.len() as f64;
    var.sqrt() / mean
}

// ===========================================================================
// Unit tests (synthetic data)
// ===========================================================================

fn synthetic_baseline(cells: Value) -> Value {
    json!({
        "schema": BASELINE_SCHEMA,
        "policy": {
            "throughput_ratchet": 0.85,
            "p99_ratchet": 0.70,
            "max_cv": 0.10,
            "ops_min_ratio_factor": 0.8,
            "p99_min_ratio_factor": 0.6
        },
        "images": {
            "redis": "redis:8.4@sha256:00",
            "dragonfly": "dragonfly@sha256:00",
            "keydb": "keydb@sha256:00",
            "garnet": "garnet@sha256:00"
        },
        "hosts": { "h": { "recorded": "2026-01-01", "cells": cells } }
    })
}

fn policy() -> Policy {
    Policy::from_baseline(&synthetic_baseline(json!({}))).unwrap()
}

#[test]
fn ratio_is_oriented_higher_is_better_for_keep() {
    assert_eq!(ratio(Metric::Ops, 200.0, 100.0), Some(2.0));
    assert_eq!(ratio(Metric::P99, 50.0, 100.0), Some(2.0));
    assert_eq!(ratio(Metric::Ops, 0.0, 100.0), None);
    assert_eq!(ratio(Metric::Ops, 100.0, 0.0), None);
    assert_eq!(ratio(Metric::P99, f64::NAN, 1.0), None);
}

#[test]
fn cell_keys_round_trip_and_reject_garbage() {
    let key = cell_key("tiered", 4, "CAS", Metric::P99, "redis");
    assert_eq!(key, "tiered/4c/CAS/p99/redis");
    assert_eq!(
        parse_cell_key(&key).unwrap(),
        CellId {
            tier: "tiered",
            cores: 4,
            phase: "CAS",
            metric: Metric::P99,
            peer: "redis"
        }
    );
    for bad in [
        "ram/1c/SET/ops",
        "ram/1/SET/ops/redis",
        "disk/1c/SET/ops/redis",
        "ram/1c/DEL/ops/redis",
        "ram/1c/SET/p50/redis",
        "ram/1c/SET/ops/memcached",
        "ram/1c/SET/ops/keep",
    ] {
        assert!(parse_cell_key(bad).is_err(), "{bad} should be rejected");
    }
}

#[test]
fn judge_uses_max_of_min_ratio_and_ratcheted_baseline() {
    let p = policy();
    let cell = json!({ "gate": "ratchet", "baseline": 1.0, "min_ratio": 0.5 });
    assert_eq!(
        judge(&cell, Metric::Ops, 0.86, &p).unwrap(),
        Verdict::Pass { threshold: 0.85 }
    );
    assert_eq!(
        judge(&cell, Metric::Ops, 0.84, &p).unwrap(),
        Verdict::Fail { threshold: 0.85 }
    );
    // p99 is ratcheted looser (0.70).
    assert!(matches!(
        judge(&cell, Metric::P99, 0.71, &p).unwrap(),
        Verdict::Pass { .. }
    ));
    // A committed min_ratio above the ratcheted baseline wins.
    let floor = json!({ "gate": "ratchet", "baseline": 1.0, "min_ratio": 0.95 });
    assert_eq!(
        judge(&floor, Metric::Ops, 0.9, &p).unwrap(),
        Verdict::Fail { threshold: 0.95 }
    );
    let report = json!({ "gate": "report", "baseline": 9.0, "min_ratio": 9.0 });
    assert_eq!(
        judge(&report, Metric::Ops, 0.01, &p).unwrap(),
        Verdict::Report
    );
}

#[test]
fn judge_fails_closed_on_malformed_cells() {
    let p = policy();
    for cell in [
        json!({ "gate": "win", "baseline": 1.0, "min_ratio": 0.5 }),
        json!({ "baseline": 1.0, "min_ratio": 0.5 }),
        json!({ "gate": "ratchet", "min_ratio": 0.5 }),
        json!({ "gate": "ratchet", "baseline": 1.0, "min_ratio": 0.0 }),
        json!({ "gate": "ratchet", "baseline": "1.0", "min_ratio": 0.5 }),
    ] {
        assert!(judge(&cell, Metric::Ops, 1.0, &p).is_err(), "{cell}");
    }
}

#[test]
fn median_and_cv() {
    assert_eq!(median(&[3.0, 1.0, 2.0]), 2.0);
    assert_eq!(median(&[4.0, 1.0, 2.0, 3.0]), 2.5);
    assert!(median(&[]).is_nan());
    assert_eq!(cv(&[5.0]), 0.0);
    assert_eq!(cv(&[10.0, 10.0, 10.0]), 0.0);
    assert!((cv(&[9.0, 11.0]) - 0.1).abs() < 1e-12);
}

#[test]
fn role_for_ratchets_only_the_hard_peers() {
    assert_eq!(role_for("redis"), "ratchet");
    assert_eq!(role_for("dragonfly"), "ratchet");
    assert_eq!(role_for("keydb"), "report");
    assert_eq!(role_for("garnet"), "report");
    assert_eq!(role_for("keep-1c"), "report");
}
