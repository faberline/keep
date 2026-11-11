//! Judging logic of the competitor ratchet gate (issue #2): keep vs RESP KV servers.
//!
//! Every ratio is oriented so higher is better for keep: throughput is
//! `keep / peer`, p99 latency is `peer / keep`. A `ratchet` cell fails when
//! `ratio < max(min_ratio, ratchet × baseline)`; a `report` cell never fails.
//! The gate fails closed on malformed cells.

#![allow(dead_code)] // the report-check and live-runner slices (keep#35, #37) use the rest of these items

use serde_json::{json, Value};
use std::collections::BTreeMap;

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

/// One measured cell: the median-of-trials ratio and keep's own trial-to-trial
/// variation on that metric.
#[derive(Debug, Clone)]
struct Measured {
    ratio: f64,
    keep_cv: f64,
    detail: String,
}

#[derive(Default, Debug)]
struct Outcome {
    lines: Vec<String>,
    /// Ratchet cells below threshold with a stable keep measurement.
    regressions: Vec<String>,
    /// Ratchet cells below threshold while keep itself was noisy (CV > max_cv):
    /// still a failure, reported apart so a rerun is the first response.
    unstable: Vec<String>,
    /// Anything that makes the verdict untrustworthy (fail closed).
    errors: Vec<String>,
}

impl Outcome {
    fn passed(&self) -> bool {
        self.regressions.is_empty() && self.unstable.is_empty() && self.errors.is_empty()
    }
}

fn evaluate(baseline: &Value, host: &str, measured: &BTreeMap<String, Measured>) -> Outcome {
    let mut out = Outcome::default();
    let policy = match Policy::from_baseline(baseline) {
        Ok(p) => p,
        Err(e) => {
            out.errors.push(e);
            return out;
        }
    };
    let Some(cells) = baseline["hosts"][host]["cells"].as_object() else {
        out.errors.push(format!(
            "no baseline for host class `{host}`; record one on this host with KEEP_PERF_RECORD=1"
        ));
        return out;
    };
    for (key, cell) in cells {
        let metric = match parse_cell_key(key) {
            Ok(id) => id.metric,
            Err(e) => {
                out.errors.push(e);
                continue;
            }
        };
        let Some(m) = measured.get(key) else {
            if cell["gate"] == "report" {
                out.lines
                    .push(format!("{key:<34} {:>8} {:>8}  UNAVAILABLE", "-", "-"));
            } else {
                out.errors
                    .push(format!("{key}: baseline cell was not measured"));
            }
            continue;
        };
        if !(m.ratio.is_finite() && m.ratio > 0.0) {
            out.errors.push(format!(
                "{key}: non-finite ratio {} ({})",
                m.ratio, m.detail
            ));
            continue;
        }
        let unstable = m.keep_cv > policy.max_cv;
        let flag = if unstable {
            format!("  UNSTABLE keep cv={:.1}%", m.keep_cv * 100.0)
        } else {
            String::new()
        };
        match judge(cell, metric, m.ratio, &policy) {
            Ok(Verdict::Pass { threshold }) => out.lines.push(format!(
                "{key:<34} {:>8.3} {threshold:>8.3}  ok{flag}",
                m.ratio
            )),
            Ok(Verdict::Fail { threshold }) => {
                out.lines.push(format!(
                    "{key:<34} {:>8.3} {threshold:>8.3}  FAIL{flag}",
                    m.ratio
                ));
                let msg = format!(
                    "{key}: ratio {:.3} < threshold {threshold:.3} ({})",
                    m.ratio, m.detail
                );
                if unstable {
                    out.unstable
                        .push(format!("{msg}; keep cv {:.1}%", m.keep_cv * 100.0));
                } else {
                    out.regressions.push(msg);
                }
            }
            Ok(Verdict::Report) => out.lines.push(format!(
                "{key:<34} {:>8.3} {:>8}  report{flag}",
                m.ratio, "-"
            )),
            Err(e) => out.errors.push(format!("{key}: {e}")),
        }
    }
    for (key, m) in measured.iter().filter(|(k, _)| !cells.contains_key(*k)) {
        out.lines.push(format!(
            "{key:<34} {:>8.3} {:>8}  NEW (not in baseline)",
            m.ratio, "-"
        ));
    }
    out
}

/// Rounds down to 4 decimals (the epsilon absorbs `0.7 * 0.8 = 0.5599…`).
fn floor4(x: f64) -> f64 {
    (x * 10_000.0 + 1e-6).floor() / 10_000.0
}

/// The next baseline after recording `measured` for `host`. Raise-only: an
/// existing cell's baseline only moves up and its `min_ratio` never moves,
/// unless `force_note` is given (then both are reset from this measurement
/// and the note is stored). New cells get `min_ratio = factor × measured`.
fn record(
    baseline: &Value,
    host: &str,
    measured: &BTreeMap<String, Measured>,
    force_note: Option<&str>,
    date: &str,
) -> Result<Value, String> {
    let policy = Policy::from_baseline(baseline)?;
    let mut next = baseline.clone();
    if !next["hosts"].is_object() {
        next["hosts"] = json!({});
    }
    let entry = next["hosts"]
        .as_object_mut()
        .expect("hosts is an object")
        .entry(host.to_string())
        .or_insert_with(|| json!({ "cores": [], "cells": {} }));
    entry["recorded"] = json!(date);
    let mut cores = host_cores(entry);
    for key in measured.keys() {
        let c = parse_cell_key(key)?.cores;
        if !cores.contains(&c) {
            cores.push(c);
        }
    }
    cores.sort_unstable();
    entry["cores"] = json!(cores);
    if let Some(note) = force_note {
        entry["note"] = json!(note);
    }
    let cells = entry["cells"]
        .as_object_mut()
        .ok_or_else(|| format!("hosts.{host}.cells is not an object"))?;
    for (key, m) in measured {
        let id = parse_cell_key(key)?;
        let value = floor4(m.ratio);
        if !(value.is_finite() && value > 0.0) {
            return Err(format!("{key}: cannot record ratio {}", m.ratio));
        }
        let min_ratio = floor4(value * policy.floor(id.metric)).max(0.0001);
        match cells.get_mut(key) {
            None => {
                cells.insert(
                    key.clone(),
                    json!({ "gate": role_for(id.peer), "baseline": value, "min_ratio": min_ratio }),
                );
            }
            Some(cell) if force_note.is_some() => {
                cell["baseline"] = json!(value);
                cell["min_ratio"] = json!(min_ratio);
            }
            Some(cell) => {
                if value > positive(cell, "baseline").map_err(|e| format!("{key}: {e}"))? {
                    cell["baseline"] = json!(value);
                }
            }
        }
    }
    Ok(next)
}

/// The server core counts a host class is gated at (`cores` in its entry;
/// hosts too small for the 4-core cells record `[1]`).
fn host_cores(entry: &Value) -> Vec<u32> {
    match entry["cores"].as_array() {
        Some(cs) => cs
            .iter()
            .filter_map(|c| c.as_u64().and_then(|c| u32::try_from(c).ok()))
            .collect(),
        None => CORES.to_vec(),
    }
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

fn m(ratio: f64) -> Measured {
    Measured {
        ratio,
        keep_cv: 0.01,
        detail: "synthetic".into(),
    }
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

#[test]
fn evaluate_passes_when_every_cell_holds() {
    let baseline = synthetic_baseline(json!({
        "ram/1c/SET/ops/redis": { "gate": "ratchet", "baseline": 1.0, "min_ratio": 0.8 },
        "ram/1c/SET/ops/keydb": { "gate": "report", "baseline": 1.0, "min_ratio": 0.8 }
    }));
    let measured = BTreeMap::from([
        ("ram/1c/SET/ops/redis".to_string(), m(0.9)),
        ("ram/1c/SET/ops/keydb".to_string(), m(0.1)),
        ("ram/1c/GET/ops/garnet".to_string(), m(2.0)),
    ]);
    let out = evaluate(&baseline, "h", &measured);
    assert!(out.passed(), "{out:?}");
    assert!(out.lines.iter().any(|l| l.contains("NEW")));
}

#[test]
fn evaluate_fails_closed() {
    let cells = json!({
        "ram/1c/SET/ops/redis": { "gate": "ratchet", "baseline": 1.0, "min_ratio": 0.8 },
        "ram/1c/GET/ops/garnet": { "gate": "report", "baseline": 1.0, "min_ratio": 0.8 }
    });
    let baseline = synthetic_baseline(cells);

    // No baseline for this host class.
    let out = evaluate(&baseline, "other-host", &BTreeMap::new());
    assert!(!out.passed() && out.errors[0].contains("other-host"));

    // A ratchet cell that was not measured fails; an unmeasured report cell does not.
    let out = evaluate(&baseline, "h", &BTreeMap::new());
    assert_eq!(out.errors.len(), 1, "{out:?}");
    assert!(out.errors[0].contains("not measured"));
    assert!(out.lines.iter().any(|l| l.contains("UNAVAILABLE")));

    // A non-finite ratio fails.
    let nan = BTreeMap::from([("ram/1c/SET/ops/redis".to_string(), m(f64::NAN))]);
    assert!(!evaluate(&baseline, "h", &nan).errors.is_empty());

    // A regression fails; with a noisy keep it is reported as UNSTABLE instead.
    let low = BTreeMap::from([("ram/1c/SET/ops/redis".to_string(), m(0.5))]);
    let out = evaluate(&baseline, "h", &low);
    assert_eq!(out.regressions.len(), 1);
    let mut noisy = m(0.5);
    noisy.keep_cv = 0.25;
    let out = evaluate(
        &baseline,
        "h",
        &BTreeMap::from([("ram/1c/SET/ops/redis".to_string(), noisy)]),
    );
    assert!(out.regressions.is_empty() && out.unstable.len() == 1 && !out.passed());

    // A broken policy fails everything.
    let mut broken = baseline.clone();
    broken["policy"]["max_cv"] = json!(null);
    assert!(!evaluate(&broken, "h", &low).errors.is_empty());
}

#[test]
fn record_is_raise_only_unless_forced() {
    let baseline = synthetic_baseline(json!({
        "ram/1c/SET/ops/redis": { "gate": "ratchet", "baseline": 1.0, "min_ratio": 0.8 }
    }));
    let key = "ram/1c/SET/ops/redis".to_string();

    let lower = BTreeMap::from([(key.clone(), m(0.7))]);
    let next = record(&baseline, "h", &lower, None, "2026-10-22").unwrap();
    assert_eq!(next["hosts"]["h"]["cells"][&key]["baseline"], json!(1.0));
    assert_eq!(next["hosts"]["h"]["cells"][&key]["min_ratio"], json!(0.8));

    let higher = BTreeMap::from([(key.clone(), m(1.23456))]);
    let next = record(&baseline, "h", &higher, None, "2026-10-22").unwrap();
    assert_eq!(next["hosts"]["h"]["cells"][&key]["baseline"], json!(1.2345));
    assert_eq!(next["hosts"]["h"]["cells"][&key]["min_ratio"], json!(0.8));
    assert_eq!(next["hosts"]["h"]["recorded"], json!("2026-10-22"));
    assert_eq!(next["hosts"]["h"]["cores"], json!([1, 4]));

    let next = record(&baseline, "h", &lower, Some("new host image"), "2026-10-22").unwrap();
    assert_eq!(next["hosts"]["h"]["cells"][&key]["baseline"], json!(0.7));
    assert_eq!(next["hosts"]["h"]["cells"][&key]["min_ratio"], json!(0.56));
    assert_eq!(next["hosts"]["h"]["note"], json!("new host image"));
}

#[test]
fn record_creates_hosts_and_cells_with_their_role() {
    let mut baseline = synthetic_baseline(json!({}));
    baseline["hosts"] = json!({});
    let measured = BTreeMap::from([
        ("tiered/4c/CAS/p99/dragonfly".to_string(), m(2.0)),
        ("tiered/4c/CAS/ops/garnet".to_string(), m(0.5)),
        ("ram/4c/SET/ops/keep-1c".to_string(), m(3.0)),
    ]);
    let next = record(&baseline, "new", &measured, None, "2026-10-22").unwrap();
    assert_eq!(next["hosts"]["new"]["cores"], json!([4]));
    let cells = &next["hosts"]["new"]["cells"];
    assert_eq!(
        cells["tiered/4c/CAS/p99/dragonfly"],
        json!({ "gate": "ratchet", "baseline": 2.0, "min_ratio": 1.2 })
    );
    assert_eq!(cells["tiered/4c/CAS/ops/garnet"]["gate"], json!("report"));
    assert_eq!(cells["ram/4c/SET/ops/keep-1c"]["gate"], json!("report"));
    // A freshly recorded host passes its own measurement.
    assert!(evaluate(&next, "new", &measured).passed());

    let bad = BTreeMap::from([("ram/1c/SET/ops/redis".to_string(), m(f64::INFINITY))]);
    assert!(record(&baseline, "h", &bad, None, "d").is_err());
}
