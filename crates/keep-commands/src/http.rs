//! HTTP/2 + OpenAPI surface for keep.
//!
//! The transport is axum over hyper (HTTP/1.1 and HTTP/2 on the same port).
//! The engine and persistence layers are transport-agnostic; everything here
//! is a thin, typed adapter onto [`crate::engine::KvEngine`].

pub use keep_access::auth;
pub mod error;
pub mod handlers;
pub mod hash;
pub mod lists;
pub mod meta;
pub mod metrics;
pub mod models;
pub mod openapi;
pub mod routes;
pub mod sets;
pub mod waiters;
pub mod zsets;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use metrics_prometheus::Sample;

use crate::engine::KvEngine;

/// Default request-body limit (16 MiB) — bounds claim-check blob writes.
pub const DEFAULT_BODY_LIMIT: usize = 16 * 1024 * 1024;

/// Shared state behind every handler.
#[derive(Clone)]
pub struct AppState {
    pub engine: Arc<KvEngine>,
    /// Max request body size, applied as a router layer.
    pub body_limit: usize,
    /// Per-route HTTP request metrics (counts + latency histograms).
    pub metrics: Arc<metrics::HttpMetrics>,
    /// Per-key wait registry for blocking list pops (BLPOP/BRPOP).
    pub waiters: Arc<waiters::ListWaiters>,
    /// Cluster topology / sharding (single-node by default).
    pub cluster: crate::cluster::Cluster,
    /// Optional claim-check token verifier (#446, #746). Wraps the scoped HMAC
    /// secret in keep's [`auth::KeepVerifier`] (a `service_auth::Verifier`). When
    /// `Some`, worker ops (GET input / PUT result) require a valid in-scope
    /// token; when `None`, claim-check is open (backward compatible).
    pub verifier: Option<Arc<auth::KeepVerifier>>,
    /// Optional blanket API token (`KEEP_API_TOKEN`) for data-plane endpoints.
    pub api_token: Option<Arc<String>>,
    /// Optional read-only bearer token (KEEP_API_TOKEN_READONLY): GET/HEAD data-plane routes only.
    pub read_only_token: Option<Arc<String>>,
    /// Per-shard raft hosts used by replica-mode write handlers. Absent in the
    /// default/single-node fast path.
    #[cfg(feature = "raft")]
    pub raft_hosts: Option<Arc<crate::raft::ShardHosts>>,
    draining: Arc<AtomicBool>,
}

impl AppState {
    pub fn new(engine: Arc<KvEngine>) -> Self {
        Self {
            engine,
            body_limit: DEFAULT_BODY_LIMIT,
            metrics: Arc::new(metrics::HttpMetrics::default()),
            waiters: Arc::new(waiters::ListWaiters::default()),
            cluster: Arc::new(crate::cluster::ClusterConfig::default()),
            verifier: None,
            api_token: None,
            read_only_token: None,
            #[cfg(feature = "raft")]
            raft_hosts: None,
            draining: Arc::new(AtomicBool::new(false)),
        }
    }

    pub fn with_body_limit(mut self, body_limit: usize) -> Self {
        self.body_limit = body_limit;
        self
    }

    /// Enable scoped claim-check token enforcement with `secret` (#446). Builds
    /// keep's [`auth::KeepVerifier`] so the shared `service_auth::Verifier`
    /// composes `libs/claim-token` while the per-handler scope decision stays in
    /// keep (#746).
    pub fn with_token_secret(mut self, secret: Vec<u8>) -> Self {
        self.verifier = Some(Arc::new(auth::KeepVerifier::new(Arc::new(secret))));
        self
    }

    /// Enable blanket API token enforcement on data-plane routes.
    pub fn with_api_token(mut self, token: impl Into<String>) -> Self {
        let token = token.into();
        if !token.is_empty() {
            self.api_token = Some(Arc::new(token));
        }
        self
    }

    /// Enable the read-only bearer token: it may call GET/HEAD data-plane routes only.
    pub fn with_read_only_token(mut self, token: impl Into<String>) -> Self {
        let token = token.into();
        if !token.is_empty() {
            self.read_only_token = Some(Arc::new(token));
        }
        self
    }

    pub fn with_cluster(mut self, cluster: crate::cluster::ClusterConfig) -> Self {
        self.cluster = Arc::new(cluster);
        self
    }

    #[cfg(feature = "raft")]
    pub fn with_raft_hosts(mut self, hosts: Arc<crate::raft::ShardHosts>) -> Self {
        self.raft_hosts = Some(hosts);
        self
    }

    /// Flip readiness to draining so `/readyz` returns 503. Called on SIGTERM.
    pub fn start_drain(&self) {
        self.draining.store(true, Ordering::SeqCst);
    }

    pub fn is_draining(&self) -> bool {
        self.draining.load(Ordering::SeqCst)
    }
}

/// Readiness source for the shared probe router (`service_http`): `/readyz`
/// reports 503 once SIGTERM flips `start_drain`.
impl service_http::ReadinessHook for AppState {
    fn is_draining(&self) -> bool {
        self.draining.load(Ordering::SeqCst)
    }
}

/// Prometheus exposition for the shared `/metrics` route: engine gauges plus the
/// per-route HTTP request metrics. This is the body the local `handlers::metrics`
/// produced before keep adopted the shared probe router.
impl service_http::MetricsProvider for AppState {
    fn render_metrics(&self) -> String {
        let keys = self.engine.len();
        let shards = self.engine.num_shards();
        let mem = self.engine.estimate_memory();
        let maxmem = self.engine.get_maxmemory();
        let disk_bytes = self.engine.used_disk_bytes();
        let evicted = self.engine.evicted_keys();
        let spilled = self.engine.spilled_keys();
        let mut out = metrics_prometheus::render(&[
            Sample::new(
                "keep_keys_total",
                "gauge",
                "Number of keys across all shards.",
                keys as u64,
            ),
            Sample::new(
                "keep_shards",
                "gauge",
                "Number of engine shards.",
                shards as u64,
            ),
            Sample::new(
                "keep_memory_bytes",
                "gauge",
                "Estimated resident bytes of stored data.",
                mem as u64,
            ),
            Sample::new(
                "keep_maxmemory_bytes",
                "gauge",
                "Configured maximum memory limit in bytes (0 = unlimited).",
                maxmem as u64,
            ),
            Sample::new(
                "keep_disk_blob_bytes",
                "gauge",
                "Bytes stored in tiered disk blob spill files.",
                disk_bytes as u64,
            ),
            Sample::new(
                "keep_evicted_keys_total",
                "counter",
                "Total keys evicted due to memory pressure.",
                evicted,
            ),
            Sample::new(
                "keep_spilled_keys_total",
                "counter",
                "Total keys spilled to disk blob storage.",
                spilled,
            ),
        ]);
        out.push_str(&self.metrics.render());
        out
    }
}

pub use openapi::ApiDoc;
pub use routes::router;
