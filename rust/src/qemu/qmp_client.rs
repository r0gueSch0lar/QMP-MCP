//! A hand-rolled dynamic QMP (QEMU Machine Protocol) client (ADR-0003/0011).
//!
//! QMP is QEMU's own JSON protocol, spoken over a UNIX socket as newline-delimited
//! JSON objects in both directions. This is a second implementation of the shared
//! bounded context, mirroring `../../src/qemu/qmp-client.ts` behaviorally: same
//! handshake sequence, same id-correlation, the same per-command timeout semantics,
//! and the same `{class, desc}` error mapping.
//!
//! Protocol shape:
//! - On connect QEMU sends a greeting: `{"QMP": {"version": ..., "capabilities": ...}}`.
//! - The client leaves negotiation mode by sending `qmp_capabilities`.
//! - Commands carry an `id`; responses echo it as `{"return": ...}` or
//!   `{"error": {"class", "desc"}}`, letting concurrent requests be correlated.
//! - Asynchronous `{"event": ...}` messages arrive at any time, unsolicited, and are
//!   surfaced on a broadcast channel for the Event Buffer (slice #24 consumes it).
//!
//! Everything stays dynamic `serde_json::Value` — matching the dynamic Command
//! Policy and `qmp_execute` (ADR-0003) — rather than the typed `qapi` crate.
//!
//! Concurrency model (idiomatic tokio, not the TS single-threaded event loop): a
//! background reader task owns the read half and dispatches each line to per-request
//! [`oneshot`] channels keyed by `id`, to the greeting waiter, or to the event
//! broadcast. Writers serialise on an async mutex over the write half. This keeps
//! [`QmpClient::execute`] on `&self` (as the driver seam requires) while serving
//! id-correlated commands over the one shared socket.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::unix::OwnedReadHalf;
use tokio::net::UnixStream;
use tokio::sync::{broadcast, oneshot, Mutex as AsyncMutex};
use tokio::task::JoinHandle;
use tokio::time::timeout;

/// Default handshake timeout: how long [`QmpClient::negotiate`] waits for the QMP
/// greeting before failing closed (a QEMU that connects but never greets must not
/// hang the launch). Mirrors the TS `DEFAULT_NEGOTIATE_TIMEOUT_MS`.
pub const DEFAULT_NEGOTIATE_TIMEOUT: Duration = Duration::from_millis(5_000);

/// Default per-command timeout. Bounds [`QmpClient::execute`] (and thus
/// `qmp_capabilities` during negotiation and every `query-status`) so a QEMU that
/// greets then goes silent fails closed instead of hanging forever. Mirrors the TS
/// `DEFAULT_COMMAND_TIMEOUT_MS`.
pub const DEFAULT_COMMAND_TIMEOUT: Duration = Duration::from_millis(10_000);

/// Capacity of the async-event broadcast channel. A slow consumer that lags beyond
/// this many buffered events observes a `RecvError::Lagged` (it never blocks the
/// reader). 256 matches the default Event Buffer size (`QMP_MCP_EVENT_BUFFER_SIZE`).
const EVENT_CHANNEL_CAPACITY: usize = 256;

/// The greeting object QEMU sends immediately on connection. The fields are kept as
/// opaque JSON (the client never inspects their shape).
#[derive(Debug, Clone)]
pub struct QmpGreeting {
    /// The reported QEMU/QMP version object.
    pub version: Value,
    /// The advertised capabilities list.
    pub capabilities: Value,
}

/// An asynchronous QMP event (e.g. `SHUTDOWN`, `STOP`, `RESUME`). `data` and
/// `timestamp` are kept dynamic. Mirrors the TS `QmpEvent` interface.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct QmpEvent {
    /// The event name (e.g. `SHUTDOWN`).
    pub event: String,
    /// Event-specific payload, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
    /// The `{seconds, microseconds}` wall-clock stamp QEMU attaches, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timestamp: Option<Value>,
}

/// Raised by the client. [`QmpError::Command`] carries the QMP error `class` + `desc`
/// (and the command name) verbatim so a caller can react or surface it;
/// [`QmpError::Connection`] covers timeouts, a closed socket, and write failures.
/// The `Command` display text mirrors the TS `QmpCommandError` message exactly.
#[derive(Debug, Clone, thiserror::Error)]
pub enum QmpError {
    /// QEMU answered the command with `{"error": {class, desc}}`.
    #[error("QMP command \"{command}\" failed [{class}]: {desc}")]
    Command {
        /// The command that failed.
        command: String,
        /// The QMP error class (e.g. `CommandNotFound`, `GenericError`).
        class: String,
        /// The human-readable error description.
        desc: String,
    },
    /// The command could not be completed because the connection failed, closed, or
    /// timed out. The message is actionable.
    #[error("{0}")]
    Connection(String),
}

/// The correlated reply the reader task hands back to a waiting [`QmpClient::execute`].
enum Reply {
    /// A `{"return": ...}` success payload.
    Return(Value),
    /// A `{"error": {class, desc}}` failure.
    Error { class: String, desc: String },
}

/// Shared state the reader task, the writers, and `close` all touch. Held in an
/// `Arc` so the reader task can outlive any single method call.
struct Shared {
    /// The write half, behind an async mutex so concurrent `execute` calls serialise
    /// their writes without interleaving bytes.
    write: AsyncMutex<tokio::net::unix::OwnedWriteHalf>,
    /// Monotonic request-id source.
    next_id: AtomicU64,
    /// Outstanding requests keyed by id, awaiting their correlated reply.
    pending: Mutex<HashMap<u64, oneshot::Sender<Reply>>>,
    /// Broadcast of async QMP events to any subscriber (the Event Buffer, slice #24).
    events_tx: broadcast::Sender<QmpEvent>,
    /// Set once the connection has failed/closed; makes teardown idempotent.
    closed: AtomicBool,
    /// The reason recorded at close, surfaced to any outstanding/subsequent command.
    close_reason: Mutex<Option<String>>,
}

impl Shared {
    /// Mark the connection closed and reject every outstanding request. Idempotent:
    /// only the first call records the reason and drains the pending map. Dropping a
    /// pending sender makes the waiting `execute` observe a closed connection.
    fn fail_all(&self, reason: &str) {
        if self.closed.swap(true, Ordering::SeqCst) {
            return;
        }
        *self.close_reason.lock().expect("close_reason mutex") = Some(reason.to_string());
        let mut pending = self.pending.lock().expect("pending mutex");
        pending.clear(); // dropping the senders wakes each waiter with a recv error
    }

    /// The recorded close reason, or a generic fallback.
    fn close_reason(&self) -> String {
        self.close_reason
            .lock()
            .expect("close_reason mutex")
            .clone()
            .unwrap_or_else(|| "QMP connection is closed.".to_string())
    }
}

/// A QMP client wrapping a connected [`UnixStream`]. Speaks QMP over it: completes
/// the greeting → `qmp_capabilities` handshake ([`negotiate`](Self::negotiate)),
/// serves id-correlated commands ([`execute`](Self::execute)), surfaces async events
/// ([`subscribe_events`](Self::subscribe_events)), and tears down ([`close`](Self::close)).
pub struct QmpClient {
    shared: std::sync::Arc<Shared>,
    /// The reader task handle, aborted on close.
    reader: Mutex<Option<JoinHandle<()>>>,
    /// The greeting receiver, taken once by [`negotiate`](Self::negotiate).
    greeting_rx: AsyncMutex<Option<oneshot::Receiver<QmpGreeting>>>,
    /// Signals the reader task to stop; fired by [`close`](Self::close).
    shutdown: Mutex<Option<oneshot::Sender<()>>>,
    /// The greeting, cached after a successful [`negotiate`](Self::negotiate).
    greeting: Mutex<Option<QmpGreeting>>,
}

impl QmpClient {
    /// Wrap an already-connected QMP socket. Spawns the background reader task that
    /// frames incoming lines and dispatches them. The caller still drives
    /// [`negotiate`](Self::negotiate) to complete the handshake.
    pub fn new(stream: UnixStream) -> Self {
        let (read, write) = stream.into_split();
        let (events_tx, _rx) = broadcast::channel(EVENT_CHANNEL_CAPACITY);
        let shared = std::sync::Arc::new(Shared {
            write: AsyncMutex::new(write),
            next_id: AtomicU64::new(1),
            pending: Mutex::new(HashMap::new()),
            events_tx,
            closed: AtomicBool::new(false),
            close_reason: Mutex::new(None),
        });
        let (greeting_tx, greeting_rx) = oneshot::channel();
        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        let reader = tokio::spawn(run_reader(
            read,
            std::sync::Arc::clone(&shared),
            shutdown_rx,
            greeting_tx,
        ));
        Self {
            shared,
            reader: Mutex::new(Some(reader)),
            greeting_rx: AsyncMutex::new(Some(greeting_rx)),
            shutdown: Mutex::new(Some(shutdown_tx)),
            greeting: Mutex::new(None),
        }
    }

    /// Dial a QMP UNIX socket and return a client wrapping the live connection. The
    /// caller still drives [`negotiate`](Self::negotiate).
    pub async fn dial(socket_path: &str) -> Result<Self, QmpError> {
        let stream = UnixStream::connect(socket_path).await.map_err(|e| {
            QmpError::Connection(format!(
                "Failed to connect to the QMP socket at {socket_path}: {e}"
            ))
        })?;
        Ok(Self::new(stream))
    }

    /// Subscribe to async QMP events. Each returned receiver observes every event
    /// broadcast from now on (the hook the Event Buffer consumes in slice #24).
    pub fn subscribe_events(&self) -> broadcast::Receiver<QmpEvent> {
        self.shared.events_tx.subscribe()
    }

    /// The greeting QEMU sent, available after [`negotiate`](Self::negotiate) resolves.
    pub fn greeting(&self) -> Option<QmpGreeting> {
        self.greeting.lock().expect("greeting mutex").clone()
    }

    /// Complete the QMP handshake with the default timeouts: wait for the greeting,
    /// then send `qmp_capabilities` to leave negotiation mode and establish the
    /// Session.
    pub async fn negotiate(&self) -> Result<(), QmpError> {
        self.negotiate_with_timeouts(DEFAULT_NEGOTIATE_TIMEOUT, DEFAULT_COMMAND_TIMEOUT)
            .await
    }

    /// [`negotiate`](Self::negotiate) with explicit timeouts (injected by tests).
    pub async fn negotiate_with_timeouts(
        &self,
        greeting_timeout: Duration,
        command_timeout: Duration,
    ) -> Result<(), QmpError> {
        let rx = self.greeting_rx.lock().await.take().ok_or_else(|| {
            QmpError::Connection("QMP greeting was already consumed.".to_string())
        })?;
        let greeting = match timeout(greeting_timeout, rx).await {
            Ok(Ok(greeting)) => greeting,
            // The reader dropped the sender: the socket closed before greeting.
            Ok(Err(_)) => {
                return Err(QmpError::Connection(
                    "QMP connection closed before the greeting arrived.".to_string(),
                ))
            }
            Err(_) => {
                return Err(QmpError::Connection(format!(
                    "Timed out after {}ms waiting for the QMP greeting.",
                    greeting_timeout.as_millis()
                )))
            }
        };
        *self.greeting.lock().expect("greeting mutex") = Some(greeting);
        self.execute_with_timeout("qmp_capabilities", None, command_timeout)
            .await?;
        Ok(())
    }

    /// Execute a QMP command with the default per-command timeout, correlating the
    /// response by `id`. Resolves with the command's `return` value, or a
    /// [`QmpError`].
    pub async fn execute(&self, command: &str, args: Option<Value>) -> Result<Value, QmpError> {
        self.execute_with_timeout(command, args, DEFAULT_COMMAND_TIMEOUT)
            .await
    }

    /// [`execute`](Self::execute) with an explicit timeout. Rejects with
    /// [`QmpError::Command`] when QEMU reports `{error}`, or [`QmpError::Connection`]
    /// when the socket is closed, the write fails, or no response arrives within
    /// `command_timeout` (so a silent QEMU fails closed, not hangs).
    pub async fn execute_with_timeout(
        &self,
        command: &str,
        args: Option<Value>,
        command_timeout: Duration,
    ) -> Result<Value, QmpError> {
        if self.shared.closed.load(Ordering::SeqCst) {
            return Err(QmpError::Connection(self.shared.close_reason()));
        }

        let id = self.shared.next_id.fetch_add(1, Ordering::SeqCst);
        let mut message = serde_json::Map::new();
        message.insert("execute".to_string(), Value::String(command.to_string()));
        if let Some(args) = args {
            message.insert("arguments".to_string(), args);
        }
        message.insert("id".to_string(), Value::from(id));
        let line = format!(
            "{}\n",
            serde_json::to_string(&Value::Object(message)).expect("serialise QMP request")
        );

        let (tx, rx) = oneshot::channel();
        self.shared
            .pending
            .lock()
            .expect("pending mutex")
            .insert(id, tx);

        // Write under the async write lock so concurrent commands never interleave.
        {
            let mut write = self.shared.write.lock().await;
            if let Err(e) = write.write_all(line.as_bytes()).await {
                self.shared
                    .pending
                    .lock()
                    .expect("pending mutex")
                    .remove(&id);
                return Err(QmpError::Connection(format!(
                    "Failed to write QMP command \"{command}\" to the socket: {e}"
                )));
            }
            let _ = write.flush().await;
        }

        match timeout(command_timeout, rx).await {
            Ok(Ok(Reply::Return(value))) => Ok(value),
            Ok(Ok(Reply::Error { class, desc })) => Err(QmpError::Command {
                command: command.to_string(),
                class,
                desc,
            }),
            // The sender was dropped (connection failed/closed) before a reply.
            Ok(Err(_)) => Err(QmpError::Connection(self.shared.close_reason())),
            Err(_) => {
                self.shared
                    .pending
                    .lock()
                    .expect("pending mutex")
                    .remove(&id);
                Err(QmpError::Connection(format!(
                    "QMP command \"{command}\" timed out after {}ms with no response from QEMU.",
                    command_timeout.as_millis()
                )))
            }
        }
    }

    /// Close the client: stop the reader, shut down the write half, and reject any
    /// outstanding requests with a closed-connection error. Idempotent.
    pub async fn close(&self) {
        if let Some(tx) = self.shutdown.lock().expect("shutdown mutex").take() {
            let _ = tx.send(());
        }
        {
            let mut write = self.shared.write.lock().await;
            let _ = write.shutdown().await;
        }
        self.shared.fail_all("QMP connection closed.");
        if let Some(handle) = self.reader.lock().expect("reader mutex").take() {
            handle.abort();
        }
    }
}

/// The background reader task: frame newline-delimited JSON off the read half and
/// dispatch each message until the socket ends or [`QmpClient::close`] signals
/// shutdown. On exit it fails every outstanding request (and, by dropping
/// `greeting_tx`, unblocks a greeting waiter) so nothing hangs.
async fn run_reader(
    read: OwnedReadHalf,
    shared: std::sync::Arc<Shared>,
    mut shutdown: oneshot::Receiver<()>,
    greeting_tx: oneshot::Sender<QmpGreeting>,
) {
    let mut greeting_tx = Some(greeting_tx);
    let mut reader = BufReader::new(read);
    let mut line = String::new();
    loop {
        line.clear();
        tokio::select! {
            _ = &mut shutdown => break,
            result = reader.read_line(&mut line) => match result {
                Ok(0) => break, // EOF: peer closed
                Ok(_) => {
                    let trimmed = line.trim();
                    if !trimmed.is_empty() {
                        dispatch(&shared, trimmed, &mut greeting_tx);
                    }
                }
                Err(_) => break, // read error: treat as closed
            },
        }
    }
    shared.fail_all("QMP connection closed.");
    // `greeting_tx` (if still Some) drops here → a pending negotiate errors out.
}

/// Parse and route one QMP line: the greeting, an async event, or a correlated
/// command reply. A malformed line is ignored (unexpected from QEMU, but it must
/// not tear down the session). Mirrors the TS `#dispatch`.
fn dispatch(
    shared: &std::sync::Arc<Shared>,
    line: &str,
    greeting_tx: &mut Option<oneshot::Sender<QmpGreeting>>,
) {
    let value: Value = match serde_json::from_str(line) {
        Ok(value) => value,
        Err(_) => return,
    };
    let Some(object) = value.as_object() else {
        return;
    };

    // Greeting: {"QMP": {version, capabilities}}.
    if let Some(qmp) = object.get("QMP") {
        let greeting = QmpGreeting {
            version: qmp.get("version").cloned().unwrap_or(Value::Null),
            capabilities: qmp
                .get("capabilities")
                .cloned()
                .unwrap_or_else(|| Value::Array(Vec::new())),
        };
        if let Some(tx) = greeting_tx.take() {
            let _ = tx.send(greeting);
        }
        return;
    }

    // Async event: broadcast to any subscriber (ignore if none).
    if object.contains_key("event") {
        if let Ok(event) = serde_json::from_value::<QmpEvent>(value) {
            let _ = shared.events_tx.send(event);
        }
        return;
    }

    // Correlated reply: {id, return|error}.
    let Some(id) = object.get("id").and_then(Value::as_u64) else {
        return;
    };
    let Some(tx) = shared.pending.lock().expect("pending mutex").remove(&id) else {
        return;
    };
    if let Some(error) = object.get("error") {
        let class = error
            .get("class")
            .and_then(Value::as_str)
            .unwrap_or("GenericError")
            .to_string();
        let desc = error
            .get("desc")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let _ = tx.send(Reply::Error { class, desc });
    } else {
        let value = object.get("return").cloned().unwrap_or(Value::Null);
        let _ = tx.send(Reply::Return(value));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::net::unix::OwnedWriteHalf;
    use tokio::net::UnixStream;

    /// A fake QEMU: the peer end of an in-memory `UnixStream` pair (no filesystem,
    /// no real process). The test drives the QMP protocol by hand over it.
    struct FakeQemu {
        reader: BufReader<OwnedReadHalf>,
        writer: OwnedWriteHalf,
    }

    impl FakeQemu {
        /// Send one JSON line (a greeting/reply/event) to the client.
        async fn send(&mut self, line: &str) {
            self.writer.write_all(line.as_bytes()).await.unwrap();
            self.writer.write_all(b"\n").await.unwrap();
            self.writer.flush().await.unwrap();
        }

        /// Read the next request line the client wrote.
        async fn recv(&mut self) -> Value {
            let mut line = String::new();
            self.reader.read_line(&mut line).await.unwrap();
            serde_json::from_str(line.trim()).unwrap()
        }
    }

    /// A connected `(client, fakeQemu)` pair over an in-memory socket.
    fn pair() -> (QmpClient, FakeQemu) {
        let (client_side, qemu_side) = UnixStream::pair().unwrap();
        let (read, write) = qemu_side.into_split();
        (
            QmpClient::new(client_side),
            FakeQemu {
                reader: BufReader::new(read),
                writer: write,
            },
        )
    }

    /// The greeting the client expects before it will leave negotiation.
    const GREETING: &str =
        r#"{"QMP":{"version":{"qemu":{"major":8,"minor":2,"micro":0}},"capabilities":[]}}"#;

    #[tokio::test]
    async fn negotiate_reads_greeting_then_sends_qmp_capabilities() {
        let (client, mut qemu) = pair();
        // The fake QEMU greets, then the client must send qmp_capabilities.
        qemu.send(GREETING).await;
        let negotiate = tokio::spawn(async move {
            client.negotiate().await.unwrap();
            client
        });
        let req = qemu.recv().await;
        assert_eq!(req["execute"], "qmp_capabilities");
        let id = req["id"].as_u64().unwrap();
        qemu.send(&format!(r#"{{"return":{{}},"id":{id}}}"#)).await;

        let client = negotiate.await.unwrap();
        assert!(client.greeting().is_some());
    }

    #[tokio::test]
    async fn correlates_concurrent_responses_by_id() {
        let (client, mut qemu) = pair();
        qemu.send(GREETING).await;
        let client = std::sync::Arc::new(client);
        {
            let c = std::sync::Arc::clone(&client);
            tokio::spawn(async move { c.negotiate().await.unwrap() });
        }
        // Drain (and answer) the qmp_capabilities handshake.
        let cap = qemu.recv().await;
        let cap_id = cap["id"].as_u64().unwrap();
        qemu.send(&format!(r#"{{"return":{{}},"id":{cap_id}}}"#))
            .await;

        // Fire two commands concurrently; answer them OUT OF ORDER.
        let c1 = std::sync::Arc::clone(&client);
        let t1 = tokio::spawn(async move { c1.execute("query-status", None).await });
        let c2 = std::sync::Arc::clone(&client);
        let t2 = tokio::spawn(async move { c2.execute("query-name", None).await });

        let a = qemu.recv().await;
        let b = qemu.recv().await;
        // Map each id back to its command so we answer the RIGHT request.
        let (status_id, name_id) = if a["execute"] == "query-status" {
            (a["id"].as_u64().unwrap(), b["id"].as_u64().unwrap())
        } else {
            (b["id"].as_u64().unwrap(), a["id"].as_u64().unwrap())
        };
        // Reply to query-name FIRST, then query-status.
        qemu.send(&format!(r#"{{"return":{{"name":"vm"}},"id":{name_id}}}"#))
            .await;
        qemu.send(&format!(
            r#"{{"return":{{"status":"running","running":true}},"id":{status_id}}}"#
        ))
        .await;

        let status = t1.await.unwrap().unwrap();
        let name = t2.await.unwrap().unwrap();
        assert_eq!(status["status"], "running");
        assert_eq!(name["name"], "vm");
    }

    #[tokio::test]
    async fn maps_qmp_error_preserving_class_and_desc() {
        let (client, mut qemu) = pair();
        qemu.send(GREETING).await;
        tokio::spawn(async move {
            let req = qemu.recv().await; // qmp_capabilities
            let id = req["id"].as_u64().unwrap();
            qemu.send(&format!(r#"{{"return":{{}},"id":{id}}}"#)).await;
            let req = qemu.recv().await; // the failing command
            let id = req["id"].as_u64().unwrap();
            qemu.send(&format!(
                r#"{{"error":{{"class":"CommandNotFound","desc":"no such command"}},"id":{id}}}"#
            ))
            .await;
        });
        client.negotiate().await.unwrap();
        let err = client.execute("bogus", None).await.unwrap_err();
        // The Display text mirrors the TS `QmpCommandError` wording exactly.
        assert_eq!(
            err.to_string(),
            r#"QMP command "bogus" failed [CommandNotFound]: no such command"#
        );
        match err {
            QmpError::Command {
                command,
                class,
                desc,
            } => {
                assert_eq!(command, "bogus");
                assert_eq!(class, "CommandNotFound");
                assert_eq!(desc, "no such command");
            }
            other => panic!("expected a Command error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn command_times_out_when_qemu_goes_silent() {
        let (client, mut qemu) = pair();
        qemu.send(GREETING).await;
        // Negotiate with a tiny command timeout, then answer only the handshake.
        let negotiate = tokio::spawn(async move {
            client
                .negotiate_with_timeouts(Duration::from_millis(500), Duration::from_millis(500))
                .await
                .unwrap();
            client
        });
        let cap = qemu.recv().await;
        let cap_id = cap["id"].as_u64().unwrap();
        qemu.send(&format!(r#"{{"return":{{}},"id":{cap_id}}}"#))
            .await;
        let client = negotiate.await.unwrap();

        // This command is never answered: it must fail closed with a timeout.
        let err = client
            .execute_with_timeout("query-status", None, Duration::from_millis(80))
            .await
            .unwrap_err();
        match err {
            QmpError::Connection(msg) => {
                assert!(msg.contains("timed out"), "got: {msg}");
                assert!(msg.contains("query-status"), "got: {msg}");
            }
            other => panic!("expected a Connection timeout, got {other:?}"),
        }
        // Keep the fake QEMU alive until here so the socket does not close early.
        drop(qemu);
    }

    #[tokio::test]
    async fn delivers_async_events_to_a_subscriber() {
        let (client, mut qemu) = pair();
        qemu.send(GREETING).await;
        tokio::spawn(async move {
            let req = qemu.recv().await; // qmp_capabilities
            let id = req["id"].as_u64().unwrap();
            qemu.send(&format!(r#"{{"return":{{}},"id":{id}}}"#)).await;
            // Now emit an unsolicited async event.
            qemu.send(
                r#"{"event":"SHUTDOWN","data":{"guest":true},"timestamp":{"seconds":1,"microseconds":2}}"#,
            )
            .await;
            // Hold the connection open so the event is not lost to a close.
            tokio::time::sleep(Duration::from_millis(200)).await;
        });
        // Subscribe BEFORE negotiate so the broadcast receiver sees the event.
        let mut events = client.subscribe_events();
        client.negotiate().await.unwrap();
        let event = timeout(Duration::from_secs(1), events.recv())
            .await
            .expect("event arrives")
            .expect("event received");
        assert_eq!(event.event, "SHUTDOWN");
        assert_eq!(event.data.unwrap()["guest"], true);
    }

    #[tokio::test]
    async fn execute_fails_closed_after_the_socket_drops() {
        let (client, qemu) = pair();
        // Drop the peer without ever greeting: the reader observes EOF and closes.
        drop(qemu);
        // Give the reader a moment to observe the closed socket.
        tokio::time::sleep(Duration::from_millis(50)).await;
        let err = client.execute("query-status", None).await.unwrap_err();
        assert!(matches!(err, QmpError::Connection(_)));
    }
}
