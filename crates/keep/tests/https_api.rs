//! Public HTTP API over TLS: a CA-trusting client reaches `/healthz` over
//! HTTPS, plain HTTP to the TLS port fails, and an unreadable or invalid
//! certificate makes startup fail with a clear error.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use keep::{AppState, KvEngine};

/// Writes a self-signed `localhost` certificate and key into `dir`.
/// Returns the certificate path, the key path and the certificate PEM.
fn write_pki(dir: &Path) -> (PathBuf, PathBuf, String) {
    let ck = rcgen::generate_simple_self_signed(vec!["localhost".to_string()]).unwrap();
    let cert_pem = ck.cert.pem();
    let key_pem = ck.key_pair.serialize_pem();
    let cert = dir.join("cert.pem");
    let key = dir.join("key.pem");
    std::fs::write(&cert, &cert_pem).unwrap();
    std::fs::write(&key, key_pem).unwrap();
    (cert, key, cert_pem)
}

#[tokio::test]
async fn https_serves_healthz_and_rejects_plain_http() {
    let dir = tempfile::tempdir().unwrap();
    let (cert, key, cert_pem) = write_pki(dir.path());

    // Load before building any reqwest client: this installs the rustls
    // default crypto provider.
    let cfg = keep::https::load_server_config(&cert, &key).unwrap();

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let app = keep::router(AppState::new(Arc::new(KvEngine::with_shards(16))));
    tokio::spawn(keep::https::serve_tls(
        listener,
        app,
        cfg,
        std::future::pending(),
    ));

    let port = addr.port();
    let trusted = reqwest::Client::builder()
        .add_root_certificate(reqwest::Certificate::from_pem(cert_pem.as_bytes()).unwrap())
        .resolve("localhost", addr)
        .timeout(Duration::from_secs(5))
        .build()
        .unwrap();
    let resp = trusted
        .get(format!("https://localhost:{port}/healthz"))
        .send()
        .await
        .expect("CA-trusting HTTPS client reaches /healthz");
    assert_eq!(resp.status(), reqwest::StatusCode::OK);

    // Plain HTTP to the TLS port must not get an answer.
    let plain = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .unwrap();
    let plain_result = plain
        .get(format!("http://127.0.0.1:{port}/healthz"))
        .send()
        .await;
    assert!(
        plain_result.is_err(),
        "plain HTTP to the TLS port must fail, got {plain_result:?}"
    );

    // An HTTPS client that does not trust the CA must fail the handshake.
    let untrusted = reqwest::Client::builder()
        .resolve("localhost", addr)
        .timeout(Duration::from_secs(5))
        .build()
        .unwrap();
    let untrusted_result = untrusted
        .get(format!("https://localhost:{port}/healthz"))
        .send()
        .await;
    assert!(
        untrusted_result.is_err(),
        "an untrusted certificate must be rejected, got {untrusted_result:?}"
    );
}

#[test]
fn load_server_config_errors_name_the_files() {
    let missing = keep::https::load_server_config(
        Path::new("/nonexistent/cert.pem"),
        Path::new("/nonexistent/key.pem"),
    );
    let Err(err) = missing else {
        panic!("a missing certificate must fail");
    };
    let text = format!("{err:#}");
    assert!(
        text.contains("/nonexistent/cert.pem"),
        "error must name the certificate file, got: {text}"
    );

    // A certificate file that is not PEM, with a valid key, must fail.
    let dir = tempfile::tempdir().unwrap();
    let (_cert, key, _pem) = write_pki(dir.path());
    let bad_cert = dir.path().join("bad-cert.pem");
    std::fs::write(&bad_cert, "not a pem").unwrap();
    assert!(
        keep::https::load_server_config(&bad_cert, &key).is_err(),
        "a non-PEM certificate must fail"
    );
}

#[test]
fn binary_fails_startup_on_unreadable_certificate() {
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_keep"))
        .args([
            "--host",
            "127.0.0.1",
            "--port",
            "0",
            "--disable-persistence",
            "--tls-cert",
            "/nonexistent/cert.pem",
            "--tls-key",
            "/nonexistent/key.pem",
        ])
        .env_remove("KEEP_TLS_CERT")
        .env_remove("KEEP_TLS_KEY")
        .output()
        .unwrap();
    assert!(!output.status.success(), "startup must fail");
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stderr),
        String::from_utf8_lossy(&output.stdout)
    );
    assert!(
        text.contains("load TLS certificate"),
        "error must say the TLS certificate failed to load, got: {text}"
    );
}

#[test]
fn binary_fails_when_only_one_tls_flag_is_set() {
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_keep"))
        .args([
            "--host",
            "127.0.0.1",
            "--port",
            "0",
            "--disable-persistence",
            "--tls-cert",
            "/nonexistent/cert.pem",
        ])
        .env_remove("KEEP_TLS_CERT")
        .env_remove("KEEP_TLS_KEY")
        .output()
        .unwrap();
    assert!(!output.status.success(), "startup must fail");
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stderr),
        String::from_utf8_lossy(&output.stdout)
    );
    assert!(
        text.contains("must be set together"),
        "error must say both TLS flags are required, got: {text}"
    );
}
