// HANDWRITE-BEGIN gap="missing-generator:logic:eb2a130d" tracker="pending-tracker" reason="New module: KeepPrincipal (concrete principal wrapping claim_token::Scope, with authorizes(id, write)) and KeepVerifier implementing service_auth::Verifier by composing service_auth::bearer_token + claim_token::verify."
//! keep's adoption of the shared `libs/service-auth` request-auth contract (#746).
//!
//! keep's claim-check auth is intentionally NOT a blanket router gate (unlike
//! lumen's per-collection RBAC). It is:
//!
//! - **Optional** — off unless `KEEP_TOKEN_SECRET` is set; when off, claim-check
//!   is open and backward compatible.
//! - **Claim-check-worker-only** — enforced only on the worker ops `GET
//!   /inputs/{id}` (read scope) and `PUT /results/{id}` (write scope);
//!   `PUT input` / `GET result` stay open by design.
//! - **Bare-id scoped** — the token scope is checked against the bare url `id`
//!   *before* the `X-Keep-Namespace` prefix is applied (loom's settled design).
//! - **403 on every failure** — missing/invalid/expired/out-of-scope all return
//!   `403 Forbidden`, not `401`.
//!
//! The shared `service_auth::auth_middleware` is a blanket layer that runs on
//! every route of the router it wraps and renders `401 Unauthenticated` for a
//! missing/invalid token. That cannot express keep's optional, selective,
//! bare-id, 403 model without changing the wire contract — so keep does **not**
//! attach it as a layer. Instead keep adopts the plumbing the shared layer owns
//! — the [`Verifier`] trait, the concrete-principal contract, and shared
//! [`bearer_token`] extraction — and invokes the verifier *per-handler* inside
//! `check_scope`, where keep's per-resource scope authorization lives. This is
//! exactly the split the service-auth lib documents: "Authorization stays in
//! handlers."

use std::sync::Arc;

use axum::http::HeaderMap;
use service_auth::{bearer_token, AuthError, Verifier};

/// keep's concrete principal: a verified claim-check token's [`claim_token::Scope`].
///
/// keep's "open" mode (enforcement off) is modeled by the *absence* of a
/// verifier in `AppState`, not by a principal variant — so the only principal a
/// [`KeepVerifier`] yields is a successfully verified scope.
#[derive(Debug, Clone)]
pub struct KeepPrincipal {
    /// The scope the presented token authorizes (readable `r`, writable `w`).
    pub scope: claim_token::Scope,
}

impl KeepPrincipal {
    /// Whether this principal authorizes the bare claim-check `id` for the op.
    ///
    /// `write` ⇒ result `PUT` (matches `scope.w`); else input `GET` (matches
    /// `scope.r`). Keyed on the bare url id, before any namespace prefix.
    pub fn authorizes(&self, id: &str, write: bool) -> bool {
        if write {
            self.scope.w().as_str() == id
        } else {
            self.scope.r().as_str() == id
        }
    }
}

/// keep's [`service_auth::Verifier`]: wraps `claim_token::verify` (HMAC-SHA256
/// over the scoped payload) and yields a concrete [`KeepPrincipal`].
///
/// Token crypto stays in `libs/claim-token`; this only composes it. The bare-id
/// scope decision (`scope.r`/`scope.w == id`) is per-resource authorization and
/// stays in the handler (`check_scope`), per the service-auth split.
#[derive(Clone)]
pub struct KeepVerifier {
    secret: Arc<Vec<u8>>,
    /// Optional previous secret, still accepted during a rotation
    /// (from `KEEP_TOKEN_SECRET_PREVIOUS`).
    previous: Option<Arc<Vec<u8>>>,
}

impl KeepVerifier {
    /// Build a verifier over the shared HMAC `secret` (from `KEEP_TOKEN_SECRET`).
    pub fn new(secret: Arc<Vec<u8>>) -> Self {
        Self {
            secret,
            previous: None,
        }
    }

    /// Also accept tokens signed with `previous` while a secret rotation runs.
    pub fn with_previous(mut self, previous: Arc<Vec<u8>>) -> Self {
        self.previous = Some(previous);
        self
    }
}

impl Verifier for KeepVerifier {
    type Principal = KeepPrincipal;

    /// Authenticate a request's `Bearer` token into a verified [`KeepPrincipal`].
    ///
    /// Uses the shared [`bearer_token`] extractor, then `claim_token::verify`
    /// against the current unix time. A missing header is treated as an empty
    /// token, which fails verification — keep's handler renders that (and every
    /// other failure) as `403`, so the [`AuthError`] variant is not surfaced on
    /// the wire here.
    fn authenticate(&self, headers: &HeaderMap) -> Result<KeepPrincipal, AuthError> {
        let token = bearer_token(headers).unwrap_or("");
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        if let Some(scope) = claim_token::verify(&self.secret, token, now) {
            return Ok(KeepPrincipal { scope });
        }
        if let Some(previous) = &self.previous {
            if let Some(scope) = claim_token::verify(previous, token, now) {
                return Ok(KeepPrincipal { scope });
            }
        }
        Err(AuthError::Unauthenticated)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn secret() -> Arc<Vec<u8>> {
        Arc::new(b"test-secret".to_vec())
    }

    fn bearer(token: &str) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert(
            axum::http::header::AUTHORIZATION,
            format!("Bearer {token}").parse().unwrap(),
        );
        h
    }

    fn scope(r: &str, w: &str, exp: u64) -> claim_token::Scope {
        claim_token::Scope::new(
            claim_token::InputKey::new(r),
            claim_token::ResultKey::new(w),
            claim_token::ExpiryUnixSeconds::new(exp),
        )
    }

    // R1: a valid in-scope token authenticates to a concrete principal and
    // authorizes its bare id for the matching op.
    #[test]
    fn valid_token_authenticates_to_principal() {
        let v = KeepVerifier::new(secret());
        let token = claim_token::sign(&secret(), &scope("job-r", "job-w", u64::MAX));
        let p = v.authenticate(&bearer(&token)).unwrap();
        assert_eq!(p.scope.r().as_str(), "job-r");
        assert_eq!(p.scope.w().as_str(), "job-w");
        // read op authorized against scope.r, write op against scope.w.
        assert!(p.authorizes("job-r", false));
        assert!(p.authorizes("job-w", true));
    }

    // R1/R3: scope is keyed on the exact bare id — a mismatch is not authorized,
    // and the read key must not satisfy a write op (or vice versa).
    #[test]
    fn out_of_scope_id_is_not_authorized() {
        let v = KeepVerifier::new(secret());
        let token = claim_token::sign(&secret(), &scope("job-r", "job-w", u64::MAX));
        let p = v.authenticate(&bearer(&token)).unwrap();
        assert!(!p.authorizes("other", false));
        assert!(!p.authorizes("other", true));
        assert!(!p.authorizes("job-w", false));
        assert!(!p.authorizes("job-r", true));
    }

    // R2/R3: missing, malformed, wrong-secret, and expired tokens all fail
    // authentication (the handler renders every one of these as 403).
    #[test]
    fn missing_or_invalid_or_expired_token_rejected() {
        let v = KeepVerifier::new(secret());

        // Missing Authorization header.
        assert!(v.authenticate(&HeaderMap::new()).is_err());
        // Malformed bearer value.
        assert!(v.authenticate(&bearer("not-a-token")).is_err());
        // Signed with a different secret.
        let wrong = claim_token::sign(&Arc::new(b"WRONG".to_vec()), &scope("a", "b", u64::MAX));
        assert!(v.authenticate(&bearer(&wrong)).is_err());
        // Expired (exp in the past, now is well beyond 0).
        let expired = claim_token::sign(&secret(), &scope("a", "b", 0));
        assert!(v.authenticate(&bearer(&expired)).is_err());
    }

    // Rotation: while `previous` is set, a token signed with the old secret
    // and one signed with the new secret both authenticate; a third secret does not.
    #[test]
    fn rotation_accepts_old_and_new_tokens() {
        let old = Arc::new(b"old-secret".to_vec());
        let new = Arc::new(b"new-secret".to_vec());
        let v = KeepVerifier::new(new.clone()).with_previous(old.clone());
        let old_token = claim_token::sign(&old, &scope("a", "b", u64::MAX));
        let new_token = claim_token::sign(&new, &scope("a", "b", u64::MAX));
        let third = Arc::new(b"third-secret".to_vec());
        let third_token = claim_token::sign(&third, &scope("a", "b", u64::MAX));
        let p = v.authenticate(&bearer(&old_token)).unwrap();
        assert_eq!(p.scope.r().as_str(), "a");
        assert!(v.authenticate(&bearer(&new_token)).is_ok());
        assert!(v.authenticate(&bearer(&third_token)).is_err());
    }

    // Negative: without `previous` the old-secret token is rejected, so removing
    // KEEP_TOKEN_SECRET_PREVIOUS ends the rotation.
    #[test]
    fn rotation_rejects_old_token_after_previous_removed() {
        let old = Arc::new(b"old-secret".to_vec());
        let new = Arc::new(b"new-secret".to_vec());
        let v = KeepVerifier::new(new.clone());
        let old_token = claim_token::sign(&old, &scope("a", "b", u64::MAX));
        let new_token = claim_token::sign(&new, &scope("a", "b", u64::MAX));
        assert!(v.authenticate(&bearer(&old_token)).is_err());
        assert!(v.authenticate(&bearer(&new_token)).is_ok());
    }
}
// HANDWRITE-END
