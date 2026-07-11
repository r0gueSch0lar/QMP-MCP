//! Real-HTTP integration test for the streamable HTTP transport and its
//! fail-closed auth middleware (slice #25, ADR-0005/0011).
//!
//! It builds the production router (`qmp_mcp::http::build_router`) over a
//! fake-free `QmpMcpServer`, serves it on an ephemeral loopback port with the same
//! `axum::serve` the binary uses, and drives it over a real TCP socket with
//! hand-written HTTP/1.1 requests. No qemu is ever launched: the assertions are all
//! about the guard layer (`401` without a key or JWT, `403` for a rebinding Origin)
//! and the MCP handshake reaching the service (`200` for an authenticated
//! `initialize`), and none of those paths invoke a tool, so the wired
//! `RealQemuDriver` is never called. Both auth providers are exercised end to end:
//! the default `apikey` mode (`X-API-Key`) and `jwt` mode (`Authorization: Bearer`,
//! HS256).

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;

use qmp_mcp::config::{load_config, Config};
use qmp_mcp::http::build_router;
use qmp_mcp::instance::image_store::{ImageStore, ImageStoreOptions};
use qmp_mcp::instance::iso_store::IsoStore;
use qmp_mcp::instance::orchestrator::{Orchestrator, OrchestratorOptions};
use qmp_mcp::qemu::real_driver::RealQemuDriver;
use qmp_mcp::server::QmpMcpServer;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Mutex;

/// One accepted API key for the authenticated cases.
const API_KEY: &str = "s3cr3t-key";

/// A minimal but valid MCP `initialize` request body. `protocolVersion` is one rmcp
/// 0.16 accepts, so the streamable service deserializes it and answers `200`.
const INITIALIZE_BODY: &str = concat!(
    r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"#,
    r#""protocolVersion":"2025-06-18","capabilities":{},"#,
    r#""clientInfo":{"name":"itest","version":"0.0.0"}}}"#
);

/// Build the HTTP config used by the test: `http` transport with one API key. The
/// bind host/port are unused here (the test binds its own ephemeral listener), but
/// `allowed_origins` defaults to the loopback origins for port 8080, which the
/// Origin-guard cases rely on.
fn http_config() -> Config {
    let env: HashMap<String, String> = [
        ("QMP_MCP_TRANSPORT", "http"),
        ("QMP_MCP_API_KEYS", API_KEY),
        ("QMP_MCP_HTTP_HOST", "127.0.0.1"),
    ]
    .iter()
    .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
    .collect();
    load_config(&env).expect("valid http config with an API key")
}

/// A server wired to the real driver (never invoked by these auth-layer tests) and
/// stores pointing at non-existent directories (never touched either).
fn test_server() -> QmpMcpServer {
    let options = OrchestratorOptions {
        qemu_binary_override: Some("qemu-system-x86_64".to_string()),
        host_arch: "x86_64".to_string(),
        qmp_socket_path: "/run/qmp-mcp/qmp.sock".to_string(),
        image_dir: None,
        iso_dir: None,
        host_share_dir: None,
        guest_share_dir: None,
        share_readonly: None,
        serial_buffer_bytes: 1 << 20,
        allow_serial_write: false,
        serial_backend: qmp_mcp::config::SerialBackend::Ringbuf,
        serial_spool_dir: None,
        hostfwd_port_range: None,
        allow_host_net: false,
        auto_start: false,
        max_memory_mb: None,
        max_vcpus: None,
        allow_raw_args: false,
        command_policy: None,
        event_buffer_size: None,
        viewer_password: None,
        viewer_user: None,
        viewer_host: "127.0.0.1".to_string(),
        viewer_port: 6080,
        start_viewer: None,
        kvm_available: Box::new(|| false),
    };
    let orchestrator = Arc::new(Mutex::new(Orchestrator::new(
        Box::new(RealQemuDriver),
        options,
    )));
    let image_store = ImageStore::new(ImageStoreOptions {
        dir: "/nonexistent/qmp-mcp-image-store".to_string(),
        max_disk_gb: 64,
        qemu_img_binary: None,
        run: None,
    });
    let iso_store = IsoStore::new("/nonexistent/qmp-mcp-iso-store".to_string());
    QmpMcpServer::new(orchestrator, image_store, iso_store)
}

/// Serve the production router on an ephemeral loopback port, returning its address.
/// The server task runs for the duration of the test process.
async fn spawn_server(config: &Config) -> SocketAddr {
    let router = build_router(config, test_server());
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });
    addr
}

/// Send one raw HTTP/1.1 POST to `/mcp` with the given extra header lines and body,
/// and return the numeric status code from the response's status line.
async fn post_mcp(addr: SocketAddr, extra_headers: &[&str], body: &str) -> u16 {
    let mut request = format!(
        "POST /mcp HTTP/1.1\r\nHost: {addr}\r\n\
         Content-Type: application/json\r\n\
         Accept: application/json, text/event-stream\r\n\
         Content-Length: {}\r\nConnection: close\r\n",
        body.len()
    );
    for header in extra_headers {
        request.push_str(header);
        request.push_str("\r\n");
    }
    request.push_str("\r\n");
    request.push_str(body);

    let stream = TcpStream::connect(addr).await.unwrap();
    let (read_half, mut write_half) = stream.into_split();
    write_half.write_all(request.as_bytes()).await.unwrap();
    write_half.flush().await.unwrap();

    let mut status_line = String::new();
    BufReader::new(read_half)
        .read_line(&mut status_line)
        .await
        .unwrap();
    // e.g. "HTTP/1.1 401 Unauthorized\r\n"
    status_line
        .split_whitespace()
        .nth(1)
        .unwrap_or_else(|| panic!("no status code in {status_line:?}"))
        .parse()
        .unwrap()
}

/// A POST with no `X-API-Key` is rejected with `401` before the MCP service runs.
#[tokio::test]
async fn rejects_request_without_api_key() {
    let addr = spawn_server(&http_config()).await;
    let status = post_mcp(addr, &[], INITIALIZE_BODY).await;
    assert_eq!(status, 401, "missing key must be 401");
}

/// A POST with a wrong `X-API-Key` is likewise `401`.
#[tokio::test]
async fn rejects_request_with_invalid_api_key() {
    let addr = spawn_server(&http_config()).await;
    let status = post_mcp(addr, &["X-API-Key: wrong"], INITIALIZE_BODY).await;
    assert_eq!(status, 401, "invalid key must be 401");
}

/// A POST with a valid key and a well-formed `initialize` reaches the MCP service
/// and gets `200` — the auth layer let it through.
#[tokio::test]
async fn accepts_initialize_with_valid_api_key() {
    let addr = spawn_server(&http_config()).await;
    let status = post_mcp(addr, &[&format!("X-API-Key: {API_KEY}")], INITIALIZE_BODY).await;
    assert_eq!(status, 200, "valid key + initialize must be 200");
}

/// A browser request whose `Origin` is not allowlisted is refused with `403` by the
/// DNS-rebinding guard — even carrying a valid key — because the origin guard runs
/// ahead of auth.
#[tokio::test]
async fn rejects_disallowed_origin_even_with_valid_key() {
    let addr = spawn_server(&http_config()).await;
    let status = post_mcp(
        addr,
        &[
            &format!("X-API-Key: {API_KEY}"),
            "Origin: http://evil.example",
        ],
        INITIALIZE_BODY,
    )
    .await;
    assert_eq!(status, 403, "disallowed Origin must be 403");
}

/// Insecure mode serves the MCP handshake with no key at all (`200`), proving the
/// auth layer is omitted while the transport still works. The origin guard remains.
#[tokio::test]
async fn insecure_mode_serves_without_a_key() {
    let env: HashMap<String, String> = [
        ("QMP_MCP_TRANSPORT", "http"),
        ("QMP_MCP_ALLOW_INSECURE", "true"),
    ]
    .iter()
    .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
    .collect();
    let config = load_config(&env).expect("insecure http config");
    let addr = spawn_server(&config).await;
    let status = post_mcp(addr, &[], INITIALIZE_BODY).await;
    assert_eq!(status, 200, "insecure mode must serve without a key");
}

/// The HS256 signing secret for the JWT-mode cases.
const JWT_SECRET: &str = "itest-jwt-signing-secret";

/// Build the HTTP config for the JWT cases: `http` transport, `jwt` auth, a secret.
fn jwt_config() -> Config {
    let env: HashMap<String, String> = [
        ("QMP_MCP_TRANSPORT", "http"),
        ("QMP_MCP_AUTH", "jwt"),
        ("QMP_MCP_JWT_SECRET", JWT_SECRET),
        ("QMP_MCP_HTTP_HOST", "127.0.0.1"),
    ]
    .iter()
    .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
    .collect();
    load_config(&env).expect("valid http config with a JWT secret")
}

/// Mint an HS256 JWT signed with `JWT_SECRET`, expiring `ttl_secs` from now (pass a
/// negative value to mint an already-expired token). `Header::default()` is HS256.
fn jwt(ttl_secs: i64) -> String {
    use jsonwebtoken::{encode, EncodingKey, Header};
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    let claims = serde_json::json!({ "sub": "itest-agent", "exp": now + ttl_secs });
    encode(
        &Header::default(),
        &claims,
        &EncodingKey::from_secret(JWT_SECRET.as_bytes()),
    )
    .unwrap()
}

/// Under `jwt` auth, a POST with no `Authorization` header is rejected with `401`
/// before the MCP service runs.
#[tokio::test]
async fn jwt_rejects_request_without_token() {
    let addr = spawn_server(&jwt_config()).await;
    let status = post_mcp(addr, &[], INITIALIZE_BODY).await;
    assert_eq!(status, 401, "missing JWT must be 401");
}

/// Under `jwt` auth, a POST bearing a valid HS256 token reaches the MCP service and
/// gets `200` — the JWT layer let it through.
#[tokio::test]
async fn jwt_accepts_initialize_with_valid_token() {
    let addr = spawn_server(&jwt_config()).await;
    let auth = format!("Authorization: Bearer {}", jwt(3600));
    let status = post_mcp(addr, &[&auth], INITIALIZE_BODY).await;
    assert_eq!(status, 200, "valid JWT + initialize must be 200");
}

/// Under `jwt` auth, an expired token is rejected with `401`.
#[tokio::test]
async fn jwt_rejects_expired_token() {
    let addr = spawn_server(&jwt_config()).await;
    let auth = format!("Authorization: Bearer {}", jwt(-3600));
    let status = post_mcp(addr, &[&auth], INITIALIZE_BODY).await;
    assert_eq!(status, 401, "expired JWT must be 401");
}
