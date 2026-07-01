//! The HTTP transport and its fail-closed guards (ADR-0005, ADR-0011).
//!
//! rmcp ships no auth provider, so — unlike the TypeScript server, which hands the
//! framework an `APIKeyAuthProvider` — the Rust variant serves rmcp's tower-based
//! [`StreamableHttpService`] nested under the configured endpoint and puts the
//! security posture in front of it as ordinary axum middleware:
//!
//! - **API-key auth** ([`require_api_key`]): a request must carry an `X-API-Key`
//!   header whose value is one of the configured keys, or it is rejected with `401`
//!   BEFORE the MCP service ever sees it. This mirrors the TS `APIKeyAuthProvider`
//!   (same default header, same "any configured key admits" semantics). It is
//!   skipped entirely — and a cleartext warning logged by the caller — only when
//!   `QMP_MCP_ALLOW_INSECURE=true` (local dev). stdio never routes through here and
//!   stays auth-free.
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
use rmcp::transport::streamable_http_server::{
    session::local::LocalSessionManager, StreamableHttpServerConfig, StreamableHttpService,
};

use crate::config::Config;
use crate::server::QmpMcpServer;

/// Shared, immutable configuration for the two guard middlewares, cheap to clone
/// (both fields are `Arc`) so each request handler gets its own handle.
#[derive(Clone)]
struct GuardState {
    /// The set of accepted API keys. Empty only in insecure mode, where the
    /// [`require_api_key`] layer is not installed at all.
    api_keys: Arc<Vec<String>>,
    /// Browser origins permitted by the DNS-rebinding guard.
    allowed_origins: Arc<Vec<String>>,
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
/// Layer order matters: axum runs the LAST-added layer first, so the origin guard is
/// added last and executes ahead of the auth layer — a DNS-rebinding attempt is
/// refused (`403`) before the key is ever inspected. By the time this runs the
/// config has already failed closed if a required key was missing (`crate::config`),
/// so an authenticated build always has a non-empty key set.
pub fn build_router(config: &Config, server: QmpMcpServer) -> Router {
    let state = GuardState {
        api_keys: Arc::new(config.api_keys.clone()),
        allowed_origins: Arc::new(config.allowed_origins.clone()),
    };

    let mut app = Router::new().nest_service(&config.http_endpoint, build_mcp_service(server));

    // Auth first (inner), so it is closest to the MCP service.
    if !config.allow_insecure {
        app = app.layer(middleware::from_fn_with_state(
            state.clone(),
            require_api_key,
        ));
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
