//! Competitive performance ratchet gate: keep vs RESP KV servers (issue #2).
//!
//! Two layers:
//!
//! * plain `#[test]`s — the judging logic (ratio direction, ratchet threshold,
//!   every fail-closed path, raise-only recording) on synthetic data, plus a
//!   structural check of the committed `tests/perf-baseline.json`;
//! * `#[ignore] competitive_perf_gate` — the live matrix. Each server runs as a
//!   Docker container (peer images pinned by digest in the baseline) on a cpuset
//!   disjoint from the load generator, which is `bench_compare --format json`
//!   in a second container. keep/peer ratios are judged against the baseline
//!   recorded for this host class.
//!
//! Every ratio is oriented so HIGHER IS BETTER FOR KEEP: throughput is
//! `keep / peer`, p99 latency is `peer / keep`. A `ratchet` cell fails when
//! `ratio < max(min_ratio, ratchet × baseline)` (policy in the baseline file);
//! a `report` cell (KeyDB, Garnet, keep's own 1→4 core scaling) is printed and
//! never fails. The gate fails closed: a container that does not start, an
//! image built for another architecture, a storage mode that does not match
//! the tier, any failed request, a baseline cell that was not measured, or a
//! host class with no baseline all fail the run.
//!
//! Run (release; ~20 min on a 10-core laptop):
//!
//! ```text
//! docker build -f Dockerfile.bench -t keep-bench:gate .
//! cargo test --release -p keep --test perf_gate_vs_kv competitive_perf_gate \
//!     -- --ignored --exact --nocapture --test-threads=1
//! ```
//!
//! Env: `KEEP_PERF_IMAGE` (default `keep-bench:gate`), `KEEP_PERF_HOST_CLASS`
//! (default derived, e.g. `darwin-aarch64-orbstack-10c`), `KEEP_PERF_NETWORK`
//! (`host` on Linux CI; default a private bridge), `KEEP_PERF_TRIALS` (3),
//! `KEEP_PERF_OPS` / `KEEP_PERF_TIERED_OPS`, `KEEP_PERF_CONCURRENCY` (50),
//! `KEEP_PERF_TIERS` / `KEEP_PERF_CORES` / `KEEP_PERF_TARGETS` (debug subsets;
//! a subset cannot pass because unmeasured ratchet cells fail), and
//! `KEEP_PERF_RECORD=1` (re-record, raise-only) or `=force` (may lower;
//! requires `KEEP_PERF_NOTE`).

use std::collections::BTreeMap;
use std::process::Command;
use std::str::FromStr;

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

/// Validates one `bench_compare --format json` report for `target` at `tier`.
/// `gated` targets (keep and ratchet peers) must prove their storage mode.
fn check_report(report: &Value, target: &str, tier: &str, gated: bool) -> Result<(), String> {
    if report["schema"] != REPORT_SCHEMA {
        return Err(format!(
            "{target}: unexpected report schema {}",
            report["schema"]
        ));
    }
    if report["backend"] != target {
        return Err(format!(
            "{target}: report is for backend {}",
            report["backend"]
        ));
    }
    let errors = report["errors"]
        .as_u64()
        .ok_or("report has no error count")?;
    if errors > 0 {
        return Err(format!("{target}: {errors} failed request(s)"));
    }
    let mode = &report["mode"];
    let verified = mode["verified"].as_bool().unwrap_or(false);
    if verified && mode["tier"] != tier {
        return Err(format!(
            "{target}: wanted {tier}, server reports {}",
            mode["detail"]
        ));
    }
    // A RAM-only launch is the default for every server; a durable tier must be
    // proven by the server before its numbers can gate anything.
    if !verified && tier != "ram" && gated {
        return Err(format!(
            "{target}: {tier} mode could not be verified ({})",
            mode["detail"]
        ));
    }
    for phase in PHASES {
        let p = report["phases"]
            .as_array()
            .and_then(|ps| ps.iter().find(|p| p["phase"] == phase))
            .ok_or_else(|| format!("{target}: report has no {phase} phase"))?;
        for k in ["ops_per_sec", "p99_us"] {
            if !p[k].as_f64().is_some_and(|v| v.is_finite() && v >= 0.0) {
                return Err(format!("{target}: {phase}.{k} missing"));
            }
        }
    }
    Ok(())
}

fn phase_value(report: &Value, phase: &str, metric: Metric) -> Option<f64> {
    let p = report["phases"]
        .as_array()?
        .iter()
        .find(|p| p["phase"] == phase)?;
    match metric {
        Metric::Ops => p["ops_per_sec"].as_f64(),
        // A sub-microsecond p99 reports 0; clamp so the ratio stays finite.
        Metric::P99 => p["p99_us"].as_f64().map(|v| v.max(1.0)),
    }
}

/// (tier, cores, target) -> one validated report per trial.
type Runs = BTreeMap<(String, u32, String), Vec<Value>>;

fn values(reports: &[Value], phase: &str, metric: Metric) -> Option<Vec<f64>> {
    reports
        .iter()
        .map(|r| phase_value(r, phase, metric))
        .collect()
}

fn build_measured(runs: &Runs) -> BTreeMap<String, Measured> {
    let mut out = BTreeMap::new();
    for ((tier, cores, target), reports) in runs {
        if target == "keep" {
            continue;
        }
        let Some(keep) = runs.get(&(tier.clone(), *cores, "keep".to_string())) else {
            continue;
        };
        for phase in PHASES {
            for metric in [Metric::Ops, Metric::P99] {
                let (Some(k), Some(p)) =
                    (values(keep, phase, metric), values(reports, phase, metric))
                else {
                    continue;
                };
                let (km, pm) = (median(&k), median(&p));
                out.insert(
                    cell_key(tier, *cores, phase, metric, target),
                    Measured {
                        ratio: ratio(metric, km, pm).unwrap_or(f64::NAN),
                        keep_cv: cv(&k),
                        detail: format!("keep {km:.0} vs {target} {pm:.0} {}", unit(metric)),
                    },
                );
            }
        }
    }
    // keep's own multi-core scaling: 4-core throughput over 1-core.
    for tier in TIERS {
        let one = runs.get(&(tier.to_string(), 1, "keep".to_string()));
        let four = runs.get(&(tier.to_string(), 4, "keep".to_string()));
        let (Some(one), Some(four)) = (one, four) else {
            continue;
        };
        for phase in PHASES {
            let (Some(a), Some(b)) = (
                values(four, phase, Metric::Ops),
                values(one, phase, Metric::Ops),
            ) else {
                continue;
            };
            let (am, bm) = (median(&a), median(&b));
            out.insert(
                cell_key(tier, 4, phase, Metric::Ops, SCALING_PEER),
                Measured {
                    ratio: ratio(Metric::Ops, am, bm).unwrap_or(f64::NAN),
                    keep_cv: cv(&a).max(cv(&b)),
                    detail: format!("keep 4c {am:.0} vs 1c {bm:.0} ops/s"),
                },
            );
        }
    }
    out
}

fn unit(metric: Metric) -> &'static str {
    match metric {
        Metric::Ops => "ops/s",
        Metric::P99 => "us p99",
    }
}

/// Structural problems in a baseline document (empty = valid).
fn validate_baseline(baseline: &Value) -> Vec<String> {
    let mut problems = Vec::new();
    if baseline["schema"] != BASELINE_SCHEMA {
        problems.push(format!("schema must be {BASELINE_SCHEMA}"));
    }
    if let Err(e) = Policy::from_baseline(baseline) {
        problems.push(e);
    }
    for target in &TARGETS[1..] {
        match baseline["images"][target].as_str() {
            Some(img) if img.contains("@sha256:") => {}
            _ => problems.push(format!("images.{target} must be pinned by @sha256 digest")),
        }
    }
    let Some(hosts) = baseline["hosts"].as_object() else {
        problems.push("hosts must be an object".into());
        return problems;
    };
    for (host, entry) in hosts {
        let cores_ok = match &entry["cores"] {
            Value::Null => true,
            Value::Array(cs) => {
                !cs.is_empty()
                    && cs
                        .iter()
                        .all(|c| c.as_u64().is_some_and(|c| (1..=256).contains(&c)))
            }
            _ => false,
        };
        if !cores_ok {
            problems.push(format!(
                "hosts.{host}.cores must be a non-empty list of core counts"
            ));
        }
        let Some(cells) = entry["cells"].as_object().filter(|c| !c.is_empty()) else {
            problems.push(format!("hosts.{host}.cells is empty"));
            continue;
        };
        for (key, cell) in cells {
            let id = match parse_cell_key(key) {
                Ok(id) => id,
                Err(e) => {
                    problems.push(e);
                    continue;
                }
            };
            // A ratchet peer's cell may be demoted to `report` on a host class
            // where it is not trustworthy (with a host `note`); a report-only
            // peer is never promoted to a ratchet.
            match cell["gate"].as_str() {
                Some("report") => {}
                Some("ratchet") if role_for(id.peer) == "ratchet" => {}
                Some("ratchet") => problems.push(format!(
                    "{host}/{key}: {} cells are report-only, not ratchet",
                    id.peer
                )),
                other => problems.push(format!("{host}/{key}: unknown gate {other:?}")),
            }
            if cell["gate"] == "report"
                && role_for(id.peer) == "ratchet"
                && entry["note"].as_str().is_none_or(|n| n.trim().is_empty())
            {
                problems.push(format!(
                    "{host}/{key}: a demoted ratchet cell needs a host `note` saying why"
                ));
            }
            match (positive(cell, "baseline"), positive(cell, "min_ratio")) {
                (Ok(b), Ok(m)) if m <= b => {}
                (Ok(_), Ok(_)) => problems.push(format!("{host}/{key}: min_ratio > baseline")),
                (Err(e), _) | (_, Err(e)) => problems.push(format!("{host}/{key}: {e}")),
            }
        }
    }
    problems
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

fn m(ratio: f64) -> Measured {
    Measured {
        ratio,
        keep_cv: 0.01,
        detail: "synthetic".into(),
    }
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
    assert!(
        validate_baseline(&next).is_empty(),
        "{:?}",
        validate_baseline(&next)
    );
    // A freshly recorded host passes its own measurement.
    assert!(evaluate(&next, "new", &measured).passed());

    let bad = BTreeMap::from([("ram/1c/SET/ops/redis".to_string(), m(f64::INFINITY))]);
    assert!(record(&baseline, "h", &bad, None, "d").is_err());
}

fn report(backend: &str, tier: &str, verified: bool, errors: u64, ops: f64, p99: u64) -> Value {
    json!({
        "schema": REPORT_SCHEMA,
        "backend": backend,
        "mode": { "tier": tier, "verified": verified, "detail": "synthetic" },
        "errors": errors,
        "phases": PHASES.iter().map(|p| json!({
            "phase": p, "ops_per_sec": ops, "p99_us": p99
        })).collect::<Vec<_>>(),
    })
}

#[test]
fn check_report_requires_clean_verified_runs() {
    assert!(check_report(
        &report("keep", "tiered", true, 0, 1.0, 1),
        "keep",
        "tiered",
        true
    )
    .is_ok());
    assert!(check_report(&report("keep", "ram", true, 3, 1.0, 1), "keep", "ram", true).is_err());
    assert!(check_report(
        &report("keep", "ram", true, 0, 1.0, 1),
        "keep",
        "tiered",
        true
    )
    .is_err());
    assert!(check_report(
        &report("redis", "ram", true, 0, 1.0, 1),
        "keep",
        "ram",
        true
    )
    .is_err());
    // Durable tier must be proven for gated targets; RAM need not be.
    assert!(check_report(
        &report("dragonfly", "ram", false, 0, 1.0, 1),
        "dragonfly",
        "ram",
        true
    )
    .is_ok());
    assert!(check_report(
        &report("redis", "tiered", false, 0, 1.0, 1),
        "redis",
        "tiered",
        true
    )
    .is_err());
    assert!(check_report(
        &report("garnet", "tiered", false, 0, 1.0, 1),
        "garnet",
        "tiered",
        false
    )
    .is_ok());
    let mut missing = report("keep", "ram", true, 0, 1.0, 1);
    missing["phases"].as_array_mut().unwrap().pop();
    assert!(check_report(&missing, "keep", "ram", true).is_err());
    let mut wrong = report("keep", "ram", true, 0, 1.0, 1);
    wrong["schema"] = json!("other/1");
    assert!(check_report(&wrong, "keep", "ram", true).is_err());
}

#[test]
fn build_measured_takes_median_ratios_and_scaling() {
    let run = |target: &str, cores: u32, ops: &[f64], p99: u64| {
        (
            ("ram".to_string(), cores, target.to_string()),
            ops.iter()
                .map(|o| report(target, "ram", true, 0, *o, p99))
                .collect::<Vec<_>>(),
        )
    };
    let runs: Runs = BTreeMap::from([
        run("keep", 1, &[100.0, 90.0, 110.0], 200),
        run("redis", 1, &[200.0, 210.0, 50.0], 100),
        run("keep", 4, &[300.0, 300.0, 300.0], 100),
        // No keep-less cells: dragonfly at 4c has keep, keydb at 2c does not.
        run("dragonfly", 4, &[600.0], 50),
        run("keydb", 2, &[1.0], 1),
    ]);
    let measured = build_measured(&runs);
    assert_eq!(measured["ram/1c/SET/ops/redis"].ratio, 0.5);
    assert_eq!(measured["ram/1c/SET/p99/redis"].ratio, 0.5);
    assert!(measured["ram/1c/SET/ops/redis"].keep_cv > 0.05);
    assert_eq!(measured["ram/4c/GET/ops/dragonfly"].ratio, 0.5);
    assert_eq!(measured["ram/4c/CAS/ops/keep-1c"].ratio, 3.0);
    assert!(!measured.keys().any(|k| k.contains("keydb")));
    assert!(!measured.keys().any(|k| k.starts_with("tiered")));
}

#[test]
fn ratchet_cells_may_be_demoted_with_a_note_but_never_promoted() {
    let mut baseline = synthetic_baseline(json!({
        "tiered/1c/SET/ops/redis": { "gate": "report", "baseline": 1.0, "min_ratio": 0.8 }
    }));
    assert_eq!(
        validate_baseline(&baseline).len(),
        1,
        "demotion without a note"
    );
    baseline["hosts"]["h"]["note"] = json!("virtualized fsync");
    assert!(validate_baseline(&baseline).is_empty());
    let promoted = synthetic_baseline(json!({
        "ram/1c/SET/ops/garnet": { "gate": "ratchet", "baseline": 1.0, "min_ratio": 0.8 }
    }));
    assert_eq!(validate_baseline(&promoted).len(), 1);
}

#[test]
fn committed_baseline_is_well_formed() {
    let text = std::fs::read_to_string(BASELINE_PATH).expect("read perf-baseline.json");
    let baseline: Value = serde_json::from_str(&text).expect("parse perf-baseline.json");
    let problems = validate_baseline(&baseline);
    assert!(problems.is_empty(), "perf-baseline.json: {problems:#?}");
    // Each recorded host covers the full ratchet matrix, so a gate run on that
    // host can never pass with a ratchet cell silently missing.
    for (host, entry) in baseline["hosts"].as_object().unwrap() {
        let cells = entry["cells"].as_object().unwrap();
        for tier in TIERS {
            for cores in host_cores(entry) {
                for phase in PHASES {
                    for metric in [Metric::Ops, Metric::P99] {
                        for peer in RATCHET_PEERS {
                            // Dragonfly has no fsync-per-write AOF: not run tiered.
                            if peer == "dragonfly" && tier == "tiered" {
                                continue;
                            }
                            let key = cell_key(tier, cores, phase, metric, peer);
                            assert!(cells.contains_key(&key), "{host}: missing {key}");
                        }
                    }
                }
            }
        }
    }
}

// ===========================================================================
// Live gate (Docker)
// ===========================================================================

fn env_or<T: FromStr>(key: &str, default: T) -> T {
    std::env::var(key)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

fn env_list(key: &str, default: &[&str]) -> Vec<String> {
    match std::env::var(key) {
        Ok(v) if !v.trim().is_empty() => v.split(',').map(|s| s.trim().to_string()).collect(),
        _ => default.iter().map(|s| s.to_string()).collect(),
    }
}

fn docker<S: AsRef<str>>(args: &[S]) -> Result<String, String> {
    let args: Vec<&str> = args.iter().map(AsRef::as_ref).collect();
    let out = Command::new("docker")
        .args(&args)
        .output()
        .map_err(|e| format!("docker: {e}"))?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
    } else {
        Err(format!(
            "docker {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        ))
    }
}

fn normalize_arch(arch: &str) -> &str {
    match arch {
        "aarch64" => "arm64",
        "x86_64" => "amd64",
        other => other,
    }
}

struct Host {
    class: String,
    arch: String,
    ncpu: u32,
}

fn host() -> Result<Host, String> {
    let info = docker(&[
        "info",
        "--format",
        "{{.Architecture}}|{{.NCPU}}|{{.OperatingSystem}}",
    ])?;
    let mut parts = info.splitn(3, '|');
    let arch = normalize_arch(parts.next().unwrap_or("")).to_string();
    let ncpu: u32 = parts
        .next()
        .and_then(|n| n.parse().ok())
        .ok_or("docker info: no NCPU")?;
    let os = parts.next().unwrap_or("");
    let runtime = if os.contains("OrbStack") {
        "orbstack"
    } else if os.contains("Docker Desktop") {
        "docker-desktop"
    } else {
        "docker"
    };
    let host_os = match std::env::consts::OS {
        "macos" => "darwin",
        other => other,
    };
    let class = std::env::var("KEEP_PERF_HOST_CLASS")
        .unwrap_or_else(|_| format!("{host_os}-{}-{runtime}-{ncpu}c", std::env::consts::ARCH));
    Ok(Host { class, arch, ncpu })
}

/// Pulls `image` if absent and refuses one built for another architecture
/// (an emulated server would gate on QEMU, not on the server).
fn ensure_image(image: &str, arch: &str) -> Result<String, String> {
    let inspect = |img: &str| {
        docker(&[
            "image",
            "inspect",
            "--format",
            "{{.Architecture}} {{.Id}}",
            img,
        ])
    };
    let found = match inspect(image) {
        Ok(found) => found,
        Err(_) => {
            docker(&["pull", "--quiet", image])?;
            inspect(image)?
        }
    };
    let (image_arch, id) = found.split_once(' ').unwrap_or((&found, ""));
    if normalize_arch(image_arch) != arch {
        return Err(format!(
            "{image} is {image_arch}, host is {arch} (emulated)"
        ));
    }
    Ok(id.chars().take(19).collect())
}

/// The server container's args for `target`, or `None` when the target does
/// not run at this tier.
fn server_args(target: &str, tier: &str, cores: u32) -> Option<(u16, Vec<String>)> {
    let durable = tier == "tiered";
    let n = cores.to_string();
    let aof: &[&str] = if durable {
        &["--appendonly", "yes", "--appendfsync", "always"]
    } else {
        &["--appendonly", "no"]
    };
    let (port, args): (u16, Vec<&str>) = match target {
        // Durable-before-ack group commit (the default) vs RAM-only.
        "keep" => {
            let mut a = vec![
                "keep",
                "--host",
                "0.0.0.0",
                "--port",
                "7117",
                "--log-level",
                "warn",
                "--worker-threads",
                &n,
                "--data-dir",
                "/home/keep/data",
            ];
            if !durable {
                a.push("--disable-persistence");
            }
            (7117, a)
        }
        "redis" => {
            let mut a = vec![
                "redis-server",
                "--port",
                "16379",
                "--protected-mode",
                "no",
                "--save",
                "",
                "--io-threads",
                &n,
            ];
            a.extend_from_slice(aof);
            (16379, a)
        }
        "keydb" => {
            let mut a = vec![
                "keydb-server",
                "--port",
                "16381",
                "--protected-mode",
                "no",
                "--save",
                "",
                "--server-threads",
                &n,
            ];
            a.extend_from_slice(aof);
            (16381, a)
        }
        // Dragonfly has no fsync-per-write AOF, so it has no tiered cell.
        "dragonfly" if durable => return None,
        "dragonfly" => (
            16380,
            vec![
                "dragonfly",
                "--logtostderr",
                "--port",
                "16380",
                "--proactor_threads",
                &n,
                "--dbfilename",
                "",
            ],
        ),
        "garnet" => {
            let mut a = vec!["--port", "16382", "--bind", "0.0.0.0", "--lua"];
            if durable {
                a.extend_from_slice(&[
                    "--aof",
                    "--aof-commit-freq",
                    "0",
                    "--aof-commit-wait",
                    "--checkpointdir",
                    "/data",
                ]);
            }
            (16382, a)
        }
        _ => return None,
    };
    Some((port, args.into_iter().map(str::to_string).collect()))
}

/// Removes the server container (and its anonymous data volume) on drop.
struct Server;

impl Server {
    fn start(net: &str, cpus: &str, image: &str, args: &[String]) -> Result<Self, String> {
        let _ = docker(&["rm", "-f", "-v", SERVER_NAME]);
        let mut run: Vec<String> = [
            "run",
            "-d",
            "--name",
            SERVER_NAME,
            "--network",
            net,
            "--cpuset-cpus",
            cpus,
            "--ulimit",
            "nofile=65536:65536",
            // Every server's data dir is a fresh anonymous volume (not overlayfs).
            "-v",
            "/data",
            "-v",
            "/home/keep",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        run.push(image.to_string());
        run.extend(args.iter().cloned());
        docker(&run)?;
        Ok(Self)
    }
    fn logs() -> String {
        docker(&["logs", "--tail", "15", SERVER_NAME]).unwrap_or_default()
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = docker(&["rm", "-f", "-v", SERVER_NAME]);
    }
}

struct Load<'a> {
    net: &'a str,
    cpus: String,
    image: &'a str,
    addr_host: &'a str,
    ops: usize,
    warmup: usize,
    concurrency: usize,
}

impl Load<'_> {
    fn run(&self, target: &str, tier: &str, port: u16, label: &str) -> Result<Value, String> {
        let keep_url = format!("http://{}:{port}", self.addr_host);
        let addr = format!("redis://{}:{port}", self.addr_host);
        let args = [
            "run",
            "--rm",
            "--network",
            self.net,
            "--cpuset-cpus",
            &self.cpus,
            self.image,
            "bench_compare",
            "--backend",
            target,
            "--keep-url",
            &keep_url,
            "--addr",
            &addr,
            "--tier",
            tier,
            "--phases",
            "set,get,cas",
            "--format",
            "json",
            "--wait-ready-secs",
            "60",
            "--ops",
            &self.ops.to_string(),
            "--warmup-ops",
            &self.warmup.to_string(),
            "--concurrency",
            &self.concurrency.to_string(),
            "--label",
            label,
        ];
        let out = docker(&args)?;
        let line = out
            .lines()
            .rev()
            .find(|l| l.starts_with('{'))
            .ok_or_else(|| format!("{target}: bench_compare printed no JSON report: {out}"))?;
        serde_json::from_str(line).map_err(|e| format!("{target}: bad report: {e}"))
    }
}

/// Fatal problems (keep or a ratchet peer cannot be measured) vs report-only
/// peers that are simply shown UNAVAILABLE.
#[derive(Default)]
struct Problems {
    fatal: Vec<String>,
    unavailable: Vec<String>,
}

impl Problems {
    fn fail(&mut self, target: &str, msg: String) {
        if target != "keep" && role_for(target) == "report" {
            self.unavailable.push(msg);
        } else {
            self.fatal.push(msg);
        }
    }
}

fn today() -> String {
    Command::new("date")
        .arg("+%F")
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default()
}

#[test]
#[ignore = "release-only: needs Docker, the keep-bench:gate image, and ~20 min"]
fn competitive_perf_gate() {
    let text = std::fs::read_to_string(BASELINE_PATH).expect("read perf-baseline.json");
    let baseline: Value = serde_json::from_str(&text).expect("parse perf-baseline.json");
    let problems = validate_baseline(&baseline);
    assert!(problems.is_empty(), "perf-baseline.json: {problems:#?}");

    let record_mode = std::env::var("KEEP_PERF_RECORD")
        .ok()
        .filter(|v| !v.is_empty());
    let force_note = match record_mode.as_deref() {
        Some("force") => Some(
            std::env::var("KEEP_PERF_NOTE")
                .ok()
                .filter(|n| !n.trim().is_empty())
                .expect("KEEP_PERF_RECORD=force requires KEEP_PERF_NOTE explaining the reset"),
        ),
        _ => None,
    };
    let host = host().unwrap_or_else(|e| panic!("docker is required: {e}"));
    let keep_image = std::env::var("KEEP_PERF_IMAGE").unwrap_or_else(|_| "keep-bench:gate".into());
    let net = std::env::var("KEEP_PERF_NETWORK").unwrap_or_else(|_| "keep-perf-gate".into());
    let addr_host = if net == "host" {
        "127.0.0.1"
    } else {
        SERVER_NAME
    };
    let trials: usize = env_or("KEEP_PERF_TRIALS", 3);
    let tiers = env_list("KEEP_PERF_TIERS", &TIERS);
    // Default to the core counts this host class was recorded at.
    let default_cores: Vec<String> = match baseline["hosts"].get(&host.class) {
        Some(entry) => host_cores(entry).iter().map(u32::to_string).collect(),
        None => CORES.iter().map(u32::to_string).collect(),
    };
    let default_cores: Vec<&str> = default_cores.iter().map(String::as_str).collect();
    let cores: Vec<u32> = env_list("KEEP_PERF_CORES", &default_cores)
        .iter()
        .map(|c| c.parse().expect("KEEP_PERF_CORES"))
        .collect();
    let targets = env_list("KEEP_PERF_TARGETS", &TARGETS);
    assert!(targets.iter().any(|t| t == "keep"), "keep must be measured");

    let mut problems = Problems::default();

    println!("\n== competitive perf gate: host class {} ==", host.class);
    let mut images = BTreeMap::new();
    for target in &targets {
        let image = if target == "keep" {
            keep_image.clone()
        } else {
            match baseline["images"][target.as_str()].as_str() {
                Some(i) => i.to_string(),
                None => {
                    problems.fail(target, format!("{target}: no pinned image"));
                    continue;
                }
            }
        };
        match ensure_image(&image, &host.arch) {
            Ok(id) => {
                println!("  {target:<10} {image} ({id})");
                images.insert(target.clone(), image);
            }
            Err(e) => problems.fail(target, format!("{target}: {e}")),
        }
    }
    assert!(
        problems.fatal.is_empty(),
        "competitive perf gate cannot run: {:#?}",
        problems.fatal
    );
    if net != "host" && docker(&["network", "inspect", &net]).is_err() {
        docker(&["network", "create", &net]).expect("create docker network");
    }

    let mut runs: Runs = BTreeMap::new();
    for tier in &tiers {
        let ops: usize = if tier == "tiered" {
            env_or("KEEP_PERF_TIERED_OPS", 40_000)
        } else {
            env_or("KEEP_PERF_OPS", 200_000)
        };
        for &c in &cores {
            assert!(
                host.ncpu >= c + 2,
                "{c}-core server needs >= {} CPUs for a disjoint load generator",
                c + 2
            );
            let server_cpus = format!("0-{}", c - 1);
            let load = Load {
                net: &net,
                cpus: format!("{c}-{}", host.ncpu - 1),
                image: &keep_image,
                addr_host,
                ops,
                warmup: ops / 10,
                concurrency: env_or("KEEP_PERF_CONCURRENCY", 50),
            };
            for target in &targets {
                let Some(image) = images.get(target) else {
                    continue;
                };
                let Some((port, args)) = server_args(target, tier, c) else {
                    continue;
                };
                let gated = role_for(target) == "ratchet" || target == "keep";
                let server = match Server::start(&net, &server_cpus, image, &args) {
                    Ok(s) => s,
                    Err(e) => {
                        problems.fail(target, format!("{target} {tier} {c}c: {e}"));
                        continue;
                    }
                };
                let mut reports = Vec::new();
                for trial in 0..trials {
                    let label = format!("{}:{tier}:{c}c:{target}:{trial}", host.class);
                    let result = load
                        .run(target, tier, port, &label)
                        .and_then(|r| check_report(&r, target, tier, gated).map(|()| r));
                    match result {
                        Ok(r) => reports.push(r),
                        Err(e) => {
                            let logs = Server::logs();
                            problems.fail(target, format!("{tier} {c}c: {e}\n{logs}"));
                            reports.clear();
                            break;
                        }
                    }
                }
                drop(server);
                if !reports.is_empty() {
                    let summary: Vec<String> = PHASES
                        .iter()
                        .map(|p| {
                            let ops = values(&reports, p, Metric::Ops).unwrap_or_default();
                            format!("{p} {:.0}/s", median(&ops))
                        })
                        .collect();
                    println!("  {tier:<6} {c}c {target:<10} {}", summary.join("  "));
                    runs.insert((tier.clone(), c, target.clone()), reports);
                }
            }
        }
    }
    if net != "host" {
        let _ = docker(&["network", "rm", &net]);
    }

    let measured = build_measured(&runs);
    let outcome = evaluate(&baseline, &host.class, &measured);
    println!(
        "\n{:<34} {:>8} {:>8}  verdict",
        "cell (higher = keep better)", "ratio", "min"
    );
    for line in &outcome.lines {
        println!("{line}");
    }
    for u in &problems.unavailable {
        println!("UNAVAILABLE (report-only): {u}");
    }
    assert!(
        problems.fatal.is_empty(),
        "competitive perf gate could not measure: {:#?}",
        problems.fatal
    );

    if record_mode.is_some() {
        let next = record(
            &baseline,
            &host.class,
            &measured,
            force_note.as_deref(),
            &today(),
        )
        .unwrap_or_else(|e| panic!("record baseline: {e}"));
        let problems = validate_baseline(&next);
        assert!(
            problems.is_empty(),
            "recorded baseline invalid: {problems:#?}"
        );
        std::fs::write(
            BASELINE_PATH,
            serde_json::to_string_pretty(&next).unwrap() + "\n",
        )
        .expect("write perf-baseline.json");
        println!("\nRECORDED baseline for host class {}", host.class);
        return;
    }
    for e in &outcome.errors {
        println!("ERROR: {e}");
    }
    for r in &outcome.regressions {
        println!("REGRESSION: {r}");
    }
    for u in &outcome.unstable {
        println!("UNSTABLE (rerun first): {u}");
    }
    assert!(
        outcome.passed(),
        "competitive perf gate FAILED: {} error(s), {} regression(s), {} unstable",
        outcome.errors.len(),
        outcome.regressions.len(),
        outcome.unstable.len()
    );
    println!("\nGATE PASSED");
}
