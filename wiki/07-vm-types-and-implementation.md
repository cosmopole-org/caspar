# 07 — VM Types & Implementation

Caspar ships seven VM runtime plugins under `vms/`. This page describes each one
in its own section — what it is, its `vm.config.json`, the run-packet fields it
reads, how it talks to the host, and how to implement/extend it — followed by a
general recipe for adding a brand-new VM type.

The prerequisites are the plugin contract in
[VM SDK & Plugins](06-vm-sdk-and-plugins.md) and the VM↔host protocol in
[Caspar Protocol](05-caspar-protocol.md).

Common run-packet fields most runtimes read: `machineId` (the program/creature
id), `vmId` (defaults to `main`), `astPath` (the resolved artifact/module
path), `input` (a JSON string payload), and resource limits parsed by
`parse_vm_resource_limits` (`ram_mb`, `max_exec_time_secs`, CPU/disk).

---

## `wasm` — WasmEdge WebAssembly VM (the default runtime)

**What it is:** the platform's default runtime. Run requests whose hints and
artifact paths match no other registered VM type fall back here. It executes
`.wasm` creature modules **in-process** on WasmEdge with the full Caspar
host-call ABI, per-VM transactions, and cost metering.

**Config highlights:** `key: wasm`, `defaultRuntime: true`, `inProcess: true`,
`artifactExtensions: [".wasm"]`, `entityFileName: module.wasm`,
`acceptsExtraFiles: true`, `setEntityLinksOnDeploy: true`,
`supportsChainTrxs: true`.

**How it runs:** `run_vm` reads `astPath`, `input`, `machineId`, `vmId`, and
limits, then spawns a dedicated thread. Inside it constructs a `WasmMac` VM with
a dispatch callback wired to `host().dispatch(...)`, registers a stop/running
handle, arms a timeout watchdog thread (`max_exec_time_secs`), runs
`execute_on_update(input)`, and finalises. Every panic/failure is contained in
the thread and surfaced as a `vmOutput` error packet; the VM slot is always
released.

**How it talks to the host:** through the `hostCall` guest import
(see [Protocol → host-call ABI](05-caspar-protocol.md#the-host-call-abi)). The
guest allocates the response with its exported `malloc`; the host writes the
JSON response into guest `memory` and returns a packed `(offset<<32|len)`.

**`build_image`:** runs the entity's `build.sh` (which compiles creature sources
into `module.wasm`).

**Implement/extend:** add host-call ops in `src/host_calls.rs`; the controller
in `src/controller.rs` implements `run_vm`/`terminate_vm`/`exec_vm`/
`build_image`. Because it is `defaultRuntime`, other runtimes may delegate to it
via `registry::run_on("wasm", packet)`.

---

## `javascript` — JavaScript VM

**What it is:** runs JavaScript program entities. Execution is **layered on the
managed wasm runtime** (resolved dynamically as the default runtime), and
sources can be transpiled to MASM for provable execution.

**Config highlights:** `key: javascript`, `aliases: ["quickjs","js"]`,
`inProcess: true`, `entityFileName: module.js`.

**How it runs:** `run_vm` / `terminate_vm` simply delegate to the platform
default runtime: `registry::run_on(&self.backing_runtime()?, packet)` where
`backing_runtime()` is `registry::default_key()`. `exec_vm` transpiles the
script to MASM (`transpile_js_to_masm(script_path)`) as a validation/build step.

**Implement/extend:** this is the canonical example of a runtime that reuses a
sibling. To build your own layered runtime, resolve a backing key and delegate
with `registry::run_on` / `registry::terminate_on`.

---

## `docker` — Docker Containers

**What it is:** runs program entities as **sandboxed docker containers**
(gVisor/`runsc` by default) with persistent per-VM bind mounts and image builds
from deployed Dockerfiles, via the Bollard client. Not in-process
(`inProcess: false`).

**Config highlights:** `key: docker`, `entityFileName: Dockerfile`,
`acceptsExtraFiles: true`, `buildOnDeploy: true`, `restorable: true`,
`execFallback: true`.

**Packet identity:** `DockerIdentity::from_packet` reads `entityId` (or
`imageName`), `containerName`, `vmId`, and `standalone`/`isStandalone`.

**How it talks to the host:** a docker creature is sandboxed with no route out
except one long-lived TCP connection to the **docker-host bridge gateway** (port
`8079`), over which it makes every host call and receives pushed signals — its
identity derived spoof-resistantly from the docker source IP. See
[Protocol → docker-host bridge gateway](05-caspar-protocol.md#the-docker-host-bridge-gateway).
The controller registers container identity (`register_vm_container`) for that
identification, and overrides `forward_http` to **proxy inbound HTTP straight to
the server running inside the container** (returning its real response instead of
the async `202`).

**Lifecycle:** `run_vm` starts/creates the container; `terminate_vm` suspends by
default (removes on `purge`); `build_image` builds the image from the deployed
Dockerfile (hence `buildOnDeploy`). `exec_vm`/`copy_to_vm` run commands / copy
files into the container (it is the `execFallback` plugin for legacy container
ABI packets like `execDocker`).

**Implement/extend:** container-style runtimes should set `inProcess: false`,
implement image build in `build_image`, and override `forward_http` if the VM
runs a long-lived HTTP server.

---

## `fire` — Firecracker microVM

**What it is:** runs program entities inside **Firecracker microVMs** with
persistent, non-escapable per-session sandboxes.

**Config highlights:** `key: fire`, `aliases: ["firecracker"]`,
`inProcess: true`, `restorable: true`, `entityFileName: module.wasm`.

**How it runs:** a live guest is a `FireVmProcess` — a supervised child process
with its own Firecracker socket, stdin/stdout/stderr piping threads, an output
buffer, and a **per-session persistent sandbox directory** under
`{storage}/vms/...`. The sandbox is retained across suspend/resume (so a session
wakes with all installed software and data intact) and only removed on an
explicit purge. `terminate_vm` suspends (keeps `vm_dir`); a caller passing
`purge: true` deletes the sandbox.

**Implement/extend:** microVM runtimes track live processes in a registry keyed
by `machine_id`/`vm_id`, stream I/O through host logging, and set
`restorable: true` so sessions survive node restarts.

---

## `modal` — Modal Cloud Sandboxes

**What it is:** runs program entities as **Modal sandboxes** — containers in
Modal's cloud rather than on this machine — each with a persistent Modal
Volume mounted at `/data`. Not in-process (`inProcess: false`).

**Config highlights:** `key: modal`, `aliases: ["modalsandbox",
"modal-sandbox"]`, `entityFileName: Modalfile`, `acceptsExtraFiles: true`,
`restorable: true`, `execFallback: false`.

**Credentials:** the pair Modal issues under Settings → API Tokens, given as
`MODAL_API_KEY` (`"<token-id>:<token-secret>"`) or as `MODAL_TOKEN_ID` +
`MODAL_TOKEN_SECRET`. `MODAL_ENVIRONMENT`, `MODAL_SERVER_URL`,
`MODAL_APP_PREFIX`, `MODAL_DEFAULT_IMAGE`, `MODAL_VOLUME_MOUNT_PATH` and
`MODAL_SANDBOX_TIMEOUT_SECS` tune the rest (see `node/sample.env`). With no
credentials the plugin still registers and every modal operation fails with
"modal runtime is not configured" — a clear message rather than a transport
error.

**How it talks to Modal:** Modal publishes no Rust SDK, so the plugin speaks
Modal's gRPC control plane directly, against a **vendored slice** of Modal's
`api.proto` (`vms/modal/proto/modal.proto`) carrying only the messages and RPCs
this runtime calls, with upstream field numbers preserved. Regenerate it with
`scripts/slice_modal_proto.py` when an upstream change breaks a call — Modal
gives no compatibility guarantee for direct gRPC use. `protox` compiles the
proto in pure Rust, so no `protoc` binary is needed on a build machine.

**Where its state lives:** a Modal sandbox outlives the node process that
started it, so this runtime keeps **no in-process registry**. The vm id →
sandbox id mapping is written to node state (`ModalSandbox::<vmId>`, plus
`ModalVolume::`, `ModalApp::`, `ModalImage::`) at create and read back by every
later operation. That is also what makes restore correct: `restore` re-attaches
to a sandbox that is still running rather than launching a second one and
billing for both.

**Run-packet fields:** `image` (registry tag), `dockerfileCommands` (layered on
top of it — what a deployed `Modalfile` becomes), `entrypoint`/`command`, `env`,
`workdir`, `ports`, `persistent` (default true — the Volume), `forceRestart`,
`idleTimeoutSecs`, `blockNetwork`, `timeoutSecs`.

**Lifecycle:** `run_vm` resolves the app, image and volume then creates the
sandbox (re-attaching to a running one unless `forceRestart`); `terminate_vm`
ends the container but **keeps the Volume**, so a later run mounts the same
`/data` and the VM continues where it left off; `delete_vm` terminates, deletes
the Volume and drops every link, leaving nothing to resume from. `exec_vm` runs
a command through `ContainerExec` and collects both streams; `copy_to_vm` /
`copy_from_vm` use Modal's container filesystem API; `forward_http` proxies to
the sandbox's Modal **tunnel** (falling back to the generic signal path when
the sandbox exposes none), and `vm_endpoints` reports those tunnels, so a
creature can hand a member the public address of something running inside the
project's own machine.

**Implement/extend:** cloud-backed runtimes should keep their instance mapping
in node state rather than memory, override `restore` to adopt live instances,
and set `execFallback: false` so legacy container-ABI packets keep going to the
local container runtime.

---

## `elpian` — Elpian AST VM

**What it is:** executes **Elpian AST programs** in-process with host-call
continuation support and memory/time limits.

**Config highlights:** `key: elpian`, `aliases: ["elpian_vm"]`,
`inProcess: true`, `artifactExtensions: [".elpian.json"]`,
`entityFileName: module.elpian.json`.

**How it runs (continuation model):** `execute_elpian_task` reads the AST file,
creates a VM from the AST (`create_vm_from_ast`), and calls
`execute_vm_func_with_input("main", payload)`. When the result reports
`has_host_call`, the runtime dispatches the host-call payload through
`host().dispatch(...)`, feeds the result back with `continue_execution`, and
loops — enforcing `ram_mb` and `max_exec_time_secs` between steps. On completion
it logs the result and destroys the VM. `terminate_vm` destroys any live VM for
the machine.

**Implement/extend:** interpreter runtimes that yield to the host use this
run→host-call→continue loop; contain panics with `catch_unwind` and surface
failures via `emit_vm_error`.

---

## `elpify` — Elpify Provable VM

**What it is:** executes **MASM programs with STARK proof generation**. It
batches per-entity transactions into single-proof windows, transpiles JS→MASM
on deploy (`buildOnDeploy`), and verifies program-execution proofs
(`providesProgramVerification: true`).

**Config highlights:** `key: elpify`, `aliases: ["masm"]`, `inProcess: true`,
`artifactExtensions: [".masm"]`, `entityFileName: module.elpify.js`,
`buildOnDeploy: true`, `providesProgramVerification: true`.

**How it runs:** `run_vm` reads `machineId`, `vmId`, `astPath` (the MASM file),
and the public inputs (`inputs` array on the packet, or `input` as a JSON string
`{"inputs":[...]}`). With `sync: true` it runs one transaction to completion via
`execute_masm_file_with_proof(masmPath, inputs)` and returns `{outputs, proof}`
synchronously (the STARK proof is base64-encoded so a single scalar survives
wasm round-trips). Otherwise it enqueues the transaction into a batched window
(`enqueue_elpify_task`) that produces one proof per window.

**Verification:** `verify_program_execution` (routed for `verifyProgramExecution`
packets) checks a `{masmPath, inputs, outputs, proof}` bundle with
`verify_execution`. `build_image` transpiles the deployed JS entity to MASM.

**Implement/extend:** provable runtimes set `providesProgramVerification: true`,
implement `verify_program_execution`, and (optionally) batch work to amortise
proving cost — proving is `O(n log² n)`, verification `O(log² n)`.

---

## Recipe: implement a new Caspar-based VM (any of the seven styles)

1. **Scaffold:** `casparctl vms new <key>` creates
   `vms/<key>/{Cargo.toml,vm.config.json,src/lib.rs,src/controller.rs}`.
2. **Declare metadata** in `vm.config.json` — pick the flags that match your
   style:
   - in-process interpreter (like `elpian`/`elpify`): `inProcess: true`,
     set `artifactExtensions`/`entityFileName`.
   - container/microVM (like `docker`/`fire`): `inProcess: false` (docker) or a
     supervised process (fire), `restorable: true`, `buildOnDeploy: true` if the
     image must be built.
   - layered on a sibling (like `javascript`): delegate in the controller.
   - default fallback: `defaultRuntime: true` (only one plugin should set this).
   - provable: `providesProgramVerification: true`.
3. **Implement the controller** — at minimum `meta`, `run_vm`, `terminate_vm`.
   Read `machineId`/`vmId`/`astPath`/`input`/limits from the packet. Reach the
   node only through `caspar_vm_sdk::host()`:
   - emit output/logs: `host().dispatch(json!({"key":"vmOutput"/"vmLog", ...}))`
     or the `log`/`log_vm` helpers.
   - persist state: `vm_json_trx_op` / `state_apply_ops`.
   - orchestrate other runtimes: dispatch `runVm`/`terminateVm` packets, or
     `registry::run_on(other_key, packet)`.
   - handle inbound HTTP: override `forward_http` if the VM serves HTTP.
4. **Contain failures:** wrap execution in `std::panic::catch_unwind` and
   surface errors with `emit_vm_error(machine, vm, key, err)` so a bad program
   can never take down the node.
5. **Wire it in:** `casparctl vms list` (verify discovery) →
   `casparctl vms sync` (regenerate registration) → `./build-dist.sh` (rebuild
   the node with the plugin compiled in).
6. **Deploy an entity to it** with the client CLI —
   `caspar-client vm.init <key> ./proj` then
   `caspar-client programs.deploy <programId> ./proj <key> '{...}'`
   (see [Client CLI](09-client-cli.md)).
