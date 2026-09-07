//! The [`VmPlugin`] trait — the full contract a VM type implements.
//!
//! All methods speak JSON packets (the VMM's native wire shape), so the
//! interface stays stable while individual runtimes evolve. Reasonable
//! defaults are provided for everything except the two operations every
//! runtime must define: [`VmPlugin::run_vm`] and [`VmPlugin::terminate_vm`].

use serde_json::{json, Map, Value};

use crate::meta::VmPluginMeta;

/// A pluggable VM runtime implementation.
pub trait VmPlugin: Send + Sync {
    /// Static descriptor of this runtime (parsed `vm.config.json`).
    fn meta(&self) -> &VmPluginMeta;

    /// One-time hook invoked right after the plugin is registered.
    fn init(&self) {}

    // ── Core lifecycle ────────────────────────────────────────────────────

    /// Launch (or resume) a VM for the given packet.
    fn run_vm(&self, packet: &Value) -> Result<Value, String>;

    /// Stop a VM (suspend by default; runtimes may honour `purge`).
    fn terminate_vm(&self, packet: &Value) -> Result<Value, String>;

    /// Permanently destroy a VM and everything it owns.
    ///
    /// Where [`VmPlugin::terminate_vm`] suspends (the instance can be resumed
    /// by a later `run_vm`), delete is final: the container/microVM/sandbox is
    /// removed, its persistent volume is dropped, and no state remains for a
    /// resume to find. The default implementation is the one every runtime
    /// already supports — a terminate carrying `purge`/`delete`, which the
    /// runtimes that distinguish the two honour by removing rather than
    /// stopping. Runtimes with resources a purge does not reach (a cloud
    /// sandbox, a remote volume, a built image) override this.
    fn delete_vm(&self, packet: &Value) -> Result<Value, String> {
        let mut purge = packet.clone();
        if let Some(obj) = purge.as_object_mut() {
            obj.insert("purge".to_string(), Value::Bool(true));
            obj.insert("delete".to_string(), Value::Bool(true));
        }
        let terminated = self.terminate_vm(&purge)?;
        Ok(json!({
            "ok": true,
            "runtime": self.meta().key,
            "deleted": true,
            "vmId": packet["vmId"].as_str().unwrap_or("main"),
            "terminate": terminated,
        }))
    }

    /// Inspect a VM without mutating it.
    ///
    /// Runtimes with an external process/container should override this and
    /// return at least `{ status, running }`. The default keeps the program API
    /// runtime-neutral while allowing runtimes without an inspect primitive to
    /// report that their state is unknown.
    fn status_vm(&self, packet: &Value) -> Result<Value, String> {
        let _ = packet;
        Ok(json!({"status": "unknown", "running": false}))
    }

    /// Execute a command inside a running VM.
    fn exec_vm(&self, packet: &Value) -> Result<Value, String> {
        let _ = packet;
        Err(format!(
            "exec is not supported for the {} runtime",
            self.meta().key
        ))
    }

    /// Copy a file into a running VM.
    fn copy_to_vm(&self, packet: &Value) -> Result<Value, String> {
        let _ = packet;
        Err(format!(
            "copy_to is not implemented yet for the {} runtime",
            self.meta().key
        ))
    }

    /// Copy a file out of a running VM.
    fn copy_from_vm(&self, packet: &Value) -> Result<Value, String> {
        let _ = packet;
        Err(format!(
            "copy_from is not implemented yet for the {} runtime",
            self.meta().key
        ))
    }

    /// The public endpoints a running VM is reachable on, if any.
    ///
    /// Runtimes that publish a VM's ports somewhere reachable — a cloud
    /// sandbox's tunnels, say — answer with them here, so a creature can hand
    /// a person a URL for the thing running inside its own VM (a desktop, a
    /// preview server) without the node inventing a scheme for it. Runtimes
    /// with nothing public return an empty list rather than an error: having
    /// no public endpoint is an ordinary state, not a failure.
    ///
    /// Returns `{ ok, endpoints: [{ containerPort, url, host, port }] }`.
    fn vm_endpoints(&self, packet: &Value) -> Result<Value, String> {
        let _ = packet;
        Ok(json!({
            "ok": true,
            "runtime": self.meta().key,
            "endpoints": [],
        }))
    }

    /// Build the deployable image/module for an entity of this runtime.
    fn build_image(&self, packet: &Value) -> Result<Value, String> {
        let _ = packet;
        Ok(json!({"ok": true, "runtime": self.meta().key, "build": "noop"}))
    }

    // ── Aliased lifecycle verbs (kept for controller-level API parity) ────

    fn create(&self, packet: &Value) -> Result<Value, String> {
        self.run_vm(packet)
    }
    fn start(&self, packet: &Value) -> Result<Value, String> {
        self.run_vm(packet)
    }
    fn stop(&self, packet: &Value) -> Result<Value, String> {
        self.terminate_vm(packet)
    }
    fn resume(&self, packet: &Value) -> Result<Value, String> {
        self.run_vm(packet)
    }
    fn pause(&self, packet: &Value) -> Result<Value, String> {
        self.terminate_vm(packet)
    }
    fn destroy(&self, packet: &Value) -> Result<Value, String> {
        self.delete_vm(packet)
    }

    // ── Snapshot restore ──────────────────────────────────────────────────

    /// Restore one previously-running VM from a node snapshot entry.
    /// Restorable runtimes relaunch the VM; others acknowledge and skip.
    fn restore(&self, snapshot_entry: &Value) -> Result<Value, String> {
        if self.meta().restorable {
            self.run_vm(snapshot_entry)
        } else {
            Ok(json!({"ok": true, "runtime": self.meta().key, "skipped": true}))
        }
    }

    // ── Runtime resolution ────────────────────────────────────────────────

    /// Whether this plugin claims a run request given the packet's runtime
    /// hints and the resolved artifact path.
    fn detect(&self, runtime_hint: &str, artifact_path: &str) -> bool {
        self.meta().matches_key(runtime_hint) || self.meta().matches_artifact(artifact_path)
    }

    /// Resolve a live VM instance name (e.g. a container name) from a source
    /// IP for gateway connection identification.
    fn identify_instance_by_ip(&self, ip: &str) -> Option<String> {
        let _ = ip;
        None
    }

    // ── Program-shell integration plans ───────────────────────────────────
    //
    // The node's program API (deploy / runEntity / stopEntity) never encodes
    // per-runtime behaviour. Instead it asks the plugin for a *plan* — plain
    // JSON describing the packet to send and the state links to write/read —
    // and executes that plan inside its own transaction.

    /// Plan a standalone `runVm` for a deployed program entity.
    ///
    /// `ctx`: `{ machineId, programId, entityId, vmId, resources, params }`.
    /// Returns `{ input: {..runVm input..}, links: [[key, value], ...] }`.
    fn plan_run_entity(&self, ctx: &Value) -> Result<Value, String> {
        let params = ctx["params"].clone();
        let data = serde_json::to_string(&params).unwrap_or_else(|_| "{}".to_string());
        Ok(json!({
            "input": {
                "runtime": self.meta().key,
                "machineId": ctx["machineId"],
                "entityId": ctx["entityId"],
                "standalone": true,
                "vmId": ctx["vmId"],
                "resources": ctx["resources"],
                "data": data,
            },
            "links": [],
        }))
    }

    /// Plan a `terminateVm` for a running program entity.
    ///
    /// `ctx`: `{ machineId, programId, entityId, vmId }`.
    /// Returns `{ input: {..terminateVm input..},
    ///            links: [{field, key, required}, ...] }` where each `links`
    /// entry asks the caller to read the state link `key` and place its value
    /// into `input[field]` (failing when `required` and the link is empty).
    fn plan_stop_entity(&self, ctx: &Value) -> Result<Value, String> {
        Ok(json!({
            "input": {
                "runtime": self.meta().key,
                "machineId": ctx["machineId"],
                "entityId": ctx["entityId"],
                "vmId": ctx["vmId"],
            },
            "links": [],
        }))
    }

    /// Plan a `deleteVm` for a deployed program entity.
    ///
    /// `ctx`: `{ machineId, programId, entityId, vmId }`. Shaped exactly like
    /// [`VmPlugin::plan_stop_entity`] — same `{input, links}` contract, so the
    /// program API resolves the same per-runtime state links (a recorded
    /// container name, a sandbox id) before dispatching. The default derives
    /// the plan from the stop plan and re-points it at the delete packet, so a
    /// runtime that overrode only `plan_stop_entity` still deletes correctly.
    fn plan_delete_entity(&self, ctx: &Value) -> Result<Value, String> {
        let mut plan = self.plan_stop_entity(ctx)?;
        if let Some(input) = plan["input"].as_object_mut() {
            input.insert("purge".to_string(), Value::Bool(true));
            input.insert("delete".to_string(), Value::Bool(true));
            input.insert(
                "programId".to_string(),
                ctx["programId"].clone(),
            );
        }
        Ok(plan)
    }

    /// Translate a host-call `terminateVm` input into the typed terminate
    /// packet dispatched to the packet router.
    fn build_terminate_request(&self, input: &Value) -> Result<Value, String> {
        let vm_id = input["vmId"].as_str().unwrap_or("").trim();
        let vm_id = if vm_id.is_empty() { "main" } else { vm_id };
        Ok(json!({
            "type": "terminateVm",
            "runtime": self.meta().key,
            "machineId": input["machineId"].as_str().unwrap_or(""),
            "vmId": vm_id,
        }))
    }

    /// Translate a host-call `deleteVm` input into the typed delete packet
    /// dispatched to the packet router.
    ///
    /// A delete always names one concrete instance: unlike terminate, there is
    /// no "stop whatever this machine is running" fallback, because deleting
    /// by machine alone would destroy instances the caller never named.
    fn build_delete_request(&self, input: &Value) -> Result<Value, String> {
        let vm_id = input["vmId"].as_str().unwrap_or("").trim();
        if vm_id.is_empty() {
            return Err(format!(
                "deleteVm requires a vmId for the {} runtime",
                self.meta().key
            ));
        }
        let mut packet = self.build_terminate_request(input)?;
        if let Some(obj) = packet.as_object_mut() {
            obj.insert("type".to_string(), Value::String("deleteVm".to_string()));
            obj.insert("purge".to_string(), Value::Bool(true));
            obj.insert("delete".to_string(), Value::Bool(true));
        }
        Ok(packet)
    }

    // ── Inbound HTTP forwarding ───────────────────────────────────────────
    //
    // The VMM exposes an HTTP ingress at
    //   `{node instance url}/{creatureId}/{programId}/{entityId}/{vmId}/{path…}`
    // It strips the four identity segments, packages the remaining request, and
    // calls [`VmPlugin::forward_http`] on the entity's runtime plugin. `vmId`
    // names the specific VM instance the request is forwarded to.

    /// Forward an inbound HTTP request to the entity's VM.
    ///
    /// `packet`:
    /// `{ creatureId, programId, entityId, machineId, vmId, containerName,
    ///    method, path, query, headers: {..}, bodyBase64 }`
    ///
    /// where `path` is the remainder of the request URL *after* the
    /// `creatureId/programId/entityId` prefix, `query` the raw query string,
    /// and `bodyBase64` the base64-encoded request body.
    ///
    /// The response is `{ ok, status, headers: {..}, body | bodyBase64 }`.
    ///
    /// The default implementation is the generic fallback every runtime
    /// supports: it packages the request into a `creatures/signal` addressed
    /// to the entity and hands it to the node's signalling API, so the VM
    /// handles the request on its next run. Because signalling is
    /// asynchronous the response is a `202 Accepted` acknowledgement.
    /// Runtimes that keep a long-lived HTTP server *inside* the VM (e.g.
    /// docker) override this to proxy the request straight to that server and
    /// return its real HTTP response.
    fn forward_http(&self, packet: &Value) -> Result<Value, String> {
        forward_http_via_signal(packet)
    }

    // ── Optional capabilities ─────────────────────────────────────────────

    /// Verify a program execution proof (provable runtimes only).
    /// Packet: `{ masmPath, inputs, outputs, proof }`.
    fn verify_program_execution(&self, packet: &Value) -> Result<Value, String> {
        let _ = packet;
        Err(format!(
            "program execution verification is not supported by the {} runtime",
            self.meta().key
        ))
    }
}

/// The generic HTTP-forwarding fallback shared by every runtime that does not
/// run a long-lived HTTP server inside its VM.
///
/// The packaged request is wrapped into a `stores::Send`-shaped
/// `creatures/signal` addressed to the entity's program and delivered through
/// the node's signalling API (`signalUser`). The node's per-machine signal
/// listener picks it up and runs the VM for the target entity, which handles
/// the request as an ordinary signal. Delivery is asynchronous, so the caller
/// receives a `202 Accepted` acknowledgement rather than the VM's own output.
pub fn forward_http_via_signal(packet: &Value) -> Result<Value, String> {
    let host = crate::host::host_or_err()?;

    let program_id = packet["programId"].as_str().unwrap_or("").trim().to_string();
    if program_id.is_empty() {
        return Err("forward_http requires a programId".to_string());
    }
    let entity_id = packet["entityId"].as_str().unwrap_or("").trim().to_string();

    // The packaged HTTP request delivered to the VM as the signal payload.
    let http = json!({
        "kind": "httpRequest",
        "creatureId": packet["creatureId"].as_str().unwrap_or(""),
        "programId": program_id,
        "entityId": entity_id,
        "method": packet["method"].as_str().unwrap_or("GET"),
        "path": packet["path"].as_str().unwrap_or("/"),
        "query": packet["query"].as_str().unwrap_or(""),
        "headers": packet["headers"].clone(),
        "bodyBase64": packet["bodyBase64"].as_str().unwrap_or(""),
    });

    // `stores::Send`-shaped signal the VMM's per-machine listener understands:
    // `action: "single"` + the target entity id, with the request in `data`.
    let signal = json!({
        "action": "single",
        "entityId": entity_id,
        "data": http.to_string(),
    });

    let call = json!({
        "op": "signalUser",
        "programId": program_id,
        "input": {
            "key": "creatures/signal",
            "userId": program_id,
            "packet": signal.to_string(),
        }
    });

    let raw = host.unified_host_call(&call);
    let acked = serde_json::from_str::<Value>(&raw)
        .ok()
        .and_then(|v| v["ok"].as_bool())
        .unwrap_or(false);

    let body = json!({
        "ok": acked,
        "forwarded": "signal",
        "programId": program_id,
        "entityId": entity_id,
    })
    .to_string();

    Ok(json!({
        "ok": acked,
        "status": if acked { 202 } else { 502 },
        "headers": { "content-type": "application/json" },
        "body": body,
    }))
}

/// Convenience: merge extra fields into a plan `input` object.
pub fn merge_input(base: &mut Value, extra: Map<String, Value>) {
    if let Some(obj) = base.as_object_mut() {
        for (k, v) in extra {
            obj.insert(k, v);
        }
    }
}
