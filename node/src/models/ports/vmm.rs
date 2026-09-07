use crate::models::worker::Trx;
use serde_json::Value as JsonValue;

/// The virtual machine manager driver interface.
///
/// `IVmm` is the contract through which the rest of the node reaches the VM
/// subsystem.  All VM lifecycle, database, and lock operations are exposed
/// here so that VMM submodules (controllers, host-call handlers) never need
/// their own global statics — they reach everything through the canonical
/// `ICore → tools() → vmm()` path.
pub trait IVmm: Send + Sync {
    // ── VM deployment & execution ─────────────────────────────────────────
    fn assign(&self, machine_id: &str);
    fn run_vm(&self, machine_id: &str, store_id: &str, data: &str);
    /// Re-run a specific *entity* of `machine_id` (e.g. `"main"`) with `data`.
    ///
    /// `run_vm` resolves the program's default module path, which for creatures
    /// deployed under a named entity (`Program.path` = `/api/main`, real module
    /// under `entities/<entityId>`) is not the wasm file. A scheduled self-wake
    /// (the `plantTrigger` alarm) must therefore name the entity so the correct
    /// module is loaded. The default preserves the old behaviour for impls that
    /// do not distinguish entities.
    fn run_vm_entity(&self, machine_id: &str, store_id: &str, data: &str, _entity_id: &str) {
        self.run_vm(machine_id, store_id, data);
    }
    fn terminate_vm(&self, machine_id: &str);
    fn build_vm_image(
        &self,
        machine_id: &str,
        entity_id: &str,
        build_path: &str,
        build_type: &str,
    );
    fn execute_chain_trxs_group(&self, trxs: Vec<Trx>);
    fn execute_chain_effects(&self, effects: &str);
    fn close_kvdb(&self);
    /// Returns `(result, gasUsed)`.
    fn vm_callback(&self, data_raw: &str) -> (String, i64);

    // ── Docker-host bridge gateway ───────────────────────────────────────
    /// Start the docker-host bridge gateway TCP listener for docker creatures.
    /// No-op when `port <= 0` or already running. The gateway is owned by the
    /// VMM instance — callers reach it only through `tools().vmm()`.
    fn start_docker_gateway(&self, port: i64);

    /// Start the VMM HTTP ingress listener. It accepts requests shaped as
    /// `/{creatureId}/{programId}/{entityId}/{vmId}/{path…}` (fully-qualified
    /// identity) or `/{creatureUsername}/{customPath…}` (a deployer-defined
    /// custom route) and forwards them to the HTTP server of the named VM
    /// instance (docker proxies to the container; other runtimes fall back to
    /// signalling). No-op when `port <= 0` or already running.
    fn start_http_ingress(&self, port: i64);
    /// Bind a docker container *name* to its VM's authoritative identity. Done
    /// when the node launches the container, so a gateway connection can be
    /// identified by resolving its source IP → container name → this identity.
    fn register_vm_container(
        &self,
        container_name: &str,
        vm_id: &str,
        creature_id: &str,
        program_id: &str,
        machine_id: &str,
        entity_id: &str,
    );
    /// Drop a container→identity binding at VM teardown.
    fn unregister_vm_container(&self, container_name: &str);
    /// Identify the creature/VM a connection belongs to from its docker-network
    /// source `ip`: the node asks docker which container owns that IP, then maps
    /// the container name to its registered identity. Returns
    /// `(vm_id, creature_id, program_id, machine_id, entity_id)`. Spoof-resistant —
    /// the container cannot forge its bridge IP or docker's view of it.
    fn identify_container_by_ip(&self, ip: &str) -> Option<(String, String, String, String, String)>;
    /// Push a signal to every live docker container of `machine_id`, regardless of
    /// entity. For packets that name no entity only. Returns the number reached.
    fn push_signal_to_machine(&self, machine_id: &str, key: &str, data: &JsonValue) -> usize;
    /// Push a signal to the container serving `entity_id` on `machine_id`. Returns
    /// the number reached (`0` ⇒ that entity is cold, so the caller queues/spawns).
    fn push_signal_to_entity(&self, machine_id: &str, entity_id: &str, key: &str, data: &JsonValue) -> usize;
    /// Queue a signal for a docker entity that has no live container yet, to be
    /// delivered when it (re)connects to the gateway. Used on the cold-spawn path
    /// so the signal that woke the creature is not lost while it boots.
    fn queue_pending_signal(&self, machine_id: &str, entity_id: &str, key: &str, data: &JsonValue);
    /// Claim the cold-spawn slot for `(machine_id, entity_id)` (debounce). Returns
    /// `true` for the caller that should boot the container and `false` while a
    /// spawn started recently is still in flight — concurrent signals then only
    /// queue. Per entity, so entities of one machine boot independently.
    fn begin_cold_spawn(&self, machine_id: &str, entity_id: &str) -> bool;

    // ── VM execution context registry ────────────────────────────────────
    /// Register an active VM execution context (vm_id → creature/machine).
    fn register_vm_context(&self, vm_id: &str, creature_id: &str, machine_id: &str);
    /// Remove a VM execution context when the VM terminates.
    fn unregister_vm_context(&self, vm_id: &str);
    /// Look up the (creature_id, machine_id) for a running VM.
    fn get_vm_context(&self, vm_id: &str) -> Option<(String, String)>;

    // ── Per-VM lifecycle transaction management ──────────────────────────
    /// Open a write-ahead transaction buffer for `vm_id`.
    /// All subsequent `vm_db_op` writes for this VM are buffered until commit.
    fn begin_vm_trx(&self, vm_id: &str);
    /// Commit all buffered writes for `vm_id` atomically via `ICore::modify_state`,
    /// then discard the buffer.
    fn commit_vm_trx(&self, vm_id: &str);
    /// Execute a key-value DB operation for a running VM, routing through the
    /// VM's write-ahead buffer when one exists.
    ///
    /// `op`: `"put"` | `"get"` | `"del"` | `"getByPrefix"`.
    /// `namespaced_key`: fully-qualified storage key (e.g. `AppletDb::...`).
    /// `val`: value for `"put"` ops (ignored otherwise).
    /// `prefix`: namespace prefix for `"getByPrefix"` ops (ignored otherwise).
    fn vm_db_op(
        &self,
        vm_id: &str,
        op: &str,
        namespaced_key: &str,
        val: &str,
        prefix: &str,
    ) -> Result<String, String>;
    /// Flush the VM's current buffer immediately (mid-execution explicit commit).
    fn vm_db_commit_explicit(&self, vm_id: &str) -> Result<(), String>;

    // ── Resource lock management ─────────────────────────────────────────
    /// Acquire an exclusive lock on `resource_id` for `owner_id`.
    /// Blocks until the lock is available (FIFO queue).
    fn acquire_resource_lock(&self, resource_id: &str, owner_id: &str) -> Result<(), String>;
    /// Release the lock on `resource_id` held by `owner_id`.
    fn release_resource_lock(&self, resource_id: &str, owner_id: &str) -> Result<(), String>;

    // ── Host-call dispatch bridge ────────────────────────────────────────
    /// Dispatch a micro host action (genId, getLink, putJson, …).
    fn host_action_micro(&self, op: &str, input: &JsonValue, req_id: i64) -> (String, i64);
    /// Run a registered shell action for a VM. `caller` is the node-resolved
    /// creature behind the call — the identity an `asSelf` request acts as, and
    /// the reason a guest cannot nominate its own.
    fn exec_shell_action(&self, caller: &str, input: &JsonValue) -> String;
    /// Dispatch a resource-store CRUD host action.
    fn host_action_resource_store(&self, op: &str, input: &JsonValue, req_id: i64) -> (String, i64);
    /// Dispatch a resource-entity create/delete host action.
    fn host_action_resource_entity_create(&self, input: &JsonValue, req_id: i64) -> (String, i64);
    fn host_action_resource_entity_delete(&self, input: &JsonValue, req_id: i64) -> (String, i64);
    /// Dispatch a store CRUD host action.
    fn host_action_store(&self, op: &str, input: &JsonValue, req_id: i64) -> (String, i64);
    /// Dispatch a creature CRUD host action.
    fn host_action_creature(&self, op: &str, input: &JsonValue, req_id: i64) -> (String, i64);
    /// Dispatch a program CRUD host action (get/list/update/listByMachine).
    fn host_action_program(&self, op: &str, input: &JsonValue, req_id: i64) -> (String, i64);

    // ── Dynamic VM runtime registry ──────────────────────────────────────
    //
    // The node never hardcodes VM type keys; everything below is answered by
    // the VM plugin registry, so the set of supported runtimes is exactly the
    // set of plugins the host admin compiled into this binary.

    /// Canonical keys of every VM runtime compiled into this node.
    fn supported_runtimes(&self) -> Vec<String>;
    /// Whether `runtime` (key or alias) names a registered VM type.
    fn is_supported_runtime(&self, runtime: &str) -> bool;
    /// Whether `runtime` executes inside the node process (managed runtime).
    fn is_managed_runtime(&self, runtime: &str) -> bool;
    /// Whether `runtime` executes grouped chain transactions.
    fn runtime_supports_chain_trxs(&self, runtime: &str) -> bool;
    /// Deploy behaviour of `runtime` as declared by its plugin:
    /// `{ entityFileName, acceptsExtraFiles, buildOnDeploy,
    ///    setEntityLinksOnDeploy }`. `None` for unknown runtimes.
    fn runtime_deploy_spec(&self, runtime: &str) -> Option<JsonValue>;
    /// Ask `runtime`'s plugin to plan a standalone entity launch.
    /// `ctx`: `{ machineId, programId, entityId, vmId, resources, params }` →
    /// `{ input, links: [[key, value], ...] }`.
    fn plan_run_entity(&self, runtime: &str, ctx: &JsonValue) -> Result<JsonValue, String>;
    /// Ask `runtime`'s plugin to plan a standalone entity stop.
    /// `ctx`: `{ machineId, programId, entityId, vmId }` →
    /// `{ input, links: [{field, key, required}, ...] }`.
    fn plan_stop_entity(&self, runtime: &str, ctx: &JsonValue) -> Result<JsonValue, String>;
    /// Ask `runtime`'s plugin to plan a standalone entity *delete* — the
    /// destructive counterpart of the stop plan, shaped identically
    /// (`{ input, links }`) so the caller resolves the same per-runtime state
    /// links before dispatching.
    fn plan_delete_entity(&self, runtime: &str, ctx: &JsonValue) -> Result<JsonValue, String>;

    /// Permanently destroy one VM instance through its runtime plugin.
    ///
    /// Node-internal: the caller has already authorized the delete (the
    /// program API checks the program's owner; the `deleteVm` host op checks
    /// the VM's owning creature). Reaching the router through the VMM rather
    /// than the guest-facing key envelope is what keeps this off the path a
    /// guest can address.
    fn delete_vm_instance(&self, input: &JsonValue) -> JsonValue;

    /// Forward a packaged inbound HTTP request to the VM instance it targets.
    ///
    /// `request`: `{ creatureId, programId, entityId, vmId, method, path,
    ///              query, headers, bodyBase64 }`.
    ///
    /// The VMM resolves the entity's module path + runtime (the same
    /// resolution the signal listener and `runVm` use) and dispatches a
    /// `forwardHttp` packet to the entity's plugin through the packet router —
    /// docker proxies to the container's HTTP server, every other runtime
    /// falls back to signalling the VM. Returns the plugin's response value
    /// `{ ok, status, headers, body | bodyBase64 }`. Owning this here keeps
    /// forwarding a capability of the VMM instance rather than free-standing
    /// logic reaching into the packet router directly.
    fn forward_http(&self, request: &JsonValue) -> JsonValue;

    /// Resolve a friendly gateway request `/{creature}/{path…}` to the VM entity
    /// a deployer bound to that custom path.
    ///
    /// `creature` is the leading path segment: either a creature username (e.g.
    /// `alice@global`, resolved through the creature index) or the creature id
    /// itself (e.g. `7@global`). The id form exists because a username qualified
    /// with a URL-shaped node source (`name@http://host:port`) cannot be placed
    /// in a URL path, whereas the id always can. `path` is everything after the
    /// leading segment. The VMM matches the longest custom-path prefix registered
    /// for that creature at deploy time (metadata `gatewayPath`). On a hit it
    /// returns `{ creatureId, programId, entityId, vmId, path }` — the identity
    /// fields `forward_http` expects, with `path` rewritten to the sub-path after
    /// the matched prefix. Returns `None` when the creature is unknown or no
    /// route matches, so the ingress can fall back to the fully-qualified
    /// identity form.
    fn resolve_http_route(&self, creature: &str, path: &str) -> Option<JsonValue>;
}
