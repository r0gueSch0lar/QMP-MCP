//! The noVNC browser Viewer (ADR-0010): an in-process, no-external-binary bridge
//! between a browser and the Guest's loopback VNC Display. It owns its OWN axum HTTP
//! server (independent of the MCP transport, so it works under `TRANSPORT=stdio`) and
//! does three things, all fail-closed behind a single human-facing secret
//! (`QMP_MCP_VIEWER_PASSWORD`):
//!
//!   1. Serves the noVNC static app, EMBEDDED in the binary via `include_dir!`
//!      (ADR-0011). Because the assets are compiled in, a request can only resolve to
//!      an embedded file — there is no runtime directory to traverse out of, so static
//!      serving is traversal-safe by construction, and the Viewer works identically on
//!      bare metal and in the container.
//!   2. Refuses the page AND the websocket unless the request authenticates (HTTP
//!      Basic with the Viewer password, constant-time compared). No password ⇒ it
//!      cannot serve (fail-closed).
//!   3. Upgrades an authenticated websocket and relays its bytes to the SERVER's
//!      loopback VNC TCP port. The proxy target is ALWAYS the server-controlled
//!      loopback endpoint — never taken from the client (no SSRF).
//!
//! Because the page is only served post-auth, the server-generated VNC password is
//! embedded into the page so noVNC auto-authenticates: one human-facing secret, with
//! the internal VNC password still guarding the loopback VNC from other local
//! processes as a second layer. A second implementation of the shared bounded context,
//! mirroring `../../typescript/src/viewer/viewer.ts` behaviorally (auth, anti-clickjacking,
//! Origin check, connection cap + backpressure + max payload, ws-path enforcement,
//! cleartext-on-non-loopback warning).

use std::sync::Arc;

use async_trait::async_trait;
use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        Path, Request, State,
    },
    http::{header, HeaderMap, HeaderValue, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::get,
    Router,
};
use bytes::Bytes;
use futures_util::{SinkExt, StreamExt};
use include_dir::{include_dir, Dir};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{Mutex, Semaphore};
use tokio::task::JoinHandle;

/// The HTTP Basic realm shown to the browser when it prompts for credentials.
const REALM: &str = "qmp-mcp viewer";

/// URL prefix the embedded noVNC assets are served under (the page's `import` of
/// `core/rfb.js` and its transitive imports resolve beneath it).
const ASSET_PREFIX: &str = "/novnc/";

/// URL path the browser opens its VNC websocket on. Only this path is a valid
/// upgrade target; any other path serves the page/404 and never upgrades (F6).
const WEBSOCKET_PATH: &str = "/websockify";

/// Cap on concurrent authenticated Viewer websocket connections (F5). The Viewer
/// fronts a single Instance's Display, so a handful of viewers is plenty; capping
/// bounds the fan-out of relays a single leaked password could open. The N+1th
/// upgrade is refused with 503. Mirrors the TS `MAX_VIEWER_CONNECTIONS`.
pub const MAX_VIEWER_CONNECTIONS: usize = 2;

/// Per-message ceiling on inbound websocket frames (browser -> VNC). RFB client
/// messages are tiny (key/pointer events, small cut-text), so a modest cap bounds
/// the memory a single frame can pin without impeding normal use (F5). Mirrors the
/// TS `MAX_WS_PAYLOAD_BYTES`.
const MAX_WS_PAYLOAD_BYTES: usize = 4 * 1024 * 1024;

/// Chunk size for reads off the VNC socket on the way to the browser. The websocket
/// sink's `send().await` applies backpressure, so a slow browser naturally throttles
/// reads off VNC — the relay never buffers an unbounded framebuffer backlog.
const VNC_READ_CHUNK: usize = 64 * 1024;

/// The noVNC static app, vendored under `assets/novnc/` and EMBEDDED in the binary
/// (ADR-0011). Only what the page actually fetches is vendored (the package's `core/`
/// and `vendor/` plus its LICENSE); serving is confined to these embedded files by
/// construction, so there is no path-traversal surface.
static ASSETS: Dir<'static> = include_dir!("$CARGO_MANIFEST_DIR/assets/novnc");

/// The (post-auth) noVNC page template. The single `__VNC_PASSWORD_JSON__` token is
/// replaced with the JSON-encoded server-generated VNC password so noVNC
/// auto-authenticates; the asset prefix and websocket path are the module constants,
/// kept in sync by [`tests`]. Everything else is verbatim from the TS `renderPage`.
const PAGE_TEMPLATE: &str = r#"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>qmp-mcp viewer</title>
<style>
  html, body { margin: 0; height: 100%; background: #282828; overflow: hidden; }
  #status { position: fixed; top: 0; left: 0; right: 0; z-index: 10; margin: 0; padding: 4px 10px;
            font: 13px/1.6 system-ui, sans-serif; color: #ddd; background: rgba(0, 0, 0, 0.6); }
  #screen { width: 100%; height: 100%; }
</style>
</head>
<body>
<p id="status">connecting…</p>
<div id="screen"></div>
<script type="module">
  import RFB from '/novnc/core/rfb.js';
  const password = __VNC_PASSWORD_JSON__;
  const status = document.getElementById('status');
  const scheme = location.protocol === 'https:' ? 'wss' : 'ws';
  const url = `${scheme}://${location.host}/websockify`;
  const rfb = new RFB(document.getElementById('screen'), url, { credentials: { password } });
  rfb.viewOnly = false;
  rfb.scaleViewport = true;
  rfb.addEventListener('connect', () => { status.style.display = 'none'; });
  rfb.addEventListener('disconnect', (e) => {
    status.style.display = 'block';
    status.textContent = e.detail && e.detail.clean ? 'disconnected' : 'connection lost';
  });
  rfb.addEventListener('securityfailure', () => { status.textContent = 'authentication failed'; });
</script>
</body>
</html>
"#;

/// Everything the Viewer needs to serve one Instance's Display (mirrors the TS
/// `ViewerOptions`). The Orchestrator injects it when a `display: vnc` Instance
/// starts.
#[derive(Debug, Clone)]
pub struct ViewerOptions {
    /// Address the Viewer's HTTP server binds to (`QMP_MCP_VIEWER_HOST`).
    pub host: String,
    /// TCP port the Viewer's HTTP server listens on (`QMP_MCP_VIEWER_PORT`); `0`
    /// binds an ephemeral port (used by the tests).
    pub port: u16,
    /// The human-facing gate (`QMP_MCP_VIEWER_PASSWORD`). The page and the websocket
    /// are refused unless the request authenticates with it via HTTP Basic.
    pub password: String,
    /// Optional username enforced alongside the password (`QMP_MCP_VIEWER_USER`). When
    /// `Some`, the HTTP Basic username must also match (constant-time); when `None` the
    /// username half is ignored — the historical password-only behavior.
    pub user: Option<String>,
    /// Loopback host of the Guest's VNC Display the proxy always dials.
    pub vnc_host: String,
    /// Loopback TCP port of the Guest's VNC Display the proxy always dials.
    pub vnc_port: u16,
    /// The server-generated VNC password, embedded into the (post-auth) page so
    /// noVNC auto-authenticates. It never leaves an authenticated response.
    pub vnc_password: String,
}

/// Raised when the Viewer cannot start (no password, or the bind failed). The message
/// is actionable and is surfaced (wrapped) to the agent by the Orchestrator. Mirrors
/// the error text the TS `startViewer` rejects with.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct ViewerError(pub String);

/// A running Viewer. Its lifetime equals the Instance's (ADR-0010); the Orchestrator
/// holds it and calls [`stop`](ViewerHandle::stop) on destroy/failure.
#[async_trait]
pub trait ViewerHandle: Send + Sync {
    /// Drop all websocket clients and close the HTTP server. Idempotent.
    async fn stop(&self);
}

/// Factory that starts a Viewer for a `display: vnc` Instance. Injected into the
/// Orchestrator so the lifecycle is testable without binding a real port; the
/// singleton wires in [`RealViewerFactory`]. Mirrors the TS `startViewer` seam.
#[async_trait]
pub trait ViewerFactory: Send + Sync {
    /// Start the Viewer and return a handle, or fail closed with an actionable error.
    async fn start(&self, options: ViewerOptions) -> Result<Box<dyn ViewerHandle>, ViewerError>;
}

/// The production factory: starts the real in-process axum Viewer.
pub struct RealViewerFactory;

#[async_trait]
impl ViewerFactory for RealViewerFactory {
    async fn start(&self, options: ViewerOptions) -> Result<Box<dyn ViewerHandle>, ViewerError> {
        start_viewer(options)
            .await
            .map(|viewer| Box::new(viewer) as Box<dyn ViewerHandle>)
    }
}

// ---------------------------------------------------------------------------
// Pure security helpers (unit-tested in isolation)
// ---------------------------------------------------------------------------

/// The hosts treated as loopback. A bind to anything else is reachable off-box, so
/// the Viewer's cleartext Basic + interactive VNC would travel in the clear and we
/// warn (F1). Kept deliberately narrow — the exact set ADR-0010 documents as local.
fn is_loopback_host(host: &str) -> bool {
    matches!(
        host.trim().to_lowercase().as_str(),
        "127.0.0.1" | "localhost" | "::1" | "[::1]"
    )
}

/// Content type for the handful of extensions the noVNC assets use, defaulting to
/// `application/octet-stream`. Mirrors the TS `CONTENT_TYPES` map.
fn content_type_for(path: &str) -> &'static str {
    let ext = path
        .rsplit('.')
        .next()
        .map(str::to_ascii_lowercase)
        .unwrap_or_default();
    match ext.as_str() {
        "js" | "mjs" => "text/javascript; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "html" => "text/html; charset=utf-8",
        "json" | "map" => "application/json; charset=utf-8",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "wasm" => "application/wasm",
        _ => "application/octet-stream",
    }
}

/// Hand-rolled SHA-256 (FIPS 180-4). Used only to hash the two passwords to a fixed
/// 32-byte digest so [`constant_time_eq`] compares equal-length inputs with no length
/// leak — mirroring the TS `timingSafeEqual(sha256(a), sha256(b))`. Hand-rolled to
/// avoid a crypto dependency, matching the repo's hand-rolled base64 ethos.
fn sha256(input: &[u8]) -> [u8; 32] {
    #[rustfmt::skip]
    const K: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5,
        0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174,
        0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da,
        0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7, 0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967,
        0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85,
        0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070,
        0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
        0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7, 0xc67178f2,
    ];
    let mut h: [u32; 8] = [
        0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
        0x5be0cd19,
    ];

    let mut msg = input.to_vec();
    let bit_len = (input.len() as u64).wrapping_mul(8);
    msg.push(0x80);
    while msg.len() % 64 != 56 {
        msg.push(0);
    }
    msg.extend_from_slice(&bit_len.to_be_bytes());

    for chunk in msg.chunks_exact(64) {
        let mut w = [0u32; 64];
        for (i, word) in w.iter_mut().take(16).enumerate() {
            *word = u32::from_be_bytes([
                chunk[4 * i],
                chunk[4 * i + 1],
                chunk[4 * i + 2],
                chunk[4 * i + 3],
            ]);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16]
                .wrapping_add(s0)
                .wrapping_add(w[i - 7])
                .wrapping_add(s1);
        }

        let (mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut hh) =
            (h[0], h[1], h[2], h[3], h[4], h[5], h[6], h[7]);
        for i in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ ((!e) & g);
            let temp1 = hh
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(K[i])
                .wrapping_add(w[i]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let temp2 = s0.wrapping_add(maj);
            hh = g;
            g = f;
            f = e;
            e = d.wrapping_add(temp1);
            d = c;
            c = b;
            b = a;
            a = temp1.wrapping_add(temp2);
        }

        h[0] = h[0].wrapping_add(a);
        h[1] = h[1].wrapping_add(b);
        h[2] = h[2].wrapping_add(c);
        h[3] = h[3].wrapping_add(d);
        h[4] = h[4].wrapping_add(e);
        h[5] = h[5].wrapping_add(f);
        h[6] = h[6].wrapping_add(g);
        h[7] = h[7].wrapping_add(hh);
    }

    let mut out = [0u8; 32];
    for (i, word) in h.iter().enumerate() {
        out[4 * i..4 * i + 4].copy_from_slice(&word.to_be_bytes());
    }
    out
}

/// Constant-time equality over equal-length byte slices (both callers pass 32-byte
/// SHA-256 digests, so no length is ever leaked). Returns false on a length mismatch.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

/// Constant-time password comparison (equal-length digests, so no length leak).
/// Mirrors the TS `passwordMatches`.
fn password_matches(expected: &str, provided: &str) -> bool {
    constant_time_eq(&sha256(expected.as_bytes()), &sha256(provided.as_bytes()))
}

/// Decode standard base64 (RFC 4648), tolerant of missing padding and embedded
/// CR/LF, returning `None` on any invalid character. Hand-rolled to match the
/// hand-rolled base64 encoder elsewhere in the crate; used only to read the HTTP
/// Basic credential.
fn base64_decode(input: &str) -> Option<Vec<u8>> {
    fn sextet(byte: u8) -> Option<u32> {
        match byte {
            b'A'..=b'Z' => Some((byte - b'A') as u32),
            b'a'..=b'z' => Some((byte - b'a' + 26) as u32),
            b'0'..=b'9' => Some((byte - b'0' + 52) as u32),
            b'+' => Some(62),
            b'/' => Some(63),
            _ => None,
        }
    }
    let mut out = Vec::with_capacity(input.len() * 3 / 4);
    let mut acc = 0u32;
    let mut bits = 0u32;
    for byte in input.bytes() {
        match byte {
            b'\r' | b'\n' => continue,
            b'=' => break,
            _ => {
                acc = (acc << 6) | sextet(byte)?;
                bits += 6;
                if bits >= 8 {
                    bits -= 8;
                    out.push((acc >> bits) as u8);
                }
            }
        }
    }
    Some(out)
}

/// Extract the username and password from an `Authorization: Basic` header, or `None`
/// when it is absent or malformed. A credential with no colon is tolerated: the whole
/// value is the password and the username is empty. Mirrors the TS `basicCredentials`.
fn basic_credentials(headers: &HeaderMap) -> Option<(String, String)> {
    let header = headers.get(header::AUTHORIZATION)?.to_str().ok()?;
    let (scheme, encoded) = header.split_once(' ')?;
    if !scheme.eq_ignore_ascii_case("basic") {
        return None;
    }
    let decoded = String::from_utf8(base64_decode(encoded)?).ok()?;
    Some(match decoded.find(':') {
        Some(colon) => (
            decoded[..colon].to_string(),
            decoded[colon + 1..].to_string(),
        ),
        None => (String::new(), decoded),
    })
}

/// True only when the request presents the correct Viewer password via HTTP Basic —
/// and, when a Viewer username is configured (`QMP_MCP_VIEWER_USER`), the correct
/// username too. Both halves are compared in constant time; the username check is a
/// no-op when `user` is `None`, preserving the password-only default. Both comparisons
/// are always evaluated so timing does not reveal which half was wrong. Mirrors the TS
/// `isAuthenticated`.
fn is_authenticated(headers: &HeaderMap, password: &str, user: Option<&str>) -> bool {
    let Some((provided_user, provided_password)) = basic_credentials(headers) else {
        return false;
    };
    let password_ok = password_matches(password, &provided_password);
    let user_ok = match user {
        Some(u) => password_matches(u, &provided_user),
        None => true,
    };
    password_ok && user_ok
}

/// The authority (`host[:port]`) of an `Origin`, or `None` for an opaque/`null`/
/// malformed value. Mirrors `new URL(origin).host`.
fn origin_authority(origin: &str) -> Option<String> {
    let after_scheme = origin.split_once("://")?.1;
    let authority = after_scheme.split(['/', '?', '#']).next()?;
    if authority.is_empty() {
        None
    } else {
        Some(authority.to_string())
    }
}

/// Same-origin guard for the websocket upgrade (F2). A non-browser client sends no
/// `Origin` and is allowed; when present, the `Origin`'s authority must equal the
/// request `Host` (so the page was served by this same Viewer). Mirrors the TS
/// `originAllowed`.
fn origin_allowed(headers: &HeaderMap) -> bool {
    let Some(origin) = headers.get(header::ORIGIN) else {
        // Omitted Origin ⇒ non-browser client; allow.
        return true;
    };
    let Ok(origin) = origin.to_str() else {
        return false;
    };
    let Some(host) = headers.get(header::HOST).and_then(|h| h.to_str().ok()) else {
        return false;
    };
    match origin_authority(origin) {
        Some(authority) => authority == host,
        None => false,
    }
}

/// Build the (post-auth) noVNC page with the server-generated VNC password embedded.
/// `serde_json` safely encodes the (server-generated, alphanumeric) secret into the
/// script's string context. Mirrors the TS `renderPage`.
fn render_page(vnc_password: &str) -> String {
    let password = serde_json::to_string(vnc_password).unwrap_or_else(|_| "\"\"".to_string());
    PAGE_TEMPLATE.replace("__VNC_PASSWORD_JSON__", &password)
}

// ---------------------------------------------------------------------------
// The axum server: static serving, Basic auth, anti-framing, ws relay
// ---------------------------------------------------------------------------

/// Shared, immutable per-Viewer state, cheap to clone (every field is `Arc` or a
/// small value) so each handler and the auth middleware get their own handle.
#[derive(Clone)]
struct ViewerState {
    /// The human-facing gate; the page and the websocket are refused without it.
    password: Arc<String>,
    /// Optional username enforced alongside the password (`QMP_MCP_VIEWER_USER`);
    /// `None` means the username is ignored (password-only).
    user: Arc<Option<String>>,
    /// The server-generated VNC password embedded into the post-auth page.
    vnc_password: Arc<String>,
    /// Loopback host the ws relay always dials (server-fixed; never client-supplied).
    vnc_host: Arc<String>,
    /// Loopback TCP port the ws relay always dials.
    vnc_port: u16,
    /// Bounds concurrent authenticated relays (F5); the N+1th upgrade gets 503.
    connections: Arc<Semaphore>,
}

/// axum middleware: gate EVERY request (page and websocket alike) on HTTP Basic with
/// the Viewer password (F, fail-closed). An unauthenticated request gets a 401 that
/// prompts the browser; the anti-framing layer still decorates that 401.
async fn require_basic_auth(
    State(state): State<ViewerState>,
    request: Request,
    next: Next,
) -> Response {
    if is_authenticated(request.headers(), &state.password, state.user.as_deref()) {
        next.run(request).await
    } else {
        unauthorized()
    }
}

/// axum middleware: set the anti-clickjacking headers on EVERY Viewer response (F2),
/// including the 401 from [`require_basic_auth`]. Both headers say the same thing to
/// old and new browsers: this page may not be framed. Mirrors `setAntiFramingHeaders`.
async fn add_anti_framing_headers(request: Request, next: Next) -> Response {
    let mut response = next.run(request).await;
    let headers = response.headers_mut();
    headers.insert(header::X_FRAME_OPTIONS, HeaderValue::from_static("DENY"));
    headers.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static("frame-ancestors 'none'"),
    );
    response
}

/// A 401 that prompts the browser for the Viewer password. Mirrors `sendUnauthorized`.
fn unauthorized() -> Response {
    let challenge = format!("Basic realm=\"{REALM}\"");
    (
        StatusCode::UNAUTHORIZED,
        [
            (
                header::WWW_AUTHENTICATE,
                HeaderValue::from_str(&challenge)
                    .unwrap_or_else(|_| HeaderValue::from_static("Basic")),
            ),
            (
                header::CONTENT_TYPE,
                HeaderValue::from_static("text/plain; charset=utf-8"),
            ),
        ],
        "Authentication required.\n",
    )
        .into_response()
}

/// A plain 404 for an unknown path (still post-auth, and still anti-framed).
fn not_found() -> Response {
    (
        StatusCode::NOT_FOUND,
        [(
            header::CONTENT_TYPE,
            HeaderValue::from_static("text/plain; charset=utf-8"),
        )],
        "Not found.\n",
    )
        .into_response()
}

/// Serve the (post-auth) noVNC page with the server-generated VNC password embedded.
async fn serve_page(State(state): State<ViewerState>) -> Response {
    (
        [
            (
                header::CONTENT_TYPE,
                HeaderValue::from_static("text/html; charset=utf-8"),
            ),
            (header::CACHE_CONTROL, HeaderValue::from_static("no-store")),
        ],
        render_page(&state.vnc_password),
    )
        .into_response()
}

/// Serve an embedded noVNC asset. Confined to the compiled-in [`ASSETS`] tree by
/// construction: a request that does not name an embedded file (including any `..`
/// or absolute path, which never matches an embedded key) is a plain 404.
async fn serve_asset(Path(rel): Path<String>) -> Response {
    match ASSETS.get_file(&rel) {
        Some(file) => (
            [
                (
                    header::CONTENT_TYPE,
                    HeaderValue::from_static(content_type_for(&rel)),
                ),
                (header::CACHE_CONTROL, HeaderValue::from_static("no-store")),
            ],
            file.contents().to_vec(),
        )
            .into_response(),
        None => not_found(),
    }
}

/// The `/websockify` upgrade. Auth is already enforced by the middleware; here the
/// upgrade must ALSO be same-origin (F2) and within the concurrent-connection cap
/// (F5), else it is refused. The relay target is the server-fixed loopback VNC port —
/// never the client's request (no SSRF). Mirrors the TS `httpServer.on('upgrade')`.
async fn websockify(
    State(state): State<ViewerState>,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> Response {
    // Same-origin only for browser callers (F2): a cross-origin page must not drive an
    // authenticated upgrade off a cached/leaked credential.
    if !origin_allowed(&headers) {
        return (
            StatusCode::FORBIDDEN,
            [(
                header::CONTENT_TYPE,
                HeaderValue::from_static("text/plain; charset=utf-8"),
            )],
            "Forbidden.\n",
        )
            .into_response();
    }
    // Cap concurrent authenticated relays (F5): refuse the N+1th with 503. The permit
    // is held for the connection's whole life and released when the relay task ends.
    let permit = match Arc::clone(&state.connections).try_acquire_owned() {
        Ok(permit) => permit,
        Err(_) => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                [(
                    header::CONTENT_TYPE,
                    HeaderValue::from_static("text/plain; charset=utf-8"),
                )],
                "Too many viewer connections.\n",
            )
                .into_response();
        }
    };

    let vnc_host = Arc::clone(&state.vnc_host);
    let vnc_port = state.vnc_port;
    // Bound any single inbound frame so one giant message cannot pin unbounded memory (F5).
    ws.max_message_size(MAX_WS_PAYLOAD_BYTES)
        .max_frame_size(MAX_WS_PAYLOAD_BYTES)
        .on_upgrade(move |socket| async move {
            let _permit = permit; // held for the relay's lifetime, then released
            proxy_to_vnc(socket, &vnc_host, vnc_port).await;
        })
}

/// Relay an authenticated websocket to the loopback VNC port. The target is
/// server-controlled (never the client's request) and bytes are relayed verbatim in
/// both directions. Backpressure is honoured by awaiting each write/send, so a slow
/// reader on either side throttles the other rather than growing an unbounded buffer.
/// Mirrors the TS `proxyToVnc`.
async fn proxy_to_vnc(socket: WebSocket, vnc_host: &str, vnc_port: u16) {
    let tcp = match TcpStream::connect((vnc_host, vnc_port)).await {
        Ok(tcp) => tcp,
        Err(err) => {
            tracing::warn!(
                "Viewer could not reach the VNC Display at {vnc_host}:{vnc_port}: {err}"
            );
            return;
        }
    };
    let (mut tcp_read, mut tcp_write) = tcp.into_split();
    let (mut ws_sink, mut ws_stream) = socket.split();

    // browser -> VNC. Awaiting the TCP write applies backpressure to the ws stream.
    let client_to_vnc = async move {
        while let Some(Ok(message)) = ws_stream.next().await {
            match message {
                Message::Binary(data) => {
                    if tcp_write.write_all(&data).await.is_err() {
                        break;
                    }
                }
                Message::Text(text) => {
                    if tcp_write.write_all(text.as_bytes()).await.is_err() {
                        break;
                    }
                }
                Message::Close(_) => break,
                // Ping/Pong are handled by axum's ws layer; nothing to relay.
                Message::Ping(_) | Message::Pong(_) => {}
            }
        }
        let _ = tcp_write.shutdown().await;
    };

    // VNC -> browser. Awaiting the ws send applies backpressure to the VNC reads.
    let vnc_to_client = async move {
        let mut buf = vec![0u8; VNC_READ_CHUNK];
        loop {
            match tcp_read.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if ws_sink
                        .send(Message::Binary(Bytes::copy_from_slice(&buf[..n])))
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
            }
        }
        let _ = ws_sink.close().await;
    };

    // Whichever direction ends first tears the relay down; dropping the other future
    // drops its half of the split socket, closing the connection.
    tokio::select! {
        _ = client_to_vnc => {}
        _ = vnc_to_client => {}
    }
}

/// Assemble the Viewer's axum application. The auth layer is inner (closest to the
/// handlers) so it gates every route including the fallback; the anti-framing layer
/// is outer so it decorates every response, including the auth 401.
fn build_router(state: ViewerState) -> Router {
    Router::new()
        .route("/", get(serve_page))
        .route("/index.html", get(serve_page))
        .route(WEBSOCKET_PATH, get(websockify))
        // The embedded assets are served beneath ASSET_PREFIX (e.g. /novnc/core/rfb.js).
        .route(&format!("{ASSET_PREFIX}{{*path}}"), get(serve_asset))
        .fallback(|| async { not_found() })
        .layer(middleware::from_fn_with_state(
            state.clone(),
            require_basic_auth,
        ))
        .layer(middleware::from_fn(add_anti_framing_headers))
        .with_state(state)
}

/// Start the Viewer's HTTP server and return a handle once it is listening.
/// Fail-closed: with no password it refuses to serve. The websocket upgrade is gated
/// by the same password as the page, and every authenticated upgrade is relayed to
/// the server-controlled loopback VNC port. Mirrors the TS `startViewer`.
pub async fn start_viewer(options: ViewerOptions) -> Result<RunningViewer, ViewerError> {
    let ViewerOptions {
        host,
        port,
        password,
        user,
        vnc_host,
        vnc_port,
        vnc_password,
    } = options;

    if password.is_empty() {
        return Err(ViewerError(
            "The Viewer requires a password (QMP_MCP_VIEWER_PASSWORD) but none was configured; \
             refusing to serve."
                .to_string(),
        ));
    }

    let addr = format!("{host}:{port}");
    let listener = TcpListener::bind(&addr).await.map_err(|err| {
        ViewerError(format!(
            "failed to bind the noVNC Viewer to {addr}: {err}. Is the port already in use, or are \
             QMP_MCP_VIEWER_HOST/QMP_MCP_VIEWER_PORT valid?"
        ))
    })?;
    let bound_port = listener.local_addr().map(|a| a.port()).unwrap_or(port);

    tracing::info!(
        "Viewer listening on http://{host}:{bound_port} \
         (noVNC over the loopback VNC Display at {vnc_host}:{vnc_port})"
    );
    // Cleartext-exposure warning (F1): a non-loopback bind serves the HTTP Basic
    // password and the interactive VNC session in the clear. Do not refuse (the
    // container legitimately binds 0.0.0.0) — warn loudly so an operator fronts it.
    if !is_loopback_host(&host) {
        tracing::warn!(
            "Viewer is serving cleartext HTTP Basic + interactive VNC on {host}:{bound_port}; \
             put it behind a TLS-terminating reverse proxy on untrusted networks."
        );
    }

    let state = ViewerState {
        password: Arc::new(password),
        user: Arc::new(user),
        vnc_password: Arc::new(vnc_password),
        vnc_host: Arc::new(vnc_host),
        vnc_port,
        connections: Arc::new(Semaphore::new(MAX_VIEWER_CONNECTIONS)),
    };
    let app = build_router(state);
    let task = tokio::spawn(async move {
        if let Err(err) = axum::serve(listener, app).await {
            tracing::warn!("Viewer HTTP server error: {err}");
        }
    });

    Ok(RunningViewer {
        host,
        port: bound_port,
        task: Mutex::new(Some(task)),
    })
}

/// A running Viewer: the HTTP server task plus the address it bound to. Stopping it
/// aborts the task — dropping the listener and every in-flight websocket relay at
/// once — which is the prompt equivalent of the TS `stopServer` (terminate clients,
/// close server).
#[derive(Debug)]
pub struct RunningViewer {
    host: String,
    port: u16,
    task: Mutex<Option<JoinHandle<()>>>,
}

impl RunningViewer {
    /// The address the HTTP server bound to.
    pub fn host(&self) -> &str {
        &self.host
    }

    /// The port the HTTP server actually bound to (useful when `port` was 0).
    pub fn port(&self) -> u16 {
        self.port
    }
}

#[async_trait]
impl ViewerHandle for RunningViewer {
    async fn stop(&self) {
        if let Some(task) = self.task.lock().await.take() {
            task.abort();
            let _ = task.await;
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    // `super::*` already brings `AsyncReadExt`/`AsyncWriteExt` (used by the raw-TCP
    // test client's `read`/`write_all`) and `TcpStream` into scope.
    use super::*;
    use std::time::Duration;

    // --- pure helpers -------------------------------------------------------

    /// SHA-256 against the FIPS 180-4 known-answer vectors, proving the hand-rolled
    /// digest is correct (the constant-time compare relies on it).
    #[test]
    fn sha256_matches_known_vectors() {
        let hex = |b: &[u8]| b.iter().map(|x| format!("{x:02x}")).collect::<String>();
        assert_eq!(
            hex(&sha256(b"")),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            hex(&sha256(b"abc")),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(
            hex(&sha256(
                b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq"
            )),
            "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1"
        );
    }

    #[test]
    fn base64_decode_round_trips_and_rejects_garbage() {
        assert_eq!(base64_decode("dXNlcjpwYXNz").unwrap(), b"user:pass");
        // Missing padding is tolerated.
        assert_eq!(base64_decode("cGFzcw").unwrap(), b"pass");
        assert_eq!(base64_decode("cGFzcw==").unwrap(), b"pass");
        // An invalid character fails closed.
        assert!(base64_decode("not base64!").is_none());
    }

    #[test]
    fn password_matches_only_the_exact_secret() {
        assert!(password_matches("s3cret", "s3cret"));
        assert!(!password_matches("s3cret", "s3creT"));
        assert!(!password_matches("s3cret", ""));
        assert!(!password_matches("s3cret", "s3cret2"));
    }

    /// Build a header map from `(name, value)` pairs.
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

    /// Encode `user:pass` the way a browser forms the HTTP Basic header value.
    fn basic(user: &str, pass: &str) -> String {
        const TABLE: &[u8; 64] =
            b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let raw = format!("{user}:{pass}");
        let bytes = raw.as_bytes();
        let mut out = String::new();
        for chunk in bytes.chunks(3) {
            let b0 = chunk[0] as u32;
            let b1 = *chunk.get(1).unwrap_or(&0) as u32;
            let b2 = *chunk.get(2).unwrap_or(&0) as u32;
            let n = (b0 << 16) | (b1 << 8) | b2;
            out.push(TABLE[((n >> 18) & 0x3f) as usize] as char);
            out.push(TABLE[((n >> 12) & 0x3f) as usize] as char);
            out.push(if chunk.len() > 1 {
                TABLE[((n >> 6) & 0x3f) as usize] as char
            } else {
                '='
            });
            out.push(if chunk.len() > 2 {
                TABLE[(n & 0x3f) as usize] as char
            } else {
                '='
            });
        }
        format!("Basic {out}")
    }

    #[test]
    fn is_authenticated_gates_on_the_password_only_when_no_user_configured() {
        // Correct password with ANY username authenticates (user = None).
        assert!(is_authenticated(
            &headers(&[("authorization", &basic("anyone", "hunter2"))]),
            "hunter2",
            None
        ));
        // Wrong password, missing header, and non-Basic scheme all fail closed.
        assert!(!is_authenticated(
            &headers(&[("authorization", &basic("anyone", "nope"))]),
            "hunter2",
            None
        ));
        assert!(!is_authenticated(&headers(&[]), "hunter2", None));
        assert!(!is_authenticated(
            &headers(&[("authorization", "Bearer hunter2")]),
            "hunter2",
            None
        ));
    }

    #[test]
    fn is_authenticated_enforces_the_configured_username() {
        // With a configured username, both halves must match.
        assert!(is_authenticated(
            &headers(&[("authorization", &basic("operator", "hunter2"))]),
            "hunter2",
            Some("operator")
        ));
        // Right password, wrong username -> refused.
        assert!(!is_authenticated(
            &headers(&[("authorization", &basic("intruder", "hunter2"))]),
            "hunter2",
            Some("operator")
        ));
        // Right username, wrong password -> refused.
        assert!(!is_authenticated(
            &headers(&[("authorization", &basic("operator", "nope"))]),
            "hunter2",
            Some("operator")
        ));
    }

    #[test]
    fn origin_allowed_matches_ts_posture() {
        // No Origin (non-browser client) is allowed.
        assert!(origin_allowed(&headers(&[("host", "127.0.0.1:6080")])));
        // Same-origin is allowed.
        assert!(origin_allowed(&headers(&[
            ("host", "127.0.0.1:6080"),
            ("origin", "http://127.0.0.1:6080"),
        ])));
        // Cross-origin is refused.
        assert!(!origin_allowed(&headers(&[
            ("host", "127.0.0.1:6080"),
            ("origin", "http://evil.example"),
        ])));
        // A "null"/opaque Origin is not same-origin; refuse.
        assert!(!origin_allowed(&headers(&[
            ("host", "127.0.0.1:6080"),
            ("origin", "null"),
        ])));
    }

    #[test]
    fn loopback_hosts_are_recognised() {
        for h in ["127.0.0.1", "localhost", "::1", "[::1]", " LocalHost "] {
            assert!(is_loopback_host(h), "{h} should be loopback");
        }
        for h in ["0.0.0.0", "10.0.0.5", "example.com"] {
            assert!(!is_loopback_host(h), "{h} should not be loopback");
        }
    }

    #[test]
    fn content_types_cover_the_novnc_assets() {
        assert_eq!(
            content_type_for("core/rfb.js"),
            "text/javascript; charset=utf-8"
        );
        assert_eq!(content_type_for("x.css"), "text/css; charset=utf-8");
        assert_eq!(content_type_for("x.wasm"), "application/wasm");
        assert_eq!(content_type_for("noext"), "application/octet-stream");
    }

    #[test]
    fn embedded_assets_include_the_served_entrypoint_and_license() {
        // The page imports this; it must be embedded (proves the vendored assets ship).
        assert!(ASSETS.get_file("core/rfb.js").is_some());
        assert!(ASSETS.get_file("LICENSE.txt").is_some());
        // A traversal attempt never matches an embedded key.
        assert!(ASSETS.get_file("../Cargo.toml").is_none());
    }

    #[test]
    fn render_page_embeds_password_and_wiring() {
        let page = render_page("Ab3Kp9Qz");
        // The server-generated VNC password is embedded for auto-auth.
        assert!(page.contains("\"Ab3Kp9Qz\""));
        // The asset entrypoint and websocket path match the module constants.
        assert!(page.contains(&format!("{ASSET_PREFIX}core/rfb.js")));
        assert!(page.contains(WEBSOCKET_PATH));
    }

    // --- integration: boot the Viewer on an ephemeral port ------------------

    /// A minimal HTTP/1.1 client over a raw TCP socket: send a request, return the
    /// full response text. Avoids pulling an HTTP client crate into the tests.
    async fn http_request(port: u16, request: &str) -> String {
        let mut stream = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        stream.write_all(request.as_bytes()).await.unwrap();
        stream.flush().await.unwrap();
        let mut buf = Vec::new();
        // Read until the server finishes the (short) response and closes, or a small
        // idle timeout elapses — enough for these tiny bodies.
        let _ = tokio::time::timeout(Duration::from_secs(2), stream.read_to_end(&mut buf)).await;
        String::from_utf8_lossy(&buf).into_owned()
    }

    /// The Basic header a browser would send for the given Viewer password.
    fn auth_header(password: &str) -> String {
        basic("viewer", password)
    }

    /// Start a Viewer with a fake loopback VNC target, returning the viewer, its bound
    /// port, and the fake VNC listener's port.
    async fn start_test_viewer() -> (RunningViewer, u16, u16) {
        start_test_viewer_with(None).await
    }

    /// Like [`start_test_viewer`] but with an optional enforced Viewer username
    /// (`QMP_MCP_VIEWER_USER`).
    async fn start_test_viewer_with(user: Option<&str>) -> (RunningViewer, u16, u16) {
        // A fake VNC server that accepts and then just holds each connection open, so
        // an upgraded relay keeps its connection-cap permit for the test's duration.
        let vnc = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let vnc_port = vnc.local_addr().unwrap().port();
        tokio::spawn(async move {
            let mut held = Vec::new();
            while let Ok((sock, _)) = vnc.accept().await {
                held.push(sock); // hold it open so the relay's permit stays acquired
            }
        });

        let viewer = start_viewer(ViewerOptions {
            host: "127.0.0.1".to_string(),
            port: 0,
            password: "hunter2".to_string(),
            user: user.map(str::to_string),
            vnc_host: "127.0.0.1".to_string(),
            vnc_port,
            vnc_password: "Ab3Kp9Qz".to_string(),
        })
        .await
        .unwrap();
        let port = viewer.port();
        (viewer, port, vnc_port)
    }

    #[tokio::test]
    async fn enforces_the_configured_viewer_username_over_http() {
        let (viewer, port, _vnc) = start_test_viewer_with(Some("operator")).await;
        let req = |auth: &str| {
            format!("GET / HTTP/1.1\r\nHost: 127.0.0.1\r\nAuthorization: {auth}\r\nConnection: close\r\n\r\n")
        };
        // Right username + password → 200.
        let ok = http_request(port, &req(&basic("operator", "hunter2"))).await;
        assert!(ok.starts_with("HTTP/1.1 200"), "got: {ok}");
        // Wrong username (even with the right password) → 401.
        let bad_user = http_request(port, &req(&basic("intruder", "hunter2"))).await;
        assert!(bad_user.starts_with("HTTP/1.1 401"), "got: {bad_user}");
        // Right username, wrong password → 401.
        let bad_pw = http_request(port, &req(&basic("operator", "nope"))).await;
        assert!(bad_pw.starts_with("HTTP/1.1 401"), "got: {bad_pw}");
        viewer.stop().await;
    }

    #[tokio::test]
    async fn fails_closed_without_a_password() {
        let err = start_viewer(ViewerOptions {
            host: "127.0.0.1".to_string(),
            port: 0,
            password: String::new(),
            user: None,
            vnc_host: "127.0.0.1".to_string(),
            vnc_port: 5900,
            vnc_password: "Ab3Kp9Qz".to_string(),
        })
        .await
        .unwrap_err();
        assert!(err.0.contains("QMP_MCP_VIEWER_PASSWORD"), "got: {}", err.0);
        assert!(err.0.contains("refusing to serve"), "got: {}", err.0);
    }

    #[tokio::test]
    async fn page_requires_auth_and_carries_anti_framing() {
        let (viewer, port, _vnc) = start_test_viewer().await;

        // No credentials → 401, with the anti-framing headers even on the 401 (F2).
        let resp = http_request(
            port,
            "GET / HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n",
        )
        .await;
        assert!(resp.starts_with("HTTP/1.1 401"), "got: {resp}");
        // hyper serialises header names in lowercase on the wire; the value is preserved.
        assert!(resp.contains("www-authenticate: Basic realm=\"qmp-mcp viewer\""));
        assert!(resp.contains("x-frame-options: DENY"));
        assert!(resp.contains("content-security-policy: frame-ancestors 'none'"));

        // With credentials → 200 and the noVNC page with the embedded VNC password.
        let resp = http_request(
            port,
            &format!(
                "GET / HTTP/1.1\r\nHost: 127.0.0.1\r\nAuthorization: {}\r\nConnection: close\r\n\r\n",
                auth_header("hunter2")
            ),
        )
        .await;
        assert!(resp.starts_with("HTTP/1.1 200"), "got: {resp}");
        assert!(resp.contains("/novnc/core/rfb.js"));
        assert!(resp.contains("\"Ab3Kp9Qz\""));
        assert!(resp.contains("x-frame-options: DENY"));

        viewer.stop().await;
    }

    #[tokio::test]
    async fn serves_an_embedded_asset_only_with_auth() {
        let (viewer, port, _vnc) = start_test_viewer().await;

        // Unauthenticated asset request is refused (auth gates every path).
        let resp = http_request(
            port,
            "GET /novnc/core/rfb.js HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n",
        )
        .await;
        assert!(resp.starts_with("HTTP/1.1 401"), "got status line: {resp}");

        // Authenticated asset request serves the embedded JavaScript.
        let resp = http_request(
            port,
            &format!(
                "GET /novnc/core/rfb.js HTTP/1.1\r\nHost: 127.0.0.1\r\nAuthorization: {}\r\n\
                 Connection: close\r\n\r\n",
                auth_header("hunter2")
            ),
        )
        .await;
        assert!(resp.starts_with("HTTP/1.1 200"), "got: {resp}");
        assert!(resp.contains("text/javascript"));

        viewer.stop().await;
    }

    /// The `Sec-WebSocket-Key`/version headers that make a GET a valid ws upgrade.
    const WS_HEADERS: &str =
        "Upgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\
         Sec-WebSocket-Version: 13\r\n";

    #[tokio::test]
    async fn websocket_upgrade_enforces_auth_origin_and_path() {
        let (viewer, port, _vnc) = start_test_viewer().await;

        // Unauthenticated upgrade → 401 (same gate as the page).
        let resp = http_request(
            port,
            &format!("GET /websockify HTTP/1.1\r\nHost: 127.0.0.1\r\n{WS_HEADERS}Connection: close\r\n\r\n"),
        )
        .await;
        // (the ws headers set Connection: Upgrade; the 401 short-circuits before upgrade)
        assert!(resp.contains(" 401 "), "expected 401, got: {resp}");

        // Authenticated but cross-origin upgrade → 403 (F2).
        let resp = http_request(
            port,
            &format!(
                "GET /websockify HTTP/1.1\r\nHost: 127.0.0.1\r\nOrigin: http://evil.example\r\n\
                 Authorization: {}\r\n{WS_HEADERS}\r\n",
                auth_header("hunter2")
            ),
        )
        .await;
        assert!(resp.contains(" 403 "), "expected 403, got: {resp}");

        // Authenticated, same-origin, correct path → 101 Switching Protocols.
        let resp = http_request(
            port,
            &format!(
                "GET /websockify HTTP/1.1\r\nHost: 127.0.0.1\r\nOrigin: http://127.0.0.1\r\n\
                 Authorization: {}\r\n{WS_HEADERS}\r\n",
                auth_header("hunter2")
            ),
        )
        .await;
        assert!(resp.contains(" 101 "), "expected 101, got: {resp}");

        viewer.stop().await;
    }

    #[tokio::test]
    async fn connection_cap_refuses_beyond_the_limit() {
        let (viewer, port, _vnc) = start_test_viewer().await;
        let upgrade = format!(
            "GET /websockify HTTP/1.1\r\nHost: 127.0.0.1\r\nOrigin: http://127.0.0.1\r\n\
             Authorization: {}\r\n{WS_HEADERS}\r\n",
            auth_header("hunter2")
        );

        // Open MAX_VIEWER_CONNECTIONS upgrades and HOLD them (permits stay acquired
        // because the fake VNC keeps each relay's TCP connection open).
        let mut held = Vec::new();
        for _ in 0..MAX_VIEWER_CONNECTIONS {
            let mut sock = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
            sock.write_all(upgrade.as_bytes()).await.unwrap();
            sock.flush().await.unwrap();
            // Read the 101 status line so the upgrade has completed and the permit is held.
            let mut buf = [0u8; 32];
            let n = sock.read(&mut buf).await.unwrap();
            let line = String::from_utf8_lossy(&buf[..n]);
            assert!(line.contains(" 101 "), "expected 101, got: {line}");
            held.push(sock);
        }

        // The N+1th upgrade is refused with 503 (F5).
        let resp = http_request(port, &upgrade).await;
        assert!(resp.contains(" 503 "), "expected 503, got: {resp}");

        viewer.stop().await;
    }

    #[tokio::test]
    async fn stop_is_idempotent() {
        let (viewer, _port, _vnc) = start_test_viewer().await;
        viewer.stop().await;
        viewer.stop().await; // second stop is a no-op
    }
}
