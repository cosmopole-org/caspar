# The meter could never settle

**Where:** `node/src/drivers/vmm/host/vm_host_functions.rs`, the dispatch for
`startHold` / `settleHold` / `releaseHold` / `reservePool` / `settlePool` /
`releasePool` / `debitPool`.

## What was wrong

Each of those resolved its calling program with `resolve_cached_vm_hierarchy`,
which looks the caller up in the out-of-band **VM** context by `vmId` and
returns an empty program id for anything that is not a VM.

Every registered meter on the Decillion platform is a **WASM creature**
(`crew/message`). A creature host call carries no `vmId`, so the caller resolved
to `""` and `host_fn_pool_authority_call` refused it before doing anything else:

```
verified meter program and poolId are required
```

The message names two conditions and the caller could see neither, so it read as
a malformed payload. It was an anonymous caller.

The consequence is the whole of it: **no pool reservation and no settlement could
ever be made by a creature meter, so no finished run was ever billed.** Runs
completed, answered, and recorded zero charge — which is exactly what the
platform's own inspection observed and could not explain.

## The fix

Use `ctx.program_id` — the same node-resolved identity `publishUpdate`,
`registerBridgeToken` and `deleteVm` already trust. It is stamped by the runtime
onto the packet, not reachable by the guest, and it still prefers the cached VM
context when there is one, so a container meter resolves exactly as before.

Nothing about the authorization is loosened: `host_fn_pool_authority_call` still
checks that the resolved caller is the pool's registered `meterProgramId` and
that this node owner is its settlement authority.

## How it was found

A live end-to-end run on a real Modal sandbox. The Decillion side now records
which program the node saw asking when a settlement is refused
(`settlementCaller` on the run record), which is what turned "required" into
"the caller is 140@global and the node still says anonymous".
