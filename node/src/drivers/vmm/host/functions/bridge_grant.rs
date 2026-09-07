//! Host ops backing the gateway subscription channel: minting the bearer
//! tokens a bridge authenticates with, and pushing updates to the bridges
//! that hold a socket open.
//!
//! These are the creature's half of `/gateway/*`. A creature that runs a
//! program outside Caspar — the crewAI bridge in a Modal sandbox — mints a
//! token scoped to the topics that program may hear, hands the token to it
//! (through the sandbox, never over a signal), and afterwards pushes updates
//! by topic.
//!
//! Authority is the point:
//!
//! * A token's owning creature is `ctx.program_id` — node-resolved, not a
//!   field the guest supplies — so a creature can only mint tokens that reach
//!   itself.
//! * A topic belongs to the first creature that grants it, and
//!   `publishUpdate` refuses any other. Without that, a creature could push
//!   packets into another creature's bridge just by naming its topic.
//! * The token is stored only as a SHA-256, so reading node state does not
//!   yield a working credential.

use crate::drivers::gateway_subs;
use crate::drivers::vmm::globals::with_global_app;
use crate::drivers::vmm::prelude::*;
use crate::models::transaction::ITrx;
use crate::shell::api::actions::gateway::{
    bridge_grant_key, bridge_topic_owner_key, hash_bridge_token,
};

/// Topics named in a host-call input, trimmed and de-duplicated.
fn requested_topics(input: &JsonValue) -> Vec<String> {
    let mut topics: Vec<String> = input["topics"]
        .as_array()
        .map(|items| {
            items
                .iter()
                .filter_map(|v| v.as_str().map(|s| s.trim().to_string()))
                .filter(|s| !s.is_empty())
                .collect()
        })
        .unwrap_or_default();
    if let Some(single) = input["topic"].as_str() {
        let single = single.trim().to_string();
        if !single.is_empty() {
            topics.push(single);
        }
    }
    topics.sort();
    topics.dedup();
    topics
}

/// `registerBridgeToken` — mint (or replace) the grant behind a bearer token.
pub(crate) fn host_fn_register_bridge_token(
    caller_program_id: &str,
    input: &JsonValue,
) -> String {
    let caller = caller_program_id.trim().to_string();
    if caller.is_empty() {
        return json!({"ok": false, "error": "registerBridgeToken requires an identified caller"})
            .to_string();
    }
    let token = input["token"].as_str().unwrap_or("").trim().to_string();
    if token.len() < 32 {
        return json!({
            "ok": false,
            "error": "bridge token must be at least 32 characters",
        })
        .to_string();
    }
    let topics = requested_topics(input);
    if topics.is_empty() {
        return json!({"ok": false, "error": "at least one topic is required"}).to_string();
    }
    let ttl_secs = input["ttlSecs"].as_i64().unwrap_or(0);
    let expires_at = if ttl_secs > 0 {
        chrono::Utc::now().timestamp_millis() + ttl_secs * 1000
    } else {
        0
    };

    let hash = hash_bridge_token(&token);
    let grant = json!({
        "creatureId": caller,
        "topics": topics,
        "expiresAt": expires_at,
        "createdAt": chrono::Utc::now().timestamp_millis(),
    });

    let caller_for_trx = caller.clone();
    let topics_for_trx = topics.clone();
    let conflict = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
    let conflict_c = conflict.clone();
    let applied = with_global_app(|app| {
        app.modify_state(
            false,
            Box::new(move |trx: &dyn ITrx| {
                // Claim each topic for this creature, refusing one another
                // creature already owns.
                for topic in &topics_for_trx {
                    let key = bridge_topic_owner_key(topic);
                    let owner = trx.get_link(&key);
                    if !owner.is_empty() && owner != caller_for_trx {
                        *conflict_c.lock().unwrap() = topic.clone();
                        return Ok(());
                    }
                }
                for topic in &topics_for_trx {
                    trx.put_link(&bridge_topic_owner_key(topic), &caller_for_trx);
                }
                trx.put_json(&bridge_grant_key(&hash), "grant", &grant, false)?;
                Ok(())
            }),
        );
    })
    .is_some();

    if !applied {
        return json!({"ok": false, "error": "node state is not available"}).to_string();
    }
    let conflict = conflict.lock().unwrap().clone();
    if !conflict.is_empty() {
        return json!({
            "ok": false,
            "error": format!("topic '{}' is owned by another creature", conflict),
        })
        .to_string();
    }

    json!({
        "ok": true,
        "topics": topics,
        "expiresAt": expires_at,
    })
    .to_string()
}

/// `revokeBridgeToken` — drop a grant, and with it every subscription that
/// authenticated through it on the next publish.
pub(crate) fn host_fn_revoke_bridge_token(
    caller_program_id: &str,
    input: &JsonValue,
) -> String {
    let caller = caller_program_id.trim().to_string();
    let token = input["token"].as_str().unwrap_or("").trim().to_string();
    let hash = if token.is_empty() {
        input["tokenHash"].as_str().unwrap_or("").trim().to_string()
    } else {
        hash_bridge_token(&token)
    };
    if hash.is_empty() {
        return json!({"ok": false, "error": "token or tokenHash is required"}).to_string();
    }

    let key = bridge_grant_key(&hash);
    let owner = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
    let owner_c = owner.clone();
    let key_read = key.clone();
    with_global_app(|app| {
        app.modify_state(
            true,
            Box::new(move |trx: &dyn ITrx| {
                if let Ok(grant) = trx.get_json(&key_read, "grant") {
                    if let Some(id) = grant.get("creatureId").and_then(|v| v.as_str()) {
                        *owner_c.lock().unwrap() = id.to_string();
                    }
                }
                Ok(())
            }),
        );
    });
    let owner = owner.lock().unwrap().clone();
    if owner.is_empty() {
        // Revoking an unknown token is a no-op, not an error: a bridge tearing
        // itself down twice must not fail the second time.
        return json!({"ok": true, "revoked": false}).to_string();
    }
    if owner != caller {
        return json!({"ok": false, "error": "you do not own this bridge token"}).to_string();
    }

    with_global_app(|app| {
        app.modify_state(
            false,
            Box::new(move |trx: &dyn ITrx| {
                trx.del_json(&key, "grant");
                Ok(())
            }),
        );
    });
    json!({"ok": true, "revoked": true}).to_string()
}

/// `publishUpdate` — push one packet to every connection subscribed to a
/// topic this creature owns.
pub(crate) fn host_fn_publish_update(caller_program_id: &str, input: &JsonValue) -> String {
    let caller = caller_program_id.trim().to_string();
    if caller.is_empty() {
        return json!({"ok": false, "error": "publishUpdate requires an identified caller"})
            .to_string();
    }
    let topic = input["topic"].as_str().unwrap_or("").trim().to_string();
    if topic.is_empty() {
        return json!({"ok": false, "error": "topic is required"}).to_string();
    }

    let owner_key = bridge_topic_owner_key(&topic);
    let owner = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
    let owner_c = owner.clone();
    with_global_app(|app| {
        app.modify_state(
            true,
            Box::new(move |trx: &dyn ITrx| {
                *owner_c.lock().unwrap() = trx.get_link(&owner_key);
                Ok(())
            }),
        );
    });
    let owner = owner.lock().unwrap().clone();
    if owner.is_empty() {
        return json!({"ok": false, "error": "no bridge grant exists for this topic"})
            .to_string();
    }
    if owner != caller {
        return json!({"ok": false, "error": "you do not own this topic"}).to_string();
    }

    let key = input["key"]
        .as_str()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .unwrap_or("gateway/update")
        .to_string();
    let data = if input["data"].is_null() {
        input["packet"].clone()
    } else {
        input["data"].clone()
    };

    let delivered = gateway_subs::publish(&topic, &key, &data);
    json!({
        "ok": true,
        "topic": topic,
        "delivered": delivered,
    })
    .to_string()
}
