//! The real [`QemuDriver`]: it actually spawns a `qemu-system-*` child, waits for it
//! to create the QMP UNIX socket, dials it, completes the QMP handshake, and hands
//! back a [`RealInstanceHandle`] that owns the live QMP Session (ADR-0003/0011).
//! Teardown terminates the child (SIGTERM, then SIGKILL after a grace period) and
//! removes the socket file.
//!
//! A second implementation of the shared bounded context, mirroring
//! `../../src/qemu/real-driver.ts`: same dial/retry loop, the same fail-fast on an
//! early qemu exit, the same refuse-on-occupied-socket startup check, and the same
//! TERM-then-KILL teardown. The QMP socket path is server-managed and never
//! network-exposed (a UNIX socket the server owns).

use std::path::Path;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use serde_json::Value;
use tokio::io::AsyncReadExt;
use tokio::process::{Child, Command};
use tokio::sync::Mutex as AsyncMutex;
use tokio::time::sleep;

use super::driver::{DriverError, InstanceHandle, LaunchRequest, QemuDriver};
use super::qmp_client::QmpClient;

/// How long to wait for QEMU to create and accept on the QMP socket.
const SOCKET_DIAL_TIMEOUT: Duration = Duration::from_millis(10_000);
/// Delay between connection attempts while QEMU is still starting up.
const SOCKET_DIAL_INTERVAL: Duration = Duration::from_millis(50);
/// Grace period after SIGTERM before escalating to SIGKILL during teardown.
const TERMINATE_GRACE: Duration = Duration::from_millis(5_000);
/// How much child stderr to retain for diagnostics on a launch failure.
const STDERR_CAP: usize = 4_000;

/// The production driver. Spawns real `qemu-system-*` processes; unit tests use the
/// in-memory `FakeQemuDriver` instead, and the single real-qemu integration test
/// (`tests/real_qemu_tcg.rs`) exercises this driver under TCG.
#[derive(Debug, Default, Clone, Copy)]
pub struct RealQemuDriver;

#[async_trait]
impl QemuDriver for RealQemuDriver {
    async fn launch(&self, request: LaunchRequest) -> Result<Box<dyn InstanceHandle>, DriverError> {
        let LaunchRequest {
            binary,
            argv,
            qmp_socket_path,
        } = request;

        // QEMU creates the socket; its parent directory must already exist.
        if let Some(parent) = Path::new(&qmp_socket_path).parent() {
            tokio::fs::create_dir_all(parent).await.map_err(|e| {
                DriverError(format!(
                    "Failed to create the QMP socket directory {}: {e}",
                    parent.display()
                ))
            })?;
        }

        // Refuse-on-startup: never clobber or adopt a socket we did not create. A
        // leftover path means either a stale socket from a crashed qemu or a live
        // process the server does not own. `symlink_metadata` does not follow links,
        // so a dangling symlink is caught too.
        if tokio::fs::symlink_metadata(&qmp_socket_path).await.is_ok() {
            return Err(DriverError(format!(
                "The QMP socket path {qmp_socket_path} is already occupied — refusing to start \
                 rather than clobber or adopt a process this server did not launch. Remove the \
                 stale socket (or stop the other process), then retry."
            )));
        }

        tracing::debug!("spawning {binary} {}", argv.join(" "));
        // stdout is unused; discard it so an undrained pipe can never backpressure the
        // child. stderr is piped and drained below for launch-failure diagnostics.
        let mut child = Command::new(&binary)
            .args(&argv)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| DriverError(format!("Failed to spawn {binary}: {e}")))?;

        let stderr_buf = Arc::new(Mutex::new(String::new()));
        if let Some(mut stderr) = child.stderr.take() {
            let buf = Arc::clone(&stderr_buf);
            tokio::spawn(async move {
                let mut chunk = [0u8; 1024];
                loop {
                    match stderr.read(&mut chunk).await {
                        Ok(0) | Err(_) => break,
                        Ok(n) => append_capped(&buf, &String::from_utf8_lossy(&chunk[..n])),
                    }
                }
            });
        }

        // Dial the socket, then negotiate. Any failure tears down the child and the
        // socket so we never leak a half-started qemu or a stale socket file.
        let client = match dial(&qmp_socket_path, &mut child, &stderr_buf).await {
            Ok(client) => client,
            Err(err) => {
                terminate(&mut child).await;
                let _ = tokio::fs::remove_file(&qmp_socket_path).await;
                return Err(err);
            }
        };
        if let Err(err) = client.negotiate().await {
            client.close().await;
            terminate(&mut child).await;
            let _ = tokio::fs::remove_file(&qmp_socket_path).await;
            return Err(DriverError(format!(
                "Failed to negotiate the QMP session with {binary}: {err}"
            )));
        }

        tracing::info!("QMP session established for {binary}");
        Ok(Box::new(RealInstanceHandle::new(
            child,
            client,
            qmp_socket_path,
        )))
    }
}

/// Retry-connect to the QMP socket until it accepts, QEMU exits, or the timeout
/// elapses. Fails fast (with the captured stderr) when qemu exits before the socket
/// is ready — the unexpected-early-exit reconciliation. Mirrors the TS `#dial`.
async fn dial(
    socket_path: &str,
    child: &mut Child,
    stderr: &Arc<Mutex<String>>,
) -> Result<QmpClient, DriverError> {
    let deadline = Instant::now() + SOCKET_DIAL_TIMEOUT;
    loop {
        if let Ok(Some(status)) = child.try_wait() {
            return Err(DriverError(format!(
                "QEMU exited before the QMP socket was ready ({}). Stderr: {}",
                describe_exit(status),
                stderr_or_empty(stderr)
            )));
        }
        match QmpClient::dial(socket_path).await {
            Ok(client) => return Ok(client),
            Err(err) => {
                if Instant::now() >= deadline {
                    return Err(DriverError(format!(
                        "Timed out after {}ms connecting to the QMP socket at {socket_path}: {err}. \
                         Stderr: {}",
                        SOCKET_DIAL_TIMEOUT.as_millis(),
                        stderr_or_empty(stderr)
                    )));
                }
                sleep(SOCKET_DIAL_INTERVAL).await;
            }
        }
    }
}

/// Append `chunk` to the shared stderr buffer, keeping only the last [`STDERR_CAP`]
/// bytes (retain the tail, which carries the fatal message).
fn append_capped(buf: &Arc<Mutex<String>>, chunk: &str) {
    let mut buf = buf.lock().expect("stderr buffer mutex");
    buf.push_str(chunk);
    if buf.len() > STDERR_CAP {
        // Trim from the front on a char boundary so the String stays valid UTF-8.
        let mut start = buf.len() - STDERR_CAP;
        while start < buf.len() && !buf.is_char_boundary(start) {
            start += 1;
        }
        *buf = buf[start..].to_string();
    }
}

/// A trimmed snapshot of the captured stderr, or `(empty)` when there is none.
fn stderr_or_empty(buf: &Arc<Mutex<String>>) -> String {
    let snapshot = buf.lock().expect("stderr buffer mutex").trim().to_string();
    if snapshot.is_empty() {
        "(empty)".to_string()
    } else {
        snapshot
    }
}

/// Human-readable `code=…, signal=…` for an exited child (mirrors the TS diagnostics).
fn describe_exit(status: std::process::ExitStatus) -> String {
    use std::os::unix::process::ExitStatusExt;
    format!("code={:?}, signal={:?}", status.code(), status.signal())
}

/// Terminate a child with SIGTERM, escalating to SIGKILL after [`TERMINATE_GRACE`].
/// A no-op if the child has already exited.
async fn terminate(child: &mut Child) {
    if matches!(child.try_wait(), Ok(Some(_))) {
        return;
    }
    // Graceful SIGTERM first: qemu exits cleanly (and removes its own socket).
    if let Some(pid) = child.id() {
        // SAFETY: `pid` is this child's PID (still live — try_wait above returned
        // None), and SIGTERM carries no memory-safety implications.
        let _ = unsafe { libc::kill(pid as libc::pid_t, libc::SIGTERM) };
    }
    match tokio::time::timeout(TERMINATE_GRACE, child.wait()).await {
        Ok(_) => {} // exited within the grace period
        Err(_) => {
            // Still alive after the grace period: SIGKILL and reap.
            let _ = child.kill().await;
        }
    }
}

/// A real launched Instance backed by a child process and a [`QmpClient`]. Owns the
/// QMP Session; callers drive the Guest through [`execute`](InstanceHandle::execute)
/// and tear everything down with [`close`](InstanceHandle::close).
pub struct RealInstanceHandle {
    child: AsyncMutex<Child>,
    client: QmpClient,
    qmp_socket_path: String,
    closed: AtomicBool,
}

impl RealInstanceHandle {
    fn new(child: Child, client: QmpClient, qmp_socket_path: String) -> Self {
        Self {
            child: AsyncMutex::new(child),
            client,
            qmp_socket_path,
            closed: AtomicBool::new(false),
        }
    }

    /// Subscribe to the Instance's async QMP events (the hook the Event Buffer
    /// consumes in slice #24). Kept on the concrete handle for now; the driver seam
    /// is extended to surface it through `dyn InstanceHandle` when that slice lands.
    pub fn subscribe_events(
        &self,
    ) -> tokio::sync::broadcast::Receiver<super::qmp_client::QmpEvent> {
        self.client.subscribe_events()
    }
}

#[async_trait]
impl InstanceHandle for RealInstanceHandle {
    async fn execute(&self, command: &str, args: Option<Value>) -> Result<Value, DriverError> {
        self.client
            .execute(command, args)
            .await
            // Preserve the QMP class+desc (or the connection reason) in the message.
            .map_err(|e| DriverError(e.to_string()))
    }

    async fn close(&self) -> Result<(), DriverError> {
        // Idempotent: the first close tears down; later ones are no-ops.
        if self.closed.swap(true, Ordering::SeqCst) {
            return Ok(());
        }
        self.client.close().await;
        {
            let mut child = self.child.lock().await;
            terminate(&mut child).await;
        }
        // QEMU removes its listening socket on a clean exit, but tidy up regardless
        // (a SIGKILLed qemu leaves it behind, which would block the next launch).
        let _ = tokio::fs::remove_file(&self.qmp_socket_path).await;
        Ok(())
    }
}
