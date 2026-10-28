//! Competitor perf comparison harness (issues #126, #2).
//!
//! ONE closed-loop load harness, interchangeable backends running the identical
//! workload and the identical measurement code:
//!
//!   engine     keep's KvEngine in-process (raw ceiling, no network/protocol)
//!   keep       keep over HTTP/2 cleartext + JSON
//!   keepnoop   keep's HTTP/2 framework floor (`GET /healthz`)
//!   redis      RESP over TCP (single-thread baseline)
//!   dragonfly  RESP over TCP (the multi-core bar)
//!   keydb      RESP over TCP (multi-threaded Redis fork)
//!   garnet     RESP over TCP (Microsoft Garnet)
//!
//! Phases (`--phases`, default `set,get`): SET, GET, and CAS. CAS is single-key
//! compare-and-swap with one round trip per op: keep `POST /kv/{k}/cas`, RESP
//! `EVALSHA` of a GET-compare-SET Lua script (atomic on every RESP server).
//! CAS keys are seeded before the timed region and each worker owns its keys,
//! so every swap is expected to apply; a swap that does not apply is an error.
//!
//! `--batch B` (B>1) amortizes per-request overhead over B keys per round trip
//! for SET/GET: keep uses `:mset`/`:mget`, RESP uses `MSET`/`MGET`, engine uses
//! `mset`/`mget`. One "op" = one key either way, so ops/s stays comparable;
//! latency becomes per-batch.
//!
//! Every failed request (transport error, non-2xx, GET miss, CAS not swapped)
//! is counted. Any error makes the run exit non-zero unless `--allow-errors`, so
//! a broken server cannot pass as a fast one. `--tier ram|tiered` checks the
//! server's actual storage mode before measuring (keep `/info`, RESP
//! `CONFIG GET appendonly/appendfsync`) and fails on a mismatch.
//!
//! `--format json` prints exactly one `keep.bench_compare/1` JSON line on
//! stdout (progress goes to stderr); the default text output is unchanged.
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
const CAS_LUA: &str = "if redis.call('GET', KEYS[1]) == ARGV[1] then \
     redis.call('SET', KEYS[1], ARGV[2]) return 1 else return 0 end";

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
    /// Keys per request for SET/GET (1 = single-key; >1 = MSET/MGET / :mset / :mget batch).
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
    /// RESP address for redis/dragonfly/keydb/garnet.
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
    /// Comma-separated phases, run in order: set, get, cas.
    #[arg(long, default_value = "set,get", value_delimiter = ',')]
    phases: Vec<String>,
    /// Output: `text` (human) or `json` (one keep.bench_compare/1 line on stdout).
    #[arg(long, default_value = "text", value_parser = ["text", "json"])]
    format: String,
    /// Required server storage mode: `ram` (no durable log) or `tiered`
    /// (durable-before-ack log). Checked before measuring; unset = not checked.
    #[arg(long, value_parser = ["ram", "tiered"])]
    tier: Option<String>,
    /// Untimed ops of the same phase run before each timed phase.
    #[arg(long, default_value_t = 0)]
    warmup_ops: usize,
    /// Free-form label echoed into the JSON report.
    #[arg(long)]
    label: Option<String>,
    /// Exit 0 even when requests failed (errors are still reported).
    #[arg(long, default_value_t = false)]
    allow_errors: bool,
    /// Poll the server for up to this many seconds before measuring.
    #[arg(long, default_value_t = 0)]
    wait_ready_secs: u64,
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

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Phase {
    Set,
    Get,
    Cas,
}

impl Phase {
    fn parse(s: &str) -> BenchResult<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "set" => Ok(Self::Set),
            "get" => Ok(Self::Get),
            "cas" => Ok(Self::Cas),
            other => Err(format!("unknown phase `{other}` (expected set|get|cas)").into()),
        }
    }
    fn label(self) -> &'static str {
        match self {
            Self::Set => "SET",
            Self::Get => "GET",
            Self::Cas => "CAS",
        }
    }
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
    let phases = args
        .phases
        .iter()
        .filter(|p| !p.trim().is_empty())
        .map(|p| Phase::parse(p))
        .collect::<BenchResult<Vec<_>>>()?;
    if phases.is_empty() {
        return Err("--phases selected nothing".into());
    }
    if args.concurrency == 0 || args.ops < args.concurrency {
        return Err("--ops must be >= --concurrency > 0".into());
    }
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
    let (mode, results) = match args.backend.as_str() {
        "engine" => bench_engine(args, &val, &phases).await?,
        "keep" => bench_keep(args, &val, &phases).await?,
        // Pure HTTP/2 framework floor: hits /healthz (no key, no engine, no
        // JSON) so the gap vs `keep` GET isolates our app-code cost.
        "keepnoop" => bench_keep_noop(args).await?,
        b if RESP_BACKENDS.contains(&b) => bench_resp(args, &val, &phases).await?,
        other => return Err(format!("unknown backend: {other}").into()),
    };
    let errors = results.iter().map(|r| r.errors).sum();
    if args.json() {
        let report = json!({
            "schema": SCHEMA,
            "label": args.label,
            "backend": args.backend,
            "mode": { "tier": mode.tier, "verified": mode.verified, "detail": mode.detail },
            "ops": args.ops,
            "concurrency": args.concurrency,
            "batch": args.batch.max(1),
            "value_size": args.value_size,
            "keyspace": args.keyspace,
            "warmup_ops": args.warmup_ops,
            "errors": errors,
            "phases": results.iter().map(PhaseResult::json).collect::<Vec<_>>(),
        });
        println!("{report}");
    }
    Ok(errors)
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

/// CAS keys are worker-owned and tagged per run so warmup and timed runs never
/// share state.
fn cas_key(tag: &str, worker: usize, slot: usize) -> String {
    format!("benchcas:{tag}:{worker}:{slot}")
}

/// The value a CAS key holds after `n` swaps, padded to `--value-size`.
fn cas_value(n: u64, size: usize) -> String {
    format!("{n:0>size$}")
}

/// requests-per-worker and actual measured op count for `ops` keys at `batch`.
fn plan(ops: usize, concurrency: usize, batch: usize) -> (usize, usize) {
    let batch = batch.max(1);
    let per_keys = ops / concurrency;
    let n_req = (per_keys / batch).max(1);
    (n_req, concurrency * n_req * batch)
}

/// CAS keys per worker: the worker cycles through these, one swap per op.
fn cas_slots(args: &Args, n_req: usize) -> usize {
    (args.keyspace / args.concurrency).clamp(1, n_req.max(1))
}

// --------------------------------------------------------------------------
// engine
// --------------------------------------------------------------------------
async fn bench_engine(
    args: &Args,
    val: &str,
    phases: &[Phase],
) -> BenchResult<(Mode, Vec<PhaseResult>)> {
    use keep::persistence::{PersistenceConfig, PersistenceHandle};
    use keep::{KvEngine, KvKey, KvValue};
    if phases.contains(&Phase::Cas) {
        return Err("the engine backend measures set/get only".into());
    }
    let engine = Arc::new(KvEngine::with_shards(args.shards));
    if let Some(dir) = &args.persist {
        let cfg = PersistenceConfig::new(dir).with_fsync_interval_ms(100);
        let p = std::sync::Arc::new(PersistenceHandle::new(cfg, engine.clone())?);
        engine.enable_persistence(p);
        note!(args, "  (engine WAL persistence -> {dir})");
    }
    let tier = if args.persist.is_some() {
        "tiered"
    } else {
        "ram"
    };
    if let Some(want) = &args.tier {
        if want != tier {
            return Err(format!("--tier {want} but the engine runs {tier} (see --persist)").into());
        }
    }
    let mode = Mode {
        tier: tier.into(),
        verified: true,
        detail: format!("in-process engine, persist={}", args.persist.is_some()),
    };
    let batch = args.batch.max(1);
    let mut results = Vec::new();

    for &phase in phases {
        for (ops, timed) in [(args.warmup_ops, false), (args.ops, true)] {
            if ops == 0 {
                continue;
            }
            let (n_req, actual) = plan(ops, args.concurrency, batch);
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
                        let ok = if phase == Phase::Set {
                            let pairs: Vec<(&KvKey, KvValue)> = keys
                                .iter()
                                .map(|k| (k, KvValue::String(val.clone())))
                                .collect();
                            engine.mset(&pairs, None).is_ok()
                        } else {
                            let refs: Vec<&KvKey> = keys.iter().collect();
                            engine.mget(&refs).iter().all(Option::is_some)
                        };
                        out.record(t, ok);
                    }
                    out
                }));
            }
            let mut outs = Vec::new();
            for h in handles {
                outs.push(h.await?);
            }
            if timed {
                let result = PhaseResult::new(phase.label(), actual, batch, start.elapsed(), outs);
                emit(args, result, &mut results);
            }
        }
    }
    Ok((mode, results))
}

// --------------------------------------------------------------------------
// keep over HTTP/2 + JSON
// --------------------------------------------------------------------------
fn keep_pool(args: &Args) -> BenchResult<transport_h2c::H2cPool> {
    let n_clients = args.keep_clients.unwrap_or(args.concurrency).max(1);
    // Shared h2c pool: N single-connection clients, round-robin dispatch (#467).
    // The 10s timeout bounds a single request so a stalled stream can't hang the run.
    Ok(transport_h2c::H2cPool::with_connections_and(
        n_clients,
        Some(Duration::from_secs(10)),
        None,
    )?)
}

async fn keep_wait_ready(args: &Args, pool: &transport_h2c::H2cPool) -> BenchResult<()> {
    let deadline = Instant::now() + Duration::from_secs(args.wait_ready_secs);
    let url = format!("{}/readyz", args.keep_url);
    loop {
        match pool.get(&url).send().await {
            Ok(resp) if resp.status().is_success() => return Ok(()),
            _ if Instant::now() < deadline => tokio::time::sleep(Duration::from_millis(200)).await,
            Ok(resp) => return Err(format!("keep not ready: {url} -> {}", resp.status()).into()),
            Err(e) => return Err(format!("keep not reachable at {url}: {e}").into()),
        }
    }
}

async fn keep_mode(args: &Args, pool: &transport_h2c::H2cPool) -> BenchResult<Mode> {
    let info: Value = pool
        .get(format!("{}/info", args.keep_url))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    let persistent = info["persistence_enabled"]
        .as_bool()
        .ok_or("keep /info has no persistence_enabled (server predates the tier check)")?;
    let tier = if persistent { "tiered" } else { "ram" };
    if let Some(want) = &args.tier {
        if want != tier {
            return Err(format!("--tier {want} but keep reports {tier} ({info})").into());
        }
    }
    Ok(Mode {
        tier: tier.into(),
        verified: true,
        detail: format!(
            "persistence_enabled={persistent} fsync_interval_ms={}",
            info["fsync_interval_ms"]
        ),
    })
}

async fn bench_keep(
    args: &Args,
    val: &str,
    phases: &[Phase],
) -> BenchResult<(Mode, Vec<PhaseResult>)> {
    let pool = keep_pool(args)?;
    note!(
        args,
        "  (keep over {} HTTP/2 connection(s))",
        pool.connections()
    );
    keep_wait_ready(args, &pool).await?;
    for _ in 0..pool.connections() {
        pool.get(format!("{}/healthz", args.keep_url))
            .send()
            .await?;
    }
    let mode = keep_mode(args, &pool).await?;
    let mut results = Vec::new();

    for &phase in phases {
        for (ops, timed) in [(args.warmup_ops, false), (args.ops, true)] {
            if ops == 0 {
                continue;
            }
            let result = if phase == Phase::Cas {
                keep_cas_phase(args, &pool, val.len(), ops, timed).await?
            } else {
                keep_kv_phase(args, &pool, val, phase, ops).await?
            };
            if timed {
                emit(args, result, &mut results);
            }
        }
    }
    Ok((mode, results))
}

async fn keep_kv_phase(
    args: &Args,
    pool: &transport_h2c::H2cPool,
    val: &str,
    phase: Phase,
    ops: usize,
) -> BenchResult<PhaseResult> {
    let batch = args.batch.max(1);
    let (n_req, actual) = plan(ops, args.concurrency, batch);
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
                let keys: Vec<String> = (0..batch).map(|j| key_for(kbase + j, keyspace)).collect();
                let t = Instant::now();
                let resp = if batch == 1 {
                    let url = format!("{base_url}/kv/{}", keys[0]);
                    if phase == Phase::Set {
                        client.put(&url).json(&json!({ "value": val })).send().await
                    } else {
                        client.get(&url).send().await
                    }
                } else if phase == Phase::Set {
                    let entries: HashMap<&str, &str> =
                        keys.iter().map(|k| (k.as_str(), val.as_str())).collect();
                    client
                        .post(format!("{base_url}/kv:mset"))
                        .json(&json!({ "entries": entries }))
                        .send()
                        .await
                } else {
                    client
                        .post(format!("{base_url}/kv:mget"))
                        .json(&json!({ "keys": keys }))
                        .send()
                        .await
                };
                let ok = matches!(&resp, Ok(r) if r.status().is_success());
                // Drain the body so the stream completes inside the timed op.
                if let Ok(r) = resp {
                    let _ = r.bytes().await;
                }
                out.record(t, ok);
            }
            out
        }));
    }
    let mut outs = Vec::new();
    for h in handles {
        outs.push(h.await?);
    }
    Ok(PhaseResult::new(
        phase.label(),
        actual,
        batch,
        start.elapsed(),
        outs,
    ))
}

async fn keep_cas_phase(
    args: &Args,
    pool: &transport_h2c::H2cPool,
    size: usize,
    ops: usize,
    timed: bool,
) -> BenchResult<PhaseResult> {
    let (n_req, actual) = plan(ops, args.concurrency, 1);
    let slots = cas_slots(args, n_req);
    let tag = if timed { "timed" } else { "warmup" };
    // Seed every worker-owned key outside the timed region.
    for w in 0..args.concurrency {
        for s in 0..slots {
            let url = format!("{}/kv/{}", args.keep_url, cas_key(tag, w, s));
            pool.client()
                .put(&url)
                .json(&json!({ "value": cas_value(0, size) }))
                .send()
                .await?
                .error_for_status()?;
        }
    }
    let start = Instant::now();
    let mut handles = Vec::new();
    for w in 0..args.concurrency {
        let client = pool.client().clone();
        let base_url = args.keep_url.clone();
        handles.push(tokio::spawn(async move {
            let mut out = WorkerOut::with_capacity(n_req);
            let mut counters = vec![0u64; slots];
            for r in 0..n_req {
                let s = r % slots;
                let url = format!("{base_url}/kv/{}/cas", cas_key(tag, w, s));
                let body = json!({
                    "expected": cas_value(counters[s], size),
                    "new": cas_value(counters[s] + 1, size),
                });
                let t = Instant::now();
                let swapped = match client.post(&url).json(&body).send().await {
                    Ok(resp) if resp.status().is_success() => resp
                        .json::<Value>()
                        .await
                        .is_ok_and(|v| v["swapped"] == true),
                    _ => false,
                };
                out.record(t, swapped);
                if swapped {
                    counters[s] += 1;
                }
            }
            out
        }));
    }
    let mut outs = Vec::new();
    for h in handles {
        outs.push(h.await?);
    }
    Ok(PhaseResult::new("CAS", actual, 1, start.elapsed(), outs))
}

// --------------------------------------------------------------------------
// keep framework floor: GET /healthz (no key / engine / JSON)
// --------------------------------------------------------------------------
async fn bench_keep_noop(args: &Args) -> BenchResult<(Mode, Vec<PhaseResult>)> {
    let pool = keep_pool(args)?;
    keep_wait_ready(args, &pool).await?;
    let url = format!("{}/healthz", args.keep_url);
    for _ in 0..pool.connections() {
        pool.get(&url).send().await?;
    }
    let mode = keep_mode(args, &pool).await?;
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
                let ok = matches!(client.get(&url).send().await, Ok(r) if r.status().is_success());
                out.record(t, ok);
            }
            out
        }));
    }
    let mut outs = Vec::new();
    for h in handles {
        outs.push(h.await?);
    }
    let mut results = Vec::new();
    let result = PhaseResult::new("NOOP", args.concurrency * per, 1, start.elapsed(), outs);
    emit(args, result, &mut results);
    Ok((mode, results))
}

// --------------------------------------------------------------------------
// redis / dragonfly / keydb / garnet (RESP)
// --------------------------------------------------------------------------
async fn resp_wait_ready(args: &Args, client: &redis::Client) -> BenchResult<()> {
    let deadline = Instant::now() + Duration::from_secs(args.wait_ready_secs);
    loop {
        let attempt = async {
            let mut c = client.get_multiplexed_async_connection().await?;
            redis::cmd("PING").query_async::<String>(&mut c).await
        };
        match attempt.await {
            Ok(_) => return Ok(()),
            Err(_) if Instant::now() < deadline => {
                tokio::time::sleep(Duration::from_millis(200)).await
            }
            Err(e) => {
                return Err(format!("{} not reachable at {}: {e}", args.backend, args.addr).into())
            }
        }
    }
}

/// `CONFIG GET name` as a plain value; `None` when the server does not expose it.
async fn config_get(con: &mut redis::aio::MultiplexedConnection, name: &str) -> Option<String> {
    let pairs: Vec<String> = redis::cmd("CONFIG")
        .arg("GET")
        .arg(name)
        .query_async(con)
        .await
        .ok()?;
    pairs.get(1).cloned()
}

async fn resp_mode(args: &Args, con: &mut redis::aio::MultiplexedConnection) -> BenchResult<Mode> {
    let appendonly = config_get(con, "appendonly").await;
    let appendfsync = config_get(con, "appendfsync").await;
    let detail = format!(
        "appendonly={} appendfsync={}",
        appendonly.as_deref().unwrap_or("unsupported"),
        appendfsync.as_deref().unwrap_or("unsupported")
    );
    let (tier, verified) = match (appendonly.as_deref(), appendfsync.as_deref()) {
        (Some("yes"), Some("always")) => ("tiered", true),
        (Some("yes"), Some(_)) => ("tiered-relaxed", true),
        (Some("no"), _) => ("ram", true),
        // No AOF config surface (e.g. Dragonfly), or an AOF without an
        // `appendfsync` knob (Garnet commits via --aof-commit-freq): the mode
        // is whatever the server was launched with and cannot be checked here.
        _ => (args.tier.as_deref().unwrap_or("unknown"), false),
    };
    if let Some(want) = &args.tier {
        if verified && want != tier {
            return Err(format!(
                "--tier {want} but {} reports {tier} ({detail})",
                args.backend
            )
            .into());
        }
    }
    Ok(Mode {
        tier: tier.into(),
        verified,
        detail,
    })
}

async fn bench_resp(
    args: &Args,
    val: &str,
    phases: &[Phase],
) -> BenchResult<(Mode, Vec<PhaseResult>)> {
    let client = redis::Client::open(args.addr.clone())?;
    resp_wait_ready(args, &client).await?;
    let mode = {
        let mut c = client.get_multiplexed_async_connection().await?;
        resp_mode(args, &mut c).await?
    };
    let mut results = Vec::new();

    for &phase in phases {
        for (ops, timed) in [(args.warmup_ops, false), (args.ops, true)] {
            if ops == 0 {
                continue;
            }
            let result = if phase == Phase::Cas {
                resp_cas_phase(args, &client, val.len(), ops, timed).await?
            } else {
                resp_kv_phase(args, &client, val, phase, ops).await?
            };
            if timed {
                emit(args, result, &mut results);
            }
        }
    }
    Ok((mode, results))
}

/// One connection per worker, pre-established OUTSIDE the timed region.
async fn resp_conns(
    client: &redis::Client,
    n: usize,
) -> BenchResult<Vec<redis::aio::MultiplexedConnection>> {
    let conns =
        futures::future::join_all((0..n).map(|_| client.get_multiplexed_async_connection()))
            .await
            .into_iter()
            .collect::<Result<Vec<_>, _>>()?;
    Ok(conns)
}

async fn resp_kv_phase(
    args: &Args,
    client: &redis::Client,
    val: &str,
    phase: Phase,
    ops: usize,
) -> BenchResult<PhaseResult> {
    let batch = args.batch.max(1);
    let (n_req, actual) = plan(ops, args.concurrency, batch);
    let conns = resp_conns(client, args.concurrency).await?;
    let start = Instant::now();
    let mut handles = Vec::new();
    for (w, mut con) in conns.into_iter().enumerate() {
        let val = val.to_string();
        let keyspace = args.keyspace;
        handles.push(tokio::spawn(async move {
            let mut out = WorkerOut::with_capacity(n_req);
            for r in 0..n_req {
                let kbase = (w * n_req + r) * batch;
                let keys: Vec<String> = (0..batch).map(|j| key_for(kbase + j, keyspace)).collect();
                let t = Instant::now();
                let ok = if phase == Phase::Set {
                    let mut cmd = redis::cmd(if batch == 1 { "SET" } else { "MSET" });
                    for k in &keys {
                        cmd.arg(k).arg(&val);
                    }
                    cmd.query_async::<()>(&mut con).await.is_ok()
                } else if batch == 1 {
                    matches!(
                        redis::cmd("GET")
                            .arg(&keys[0])
                            .query_async::<Option<String>>(&mut con)
                            .await,
                        Ok(Some(_))
                    )
                } else {
                    let mut cmd = redis::cmd("MGET");
                    for k in &keys {
                        cmd.arg(k);
                    }
                    cmd.query_async::<Vec<Option<String>>>(&mut con)
                        .await
                        .is_ok_and(|vs| vs.iter().all(Option::is_some))
                };
                out.record(t, ok);
            }
            out
        }));
    }
    let mut outs = Vec::new();
    for h in handles {
        outs.push(h.await?);
    }
    Ok(PhaseResult::new(
        phase.label(),
        actual,
        batch,
        start.elapsed(),
        outs,
    ))
}

async fn resp_cas_phase(
    args: &Args,
    client: &redis::Client,
    size: usize,
    ops: usize,
    timed: bool,
) -> BenchResult<PhaseResult> {
    let (n_req, actual) = plan(ops, args.concurrency, 1);
    let slots = cas_slots(args, n_req);
    let tag = if timed { "timed" } else { "warmup" };
    let sha: String = {
        let mut c = client.get_multiplexed_async_connection().await?;
        // Seed every worker-owned key outside the timed region.
        for w in 0..args.concurrency {
            let mut cmd = redis::cmd("MSET");
            for s in 0..slots {
                cmd.arg(cas_key(tag, w, s)).arg(cas_value(0, size));
            }
            cmd.query_async::<()>(&mut c).await?;
        }
        redis::cmd("SCRIPT")
            .arg("LOAD")
            .arg(CAS_LUA)
            .query_async(&mut c)
            .await?
    };
    let conns = resp_conns(client, args.concurrency).await?;
    let start = Instant::now();
    let mut handles = Vec::new();
    for (w, mut con) in conns.into_iter().enumerate() {
        let sha = sha.clone();
        handles.push(tokio::spawn(async move {
            let mut out = WorkerOut::with_capacity(n_req);
            let mut counters = vec![0u64; slots];
            for r in 0..n_req {
                let s = r % slots;
                let t = Instant::now();
                let swapped = matches!(
                    redis::cmd("EVALSHA")
                        .arg(&sha)
                        .arg(1)
                        .arg(cas_key(tag, w, s))
                        .arg(cas_value(counters[s], size))
                        .arg(cas_value(counters[s] + 1, size))
                        .query_async::<i64>(&mut con)
                        .await,
                    Ok(1)
                );
                out.record(t, swapped);
                if swapped {
                    counters[s] += 1;
                }
            }
            out
        }));
    }
    let mut outs = Vec::new();
    for h in handles {
        outs.push(h.await?);
    }
    Ok(PhaseResult::new("CAS", actual, 1, start.elapsed(), outs))
}
