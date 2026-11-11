//! Process-level rustls crypto provider install.
//!
//! Several keep build paths link rustls: the k8s operator (kube-rs → hyper +
//! rustls), the raft-runtime peer transport, and the online CLI ops (`upgrade` /
//! `issue`, via reqwest). rustls 0.23 refuses to pick a default crypto provider
//! when more than one is linked in the process, so any binary that reaches a TLS
//! path must install one explicitly — before the first `ClientConfig` /
//! `ServerConfig` is built — or it panics at runtime. `keep` calls this once at
//! the very top of `main`, before command parsing, so every path (serve,
//! operator, upgrade, issue) is covered.
//!
//! Builds that link no rustls path (the default `keep` build) compile this as a
//! no-op, so the fast dev/test cycle pays nothing.
//!
//! The install itself is delegated to core `peer-tls`, which installs the
//! aws-lc-rs provider once for every peer-mTLS path.
//!
//! @spec .aw/tech-design/projects/keep/interfaces/cli/adopt-libs-operator-keep-k8s-crd-operator-instance-cli.md

/// Install the process default rustls crypto provider, once, by delegating to
/// core `peer-tls`, which installs it once for every peer-mTLS path.
///
/// Idempotent: a second call — or a provider a dependency installed first — is
/// ignored (we only need *a* default present). A no-op in builds without a
/// rustls path (the private `rustls-provider` feature is off).
pub fn install_default_crypto_provider() {
    #[cfg(feature = "rustls-provider")]
    peer_tls::install_default_crypto_provider();
}

#[cfg(all(test, feature = "rustls-provider"))]
mod tests {
    #[test]
    fn install_is_idempotent_and_sets_default() {
        super::install_default_crypto_provider();
        super::install_default_crypto_provider();
        assert!(rustls::crypto::CryptoProvider::get_default().is_some());
    }
}
