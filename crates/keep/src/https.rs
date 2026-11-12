//! TLS for the public HTTP API: load the PEM certificate and key, and serve
//! HTTP/1.1 and HTTP/2 over TLS through `service-http`.

use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result};
use axum::Router;
use tokio::net::TcpListener;

/// Load the certificate chain and private key for the public API.
pub fn load_server_config(cert: &Path, key: &Path) -> Result<Arc<rustls::ServerConfig>> {
    let tls = peer_tls::PeerTlsConfig {
        cert: cert.to_path_buf(),
        key: key.to_path_buf(),
        ca: cert.to_path_buf(),
        required: false,
    };
    let mut config = tls.rustls_server_config().with_context(|| {
        format!(
            "load TLS certificate {} and key {}",
            cert.display(),
            key.display()
        )
    })?;
    // peer-tls advertises only `h2`; the public port also serves HTTP/1.1.
    config.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
    Ok(Arc::new(config))
}

/// Wrap a loaded config as the source `service-http` reads from.
pub fn config_source(cfg: Arc<rustls::ServerConfig>) -> service_http::ServerConfigSource {
    service_http::config_source(move || Some(cfg.clone()))
}

/// Serve `app` over TLS until `shutdown` resolves. There is no cleartext
/// fallback here: callers choose this path only when TLS is configured.
pub async fn serve_tls(
    listener: TcpListener,
    app: Router,
    cfg: Arc<rustls::ServerConfig>,
    shutdown: impl std::future::Future<Output = ()> + Send + 'static,
) {
    service_http::serve_tls(listener, app, config_source(cfg), shutdown).await
}
