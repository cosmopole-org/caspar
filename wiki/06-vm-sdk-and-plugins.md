# 06 — VM SDK & Plugins

This page is the contract for building a Caspar VM runtime. A **VM type** is a
standalone Rust project under `vms/` that depends on `caspar-vm-sdk`, implements
the `VmPlugin` trait for its controller, describes itself with a
`vm.config.json`, and exposes `pub fn register()`. The node never names a VM
type; it resolves everything through the SDK's plugin registry.

---

## How a plugin is wired into the node

1. At **build time**, `casparctl vms sync` scans `vms/`, and generates a
   temporary aggregation crate `node/crates/caspar-vm-plugins/` (marked
   `@generated`) that `use`s every *enabled* plugin and calls its `register()`
   in a `register_all()`. `build-dist.sh` runs this before compiling.
2. At **node start**, `register_all()` runs, so each plugin's `register()`
   parses its `vm.config.json` into a `VmPluginMeta` and calls
   `registry::register_plugin(Arc::new(<Controller>::new(meta)))`.
3. At **runtime**, the node publishes a `VmHost` implementation via
   `host::set_host(...)`, and every VM operation is resolved dynamically through
   `caspar_vm_sdk::registry`.

The node source therefore never mentions `docker`, `wasm`, etc. — the only
place plugin crates are imported is the generated aggregation crate.

---

## Plugin project structure

```text
vms/<key>/
├── Cargo.toml        # package name MUST be caspar-vm-<something>; a lib crate
├── vm.config.json    # plugin metadata (VmPluginMeta)
├── src/
│   ├── lib.rs        # exposes `pub fn register()`
│   ├── controller.rs # the VmPlugin implementation
│   └── ...           # models, runtime, anything the VM type needs
└── crates/           # optional: library crates owned by this VM type
```

Requirements for a valid plugin:

1. `Cargo.toml` defines a **library** crate; the package name is its identity
   in the build graph.
2. `vm.config.json` describes the runtime (minimum `{"key": "<runtime key>"}`).
3. `src/lib.rs` exposes `pub fn register()`.
4. Every interaction with the node goes through `caspar_vm_sdk::host()`
   (`VmHost`).

---

## `vm.config.json` — `VmPluginMeta`

Loaded and validated by `VmPluginMeta::from_config_str`. Fields (camelCase):

| Field | Meaning |
|-------|---------|
| `key` (required) | Canonical lowercase runtime name, e.g. `"docker"`. |
| `name`, `version`, `description` | Human-readable metadata. |
| `aliases` | Alternative runtime hints resolving here (e.g. `"firecracker"` → `fire`). |
| `artifactExtensions` | Artifact path suffixes claimed (e.g. `".masm"`, `".elpian.json"`). |
| `inProcess` | True when VMs run inside the node process (managed runtimes). |
| `defaultRuntime` | The fallback plugin when no hint/extension matches. |
| `entityFileName` | Primary file a deployed entity is stored as (`module.wasm`, `Dockerfile`, …). Default `module.wasm`. |
| `acceptsExtraFiles` | Deploy accepts an extra `files` map beside the payload. |
| `buildOnDeploy` | A deploy must be followed by an async `buildVmImage`. |
| `setEntityLinksOnDeploy` | Deploy records `vmEntityPath`/`vmEntityType` links for signal dispatch. |
| `supportsChainTrxs` | Runtime executes grouped chain transactions. |
| `restorable` | Running VMs are relaunched when the node restores a snapshot. |
| `execFallback` | Handles exec/copy/build packets with no resolvable runtime hint (legacy container ABI). |
| `providesProgramVerification` | Handles `verifyProgramExecution` packets. |

`matches_key(hint)` (key or alias) and `matches_artifact(path)` (extension)
drive resolution; `deploy_spec_json()` is what the node's deploy action reads.

---

## The `VmPlugin` trait (what a controller implements)

All methods speak JSON packets. Only two are mandatory — every other method has
a sensible default.

**Mandatory:**

- `fn meta(&self) -> &VmPluginMeta`
- `fn run_vm(&self, packet: &Value) -> Result<Value, String>` — launch (or
  resume) a VM.
- `fn terminate_vm(&self, packet: &Value) -> Result<Value, String>` — stop a VM.

**Core lifecycle (default = "unsupported"/no-op):**

- `exec_vm`, `copy_to_vm`, `copy_from_vm`, `build_image`.
- `vm_endpoints` — the public URLs a running VM is reachable on. A runtime that
  publishes a VM's ports somewhere reachable (a cloud sandbox's tunnels)
  answers here; one with nothing public returns an empty list, because having
  no public endpoint is an ordinary state and not a failure.
- `delete_vm` — **permanently** destroy a VM and everything it owns. Terminate
  suspends: the instance can be resumed and its persistent volume survives, so
  a runtime that only implements terminate can never actually free anything.
  The default is a terminate carrying `purge`/`delete`, which the runtimes that
  distinguish the two honour by removing rather than stopping; a runtime with
  resources a purge does not reach (a cloud sandbox, a remote volume, a built
  image) overrides it.
- Aliased verbs `create` / `start` / `resume` → `run_vm`; `stop` / `pause` →
  `terminate_vm`; `destroy` → `delete_vm`.
- `init(&self)` — one-time hook after registration.

**Snapshot restore:**

- `restore(&self, snapshot_entry)` — restorable runtimes relaunch; others ack
  and skip.

**Runtime resolution:**

- `detect(&self, runtime_hint, artifact_path)` — default matches key or artifact
  extension.
- `identify_instance_by_ip(&self, ip)` — resolve a live VM instance name from a
  source IP (for gateway identification).

**Program-shell integration plans** — the node's program API never encodes
per-runtime behaviour; it asks the plugin for a *plan* (plain JSON describing
the packet to send and the state links to read/write) and executes it inside its
own transaction:

- `plan_run_entity(ctx)` → `{ input, links }` for a standalone `runVm`.
- `plan_stop_entity(ctx)` → `{ input, links }` for a `terminateVm`, where each
  link asks the caller to read a state key into an input field.
- `plan_delete_entity(ctx)` → `{ input, links }` for a `deleteVm`, same shape
  as the stop plan (the default derives it from that plan, so a runtime that
  overrode only `plan_stop_entity` still deletes correctly).
- `build_terminate_request(input)` → the typed terminate packet.
- `build_delete_request(input)` → the typed delete packet. Unlike terminate it
  always names one concrete instance: deleting "whatever this machine is
  running" would destroy VMs the caller never named.

**Inbound HTTP forwarding:**

- `forward_http(packet)` — default wraps the request into a `creatures/signal`
  and returns `202 Accepted`; override to proxy to an in-VM HTTP server.

**Optional capabilities:**

- `verify_program_execution(packet)` — provable runtimes only.

---

## The `VmHost` trait (what the node offers a plugin)

Published once by the node via `host::set_host`. Reach it with
`caspar_vm_sdk::host()` (`Option<Arc<dyn VmHost>>`) or `host_or_err()`.
Capabilities:

- **Packet dispatch:** `dispatch(packet)` (emit `vmOutput`/`vmLog`/`signal`, or
  invoke other runtimes), `unified_host_call(packet)` (`signalUser`, `dbOp`,
  CRUD).
- **Logging:** `storage_log_vm(vm_id, log_type, text, ts)`; helpers `log(text)`
  and `log_vm(text, vm_id, log_type)` emit `vmLog` packets;
  `set_log_vm_context(vm_id)` binds the thread's log lines.
- **VM context registry:** `register_vm_context` / `unregister_vm_context` /
  `get_vm_context`.
- **Container identity registry:** `register_vm_container` /
  `unregister_vm_container` (gateway identification).
- **Per-VM write buffer:** `begin_vm_buffer` / `commit_vm_buffer` (dbOp
  transactions).
- **Resource locks:** `acquire_resource_lock` / `release_resource_lock`.
- **Node state (links):** `state_get`, `state_get_by_prefix`, `state_apply_ops`
  (atomic batch of `KvOp { op, key, val }`).
- **Per-VM JSON transaction:** `vm_json_trx_op(vm_id, op, input)`
  (`putJson`/`getJson`/`getByPrefix`/`delKey`), `end_vm_json_trx(vm_id)`.
- **Misc:** `http_request(input)` (returns base64 body), `storage_root()`.

---

## The plugin registry

`caspar_vm_sdk::registry` is the global set of registered plugins. Useful calls:

- `register_plugin(plugin)`, `plugins()`, `keys()`, `get(runtime)`,
  `resolve_key(runtime)`.
- `default_plugin()` / `default_key()` — the `defaultRuntime` plugin.
- `resolve_for_packet(packet, artifact_path)` — resolve for a run packet.
- `resolve_for_exec(packet)` / `exec_fallback_plugin()` — resolve for
  exec/copy/build.
- `verifier_plugin()` — the proof-verifying plugin.
- `is_supported`, `is_managed`, `supports_chain_trxs`.
- **Cross-plugin delegation:** `run_on(runtime, packet)` /
  `terminate_on(runtime, packet)` / `delete_on(runtime, packet)` — for a runtime
  that layers on a sibling rather than executing itself. No shipped plugin does
  today; `javascript` used to, and executed nothing as a result.

---

## Minimal plugin skeleton

`casparctl vms new <key>` writes exactly this shape. `src/lib.rs`:

```rust
mod controller;
use std::sync::Arc;
use caspar_vm_sdk::{registry, VmPluginMeta};
pub use controller::MyvmVmController;

pub fn register() {
    let meta = VmPluginMeta::from_config_str(include_str!("../vm.config.json"))
        .expect("caspar-vm-myvm: invalid vm.config.json");
    registry::register_plugin(Arc::new(MyvmVmController::new(meta)));
}
```

`src/controller.rs`:

```rust
use serde_json::{json, Value as JsonValue};
use caspar_vm_sdk::{VmPlugin, VmPluginMeta};

pub struct MyvmVmController { meta: VmPluginMeta }
impl MyvmVmController { pub fn new(meta: VmPluginMeta) -> Self { Self { meta } } }

impl VmPlugin for MyvmVmController {
    fn meta(&self) -> &VmPluginMeta { &self.meta }

    fn run_vm(&self, packet: &JsonValue) -> Result<JsonValue, String> {
        let machine_id = packet["machineId"].as_str().unwrap_or("");
        if machine_id.is_empty() { return Err("machineId is required".into()); }
        // Launch the VM. Reach the node through caspar_vm_sdk::host().
        Ok(json!({"ok": true, "runtime": "myvm", "machineId": machine_id}))
    }

    fn terminate_vm(&self, packet: &JsonValue) -> Result<JsonValue, String> {
        let machine_id = packet["machineId"].as_str().unwrap_or("");
        if machine_id.is_empty() { return Err("machineId is required".into()); }
        Ok(json!({"ok": true, "runtime": "myvm", "machineId": machine_id}))
    }
}
```

Then: `casparctl vms sync` → `./build-dist.sh`. See
[VM Types](07-vm-types-and-implementation.md) for six worked examples and the
run-packet fields each runtime reads (`astPath`, `input`, `machineId`, `vmId`,
resource limits, …).
