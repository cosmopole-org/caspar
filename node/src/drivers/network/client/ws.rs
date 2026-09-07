//! Translation of `drivers/network/client/ws/ws.go`.
//!
//! `Ws` implements [`IWs`] — a TLS WebSocket server using `tungstenite` for
//! the WS protocol. Each accepted WS connection runs the same authentication
//! / action-dispatch state machine as the TCP driver, but speaks the framing
//! over `Message::Binary(...)` frames instead of length-prefixed TCP frames.
//!
//! Go's original (lxzan/gws) wrote two messages per outbound payload — first
//! a 4-byte length and then the body. The Rust translation collapses that
//! into a single Binary message whose payload is `u32be(len) || body` so the
//! shape matches what the existing Go clients expect on the wire.
//!
//! ## Concurrency model
//!
//! `tungstenite::WebSocket` is a single owned object that backs both `read()`
//! and `send()` with the same underlying TCP stream. Sharing it across the
//! reader and arbitrary writer threads requires a mutex, but holding that
//! mutex across the blocking `read()` call starves writers (creature signal
//! results from a wasm thread, federation pushes, etc.) which is exactly the
//! symptom we used to see: the inbound side never let go of the WS while a
//! client was idle waiting for an async response.
//!
//! The driver therefore pins each connection's `WebSocket<TlsStream>` to a
//! single dedicated I/O thread. External writers push outbound frames onto a
//! per-socket `mpsc::Sender`, never touching the WS directly. The I/O thread
//! runs a tight loop that:
//!
//! 1. drains the outbound channel into a local FIFO,
//! 2. pushes the next frame onto the wire if the prior frame has been ACKed,
//! 3. performs a short-timeout `ws.read()` to interleave new inbound packets
//!    (ACK / request / control) without ever starving the writer side.
//!
//! Shutdown is cooperative: anyone can call `socket.shutdown()` to flip the
//! disconnected flag and queue a `Shutdown` token; the I/O thread drains its
//! pending sends, closes the underlying WS, and exits.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Sender, TryRecvError};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use dashmap::DashMap;
use serde_json::Value;
use tungstenite::protocol::Message;
use tungstenite::{accept as ws_accept, WebSocket};

use crate::drivers::gateway_subs;
use crate::drivers::network::framing::{
    accept, bind_tls, decode_request_body, encode_client_response_body,
    encode_client_update_body, TlsStream,
};
use crate::models::core::ICore;
use crate::models::packet::{build_error_json, ResponseSimpleMessage};
use crate::models::ports::network::ws::IWs;
use crate::models::ports::ratelimit::{
    rate_limited_body, Protocol, RateLimitDecision, RateLimitKey, RATE_LIMITED_RES_CODE,
};
use crate::models::ports::network::TlsConfig;
use crate::models::ports::signaler::Listener;
use crate::models::transaction::ITrx;
use crate::shell::utils::crypto::secure_unique_string;

/// Items the I/O thread accepts from external writers.
enum OutboundFrame {
    /// Already-encoded frame body (no length prefix yet).
    Body(Vec<u8>),
    /// Cooperative shutdown signal.
    Shutdown,
}

/// Per-WS-connection state visible to the rest of the node.
///
/// The actual `WebSocket` handle lives inside a dedicated I/O thread (see
/// [`Ws::handle_connection`]); writers reach it only through `outbound`.
pub struct Socket {
    pub id: String,
    peer: String,
    user_id: Mutex<String>,
    disconnected: AtomicBool,
    listener_registered: AtomicBool,
    /// Single producer / single consumer in steady state, but we use a
    /// standard `mpsc` so any number of writer threads can enqueue without
    /// taking a lock on the WS itself. `None` once the socket has shut down.
    outbound: Mutex<Option<Sender<OutboundFrame>>>,
}

impl Socket {
    fn new(peer: String, outbound: Sender<OutboundFrame>) -> Arc<Socket> {
        Arc::new(Socket {
            id: secure_unique_string(),
            peer,
            user_id: Mutex::new(String::new()),
            disconnected: AtomicBool::new(false),
            listener_registered: AtomicBool::new(false),
            outbound: Mutex::new(Some(outbound)),
        })
    }

    fn peer_ip(&self) -> String {
        self.peer
            .rsplit_once(':')
            .map(|(a, _)| a.to_string())
            .unwrap_or_else(|| self.peer.clone())
    }

    fn user_id(&self) -> String {
        self.user_id.lock().unwrap().clone()
    }

    fn set_user_id(&self, id: &str) {
        *self.user_id.lock().unwrap() = id.to_string();
    }

    fn is_disconnected(&self) -> bool {
        self.disconnected.load(Ordering::Acquire)
    }

    /// Queue a frame for the I/O thread to send. Drops silently if the
    /// socket has already shut down — callers (signaler listeners, async
    /// action handlers) should treat send as best-effort, since the same
    /// connection may close at any time.
    fn enqueue(&self, frame: Vec<u8>) {
        if self.is_disconnected() {
            return;
        }
        let guard = self.outbound.lock().unwrap();
        if let Some(tx) = guard.as_ref() {
            let _ = tx.send(OutboundFrame::Body(frame));
        }
    }

    fn write_update(&self, key: &str, payload: &[u8]) {
        let frame = encode_client_update_body(key, payload);
        self.enqueue(frame);
    }

    fn write_response(&self, packet_id: &str, res_code: i64, payload: &[u8]) {
        let frame = encode_client_response_body(packet_id, res_code, payload);
        self.enqueue(frame);
    }

    /// Idempotent cooperative shutdown.
    fn shutdown(&self) {
        if self.disconnected.swap(true, Ordering::AcqRel) {
            return;
        }
        let mut guard = self.outbound.lock().unwrap();
        if let Some(tx) = guard.take() {
            let _ = tx.send(OutboundFrame::Shutdown);
        }
    }
}

/// `Ws` driver implementing [`IWs`].
pub struct Ws {
    app: Arc<dyn ICore>,
    sockets: Arc<DashMap<String, Arc<Socket>>>,
    /// See `Tcp::user_sockets`: multiple concurrent connections may share one
    /// user_id, but the signaler keeps a single listener per user. We fan
    /// signal results out to every live socket of the user (`user_id ->
    /// {socket.id -> socket}`); clients de-dupe by correlationId.
    user_sockets: Arc<DashMap<String, Arc<DashMap<String, Arc<Socket>>>>>,
}

impl Ws {
    /// `NewWs(app)`.
    pub fn new(app: Arc<dyn ICore>) -> Arc<Ws> {
        Arc::new(Ws {
            app,
            sockets: Arc::new(DashMap::new()),
            user_sockets: Arc::new(DashMap::new()),
        })
    }

    fn process_inbound(self: &Arc<Self>, socket: &Arc<Socket>, body: Vec<u8>) {
        // The 4-byte length prefix that the legacy Go (gws) client wraps
        // every message in is already stripped by the I/O loop before this
        // is called. ACK detection also happens upstream, so any body that
        // reaches here is a real request body.
        let body_len = body.len();
        let parsed = match decode_request_body(&body) {
            Ok(p) => p,
            Err(e) => {
                eprintln!(
                    "[ws] decode_request_body failed: peer={} body_len={} err={}",
                    socket.peer, body_len, e
                );
                return;
            }
        };
        let peer_ip = socket.peer_ip();
        let started = Instant::now();
        eprintln!(
            "[ws] >> path={} user={} pkt={} payload_len={} peer={}",
            parsed.path,
            parsed.user_id,
            parsed.packet_id,
            parsed.payload.len(),
            socket.peer
        );

        // Cross-protocol admission control — the same shared limiter the TCP and
        // HTTP-ingress transports use. Key on the socket's verified user id (set
        // only after signature auth); pre-auth traffic bills the peer IP. See
        // `Tcp::process_inbound` for the rationale.
        let verified_user = socket.user_id();
        let rl_key = if verified_user.is_empty() {
            RateLimitKey::anonymous(Protocol::Ws, &peer_ip, &parsed.path)
        } else {
            RateLimitKey::authenticated(Protocol::Ws, &verified_user, &peer_ip, &parsed.path)
        };
        if let RateLimitDecision::Limited { retry_after, scope } =
            self.app.tools().rate_limiter().check(&rl_key)
        {
            let body = serde_json::to_vec(&rate_limited_body(retry_after, scope))
                .unwrap_or_default();
            socket.write_response(&parsed.packet_id, RATE_LIMITED_RES_CODE, &body);
            eprintln!(
                "[ws] << path={} pkt={} code={} rate_limited scope={} retry_ms={} elapsed_ms={}",
                parsed.path,
                parsed.packet_id,
                RATE_LIMITED_RES_CODE,
                scope.as_str(),
                retry_after.as_millis(),
                started.elapsed().as_millis()
            );
            return;
        }

        match parsed.path.as_str() {
            "logout" => {
                let (ok, _, _) = self
                    .app
                    .tools()
                    .security()
                    .auth_with_signature(&parsed.user_id, &parsed.payload, &parsed.signature);
                let msg = if ok {
                    self.app
                        .tools()
                        .signaler()
                        .listeners()
                        .remove(&parsed.user_id);
                    "loggedout"
                } else {
                    "logout_failed"
                };
                socket.write_response(
                    &parsed.packet_id,
                    0,
                    &serde_json::to_vec(&build_error_json(msg)).unwrap_or_default(),
                );
                eprintln!(
                    "[ws] << path=logout pkt={} elapsed_ms={}",
                    parsed.packet_id,
                    started.elapsed().as_millis()
                );
                return;
            }
            "authenticate" | "/creatures/authenticate" => {
                let (ok, _, _) = self
                    .app
                    .tools()
                    .security()
                    .auth_with_signature(&parsed.user_id, &parsed.payload, &parsed.signature);
                if ok {
                    self.attach_user_listener(socket, &parsed.user_id);
                    socket.write_response(
                        &parsed.packet_id,
                        0,
                        &serde_json::to_vec(&build_error_json("authenticated"))
                            .unwrap_or_default(),
                    );
                    let msg = serde_json::to_vec(&ResponseSimpleMessage {
                        message: "old_queue_end".to_string(),
                    })
                    .unwrap_or_default();
                    socket.write_update("old_queue_end", &msg);
                } else {
                    socket.write_response(
                        &parsed.packet_id,
                        4,
                        &serde_json::to_vec(&build_error_json("authentication failed"))
                            .unwrap_or_default(),
                    );
                }
                eprintln!(
                    "[ws] << path=authenticate pkt={} elapsed_ms={}",
                    parsed.packet_id,
                    started.elapsed().as_millis()
                );
                return;
            }
            _ => {}
        }

        let secure = match self.app.actor().fetch_secure_action(&parsed.path) {
            Some(s) => s,
            None => {
                socket.write_response(
                    &parsed.packet_id,
                    1,
                    &serde_json::to_vec(&build_error_json("action not found"))
                        .unwrap_or_default(),
                );
                eprintln!(
                    "[ws] << path={} pkt={} code=1 action_not_found elapsed_ms={}",
                    parsed.path,
                    parsed.packet_id,
                    started.elapsed().as_millis()
                );
                return;
            }
        };
        let raw_payload =
            serde_json::from_slice::<Value>(&parsed.payload).unwrap_or(Value::Null);
        let input = match secure.parse_input("ws", raw_payload) {
            Ok(i) => i,
            Err(e) => {
                socket.write_response(
                    &parsed.packet_id,
                    2,
                    &serde_json::to_vec(&build_error_json(&format!("{}", e)))
                        .unwrap_or_default(),
                );
                eprintln!(
                    "[ws] << path={} pkt={} code=2 parse_input_err={} elapsed_ms={}",
                    parsed.path,
                    parsed.packet_id,
                    e,
                    started.elapsed().as_millis()
                );
                return;
            }
        };
        match secure.securely_act(
            &parsed.user_id,
            &parsed.packet_id,
            &parsed.payload,
            &parsed.signature,
            input,
            &peer_ip,
            &[],
        ) {
            Ok((sc, value)) => {
                let body = serde_json::to_vec(&value).unwrap_or_default();
                socket.write_response(&parsed.packet_id, sc, &body);
                eprintln!(
                    "[ws] << path={} pkt={} code={} resp_len={} elapsed_ms={}",
                    parsed.path,
                    parsed.packet_id,
                    sc,
                    body.len(),
                    started.elapsed().as_millis()
                );
                // Lazily register the update-stream listener after the first
                // successful authenticated request on this connection, exactly
                // like Tcp does. Without this, a WS client that dev-logs-in
                // via /creatures/login (and never calls the token-based
                // /creatures/authenticate) has no listener, so creature signal
                // results (creatures/signal/result) are silently dropped.
                if !parsed.user_id.is_empty()
                    && !socket.listener_registered.load(Ordering::Acquire)
                {
                    socket.listener_registered.store(true, Ordering::Release);
                    self.attach_user_listener(socket, &parsed.user_id);
                }
                // The gateway subscription channel. `/gateway/subscribe`
                // verifies the bridge's bearer token and answers with the
                // topics it granted; binding the socket has to happen here,
                // because only the transport can write to this connection.
                self.apply_gateway_subscription(socket, &parsed.path, &value);
            }
            Err(e) => {
                socket.write_response(
                    &parsed.packet_id,
                    3,
                    &serde_json::to_vec(&build_error_json(&format!("{}", e)))
                        .unwrap_or_default(),
                );
                eprintln!(
                    "[ws] << path={} pkt={} code=3 act_err={} elapsed_ms={}",
                    parsed.path,
                    parsed.packet_id,
                    e,
                    started.elapsed().as_millis()
                );
            }
        }
    }

    /// Bind (or release) this connection's gateway topic subscription from an
    /// action's result.
    ///
    /// The action layer authenticates the bearer token and decides the topics;
    /// this only wires the socket to them. The sink returns `false` once the
    /// connection is gone, which is how the registry reaps a bridge whose
    /// sandbox went away.
    fn apply_gateway_subscription(&self, socket: &Arc<Socket>, path: &str, result: &Value) {
        match path {
            "/gateway/subscribe" => {
                let grant = &result["gatewaySubscribe"];
                let topics: Vec<String> = grant["topics"]
                    .as_array()
                    .map(|items| {
                        items
                            .iter()
                            .filter_map(|v| v.as_str().map(|s| s.to_string()))
                            .collect()
                    })
                    .unwrap_or_default();
                if topics.is_empty() {
                    return;
                }
                let sink_socket = socket.clone();
                gateway_subs::subscribe(gateway_subs::Subscriber {
                    id: socket.id.clone(),
                    creature_id: grant["creatureId"].as_str().unwrap_or("").to_string(),
                    topics,
                    sink: Arc::new(move |key: &str, data: &Value| {
                        if sink_socket.is_disconnected() {
                            return false;
                        }
                        let bytes = serde_json::to_vec(data).unwrap_or_default();
                        sink_socket.write_update(key, &bytes);
                        true
                    }),
                });
            }
            "/gateway/unsubscribe" => {
                if result["gatewayUnsubscribe"].as_bool().unwrap_or(false) {
                    gateway_subs::unsubscribe(&socket.id);
                }
            }
            _ => {}
        }
    }

    fn attach_user_listener(self: &Arc<Self>, socket: &Arc<Socket>, user_id: &str) {
        // Track every live socket for this user so signal results fan out to all
        // of the user's connections, not just the most recent (the signaler
        // keeps a single listener per user_id). See `Tcp::attach_user_listener`.
        let user_set = self
            .user_sockets
            .entry(user_id.to_string())
            .or_insert_with(|| Arc::new(DashMap::new()))
            .value()
            .clone();
        user_set.insert(socket.id.clone(), socket.clone());

        let broadcast_set = user_set.clone();
        let listener = Arc::new(Listener {
            id: user_id.to_string(),
            paused: false,
            dis_time: 0,
            signal: Arc::new(move |key, value| {
                let bytes = serde_json::to_vec(&value).unwrap_or_default();
                // Pushes onto the I/O thread's outbound channel; never
                // contends with the read loop.
                for entry in broadcast_set.iter() {
                    entry.value().write_update(&key, &bytes);
                }
            }),
        });
        self.sockets.insert(user_id.to_string(), socket.clone());
        socket.set_user_id(user_id);
        self.app.tools().signaler().listen_to_single(listener);

        let prefix = format!("hasaccess::{}::", user_id);
        let store_ids = Arc::new(Mutex::new(Vec::<String>::new()));
        let store_clone = store_ids.clone();
        let prefix_owned = prefix.clone();
        self.app.modify_state(
            true,
            Box::new(move |trx: &dyn ITrx| {
                if let Ok(ids) = trx.get_links_list(&prefix_owned, -1, -1, &[]) {
                    *store_clone.lock().unwrap() = ids;
                }
                Ok(())
            }),
        );
        let ids = store_ids.lock().unwrap().clone();
        for id in ids {
            let store_id = id.strip_prefix(&prefix).unwrap_or(&id).to_string();
            self.app.tools().signaler().join_group(&store_id, user_id);
        }
    }

    fn handle_connection(self: Arc<Self>, stream: TlsStream) {
        let peer = stream.peer_addr();
        let mut ws = match ws_accept(stream) {
            Ok(ws) => ws,
            Err(e) => {
                eprintln!("ws handshake from {}: {}", peer, e);
                return;
            }
        };
        // Short read timeout so `ws.read()` returns `WouldBlock` periodically
        // and the I/O loop can interleave outbound sends. The lock-free
        // architecture means writers never block on this; the timeout just
        // bounds how long the loop sleeps between outbound flushes.
        let _ = ws.get_ref().set_read_timeout(Some(Duration::from_millis(20)));

        let (tx, rx) = mpsc::channel::<OutboundFrame>();
        let socket = Socket::new(peer.clone(), tx);
        // Register the connection under its source IP up front so an inbound
        // signal can find a socket for an as-yet-unauthenticated peer. This
        // entry MUST be torn down on disconnect even when the connection never
        // authenticates (see the cleanup below) — otherwise every probe or
        // pre-auth client leaks a stale `Arc<Socket>` for the life of the node.
        let peer_key = socket.peer_ip();
        if !peer_key.is_empty() {
            self.sockets.insert(peer_key.clone(), socket.clone());
        }

        // Local frame queue + back-pressure flag. The protocol requires that
        // we wait for the client's ACK byte (`0x01`) before sending the next
        // frame, otherwise old clients drop frames silently.
        let mut buffered: VecDeque<Vec<u8>> = VecDeque::new();
        let mut ack_ready = true;
        let mut shutting_down = false;
        let mut shutdown_requested = false;
        // Keepalive: send a WS Ping every 30 s when the connection is idle.
        const KEEPALIVE_IDLE: Duration = Duration::from_secs(30);
        let mut last_activity = Instant::now();

        'io: loop {
            // 1) Pull any newly queued outbound frames onto the local FIFO.
            loop {
                match rx.try_recv() {
                    Ok(OutboundFrame::Body(b)) => buffered.push_back(b),
                    Ok(OutboundFrame::Shutdown) => {
                        shutdown_requested = true;
                    }
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => {
                        shutdown_requested = true;
                        break;
                    }
                }
            }

            // Flush the queue first before honouring a shutdown request — the
            // creature's final response should still reach the client.
            if shutdown_requested && buffered.is_empty() {
                shutting_down = true;
            }

            // 2) Push the next frame if we're allowed to.
            if ack_ready {
                if let Some(frame) = buffered.front() {
                    // Update frames (tag 0x01) are fire-and-forget push
                    // notifications; the client never ACKs them. Response
                    // frames (tag 0x02) require an ACK before the next send.
                    let is_update = frame.first().copied() == Some(0x01);
                    let mut msg = Vec::with_capacity(4 + frame.len());
                    msg.extend_from_slice(&(frame.len() as u32).to_be_bytes());
                    msg.extend_from_slice(frame);
                    match ws.send(Message::Binary(msg.into())) {
                        Ok(()) => {
                            last_activity = Instant::now();
                            buffered.pop_front();
                            if !is_update {
                                // Wait for client ACK before sending the next frame.
                                ack_ready = false;
                            }
                            // Update frames: ack_ready stays true, next frame
                            // can be sent immediately on the next iteration.
                        }
                        Err(_) => break 'io,
                    }
                } else if last_activity.elapsed() >= KEEPALIVE_IDLE {
                    // Idle keepalive: WebSocket Ping so the OS detects
                    // half-open connections and clients can implement pong.
                    match ws.send(Message::Ping(vec![].into())) {
                        Ok(()) => last_activity = Instant::now(),
                        Err(_) => break 'io,
                    }
                }
            }

            if shutting_down {
                break 'io;
            }

            // 3) Read whatever the client sent (with timeout).
            match ws.read() {
                Ok(Message::Binary(bytes)) => {
                    last_activity = Instant::now();
                    let body = bytes.to_vec();
                    // The Go (gws) client wraps every payload in its own
                    // 4-byte length prefix, mirroring the TCP framing. Strip
                    // it here so downstream handlers see a clean body.
                    let body = if body.len() >= 4 { body[4..].to_vec() } else { body };
                    if body.len() == 1 && body[0] == 0x01 {
                        // Client ACK: the response frame was delivered.
                        // The frame was already removed from buffered at send
                        // time; just re-enable sending.
                        ack_ready = true;
                    } else {
                        self.process_inbound(&socket, body);
                    }
                }
                Ok(Message::Ping(payload)) => {
                    let _ = ws.send(Message::Pong(payload));
                    last_activity = Instant::now();
                }
                Ok(Message::Pong(_)) => {
                    last_activity = Instant::now();
                }
                Ok(Message::Close(_)) => break 'io,
                Ok(_) => {}
                Err(tungstenite::error::Error::Io(io_err))
                    if io_err.kind() == std::io::ErrorKind::WouldBlock
                        || io_err.kind() == std::io::ErrorKind::TimedOut =>
                {
                    // Idle tick; loop back to outbound drain.
                }
                Err(_) => break 'io,
            }
        }

        // Cleanup: try a polite close, mark the socket disconnected, and
        // schedule the 60s deferred removal so a quick reconnect by the
        // same user doesn't lose its still-registered listener.
        let _ = ws.close(None);
        socket.shutdown();
        // A gateway subscription belongs to this connection alone (a bridge
        // holds one socket, not a user's set of them), so it is dropped
        // immediately rather than after the reconnect grace period — a
        // reconnecting bridge re-subscribes with its token.
        gateway_subs::unsubscribe(&socket.id);
        let user_id = socket.user_id();
        // Always drop the IP-keyed `sockets` entry this connection registered at
        // accept time, before the unauthenticated early-return below. Only the
        // deferred branch used to remove from `sockets`, and it only removed the
        // user-id entry, so unauthenticated probes (and any connection from an IP
        // that never logged in) leaked a stale `Arc<Socket>` for the life of the
        // node. `remove_if` runs the predicate under the shard write lock, so the
        // entry is dropped only when it still points at this socket (a fresh
        // connection from the same IP may already have replaced it).
        if !peer_key.is_empty() {
            self.sockets
                .remove_if(&peer_key, |_, current| Arc::ptr_eq(&socket, current));
        }
        if user_id.is_empty() {
            return;
        }
        let trans = self.clone();
        let socket_clone = socket.clone();
        thread::spawn(move || {
            thread::sleep(Duration::from_secs(60));
            if !socket_clone.is_disconnected() {
                return;
            }
            // Drop just this connection from the user's set; tear the listener
            // down only when no live connections remain for the user.
            if let Some(set) = trans.user_sockets.get(&user_id).map(|e| e.value().clone()) {
                set.remove(&socket_clone.id);
                if set.is_empty() {
                    trans
                        .user_sockets
                        .remove_if(&user_id, |_, current| current.is_empty());
                    if !trans.user_sockets.contains_key(&user_id) {
                        let signaler = trans.app.tools().signaler();
                        signaler.listeners().remove(&user_id);
                        // Symmetric with the `join_group` calls in
                        // `attach_user_listener`: once the user has no live
                        // connection, drop its group memberships so the
                        // signaler's group `stores` (and the empty groups left
                        // behind) don't accumulate for the life of the node.
                        signaler.leave_all_groups(&user_id);
                    }
                }
            }
            // Tidy the legacy `sockets` map only if it still points at us.
            // `remove_if` runs the predicate under the shard write lock, so we
            // never hold a read `Ref` across a `remove` on the same shard
            // (which would deadlock its RwLock).
            trans
                .sockets
                .remove_if(&user_id, |_, current| Arc::ptr_eq(&socket_clone, current));
        });
    }
}

impl IWs for Ws {
    fn listen(&self, port: i64, tls_config: Option<TlsConfig>) {
        let trans_self = Arc::new(self.clone_for_listen());
        thread::spawn(move || {
            let cfg_ref = tls_config.as_ref();
            let (listener, server) = match bind_tls(port, cfg_ref) {
                Ok(p) => p,
                Err(e) => {
                    eprintln!("ws listen :{}: {}", port, e);
                    return;
                }
            };
            loop {
                let stream = match accept(&listener, server.as_ref()) {
                    Ok(s) => s,
                    Err(e) => {
                        eprintln!("ws accept: {}", e);
                        continue;
                    }
                };
                let trans = trans_self.clone();
                thread::spawn(move || trans.handle_connection(stream));
            }
        });
    }
}

impl Ws {
    fn clone_for_listen(&self) -> Ws {
        Ws {
            app: self.app.clone(),
            sockets: self.sockets.clone(),
            user_sockets: self.user_sockets.clone(),
        }
    }
}

// Suppress unused warning in some build configs where `WebSocket` is only
// used inside `handle_connection`.
#[allow(dead_code)]
type _PhantomWebSocket<S> = WebSocket<S>;
