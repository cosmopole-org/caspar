//! Translation of `drivers/vmm/vmm.go`.
//!
//! Top-level `Vmm` struct, ZMQ REQ/REP loop, and the high-level public API
//! (`assign`, `run_vm`, `run_vm_entity`, `terminate_vm`, `build_vm_image`,
//! `close_kvdb`). Per-runtime hostcall handlers live in
//! [`hostcall_entities`](super::hostcall_entities) /
//! [`hostcall_logs`](super::hostcall_logs); the dispatcher lives in
//! [`hostcall_global`](super::hostcall_global).

use crate::drivers::vmm::dispatch_packet;
use crate::drivers::vmm::globals::{ResourceLockRegistry, VmDbBuffer};
use std::collections::VecDeque;
use dashmap::DashMap;
use std::collections::HashMap;
use std::fs;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::Result;
use serde_json::{json, Value};

use crate::models::ports::file::IFile;
use crate::models::ports::signaler::Listener;
use crate::models::ports::storage::IStorage;
use crate::models::ports::vmm::IVmm;
use crate::models::core::ICore;
use crate::models::transaction::ITrx;
use crate::models::worker::Trx as WorkerTrx;
use crate::shell::api::model::{Creature, Entity, Program, Store};
use crate::shell::api::packets::stores;

/// Default appengine REP socket exposed *by* the node (the engine connects
/// to this with a REQ socket).

/// The virtual-machine driver.
///
/// `Vmm` serves as the canonical owner of all per-execution concurrent state.
/// Rather than scattering independent global statics for VM contexts, per-VM
/// transaction buffers, and resource lock registries, they live here as fields
/// so that the entire VMM state has a single, well-defined owner that is
/// accessible through the canonical `ICore → tools() → vmm()` path.
pub struct Vmm {
    pub(super) app: Arc<dyn ICore>,
    pub(super) storage_root: String,
    pub(super) storage: Arc<dyn IStorage>,
    pub(super) file: Arc<dyn IFile>,

    /// vm_id → (creature_id, machine_id): active VM execution context map.
    pub(crate) vm_context: DashMap<String, (String, String)>,

    /// vm_id → write-ahead transaction buffer for Docker/Fire VM executions.
    pub(crate) vm_trx: DashMap<String, Arc<Mutex<VmDbBuffer>>>,

    /// resource_id → per-resource lock state (used by lockResource host call).
    /// The registry reaps idle entries so a guest cannot pin one lock per
    /// distinct `resource_id` for the life of the node.
    pub(crate) resource_locks: ResourceLockRegistry,

    /// The docker-host bridge gateway. Owned here so docker creatures reach it
    /// only through the canonical `ICore → tools() → vmm()` object graph — there
    /// is no gateway global.
    pub(crate) gateway: Arc<crate::drivers::vmm::network::docker_host::DockerHostGateway>,

    /// The VMM HTTP ingress server. Accepts inbound
    /// `/{creatureId}/{programId}/{entityId}/{vmId}/{path…}` requests and
    /// forwards them to the named VM instance. Owned here for the same reason as
    /// the gateway — reached only through the canonical `tools().vmm()` path.
    pub(crate) http_ingress: Arc<crate::drivers::vmm::network::ingress::VmHttpIngress>,

    /// docker container name → authoritative VM identity. Populated when the
    /// node launches a docker creature; the gateway resolves a connection's
    /// identity by mapping its source IP to a container name and looking it up
    /// here, so a container can never declare/spoof its own identity.
    pub(crate) vm_containers:
        DashMap<String, crate::drivers::vmm::network::docker_host::ContainerIdentity>,
}

impl Vmm {
    /// `NewVmm(core, storageRoot, storage, kvDbPath, file)`.
    pub fn new(
        app: Arc<dyn ICore>,
        storage_root: &str,
        storage: Arc<dyn IStorage>,
        kv_db_path: &str,
        file: Arc<dyn IFile>,
    ) -> Arc<Vmm> {
        let _ = fs::create_dir_all(kv_db_path);
        // Publish the core handle so stateless VM host-call handlers can
        // reach the signaler / storage tools without a Vmm reference.
        crate::drivers::vmm::globals::set_global_app(app.clone());
        // Publish the SDK host bridge and register every VM runtime plugin
        // compiled into this binary (the generated caspar-vm-plugins crate).
        crate::drivers::vmm::host_bridge::init_vm_plugins();
        let gateway = crate::drivers::vmm::network::docker_host::DockerHostGateway::new(app.clone());
        let http_ingress = crate::drivers::vmm::network::ingress::VmHttpIngress::new(app.clone());
        let vmm = Arc::new(Vmm {
            app,
            storage_root: storage_root.to_string(),
            storage,
            file,
            vm_context: DashMap::new(),
            vm_trx: DashMap::new(),
            resource_locks: ResourceLockRegistry::new(),
            gateway,
            http_ingress,
            vm_containers: DashMap::new(),
        });
        vmm
    }

    pub(super) fn send_to_engine(&self, value: Value) {
        let _ = dispatch_packet(&value);
    }

    /// `RunVm(machineId, storeId, data)`.
    pub fn run_vm_inner(self: &Arc<Self>, machine_id: &str, store_id: &str, data: &str) {
        self.run_vm_entity_inner(machine_id, store_id, data, "");
    }

    /// `RunVmEntity(machineId, storeId, data, entityId)`.
    pub fn run_vm_entity_inner(
        self: &Arc<Self>,
        machine_id: &str,
        store_id: &str,
        data: &str,
        entity_id: &str,
    ) {
        let store_id_owned = store_id.to_string();
        let machine_id_owned = machine_id.to_string();
        let store_slot = Arc::new(Mutex::new(Store::default()));
        let member_slot = Arc::new(Mutex::new(false));
        let store_clone = store_slot.clone();
        let member_clone = member_slot.clone();
        let store_id_clone = store_id_owned.clone();
        let machine_id_clone = machine_id_owned.clone();
        self.app.modify_state(
            true,
            Box::new(move |trx: &dyn ITrx| {
                let s = Store {
                    id: store_id_clone.clone(),
                    ..Default::default()
                }
                .pull(trx);
                *store_clone.lock().unwrap() = s;
                *member_clone.lock().unwrap() = trx.get_link(&format!(
                    "hasaccess::{}::{}",
                    machine_id_clone, store_id_clone
                )) == "true";
                Ok(())
            }),
        );
        if !*member_slot.lock().unwrap() {
            return;
        }
        let store = store_slot.lock().unwrap().clone();
        let (ast_path, vm_type) = self.resolve_vm_execution_target(machine_id, entity_id);
        let send_payload = stores::Send {
            user: Creature::default(),
            store,
            action: "single".to_string(),
            data: data.to_string(),
            ..Default::default()
        };
        let input = serde_json::to_string(&send_payload).unwrap_or_default();
        self.send_to_engine(json!({
            "type": "runVm",
            "machineId": machine_id,
            "input": input,
            "astPath": ast_path,
            "vmType": vm_type,
        }));
    }

    pub(super) fn resolve_vm_execution_target(
        &self,
        machine_id: &str,
        entity_id: &str,
    ) -> (String, String) {
        let default_path = format!(
            "{}/machines/{}/module",
            self.storage.storage_root(),
            machine_id
        );
        let path_slot = Arc::new(Mutex::new(default_path));
        let type_slot = Arc::new(Mutex::new(default_runtime_key()));
        let path_clone = path_slot.clone();
        let type_clone = type_slot.clone();
        let machine_id_owned = machine_id.to_string();
        let entity_id_owned = entity_id.to_string();
        self.app.modify_state(
            true,
            Box::new(move |trx: &dyn ITrx| {
                let vm = Program {
                    id: machine_id_owned.clone(),
                    ..Default::default()
                }
                .pull(trx);
                if !vm.path.is_empty() {
                    *path_clone.lock().unwrap() = vm.path.clone();
                }
                if !vm.runtime.is_empty() {
                    *type_clone.lock().unwrap() = vm.runtime.trim().to_lowercase();
                }
                if !entity_id_owned.is_empty() {
                    // Runtimes that record no `vmEntityType` link on deploy
                    // (docker: setEntityLinksOnDeploy=false) carry their type only
                    // on the Entity record. When the program itself names no
                    // runtime, fall back to it so callers that resolve by
                    // (program, entity) — forward_http, cold spawn — pick the
                    // right plugin instead of the default.
                    if vm.runtime.is_empty() {
                        let ent = Entity {
                            program_id: machine_id_owned.clone(),
                            entity_id: entity_id_owned.clone(),
                            ..Default::default()
                        }
                        .pull(trx);
                        if !ent.entity_type.is_empty() {
                            *type_clone.lock().unwrap() = ent.entity_type.trim().to_lowercase();
                        }
                    }
                    let runtime_link = trx.get_link(&format!(
                        "vmEntityType::{}::{}",
                        machine_id_owned, entity_id_owned
                    ));
                    if !runtime_link.is_empty() {
                        *type_clone.lock().unwrap() = runtime_link.trim().to_lowercase();
                    }
                    let path_link = trx.get_link(&format!(
                        "vmEntityPath::{}::{}",
                        machine_id_owned, entity_id_owned
                    ));
                    if !path_link.is_empty() {
                        *path_clone.lock().unwrap() = path_link;
                    }
                }
                Ok(())
            }),
        );
        let path = path_slot.lock().unwrap().clone();
        let vm_type = type_slot.lock().unwrap().clone();
        (path, vm_type)
    }

    /// Whether the state mutations of `vm_id` may enter the cluster
    /// consensus: true only for VMs whose program was deployed with
    /// `distribution: "cluster"`. Local-mode VM state never leaves this
    /// instance.
    fn vm_replication_allowed(&self, vm_id: &str) -> bool {
        if !crate::drivers::cluster::is_active() {
            return false;
        }
        let machine_id = self.get_vm_context(vm_id).map(|(_, m)| m);
        let vm_id_owned = vm_id.to_string();
        let slot = Arc::new(Mutex::new(false));
        let slot_clone = slot.clone();
        self.app.modify_state(
            true,
            Box::new(move |trx: &dyn ITrx| {
                let mut distributed =
                    trx.get_link(&format!("vmDistributed::{}", vm_id_owned)) == "true";
                if !distributed {
                    if let Some(m) = &machine_id {
                        distributed =
                            trx.get_link(&format!("vmDistribution::{}", m)) == "cluster";
                    }
                }
                *slot_clone.lock().unwrap() = distributed;
                Ok(())
            }),
        );
        let allowed = *slot.lock().unwrap();
        allowed
    }
}

impl IVmm for Vmm {
    fn assign(&self, machine_id: &str) {
        // Equivalent to Go's signaler listener registered per machine. The
        // listener forwards `creatures/signal` events to the appengine for
        // delivery to the machine's VM instance.
        let trans = Arc::new(VmmListenerCtx {
            app: self.app.clone(),
            storage_root: self.storage_root.clone(),
        });
        let machine_id_owned = machine_id.to_string();
        let listener = Arc::new(Listener {
            id: machine_id.to_string(),
            paused: false,
            dis_time: 0,
            signal: Arc::new(move |key, value| {
                if key != "creatures/signal" {
                    return;
                }
                let raw = serde_json::to_vec(&value).unwrap_or_default();
                let entity_id = serde_json::from_slice::<stores::Send>(&raw)
                    .ok()
                    .map(|p| p.entity_id)
                    .unwrap_or_default();
                // Proxied response: a packet carrying a correlation id that one
                // of this machine's proxy entities recorded is routed back to
                // the original sender (with the proxy as sender identity)
                // instead of running anything here.
                if crate::drivers::vmm::proxy::try_route_proxy_response(
                    &trans.app,
                    &machine_id_owned,
                    &value,
                ) {
                    return;
                }
                // Proxy entity request: no runnable exists — attach the
                // entity's data file and forward the signal to its target.
                if crate::drivers::vmm::proxy::try_forward_through_proxy(
                    &trans.app,
                    &machine_id_owned,
                    &entity_id,
                    &value,
                ) {
                    return;
                }
                // If the *entity* this signal targets is connected to the bridge
                // gateway, deliver straight to its container over the live TCP
                // connection instead of cold-spawning a VM. A machine can serve
                // several entities, each its own container, so delivery is keyed by
                // entity; a packet that names no entity falls back to any container
                // of the machine (e.g. a bare reply). Reached through the canonical
                // tools().vmm() object graph.
                let vmm = trans.app.tools().vmm();
                let delivered = if entity_id.is_empty() {
                    vmm.push_signal_to_machine(&machine_id_owned, &key, &value)
                } else {
                    vmm.push_signal_to_entity(&machine_id_owned, &entity_id, &key, &value)
                };
                if delivered > 0 {
                    return;
                }
                let (ast_path, vm_type) =
                    trans.resolve_vm_execution_target(&machine_id_owned, &entity_id);
                let is_docker = vm_type == "docker";
                // The node-authoritative owner of this program. Stamped onto the
                // cold-spawn packet so a docker container registers its real
                // creature identity (get_vm_context) and can read secrets granted
                // to it — the agent backbone hydrates its platform LLM keys this
                // way. Empty for a program with no record (harmless: docker keeps
                // its prior empty-id behaviour).
                let creature_id_owner = resolve_program_owner(&trans.app, &machine_id_owned);
                let payload = json!({
                    "type": "runVm",
                    "machineId": machine_id_owned,
                    "creatureId": creature_id_owner,
                    // Carry the resolved entity so a docker creature cold-spawns the
                    // SAME entity image + container id the listener resolved above
                    // (and that its deploy started), not the program default: the
                    // controller otherwise falls back to entity "main", boots the
                    // wrong/absent image and never serves the signalled tool.
                    "entityId": entity_id,
                    "input": String::from_utf8_lossy(&raw),
                    "astPath": ast_path,
                    "vmType": vm_type,
                });
                if is_docker && !entity_id.is_empty() {
                    // A docker *serving* entity (e.g. a tool) that has gone cold has
                    // no gateway connection, so the push above reached nobody. Booting
                    // the container alone would drop the very signal that triggered
                    // the spawn — the serve loop only reads gateway pushes — hanging
                    // the caller. Instead queue the packet against THIS entity; the
                    // node flushes it (in order, with any others that pile up) the
                    // moment that entity's container connects. The spawn is debounced
                    // per entity, so a burst of concurrent signals boots the entity's
                    // container once — and a sibling entity of the same machine is
                    // still spawned independently.
                    vmm.queue_pending_signal(&machine_id_owned, &entity_id, &key, &value);
                    if vmm.begin_cold_spawn(&machine_id_owned, &entity_id) {
                        let _ = dispatch_packet(&payload);
                    }
                } else {
                    // WASM / one-shot runtimes, or a packet that names no entity:
                    // executed with the signal as its input directly — no gateway
                    // connection to wait on, nothing to queue.
                    let _ = dispatch_packet(&payload);
                }
            }),
        });
        self.app.tools().signaler().listen_to_single(listener);
    }

    fn run_vm(&self, machine_id: &str, store_id: &str, data: &str) {
        // We need `Arc<Self>` to drive the inner helper that touches state.
        // Reconstruct one by leaking a clone of the application interface so
        // method calls dispatch normally; the Arc itself isn't recoverable
        // from `&self`, so this is a thin wrapper.
        let trans = Arc::new(VmmShim {
            app: self.app.clone(),
            storage_root: self.storage_root.clone(),
            storage: self.storage.clone(),
        });
        trans.run_vm(machine_id, store_id, data);
    }

    fn run_vm_entity(&self, machine_id: &str, store_id: &str, data: &str, entity_id: &str) {
        let trans = Arc::new(VmmShim {
            app: self.app.clone(),
            storage_root: self.storage_root.clone(),
            storage: self.storage.clone(),
        });
        trans.run_vm_entity(machine_id, store_id, data, entity_id);
    }

    fn terminate_vm(&self, machine_id: &str) {
        self.send_to_engine(json!({
            "type": "terminateVm",
            "machineId": machine_id,
        }));
    }

    fn build_vm_image(
        &self,
        machine_id: &str,
        entity_id: &str,
        build_path: &str,
        build_type: &str,
    ) {
        self.send_to_engine(json!({
            "type": "buildVmImage",
            "runtime": build_type,
            "machineId": machine_id,
            "entityId": entity_id,
            "imageBuildPath": build_path,
            "buildType": build_type,
        }));
    }

    fn execute_chain_trxs_group(&self, _trxs: Vec<WorkerTrx>) {
        // Go's implementation is also a no-op placeholder ("_ = trxs").
    }

    fn execute_chain_effects(&self, _effects: &str) {
        // Same as Go: no-op placeholder.
    }

    fn close_kvdb(&self) {
        // RocksDB closes via `Drop` on the `Arc<TransactionDB>`; nothing to
        // do here. Matches Go semantics (its closeKvdb body is a stub).
    }

    fn vm_callback(&self, data_raw: &str) -> (String, i64) {
        Vmm::vm_callback(self, data_raw)
    }

    // ── Docker-host bridge gateway ────────────────────────────────────────────

    fn start_docker_gateway(&self, port: i64) {
        self.gateway.listen(port);
    }

    fn start_http_ingress(&self, port: i64) {
        self.http_ingress.listen(port);
    }

    fn register_vm_container(
        &self,
        container_name: &str,
        vm_id: &str,
        creature_id: &str,
        program_id: &str,
        machine_id: &str,
        entity_id: &str,
    ) {
        self.vm_containers.insert(
            container_name.to_string(),
            crate::drivers::vmm::network::docker_host::ContainerIdentity {
                vm_id: vm_id.to_string(),
                creature_id: creature_id.to_string(),
                program_id: program_id.to_string(),
                machine_id: machine_id.to_string(),
                entity_id: entity_id.to_string(),
            },
        );
    }

    fn unregister_vm_container(&self, container_name: &str) {
        self.vm_containers.remove(container_name);
    }

    fn identify_container_by_ip(&self, ip: &str) -> Option<(String, String, String, String, String)> {
        // Ask each registered VM runtime whether it owns a live instance on
        // this source IP (container-style runtimes resolve it through their
        // supervisor), then map the instance name to the identity we recorded
        // at launch.
        let name = caspar_vm_sdk::registry::plugins()
            .into_iter()
            .find_map(|plugin| plugin.identify_instance_by_ip(ip))?;
        self.vm_containers.get(&name).map(|e| {
            let id = e.value();
            (
                id.vm_id.clone(),
                id.creature_id.clone(),
                id.program_id.clone(),
                id.machine_id.clone(),
                id.entity_id.clone(),
            )
        })
    }

    fn push_signal_to_machine(&self, machine_id: &str, key: &str, data: &Value) -> usize {
        self.gateway.push_signal_to_machine(machine_id, key, data)
    }

    fn push_signal_to_entity(&self, machine_id: &str, entity_id: &str, key: &str, data: &Value) -> usize {
        self.gateway.push_signal_to_entity(machine_id, entity_id, key, data)
    }

    fn queue_pending_signal(&self, machine_id: &str, entity_id: &str, key: &str, data: &Value) {
        self.gateway.queue_pending_signal(machine_id, entity_id, key, data);
    }

    fn begin_cold_spawn(&self, machine_id: &str, entity_id: &str) -> bool {
        self.gateway.begin_cold_spawn(machine_id, entity_id)
    }

    // ── VM execution context registry ────────────────────────────────────────

    fn register_vm_context(&self, vm_id: &str, creature_id: &str, machine_id: &str) {
        self.vm_context.insert(
            vm_id.to_string(),
            (creature_id.to_string(), machine_id.to_string()),
        );
    }

    fn unregister_vm_context(&self, vm_id: &str) {
        self.vm_context.remove(vm_id);
    }

    fn get_vm_context(&self, vm_id: &str) -> Option<(String, String)> {
        self.vm_context
            .get(vm_id)
            .map(|e| (e.value().0.clone(), e.value().1.clone()))
    }

    // ── Per-VM lifecycle transaction ──────────────────────────────────────────

    fn begin_vm_trx(&self, vm_id: &str) {
        self.vm_trx.insert(
            vm_id.to_string(),
            Arc::new(Mutex::new(VmDbBuffer::new())),
        );
    }

    fn commit_vm_trx(&self, vm_id: &str) {
        if let Some((_, buf_arc)) = self.vm_trx.remove(vm_id) {
            let replicate = self.vm_replication_allowed(vm_id);
            crate::drivers::cluster::with_replication_scope(replicate, || {
                if let Err(e) = buf_arc.lock().unwrap().commit() {
                    eprintln!("[vmm] commit_vm_trx({}) failed: {}", vm_id, e);
                }
            });
        }
    }

    fn vm_db_op(
        &self,
        vm_id: &str,
        op: &str,
        namespaced_key: &str,
        val: &str,
        prefix: &str,
    ) -> Result<String, String> {
        // Look up this VM's lifecycle buffer.
        let buf_arc = if !vm_id.is_empty() {
            self.vm_trx.get(vm_id).map(|r| r.clone())
        } else {
            None
        };

        match op {
            "put" => {
                if let Some(buf) = buf_arc {
                    buf.lock().unwrap().put(namespaced_key.to_string(), val.to_string());
                } else {
                    let k = namespaced_key.to_string();
                    let v = val.to_string();
                    let replicate = self.vm_replication_allowed(vm_id);
                    crate::drivers::cluster::with_replication_scope(replicate, || {
                        self.app.modify_state(
                            false,
                            Box::new(move |trx: &dyn crate::models::transaction::ITrx| {
                                trx.put_link(&k, &v);
                                Ok(())
                            }),
                        );
                    });
                }
                Ok("{}".to_string())
            }
            "get" => {
                // 1. Check write-ahead buffer.
                if let Some(ref buf) = buf_arc {
                    let guard = buf.lock().unwrap();
                    match guard.get_local(namespaced_key) {
                        Some(Some(v)) => return Ok(serde_json::json!({"data": v}).to_string()),
                        Some(None)    => return Ok(serde_json::json!({"data": ""}).to_string()),
                        None          => {}
                    }
                    if let Some(cached) = guard.read_cache.get(namespaced_key) {
                        return Ok(serde_json::json!({"data": cached}).to_string());
                    }
                }
                // 2. Fall through to ICore.
                let k = namespaced_key.to_string();
                let slot = Arc::new(Mutex::new(String::new()));
                let slot_c = slot.clone();
                self.app.modify_state(
                    true,
                    Box::new(move |trx: &dyn crate::models::transaction::ITrx| {
                        *slot_c.lock().unwrap() = trx.get_link(&k);
                        Ok(())
                    }),
                );
                let val_str = { slot.lock().unwrap().clone() };
                if let Some(buf) = buf_arc {
                    buf.lock().unwrap().read_cache.insert(namespaced_key.to_string(), val_str.clone());
                }
                Ok(serde_json::json!({"data": val_str}).to_string())
            }
            "del" => {
                if let Some(buf) = buf_arc {
                    buf.lock().unwrap().del(namespaced_key.to_string());
                } else {
                    let k = namespaced_key.to_string();
                    let replicate = self.vm_replication_allowed(vm_id);
                    crate::drivers::cluster::with_replication_scope(replicate, || {
                        self.app.modify_state(
                            false,
                            Box::new(move |trx: &dyn crate::models::transaction::ITrx| {
                                trx.del_key(&k);
                                Ok(())
                            }),
                        );
                    });
                }
                Ok("{}".to_string())
            }
            "getByPrefix" => {
                let slot = Arc::new(Mutex::new(Vec::<String>::new()));
                let slot_c = slot.clone();
                let pfx = prefix.to_string();
                self.app.modify_state(
                    true,
                    Box::new(move |trx: &dyn crate::models::transaction::ITrx| {
                        *slot_c.lock().unwrap() = trx.get_by_prefix(&pfx);
                        Ok(())
                    }),
                );
                let mut vals = { slot.lock().unwrap().clone() };
                // Overlay write-ahead buffer.
                if let Some(buf) = buf_arc {
                    let guard = buf.lock().unwrap();
                    for (k, v) in &guard.pending_puts {
                        if k.starts_with(prefix) && !vals.contains(v) {
                            vals.push(v.clone());
                        }
                    }
                }
                Ok(serde_json::json!({"data": vals}).to_string())
            }
            _ => Err(format!("unsupported dbOp: {}", op)),
        }
    }

    fn vm_db_commit_explicit(&self, vm_id: &str) -> Result<(), String> {
        if let Some(buf_ref) = self.vm_trx.get(vm_id) {
            let replicate = self.vm_replication_allowed(vm_id);
            crate::drivers::cluster::with_replication_scope(replicate, || {
                buf_ref.lock().unwrap().commit()
            })
        } else {
            Ok(())
        }
    }

    // ── Resource lock management ──────────────────────────────────────────────

    fn acquire_resource_lock(&self, resource_id: &str, owner_id: &str) -> Result<(), String> {
        self.resource_locks.acquire(resource_id, owner_id)
    }

    fn release_resource_lock(&self, resource_id: &str, owner_id: &str) -> Result<(), String> {
        self.resource_locks.release(resource_id, owner_id)
    }

    // ── Host-call dispatch bridge ─────────────────────────────────────────────

    fn host_action_micro(&self, op: &str, input: &serde_json::Value, req_id: i64) -> (String, i64) {
        self.handle_micro_host_action(op, input, req_id)
    }

    fn exec_shell_action(&self, caller: &str, input: &serde_json::Value) -> String {
        self.handle_exec_shell_action(caller, input, 0).0
    }

    fn host_action_resource_store(&self, op: &str, input: &serde_json::Value, req_id: i64) -> (String, i64) {
        self.handle_resource_store_crud(op, input, req_id)
    }

    fn host_action_resource_entity_create(&self, input: &serde_json::Value, req_id: i64) -> (String, i64) {
        self.handle_resource_entity_create(input, req_id)
    }

    fn host_action_resource_entity_delete(&self, input: &serde_json::Value, req_id: i64) -> (String, i64) {
        self.handle_resource_entity_delete(input, req_id)
    }

    fn host_action_store(&self, op: &str, input: &serde_json::Value, req_id: i64) -> (String, i64) {
        self.handle_store_crud(op, input, req_id)
    }

    fn host_action_creature(&self, op: &str, input: &serde_json::Value, req_id: i64) -> (String, i64) {
        self.handle_creature_crud(op, input, req_id)
    }

    fn host_action_program(&self, op: &str, input: &serde_json::Value, req_id: i64) -> (String, i64) {
        self.handle_program_crud(op, input, req_id)
    }

    // ── Dynamic VM runtime registry (answered by the plugin registry) ─────────

    fn supported_runtimes(&self) -> Vec<String> {
        caspar_vm_sdk::registry::keys()
    }

    fn is_supported_runtime(&self, runtime: &str) -> bool {
        caspar_vm_sdk::registry::is_supported(runtime)
    }

    fn is_managed_runtime(&self, runtime: &str) -> bool {
        caspar_vm_sdk::registry::is_managed(runtime)
    }

    fn runtime_supports_chain_trxs(&self, runtime: &str) -> bool {
        caspar_vm_sdk::registry::supports_chain_trxs(runtime)
    }

    fn runtime_deploy_spec(&self, runtime: &str) -> Option<Value> {
        caspar_vm_sdk::registry::get(runtime).map(|p| p.meta().deploy_spec_json())
    }

    fn plan_run_entity(&self, runtime: &str, ctx: &Value) -> Result<Value, String> {
        caspar_vm_sdk::registry::get(runtime)
            .ok_or_else(|| format!("runtime '{}' is not registered", runtime))?
            .plan_run_entity(ctx)
    }

    fn plan_stop_entity(&self, runtime: &str, ctx: &Value) -> Result<Value, String> {
        caspar_vm_sdk::registry::get(runtime)
            .ok_or_else(|| format!("runtime '{}' is not registered", runtime))?
            .plan_stop_entity(ctx)
    }

    fn plan_delete_entity(&self, runtime: &str, ctx: &Value) -> Result<Value, String> {
        caspar_vm_sdk::registry::get(runtime)
            .ok_or_else(|| format!("unsupported runtime: {}", runtime))?
            .plan_delete_entity(ctx)
    }

    fn delete_vm_instance(&self, input: &Value) -> Value {
        let mut packet = input.clone();
        if let Some(obj) = packet.as_object_mut() {
            obj.insert("type".to_string(), Value::String("deleteVm".to_string()));
            obj.insert("purge".to_string(), Value::Bool(true));
            obj.insert("delete".to_string(), Value::Bool(true));
        }
        let raw = dispatch_packet(&packet);
        serde_json::from_str::<Value>(&raw)
            .unwrap_or_else(|_| json!({"ok": false, "error": raw}))
    }

    fn forward_http(&self, request: &Value) -> Value {
        let program_id = request["programId"].as_str().unwrap_or("");
        let entity_id = request["entityId"].as_str().unwrap_or("");
        // Resolve the entity's module path + runtime through the VMM's own
        // resolver (the same one the signal listener and runVm use), then hand
        // the packet router a `forwardHttp` packet shaped exactly like a runVm
        // packet so it resolves the responsible plugin identically.
        let (ast_path, resolved_type) = self.resolve_vm_execution_target(program_id, entity_id);
        // A custom-route request carries the target entity's runtime captured at
        // deploy (request["runtime"]); trust it over the state-derived type,
        // which is unreliable for docker (no vmEntityType link, and the program
        // record may name no runtime) and would otherwise fall back to the
        // default plugin and take the generic signal path instead of the
        // container HTTP proxy.
        let vm_type = match request["runtime"].as_str() {
            Some(r) if !r.trim().is_empty() => r.trim().to_lowercase(),
            _ => resolved_type.clone(),
        };
        // Diagnostic: make runtime resolution observable in the node log, so a
        // forwarded request that takes the signal fallback instead of the docker
        // HTTP proxy can be told apart from a routing miss at a glance.
        eprintln!(
            "[vm-http-ingress] forward_http program={} entity={} route_runtime={:?} resolved_type={} -> vm_type={}",
            program_id,
            entity_id,
            request["runtime"].as_str().unwrap_or(""),
            resolved_type,
            vm_type
        );
        let mut packet = request.clone();
        if let Some(obj) = packet.as_object_mut() {
            obj.insert("type".to_string(), json!("forwardHttp"));
            obj.insert("vmType".to_string(), json!(vm_type));
            obj.insert("astPath".to_string(), json!(ast_path));
            obj.insert("machineId".to_string(), json!(program_id));
        }
        // Dispatch through the VMM's own router entry — the same seam
        // `send_to_engine` uses — and return the plugin's parsed response.
        let raw = dispatch_packet(&packet);
        serde_json::from_str(&raw).unwrap_or(Value::Null)
    }

    fn resolve_http_route(&self, username: &str, path: &str) -> Option<Value> {
        use crate::drivers::vmm::http_route;

        let username = username.trim();
        if username.is_empty() {
            return None;
        }
        // Longest-prefix candidates over the leading request segments, capped so
        // the per-request work is bounded regardless of path length.
        let segments: Vec<String> = path
            .split('/')
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string())
            .collect();
        if segments.is_empty() {
            return None;
        }

        // Resolve username → creature id and match a registered route inside a
        // single state read.
        let result_slot = Arc::new(Mutex::new(None::<Value>));
        let result_clone = result_slot.clone();
        let username_owned = username.to_string();
        let segments_owned = segments.clone();
        self.app.modify_state(
            true,
            Box::new(move |trx: &dyn ITrx| {
                // The leading segment addresses the owning creature either by its
                // username (resolved through the index) or — because a username
                // qualified with a URL-shaped node source (e.g.
                // `name@http://host:port`) cannot be placed in a URL path — by its
                // creature id directly (e.g. `7@global`, which is path-safe). Try
                // the username index first, then fall back to treating the segment
                // itself as the creature id. Routes are stored keyed by creature
                // id, so both address forms converge on the same lookup.
                let mut candidates: Vec<String> = Vec::new();
                let via_username =
                    trx.get_index("Creature", "username", "id", &username_owned);
                if !via_username.is_empty() {
                    candidates.push(via_username);
                }
                // Bare username local part (e.g. `m-tool-github`) → creature id,
                // via the alias link written when the route was registered.
                let via_alias =
                    trx.get_link(&http_route::route_alias_link_key(&username_owned));
                if !via_alias.is_empty() && !candidates.iter().any(|c| c == &via_alias) {
                    candidates.push(via_alias);
                }
                if !candidates.iter().any(|c| c == &username_owned) {
                    candidates.push(username_owned.clone());
                }
                let max = segments_owned.len().min(http_route::MAX_ROUTE_SEGMENTS);
                'outer: for creature_id in &candidates {
                    for take in (1..=max).rev() {
                        let prefix = segments_owned[..take].join("/");
                        let stored =
                            trx.get_link(&http_route::route_link_key(creature_id, &prefix));
                        if stored.is_empty() {
                            continue;
                        }
                        let rest: Vec<&str> =
                            segments_owned[take..].iter().map(|s| s.as_str()).collect();
                        if let Some(route) = http_route::decode_target(&stored, &rest) {
                            *result_clone.lock().unwrap() = Some(json!({
                                "creatureId": creature_id,
                                "programId": route.program_id,
                                "entityId": route.entity_id,
                                "vmId": route.vm_id,
                                "runtime": route.runtime,
                                "path": route.rest_path,
                            }));
                            break 'outer;
                        }
                    }
                }
                Ok(())
            }),
        );
        let out = result_slot.lock().unwrap().take();
        out
    }
}

/// Small shim that owns the same handles as `Vmm` and can be cloned/wrapped
/// in `Arc` so the `&self` IVmm methods can still dispatch through helpers
/// expecting `Arc<Vmm>`.
struct VmmShim {
    app: Arc<dyn ICore>,
    storage_root: String,
    storage: Arc<dyn IStorage>,
}

impl VmmShim {
    fn run_vm(self: &Arc<Self>, machine_id: &str, store_id: &str, data: &str) {
        self.run_vm_entity(machine_id, store_id, data, "");
    }

    /// Entity-aware re-run: resolves the module path of `entity_id` (via the
    /// `vmEntityPath::<machine>::<entity>` link) instead of the program's
    /// default module path. Used by the `plantTrigger` alarm wake so a wasm
    /// creature deployed under a named entity ("main") is actually re-loaded.
    fn run_vm_entity(self: &Arc<Self>, machine_id: &str, store_id: &str, data: &str, entity_id: &str) {
        // Inline of Vmm::run_vm_entity_inner against the shim's handles.
        let store_id_owned = store_id.to_string();
        let machine_id_owned = machine_id.to_string();
        let store_slot = Arc::new(Mutex::new(Store::default()));
        let member_slot = Arc::new(Mutex::new(false));
        let store_clone = store_slot.clone();
        let member_clone = member_slot.clone();
        let store_id_clone = store_id_owned.clone();
        let machine_id_clone = machine_id_owned.clone();
        self.app.modify_state(
            true,
            Box::new(move |trx: &dyn ITrx| {
                let s = Store {
                    id: store_id_clone.clone(),
                    ..Default::default()
                }
                .pull(trx);
                *store_clone.lock().unwrap() = s;
                *member_clone.lock().unwrap() = trx.get_link(&format!(
                    "hasaccess::{}::{}",
                    machine_id_clone, store_id_clone
                )) == "true";
                Ok(())
            }),
        );
        if !*member_slot.lock().unwrap() {
            return;
        }
        let store = store_slot.lock().unwrap().clone();
        let (ast_path, vm_type) =
            resolve_vm_execution_target(&self.app, &self.storage, machine_id, entity_id);
        let send_payload = stores::Send {
            user: Creature::default(),
            store,
            action: "single".to_string(),
            data: data.to_string(),
            ..Default::default()
        };
        let input = serde_json::to_string(&send_payload).unwrap_or_default();
        let payload = json!({
            "type": "runVm",
            "machineId": machine_id,
            "input": input,
            "astPath": ast_path,
            "vmType": vm_type,
        });
        let _ = dispatch_packet(&payload);
        let _ = &self.storage_root;
    }
}

/// Used by the `assign()` signaler listener — same shape as `VmmShim` but
/// keeps only the handles the listener actually needs.
struct VmmListenerCtx {
    app: Arc<dyn ICore>,
    storage_root: String,
}

impl VmmListenerCtx {
    fn resolve_vm_execution_target(&self, machine_id: &str, entity_id: &str) -> (String, String) {
        let default_path = format!("{}/machines/{}/module", self.storage_root, machine_id);
        resolve_vm_execution_target_inner(&self.app, &default_path, machine_id, entity_id)
    }
}

fn resolve_vm_execution_target(
    app: &Arc<dyn ICore>,
    storage: &Arc<dyn IStorage>,
    machine_id: &str,
    entity_id: &str,
) -> (String, String) {
    let default_path = format!("{}/machines/{}/module", storage.storage_root(), machine_id);
    resolve_vm_execution_target_inner(app, &default_path, machine_id, entity_id)
}

fn resolve_vm_execution_target_inner(
    app: &Arc<dyn ICore>,
    default_path: &str,
    machine_id: &str,
    entity_id: &str,
) -> (String, String) {
    let path_slot = Arc::new(Mutex::new(default_path.to_string()));
    let type_slot = Arc::new(Mutex::new(default_runtime_key()));
    let path_clone = path_slot.clone();
    let type_clone = type_slot.clone();
    let machine_id_owned = machine_id.to_string();
    let entity_id_owned = entity_id.to_string();
    app.modify_state(
        true,
        Box::new(move |trx: &dyn ITrx| {
            let vm = Program {
                id: machine_id_owned.clone(),
                ..Default::default()
            }
            .pull(trx);
            if !vm.path.is_empty() {
                *path_clone.lock().unwrap() = vm.path.clone();
            }
            if !vm.runtime.is_empty() {
                *type_clone.lock().unwrap() = vm.runtime.trim().to_lowercase();
            }
            if !entity_id_owned.is_empty() {
                // Fall back to the Entity record's type for runtimes that record
                // no vmEntityType link on deploy (docker), so resolution by
                // (program, entity) picks the right plugin — see the Vmm method.
                if vm.runtime.is_empty() {
                    let ent = Entity {
                        program_id: machine_id_owned.clone(),
                        entity_id: entity_id_owned.clone(),
                        ..Default::default()
                    }
                    .pull(trx);
                    if !ent.entity_type.is_empty() {
                        *type_clone.lock().unwrap() = ent.entity_type.trim().to_lowercase();
                    }
                }
                let runtime_link = trx.get_link(&format!(
                    "vmEntityType::{}::{}",
                    machine_id_owned, entity_id_owned
                ));
                if !runtime_link.is_empty() {
                    *type_clone.lock().unwrap() = runtime_link.trim().to_lowercase();
                }
                let path_link = trx.get_link(&format!(
                    "vmEntityPath::{}::{}",
                    machine_id_owned, entity_id_owned
                ));
                if !path_link.is_empty() {
                    *path_clone.lock().unwrap() = path_link;
                }
            }
            Ok(())
        }),
    );
    let path = path_slot.lock().unwrap().clone();
    let vm_type = type_slot.lock().unwrap().clone();
    (path, vm_type)
}

/// The owning machine-creature id of a program (`Program.machineId`), or empty
/// when the program has no record. This is the node-authoritative identity a
/// docker container must run as: it is what `register_vm_context` records and
/// `get_vm_context` returns, so every identity-gated host call the container
/// makes (secretGet / secretListGranted / dbOp namespacing) resolves to — and
/// can read secrets granted to — the creature that actually owns the program.
/// A guest can neither see nor influence it (it is read from chain state here,
/// before the VM starts). Without this a cold-spawned docker creature (the agent
/// backbone, a tool) would register an EMPTY creature id and could read nothing.
fn resolve_program_owner(app: &Arc<dyn ICore>, program_id: &str) -> String {
    let slot = Arc::new(Mutex::new(String::new()));
    let slot_clone = slot.clone();
    let id_owned = program_id.to_string();
    app.modify_state(
        true,
        Box::new(move |trx: &dyn ITrx| {
            let p = Program {
                id: id_owned.clone(),
                ..Default::default()
            }
            .pull(trx);
            *slot_clone.lock().unwrap() = p.machine_id.clone();
            Ok(())
        }),
    );
    let owner = slot.lock().unwrap().clone();
    owner
}

/// `isManagedRuntime` — runtimes whose VMs run inside the node process.
/// Answered dynamically by the VM plugin registry.
pub(super) fn is_managed_runtime(runtime: &str) -> bool {
    caspar_vm_sdk::registry::is_managed(runtime)
}

/// `normalizeRuntime` — Go's `strings.ToLower(TrimSpace(.))`.
pub(super) fn normalize_runtime(runtime: &str) -> String {
    runtime.trim().to_lowercase()
}

/// Canonical key of the registered fallback runtime, used when a program
/// record carries no runtime of its own.
pub(super) fn default_runtime_key() -> String {
    caspar_vm_sdk::registry::default_key().unwrap_or_default()
}

/// Field-getter helper — emulates Go's generic `checkField[T]`.
pub(super) fn check_field<'a>(input: &'a Value, key: &str) -> Option<&'a Value> {
    input.get(key)
}

pub(super) fn check_str(input: &Value, key: &str, default: &str) -> String {
    input
        .get(key)
        .and_then(Value::as_str)
        .map(str::to_string)
        .unwrap_or_else(|| default.to_string())
}

pub(super) fn check_i64(input: &Value, key: &str, default: i64) -> i64 {
    input
        .get(key)
        .and_then(|v| v.as_i64().or_else(|| v.as_f64().map(|f| f as i64)))
        .unwrap_or(default)
}

pub(super) fn check_bool(input: &Value, key: &str, default: bool) -> bool {
    if let Some(v) = input.get(key) {
        if let Some(b) = v.as_bool() {
            return b;
        }
        if let Some(s) = v.as_str() {
            return s == "true" || s == "1";
        }
    }
    default
}

/// Convenience for `time.Now().UnixMilli()`.
pub(super) fn now_unix_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}

#[allow(dead_code)]
fn _force_use() -> Result<()> {
    Ok(())
}
