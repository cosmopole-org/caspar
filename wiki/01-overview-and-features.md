# 01 — Overview & Features

Caspar is a single Rust binary (`caspar-node`) that runs several concurrent
services and hosts user logic as WebAssembly **creatures**, while being able to
spawn subordinate VMs in seven different runtimes. This page describes each
headline feature in its own section, briefly and completely.

---

## Hashgraph consensus (BFT)

Consensus is provided by an **embedded Babble (Rust) hashgraph chain** —
leaderless, asynchronous, Byzantine-fault-tolerant. The node registers a
`commit_handler` that receives ordered blocks of transactions from the chain
and fans them out to application state. Only writes that need global ordering
go through the chain; reads and signals bypass it (see
[Consensus routing](#consensus-routing)).

---

## Creature programming model

A **creature** is the fundamental unit of application logic:

- **Code** — a `.wasm` binary loaded into a WasmEdge instance.
- **State** — a per-creature RocksDB namespace reached through host calls
  (`putJson`, `getJson`, `getByPrefix`, `delKey`); state persists across signals.
- **Signals** — `signalGroup` fans a message to elected validators / group
  members without passing through consensus (sub-consensus fan-out).
- **VM ops** — `buildVmImage`, `runVm`, `execVm`, `copyToVm`, `terminateVm`
  let a creature orchestrate subordinate VMs in any runtime.
- **Transactions** — `commitTrx` checkpoints the per-VM transaction.

Creatures are isolated from each other; all platform access flows through the
single `hostCall` ABI (see [Protocol](05-caspar-protocol.md#the-host-call-abi)).

---

## Unified multi-runtime VM router

`build` / `run` / `exec` / `copy` / `terminate` / `delete` operations dispatch to seven
runtimes through one runtime-agnostic code path (`dispatch_packet` /
`route_vm_packet`). Each operation injects a canonical `"type"` field and
delegates; the router is the single locus of per-runtime branching, so adding a
runtime is a one-file change. The seven runtimes are `wasm`, `docker`, `elpify`,
`elpian`, `javascript`, `fire`, `modal` — see
[VM Types](07-vm-types-and-implementation.md).

---

## Pluggable VM types

The node itself never names a VM type. Each runtime is a **standalone Rust
project** under `vms/` implementing the `caspar-vm-sdk` interface. At build
time every *enabled* plugin is compiled into the node and registered; at
runtime the node resolves every VM operation dynamically through the plugin
registry. The host admin picks which types the binary supports with
`casparctl vms enable|disable`, and scaffolds new ones with `casparctl vms new`.
See [VM SDK & Plugins](06-vm-sdk-and-plugins.md).

---

## Per-VM persistent transaction

Each active VM holds a single `TrxWrapper` (one RocksDB transaction) for its
lifetime, stored in a per-VM registry inside the `ICore` context. All key-value
mutations within a creature signal commit **atomically** — on an explicit
`commitTrx` or on VM finalisation — so there is no intra-signal write
amplification and partial state is never exposed between host calls.

---

## elpify-chain (STARK validator election)

A commit-reveal Proof-of-Stake validator election runs entirely inside WASM and
is attested by Miden **STARK** zero-knowledge proofs. Its five phases are Stake
→ Commit → Reveal → electionTick (MASM program proven by the `elpify-lang`
crate) → Validator consensus. Proving is `O(n log² n)` and verification
`O(log² n)`, so adding a validator is far cheaper than generating the proof.
See [Consensus](08-consensus-federation-cluster.md#elpify-chain).

---

## Federation bridge

Each node runs a federation transport (`FEDERATION_API_PORT`) that validates the
signature and origin of inbound packets against a known-origins registry before
admitting them to the local action router. Outbound calls propagate creature
updates and chain events to registered peers, enabling cross-deployment
creature composition and cross-shard event delivery **without** replicating
consensus history.

---

## Geo-distributed instance mesh (OpenRaft cluster)

Instances of the same origin form an edge-style global cluster replicated with
**OpenRaft**: shell-API state is available on every instance, creatures can opt
into cluster-wide distributed deployment (`distribution: "cluster"`), and
distributed VM state propagates through consensus while local-mode VMs stay
node-local. Orchestrated via `casparctl cluster …` (see
[Cluster](08-consensus-federation-cluster.md#geo-distributed-cluster)).

---

## Shard-parallel scale-out

A shard is a self-contained three-node Babble consensus group with no
cross-shard coordination, so aggregate throughput is additive:
`TPS_network = S × TPS_shard`. The chain API exposes `chains/createShard` and
`chains/registerNode`.

---

## Cross-protocol rate limiting

One shared token-bucket limiter (`IRateLimiter`, hung off `ITools`) throttles
client requests across the TLS-TCP, TLS-WebSocket, and VMM HTTP-ingress
transports. All three consult the **same** limiter instance keyed on identity —
never on the wire the request arrived on — so a client cannot multiply its quota
by spreading load across protocols. Each identity's bucket holds up to `burst`
tokens, refills at `rate_per_sec`, and spends one token per request.

Three tiers:

| Tier | Keyed by | Purpose | Default |
|------|----------|---------|---------|
| **Authenticated** | verified `user_id` | fair per-user quota | 50 rps, burst 100 |
| **Anonymous** | peer IP | pre-auth traffic (handshakes, `authenticate`); blunts connect floods | 10 rps, burst 20 |
| **Global** | node-wide | aggregate safety net against distributed floods | 5000 rps, burst 10000 |

The key derives only from **verified** identity (the `user_id` a socket pinned
*after* a signature check, never the claimed id), so a spoofed id can't mint a
fresh bucket; pre-auth traffic is billed to the hard-to-spoof peer IP. A
rejected request returns action code `8` (TCP/WS) or `429 Too Many Requests`
with `Retry-After` (HTTP), both carrying a `retryAfterMs` hint and the `scope`
(`identity` vs `global`). Per-identity buckets live in a `DashMap` swept by a
background thread (`RATE_LIMIT_IDLE_EVICT_SECS`, default 300 s), so the map can't
grow unbounded. Every knob is an env var (`RATE_LIMIT_ENABLED`,
`RATE_LIMIT_AUTH_RPS`/`_BURST`, `RATE_LIMIT_ANON_RPS`/`_BURST`,
`RATE_LIMIT_GLOBAL_RPS`/`_BURST`); unset/malformed values fall back to defaults,
and disabling admits every request.

---

## Telemetry

A cached snapshot is served over HTTP at `GET /telemetry/snapshot`
(`TELEMETRY_API_PORT`, default `9099`) and consumed by the live `casparctl stats`
TUI. It carries node, chain, federation, client, VM, machine, cost,
transaction, packet, message, creature, validator, staking, and election
metrics. No agent runs on the node — telemetry is served by the node's own HTTP
listener.

---

## Runtime profiler (pprof)

The node exposes a Rust-native `pprof` HTTP server on `:9999` with runtime,
heap, thread, flamegraph, and CPU-profile endpoints, queried by
`casparctl pprof`.

---

## Security model 🔒

- Signed request packets (identity + signature validation) over **mutual TLS**
  (Rustls + ring); identity keys are secp256k1 (`k256`), server crypto RSA + SHA-2.
- **Guard-based authorisation** on action routes.
- Store / member / access checks before state mutation.
- Federated packets validated against known origins.
- WASM provides capability-based logical isolation; for cross-tenant secrets a
  defence-in-depth deployment pins the Firecracker / container runtimes.

---

## Consensus routing

Whether an action is ordered through the chain is decided by its input's
`origin` field, **not** the route name:

- `origin == "global"` → consensus-bound (e.g. `creatures.create`,
  `programs.deploy`, `chains/*`). Produces `Adding Transaction → Commit block=N`.
- `origin == ""` → local (reads, `creatures.signal`, dev `login`). No chain
  activity.
