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
//!
//! Phases (`--phases`, default `set,get`): SET, GET, and CAS. CAS is single-key
//! compare-and-swap with one round trip per op: keep `POST /kv/{k}/cas`, RESP
//! `EVALSHA` of a GET-compare-SET Lua script (atomic on every RESP server).
//! CAS keys are seeded before the timed region and each worker owns its keys,
//! so every swap is expected to apply; a swap that does not apply is an error.
//!
//! `--tier ram|tiered` checks the server's actual storage mode before measuring
//! (keep `/info`, RESP `CONFIG GET appendonly/appendfsync`) and fails on a mismatch.
//!
//! `--warmup-ops N` runs N unmeasured SET+GET ops (keep and RESP backends) before the timed phases.

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
    /// Comma-separated phases, run in order: set, get, cas.
    #[arg(long, default_value = "set,get", value_delimiter = ',')]
    phases: Vec<String>,
    /// Output: `text` (human) or `json` (one keep.bench_compare/1 line on stdout).
    #[arg(long, default_value = "text", value_parser = ["text", "json"])]
    format: String,
    /// Free-form label echoed into the JSON report.
    #[arg(long)]
    label: Option<String>,
    /// Exit 0 even when requests failed (errors are still reported).
    #[arg(long, default_value_t = false)]
    allow_errors: bool,
    /// Required server storage mode: `ram` (no durable log) or `tiered`
    /// (durable-before-ack log). Checked before measuring; unset = not checked.
    #[arg(long, value_parser = ["ram", "tiered"])]
    tier: Option<String>,
    /// Poll the server for up to this many seconds before measuring.
    #[arg(long, default_value_t = 0)]
    wait_ready_secs: u64,
    /// Unmeasured SET+GET operations run before the timed phases (0 = no warmup).
    #[arg(long, default_value_t = 0)]
    warmup_ops: usize,
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

/// Tier of the in-process engine: durable when it has a persist dir.
fn engine_tier(persist: bool) -> &'static str {
    if persist {
        "tiered"
    } else {
        "ram"
    }
}

/// Tier of a keep server from its `/info` JSON.
fn keep_tier(info: &Value) -> BenchResult<&'static str> {
    match info["persistence_enabled"].as_bool() {
        Some(true) => Ok("tiered"),
        Some(false) => Ok("ram"),
        None => {
            Err("keep /info has no persistence_enabled (server predates the tier check)".into())
        }
    }
}

/// Tier of a keep server and whether it is verified. A server without the
/// field gives an unverified `unknown` tier, and an error only when `--tier`
/// asks for a check.
fn keep_tier_checked(info: &Value, want: Option<&str>) -> BenchResult<(&'static str, bool)> {
    match keep_tier(info) {
        Ok(tier) => Ok((tier, true)),
        Err(_) if want.is_none() => Ok(("unknown", false)),
        Err(err) => Err(err),
    }
}

/// Tier of a RESP server from `CONFIG GET appendonly` / `appendfsync`, and
/// whether that tier is verified. Without an AOF config surface the tier is
/// unknown and unverified, never an error.
fn resp_tier(appendonly: Option<&str>, appendfsync: Option<&str>) -> (&'static str, bool) {
    match (appendonly, appendfsync) {
        (Some("yes"), Some("always")) => ("tiered", true),
        (Some("yes"), Some(_)) => ("tiered-relaxed", true),
        (Some("no"), _) => ("ram", true),
        _ => ("unknown", false),
    }
}

/// Fails only when a tier was asked for and the server verifiably reports another.
fn check_tier(want: Option<&str>, tier: &str, verified: bool, who: &str) -> BenchResult<()> {
    match want {
        Some(want) if verified && want != tier => {
            Err(format!("--tier {want} but {who} reports {tier}").into())
        }
        _ => Ok(()),
    }
}

fn keep_pool(args: &Args) -> BenchResult<transport_h2c::H2cPool> {
    let n_clients = args.keep_clients.unwrap_or(args.concurrency).max(1);
    // The 10s timeout bounds a single request so a stalled stream can't hang the run.
    Ok(transport_h2c::H2cPool::with_connections_and(
        n_clients,
        Some(Duration::from_secs(10)),
        None,
    )?)
}

/// Polls keep `/readyz` every 200 ms until it succeeds or `--wait-ready-secs` passes.
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

/// Pings the RESP server every 200 ms until it answers or `--wait-ready-secs` passes.
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

/// Reads the server's storage mode and checks it against `--tier`.
async fn detect_mode(args: &Args) -> BenchResult<Mode> {
    match args.backend.as_str() {
        "engine" => {
            let persist = args.persist.is_some();
            let tier = engine_tier(persist);
            check_tier(
                args.tier.as_deref(),
                tier,
                true,
                "the engine (see --persist)",
            )?;
            Ok(Mode {
                tier: tier.into(),
                verified: true,
                detail: format!("in-process engine, persist={persist}"),
            })
        }
        "keep" => {
            let pool = keep_pool(args)?;
            keep_wait_ready(args, &pool).await?;
            let info: Value = pool
                .get(format!("{}/info", args.keep_url))
                .send()
                .await?
                .error_for_status()?
                .json()
                .await?;
            let (tier, verified) = keep_tier_checked(&info, args.tier.as_deref())?;
            check_tier(args.tier.as_deref(), tier, verified, "keep")?;
            Ok(Mode {
                tier: tier.into(),
                verified,
                detail: if verified {
                    format!(
                        "persistence_enabled={} fsync_interval_ms={}",
                        tier == "tiered",
                        info["fsync_interval_ms"]
                    )
                } else {
                    "keep /info has no persistence_enabled".into()
                },
            })
        }
        "keepnoop" => {
            let pool = keep_pool(args)?;
            keep_wait_ready(args, &pool).await?;
            Ok(Mode {
                tier: "unknown".into(),
                verified: false,
                detail: "backend=keepnoop".into(),
            })
        }
        b if RESP_BACKENDS.contains(&b) => {
            let client = redis::Client::open(args.addr.clone())?;
            resp_wait_ready(args, &client).await?;
            let mut con = client.get_multiplexed_async_connection().await?;
            let appendonly = config_get(&mut con, "appendonly").await;
            let appendfsync = config_get(&mut con, "appendfsync").await;
            let (tier, verified) = resp_tier(appendonly.as_deref(), appendfsync.as_deref());
            // Unverified: the tier is whatever the server was launched with.
            let tier = if verified {
                tier
            } else {
                args.tier.as_deref().unwrap_or("unknown")
            };
            check_tier(args.tier.as_deref(), tier, verified, &args.backend)?;
            Ok(Mode {
                tier: tier.into(),
                verified,
                detail: format!(
                    "appendonly={} appendfsync={}",
                    appendonly.as_deref().unwrap_or("unsupported"),
                    appendfsync.as_deref().unwrap_or("unsupported")
                ),
            })
        }
        other => Err(format!("unknown backend: {other}").into()),
    }
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
    // Checked before any load, so a wrong storage mode fails before measuring.
    let mode = detect_mode(args).await?;
    if let Some(warm) = warmup_args(args) {
        note!(args, "  (warmup: {} ops, not measured)", warm.ops);
        let _ = match warm.backend.as_str() {
            "keep" => bench_keep(&warm, &val, &[Phase::Set, Phase::Get]).await?,
            b if RESP_BACKENDS.contains(&b) => {
                bench_resp(&warm, &val, &[Phase::Set, Phase::Get]).await?
            }
            _ => Vec::new(),
        };
    }
    let results = match args.backend.as_str() {
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

/// The args of the unmeasured warmup pass, or `None` when no warmup runs.
fn warmup_args(args: &Args) -> Option<Args> {
    let has_set_or_get = args
        .phases
        .iter()
        .map(|p| p.trim().to_ascii_lowercase())
        .any(|p| p == "set" || p == "get");
    if args.warmup_ops == 0
        || args.backend == "engine"
        || args.backend == "keepnoop"
        || !has_set_or_get
    {
        return None;
    }
    Some(Args {
        ops: args.warmup_ops,
        phases: vec!["set".into(), "get".into()],
        format: "json".into(),
        warmup_ops: 0,
        ..args.clone()
    })
}

/// requests-per-worker and actual measured op count for the chosen batch.
fn plan(args: &Args) -> (usize, usize) {
    let batch = args.batch.max(1);
    let per_keys = args.ops / args.concurrency;
    let n_req = (per_keys / batch).max(1);
    let actual_ops = args.concurrency * n_req * batch;
    (n_req, actual_ops)
}

/// CAS keys are worker-owned: `benchcas:<tag>:<worker>:<slot>`.
fn cas_key(tag: &str, worker: usize, slot: usize) -> String {
    format!("benchcas:{tag}:{worker}:{slot}")
}

/// The value a CAS key holds after `n` swaps, padded to `size`.
fn cas_value(n: u64, size: usize) -> String {
    format!("{n:0>size$}")
}

/// CAS keys per worker: the worker cycles through these, one swap per op.
fn cas_slots(args: &Args, n_req: usize) -> usize {
    (args.keyspace / args.concurrency).clamp(1, n_req.max(1))
}

// --------------------------------------------------------------------------
// engine
// --------------------------------------------------------------------------
async fn bench_engine(args: &Args, val: &str, phases: &[Phase]) -> BenchResult<Vec<PhaseResult>> {
    if phases.contains(&Phase::Cas) {
        return Err("the engine backend measures set/get only".into());
    }
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

    for &phase in phases {
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
            PhaseResult::new(phase.label(), actual, batch, start.elapsed(), all),
            &mut results,
        );
    }
    Ok(results)
}

// --------------------------------------------------------------------------
// keep over HTTP/2 + JSON
// --------------------------------------------------------------------------
async fn bench_keep(args: &Args, val: &str, phases: &[Phase]) -> BenchResult<Vec<PhaseResult>> {
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

    for &phase in phases {
        if phase == Phase::Cas {
            emit(
                args,
                keep_cas_phase(args, &pool, args.value_size).await?,
                &mut results,
            );
            continue;
        }
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
                        let resp = if phase == Phase::Set {
                            client.put(&url).json(&json!({ "value": val })).send().await
                        } else {
                            client.get(&url).send().await
                        };
                        matches!(&resp, Ok(r) if r.status().is_success())
                    } else if phase == Phase::Set {
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
            PhaseResult::new(phase.label(), actual, batch, start.elapsed(), all),
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
async fn bench_resp(args: &Args, val: &str, phases: &[Phase]) -> BenchResult<Vec<PhaseResult>> {
    let client = redis::Client::open(args.addr.clone())?;
    {
        let mut c = client.get_multiplexed_async_connection().await?;
        redis::cmd("PING").query_async::<String>(&mut c).await?;
    }
    let (n_req, actual) = plan(args);
    let batch = args.batch.max(1);
    let mut results = Vec::new();

    for &phase in phases {
        if phase == Phase::Cas {
            emit(
                args,
                resp_cas_phase(args, &client, args.value_size).await?,
                &mut results,
            );
            continue;
        }
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
                    let ok = if phase == Phase::Set {
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
            PhaseResult::new(phase.label(), actual, batch, start.elapsed(), all),
            &mut results,
        );
    }
    Ok(results)
}

// --------------------------------------------------------------------------
// CAS: single key, one round trip per op, every swap expected to apply
// --------------------------------------------------------------------------
async fn keep_cas_phase(
    args: &Args,
    pool: &transport_h2c::H2cPool,
    size: usize,
) -> BenchResult<PhaseResult> {
    let one = Args {
        batch: 1,
        ..args.clone()
    };
    let (n_req, actual) = plan(&one);
    let slots = cas_slots(args, n_req);
    // Seed every worker-owned key outside the timed region.
    for w in 0..args.concurrency {
        for s in 0..slots {
            let url = format!("{}/kv/{}", args.keep_url, cas_key("timed", w, s));
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
                let url = format!("{base_url}/kv/{}/cas", cas_key("timed", w, s));
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

async fn resp_cas_phase(
    args: &Args,
    client: &redis::Client,
    size: usize,
) -> BenchResult<PhaseResult> {
    let one = Args {
        batch: 1,
        ..args.clone()
    };
    let (n_req, actual) = plan(&one);
    let slots = cas_slots(args, n_req);
    let sha: String = {
        let mut c = client.get_multiplexed_async_connection().await?;
        // Seed every worker-owned key outside the timed region.
        for w in 0..args.concurrency {
            let mut cmd = redis::cmd("MSET");
            for s in 0..slots {
                cmd.arg(cas_key("timed", w, s)).arg(cas_value(0, size));
            }
            cmd.query_async::<()>(&mut c).await?;
        }
        redis::cmd("SCRIPT")
            .arg("LOAD")
            .arg(CAS_LUA)
            .query_async(&mut c)
            .await?
    };
    // pre-establish one connection per worker, OUTSIDE the timed region.
    let conns: Vec<_> = futures::future::join_all(
        (0..args.concurrency).map(|_| client.get_multiplexed_async_connection()),
    )
    .await
    .into_iter()
    .collect::<Result<_, _>>()?;
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
                        .arg(cas_key("timed", w, s))
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

    #[test]
    fn phase_parse_accepts_set_get_cas() {
        assert_eq!(Phase::parse("set").unwrap(), Phase::Set);
        assert_eq!(Phase::parse(" GET ").unwrap(), Phase::Get);
        assert_eq!(Phase::parse("Cas").unwrap(), Phase::Cas);
        assert_eq!(Phase::Set.label(), "SET");
        assert_eq!(Phase::Get.label(), "GET");
        assert_eq!(Phase::Cas.label(), "CAS");
    }

    #[test]
    fn phase_parse_rejects_other_words() {
        let err = Phase::parse("del").unwrap_err();
        assert!(err.to_string().contains("unknown phase"));
    }

    #[test]
    fn phases_default_and_list() {
        let a = Args::parse_from(["bench_compare", "--backend", "engine"]);
        assert_eq!(a.phases, ["set", "get"]);
        let b = Args::parse_from([
            "bench_compare",
            "--backend",
            "engine",
            "--phases",
            "set,get,cas",
        ]);
        assert_eq!(b.phases, ["set", "get", "cas"]);
    }

    #[test]
    fn cas_key_is_worker_owned() {
        assert_eq!(cas_key("timed", 3, 7), "benchcas:timed:3:7");
    }

    #[test]
    fn cas_value_pads_to_size() {
        assert_eq!(cas_value(5, 4), "0005");
        assert_eq!(cas_value(12345, 3), "12345");
    }

    #[test]
    fn cas_slots_clamps() {
        let a = Args::parse_from([
            "bench_compare",
            "--backend",
            "engine",
            "--keyspace",
            "100",
            "--concurrency",
            "10",
        ]);
        assert_eq!(cas_slots(&a, 50), 10);
        assert_eq!(cas_slots(&a, 4), 4);
        let b = Args::parse_from([
            "bench_compare",
            "--backend",
            "engine",
            "--keyspace",
            "5",
            "--concurrency",
            "10",
        ]);
        assert_eq!(cas_slots(&b, 50), 1);
    }

    #[test]
    fn plan_counts_requests_and_ops() {
        let make = |ops: &str, batch: &str| {
            Args::parse_from([
                "bench_compare",
                "--backend",
                "engine",
                "--ops",
                ops,
                "--concurrency",
                "10",
                "--batch",
                batch,
            ])
        };
        assert_eq!(plan(&make("1000", "5")), (20, 1000));
        assert_eq!(plan(&make("1000", "0")), (100, 1000));
        assert_eq!(plan(&make("10", "5")), (1, 50));
    }

    #[test]
    fn engine_tier_follows_persist() {
        assert_eq!(engine_tier(true), "tiered");
        assert_eq!(engine_tier(false), "ram");
    }

    #[test]
    fn keep_tier_reads_persistence_enabled() {
        assert_eq!(
            keep_tier(&json!({"persistence_enabled": true})).unwrap(),
            "tiered"
        );
        assert_eq!(
            keep_tier(&json!({"persistence_enabled": false})).unwrap(),
            "ram"
        );
    }

    #[test]
    fn keep_tier_rejects_old_servers() {
        let missing = keep_tier(&json!({})).unwrap_err().to_string();
        assert!(missing.contains("persistence_enabled"), "{missing}");
        assert!(keep_tier(&json!({"persistence_enabled": "yes"})).is_err());
    }

    #[test]
    fn keep_tier_checked_old_server_without_tier_is_unknown() {
        assert_eq!(
            keep_tier_checked(&json!({}), None).unwrap(),
            ("unknown", false)
        );
        assert_eq!(
            keep_tier_checked(&json!({"persistence_enabled": "yes"}), None).unwrap(),
            ("unknown", false)
        );
    }

    #[test]
    fn keep_tier_checked_old_server_with_tier_errors() {
        let err = keep_tier_checked(&json!({}), Some("ram"))
            .unwrap_err()
            .to_string();
        assert!(err.contains("persistence_enabled"), "{err}");
    }

    #[test]
    fn keep_tier_checked_new_server_is_verified() {
        assert_eq!(
            keep_tier_checked(&json!({"persistence_enabled": true}), Some("tiered")).unwrap(),
            ("tiered", true)
        );
        assert_eq!(
            keep_tier_checked(&json!({"persistence_enabled": false}), None).unwrap(),
            ("ram", true)
        );
    }

    #[test]
    fn resp_tier_decisions() {
        assert_eq!(resp_tier(Some("yes"), Some("always")), ("tiered", true));
        assert_eq!(
            resp_tier(Some("yes"), Some("everysec")),
            ("tiered-relaxed", true)
        );
        assert_eq!(resp_tier(Some("no"), None), ("ram", true));
        assert_eq!(resp_tier(None, None), ("unknown", false));
        assert_eq!(resp_tier(Some("yes"), None), ("unknown", false));
    }

    #[test]
    fn check_tier_fails_only_on_verified_mismatch() {
        let err = check_tier(Some("ram"), "tiered", true, "keep")
            .unwrap_err()
            .to_string();
        assert!(err.contains("--tier ram"), "{err}");
        assert!(err.contains("tiered"), "{err}");
        assert!(check_tier(Some("ram"), "ram", true, "keep").is_ok());
        assert!(check_tier(None, "tiered", true, "keep").is_ok());
        assert!(check_tier(Some("ram"), "unknown", false, "redis").is_ok());
    }

    #[test]
    fn tier_and_wait_ready_args() {
        let a = Args::parse_from([
            "bench_compare",
            "--backend",
            "keep",
            "--tier",
            "ram",
            "--wait-ready-secs",
            "5",
        ]);
        assert_eq!(a.tier.as_deref(), Some("ram"));
        assert_eq!(a.wait_ready_secs, 5);
        let d = Args::parse_from(["bench_compare", "--backend", "keep"]);
        assert_eq!(d.tier, None);
        assert_eq!(d.wait_ready_secs, 0);
        assert!(
            Args::try_parse_from(["bench_compare", "--backend", "keep", "--tier", "disk"]).is_err()
        );
    }

    #[test]
    fn warmup_ops_arg_parses() {
        let a = Args::parse_from(["bench_compare", "--backend", "keep", "--warmup-ops", "7"]);
        assert_eq!(a.warmup_ops, 7);
        let d = Args::parse_from(["bench_compare", "--backend", "keep"]);
        assert_eq!(d.warmup_ops, 0);
    }

    #[test]
    fn warmup_args_is_none_when_off_or_not_applicable() {
        let none = |extra: &[&str]| {
            let mut v = vec!["bench_compare"];
            v.extend_from_slice(extra);
            warmup_args(&Args::parse_from(v)).is_none()
        };
        assert!(none(&["--backend", "keep", "--warmup-ops", "0"]));
        assert!(none(&["--backend", "engine", "--warmup-ops", "5"]));
        assert!(none(&["--backend", "keepnoop", "--warmup-ops", "5"]));
        assert!(none(&[
            "--backend",
            "keep",
            "--warmup-ops",
            "5",
            "--phases",
            "cas"
        ]));
    }

    #[test]
    fn warmup_args_builds_a_silent_set_get_pass() {
        let a = Args::parse_from([
            "bench_compare",
            "--backend",
            "redis",
            "--warmup-ops",
            "5",
            "--ops",
            "1000",
            "--phases",
            "set,get,cas",
            "--format",
            "text",
        ]);
        let w = warmup_args(&a).expect("warmup pass");
        assert_eq!(w.ops, 5);
        assert_eq!(w.phases, ["set", "get"]);
        assert_eq!(w.format, "json");
        assert_eq!(w.warmup_ops, 0);
        assert_eq!(w.backend, "redis");
        assert_eq!(a.ops, 1000);
    }

    #[test]
    fn perf_gate_args_are_all_accepted() {
        let r = Args::try_parse_from([
            "bench_compare",
            "--backend",
            "keep",
            "--keep-url",
            "http://x:1",
            "--addr",
            "redis://x:1",
            "--tier",
            "ram",
            "--phases",
            "set,get,cas",
            "--format",
            "json",
            "--wait-ready-secs",
            "60",
            "--ops",
            "1000",
            "--warmup-ops",
            "100",
            "--concurrency",
            "50",
            "--label",
            "t",
        ]);
        assert!(r.is_ok(), "{:?}", r.err());
    }
}
