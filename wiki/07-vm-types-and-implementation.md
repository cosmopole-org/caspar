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

## `javascript` — QuickJS JavaScript VM

**What it is:** the platform's second in-process runtime. It executes deployed
JavaScript program entities on **QuickJS** (via `rquickjs`) with the full Caspar
host-call ABI, the per-VM JSON transaction, memory and stack limits, and a real
**interrupt-based** execution deadline. A creature written in JavaScript has
exactly the same host capabilities as one written for `wasm`; only the language
differs.

**Config highlights:** `key: javascript`, `aliases: ["quickjs","js"]`,
`inProcess: true`, `entityFileName: module.js`, `artifactExtensions: [".js"]`,
`setEntityLinksOnDeploy: true`, `supportsChainTrxs: true`,
`acceptsExtraFiles: false`.

**What a deployed entity looks like.** One self-contained script — no module
loader, no filesystem, no imports resolved at run time (`acceptsExtraFiles` is
false, so there is nothing beside it to import). Bundle to a single file (an
esbuild `iife` build, say) and assign the entry point:

```js
globalThis.update = function (inputJson) {
  const p = JSON.parse(inputJson);
  const rec = hostCall("getJson", { key: "Json::Counter::" + p.id, path: "doc" });
  const next = (rec.data?.n ?? 0) + 1;
  hostCall("putJson", { key: "Json::Counter::" + p.id, path: "doc", data: { n: next } });
  return { ok: true, n: next };            // the VM's output
};
```

`update` may be `async`: the runtime drives the promise job queue to quiescence
and reports what it settled on. It never waits on anything the host could
resolve later, because **every host call is synchronous** — there is no host-side
async to await, and a promise still pending when the queue goes quiet is
reported as a failure rather than left hanging.

**How it runs:** `run_vm` validates `astPath`, then spawns a dedicated thread
(every panic contained inside it, surfaced as a `vmOutput` error packet, the VM
slot always released). Inside it builds one QuickJS runtime and context, applies
`ram_mb` as the heap limit and a 1 MiB stack cap, arms the interrupt handler,
evaluates the guest prelude and then the entity, calls `update(input)`, drains
the job queue, commits, and finalises. A context lives for one run: there is no
warm-VM pool, because building a QuickJS context costs microseconds and a fresh
one makes state bleeding between signals impossible.

**How it talks to the host:** through the `hostCall` global — the same
`{"op":…,"input":{…}}` protocol and the same op table as the wasm ABI
(see [Protocol → host-call ABI](05-caspar-protocol.md#the-host-call-abi)),
with the string passed and returned directly instead of through guest memory.
Identity is stamped node-side from the VM's runtime context on every forwarded
op, exactly as in `wasm`, so a guest can neither fabricate nor spoof it.

**The guest prelude** additionally provides `console.*` (routed to the VM log,
not the node's stdout), a frozen `caspar` identity object
(`machineId` / `programId` / `vmId` / `storeId` / `runtime`), `btoa`/`atob`,
`TextEncoder`/`TextDecoder` (UTF-8), and `structuredClone`. QuickJS supplies the
rest of ES2023.

**Setting the output:** return a value from `update` (a string is used verbatim,
anything else is JSON-encoded), or call the `output` host op. An explicit
`output` call **wins**, so a creature ported from `wasm` behaves identically.

**Termination is real.** QuickJS exposes an interrupt hook, so both the exec
deadline (`max_exec_time_secs`) and `terminateVm` genuinely stop a running
script — including one looping inside a `.then()`. The wasm runtime can only ask
a guest to stop; this one does not have to. A terminate naming a `vmId` stops
that instance; one naming only a machine stops every instance of it. A
cancellable watchdog thread backs the interrupt for the case it cannot reach: a
run blocked in a host call rather than in JavaScript.

**`exec_vm`** transpiles the script to MASM (`transpile_js_to_masm`) as a
provable-execution validation step. That is the `elpify` path, not the execution
path — a script that will not transpile still runs perfectly well here.

**Implement/extend:** the op table is `src/host_calls.rs` (keep it in step with
`vms/wasm/src/host_calls.rs` — a creature must not be able to tell which
in-process runtime it landed on by the behaviour of a host call); the engine is
`src/runtime.rs`; the guest prelude is `src/prelude.js`.

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
