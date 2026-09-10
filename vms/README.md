# Caspar VM plugins (`vms/`)

This folder holds the pluggable VM runtime projects of a Caspar node. Each
subfolder is a **standalone Rust project** implementing one VM type against the
`caspar-vm-sdk` interface crate (`../vm-sdk`). The Caspar node itself never
names a VM type — at build time every enabled plugin in this folder is
registered into the node binary, and at runtime the node resolves every VM
operation dynamically through the SDK's plugin registry.

## Default VM types

| folder        | key          | description                                        |
|---------------|--------------|----------------------------------------------------|
| `wasm/`       | `wasm`       | WasmEdge WebAssembly VM (the default runtime)      |
| `javascript/` | `javascript` | JavaScript entities on QuickJS (full host-call ABI) |
| `docker/`     | `docker`     | Docker containers (gVisor-sandboxed)               |
| `fire/`       | `fire`       | Firecracker microVMs                               |
| `elpian/`     | `elpian`     | Elpian AST interpreter                             |
| `elpify/`     | `elpify`     | Provable MASM/STARK runtime                        |

## Plugin project structure

```
vms/<key>/
├── Cargo.toml        # package name MUST be caspar-vm-<something>; lib crate
├── vm.config.json    # plugin metadata (key, aliases, deploy behaviour, …)
├── src/
│   ├── lib.rs        # exposes `pub fn register()`
│   ├── controller.rs # the VmPlugin (VM controller) implementation
│   └── ...           # models, runtime, anything the VM type needs
└── crates/           # optional: library crates owned by this VM type
    └── ...           # (e.g. vms/wasm/crates/wasmedge-sys,
                      #  vms/elpify/crates/elpify-lang,
                      #  vms/elpian/crates/elpian-vm)
```

Requirements for a valid plugin:

1. `Cargo.toml` defines a **library** crate; the package name is the plugin's
   identity in the build graph.
2. `vm.config.json` describes the runtime. Minimum: `{"key": "<runtime key>"}`.
   The full schema is `caspar_vm_sdk::VmPluginMeta` (aliases, artifact
   extensions, `inProcess`, `defaultRuntime`, `entityFileName`,
   `acceptsExtraFiles`, `buildOnDeploy`, `setEntityLinksOnDeploy`,
   `supportsChainTrxs`, `restorable`).
3. `src/lib.rs` exposes `pub fn register()` which parses the config and calls
   `caspar_vm_sdk::registry::register_plugin(...)` with a type implementing
   `caspar_vm_sdk::VmPlugin` — the VM lifecycle API (run/terminate/exec/
   copy/build), snapshot restore, deploy/run/stop plans for the program
   shell, terminate-request building, optional proof verification and
   IP-based instance identification.
4. Every interaction with the node goes through `caspar_vm_sdk::host()`
   (`VmHost`): packet dispatch, VM context registry, state DB, per-VM
   transactions, resource locks, HTTP, logging.

## Adding a new VM type

```sh
casparctl vms new <key>          # scaffold vms/<key> from a template
# ... implement the controller ...
casparctl vms list               # verify it is discovered
casparctl vms sync               # regenerate the node's plugin registration
./build-dist.sh                  # rebuild the node with the plugin compiled in
```

Or copy any existing plugin folder next to the defaults, adjust its
`Cargo.toml` package name, `vm.config.json` key and controller, then run
`casparctl vms sync`.

## Enabling / disabling VM types

The host admin picks which VM types the node binary supports:

```sh
casparctl vms list               # shows every plugin and its enabled state
casparctl vms disable docker     # exclude a plugin from the next build
casparctl vms enable docker      # include it again
casparctl vms sync               # apply the selection (regenerates code)
```

The selection is stored in `vms/vms.state.json`; `casparctl vms sync`
regenerates the aggregation crate at
`node/crates/caspar-vm-plugins/` (marked `@generated`) which is the ONLY
place plugin crates are imported and registered. `build-dist.sh` runs the
sync automatically before compiling the node, and also accepts
`--disable-vm <key>[,<key>...]` for one-shot selection during install.
