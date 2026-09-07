//! `/gateway/*` — the subscription channel for programs that hold a socket
//! open but are not Caspar users.
//!
//! A creature's own VMs reach the node through their runtime's host calls.
//! Something running *beside* a VM — the crewAI bridge inside a Modal sandbox
//! — has neither a Caspar key nor a host ABI: it is an ordinary client
//! connection. It authenticates with a **bearer token its owning creature
//! minted** (`registerBridgeToken`), and that grant is the whole of its
//! authority: which topics it may subscribe to, and which creature its
//! signals are delivered to.
//!
//! ```text
//!   creature ── registerBridgeToken ─► grant (token hash → topics + owner)
//!   bridge   ── /gateway/subscribe ──► receives that topic's updates
//!   bridge   ── /gateway/signal ─────► signals the granting creature
//!   creature ── publishUpdate ───────► every subscriber of the topic
//! ```
//!
//! The token is never stored: only its SHA-256, so a state dump does not hand
//! anybody a working credential. Actions here take [`Guard::default()`] —
//! anonymous — because a bridge cannot sign; the token *is* the identity
//! check, and every one of these bodies performs it before doing anything.

use std::sync::Arc;

use anyhow::{anyhow, Result};
use serde_json::{json, Value};

use crate::core::actor::model::secured::guard::Guard;
use crate::models::action::ISecureAction;
use crate::models::core::ICore;
use crate::models::state::IState;
use crate::models::transaction::ITrx;
use crate::shell::api::packets::gateway::{
    GatewaySignalInput, GatewaySubscribeInput, GatewayUnsubscribeInput,
};
use crate::shell::utils::future::async_once;

use super::util::build_secure_action;

/// State key holding one bridge grant, keyed by the token's hash.
pub fn bridge_grant_key(token_hash: &str) -> String {
    format!("Json::BridgeGrant::{}", token_hash)
}

/// State link recording which creature owns a topic. A topic has exactly one
/// owner: without that, any creature could publish into another's bridge.
pub fn bridge_topic_owner_key(topic: &str) -> String {
    format!("BridgeTopicOwner::{}", topic)
}

/// Hash a bearer token the way grants are keyed.
pub fn hash_bridge_token(token: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(token.trim().as_bytes());
    hex::encode(hasher.finalize())
}

/// A verified bridge grant.
pub struct BridgeGrant {
    pub creature_id: String,
    pub topics: Vec<String>,
    pub expires_at: i64,
}

/// Read and validate the grant behind a bearer token.
///
/// Returns `None` for an unknown or expired token — the caller must not be
/// able to tell those apart, so both produce the same refusal.
pub fn resolve_bridge_grant(trx: &dyn ITrx, token: &str) -> Option<BridgeGrant> {
    let token = token.trim();
    if token.is_empty() {
        return None;
    }
    let hash = hash_bridge_token(token);
    let grant = Value::Object(trx.get_json(&bridge_grant_key(&hash), "grant").ok()?);
    let creature_id = grant["creatureId"].as_str().unwrap_or("").trim().to_string();
    if creature_id.is_empty() {
        return None;
    }
    let expires_at = grant["expiresAt"].as_i64().unwrap_or(0);
    if expires_at > 0 && chrono::Utc::now().timestamp_millis() > expires_at {
        return None;
    }
    let topics: Vec<String> = grant["topics"]
        .as_array()
        .map(|items| {
            items
                .iter()
                .filter_map(|v| v.as_str().map(|s| s.trim().to_string()))
                .filter(|s| !s.is_empty())
                .collect()
        })
        .unwrap_or_default();
    Some(BridgeGrant {
        creature_id,
        topics,
        expires_at,
    })
}

/// `/gateway/subscribe` — bind this connection to the topics a token grants.
///
/// The action verifies the token and answers with the granted topics plus a
/// subscription id; the *transport* binds the socket, because only it can
/// write to the connection. That split is why this returns
/// `gatewaySubscribe` in its payload: the WS driver looks for it after a
/// successful action and attaches the sink.
fn subscribe(app: Arc<dyn ICore>) -> Arc<dyn ISecureAction> {
    build_secure_action::<GatewaySubscribeInput, _>(
        app,
        "/gateway/subscribe",
        Guard::default(),
        move |state: Arc<dyn IState>, input: GatewaySubscribeInput| -> Result<Value> {
            let trx = state.trx();
            let Some(grant) = resolve_bridge_grant(&*trx, &input.token) else {
                return Err(anyhow!("invalid or expired bridge token"));
            };

            // A request for specific topics can only ever narrow the grant.
            let requested: Vec<String> = input
                .topics
                .iter()
                .map(|t| t.trim().to_string())
                .filter(|t| !t.is_empty())
                .collect();
            let topics: Vec<String> = if requested.is_empty() {
                grant.topics.clone()
            } else {
                requested
                    .into_iter()
                    .filter(|t| grant.topics.iter().any(|g| g == t))
                    .collect()
            };
            if topics.is_empty() {
                return Err(anyhow!("token grants none of the requested topics"));
            }

            Ok(json!({
                "ok": true,
                // Consumed by the transport, which owns the socket.
                "gatewaySubscribe": {
                    "topics": topics,
                    "creatureId": grant.creature_id,
                },
                "topics": topics,
                "creatureId": grant.creature_id,
                "expiresAt": grant.expires_at,
            }))
        },
    )
}

/// `/gateway/unsubscribe` — stop receiving updates on this connection.
fn unsubscribe(app: Arc<dyn ICore>) -> Arc<dyn ISecureAction> {
    build_secure_action::<GatewayUnsubscribeInput, _>(
        app,
        "/gateway/unsubscribe",
        Guard::default(),
        move |state: Arc<dyn IState>, input: GatewayUnsubscribeInput| -> Result<Value> {
            let trx = state.trx();
            if resolve_bridge_grant(&*trx, &input.token).is_none() {
                return Err(anyhow!("invalid or expired bridge token"));
            }
            Ok(json!({"ok": true, "gatewayUnsubscribe": true}))
        },
    )
}

/// `/gateway/signal` — the inbound direction: a bridge asks its owning
/// creature to do something.
///
/// The creature is **not** taken from the request. It is the one recorded in
/// the grant, so a token can only ever reach the creature that minted it —
/// a bridge cannot address the rest of the platform. The payload travels as
/// an ordinary `creatures/signal`, tagged with the bridge's topic so the
/// creature knows which of its bridges is calling.
fn signal(app: Arc<dyn ICore>) -> Arc<dyn ISecureAction> {
    let app_for_handler = app.clone();
    build_secure_action::<GatewaySignalInput, _>(
        app,
        "/gateway/signal",
        Guard::default(),
        move |state: Arc<dyn IState>, input: GatewaySignalInput| -> Result<Value> {
            let trx = state.trx();
            let Some(grant) = resolve_bridge_grant(&*trx, &input.token) else {
                return Err(anyhow!("invalid or expired bridge token"));
            };
            let topic = input.topic.trim().to_string();
            if !topic.is_empty() && !grant.topics.iter().any(|t| t == &topic) {
                return Err(anyhow!("token does not grant this topic"));
            }
            let action = input.action.trim().to_string();
            if action.is_empty() {
                return Err(anyhow!("action is required"));
            }

            // Shaped like the envelope creatures already unwrap
            // (`unwrapSignal`), with the bridge's provenance attached so the
            // creature can tell a bridge call from a user's.
            let payload = json!({
                "action": action,
                "correlationId": input.correlation_id,
                "payload": input.payload,
                "bridge": {
                    "topic": topic,
                    "creatureId": grant.creature_id,
                },
            });
            let packet = json!({
                "action": "single",
                "entityId": "main",
                "data": json!({
                    "correlationId": input.correlation_id,
                    "payload": payload.to_string(),
                })
                .to_string(),
            });

            let creature_id = grant.creature_id.clone();
            let app_async = app_for_handler.clone();
            let _ = async_once(move || {
                app_async.tools().signaler().signal_user(
                    "creatures/signal",
                    &creature_id,
                    packet,
                    true,
                );
            });

            Ok(json!({
                "ok": true,
                "creatureId": grant.creature_id,
                "correlationId": input.correlation_id,
            }))
        },
    )
}

pub fn install(app: Arc<dyn ICore>) {
    let actor = app.actor();
    let handlers: Vec<Arc<dyn ISecureAction>> = vec![
        subscribe(app.clone()),
        unsubscribe(app.clone()),
        signal(app.clone()),
    ];
    for h in handlers {
        actor.inject_secure_action(h);
    }
}
