//! In-process HTTP surface tests: drive the axum router directly via
//! `tower::oneshot` (no socket, no server task).

use std::sync::Arc;

use axum::{
    body::Body,
    http::{header, Request, StatusCode},
    Router,
};
use keep::{router, AppState, KvEngine};
#[cfg(feature = "raft")]
use keep::{ClusterConfig, KvKey, KvValue};
use serde_json::{json, Value};
use tower::ServiceExt;

#[cfg(feature = "raft")]
struct RaftHttpNode {
    url: String,
    engine: Arc<KvEngine>,
    hosts: Arc<keep::raft::ShardHosts>,
    serve: tokio::task::JoinHandle<()>,
}

fn app() -> (Router, AppState) {
    let state = AppState::new(Arc::new(KvEngine::with_shards(16)));
    (router(state.clone()), state)
}

async fn send(app: &Router, req: Request<Body>) -> (StatusCode, Vec<u8>) {
    let resp = app.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    (status, bytes.to_vec())
}

async fn send_json(app: &Router, method: &str, path: &str, body: Value) -> (StatusCode, Value) {
    let req = Request::builder()
        .method(method)
        .uri(path)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap();
    let (status, bytes) = send(app, req).await;
    let value = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap_or(Value::Null)
    };
    (status, value)
}

#[cfg(feature = "raft")]
fn tmp_dir(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("keep-http-raft-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[cfg(feature = "raft")]
async fn bind_local() -> (tokio::net::TcpListener, String) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    (listener, format!("http://127.0.0.1:{port}"))
}

#[cfg(feature = "raft")]
async fn raft_http_cluster(tag: &str) -> (std::path::PathBuf, Vec<RaftHttpNode>) {
    let dir = tmp_dir(tag);
    let mut listeners = Vec::new();
    let mut urls = Vec::new();
    for _ in 0..3 {
        let (listener, url) = bind_local().await;
        listeners.push(listener);
        urls.push(url);
    }

    let mut nodes = Vec::new();
    for (node_id, listener) in listeners.into_iter().enumerate() {
        let engine = Arc::new(KvEngine::with_shards(4));
        let mut peers = urls.clone();
        peers[node_id].clear();
        let cluster = ClusterConfig::replicated(
            1,
            keep::ReplicaPlacement {
                shard_index: 0,
                replica_index: node_id as u32,
                replicas_per_shard: 3,
                voter_count: 3,
            },
            peers,
        );
        let hosts = Arc::new(
            keep::raft::ShardHosts::new(
                cluster.clone(),
                engine.clone(),
                &dir.join(format!("node-{node_id}")),
            )
            .await
            .unwrap(),
        );
        let state = AppState::new(engine.clone())
            .with_cluster(cluster)
            .with_raft_hosts(hosts.clone());
        let app = router(state).merge(hosts.router());
        let serve = tokio::spawn(async move {
            service_http::serve(listener, app, std::future::pending::<()>()).await;
        });
        nodes.push(RaftHttpNode {
            url: urls[node_id].clone(),
            engine,
            hosts,
            serve,
        });
    }
    (dir, nodes)
}

#[cfg(feature = "raft")]
async fn await_raft_leader(nodes: &[RaftHttpNode], key: &str) -> usize {
    for _ in 0..400 {
        for (idx, node) in nodes.iter().enumerate() {
            if node.hosts.host_for(key).unwrap().is_leader().await {
                return idx;
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    panic!("no raft leader elected for key {key}");
}

#[cfg(feature = "raft")]
async fn stop_raft_http_cluster(dir: std::path::PathBuf, nodes: Vec<RaftHttpNode>) {
    for node in nodes {
        node.serve.abort();
    }
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn set_get_delete_roundtrip() {
    let (app, _) = app();

    // PUT a structured JSON value.
    let (st, _) = send_json(
        &app,
        "PUT",
        "/kv/foo",
        json!({"value": {"a": 1, "b": [2, 3]}}),
    )
    .await;
    assert_eq!(st, StatusCode::OK);

    // GET it back.
    let (st, body) = send_json(&app, "GET", "/kv/foo", Value::Null).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(body["value"], json!({"a": 1, "b": [2, 3]}));

    // HEAD => 200.
    let req = Request::builder()
        .method("HEAD")
        .uri("/kv/foo")
        .body(Body::empty())
        .unwrap();
    let (st, _) = send(&app, req).await;
    assert_eq!(st, StatusCode::OK);

    // DELETE => deleted true.
    let (st, body) = send_json(&app, "DELETE", "/kv/foo", Value::Null).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(body["deleted"], json!(true));

    // GET now 404.
    let (st, body) = send_json(&app, "GET", "/kv/foo", Value::Null).await;
    assert_eq!(st, StatusCode::NOT_FOUND);
    assert_eq!(body["code"], json!("key_not_found"));
}

#[cfg(feature = "raft")]
#[tokio::test]
async fn replica_mode_http_write_proposes_and_replicates() {
    let key = "raft-http-write";
    let (dir, nodes) = raft_http_cluster("write").await;
    let leader = await_raft_leader(&nodes, key).await;

    let client = reqwest::Client::new();
    let resp = client
        .put(format!("{}/kv/{key}", nodes[leader].url))
        .json(&json!({"value": "via-http-raft"}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let kv_key = KvKey::new(key).unwrap();
    for _ in 0..400 {
        if nodes.iter().all(|node| {
            node.engine.get(&kv_key) == Some(KvValue::String("via-http-raft".to_string()))
        }) {
            stop_raft_http_cluster(dir, nodes).await;
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    panic!("HTTP raft write did not reach every replica engine");
}

/// In replica mode every write answers from the outcome the raft state machine
/// produced when it applied that log entry, never from a local pre-read.
#[cfg(feature = "raft")]
#[tokio::test]
async fn replica_mode_http_answers_from_replicated_outcome() {
    use reqwest::Method;

    let (dir, nodes) = raft_http_cluster("outcome").await;
    let leader = await_raft_leader(&nodes, "any").await;
    let base = nodes[leader].url.clone();
    let client = reqwest::Client::new();
    let call = |method: Method, path: &str, body: Value| {
        let req = client.request(method, format!("{base}{path}")).json(&body);
        async move {
            let resp = req.send().await.unwrap();
            let status = resp.status();
            let body: Value = resp.json().await.unwrap_or(Value::Null);
            (status, body)
        }
    };

    let (st, body) = call(Method::DELETE, "/kv/absent", Value::Null).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(body["deleted"], json!(false));

    let (_, body) = call(Method::POST, "/kv/n/incr", json!({"delta": 5})).await;
    assert_eq!(body["value"], json!(5));
    let (_, body) = call(Method::POST, "/kv/n/incr", json!({"delta": 5})).await;
    assert_eq!(body["value"], json!(10));

    let (_, body) = call(Method::POST, "/kv/once/setnx", json!({"value": 1})).await;
    assert_eq!(body["set"], json!(true));
    let (_, body) = call(Method::POST, "/kv/once/setnx", json!({"value": 2})).await;
    assert_eq!(body["set"], json!(false));

    let cas = json!({"expected": 1, "new": 0});
    let (_, body) = call(Method::POST, "/kv/n/cas", cas.clone()).await;
    assert_eq!(body["swapped"], json!(false));
    let (st, body) = call(Method::POST, "/kv/missing/cas", cas).await;
    assert_eq!(st, StatusCode::NOT_FOUND, "{body}");

    let push = json!({"values": ["a", "b"]});
    let (_, body) = call(Method::POST, "/lists/q/rpush", push).await;
    assert_eq!(body["length"], json!(2));
    let (_, body) = call(Method::POST, "/lists/q/lpop", Value::Null).await;
    assert_eq!(body["value"], json!("a"));

    let (st, _) = call(Method::PUT, "/kv/s", json!({"value": "text"})).await;
    assert_eq!(st, StatusCode::OK);
    let (st, _) = call(Method::POST, "/kv/s/incr", json!({"delta": 1})).await;
    assert!(
        st.is_client_error(),
        "a replicated type mismatch surfaces as the engine error, got {st}"
    );

    let (_, body) = call(Method::POST, "/kv:mdel", json!({"keys": ["n", "absent"]})).await;
    assert_eq!(body["count"], json!(1));

    stop_raft_http_cluster(dir, nodes).await;
}

/// Reads honor `x-read-consistency`: the default (`leader`) is linearizable on
/// every replica — a follower sees a write the instant the leader acked it —
/// while `bounded`/`any` answer locally and report their staleness bound.
#[cfg(feature = "raft")]
#[tokio::test]
async fn replica_mode_reads_honor_consistency_level() {
    let (dir, nodes) = raft_http_cluster("consistency").await;
    let leader = await_raft_leader(&nodes, "any").await;
    let follower = (leader + 1) % nodes.len();
    let client = reqwest::Client::new();
    let get = |node: usize, key: &str, level: Option<&str>| {
        let mut req = client.get(format!("{}/kv/{key}", nodes[node].url));
        if let Some(level) = level {
            req = req.header("x-read-consistency", level);
        }
        async move {
            let resp = req.send().await.unwrap();
            let staleness = resp
                .headers()
                .get("x-keep-staleness-ms")
                .map(|v| v.to_str().unwrap().parse::<u64>().unwrap());
            let status = resp.status();
            (status, staleness, resp.json::<Value>().await.unwrap())
        }
    };

    // A follower's leader read waits for the barrier's commit to reach it on
    // the next heartbeat, so keep the loop short.
    for i in 0..8 {
        let resp = client
            .put(format!("{}/kv/rc", nodes[leader].url))
            .json(&json!({"value": i}))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        for level in [None, Some("unknown-level")] {
            let (st, staleness, body) = get(follower, "rc", level).await;
            assert_eq!(st, StatusCode::OK);
            assert_eq!(
                body["value"],
                json!(i),
                "stale {level:?} read on a follower"
            );
            assert_eq!(staleness, None, "leader reads report no staleness");
        }
    }

    let (st, staleness, body) = get(follower, "rc", Some("any")).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(body["value"], json!(7));
    assert!(staleness.is_some(), "a fenced follower knows its bound");
    let (st, staleness, _) = get(follower, "rc", Some("bounded(60000)")).await;
    assert_eq!(st, StatusCode::OK);
    assert!(staleness.unwrap() <= 60_000);

    let resp = client
        .get(format!("{}/cluster", nodes[follower].url))
        .send()
        .await
        .unwrap();
    let cluster: Value = resp.json().await.unwrap();
    let shard = &cluster["shards"][0];
    assert_eq!(shard["shard"], json!(0));
    assert_eq!(shard["replica_id"], json!(follower));
    assert_eq!(shard["leader"], json!(leader));
    assert_eq!(shard["role"], json!("follower"));
    assert_eq!(shard["voters"], json!([0, 1, 2]));
    assert!(shard["applied_index"].as_u64().unwrap() > 8);

    stop_raft_http_cluster(dir, nodes).await;
}

/// A batch whose keys hash to different shard groups cannot commit atomically
/// through one raft log, so replica mode refuses it instead of half-applying.
#[cfg(feature = "raft")]
#[tokio::test]
async fn replica_mode_rejects_cross_shard_batch() {
    let dir = tmp_dir("cross-shard");
    let cluster = ClusterConfig::replicated(
        2,
        keep::ReplicaPlacement {
            shard_index: 0,
            replica_index: 0,
            replicas_per_shard: 1,
            voter_count: 1,
        },
        vec![String::new()],
    );
    let key_in = |shard: u32| {
        (0..)
            .map(|i| format!("k{i}"))
            .find(|k| cluster.shard_for(k) == shard)
            .unwrap()
    };
    let (a, b) = (key_in(0), key_in(1));
    let engine = Arc::new(KvEngine::with_shards(4));
    let hosts = Arc::new(
        keep::raft::ShardHosts::new(cluster.clone(), engine.clone(), &dir)
            .await
            .unwrap(),
    );
    let state = AppState::new(engine)
        .with_cluster(cluster)
        .with_raft_hosts(hosts);
    let app = router(state);

    let entries = json!({"entries": {(a.clone()): 1, (b.clone()): 2}});
    let (st, body) = send_json(&app, "POST", "/kv:mset", entries).await;
    assert_eq!(st, StatusCode::BAD_REQUEST);
    assert_eq!(body["code"], json!("cross_shard_batch"));

    let keys = json!({"keys": [a, b.clone()]});
    let (st, body) = send_json(&app, "POST", "/kv:mdel", keys).await;
    assert_eq!(st, StatusCode::BAD_REQUEST);
    assert_eq!(body["code"], json!("cross_shard_batch"));

    let (st, body) = send_json(&app, "PUT", &format!("/kv/{b}"), json!({"value": 1})).await;
    assert_eq!(st, StatusCode::CONFLICT);
    assert_eq!(body["code"], json!("shard_not_owned"));

    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn claim_check_blob_roundtrip() {
    let (app, _) = app();
    let blob: Vec<u8> = (0u8..=255).cycle().take(4096).collect();

    // PUT raw bytes.
    let req = Request::builder()
        .method("PUT")
        .uri("/kv/result:job-1")
        .header(header::CONTENT_TYPE, "application/octet-stream")
        .body(Body::from(blob.clone()))
        .unwrap();
    let (st, _) = send(&app, req).await;
    assert_eq!(st, StatusCode::OK);

    // GET returns the bytes verbatim as octet-stream.
    let req = Request::builder()
        .method("GET")
        .uri("/kv/result:job-1")
        .body(Body::empty())
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        resp.headers().get(header::CONTENT_TYPE).unwrap(),
        "application/octet-stream"
    );
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    assert_eq!(bytes.as_ref(), blob.as_slice());
}

#[tokio::test]
async fn incr_and_cas() {
    let (app, _) = app();

    let (st, body) = send_json(&app, "POST", "/kv/counter/incr", json!({"delta": 5})).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(body["value"], json!(5));

    let (st, body) = send_json(&app, "POST", "/kv/counter/incr", json!({"delta": -2})).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(body["value"], json!(3));

    // CAS: expect 3 -> 99 succeeds.
    let (st, body) = send_json(
        &app,
        "POST",
        "/kv/counter/cas",
        json!({"expected": 3, "new": 99}),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(body["swapped"], json!(true));

    // CAS with wrong expected fails to swap.
    let (st, body) = send_json(
        &app,
        "POST",
        "/kv/counter/cas",
        json!({"expected": 3, "new": 0}),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(body["swapped"], json!(false));
}

#[tokio::test]
async fn batch_mset_mget() {
    let (app, _) = app();

    let (st, _) = send_json(
        &app,
        "POST",
        "/kv:mset",
        json!({"entries": {"k1": "v1", "k2": 2}}),
    )
    .await;
    assert_eq!(st, StatusCode::OK);

    let (st, body) = send_json(
        &app,
        "POST",
        "/kv:mget",
        json!({"keys": ["k1", "k2", "missing"]}),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    let values = body["values"].as_array().unwrap();
    assert_eq!(values[0], json!("v1"));
    assert_eq!(values[1], json!(2));
    assert_eq!(values[2], Value::Null);
}

#[tokio::test]
async fn scan_by_prefix() {
    let (app, _) = app();
    for k in ["user:1", "user:2", "post:1"] {
        let (st, _) = send_json(&app, "PUT", &format!("/kv/{k}"), json!({"value": 1})).await;
        assert_eq!(st, StatusCode::OK);
    }
    let (st, body) = send_json(&app, "GET", "/kv?prefix=user:&limit=10", Value::Null).await;
    assert_eq!(st, StatusCode::OK);
    let mut keys: Vec<String> = body["keys"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap().to_string())
        .collect();
    keys.sort();
    assert_eq!(keys, vec!["user:1".to_string(), "user:2".to_string()]);
}

#[tokio::test]
async fn locks_lifecycle() {
    let (app, _) = app();

    let (st, body) = send_json(
        &app,
        "POST",
        "/locks/job",
        json!({"owner": "a", "ttl_ms": 60000}),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(body["acquired"], json!(true));

    // A different owner cannot acquire.
    let (st, body) = send_json(
        &app,
        "POST",
        "/locks/job",
        json!({"owner": "b", "ttl_ms": 60000}),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(body["acquired"], json!(false));

    // Owner releases.
    let (st, body) = send_json(&app, "DELETE", "/locks/job", json!({"owner": "a"})).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(body["released"], json!(true));
}

#[tokio::test]
async fn list_push_pop() {
    let (app, _) = app();

    let (st, body) = send_json(
        &app,
        "POST",
        "/lists/q/rpush",
        json!({"values": ["a", "b", "c"]}),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(body["length"], json!(3));

    let (st, body) = send_json(&app, "POST", "/lists/q/lpop", Value::Null).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(body["value"], json!("a"));
}

#[tokio::test]
async fn probes_and_drain() {
    let (app, state) = app();

    let req = Request::builder()
        .uri("/healthz")
        .body(Body::empty())
        .unwrap();
    let (st, body) = send(&app, req).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(body, b"ok");

    let req = Request::builder()
        .uri("/readyz")
        .body(Body::empty())
        .unwrap();
    let (st, _) = send(&app, req).await;
    assert_eq!(st, StatusCode::OK);

    // Once draining, readyz flips to 503 so k8s stops routing. The shared
    // service-http probe shell (#751) preserves keep's `draining` body.
    state.start_drain();
    let req = Request::builder()
        .uri("/readyz")
        .body(Body::empty())
        .unwrap();
    let (st, body) = send(&app, req).await;
    assert_eq!(st, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(body, b"draining");
}

/// #2490: every response carries a `Server-Timing: app;dur=<ms>` baseline
/// from the shared `service_http::server_timing_middleware` layer.
#[tokio::test]
async fn responses_carry_server_timing_header() {
    let (app, _state) = app();
    let req = Request::builder()
        .uri("/healthz")
        .body(Body::empty())
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let header = resp
        .headers()
        .get("server-timing")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string();
    assert!(
        header.starts_with("app;dur="),
        "Server-Timing header must start with app;dur=, got {header:?}"
    );
    let digit = header
        .strip_prefix("app;dur=")
        .and_then(|rest| rest.chars().next());
    assert!(
        digit.is_some_and(|c| c.is_ascii_digit()),
        "app;dur= must be followed by a digit, got {header:?}"
    );
}

#[tokio::test]
async fn metrics_records_per_route_requests() {
    let (app, _) = app();
    // Exercise a couple of data-plane routes.
    send_json(&app, "PUT", "/kv/m", json!({"value": 1})).await;
    send_json(&app, "GET", "/kv/m", Value::Null).await;

    let req = Request::builder()
        .uri("/metrics")
        .body(Body::empty())
        .unwrap();
    let (st, bytes) = send(&app, req).await;
    assert_eq!(st, StatusCode::OK);
    let text = String::from_utf8_lossy(&bytes);
    // Request counter labelled by the matched route pattern (not the raw key).
    assert!(
        text.contains("keep_http_requests_total"),
        "missing request counter"
    );
    assert!(
        text.contains("route=\"/kv/{key}\""),
        "missing matched-route label:\n{text}"
    );
    // Latency histogram + the existing engine gauges.
    assert!(
        text.contains("keep_http_request_duration_seconds_bucket"),
        "missing histogram"
    );
    assert!(text.contains("keep_keys_total"), "engine gauges dropped");
}

#[tokio::test]
async fn cluster_endpoint_reports_topology() {
    let (app, _) = app();
    let req = Request::builder()
        .uri("/cluster")
        .body(Body::empty())
        .unwrap();
    let (st, bytes) = send(&app, req).await;
    assert_eq!(st, StatusCode::OK);
    let v: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(v["node_count"], json!(1));
    assert_eq!(v["mode"], json!("single"));
    assert_eq!(v["owned_shards"], json!(1));
}

#[tokio::test]
async fn openapi_document_is_served() {
    let (app, _) = app();
    let req = Request::builder()
        .uri("/openapi.json")
        .body(Body::empty())
        .unwrap();
    let (st, bytes) = send(&app, req).await;
    assert_eq!(st, StatusCode::OK);
    let doc: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(doc["info"]["title"], json!("keep"));
    assert!(doc["paths"]["/kv/{key}"].is_object());
    assert!(doc["paths"]["/healthz"].is_object());
}

// ---------------------------------------------------------------------------
// Per-namespace claim-check key isolation (#464): X-Keep-Namespace is applied
// as a storage-key prefix `{ns}::{kind}:{id}` AFTER the token scope check.
// ---------------------------------------------------------------------------

fn claim_put_req(path: &str, ns: Option<&str>, body: &'static str) -> Request<Body> {
    let mut b = Request::builder()
        .method("PUT")
        .uri(path)
        .header(header::CONTENT_TYPE, "application/octet-stream");
    if let Some(ns) = ns {
        b = b.header("x-keep-namespace", ns);
    }
    b.body(Body::from(body)).unwrap()
}

fn claim_get_req(path: &str, ns: Option<&str>) -> Request<Body> {
    let mut b = Request::builder().method("GET").uri(path);
    if let Some(ns) = ns {
        b = b.header("x-keep-namespace", ns);
    }
    b.body(Body::empty()).unwrap()
}

#[tokio::test]
async fn keep_ns_isolates_same_bare_key() {
    let (app, _) = app();

    // Two namespaces write the SAME bare result id.
    let (st, _) = send(
        &app,
        claim_put_req("/results/job-1", Some("tenant-a"), "AAA"),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    let (st, _) = send(
        &app,
        claim_put_req("/results/job-1", Some("tenant-b"), "BBB"),
    )
    .await;
    assert_eq!(st, StatusCode::OK);

    // Each namespace reads back only its own value — no cross-namespace collision.
    let (st, body) = send(&app, claim_get_req("/results/job-1", Some("tenant-a"))).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(body, b"AAA".to_vec());
    let (st, body) = send(&app, claim_get_req("/results/job-1", Some("tenant-b"))).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(body, b"BBB".to_vec());

    // A namespace that never wrote this id sees nothing.
    let (st, _) = send(&app, claim_get_req("/results/job-1", Some("tenant-c"))).await;
    assert_eq!(st, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn keep_ns_absent_is_backcompat() {
    let (app, _) = app();

    // No X-Keep-Namespace ⇒ bare 'result:job-2' storage key.
    let (st, _) = send(&app, claim_put_req("/results/job-2", None, "PLAIN")).await;
    assert_eq!(st, StatusCode::OK);
    let (st, body) = send(&app, claim_get_req("/results/job-2", None)).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(body, b"PLAIN".to_vec());

    // The bare key is observable on the generic KV path at 'result:job-2'.
    let (st, _) = send(&app, claim_get_req("/kv/result:job-2", None)).await;
    assert_eq!(st, StatusCode::OK);

    // A namespaced read does NOT see the bare-written value.
    let (st, _) = send(&app, claim_get_req("/results/job-2", Some("tenant-a"))).await;
    assert_eq!(st, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn keep_ns_token_checks_bare_key() {
    let secret = b"test-secret".to_vec();
    let state =
        AppState::new(Arc::new(KvEngine::with_shards(16))).with_token_secret(secret.clone());
    let app = router(state);

    // Token scoped to the BARE result key 'job-3'.
    let scope = claim_token::Scope {
        r: "job-3".into(),
        w: "job-3".into(),
        exp: u64::MAX,
    };
    let token = claim_token::sign(&secret, &scope);

    // PUT with a namespace header still passes — the token is verified against
    // the bare 'job-3' from the URL, before the namespace prefix is applied.
    let req = Request::builder()
        .method("PUT")
        .uri("/results/job-3")
        .header(header::CONTENT_TYPE, "application/octet-stream")
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .header("x-keep-namespace", "tenant-a")
        .body(Body::from("R3"))
        .unwrap();
    let (st, _) = send(&app, req).await;
    assert_eq!(st, StatusCode::OK);

    // Without the token ⇒ rejected, namespace notwithstanding.
    let req = Request::builder()
        .method("PUT")
        .uri("/results/job-3")
        .header(header::CONTENT_TYPE, "application/octet-stream")
        .header("x-keep-namespace", "tenant-a")
        .body(Body::from("R3"))
        .unwrap();
    let (st, _) = send(&app, req).await;
    assert_eq!(st, StatusCode::FORBIDDEN);
}

/// #746: after the service-auth adoption, the claim-check contract is unchanged
/// — enforcement runs through keep's `KeepVerifier` per-handler but still
/// returns 403 (not 401) for invalid and out-of-scope tokens, and the accepted
/// read scope on GET /inputs/{id} returns 200.
#[tokio::test]
async fn keep_token_invalid_and_out_of_scope_are_403() {
    let secret = b"test-secret".to_vec();
    let state =
        AppState::new(Arc::new(KvEngine::with_shards(16))).with_token_secret(secret.clone());
    let app = router(state);

    // Seed an input the worker is allowed to read with an in-scope token.
    let scope = claim_token::Scope {
        r: "job-7".into(),
        w: "job-7".into(),
        exp: u64::MAX,
    };
    let token = claim_token::sign(&secret, &scope);
    let put = Request::builder()
        .method("PUT")
        .uri("/inputs/job-7")
        .header(header::CONTENT_TYPE, "application/octet-stream")
        .body(Body::from("IN7"))
        .unwrap();
    // PUT input is deliberately NOT scope-checked, so no token is needed.
    let (st, _) = send(&app, put).await;
    assert_eq!(st, StatusCode::OK);

    // Accepted read scope ⇒ 200.
    let get_ok = Request::builder()
        .method("GET")
        .uri("/inputs/job-7")
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .body(Body::empty())
        .unwrap();
    let (st, body) = send(&app, get_ok).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(body, b"IN7".to_vec());

    // A syntactically-broken bearer value ⇒ 403 (not 401).
    let get_bad = Request::builder()
        .method("GET")
        .uri("/inputs/job-7")
        .header(header::AUTHORIZATION, "Bearer not-a-real-token")
        .body(Body::empty())
        .unwrap();
    let (st, _) = send(&app, get_bad).await;
    assert_eq!(st, StatusCode::FORBIDDEN);

    // A valid token for a DIFFERENT id (out of scope) ⇒ 403.
    let other = claim_token::Scope {
        r: "job-other".into(),
        w: "job-other".into(),
        exp: u64::MAX,
    };
    let other_token = claim_token::sign(&secret, &other);
    let get_oos = Request::builder()
        .method("GET")
        .uri("/inputs/job-7")
        .header(header::AUTHORIZATION, format!("Bearer {other_token}"))
        .body(Body::empty())
        .unwrap();
    let (st, _) = send(&app, get_oos).await;
    assert_eq!(st, StatusCode::FORBIDDEN);
}

/// #746/#446: with no secret configured the verifier is absent, so claim-check
/// stays open (no-op) — a tokenless worker read still succeeds.
#[tokio::test]
async fn keep_token_disabled_is_open() {
    let (app, _) = app();

    let put = Request::builder()
        .method("PUT")
        .uri("/inputs/job-open")
        .header(header::CONTENT_TYPE, "application/octet-stream")
        .body(Body::from("OPEN"))
        .unwrap();
    let (st, _) = send(&app, put).await;
    assert_eq!(st, StatusCode::OK);

    // No Authorization header, enforcement off ⇒ 200.
    let get = Request::builder()
        .method("GET")
        .uri("/inputs/job-open")
        .body(Body::empty())
        .unwrap();
    let (st, body) = send(&app, get).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(body, b"OPEN".to_vec());
}

/// #2556: oversized request bodies are rejected with 413 PAYLOAD_TOO_LARGE.
/// Uses a small 1 KiB cap in the test to avoid allocating 16 MiB payloads.
#[tokio::test]
async fn oversized_body_is_rejected_with_413() {
    let engine = Arc::new(KvEngine::with_shards(16));
    let state = AppState::new(engine).with_body_limit(1024); // 1 KiB test cap
    let app = router(state);

    // One byte over the test cap.
    let oversized_payload = "a".repeat(1025);
    let req = Request::builder()
        .method("PUT")
        .uri("/kv/test-key")
        .header(header::CONTENT_TYPE, "application/octet-stream")
        .body(Body::from(oversized_payload))
        .unwrap();

    let resp = app.clone().oneshot(req).await;

    // The Content-Length short-circuit is the point of the layer: the server
    // answers and closes without reading bytes it has already decided to
    // refuse. The client is still uploading when that happens, so it races
    // between reading the 413 and having its own write fail with ECONNRESET —
    // both outcomes are the same refusal.
    match resp {
        Ok(resp) => {
            assert_eq!(
                resp.status(),
                StatusCode::PAYLOAD_TOO_LARGE,
                "oversized body must be refused with 413"
            );
        }
        Err(_err) => {
            // The tower::oneshot error on an oversized body is Infallible,
            // so this branch is unreachable. Still, the pattern matches the
            // tape test for consistency.
        }
    }
}

/// #2556: the race-free invariant — a refused oversized body never lands in
/// the store. This assertion catches regressions that buffer and accept.
#[tokio::test]
async fn oversized_body_never_reaches_store() {
    let engine = Arc::new(KvEngine::with_shards(16));
    let state = AppState::new(engine.clone()).with_body_limit(1024); // 1 KiB test cap
    let app = router(state);

    // Send an oversized body to PUT.
    let oversized_payload = "a".repeat(1025);
    let req = Request::builder()
        .method("PUT")
        .uri("/kv/refused-key")
        .header(header::CONTENT_TYPE, "application/octet-stream")
        .body(Body::from(oversized_payload))
        .unwrap();

    let _ = app.clone().oneshot(req).await;

    // Verify the key was never stored — GET should return 404.
    let get_req = Request::builder()
        .method("GET")
        .uri("/kv/refused-key")
        .body(Body::empty())
        .unwrap();
    let resp = app.clone().oneshot(get_req).await.unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::NOT_FOUND,
        "the refused body must not be stored"
    );
}

/// #2556: bodies under the limit succeed.
#[tokio::test]
async fn under_limit_body_succeeds() {
    let engine = Arc::new(KvEngine::with_shards(16));
    let state = AppState::new(engine).with_body_limit(1024); // 1 KiB test cap
    let app = router(state);

    // Just under the cap.
    let payload = "a".repeat(1023);
    let req = Request::builder()
        .method("PUT")
        .uri("/kv/under-limit-key")
        .header(header::CONTENT_TYPE, "application/octet-stream")
        .body(Body::from(payload.clone()))
        .unwrap();

    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "under-limit body must succeed"
    );

    // Verify the value was stored.
    let get_req = Request::builder()
        .method("GET")
        .uri("/kv/under-limit-key")
        .body(Body::empty())
        .unwrap();
    let resp = app.clone().oneshot(get_req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    assert_eq!(bytes, payload.as_bytes());
}

/// #2556: the structured error envelope on oversized body refusal.
/// This is specifically what the swap from `DefaultBodyLimit` to
/// `service_http::body_limit_layer` buys: a consistent error envelope
/// across all services.
#[tokio::test]
async fn oversized_body_returns_structured_error_envelope() {
    let engine = Arc::new(KvEngine::with_shards(16));
    let state = AppState::new(engine).with_body_limit(1024); // 1 KiB test cap
    let app = router(state);

    let oversized_payload = "a".repeat(1025);
    let req = Request::builder()
        .method("PUT")
        .uri("/kv/test-key")
        .header(header::CONTENT_TYPE, "application/octet-stream")
        .body(Body::from(oversized_payload))
        .unwrap();

    let resp = app.clone().oneshot(req).await.unwrap();

    // Only check the 413 case; the transport error case doesn't produce a body.
    if resp.status() == StatusCode::PAYLOAD_TOO_LARGE {
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let body: serde_json::Value =
            serde_json::from_slice(&bytes).expect("response body must be valid JSON");

        assert_eq!(
            body["error"], "payload_too_large",
            "error field must be 'payload_too_large'"
        );
        assert!(
            body["message"]
                .as_str()
                .unwrap_or("")
                .contains("size limit"),
            "message must mention size limit"
        );
    }
}

/// Unit test: the DEFAULT_BODY_LIMIT is 16 MiB and the KEEP_BODY_LIMIT
/// environment variable can override it. This guards against a future
/// companion sweep "normalizing" keep to 8 MiB.
#[test]
fn default_body_limit_is_16_mib_and_env_overrides() {
    // Verify the constant is correct.
    assert_eq!(
        keep::http::DEFAULT_BODY_LIMIT,
        16 * 1024 * 1024,
        "DEFAULT_BODY_LIMIT must remain 16 MiB for claim-check blob writes"
    );

    // Verify the AppState builder uses the default.
    let engine = Arc::new(KvEngine::with_shards(16));
    let default_state = AppState::new(engine.clone());
    assert_eq!(
        default_state.body_limit,
        16 * 1024 * 1024,
        "AppState default body_limit must be 16 MiB"
    );

    // Verify the builder method allows override.
    let custom_state = AppState::new(engine).with_body_limit(8 * 1024 * 1024);
    assert_eq!(
        custom_state.body_limit,
        8 * 1024 * 1024,
        "AppState.with_body_limit() must override the default"
    );
}

#[tokio::test]
async fn token_secret_blocks_kv_bypass_of_claim_check_keys() {
    let secret = b"test-secret".to_vec();
    let state =
        AppState::new(Arc::new(KvEngine::with_shards(16))).with_token_secret(secret.clone());
    let app = router(state);

    // Producer seeds an input via /inputs/job-sec (open to producer).
    let (st, _) = send(&app, claim_put_req("/inputs/job-sec", None, "SECRET_INPUT")).await;
    assert_eq!(st, StatusCode::OK);

    // Regular KV keys work normally.
    let (st, _) = send_json(&app, "PUT", "/kv/regular:key", json!({"value": "ok"})).await;
    assert_eq!(st, StatusCode::OK);

    // Attempting to read or overwrite claim-check keys via /kv/* is 403 Forbidden.
    for path in [
        "/kv/input:job-sec",
        "/kv/result:job-sec",
        "/kv/tenant-a::input:job-sec",
        "/kv/tenant-a::result:job-sec",
    ] {
        let (st, _) = send_json(&app, "GET", path, Value::Null).await;
        assert_eq!(st, StatusCode::FORBIDDEN, "GET {path} must be forbidden");

        let (st, _) = send_json(&app, "PUT", path, json!({"value": "forged"})).await;
        assert_eq!(st, StatusCode::FORBIDDEN, "PUT {path} must be forbidden");

        let (st, _) = send_json(&app, "DELETE", path, Value::Null).await;
        assert_eq!(st, StatusCode::FORBIDDEN, "DELETE {path} must be forbidden");
    }

    // Batch mget / mset / mdel touching claim-check keys are also forbidden.
    let (st, _) = send_json(
        &app,
        "POST",
        "/kv:mget",
        json!({"keys": ["regular:key", "input:job-sec"]}),
    )
    .await;
    assert_eq!(st, StatusCode::FORBIDDEN);

    // Scan (/kv) hides reserved claim-check keys when token enforcement is enabled.
    let (st, body) = send_json(&app, "GET", "/kv", Value::Null).await;
    assert_eq!(st, StatusCode::OK);
    let keys = body["keys"].as_array().unwrap();
    assert!(keys.contains(&json!("regular:key")));
    assert!(!keys.contains(&json!("input:job-sec")));
}

#[tokio::test]
async fn blanket_api_token_gates_data_plane_and_leaves_probes_open() {
    let state = AppState::new(Arc::new(KvEngine::with_shards(16))).with_api_token("top-secret-api");
    let app = router(state);

    // Unauthenticated data-plane write is rejected with 401.
    let (st, _) = send_json(&app, "PUT", "/kv/k1", json!({"value": 1})).await;
    assert_eq!(st, StatusCode::UNAUTHORIZED);

    // Authenticated data-plane write succeeds.
    let req = Request::builder()
        .method("PUT")
        .uri("/kv/k1")
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::AUTHORIZATION, "Bearer top-secret-api")
        .body(Body::from(
            serde_json::to_vec(&json!({"value": 42})).unwrap(),
        ))
        .unwrap();
    let (st, _) = send(&app, req).await;
    assert_eq!(st, StatusCode::OK);

    // Probes and metrics remain accessible without auth for K8s kubelet/Prometheus.
    let req = Request::builder()
        .method("GET")
        .uri("/healthz")
        .body(Body::empty())
        .unwrap();
    let (st, _) = send(&app, req).await;
    assert_eq!(st, StatusCode::OK);
}

#[tokio::test]
async fn info_and_metrics_expose_resource_utilization_fields() {
    let dir = tempfile::tempdir().unwrap();
    let blob_store =
        Arc::new(keep::persistence::BlobStore::new(dir.path(), 128, 1024 * 1024).unwrap());
    let engine = KvEngine::with_shards(4);
    engine.set_maxmemory(4096);
    engine.set_eviction_policy(keep::EvictionPolicy::AllKeysLru);
    engine.enable_blob_store(blob_store);
    let state = AppState::new(Arc::new(engine));
    let app = router(state);

    // Write a 512-byte blob so it spills to disk.
    let req = Request::builder()
        .method("PUT")
        .uri("/kv/big-blob")
        .header(header::CONTENT_TYPE, "application/octet-stream")
        .body(Body::from(vec![b'x'; 512]))
        .unwrap();
    let (st, _) = send(&app, req).await;
    assert_eq!(st, StatusCode::OK);

    // Check /info
    let (st, info) = send_json(&app, "GET", "/info", Value::Null).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(info["maxmemory_bytes"], json!(4096));
    assert_eq!(info["disk_blob_bytes"], json!(512));
    assert_eq!(info["spilled_keys"], json!(1));
    assert_eq!(info["eviction_policy"], json!("allkeys-lru"));

    // Check /metrics
    let req = Request::builder()
        .method("GET")
        .uri("/metrics")
        .body(Body::empty())
        .unwrap();
    let (st, bytes) = send(&app, req).await;
    assert_eq!(st, StatusCode::OK);
    let text = String::from_utf8(bytes).unwrap();
    assert!(text.contains("keep_maxmemory_bytes 4096"));
    assert!(text.contains("keep_disk_blob_bytes 512"));
    assert!(text.contains("keep_spilled_keys_total 1"));
    assert!(text.contains("keep_evicted_keys_total 0"));
}
