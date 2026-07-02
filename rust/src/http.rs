//! The HTTP transport and its fail-closed guards (ADR-0005, ADR-0011).
//!
//! rmcp ships no auth provider, so — unlike the TypeScript server, which hands the
//! framework an `APIKeyAuthProvider` — the Rust variant serves rmcp's tower-based
//! [`StreamableHttpService`] nested under the configured endpoint and puts the
//! security posture in front of it as ordinary axum middleware:
//!
//! - **API-key auth** ([`require_api_key`], the default): a request must carry an
//!   `X-API-Key` header whose value is one of the configured keys, or it is rejected
//!   with `401` BEFORE the MCP service ever sees it. This mirrors the TS
//!   `APIKeyAuthProvider` (same default header, same "any configured key admits"
//!   semantics). It is skipped entirely — and a cleartext warning logged by the
//!   caller — only when `QMP_MCP_ALLOW_INSECURE=true` (local dev). stdio never routes
//!   through here and stays auth-free.
//! - **JWT auth** ([`require_jwt`], selected by `QMP_MCP_AUTH=jwt`): a request must
//!   carry an `Authorization: Bearer <token>` header whose JWT verifies against the
//!   configured `QMP_MCP_JWT_SECRET`, or it is rejected with `401` before the MCP
//!   service runs. Verification is **pinned to HS256** — the algorithm is fixed by
//!   the verifier, never read from the token's own header — so a token presenting
//!   `alg: none` or a weaker/other algorithm is refused, not trusted. This mirrors
//!   the TS `JWTAuthProvider` built with `algorithms: ['HS256']`. Exactly one of the
//!   API-key or JWT layer is installed (per `QMP_MCP_AUTH`); insecure mode installs
//!   neither.
//! - **DNS-rebinding / CORS origin guard** ([`guard_origin`]): a browser request
//!   whose `Origin` is not in the configured allowlist is rejected with `403`
//!   before any handler runs; a request with no `Origin` (curl, MCP SDK clients) is
//!   always allowed through. This is the same control the TS `cors.allowedOrigins`
//!   applies. It runs OUTSIDE the auth layer, so a hostile page is refused whether
//!   or not it guessed a key.
//!
//! Fail-closed startup (missing/blank key under HTTP) is enforced earlier, at
//! config load (`crate::config`), exactly as in the TS server; by the time a router
//! is built the credentials are guaranteed present (or insecure mode is explicit).

use std::sync::Arc;

use axum::{
    extract::{Request, State},
    http::{header, HeaderMap, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    Router,
};
use jsonwebtoken::{decode, Algorithm, DecodingKey, Validation};
use rmcp::transport::streamable_http_server::{
    session::local::LocalSessionManager, StreamableHttpServerConfig, StreamableHttpService,
};

use crate::config::{AuthMode, Config};
use crate::server::QmpMcpServer;

/// Shared, immutable configuration for the two guard middlewares, cheap to clone
/// (both fields are `Arc`) so each request handler gets its own handle.
#[derive(Clone)]
struct GuardState {
    /// The set of accepted API keys. Non-empty (and consulted) only in `apikey`
    /// mode, where the [`require_api_key`] layer is installed.
    api_keys: Arc<Vec<String>>,
    /// Browser origins permitted by the DNS-rebinding guard.
    allowed_origins: Arc<Vec<String>>,
    /// HS256 signing-secret bytes for the JWT guard. Non-empty (and consulted) only
    /// in `jwt` mode, where the [`require_jwt`] layer is installed; config has already
    /// failed closed if the secret was missing, so it is guaranteed present there.
    jwt_secret: Arc<Vec<u8>>,
}

/// True when `headers` carries an `X-API-Key` whose value exactly equals one of
/// `keys`. Absent header, non-ASCII value, or no match all read as false — the
/// request is unauthenticated. Header lookup is case-insensitive, so `X-API-Key`
/// and `x-api-key` are equivalent (parity with the TS provider's default header).
fn api_key_ok(headers: &HeaderMap, keys: &[String]) -> bool {
    let Some(provided) = headers.get("x-api-key").and_then(|v| v.to_str().ok()) else {
        return false;
    };
    keys.iter().any(|k| k == provided)
}

/// Whether `headers` carries a valid `Authorization: Bearer <jwt>` whose token
/// verifies against `secret` under **HS256 only**. The algorithm is fixed by the
/// verifier (`Validation::new(Algorithm::HS256)`), never taken from the token's own
/// `alg` header, so a token presenting `alg: none` or any other/weaker algorithm is
/// rejected rather than trusted (parity with the TS `JWTAuthProvider` built with
/// `algorithms: ['HS256']`). An absent header, a non-`Bearer` scheme, a malformed
/// token, a bad signature, an expired token, or a wrong algorithm all read as false —
/// the request is unauthenticated. Header lookup is case-insensitive.
///
/// The claim-*presence* checks are relaxed to match the TS provider exactly: npm
/// `jsonwebtoken.verify(token, secret, { algorithms: ['HS256'] })` requires no claims
/// and only checks `aud` when an expected audience is configured, whereas the Rust
/// crate's defaults would additionally *require* `exp` and reject any token carrying
/// an `aud`. Clearing `required_spec_claims` and disabling the audience check restores
/// parity while KEEPING signature verification, the HS256 pin, and expiry validation
/// (a token whose `exp` is in the past is still rejected).
fn jwt_ok(headers: &HeaderMap, secret: &[u8]) -> bool {
    let Some(auth) = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
    else {
        return false;
    };
    // Require the `Bearer` scheme; the token is the remainder (parity with the TS
    // provider's `requireBearer` default). A missing or different scheme is unauthed.
    let Some(token) = auth.strip_prefix("Bearer ") else {
        return false;
    };
    let mut validation = Validation::new(Algorithm::HS256);
    validation.required_spec_claims.clear();
    validation.validate_aud = false;
    decode::<serde_json::Value>(token, &DecodingKey::from_secret(secret), &validation).is_ok()
}

/// Whether a request's `Origin` is permitted by the DNS-rebinding guard. A request
/// with no `Origin` header (non-browser clients such as curl or the MCP SDK) is
/// always allowed; a present `Origin` must be one of `allowed`, else the request is
/// refused. A malformed (non-ASCII) `Origin` is treated as present-and-disallowed.
fn origin_ok(headers: &HeaderMap, allowed: &[String]) -> bool {
    match headers.get(header::ORIGIN) {
        None => true,
        Some(value) => match value.to_str() {
            Ok(origin) => allowed.iter().any(|a| a == origin),
            Err(_) => false,
        },
    }
}

/// axum middleware: reject any request without a valid `X-API-Key` with `401`
/// before it can reach the MCP service (ADR-0005 fail-closed). Installed only in
/// authenticated mode.
async fn require_api_key(
    State(state): State<GuardState>,
    request: Request,
    next: Next,
) -> Response {
    if api_key_ok(request.headers(), &state.api_keys) {
        next.run(request).await
    } else {
        (
            StatusCode::UNAUTHORIZED,
            "Unauthorized: a valid X-API-Key header is required.\n",
        )
            .into_response()
    }
}

/// axum middleware: reject any request that does not carry a valid `Authorization:
/// Bearer <jwt>` (HS256, verified against the configured secret) with `401` before it
/// can reach the MCP service (ADR-0005 fail-closed). Installed only when
/// `QMP_MCP_AUTH=jwt` and not in insecure mode. Mirrors the TS `JWTAuthProvider`.
async fn require_jwt(State(state): State<GuardState>, request: Request, next: Next) -> Response {
    if jwt_ok(request.headers(), &state.jwt_secret) {
        next.run(request).await
    } else {
        (
            StatusCode::UNAUTHORIZED,
            "Unauthorized: a valid Authorization: Bearer <JWT> is required.\n",
        )
            .into_response()
    }
}

/// axum middleware: reject a browser request whose `Origin` is not allowlisted with
/// `403` (the DNS-rebinding guard). Runs outside [`require_api_key`], so a hostile
/// cross-origin page is refused regardless of whether it supplied a key.
async fn guard_origin(State(state): State<GuardState>, request: Request, next: Next) -> Response {
    if origin_ok(request.headers(), &state.allowed_origins) {
        next.run(request).await
    } else {
        (
            StatusCode::FORBIDDEN,
            "Forbidden: this Origin is not permitted.\n",
        )
            .into_response()
    }
}

/// Build the tower [`StreamableHttpService`] that speaks the MCP streamable-HTTP
/// protocol. The `service_factory` is called once per session and hands back a
/// clone of `server`; every clone shares the same `Arc<Mutex<Orchestrator>>`, so
/// concurrent HTTP sessions still drive the single managed Instance (ADR-0011).
fn build_mcp_service(server: QmpMcpServer) -> StreamableHttpService<QmpMcpServer> {
    StreamableHttpService::new(
        move || Ok(server.clone()),
        LocalSessionManager::default().into(),
        StreamableHttpServerConfig::default(),
    )
}

/// Assemble the HTTP application: the MCP service nested under `config.http_endpoint`,
/// fronted by the origin guard (always) and — unless `config.allow_insecure` — the
/// fail-closed API-key auth layer.
///
/// The auth provider is selected by `config.auth_mode`: `apikey` installs the
/// [`require_api_key`] layer (the default), `jwt` installs [`require_jwt`]; insecure
/// mode installs neither. Layer order matters: axum runs the LAST-added layer first,
/// so the origin guard is added last and executes ahead of the auth layer — a
/// DNS-rebinding attempt is refused (`403`) before any credential is inspected. By the
/// time this runs the config has already failed closed if the selected provider's
/// credential was missing (`crate::config`), so an authenticated build always has the
/// key set (or JWT secret) it needs.
pub fn build_router(config: &Config, server: QmpMcpServer) -> Router {
    let state = GuardState {
        api_keys: Arc::new(config.api_keys.clone()),
        allowed_origins: Arc::new(config.allowed_origins.clone()),
        jwt_secret: Arc::new(config.jwt_secret.clone().unwrap_or_default().into_bytes()),
    };

    let mut app = Router::new().nest_service(&config.http_endpoint, build_mcp_service(server));

    // Auth first (inner), so it is closest to the MCP service. Exactly one provider is
    // installed, chosen by config.auth_mode; insecure mode installs none.
    if !config.allow_insecure {
        app = match config.auth_mode {
            AuthMode::ApiKey => app.layer(middleware::from_fn_with_state(
                state.clone(),
                require_api_key,
            )),
            AuthMode::Jwt => app.layer(middleware::from_fn_with_state(state.clone(), require_jwt)),
        };
    }
    // Origin guard last (outer), so it runs before auth.
    app.layer(middleware::from_fn_with_state(state, guard_origin))
}

/// Bind and serve the HTTP transport until `shutdown` resolves (a termination
/// signal, or — in `both` mode — the stdio transport closing). Returns an
/// actionable error if the configured host/port cannot be bound.
///
/// The MCP service (with the auth + origin guards in front) is served with `axum`;
/// `Body` is axum's request body, which satisfies the `http_body::Body` bound
/// [`StreamableHttpService`] requires.
pub async fn serve<F>(
    config: &Config,
    server: QmpMcpServer,
    shutdown: F,
) -> Result<(), Box<dyn std::error::Error>>
where
    F: std::future::Future<Output = ()> + Send + 'static,
{
    let router: Router<()> = build_router(config, server);
    let addr = format!("{}:{}", config.http_host, config.http_port);
    let listener = tokio::net::TcpListener::bind(&addr).await.map_err(|err| {
        format!(
            "failed to bind the HTTP transport to {addr}: {err}. \
             Is the port already in use, or are QMP_MCP_HTTP_HOST/QMP_MCP_HTTP_PORT valid?"
        )
    })?;
    axum::serve(listener, router)
        .with_graceful_shutdown(shutdown)
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for (name, value) in pairs {
            map.insert(
                axum::http::HeaderName::from_bytes(name.as_bytes()).unwrap(),
                HeaderValue::from_str(value).unwrap(),
            );
        }
        map
    }

    #[test]
    fn api_key_allows_a_configured_key() {
        let keys = vec!["k1".to_string(), "k2".to_string()];
        assert!(api_key_ok(&headers(&[("X-API-Key", "k1")]), &keys));
        assert!(api_key_ok(&headers(&[("X-API-Key", "k2")]), &keys));
        // Header lookup is case-insensitive.
        assert!(api_key_ok(&headers(&[("x-api-key", "k1")]), &keys));
    }

    #[test]
    fn api_key_denies_missing_or_wrong_key() {
        let keys = vec!["k1".to_string()];
        // No header at all.
        assert!(!api_key_ok(&headers(&[]), &keys));
        // Wrong value.
        assert!(!api_key_ok(&headers(&[("X-API-Key", "nope")]), &keys));
        // Empty value never matches a (non-empty) configured key.
        assert!(!api_key_ok(&headers(&[("X-API-Key", "")]), &keys));
        // A different header carrying the key does not count.
        assert!(!api_key_ok(&headers(&[("Authorization", "k1")]), &keys));
    }

    #[test]
    fn api_key_denies_everything_when_no_keys_configured() {
        assert!(!api_key_ok(&headers(&[("X-API-Key", "k1")]), &[]));
    }

    /// The HS256 signing secret used across the JWT unit tests.
    const JWT_SECRET: &[u8] = b"jwt-signing-secret";

    /// Current UNIX time in whole seconds, for minting `exp`/`nbf` claims.
    fn now_secs() -> i64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64
    }

    /// Mint a JWT signed with `secret` under `alg` from a JSON claims value. Used to
    /// build both valid HS256 tokens and adversarial ones (wrong secret / wrong alg).
    fn mint(secret: &[u8], alg: Algorithm, claims: &serde_json::Value) -> String {
        use jsonwebtoken::{encode, EncodingKey, Header};
        encode(&Header::new(alg), claims, &EncodingKey::from_secret(secret)).unwrap()
    }

    /// Wrap a raw token in an `Authorization: Bearer <token>` header map.
    fn bearer(token: &str) -> HeaderMap {
        let value = format!("Bearer {token}");
        headers(&[("Authorization", value.as_str())])
    }

    #[test]
    fn jwt_allows_a_valid_hs256_token() {
        let token = mint(
            JWT_SECRET,
            Algorithm::HS256,
            &serde_json::json!({ "sub": "agent", "exp": now_secs() + 3600 }),
        );
        assert!(jwt_ok(&bearer(&token), JWT_SECRET));
    }

    #[test]
    fn jwt_allows_a_valid_token_without_exp() {
        // Parity with the TS provider: `exp` is optional (not required), so a signed
        // token carrying no expiry is accepted.
        let token = mint(
            JWT_SECRET,
            Algorithm::HS256,
            &serde_json::json!({ "sub": "agent" }),
        );
        assert!(jwt_ok(&bearer(&token), JWT_SECRET));
    }

    #[test]
    fn jwt_denies_missing_or_malformed_authorization() {
        let token = mint(
            JWT_SECRET,
            Algorithm::HS256,
            &serde_json::json!({ "sub": "agent" }),
        );
        // No Authorization header at all.
        assert!(!jwt_ok(&headers(&[]), JWT_SECRET));
        // Present, valid token, but WITHOUT the required `Bearer ` scheme prefix.
        assert!(!jwt_ok(
            &headers(&[("Authorization", token.as_str())]),
            JWT_SECRET
        ));
        // A different scheme carrying the valid token does not count.
        let token_scheme = format!("Token {token}");
        assert!(!jwt_ok(
            &headers(&[("Authorization", token_scheme.as_str())]),
            JWT_SECRET
        ));
        // Bearer scheme but a non-JWT garbage token.
        assert!(!jwt_ok(&bearer("not-a-jwt"), JWT_SECRET));
    }

    #[test]
    fn jwt_denies_bad_signature() {
        // A correctly-formed HS256 token, but signed with a different secret.
        let token = mint(
            b"a-different-secret",
            Algorithm::HS256,
            &serde_json::json!({ "sub": "agent", "exp": now_secs() + 3600 }),
        );
        assert!(!jwt_ok(&bearer(&token), JWT_SECRET));
    }

    #[test]
    fn jwt_denies_wrong_algorithm() {
        // Signed with HS384 (a different algorithm) using the SAME secret. The verifier
        // is pinned to HS256, so the token's chosen alg is refused — a token can never
        // pick the algorithm it is verified under.
        let token = mint(
            JWT_SECRET,
            Algorithm::HS384,
            &serde_json::json!({ "sub": "agent", "exp": now_secs() + 3600 }),
        );
        assert!(!jwt_ok(&bearer(&token), JWT_SECRET));
    }

    #[test]
    fn jwt_denies_alg_none() {
        // The classic downgrade attack: an unsigned token whose header is
        // {"alg":"none","typ":"JWT"} with an empty signature segment. HS256 pinning
        // rejects it outright — there is no signature to verify.
        const ALG_NONE: &str = "eyJhbGciOiJub25lIiwidHlwIjoiSldUIn0.eyJzdWIiOiJhbGctbm9uZSJ9.";
        assert!(!jwt_ok(&bearer(ALG_NONE), JWT_SECRET));
    }

    #[test]
    fn jwt_denies_expired_token() {
        // `exp` an hour in the past — beyond the crate's default 60s leeway.
        let token = mint(
            JWT_SECRET,
            Algorithm::HS256,
            &serde_json::json!({ "sub": "agent", "exp": now_secs() - 3600 }),
        );
        assert!(!jwt_ok(&bearer(&token), JWT_SECRET));
    }

    #[test]
    fn origin_allows_absent_and_allowlisted() {
        let allowed = vec![
            "http://localhost:8080".to_string(),
            "http://127.0.0.1:8080".to_string(),
        ];
        // Non-browser client: no Origin header.
        assert!(origin_ok(&headers(&[]), &allowed));
        // An allowlisted browser Origin.
        assert!(origin_ok(
            &headers(&[("Origin", "http://localhost:8080")]),
            &allowed
        ));
    }

    #[test]
    fn origin_denies_unlisted() {
        let allowed = vec!["http://localhost:8080".to_string()];
        assert!(!origin_ok(
            &headers(&[("Origin", "http://evil.example")]),
            &allowed
        ));
        // Exact match required — a different port is a different origin.
        assert!(!origin_ok(
            &headers(&[("Origin", "http://localhost:9999")]),
            &allowed
        ));
    }
}
