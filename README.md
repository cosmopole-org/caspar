# Caspar Protocol 🌐

> **Status:** Docs current as of **2026-07-16** for the Rust codebase
> (`caspar-node` v0.1.0); the full guide lives in [`wiki/`](wiki/README.md).
> Benchmark figures are from the `reports/final` run (2026-05-29).

**Caspar** is a decentralised protocol stack that unifies hashgraph-style
Byzantine-fault-tolerant (BFT) consensus, federation-native messaging, and a
multi-runtime virtual-machine execution engine under a single **creature**
programming model. Nodes expose a signed binary action protocol over
mutual-TLS TCP/WebSocket transports, execute user-defined WebAssembly
creatures, replicate state through an embedded Babble hashgraph chain, and can
spawn subordinate VMs in any of **seven runtimes**: `wasm`, `docker`, `elpify`,
`elpian`, `javascript`, `firecracker`, and `modal` (cloud sandboxes).

The entire node, the local crates, and the `casparctl` operator CLI are
written in **Rust** (the previous Go implementation is retained only under
`node.old/`).

## ✨ Highlights

- **Hashgraph consensus** — leaderless asynchronous BFT via an embedded Babble
  (Rust) chain; consensus-bound writes are ordered through the chain, reads and
  signals bypass it.
- **Creature programming model** — application logic lives in isolated WASM
  modules (WasmEdge) with a per-creature RocksDB key-value namespace and a
  signal bus.
- **Unified multi-runtime VM router** — `build`/`run`/`exec`/`copy`/`terminate`
  operations dispatch to seven runtimes through one runtime-agnostic code path.
- **Per-VM persistent transaction** — all key-value mutations within a single
  creature signal commit atomically as one RocksDB write (no intra-signal write
  amplification, no partially-visible state).
- **elpify-chain** — a commit-reveal Proof-of-Stake validator election that runs
  inside WASM and is attested by Miden **STARK** zero-knowledge proofs.
- **Federation bridge** — authenticated cross-origin request/update propagation
  for composing creatures across independent deployments.
- **Geo-distributed instance mesh** — instances of the same origin form an
  edge-style global cluster replicated with **OpenRaft**: shell API state is
  available on every instance, creatures can opt into cluster-wide
  distributed deployment (`distribution: "cluster"`), and distributed VM
  state propagates through the consensus while local-mode VMs stay
  node-local. Orchestrated via `casparctl cluster …`
  (see [the cluster guide](wiki/08-consensus-federation-cluster.md#geo-distributed-cluster)).
- **Shard-parallel scale-out** — each shard is a self-contained three-node
  hashgraph group; aggregate throughput scales linearly with shard count.
- **Cross-protocol rate limiting** — one shared token-bucket limiter throttles
  client requests across the TCP, WebSocket, and HTTP-ingress transports, so a
  client's quota is unified regardless of protocol; per-user and per-IP tiers
  plus a node-wide safety net (see [rate limiting](wiki/01-overview-and-features.md#cross-protocol-rate-limiting)).
- **Telemetry** — a snapshot HTTP API and a live `casparctl stats` TUI.

## 📊 Current Measured State (`reports/final`, 2026-05-29)

Single shard = three local nodes (8074 / 8174 / 8274) sharing one Babble group.

| Metric | Value |
|--------|-------|
| Workflow correctness | **154 / 154** steps (9 suites, 100%) |
| Peak sequential throughput | **10.5 ops/s** (`chain:submitBaseTrx`) |
| Median consensus round | **≈95 ms** (84 ms floor) |
| Peak STARK throughput | **11.3 proofs/s** @ concurrency 2 |
| Concurrent load success | **100%** across C = 1…32 |
| WASM payload per node | **37 creatures, ≈11.6 MB** |

`reports/final/` is the authoritative benchmark artifact.

## 🧭 Repository Map

- `node/` — main node runtime (**Rust**; binaries `caspar-node`, `caspar-keygen`)
  - `node/src/` — action router, core transactions, chain, VM manager, telemetry
  - `node/crates/` — `caspar-vm-plugins` (generated VM plugin registration)
- `vm-sdk/` — `caspar-vm-sdk`: the interface SDK every VM plugin implements
- `vms/` — pluggable VM runtime projects (`wasm`, `javascript`, `docker`,
  `fire`, `elpian`, `elpify`, plus any admin-added types); see `vms/README.md`.
  Runtime library crates live with the plugin that uses them:
  `vms/wasm/crates/` (`wasmedge-sys`/`-types`/`-macro`, `async-wasi`),
  `vms/elpify/crates/elpify-lang` (Miden STARK), `vms/elpian/crates/elpian-vm`
- `cmd/casparctl/` — operator CLI (**Rust**): install / control / telemetry TUI
  / VM plugin selection (`casparctl vms …`)
- `client-cli/` — Caspar client CLI (**TypeScript**, `caspar-client`): shell-API
  client for creatures/programs + VM project template scaffolding for all seven
  runtimes (see [`client-cli/README.md`](client-cli/README.md))
- `wiki/` — full project wiki: overview, architecture, protocol, casparctl, VM
  SDK/plugins, the seven VM types, consensus/federation/cluster, and the client
  CLI (see [`wiki/README.md`](wiki/README.md))
- `sdk/` — Python client (`caspar_client.py`) + sample creatures
- `reports/` — benchmark run artifacts (`reports/final/` is current)
- `dist/` — pre-built artifacts (`caspar-node`, `caspar-keygen`, `casparctl`,
  WasmEdge lib, QuestDB jar); rebuilt & published by CI
- `bench-all.sh`, `run-nodes.sh`, `stop-nodes.sh`, `build-dist.sh` — operations
- `.github/workflows/build-node.yml` — CI that builds `caspar-node` and commits
  the refreshed binary into `dist/`
- `node.old/` — legacy Go implementation (reference only)

## 🛠️ `casparctl` CLI

```bash
# build & install the Rust CLI
make -C node casparctl-install      # or: cargo install --path cmd/casparctl
```

**Container flow (Docker + gVisor + nginx TLS proxy):**

```bash
casparctl install --name caspar-node
casparctl start
casparctl stats        # live telemetry TUI
casparctl pause | resume | stop | uninstall | purge
```

**Local flow (no Docker — runs the pre-built `dist/` binary directly):**

```bash
casparctl install --local   # once: verify requirements, generate keys/.env/genesis
casparctl run --detach      # start QuestDB + the node
casparctl status            # process / port / telemetry status
casparctl stop              # stop the local node (falls back to the container)
```

**Pick which VM types this node supports (plugin-based VMM):**

```bash
casparctl vms list             # discover the VM projects in vms/
casparctl vms disable docker   # exclude a VM type from the next build
casparctl vms enable docker    # include it again
casparctl vms sync             # regenerate the node's registration code
casparctl vms new myvm         # scaffold a brand-new VM plugin project
```

**Orchestrate the geo-distributed instance mesh (OpenRaft cluster):**

```bash
casparctl cluster status                                    # leader/membership/RTT
casparctl cluster add-peer --id 2 --addr eu.example.com:7440 --region eu-west
casparctl cluster apply -f cluster.json                     # whole-cluster config
casparctl cluster config set heartbeat_interval_ms 250      # one knob at a time
```

`casparctl stats` polls `TELEMETRY_API_PORT` (default `9099`) and renders live
throughput, latency percentiles, and consensus round counters. No agent runs on
the node — telemetry is served by the node's own HTTP listener. Full command
reference: [`wiki/04-casparctl.md`](wiki/04-casparctl.md).

## 🚀 Quick Start

**Run a node.** Either bring up a 3-node shard from source:

```bash
cd node && cp sample.env .env       # configure: OWNER_ID, ports, paths
make build                          # Rust; no Go toolchain -> target/release/caspar-node
cd .. && ./run-nodes.sh             # 3-node shard + QuestDB
```

…or, for the lightest single node (no Docker, uses the pre-built `dist/` binary):

```bash
casparctl install --local && casparctl run --detach
```

**Talk to it and deploy a VM** with the TypeScript client CLI. The node serves
plaintext transports directly (TLS is normally terminated by a proxy), so set
`CASPAR_TLS=0` for a direct connection:

```bash
cd client-cli && npm install && npm run build && npm install -g .

export CASPAR_HOST=127.0.0.1 CASPAR_TLS=0
caspar-client login alice alice@example.com     # authenticate against the node
caspar-client vm.init wasm ./my-vm main         # scaffold a deployable VM project
caspar-client creatures.createMachine 1 my-app "My app" demo   # -> creatureId
caspar-client programs.create ep <creatureId> /api/main wasm entry  # -> programId
caspar-client programs.deploy <programId> ./my-vm wasm '{}'    # build + deploy
caspar-client programs.run <programId>          # launch the VM
```

Full instructions: [Getting Started](wiki/03-getting-started.md) ·
[Client CLI](wiki/09-client-cli.md).

## 📚 Documentation

The **full project wiki** lives in [`wiki/`](wiki/README.md) — start there for an
end-to-end guide. Key pages:

- [Getting Started](wiki/03-getting-started.md) — prerequisites, build, run, lifecycle
- [Architecture](wiki/02-architecture.md) — subsystems and mechanisms
- [Caspar Protocol](wiki/05-caspar-protocol.md) — wire format, routes, host-call ABI, docker-host gateway
- [Casparctl](wiki/04-casparctl.md) — the operator CLI, every command
- [VM SDK & Plugins](wiki/06-vm-sdk-and-plugins.md) + [VM Types & Implementation](wiki/07-vm-types-and-implementation.md)
- [Consensus, Federation & Cluster](wiki/08-consensus-federation-cluster.md) — Babble behaviour/routing, rate limiting, cluster
- [Client CLI](wiki/09-client-cli.md) — the `caspar-client` command reference
- `sdk/README.md` — Python client SDK and samples
