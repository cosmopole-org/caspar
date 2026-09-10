# `javascript` — the QuickJS VM plugin

Runs deployed JavaScript program entities **in-process** on QuickJS
(`rquickjs`), with the full Caspar host-call ABI, the per-VM JSON transaction,
memory and stack limits, and a real interrupt-based execution deadline.

A creature written in JavaScript has exactly the same host capabilities as one
written for `wasm`. Only the language differs — and that is the property the op
table in `src/host_calls.rs` exists to protect: **it must stay in step with
`vms/wasm/src/host_calls.rs`**, because a creature should not be able to tell
which in-process runtime it landed on by the behaviour of a host call.

## Writing an entity

One self-contained script assigning `globalThis.update`. See
[`examples/counter.js`](examples/counter.js).

```js
globalThis.update = function (inputJson) {
  const packet = JSON.parse(inputJson);
  const doc = hostCall("getJson", { key: "Json::Thing::1", path: "doc" });
  return { ok: true, seen: doc.data };
};
```

`update` may be `async`; the runtime drives the promise job queue to quiescence
and reports what it settled on. Every host call is **synchronous** — there is no
host-side async to await — so a promise still pending when the queue goes quiet
is reported as a failure rather than left hanging.

There is **no module loader and no filesystem**. `acceptsExtraFiles` is false,
so an entity is one file; bundle anything larger with
`esbuild --bundle --format=iife --target=es2020`.

## What the guest gets

| Global | Notes |
|---|---|
| `hostCall(op, input)` | the ABI. Returns the parsed response. Also accepts a whole `{op, input}` object or its JSON string |
| `hostCallRaw(request)` | the same call, returning the host's bytes verbatim |
| `caspar` | frozen `{machineId, programId, vmId, storeId, runtime}` — what the NODE says this VM is |
| `console.log/info/debug/warn/error/trace` | routed to the VM's log stream, not the node's stdout |
| `btoa` / `atob`, `TextEncoder` / `TextDecoder`, `structuredClone` | in `src/prelude.js` |

QuickJS supplies the rest of ES2023.

## Output

Return a value from `update` — a string is used verbatim, anything else is
JSON-encoded — **or** call the `output` host op. An explicit `output` call
**wins**, so a creature ported from `wasm` behaves identically.

## Limits and termination

* `resources.ramMb` → the QuickJS heap limit. Exhausting it is a contained
  error, never an abort.
* A 1 MiB stack cap turns runaway recursion into a guest error rather than a
  host segfault.
* `resources.maxExecTimeSeconds` → an **interrupt handler**, so the deadline
  stops a `while (true) {}` — including one inside a `.then()`, because the job
  queue drains under the same handler.
* `terminateVm` trips the same interrupt, so it genuinely stops a running
  script. A terminate naming a `vmId` stops that instance; one naming only a
  machine stops every instance of it.
* A cancellable watchdog thread backs the interrupt for the one case it cannot
  reach: a run blocked in a host call rather than in JavaScript.

## No warm-VM pool

Unlike `wasm`, a context lives for exactly one run. Building a QuickJS context
costs microseconds rather than the tens of milliseconds WasmEdge's
Config/Store/Executor construction costs, so pooling would buy nothing — and a
fresh context makes state bleeding between signals impossible by construction.
`one_run_cannot_see_another_runs_globals` in `src/tests.rs` is what keeps that
true if it is ever revisited.

## Layout

```
src/lib.rs          register()
src/controller.rs   the VmPlugin: run/terminate/delete/status/exec/build, watchdog
src/runtime.rs      the QuickJS engine: JsMac, limits, interrupt, finalize
src/host_calls.rs   the op table (keep in step with vms/wasm)
src/prelude.js      the guest prelude
src/tests.rs        runtime tests against an in-memory host
examples/counter.js a complete, deployable creature
```

## Tests

```bash
cargo test -p caspar-vm-javascript --manifest-path ../../node/Cargo.toml
```
