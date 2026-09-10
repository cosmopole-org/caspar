# 02 — Architecture

Caspar is a single Rust binary that runs several concurrent services and hosts
user logic as WebAssembly creatures. This page walks the subsystems and the
mechanisms that connect them, each in its own section.

---

## Runtime topology

```text
        mutual-TLS TCP / WebSocket clients
                       │
                ┌──────▼───────┐
                │ Action Router │  signed-packet validation · guards · auth
                └──────┬───────┘
        ┌──────────────┼───────────────────────────┐
        ▼              ▼                             ▼
  Core Txn        Store Layer                  Storage Layer
  (Babble)                                     (RocksDB · QuestDB)
        │              │
        ▼              ▼
 Hashgraph Chain   VM Packet Router  ── runtime-agnostic dispatch ──┐
                                                                    ▼
                        wasm · docker · elpify · elpian · javascript · firecracker
```

---

## Startup composition

The entrypoint (`node/src/main.rs`) wires adapters/tools, installs action
routes and pluggers, then starts concurrent services:

- **pprof** profiling HTTP server (`:9999`, Rust-native).
- **Telemetry** HTTP server — `GET /telemetry/snapshot` (`TELEMETRY_API_PORT`,
  default `:9099`).
- **TLS TCP** client server (`CLIENT_TCP_API_PORT`).
- **TLS WebSocket** client server (`CLIENT_WS_API_PORT`).
- **Federation** transport (`FEDERATION_API_PORT`).
- **Babble chain** service (`BLOCKCHAIN_API_PORT`).

On boot the node also restores in-memory VMM signaler listeners (one per unique
`machine_id`) so previously-deployed creatures keep receiving signals.

---

## Action Router

`node/src/shell/api` validates the signed packet, resolves the target route,
applies guard-based authorisation, and dispatches to a handler. Action handlers
live in `shell/api/actions` (`auth`, `creature`, `program`, …). This is the
single entry point for every client and federation request.

---

## Core transactions

`node/src/core` owns the transaction lifecycle, the `ICore` context, callbacks,
and update propagation. Actions whose input declares `origin == "global"` are
consensus-bound; `origin == ""` runs locally. The `ICore` context also holds the
per-VM transaction registry (`Mutex<HashMap<VmId, Arc<dyn ITrx>>>`).

---

## Hashgraph chain

`node/src/drivers/network/chain` is an embedded Babble (Rust) implementation.
The node registers a `commit_handler` that receives ordered blocks of
transactions and fans them out to application state. See
[Consensus](08-consensus-federation-cluster.md).

---

## VM Manager / VM Packet Router

`node/src/drivers/vmm` exposes a single `dispatch_packet` / `route_vm_packet`
path that drives all seven runtimes. The controllers live in
`drivers/vmm/controllers`. `task_graph.rs` is runtime-agnostic: each op injects
a canonical `"type"` field and delegates to `dispatch_packet`, so the router is
the only place runtime branching happens. At the SDK layer this dispatch is
resolved through the plugin registry (see
[VM SDK & Plugins](06-vm-sdk-and-plugins.md)).

---

## appengine

The execution engine that hosts the managed (non-wasm) runtimes; the node
exposes a local socket the engine connects to. WASM creatures themselves run
in-process via WasmEdge.

---

## Storage drivers

`node/src/drivers/storage.rs` provides:

- **RocksDB** for creature and chain state, via the `TrxWrapper` abstraction
  (the per-VM transaction handle).
- **QuestDB** over the PostgreSQL wire protocol for telemetry / time-series data.

---

## Telemetry subsystem

`node/src/telemetry` maintains a cached snapshot served over HTTP and consumed
by `casparctl stats`. The collector also proxies the live chain endpoints
(`/stats`, `/peers`, `/validators`, `/history`) into the snapshot's `chain`,
`validators`, `staking`, and `election` fields.

---

## Creature programming model

A **creature** is code (`.wasm`) + state (a per-creature RocksDB namespace) +
signals (a message bus) + VM ops (orchestration of subordinate VMs). State is
reached through host calls (`putJson`, `getJson`, `getByPrefix`, `delKey`) and
persists across signals. All platform access flows through the single
`hostCall` ABI.

### Per-VM persistent transaction

Each active VM holds a single `TrxWrapper` (one RocksDB transaction) for its
lifetime. All key-value mutations within a creature signal commit atomically —
on an explicit `commitTrx` or on VM finalisation — eliminating write
amplification and never exposing partial state between host calls.

---

## Multi-runtime VM router (summary)

| Runtime | Purpose |
|---------|---------|
| `wasm` | WasmEdge-hosted secondary WASM modules (the default runtime) |
| `docker` | OCI container images (gVisor-sandboxed) via the Bollard client |
| `elpify` | Miden STARK VM (proof-generating MASM execution) |
| `elpian` | native Rust AST VM for high-speed computation |
| `javascript` | QuickJS JavaScript runtime (full host-call ABI, in-process) |
| `fire` (firecracker) | micro-VM hypervisor for hardware-isolated workloads |

---

## Federation

Each node runs a federation transport that validates the signature and origin of
inbound packets against a known-origins registry before admitting them to the
local action router. Outbound calls propagate creature updates and chain events
to registered peers.

---

## Sharding

A shard is a self-contained three-node Babble consensus group. The chain API
exposes `chains/createShard` and `chains/registerNode`; each shard runs its own
Babble instance with no cross-shard coordination, so aggregate throughput is
additive.

---

## Repository map

- `node/` — main node runtime (binaries `caspar-node`, `caspar-keygen`)
  - `node/src/` — action router, core transactions, chain, VM manager, telemetry
  - `node/crates/caspar-vm-plugins/` — **generated** VM plugin registration
- `vm-sdk/` — `caspar-vm-sdk`: the interface SDK every VM plugin implements
- `vms/` — pluggable VM runtime projects (`wasm`, `javascript`, `docker`,
  `fire`, `elpian`, `elpify`, plus any admin-added types)
- `cmd/casparctl/` — operator CLI (install / control / telemetry TUI / VM
  plugin selection / cluster)
- `client-cli/` — TypeScript client CLI for the shell API (see
  [Client CLI](09-client-cli.md))
- `sdk/` — Python client + sample creatures
- `wiki/` — the full project wiki (this documentation)
- `reports/` — benchmark run artifacts
- `bench-all.sh`, `run-nodes.sh`, `stop-nodes.sh`, `build-dist.sh` — operations
