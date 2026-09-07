//! Proxy entities — non-runnable creature program entities that forward
//! signals.
//!
//! A developer can deploy an entity with `entityType: "proxy"`. Such an
//! entity has no runnable module; it only holds a data file plus a target
//! descriptor (another program's entity, possibly of another creature). When
//! the proxy entity receives a signal, the node:
//!
//! 1. attaches the stored data file's content to the received payload
//!    (under the configured attach field),
//! 2. stamps a correlation id on the packet and records who sent it,
//! 3. forwards the repackaged signal to the target entity.
//!
//! When the target eventually signals back carrying the same correlation id
//! (addressed to the proxy's program), the node resolves the recorded
//! correlation and delivers the response to the original sender — with the
//! proxy entity's identity as the response sender, so the requester only
//! ever observes the proxy.
//!
//! **Streaming.** A correlation is not necessarily one message. A target may
//! send many responses on the same correlation id: every response marked as a
//! non-terminal *stream chunk* (`stream: true`, `final: false`, or a `kind`
//! ending in `/step`) is relayed to the original sender and the correlation is
//! *kept alive* (its expiry window refreshed) so more can follow; the record
//! is consumed only when a terminal message arrives. Responders that send a
//! single terminal message keep the original one-shot behavior unchanged.
//!
//! This is the substrate for "agent" deployments: an AI-agent skill file is
//! deployed as a proxy entity targeting the platform's agent backbone; every
//! request through the proxy reaches that backbone with the skill attached
//! (used as the session's system instruction), and the backbone streams its
//! trajectory (thoughts, tool steps) plus the final result back through the
//! proxy to the requester on one correlation.

use std::fs;
use std::sync::{Arc, Mutex};

use serde_json::{json, Map, Value};
use uuid::Uuid;

use crate::models::core::ICore;
use crate::models::transaction::ITrx;
use crate::shell::api::model::{Creature, Program};
use crate::shell::api::packets::stores::Send as StoresSend;

/// The pseudo-runtime key a proxy entity is deployed under. It is not a VM
/// runtime: nothing ever runs for a proxy entity.
pub const PROXY_RUNTIME_KEY: &str = "proxy";

/// Default payload field the proxy's data file content is attached under.
pub const DEFAULT_ATTACH_FIELD: &str = "attachment";

/// Default lifetime of a correlation record (ms). A target that never
/// responds must not leak its correlation record forever: after this window
/// the record is consumed by the reaper (or by a late response, which is
/// then dropped). Sized to comfortably outlast a long streaming run (e.g. a
/// an agent's wall-clock budget); each streamed chunk also refreshes the
/// window, so an actively-streaming correlation never expires mid-run.
pub const DEFAULT_CORRELATION_TTL_MS: i64 = 20 * 60 * 1000;

/// Floor for a per-entity configured TTL, so a typo cannot make records
/// expire before the target has any chance to answer.
const MIN_CORRELATION_TTL_MS: i64 = 1_000;

fn correlation_key(correlation_id: &str) -> String {
    format!("Json::ProxyCorrelation::{}", correlation_id)
}

/// Expiry index: one link per live correlation
/// (`ProxyCorrExpiry::{correlationId}` → expiry timestamp ms) so the reaper
/// can enumerate records without scanning the JSON keyspace.
fn correlation_expiry_link(correlation_id: &str) -> String {
    format!("ProxyCorrExpiry::{}", correlation_id)
}

const CORRELATION_EXPIRY_PREFIX: &str = "ProxyCorrExpiry::";

fn config_key(program_id: &str, entity_id: &str) -> String {
    format!("Json::ProxyEntity::{}::{}", program_id, entity_id)
}

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

/// Delete a correlation record and its expiry-index link inside an open
/// transaction.
fn delete_correlation(trx: &dyn ITrx, correlation_id: &str) {
    trx.del_json(&correlation_key(correlation_id), "record");
    trx.del_key(&format!("link::{}", correlation_expiry_link(correlation_id)));
}

/// Normalized proxy-entity configuration.
#[derive(Debug, Clone, Default)]
pub struct ProxyConfig {
    pub target_program_id: String,
    pub target_entity_id: String,
    pub attach_field: String,
    /// Correlation-record lifetime in ms; `0` means the node default.
    pub correlation_ttl_ms: i64,
    /// Static fields the proxy deep-merges into every forwarded payload,
    /// stored in the entity's (private) config — never in public metadata.
    ///
    /// This is where an agent's own configuration lives: e.g. its LLM provider
    /// override `{ "config": { "llm": { "provider", "model", "apiKey" } } }`.
    /// The merge wins over anything the caller sent, so the agent always runs
    /// with its stored key and a client cannot forge another provider. Because
    /// it is held in the internal proxy config (not the `public.decillion`
    /// descriptor), the API key is never exposed by a metadata read.
    pub inject: Value,
}

impl ProxyConfig {
    pub fn to_value(&self) -> Value {
        let mut v = json!({
            "targetProgramId": self.target_program_id,
            "targetEntityId": self.target_entity_id,
            "attachField": self.attach_field,
            "correlationTtlMs": self.correlation_ttl_ms,
        });
        if self.inject.is_object() {
            v.as_object_mut()
                .unwrap()
                .insert("inject".to_string(), self.inject.clone());
        }
        v
    }

    pub fn from_value(v: &Value) -> ProxyConfig {
        let s = |k: &str| v.get(k).and_then(Value::as_str).unwrap_or("").to_string();
        let mut cfg = ProxyConfig {
            target_program_id: s("targetProgramId"),
            target_entity_id: s("targetEntityId"),
            attach_field: s("attachField"),
            correlation_ttl_ms: v
                .get("correlationTtlMs")
                .and_then(value_as_ms)
                .unwrap_or(0),
            inject: v
                .get("inject")
                .filter(|x| x.is_object())
                .cloned()
                .unwrap_or(Value::Null),
        };
        if cfg.attach_field.is_empty() {
            cfg.attach_field = DEFAULT_ATTACH_FIELD.to_string();
        }
        cfg
    }

    /// The effective correlation-record lifetime for this proxy entity.
    pub fn effective_correlation_ttl_ms(&self) -> i64 {
        if self.correlation_ttl_ms <= 0 {
            DEFAULT_CORRELATION_TTL_MS
        } else {
            self.correlation_ttl_ms.max(MIN_CORRELATION_TTL_MS)
        }
    }
}

fn value_as_ms(v: &Value) -> Option<i64> {
    match v {
        Value::Number(n) => n.as_i64().or_else(|| n.as_f64().map(|f| f as i64)),
        Value::String(s) => s.trim().parse::<i64>().ok(),
        _ => None,
    }
}

/// Whether a proxied response is a non-terminal *stream chunk*. The proxy keeps
/// the correlation alive for these and only consumes it on the terminal
/// message, so a target can stream many responses on one correlation.
///
/// A responder opts into streaming by marking a chunk with any of: `stream:
/// true`, `final: false`, or a `kind` ending in `/step` (or `/stream`). The
/// marker is read from the packet top-level or from its `data` JSON string (a
/// docker creature's raw result travels whole under `data`). Anything else is
/// terminal, preserving the original one-shot behavior for non-streaming
/// responders.
fn is_streaming_chunk(value: &Value) -> bool {
    fn from_obj(v: &Value) -> Option<bool> {
        if v.get("stream").and_then(Value::as_bool) == Some(true) {
            return Some(true);
        }
        if let Some(f) = v.get("final").and_then(Value::as_bool) {
            return Some(!f);
        }
        if let Some(kind) = v.get("kind").and_then(Value::as_str) {
            return Some(kind.ends_with("/step") || kind.ends_with("/stream"));
        }
        None
    }
    if let Some(b) = from_obj(value) {
        return b;
    }
    if let Some(data) = value.get("data").and_then(Value::as_str) {
        if let Ok(parsed) = serde_json::from_str::<Value>(data) {
            if let Some(b) = from_obj(&parsed) {
                return b;
            }
        }
    }
    false
}

/// Extract the proxy configuration from a deploy request's metadata. Accepts
/// either a nested `"proxy": {...}` object or flat `proxyTarget*` keys.
pub fn config_from_metadata<M>(get: M) -> Result<ProxyConfig, String>
where
    M: Fn(&str) -> Option<Value>,
{
    let nested = get("proxy").unwrap_or(Value::Null);
    let pick = |nested_key: &str, flat_key: &str| -> String {
        nested
            .get(nested_key)
            .and_then(Value::as_str)
            .map(str::to_string)
            .filter(|s| !s.is_empty())
            .or_else(|| {
                get(flat_key)
                    .and_then(|v| v.as_str().map(str::to_string))
                    .filter(|s| !s.is_empty())
            })
            .unwrap_or_default()
    };
    let mut target_program_id = pick("targetProgramId", "proxyTargetProgramId");
    if target_program_id.is_empty() {
        target_program_id = pick("targetMachineId", "proxyTargetMachineId");
    }
    if target_program_id.is_empty() {
        target_program_id = pick("targetCreatureId", "proxyTargetCreatureId");
    }
    if target_program_id.is_empty() {
        return Err(
            "proxy entity deploy requires metadata.proxy.targetProgramId (the program/creature the proxy forwards to)"
                .to_string(),
        );
    }
    let target_entity_id = pick("targetEntityId", "proxyTargetEntityId");
    let mut attach_field = pick("attachField", "proxyAttachField");
    if attach_field.is_empty() {
        attach_field = DEFAULT_ATTACH_FIELD.to_string();
    }
    let correlation_ttl_ms = nested
        .get("correlationTtlMs")
        .and_then(value_as_ms)
        .or_else(|| get("proxyCorrelationTtlMs").as_ref().and_then(value_as_ms))
        .unwrap_or(0);
    // Static payload injection (e.g. the agent's own `config.llm`). Accept it
    // nested under `proxy.inject` or as a flat `proxyInject` key; keep only an
    // object so a stray scalar cannot corrupt the forwarded payload.
    let inject = nested
        .get("inject")
        .cloned()
        .or_else(|| get("proxyInject"))
        .filter(|x| x.is_object())
        .unwrap_or(Value::Null);
    Ok(ProxyConfig {
        target_program_id,
        target_entity_id,
        attach_field,
        correlation_ttl_ms,
        inject,
    })
}

/// Deep-merge `src` into `dst`: nested objects merge recursively, and any
/// non-object value in `src` overwrites `dst`. Used to overlay the proxy's
/// injected config onto a forwarded payload so the agent's stored fields (its
/// LLM key) win over anything the caller supplied.
fn deep_merge(dst: &mut Value, src: &Value) {
    match (dst, src) {
        (Value::Object(d), Value::Object(s)) => {
            for (k, v) in s {
                deep_merge(d.entry(k.clone()).or_insert(Value::Null), v);
            }
        }
        (d, s) => {
            *d = s.clone();
        }
    }
}

/// Record a deployed proxy entity inside an open state transaction: the
/// entity record itself, the `vmEntityType`/`vmEntityPath` links (path points
/// at the stored data file) and the proxy target configuration.
pub fn record_proxy_entity(
    trx: &dyn ITrx,
    program_id: &str,
    entity_id: &str,
    data_path: &str,
    config: &ProxyConfig,
) {
    crate::shell::api::model::Entity {
        program_id: program_id.to_string(),
        entity_id: entity_id.to_string(),
        entity_type: PROXY_RUNTIME_KEY.to_string(),
        image_name: entity_id.to_string(),
    }
    .push(trx);
    trx.put_link(
        &format!("vmEntityType::{}::{}", program_id, entity_id),
        PROXY_RUNTIME_KEY,
    );
    trx.put_link(
        &format!("vmEntityPath::{}::{}", program_id, entity_id),
        data_path,
    );
    let _ = trx.put_json(
        &config_key(program_id, entity_id),
        "config",
        &config.to_value(),
        true,
    );
}

fn read_state<T, F>(app: &Arc<dyn ICore>, default: T, f: F) -> T
where
    T: Clone + Send + 'static,
    F: Fn(&dyn ITrx) -> T + Send + Sync + 'static,
{
    let slot = Arc::new(Mutex::new(default));
    let slot_clone = slot.clone();
    app.modify_state(
        true,
        Box::new(move |trx: &dyn ITrx| {
            *slot_clone.lock().unwrap() = f(trx);
            Ok(())
        }),
    );
    let out = slot.lock().unwrap().clone();
    out
}

/// The identity a proxied packet travels under: the proxy's program, with the
/// owning machine creature's username when available.
fn proxy_identity(app: &Arc<dyn ICore>, program_id: &str) -> Creature {
    let program_id_owned = program_id.to_string();
    read_state(app, Creature::default(), move |trx| {
        let program = Program {
            id: program_id_owned.clone(),
            ..Default::default()
        }
        .pull(trx);
        let owner = Creature {
            id: program.machine_id.clone(),
            ..Default::default()
        }
        .pull(trx);
        Creature {
            id: program_id_owned.clone(),
            type_name: "machine".to_string(),
            username: if owner.username.is_empty() {
                program_id_owned.clone()
            } else {
                owner.username
            },
            ..Default::default()
        }
    })
}

/// Pull the correlation id out of an incoming `creatures/signal` payload:
/// either a top-level `correlationId` or one embedded in the packet's `data`
/// JSON string.
fn extract_correlation_id(value: &Value) -> String {
    fn from_obj(v: &Value) -> String {
        v.get("correlationId")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string()
    }
    let c = from_obj(value);
    if !c.is_empty() {
        return c;
    }
    if let Some(data) = value.get("data").and_then(Value::as_str) {
        if let Ok(parsed) = serde_json::from_str::<Value>(data) {
            let c = from_obj(&parsed);
            if !c.is_empty() {
                return c;
            }
            // Client convention: the caller's payload travels as a JSON
            // string under `data.payload` — the correlation id the requester
            // wants echoed back lives inside it.
            match parsed.get("payload") {
                Some(Value::String(p)) => {
                    if let Ok(inner) = serde_json::from_str::<Value>(p) {
                        let c = from_obj(&inner);
                        if !c.is_empty() {
                            return c;
                        }
                    }
                }
                Some(obj @ Value::Object(_)) => {
                    let c = from_obj(obj);
                    if !c.is_empty() {
                        return c;
                    }
                }
                _ => {}
            }
        }
    }
    String::new()
}

/// Response direction. If `value` carries a correlation id recorded by one of
/// `listener_machine_id`'s proxy entities, deliver it to the original sender
/// with the proxy's identity as the response sender, drop the correlation
/// record, and return `true`. Returns `false` when the packet is not a
/// proxied response for this machine.
pub fn try_route_proxy_response(
    app: &Arc<dyn ICore>,
    listener_machine_id: &str,
    value: &Value,
) -> bool {
    let correlation_id = extract_correlation_id(value);
    if correlation_id.is_empty() {
        return false;
    }
    let corr_key_owned = correlation_key(&correlation_id);
    let record = read_state(app, Map::new(), move |trx| {
        trx.get_json(&corr_key_owned, "record").unwrap_or_default()
    });
    if record.is_empty() {
        return false;
    }
    let rec = |k: &str| record.get(k).and_then(Value::as_str).unwrap_or("").to_string();
    // Only the proxy that created the correlation may route the response —
    // the same correlation id travelling on the forwarded request must not
    // bounce at the target's own listener.
    if rec("proxyProgramId") != listener_machine_id {
        return false;
    }
    let sender_id = rec("senderId");
    if sender_id.is_empty() {
        return false;
    }
    // A correlation is only valid inside its lifetime window: a late
    // response consumes the stale record but is not delivered — the
    // requester has long since given up on it.
    let expires_at = record.get("expiresAt").and_then(value_as_ms).unwrap_or(0);
    if expires_at > 0 && now_ms() > expires_at {
        let corr_owned = correlation_id.clone();
        app.modify_state(
            false,
            Box::new(move |trx: &dyn ITrx| {
                delete_correlation(trx, &corr_owned);
                Ok(())
            }),
        );
        crate::drivers::vmm::bridge::runtime_io::log(format!(
            "proxy correlation {} expired; dropping late response for {}",
            correlation_id, listener_machine_id
        ));
        return true;
    }
    let proxy_entity_id = rec("proxyEntityId");
    // Preserve the responder's payload; a non-Send-shaped packet (e.g. a raw
    // result object from a docker creature) is carried whole in `data`.
    let data = match value.get("data").and_then(Value::as_str) {
        Some(d) if !d.is_empty() => d.to_string(),
        _ => value.to_string(),
    };
    let response = StoresSend {
        user: proxy_identity(app, listener_machine_id),
        action: "single".to_string(),
        data,
        entity_id: proxy_entity_id,
        correlation_id: correlation_id.clone(),
        ..Default::default()
    };
    // A non-terminal stream chunk keeps the correlation alive (and refreshes its
    // expiry window so a long run cannot lapse mid-stream); only a terminal
    // message consumes the record. Non-streaming responders send a single
    // terminal message, so this preserves the original one-shot behavior.
    if is_streaming_chunk(value) {
        let ttl = record
            .get("ttlMs")
            .and_then(value_as_ms)
            .filter(|v| *v > 0)
            .unwrap_or(DEFAULT_CORRELATION_TTL_MS);
        let new_expires_at = now_ms() + ttl;
        let mut refreshed = record.clone();
        refreshed.insert("expiresAt".to_string(), json!(new_expires_at));
        let corr_owned = correlation_id.clone();
        app.modify_state(
            false,
            Box::new(move |trx: &dyn ITrx| {
                let _ = trx.put_json(
                    &correlation_key(&corr_owned),
                    "record",
                    &Value::Object(refreshed.clone()),
                    true,
                );
                trx.put_link(
                    &correlation_expiry_link(&corr_owned),
                    &new_expires_at.to_string(),
                );
                Ok(())
            }),
        );
    } else {
        // Terminal: the round trip is complete — consume the record.
        let corr_owned = correlation_id.clone();
        app.modify_state(
            false,
            Box::new(move |trx: &dyn ITrx| {
                delete_correlation(trx, &corr_owned);
                Ok(())
            }),
        );
    }
    app.tools().signaler().signal_user(
        "creatures/signal",
        &sender_id,
        serde_json::to_value(&response).unwrap_or(Value::Null),
        true,
    );
    true
}

/// Request direction. If `entity_id` names a proxy entity of `machine_id`,
/// attach the entity's data file to the packet, record the correlation and
/// forward the repackaged signal to the configured target entity. Returns
/// `true` when the signal was consumed by a proxy entity.
pub fn try_forward_through_proxy(
    app: &Arc<dyn ICore>,
    machine_id: &str,
    entity_id: &str,
    value: &Value,
) -> bool {
    if entity_id.is_empty() {
        return false;
    }
    let machine_owned = machine_id.to_string();
    let entity_owned = entity_id.to_string();
    let (entity_type, data_path, config_raw) = read_state(
        app,
        (String::new(), String::new(), Map::new()),
        move |trx| {
            let etype =
                trx.get_link(&format!("vmEntityType::{}::{}", machine_owned, entity_owned));
            if etype != PROXY_RUNTIME_KEY {
                return (etype, String::new(), Map::new());
            }
            let path =
                trx.get_link(&format!("vmEntityPath::{}::{}", machine_owned, entity_owned));
            let cfg = trx
                .get_json(&config_key(&machine_owned, &entity_owned), "config")
                .unwrap_or_default();
            (etype, path, cfg)
        },
    );
    if entity_type != PROXY_RUNTIME_KEY {
        return false;
    }
    let config = ProxyConfig::from_value(&Value::Object(config_raw));
    if config.target_program_id.is_empty() {
        crate::drivers::vmm::bridge::runtime_io::log(format!(
            "proxy entity {}::{} has no target configured; dropping signal",
            machine_id, entity_id
        ));
        return true;
    }
    let send: StoresSend = serde_json::from_value(value.clone()).unwrap_or_default();
    // Reuse the sender's correlation id when provided so the requester can
    // match the response; otherwise mint one for the round trip.
    let correlation_id = if !send.correlation_id.is_empty() {
        send.correlation_id.clone()
    } else {
        let embedded = extract_correlation_id(value);
        if embedded.is_empty() {
            Uuid::new_v4().to_string()
        } else {
            embedded
        }
    };
    let attachment = if data_path.is_empty() {
        String::new()
    } else {
        fs::read_to_string(&data_path).unwrap_or_default()
    };
    // Attach the proxy's data file and routing envelope to the payload.
    let mut payload: Map<String, Value> = match serde_json::from_str::<Value>(&send.data) {
        Ok(Value::Object(o)) => o,
        _ => {
            let mut m = Map::new();
            if !send.data.is_empty() {
                m.insert("data".to_string(), json!(send.data));
            }
            m
        }
    };
    payload.insert(config.attach_field.clone(), json!(attachment));
    // Overlay the proxy's stored config (e.g. the agent's own `config.llm`) so
    // it reaches the backbone and wins over any caller-supplied value — a
    // client prompting a marketplace agent can neither read nor override the
    // agent's stored key.
    if config.inject.is_object() {
        let mut merged = Value::Object(payload);
        deep_merge(&mut merged, &config.inject);
        payload = match merged {
            Value::Object(o) => o,
            _ => Map::new(),
        };
    }
    payload.insert("correlationId".to_string(), json!(correlation_id));
    payload.insert("replyTo".to_string(), json!(machine_id));
    payload.insert("proxyProgramId".to_string(), json!(machine_id));
    payload.insert("proxyEntityId".to_string(), json!(entity_id));
    let created_at = now_ms();
    let expires_at = created_at + config.effective_correlation_ttl_ms();
    let record = json!({
        "senderId": send.user.id,
        "proxyProgramId": machine_id,
        "proxyEntityId": entity_id,
        "targetProgramId": config.target_program_id,
        "targetEntityId": config.target_entity_id,
        "createdAt": created_at,
        "expiresAt": expires_at,
        // Retained so a streamed chunk can refresh the expiry window by the
        // same lifetime the entity was configured with.
        "ttlMs": config.effective_correlation_ttl_ms(),
    });
    let corr_key = correlation_key(&correlation_id);
    let expiry_link = correlation_expiry_link(&correlation_id);
    app.modify_state(
        false,
        Box::new(move |trx: &dyn ITrx| {
            let _ = trx.put_json(&corr_key, "record", &record, true);
            trx.put_link(&expiry_link, &expires_at.to_string());
            Ok(())
        }),
    );
    // Say where this goes. A proxy relay is otherwise completely invisible: the
    // requester sees only silence if the configured target no longer exists (a
    // backbone redeployed under a new program id leaves every proxy pointing at
    // a dead one), and nothing in any log ties the prompt to the id it was
    // actually sent to. One line per relay makes that a grep instead of a
    // deduction.
    crate::drivers::vmm::bridge::runtime_io::log(format!(
        "proxy {}::{} -> {}::{} corr={}",
        machine_id, entity_id, config.target_program_id, config.target_entity_id, correlation_id
    ));
    let forwarded = StoresSend {
        user: proxy_identity(app, machine_id),
        action: "single".to_string(),
        // Carry the originating store through to the backbone untouched: the
        // requester scoped this signal to a space (store), and the agent behind
        // the proxy must see the same space the signal came from.
        store: send.store.clone(),
        data: Value::Object(payload).to_string(),
        is_temp: send.is_temp,
        entity_id: config.target_entity_id.clone(),
        correlation_id,
        ..Default::default()
    };
    app.tools().signaler().signal_user(
        "creatures/signal",
        &config.target_program_id,
        serde_json::to_value(&forwarded).unwrap_or(Value::Null),
        true,
    );
    true
}

/// Drop every correlation record whose lifetime has elapsed. Records are
/// found through the `ProxyCorrExpiry::{id}` link index; a link whose value
/// cannot be parsed is treated as expired so a half-written entry can never
/// survive forever.
pub fn sweep_expired_correlations(app: &Arc<dyn ICore>) {
    let now = now_ms();
    let expired = read_state(app, Vec::<String>::new(), move |trx| {
        let links = trx
            .get_links_list(CORRELATION_EXPIRY_PREFIX, -1, -1, &[])
            .unwrap_or_default();
        let mut out = Vec::new();
        for link in links {
            let corr_id = link
                .strip_prefix(CORRELATION_EXPIRY_PREFIX)
                .unwrap_or(&link)
                .to_string();
            if corr_id.is_empty() {
                continue;
            }
            let expires_at = trx.get_link(&link).trim().parse::<i64>().unwrap_or(0);
            if expires_at <= now {
                out.push(corr_id);
            }
        }
        out
    });
    if expired.is_empty() {
        return;
    }
    let count = expired.len();
    app.modify_state(
        false,
        Box::new(move |trx: &dyn ITrx| {
            for corr_id in &expired {
                delete_correlation(trx, corr_id);
            }
            Ok(())
        }),
    );
    crate::drivers::vmm::bridge::runtime_io::log(format!(
        "proxy correlation reaper dropped {} expired record(s)",
        count
    ));
}

/// Spawn the background reaper that keeps the correlation store bounded even
/// when a target fails silently and never responds.
pub fn start_correlation_reaper(app: Arc<dyn ICore>) {
    std::thread::spawn(move || loop {
        std::thread::sleep(std::time::Duration::from_secs(60));
        sweep_expired_correlations(&app);
    });
}

#[cfg(test)]
mod inject_tests {
    // Exercises the REAL config parsing + deep-merge used by
    // try_forward_through_proxy, proving an agent's stored config.llm (with its
    // API key) is read from entity metadata, survives the stored-config
    // round-trip, and overrides a caller-supplied value while preserving the
    // caller's other fields.
    use super::{config_from_metadata, deep_merge, ProxyConfig};
    use serde_json::{json, Map, Value};

    fn getter(meta: Value) -> impl Fn(&str) -> Option<Value> {
        let m: Map<String, Value> = match meta {
            Value::Object(o) => o,
            _ => Map::new(),
        };
        move |k: &str| m.get(k).cloned()
    }

    #[test]
    fn inject_parsed_roundtripped_and_overrides() {
        let get = getter(json!({
            "proxy": {
                "targetProgramId": "prog_backbone",
                "inject": { "config": { "llm": { "provider": "openai", "model": "gpt-5", "apiKey": "sk-SECRET" } } }
            }
        }));
        let cfg = config_from_metadata(get).expect("config");
        assert_eq!(cfg.inject.pointer("/config/llm/apiKey"), Some(&json!("sk-SECRET")));

        // Stored-config round-trip (record_proxy_entity persists to_value()).
        let restored = ProxyConfig::from_value(&cfg.to_value());
        assert_eq!(restored.inject.pointer("/config/llm/model"), Some(&json!("gpt-5")));

        // Merge onto a payload with caller tools + a forged llm.
        let mut payload = json!({
            "prompt": "hi",
            "config": { "tools": ["caspar__sandbox"], "llm": { "provider": "attacker", "apiKey": "sk-FORGED" } }
        });
        deep_merge(&mut payload, &restored.inject);
        assert_eq!(payload.pointer("/config/llm/apiKey"), Some(&json!("sk-SECRET")), "agent key wins");
        assert_eq!(payload.pointer("/config/llm/provider"), Some(&json!("openai")), "agent provider wins");
        assert_eq!(payload.pointer("/config/tools/0"), Some(&json!("caspar__sandbox")), "caller tools kept");
        assert_eq!(payload.pointer("/prompt"), Some(&json!("hi")), "caller prompt kept");
    }

    #[test]
    fn no_inject_leaves_payload_unchanged() {
        let get = getter(json!({ "proxy": { "targetProgramId": "p" } }));
        let cfg = config_from_metadata(get).expect("config");
        assert!(!cfg.inject.is_object(), "absent inject is not an object");
        let before = json!({ "prompt": "x", "config": { "llm": { "apiKey": "own" } } });
        let mut after = before.clone();
        if cfg.inject.is_object() {
            deep_merge(&mut after, &cfg.inject);
        }
        assert_eq!(after, before);
    }
}
