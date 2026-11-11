//! One-off competitor perf comparison (issue #126).
//!
//! ONE closed-loop load harness, four interchangeable backends running the
//! identical SET-then-GET workload and the identical measurement code:
//!
//!   engine     keep's KvEngine in-process (raw ceiling, no network/protocol)
//!   keep       keep over HTTP/2 cleartext + JSON
//!   redis      RESP over TCP (single-thread baseline)
//!   dragonfly  RESP over TCP (the multi-core bar)
//!
//! `--batch B` (B>1) amortizes per-request overhead over B keys per round trip:
//! keep uses `:mset`/`:mget`, RESP uses `MSET`/`MGET`, engine uses `mset`/`mget`.
//! One "op" = one key either way, so ops/s stays comparable; latency becomes
//! per-batch.
//!
//! Connections are pre-established / pre-warmed OUTSIDE the timed region.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use clap::Parser;
use serde_json::{json, Value};

type BoxError = Box<dyn std::error::Error + Send + Sync>;
type BenchResult<T> = Result<T, BoxError>;

const SCHEMA: &str = "keep.bench_compare/1";
const RESP_BACKENDS: [&str; 4] = ["redis", "dragonfly", "keydb", "garnet"];

#[derive(Parser, Debug, Clone)]
struct Args {
    /// engine | keep | keepnoop | redis | dragonfly | keydb | garnet
    #[arg(long)]
    backend: String,
    /// Total operations (keys) per phase.
    #[arg(long, default_value_t = 200_000)]
    ops: usize,
    /// Concurrent workers.
    #[arg(long, default_value_t = 50)]
    concurrency: usize,
    /// Keys per request (1 = single-key; >1 = MSET/MGET / :mset / :mget batch).
    #[arg(long, default_value_t = 1)]
    batch: usize,
    /// Value size in bytes.
    #[arg(long, default_value_t = 64)]
    value_size: usize,
    /// Distinct keys.
    #[arg(long, default_value_t = 100_000)]
    keyspace: usize,
    /// keep base URL.
    #[arg(long, default_value = "http://127.0.0.1:7117")]
    keep_url: String,
    /// RESP address for redis/dragonfly.
    #[arg(long, default_value = "redis://127.0.0.1:6379")]
    addr: String,
    /// Engine shard count (engine backend only).
    #[arg(long, default_value_t = 256)]
    shards: usize,
    /// keep backend: number of separate HTTP/2 connections (reqwest Clients).
    #[arg(long)]
    keep_clients: Option<usize>,
    /// engine backend: enable WAL persistence into this dir (durable path).
    #[arg(long)]
    persist: Option<String>,
    /// Output: `text` (human) or `json` (one keep.bench_compare/1 line on stdout).
    #[arg(long, default_value = "text", value_parser = ["text", "json"])]
    format: String,
    /// Free-form label echoed into the JSON report.
    #[arg(long)]
    label: Option<String>,
    /// Exit 0 even when requests failed (errors are still reported).
    #[arg(long, default_value_t = false)]
    allow_errors: bool,
}

impl Args {
    fn json(&self) -> bool {
        self.format == "json"
    }
}

/// Progress lines go to stdout in text mode and stderr in json mode, so the
/// json stdout stays exactly one parseable line.
macro_rules! note {
    ($args:expr, $($fmt:tt)*) => {
        if $args.json() { eprintln!($($fmt)*) } else { println!($($fmt)*) }
    };
}

/// One worker's latencies (µs per request) and failed-request count.
#[derive(Default)]
struct WorkerOut {
    lat: Vec<u64>,
    errors: u64,
}

impl WorkerOut {
    fn with_capacity(n: usize) -> Self {
        Self {
            lat: Vec::with_capacity(n),
            errors: 0,
        }
    }
    fn record(&mut self, t: Instant, ok: bool) {
        self.lat.push(t.elapsed().as_micros() as u64);
        if !ok {
            self.errors += 1;
        }
    }
}

struct PhaseResult {
    phase: &'static str,
    ops: usize,
    batch: usize,
    errors: u64,
    secs: f64,
    p50: u64,
    p99: u64,
    p999: u64,
}

impl PhaseResult {
    fn new(
        phase: &'static str,
        ops: usize,
        batch: usize,
        elapsed: Duration,
        outs: Vec<WorkerOut>,
    ) -> Self {
        let errors = outs.iter().map(|o| o.errors).sum();
        let mut lat: Vec<u64> = outs.into_iter().flat_map(|o| o.lat).collect();
        lat.sort_unstable();
        let pct = |p: f64| -> u64 {
            if lat.is_empty() {
                return 0;
            }
            lat[(((lat.len() - 1) as f64) * p).round() as usize]
        };
        Self {
            phase,
            ops,
            batch,
            errors,
            secs: elapsed.as_secs_f64(),
            p50: pct(0.50),
            p99: pct(0.99),
            p999: pct(0.999),
        }
    }
    fn ops_per_sec(&self) -> f64 {
        self.ops as f64 / self.secs
    }
    fn print_text(&self) {
        let unit = if self.batch > 1 { "batch" } else { "op" };
        let errors = if self.errors > 0 {
            format!("   ERRORS {}", self.errors)
        } else {
            String::new()
        };
        println!(
            "  {:4}  {:>11.0} ops/s   p50 {:>6}us/{unit}   p99 {:>7}us   p99.9 {:>7}us   ({:.2}s){errors}",
            self.phase,
            self.ops_per_sec(),
            self.p50,
            self.p99,
            self.p999,
            self.secs,
        );
    }
    fn json(&self) -> Value {
        json!({
            "phase": self.phase,
            "ops": self.ops,
            "batch": self.batch,
            "errors": self.errors,
            "secs": self.secs,
            "ops_per_sec": self.ops_per_sec(),
            "p50_us": self.p50,
            "p99_us": self.p99,
            "p999_us": self.p999,
        })
    }
}

/// The storage mode the server reported before measuring.
struct Mode {
    tier: String,
    verified: bool,
    detail: String,
}

#[tokio::main]
async fn main() {
    let args = Args::parse();
    match run(&args).await {
        Ok(errors) if errors > 0 && !args.allow_errors => {
            eprintln!("bench_compare: {errors} request(s) failed (pass --allow-errors to ignore)");
            std::process::exit(1);
        }
        Ok(_) => {}
        Err(e) => {
            eprintln!("bench_compare: {e}");
            std::process::exit(2);
        }
    }
}

/// Runs the benchmark and returns the total failed-request count.
async fn run(args: &Args) -> BenchResult<u64> {
    let val = "x".repeat(args.value_size);
    note!(
        args,
        "\n== backend={} ops={} concurrency={} batch={} value={}B keyspace={} ==",
        args.backend,
        args.ops,
        args.concurrency,
        args.batch,
        args.value_size,
        args.keyspace
    );
    let results = match args.backend.as_str() {
        "engine" => bench_engine(args, &val).await?,
        "keep" => bench_keep(args, &val).await?,
        // Pure HTTP/2 framework floor: hits /healthz (no key, no engine, no
        // JSON) so the gap vs `keep` GET isolates our app-code cost.
        "keepnoop" => bench_keep_noop(args).await?,
        b if RESP_BACKENDS.contains(&b) => bench_resp(args, &val).await?,
        other => return Err(format!("unknown backend: {other}").into()),
    };
    // The storage tier is verified by a later slice; until then it is reported as unknown.
    let mode = Mode {
        tier: "unknown".into(),
        verified: false,
        detail: format!("backend={}", args.backend),
    };
    let errors = results.iter().map(|r| r.errors).sum();
    if args.json() {
        println!("{}", report(args, &mode, &results));
    }
    Ok(errors)
}

fn report(args: &Args, mode: &Mode, results: &[PhaseResult]) -> Value {
    json!({
        "schema": SCHEMA,
        "label": args.label,
        "backend": args.backend,
        "mode": { "tier": mode.tier, "verified": mode.verified, "detail": mode.detail },
        "ops": args.ops,
        "concurrency": args.concurrency,
        "batch": args.batch.max(1),
        "value_size": args.value_size,
        "keyspace": args.keyspace,
        "errors": results.iter().map(|r| r.errors).sum::<u64>(),
        "phases": results.iter().map(PhaseResult::json).collect::<Vec<_>>(),
    })
}

fn emit(args: &Args, result: PhaseResult, out: &mut Vec<PhaseResult>) {
    if !args.json() {
        result.print_text();
    }
    out.push(result);
}

fn key_for(i: usize, keyspace: usize) -> String {
    format!("benchkey:{}", i % keyspace)
}

/// requests-per-worker and actual measured op count for the chosen batch.
fn plan(args: &Args) -> (usize, usize) {
    let batch = args.batch.max(1);
    let per_keys = args.ops / args.concurrency;
    let n_req = (per_keys / batch).max(1);
    let actual_ops = args.concurrency * n_req * batch;
    (n_req, actual_ops)
}

// --------------------------------------------------------------------------
// engine
// --------------------------------------------------------------------------
async fn bench_engine(args: &Args, val: &str) -> BenchResult<Vec<PhaseResult>> {
    use keep::persistence::{PersistenceConfig, PersistenceHandle};
    use keep::{KvEngine, KvKey, KvValue};
    let engine = Arc::new(KvEngine::with_shards(args.shards));
    if let Some(dir) = &args.persist {
        let cfg = PersistenceConfig::new(dir).with_fsync_interval_ms(100);
        let p = std::sync::Arc::new(PersistenceHandle::new(cfg, engine.clone())?);
        engine.enable_persistence(p);
        note!(args, "  (engine WAL persistence -> {dir})");
    }
    let (n_req, actual) = plan(args);
    let batch = args.batch.max(1);
    let mut results = Vec::new();

    for phase in ["SET", "GET"] {
        let start = Instant::now();
        let mut handles = Vec::new();
        for w in 0..args.concurrency {
            let engine = engine.clone();
            let val = val.to_string();
            let keyspace = args.keyspace;
            handles.push(tokio::task::spawn_blocking(move || {
                let mut out = WorkerOut::with_capacity(n_req);
                for r in 0..n_req {
                    let base = (w * n_req + r) * batch;
                    let keys: Vec<KvKey> = (0..batch)
                        .map(|j| KvKey::new(key_for(base + j, keyspace)).unwrap())
                        .collect();
                    let t = Instant::now();
                    let ok = if phase == "SET" {
                        let pairs: Vec<(&KvKey, KvValue)> = keys
                            .iter()
                            .map(|k| (k, KvValue::String(val.clone())))
                            .collect();
                        engine.mset(&pairs, None).is_ok()
                    } else {
                        let refs: Vec<&KvKey> = keys.iter().collect();
                        let _ = engine.mget(&refs);
                        true
                    };
                    out.record(t, ok);
                }
                out
            }));
        }
        let mut all = Vec::new();
        for h in handles {
            all.push(h.await?);
        }
        emit(
            args,
            PhaseResult::new(phase, actual, batch, start.elapsed(), all),
            &mut results,
        );
    }
    Ok(results)
}

// --------------------------------------------------------------------------
// keep over HTTP/2 + JSON
// --------------------------------------------------------------------------
async fn bench_keep(args: &Args, val: &str) -> BenchResult<Vec<PhaseResult>> {
    let n_clients = args.keep_clients.unwrap_or(args.concurrency).max(1);
    // Shared h2c pool: N single-connection clients, round-robin dispatch (#467).
    // The 10s timeout bounds a single request so a stalled stream can't hang the run.
    let pool = transport_h2c::H2cPool::with_connections_and(
        n_clients,
        Some(Duration::from_secs(10)),
        None,
    )?;
    note!(
        args,
        "  (keep over {} HTTP/2 connection(s))",
        pool.connections()
    );
    for _ in 0..pool.connections() {
        let _ = pool.get(format!("{}/healthz", args.keep_url)).send().await;
    }

    let (n_req, actual) = plan(args);
    let batch = args.batch.max(1);
    let mut results = Vec::new();

    for phase in ["SET", "GET"] {
        let start = Instant::now();
        let mut handles = Vec::new();
        for w in 0..args.concurrency {
            let client = pool.client().clone();
            let base_url = args.keep_url.clone();
            let val = val.to_string();
            let keyspace = args.keyspace;
            handles.push(tokio::spawn(async move {
                let mut out = WorkerOut::with_capacity(n_req);
                for r in 0..n_req {
                    let kbase = (w * n_req + r) * batch;
                    let keys: Vec<String> =
                        (0..batch).map(|j| key_for(kbase + j, keyspace)).collect();
                    let t = Instant::now();
                    let ok = if batch == 1 {
                        let url = format!("{base_url}/kv/{}", keys[0]);
                        let resp = if phase == "SET" {
                            client.put(&url).json(&json!({ "value": val })).send().await
                        } else {
                            client.get(&url).send().await
                        };
                        matches!(&resp, Ok(r) if r.status().is_success())
                    } else if phase == "SET" {
                        let entries: HashMap<&str, &str> =
                            keys.iter().map(|k| (k.as_str(), val.as_str())).collect();
                        let resp = client
                            .post(format!("{base_url}/kv:mset"))
                            .json(&json!({ "entries": entries }))
                            .send()
                            .await;
                        matches!(&resp, Ok(r) if r.status().is_success())
                    } else {
                        let resp = client
                            .post(format!("{base_url}/kv:mget"))
                            .json(&json!({ "keys": keys }))
                            .send()
                            .await;
                        matches!(&resp, Ok(r) if r.status().is_success())
                    };
                    out.record(t, ok);
                }
                out
            }));
        }
        let mut all = Vec::new();
        for h in handles {
            all.push(h.await?);
        }
        emit(
            args,
            PhaseResult::new(phase, actual, batch, start.elapsed(), all),
            &mut results,
        );
    }
    Ok(results)
}

// --------------------------------------------------------------------------
// keep framework floor: GET /healthz (no key / engine / JSON)
// --------------------------------------------------------------------------
async fn bench_keep_noop(args: &Args) -> BenchResult<Vec<PhaseResult>> {
    let n_clients = args.keep_clients.unwrap_or(args.concurrency).max(1);
    // Shared h2c pool, round-robin dispatch (#467); 10s per-request timeout.
    let pool = transport_h2c::H2cPool::with_connections_and(
        n_clients,
        Some(Duration::from_secs(10)),
        None,
    )?;
    let url = format!("{}/healthz", args.keep_url);
    for _ in 0..pool.connections() {
        let _ = pool.get(&url).send().await;
    }
    let per = args.ops / args.concurrency;
    let start = Instant::now();
    let mut handles = Vec::new();
    for _ in 0..args.concurrency {
        let client = pool.client().clone();
        let url = url.clone();
        handles.push(tokio::spawn(async move {
            let mut out = WorkerOut::with_capacity(per);
            for _ in 0..per {
                let t = Instant::now();
                let resp = client.get(&url).send().await;
                let ok = matches!(&resp, Ok(r) if r.status().is_success());
                out.record(t, ok);
            }
            out
        }));
    }
    let mut all = Vec::new();
    for h in handles {
        all.push(h.await?);
    }
    let mut results = Vec::new();
    emit(
        args,
        PhaseResult::new("NOOP", args.concurrency * per, 1, start.elapsed(), all),
        &mut results,
    );
    Ok(results)
}

// --------------------------------------------------------------------------
// redis / dragonfly / keydb / garnet (RESP)
// --------------------------------------------------------------------------
async fn bench_resp(args: &Args, val: &str) -> BenchResult<Vec<PhaseResult>> {
    let client = redis::Client::open(args.addr.clone())?;
    {
        let mut c = client.get_multiplexed_async_connection().await?;
        redis::cmd("PING").query_async::<String>(&mut c).await?;
    }
    let (n_req, actual) = plan(args);
    let batch = args.batch.max(1);
    let mut results = Vec::new();

    for phase in ["SET", "GET"] {
        // pre-establish one connection per worker, OUTSIDE the timed region.
        let conns: Vec<_> = futures::future::join_all(
            (0..args.concurrency).map(|_| client.get_multiplexed_async_connection()),
        )
        .await
        .into_iter()
        .map(|r| r.expect("connect"))
        .collect();

        let start = Instant::now();
        let mut handles = Vec::new();
        for (w, mut con) in conns.into_iter().enumerate() {
            let val = val.to_string();
            let keyspace = args.keyspace;
            handles.push(tokio::spawn(async move {
                let mut out = WorkerOut::with_capacity(n_req);
                for r in 0..n_req {
                    let kbase = (w * n_req + r) * batch;
                    let keys: Vec<String> =
                        (0..batch).map(|j| key_for(kbase + j, keyspace)).collect();
                    let t = Instant::now();
                    let ok = if phase == "SET" {
                        let mut cmd = redis::cmd(if batch == 1 { "SET" } else { "MSET" });
                        for k in &keys {
                            cmd.arg(k).arg(&val);
                        }
                        cmd.query_async::<()>(&mut con).await.is_ok()
                    } else if batch == 1 {
                        redis::cmd("GET")
                            .arg(&keys[0])
                            .query_async::<Option<String>>(&mut con)
                            .await
                            .is_ok()
                    } else {
                        let mut cmd = redis::cmd("MGET");
                        for k in &keys {
                            cmd.arg(k);
                        }
                        cmd.query_async::<Vec<Option<String>>>(&mut con)
                            .await
                            .is_ok()
                    };
                    out.record(t, ok);
                }
                out
            }));
        }
        let mut all = Vec::new();
        for h in handles {
            all.push(h.await?);
        }
        emit(
            args,
            PhaseResult::new(phase, actual, batch, start.elapsed(), all),
            &mut results,
        );
    }
    Ok(results)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percentiles_and_errors() {
        let outs = vec![
            WorkerOut {
                lat: (1..=50).collect(),
                errors: 1,
            },
            WorkerOut {
                lat: (51..=100).collect(),
                errors: 2,
            },
        ];
        let r = PhaseResult::new("SET", 100, 1, Duration::from_secs(2), outs);
        assert_eq!(r.errors, 3);
        assert_eq!(r.p50, 51);
        assert_eq!(r.p99, 99);
        assert_eq!(r.p999, 100);
        assert_eq!(r.ops_per_sec(), 50.0);
    }

    #[test]
    fn worker_record_counts_only_failures() {
        let mut w = WorkerOut::with_capacity(2);
        w.record(Instant::now(), true);
        w.record(Instant::now(), false);
        assert_eq!(w.lat.len(), 2);
        assert_eq!(w.errors, 1);
    }

    #[test]
    fn json_report_has_the_schema_and_phase_fields() {
        let args = Args::parse_from([
            "bench_compare",
            "--backend",
            "engine",
            "--format",
            "json",
            "--label",
            "t",
        ]);
        let results = vec![PhaseResult {
            phase: "SET",
            ops: 10,
            batch: 1,
            errors: 2,
            secs: 1.0,
            p50: 1,
            p99: 2,
            p999: 3,
        }];
        let mode = Mode {
            tier: "unknown".into(),
            verified: false,
            detail: "d".into(),
        };
        let v = report(&args, &mode, &results);
        assert_eq!(v["schema"], "keep.bench_compare/1");
        assert_eq!(v["label"], "t");
        assert_eq!(v["backend"], "engine");
        assert_eq!(v["errors"], 2);
        assert_eq!(v["mode"]["verified"], false);
        assert_eq!(v["phases"][0]["phase"], "SET");
        for key in [
            "ops_per_sec",
            "p99_us",
            "p50_us",
            "p999_us",
            "errors",
            "secs",
            "batch",
            "ops",
        ] {
            assert!(v["phases"][0].get(key).is_some(), "missing {key}");
        }
    }

    #[test]
    fn format_defaults_to_text_and_rejects_other_values() {
        assert!(!Args::parse_from(["bench_compare", "--backend", "engine"]).json());
        assert!(
            Args::parse_from(["bench_compare", "--backend", "engine", "--format", "json"]).json()
        );
        assert!(
            Args::try_parse_from(["bench_compare", "--backend", "engine", "--format", "xml"])
                .is_err()
        );
    }
}
