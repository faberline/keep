//! The keep server (the default, no-subcommand path): flags, startup and drain.

use std::net::SocketAddr;

use std::path::PathBuf;

use std::sync::Arc;

use std::time::Duration;

use anyhow::Result;

use tokio::net::TcpListener;

use tracing::{info, warn};

use keep::persistence::recovery::RecoveryManager;

use keep::persistence::{PersistenceConfig, PersistenceHandle};

use keep::{AppState, KvEngine};

use crate::serve_config::{
    derive_cluster_config, detect_cgroup_memory_limit, parse_byte_size, resolve_maxmemory,
};

#[derive(clap::Args, Debug)]
pub(crate) struct ServeArgs {
    /// Bind host. k8s passes 0.0.0.0.
    #[arg(long, env = "KEEP_HOST", default_value = "127.0.0.1")]
    pub(crate) host: String,
    /// HTTP port (HTTP/1.1 + HTTP/2 cleartext on the same port).
    #[arg(long, env = "KEEP_PORT", default_value_t = 7117)]
    pub(crate) port: u16,
    /// Tokio worker thread count (0 = auto-detect from CPU cores / cgroup quota).
    #[arg(long, env = "KEEP_WORKER_THREADS", default_value_t = 0)]
    pub(crate) worker_threads: usize,
    /// Engine shard count (multi-core scaling; 0 = auto-scale from worker threads).
    #[arg(long, env = "KEEP_SHARDS", default_value_t = 256)]
    pub(crate) shards: usize,
    /// Maximum resident memory for engine entries (e.g. `512Mi`, `4Gi`, or bytes;
    /// `0` = unlimited). When unset, defaults to `maxmemory_ratio` of detected
    /// cgroup memory limit if running inside a memory-limited container.
    #[arg(long, env = "KEEP_MAXMEMORY")]
    pub(crate) maxmemory: Option<String>,
    /// Fraction of container cgroup memory limit to use when `--maxmemory` is unset.
    #[arg(long, env = "KEEP_MAXMEMORY_RATIO", default_value_t = 0.80)]
    pub(crate) maxmemory_ratio: f64,
    /// Memory eviction policy (`noeviction`, `allkeys-lru`, `volatile-lru`,
    /// `allkeys-lfu`, `allkeys-random`).
    #[arg(long, env = "KEEP_EVICTION_POLICY", default_value = "allkeys-lru")]
    pub(crate) eviction_policy: String,
    /// Byte size threshold above which `Bytes` payloads spill to `/data/blobs/`
    /// when persistence is enabled (`0` disables disk blob tiering).
    #[arg(long, env = "KEEP_SPILL_THRESHOLD_BYTES", default_value_t = 65_536)]
    pub(crate) spill_threshold_bytes: usize,
    /// Maximum disk usage for tiered blob storage (e.g. `10Gi`, `50Gi`, or bytes;
    /// unset or `0` = unlimited).
    #[arg(long, env = "KEEP_MAX_DISK_BYTES")]
    pub(crate) max_disk_bytes: Option<String>,
    /// Background active TTL expiry sweep interval in milliseconds (`0` disables).
    #[arg(long, env = "KEEP_EXPIRY_INTERVAL_MS", default_value_t = 1000)]
    pub(crate) expiry_interval_ms: u64,
    /// Optional blanket Bearer API token required on data-plane routes.
    #[arg(long, env = "KEEP_API_TOKEN")]
    pub(crate) api_token: Option<String>,
    /// PEM certificate chain; with --tls-key, serve the public API over HTTPS.
    #[arg(long, env = "KEEP_TLS_CERT")]
    pub(crate) tls_cert: Option<PathBuf>,
    /// PEM private key for --tls-cert.
    #[arg(long, env = "KEEP_TLS_KEY")]
    pub(crate) tls_key: Option<PathBuf>,
    /// Optional read-only Bearer token: may only GET/HEAD data-plane routes (403 on writes).
    #[arg(long, env = "KEEP_API_TOKEN_READONLY")]
    pub(crate) api_token_readonly: Option<String>,
    /// `trace|debug|info|warn|error` (RUST_LOG overrides this).
    #[arg(long, env = "KEEP_LOG_LEVEL", default_value = "info")]
    pub(crate) log_level: String,
    /// Stdout format: `pretty` for local use or collector-compatible `json`.
    #[arg(
        long,
        env = "KEEP_LOG_FORMAT",
        default_value = "pretty",
        value_parser = ["pretty", "json"]
    )]
    pub(crate) log_format: String,
    /// Optional shared OTLP endpoint for trace export; Sift collector routing
    /// remains entirely deployment-owned.
    #[arg(long, env = "KEEP_OTLP_ENDPOINT")]
    pub(crate) otlp_endpoint: Option<String>,
    /// Data directory for WAL + snapshots + tiered blobs (the disk tier).
    #[arg(long, env = "KEEP_DATA_DIR", default_value = "./data")]
    pub(crate) data_dir: PathBuf,
    /// Run in-memory only (no durability).
    #[arg(long, env = "KEEP_DISABLE_PERSISTENCE", default_value_t = false)]
    pub(crate) disable_persistence: bool,
    /// WAL fsync interval (ms).
    #[arg(long, env = "KEEP_FSYNC_MS", default_value_t = 100)]
    pub(crate) fsync_interval_ms: u64,
    /// Snapshot interval (s).
    #[arg(long, env = "KEEP_SNAPSHOT_SECS", default_value_t = 300)]
    pub(crate) snapshot_interval_secs: u64,
    /// Snapshot trigger threshold (op count).
    #[arg(long, env = "KEEP_SNAPSHOT_OPS", default_value_t = 100_000)]
    pub(crate) snapshot_ops_threshold: usize,
    /// Max request body size (bytes) — bounds claim-check blob writes.
    #[arg(long, env = "KEEP_BODY_LIMIT", default_value_t = keep::http::DEFAULT_BODY_LIMIT)]
    pub(crate) body_limit: usize,
    /// Graceful drain window on SIGTERM (s).
    #[arg(long, env = "KEEP_GRACE_SECS", default_value_t = 30)]
    pub(crate) grace_secs: u64,
    /// This node's ordinal in the cluster (k8s StatefulSet pod index).
    #[arg(long, env = "KEEP_NODE_ID", default_value_t = 0)]
    pub(crate) node_id: usize,
    /// Total nodes in the cluster.
    #[arg(long, env = "KEEP_NODE_COUNT", default_value_t = 1)]
    pub(crate) node_count: usize,
    /// Virtual shard count for client-side routing (>= node_count).
    #[arg(long, env = "KEEP_SHARD_COUNT", default_value_t = 1)]
    pub(crate) shard_count: u32,
    /// Peer base URLs (comma-separated), index = node ordinal.
    #[arg(long, env = "KEEP_PEERS", value_delimiter = ',')]
    pub(crate) peers: Vec<String>,
}

// <HANDWRITE gap="missing-generator:logic" tracker="#2414" reason="logic section in keep.rs is hand-written pending codegen support">
/// Run the keep server (the default, no-subcommand path).
pub(crate) async fn serve_main(args: ServeArgs, worker_threads: usize) -> Result<()> {
    // Stdout is the telemetry boundary: the shared observability shell owns the
    // schema, resource attributes, and RUST_LOG precedence so every service
    // emits the one record shape sift ingests.
    let log_format = match args.log_format.as_str() {
        "pretty" => service_http::LogFormat::Pretty,
        "json" => service_http::LogFormat::Json,
        value => anyhow::bail!("--log-format must be `pretty` or `json`, got `{value}`"),
    };
    let config = service_http::HttpConfig::new(
        args.host.clone(),
        args.port,
        args.log_level.clone(),
        log_format,
        args.grace_secs,
        args.body_limit,
        args.otlp_endpoint.clone().filter(|e| !e.is_empty()),
    );
    let identity = service_http::ServiceIdentity::new("keep", env!("CARGO_PKG_VERSION"))?;
    service_http::init_tracing_with_identity(&config, &identity)?;

    let shards = if args.shards == 0 {
        (worker_threads.saturating_mul(16))
            .next_power_of_two()
            .clamp(16, 1024)
    } else {
        args.shards
    };
    let eviction_policy =
        keep::EvictionPolicy::from_str_name(&args.eviction_policy).ok_or_else(|| {
            anyhow::anyhow!(
                "invalid --eviction-policy `{}`; expected one of: noeviction, allkeys-lru, volatile-lru, allkeys-lfu, allkeys-random",
                args.eviction_policy
            )
        })?;
    let max_memory = resolve_maxmemory(
        args.maxmemory.as_deref(),
        args.maxmemory_ratio,
        detect_cgroup_memory_limit(),
    )?;
    let max_disk_bytes = match args.max_disk_bytes.as_deref() {
        Some(raw) if !raw.trim().is_empty() => {
            let parsed = parse_byte_size(raw)?;
            (parsed > 0).then_some(parsed)
        }
        _ => None,
    };

    let addr: SocketAddr = format!("{}:{}", args.host, args.port).parse()?;
    let tls_config = match (args.tls_cert.as_deref(), args.tls_key.as_deref()) {
        (None, None) => None,
        (Some(cert), Some(key)) => Some(keep::https::load_server_config(cert, key)?),
        _ => anyhow::bail!(
            "--tls-cert and --tls-key (KEEP_TLS_CERT / KEEP_TLS_KEY) must be set together"
        ),
    };
    info!(
        %addr,
        worker_threads,
        shards,
        max_memory = ?max_memory,
        eviction_policy = eviction_policy.as_str(),
        spill_threshold_bytes = args.spill_threshold_bytes,
        max_disk_bytes = ?max_disk_bytes,
        "starting keep"
    );

    // Engine + persistence: recover from disk on a cold start.
    let (engine, persistence) = if args.disable_persistence {
        info!("persistence disabled — in-memory only");
        let engine = KvEngine::with_shards(shards);
        if let Some(limit) = max_memory {
            engine.set_maxmemory(limit);
        }
        engine.set_eviction_policy(eviction_policy);
        (Arc::new(engine), None)
    } else {
        info!(data_dir = %args.data_dir.display(), "recovering engine");
        let recovered = KvEngine::with_shards(shards);
        recovered.set_eviction_policy(eviction_policy);
        if let Some(limit) = max_memory {
            recovered.set_maxmemory(limit);
        }
        if args.spill_threshold_bytes > 0 {
            let blob_store = Arc::new(keep::persistence::BlobStore::new(
                &args.data_dir,
                args.spill_threshold_bytes,
                max_disk_bytes.unwrap_or(0),
            )?);
            recovered.enable_blob_store(blob_store);
        }
        let stats = RecoveryManager::recover_into(&recovered, &args.data_dir)?;
        info!(
            snapshot_entries = stats.snapshot_entries,
            wal_replayed = stats.wal_entries_replayed,
            corrupted = stats.corrupted_entries,
            "recovery complete"
        );
        let config = PersistenceConfig::new(&args.data_dir)
            .with_fsync_interval_ms(args.fsync_interval_ms)
            .with_snapshot_interval_secs(args.snapshot_interval_secs)
            .with_snapshot_ops_threshold(args.snapshot_ops_threshold);
        let engine = Arc::new(recovered);
        let persistence = Arc::new(PersistenceHandle::new(config, engine.clone())?);
        engine.enable_persistence(persistence.clone());
        (engine, Some(persistence))
    };

    // Background active TTL expiration reaper task so expired keys (and their
    // spilled disk blobs) are reclaimed promptly without waiting for reads.
    let expiry_task = if args.expiry_interval_ms > 0 {
        let reaper_engine = engine.clone();
        let interval = Duration::from_millis(args.expiry_interval_ms);
        Some(tokio::spawn(async move {
            let mut ticker = tokio::time::interval(interval);
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                ticker.tick().await;
                let _ = reaper_engine.cleanup_expired();
            }
        }))
    } else {
        None
    };

    let cluster = derive_cluster_config(&args)?;
    info!(
        node_id = cluster.node_id,
        node_count = cluster.node_count,
        shard_count = cluster.shard_count,
        owned_shards = cluster.owned_shards().len(),
        "cluster topology"
    );
    let mut state = AppState::new(engine)
        .with_body_limit(args.body_limit)
        .with_cluster(cluster);
    if let Some(token) = args.api_token.as_deref() {
        if !token.is_empty() {
            state = state.with_api_token(token);
            tracing::info!("blanket API token enforcement ON");
        }
    }
    if let Some(token) = args.api_token_readonly.as_deref() {
        if !token.is_empty() {
            state = state.with_read_only_token(token);
            tracing::info!("read-only API token enforcement ON");
        }
    }
    // Scoped claim-check tokens (#446): enforce when KEEP_TOKEN_SECRET is set.
    if let Ok(secret) = std::env::var("KEEP_TOKEN_SECRET") {
        if !secret.is_empty() {
            state = state.with_token_secret(secret.into_bytes());
            tracing::info!("claim-check token enforcement ON");
        }
    }
    // Peer raft RPCs (`/shard/{id}/raft/*`, `/shard/{id}/raftz`) share the h2c
    // serve port. Built only when the raft feature is compiled in and k8s has
    // scaled the StatefulSet into replica/HA mode (REPLICAS_PER_SHARD > 1);
    // single-node deployments keep the direct-to-engine write path. The hosts
    // are held for the server's lifetime (Drop aborts their tick/pump tasks).
    #[cfg(feature = "raft")]
    let shard_hosts = if raft_runtime::cluster::replica_mode() {
        let replicas = std::env::var("KEEP_REPLICAS_PER_SHARD")
            .ok()
            .and_then(|v| v.parse::<u32>().ok())
            .unwrap_or(1);
        let hosts = Arc::new(
            keep::raft::ShardHosts::new(
                (*state.cluster).clone(),
                state.engine.clone(),
                &args.data_dir,
                replicas,
            )
            .await?,
        );
        info!(
            shard_hosts = hosts.host_count(),
            replicas_per_shard = replicas,
            "raft: per-shard hosts up; peer transport merged onto serve port"
        );
        Some(hosts)
    } else {
        None
    };
    #[cfg(feature = "raft")]
    if let Some(hosts) = shard_hosts.clone() {
        state = state.with_raft_hosts(hosts);
    }

    #[cfg_attr(not(feature = "raft"), allow(unused_mut))]
    let mut app = keep::router(state.clone());
    #[cfg(feature = "raft")]
    if let Some(hosts) = &shard_hosts {
        app = app.merge(hosts.router());
    }

    let listener = TcpListener::bind(addr).await?;

    // Serve HTTP/1.1 + h2c (or HTTPS when configured) on one port and drain on
    // SIGTERM. The signal wait and the drain grace come from the shared
    // `server-lifecycle` crate (#751): `start_drain` flips `/readyz` to 503 for
    // the grace window before the listener closes.
    let grace = Duration::from_secs(args.grace_secs);
    let shutdown = server_lifecycle::shutdown_with_drain(move || state.start_drain(), grace);
    match tls_config {
        Some(cfg) => {
            info!(%addr, "listening (HTTPS: HTTP/1.1 + HTTP/2 over TLS)");
            keep::https::serve_tls(listener, app, cfg, shutdown).await
        }
        None => {
            info!(%addr, "listening (HTTP/1.1 + HTTP/2 cleartext)");
            service_http::serve(listener, app, shutdown).await
        }
    }

    if let Some(handle) = expiry_task {
        handle.abort();
    }

    // Post-drain: flush WAL/snapshot to disk so the result store is durable.
    if let Some(p) = persistence {
        info!("flushing persistence");
        match Arc::try_unwrap(p) {
            Ok(p) => {
                if let Err(e) = p.shutdown() {
                    warn!(error = %e, "persistence shutdown error");
                }
            }
            Err(_) => warn!("persistence still shared — relying on Drop"),
        }
    }
    info!("shutdown complete");
    Ok(())
}
// </HANDWRITE>
