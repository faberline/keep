//! Raft peer routes on their own mutual TLS port (`KEEP_PEER_PORT`).
//!
//! Proves that a peer without a certificate of the cluster CA is refused, that a
//! peer with a trusted certificate is served, that the public port has no peer
//! routes once the peer port is set, and that the peer port refuses to start
//! without mutual TLS.

#![cfg(feature = "raft")]

use std::io::Read;
use std::net::TcpListener as StdListener;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::Router;
use keep::raft::ShardHosts;
use keep::{ClusterConfig, KvEngine};
use raft_runtime::PeerTransport;
use rcgen::{
    date_time_ymd, BasicConstraints, Certificate, CertificateParams, DnType,
    ExtendedKeyUsagePurpose, IsCa, KeyPair,
};
use tempfile::TempDir;

struct Authority {
    cert: Certificate,
    key: KeyPair,
}

impl Authority {
    fn generate(name: &str) -> Self {
        let mut params = CertificateParams::new(Vec::new()).unwrap();
        params.distinguished_name.push(DnType::CommonName, name);
        params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        let key = KeyPair::generate().unwrap();
        let cert = params.self_signed(&key).unwrap();
        Self { cert, key }
    }
}

struct Material {
    _dir: TempDir,
    config: peer_tls::PeerTlsConfig,
}

fn material(signer: &Authority, trust: &Authority, identity: &str, expired: bool) -> Material {
    let mut params = CertificateParams::new(vec![identity.to_owned()]).unwrap();
    params.distinguished_name.push(DnType::CommonName, identity);
    params.extended_key_usages = vec![
        ExtendedKeyUsagePurpose::ServerAuth,
        ExtendedKeyUsagePurpose::ClientAuth,
    ];
    if expired {
        params.not_before = date_time_ymd(2019, 1, 1);
        params.not_after = date_time_ymd(2020, 1, 1);
    }
    let key = KeyPair::generate().unwrap();
    let cert = params.signed_by(&key, &signer.cert, &signer.key).unwrap();
    let dir = tempfile::tempdir().unwrap();
    let cert_path = dir.path().join("cert.pem");
    let key_path = dir.path().join("key.pem");
    let ca_path = dir.path().join("ca.pem");
    std::fs::write(&cert_path, cert.pem()).unwrap();
    std::fs::write(&key_path, key.serialize_pem()).unwrap();
    std::fs::write(&ca_path, trust.cert.pem()).unwrap();
    Material {
        _dir: dir,
        config: peer_tls::PeerTlsConfig::new(cert_path, key_path, ca_path, true),
    }
}

/// Start a single-node shard host set whose peer routes are served only through
/// `server` on an ephemeral port. Returns the port, the serve task and the hosts
/// (the hosts must stay alive while the task runs).
async fn start_peer_server(
    server: &PeerTransport,
    data: &Path,
) -> (
    u16,
    tokio::task::JoinHandle<anyhow::Result<()>>,
    Arc<ShardHosts>,
) {
    let hosts = Arc::new(
        ShardHosts::with_peer_transport(
            ClusterConfig::new(0, 1, 1, Vec::new()),
            Arc::new(KvEngine::with_shards(4)),
            data,
            1,
            1,
            server.clone(),
        )
        .await
        .unwrap(),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let router: Router = hosts.router();
    let transport = server.clone();
    let task = tokio::spawn(async move {
        transport
            .serve(listener, router, std::future::pending::<()>())
            .await
    });
    (port, task, hosts)
}

/// `GET https://localhost:<port>/shard/0/raftz` with the given client.
async fn get_peer(
    client: &reqwest::Client,
    port: u16,
) -> Result<reqwest::Response, reqwest::Error> {
    client
        .get(format!("https://localhost:{port}/shard/0/raftz"))
        .timeout(Duration::from_secs(3))
        .send()
        .await
}

/// A client with no certificate of its own. It accepts any server certificate,
/// so a refusal below comes from the server rejecting the missing client identity.
fn plain_client() -> reqwest::Client {
    reqwest::Client::builder()
        .danger_accept_invalid_certs(true)
        .build()
        .unwrap()
}

fn free_port() -> u16 {
    StdListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// Kills the child process when the test ends, also on panic.
struct Keep(Child);

impl Drop for Keep {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn keep_command(
    port: u16,
    peer_port: u16,
    node: &peer_tls::PeerTlsConfig,
    data: &Path,
    mtls: bool,
) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_keep"));
    command
        .args([
            "--host",
            "127.0.0.1",
            "--port",
            &port.to_string(),
            "--data-dir",
        ])
        .arg(data)
        .env("POD_NAME", "keep-0")
        .env("SHARD_COUNT", "1")
        .env("REPLICAS_PER_SHARD", "3")
        .env("VOTER_COUNT", "3")
        .env("KEEP_REPLICAS_PER_SHARD", "3")
        .env("KEEP_HEADLESS_SERVICE", "keep-headless")
        .env("KEEP_SHARDS", "4")
        .env("KEEP_GRACE_SECS", "1")
        .env("KEEP_PEER_PORT", peer_port.to_string())
        .env("KEEP_PEER_TLS_CERT", node.cert())
        .env("KEEP_PEER_TLS_KEY", node.key())
        .env("KEEP_PEER_TLS_CA", node.ca())
        .stdout(Stdio::null());
    if mtls {
        command.env("KEEP_PEER_MTLS", "on");
    } else {
        command.env_remove("KEEP_PEER_MTLS");
    }
    command
}

async fn wait_until_ready(port: u16) {
    let client = reqwest::Client::new();
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let ready = client
            .get(format!("http://127.0.0.1:{port}/readyz"))
            .timeout(Duration::from_secs(3))
            .send()
            .await
            .map(|response| response.status() == reqwest::StatusCode::OK)
            .unwrap_or(false);
        if ready {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "keep did not become ready on port {port} within 30 s"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

#[tokio::test]
async fn peer_request_without_client_certificate_is_refused() {
    let ca = Authority::generate("peer test ca");
    let node = material(&ca, &ca, "localhost", false);
    let server = PeerTransport::from_config(&node.config).unwrap();
    let data = tempfile::tempdir().unwrap();
    let (port, task, _hosts) = start_peer_server(&server, data.path()).await;

    assert!(get_peer(&plain_client(), port).await.is_err());

    task.abort();
}

#[tokio::test]
async fn peer_request_with_untrusted_client_certificate_is_refused() {
    let ca_a = Authority::generate("peer test ca a");
    let ca_b = Authority::generate("peer test ca b");
    let node = material(&ca_a, &ca_a, "localhost", false);
    // Signed by CA B, but it trusts CA A so that it accepts the server certificate.
    let rogue = material(&ca_b, &ca_a, "client.local", false);
    let server = PeerTransport::from_config(&node.config).unwrap();
    let rogue_client = PeerTransport::from_config(&rogue.config).unwrap();
    let data = tempfile::tempdir().unwrap();
    let (port, task, _hosts) = start_peer_server(&server, data.path()).await;

    assert!(get_peer(&rogue_client.http_client(), port).await.is_err());

    task.abort();
}

#[tokio::test]
async fn peer_request_with_trusted_certificate_is_served() {
    let ca = Authority::generate("peer test ca");
    let node = material(&ca, &ca, "localhost", false);
    let trusted = material(&ca, &ca, "client.local", false);
    let server = PeerTransport::from_config(&node.config).unwrap();
    let trusted_client = PeerTransport::from_config(&trusted.config).unwrap();
    let data = tempfile::tempdir().unwrap();
    let (port, task, _hosts) = start_peer_server(&server, data.path()).await;

    let response = get_peer(&trusted_client.http_client(), port).await.unwrap();
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    let body = response.text().await.unwrap();
    assert!(body.contains("\"is_leader\""), "unexpected body: {body}");

    task.abort();
}

#[tokio::test]
async fn keep_binary_serves_peer_routes_only_on_the_peer_port() {
    let ca = Authority::generate("peer test ca");
    let node = material(&ca, &ca, "localhost", false);
    let trusted = material(&ca, &ca, "client.local", false);
    let trusted_client = PeerTransport::from_config(&trusted.config).unwrap();
    let data = tempfile::tempdir().unwrap();
    let port = free_port();
    let peer_port = free_port();
    let mut command = keep_command(port, peer_port, &node.config, data.path(), true);
    let _keep = Keep(command.stderr(Stdio::null()).spawn().unwrap());
    wait_until_ready(port).await;

    // The public port has no peer routes.
    let public = reqwest::Client::new()
        .get(format!("http://127.0.0.1:{port}/shard/0/raftz"))
        .timeout(Duration::from_secs(3))
        .send()
        .await
        .unwrap();
    assert_eq!(public.status(), reqwest::StatusCode::NOT_FOUND);

    // The peer port serves a trusted peer and refuses one without a certificate.
    let served = get_peer(&trusted_client.http_client(), peer_port)
        .await
        .unwrap();
    assert_eq!(served.status(), reqwest::StatusCode::OK);
    assert!(get_peer(&plain_client(), peer_port).await.is_err());
}

#[test]
fn keep_binary_refuses_a_peer_port_without_mutual_tls() {
    let ca = Authority::generate("peer test ca");
    let node = material(&ca, &ca, "localhost", false);
    let data = tempfile::tempdir().unwrap();
    let mut command = keep_command(free_port(), free_port(), &node.config, data.path(), false);
    let mut child = command.stderr(Stdio::piped()).spawn().unwrap();

    let deadline = Instant::now() + Duration::from_secs(30);
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("keep did not exit within 30 s without mutual TLS");
        }
        std::thread::sleep(Duration::from_millis(100));
    };

    let mut stderr = String::new();
    child
        .stderr
        .take()
        .unwrap()
        .read_to_string(&mut stderr)
        .unwrap();
    assert!(!status.success(), "keep must not start: {stderr}");
    assert!(stderr.contains("mutual TLS"), "unexpected stderr: {stderr}");
}
