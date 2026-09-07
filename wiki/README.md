# Caspar Wiki 🌐

A complete, self-contained guide to **Caspar** — how it works, its features, and
how to operate, extend, and build on it. It is written so that a human or an AI
agent handed this repository can:

- **install and manage a Caspar node's whole lifecycle** through `casparctl`,
- **use the Caspar client CLI** (`client-cli/`) to do anything the node's shell
  API exposes,
- **understand the Caspar protocol** — the signed action wire format and the
  way VMs communicate with the host, and
- **implement a new Caspar-based VM** in any of the seven runtime plugin types.

## What is Caspar?

Caspar is a decentralised protocol stack that unifies hashgraph-style
Byzantine-fault-tolerant (BFT) consensus, federation-native messaging, and a
multi-runtime virtual-machine execution engine under a single **creature**
programming model. Nodes speak a signed binary action protocol over
mutual-TLS TCP/WebSocket transports, execute user-defined WebAssembly
creatures, replicate state through an embedded Babble hashgraph chain, and can
spawn subordinate VMs in seven runtimes: `wasm`, `docker`, `elpify`, `elpian`,
`javascript`, and `firecracker` (`fire`).

The node, its crates, and the `casparctl` operator CLI are written in **Rust**.
The client CLI is written in **TypeScript**.

## Wiki map

| Page | What it covers |
|------|----------------|
| [01 — Overview & Features](01-overview-and-features.md) | Every headline feature, one section each |
| [02 — Architecture](02-architecture.md) | Subsystems, runtime topology, the creature model |
| [03 — Getting Started](03-getting-started.md) | Prerequisites, build, run, install lifecycle |
| [04 — Casparctl](04-casparctl.md) | The operator CLI, every command in its own section |
| [05 — Caspar Protocol](05-caspar-protocol.md) | Wire format, routes, host-call ABI, VM↔host protocol |
| [06 — VM SDK & Plugins](06-vm-sdk-and-plugins.md) | The plugin interface and how to implement one |
| [07 — VM Types & Implementation](07-vm-types-and-implementation.md) | All seven VM types + how to build each |
| [08 — Consensus, Federation & Cluster](08-consensus-federation-cluster.md) | Hashgraph, elpify-chain, federation, sharding, mesh |
| [09 — Client CLI](09-client-cli.md) | The `caspar-client` CLI, every command + VM templates |

## The three core concepts

1. **Creature** — the fundamental unit of application logic: a WebAssembly
   module with its own persistent key-value namespace and a signal bus. A
   creature is addressed by an id like `123@global`.
2. **Program** — a deployable, runnable entity attached to a (machine-type)
   creature. A program targets one of the seven VM runtimes; deploying it ships
   its artifact to the node, and running it launches a VM.
3. **VM plugin** — a standalone Rust project under `vms/` that implements one
   runtime against the `caspar-vm-sdk` interface. The node never names a VM
   type; it resolves everything dynamically through the plugin registry.

## Fastest paths

- **Operate a node:** [Getting Started](03-getting-started.md) →
  [Casparctl](04-casparctl.md).
- **Talk to a node / deploy a VM:** [Client CLI](09-client-cli.md).
- **Write a new VM runtime:** [VM SDK & Plugins](06-vm-sdk-and-plugins.md) →
  [VM Types & Implementation](07-vm-types-and-implementation.md).
- **Understand the wire protocol:** [Caspar Protocol](05-caspar-protocol.md).
