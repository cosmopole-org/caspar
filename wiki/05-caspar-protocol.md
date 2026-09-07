# 05 — Caspar Protocol

Caspar exposes several surfaces. This page documents the **signed binary action
protocol** (how clients talk to the node) and the **VM↔host protocols** (how a
running VM communicates with the node): the WASM guest `hostCall` ABI and the
JSON VM packet router. Each is a self-contained section.

Caspar's surfaces:

1. Signed binary action protocol (mutual-TLS TCP + TLS WS) — client ⇄ node.
2. Entity / stream endpoints (`ENTITY_API_PORT`, `VM_API_PORT`).
3. Hashgraph service HTTP endpoints (`BLOCKCHAIN_API_PORT`).
4. Telemetry snapshot HTTP endpoint (`TELEMETRY_API_PORT`).

---

## Binary action protocol — framing

Every message is length-prefixed:

```text
[4 bytes body_len (big-endian)]
[body]
```

The `body` layout:

```text
[4 bytes signature_len][signature]
[4 bytes user_id_len][user_id]
[4 bytes path_len][path]
[4 bytes request_id_len][request_id]
[payload_json]
```

This is exactly what the client CLI builds in `createRequest`
(see [Client CLI](09-client-cli.md)).

---

## Frame tag bytes

A single leading byte tags each frame:

- `0x01` — asynchronous update / signal frame (e.g. `creatures/signal/result`).
- `0x02` — synchronous response frame (carries `request_id` + status + payload).
- `0x03` — request frame.

After processing each inbound frame a client sends a 5-byte flow-control ack
(`00 00 00 01 01`).

---

## Status codes

- `0` success
- `1` action not found
- `2` parse/validation error
- `3` execution error
- `4` auth/authorization failure

---

## Request signatures

Signed actions carry a base64 RSA-SHA256 signature over the exact payload
bytes, verified against the public key registered at login/create. Two padding
schemes are accepted:

- **RSA-PSS** (primary — what the Python SDK, the deploy tooling, and the
  client CLI produce), and
- **RSASSA-PKCS#1 v1.5** (fallback — for mbedTLS-backed clients such as a Godot
  game client using `Crypto.sign`).

Both bind the same key and digest; prefer PSS when the crypto stack supports it.

---

## Consensus routing

Whether an action is ordered through the Babble chain is determined by its
input's `origin` field, **not** by the route name:

- `origin == "global"` → consensus-bound (e.g. `creatures.create`,
  `creatures.createMachine`, `programs.create`, `programs.deploy`, `chains/*`).
  Produces `Adding Transaction → Commit block=N`.
- `origin == ""` → local (reads, `creatures.signal`, dev `login`). No chain
  activity.

---

## Route groups

### Auth
- `GET /auths/getServerPublicKey`, `/auths/getServersMap`

### Users / creatures
- `POST /users/authenticate`, `/users/transfer`, `/users/mint`, `/users/checkSign`
- `POST /users/lockToken`, `/users/consumeLock`, `/users/login`, `/users/create`, `/users/delete`, `/users/update`
- `GET /users/meta`, `/users/get`, `/users/getByUsername`, `/users/find`, `/users/list`

> In the current model these are addressed as `/creatures/*` by the client CLI
> (`/creatures/login`, `/creatures/authenticate`, `/creatures/get`,
> `/creatures/create`, `/creatures/signal`, …).

### Gateway (bridge subscriptions)
- `POST /gateway/subscribe`, `/gateway/unsubscribe`, `/gateway/signal`
  — see [The gateway subscription channel](#the-gateway-subscription-channel-gateway).

### Stores
- `POST /stores/addMachine`, `/stores/listMachines`, `/stores/updateProgram`, `/stores/removeMachine`
- `POST /stores/addProgram`, `/stores/removeProgram`, `/stores/addMember`, `/stores/updateMember`
- `POST /stores/setAccess`, `/stores/getAccess`, `/stores/readMembers`, `/stores/removeMember`
- `POST /stores/create`, `/stores/join`, `/stores/leave`, `/stores/signal`, `/stores/history`
- `PUT /stores/update`, `DELETE /stores/delete`, `GET /stores/meta`, `/stores/get`, `/stores/read`, `/stores/list`

#### Store signals: the messaging layer

`/stores/signal` and `/stores/history` are the node's own messaging. A signal is
addressed to a store; the node fans it out live to every connected member and,
when the store was created with `persHist`, writes it to the time-series signal
log. `/stores/history` reads that log back. Together they are enough to build a
conversation — the sender keeps no transcript of its own.

```
POST /stores/signal   { storeId, data, tags: ["kind=message", "thread=main"], temp? }
   → { passed, persisted, signalId, time, tags }        // + live fan-out on `stores/signal`
POST /stores/history  { storeId, tagsAll?, tagsAny?, beforeTime?, afterTime?, count? }
   → { storeId, signals: [{ id, userId, data, tags, time, edited }] }
```

**Tags** are the sender's labels, stored with the packet and the only thing
`/stores/history` filters on: `tagsAll` must all be present, `tagsAny` needs at
least one. A tag is a short `key=value` (or bare) label limited to alphanumerics
and `= @ . : - / + #` — a tag carrying the column separator, a quote or a SQL
wildcard is **rejected**, so a malformed tag fails its signal instead of quietly
widening someone else's filter. `temp: true` delivers a signal live without
recording it, for traffic that is meaningless once seen.

**Permissions.** `onaccess::<storeId>::<memberId>` holds a permission set —
`read`, `signal`, `manage`. `/stores/signal` requires `signal`, `/stores/history`
requires `read`, and `/stores/setAccess` requires `manage`. An absent or
unparseable grant means **no permissions**: there is no implicit default, so a
member whose grant was never written is refused rather than silently allowed. A
grant is stated when access is created (`createAccess`/`updateAccess` take an
explicit `permissions` list) — which is how a viewer is given `read` alone and
cannot post.

**Federation.** Every input carries an `origin`; a store owned by another node has
the whole action routed there and served against that node's log, so a
federation's stores are readable and writable through the same two calls without
replicating their signals into chain state. Members on a peer node also receive
the live fan-out (the persisting node pushes it to each peer holding a member).

**Creatures and VMs** reach the same log through the `signal` and `readSignals`
host calls, so an agent runtime reconstructs a conversation from exactly the rows
a client sees. To perform an action that is genuinely its own — storing media it
produced, say — a creature calls `execShellAction` with `asSelf: true`: the node
runs the named action under the creature identity it resolved for that VM, never
one the guest names, and refuses the call outright when it cannot resolve one.

### Invites
- `POST /invites/create`, `/invites/listStoreInvites`, `/invites/listUserInvites`, `/invites/cancel`, `/invites/accept`, `/invites/decline`

### Machines + Programs
- Machines: `/machines/create`, `/machines/delete`, `/machines/update`, `/machines/myCreated`, `/machines/signal`, `/machines/runProgramEntity`, `/machines/stopProgramEntity`, `/machines/readBuildLogs`, `/machines/readMachineBuilds`, `/machines/deploy`, `/machines/list`, `/machines/listProgramMachines`
- Programs: `/programs/create`, `/programs/deploy`, `/programs/runEntity`, `/programs/stopEntity`, `/programs/update`, `/programs/delete`, `/programs/list`, `/programs/readVmLogs`

### Storage
- `POST /storage/upload`, `/storage/uploadUserEntity`, `/storage/deleteUserEntity`, `/storage/uploadStoreEntity`, `/storage/uploadAppEntity`, `/storage/deleteStoreEntity`, `/storage/download`

### Chains
- `POST /chains/create`, `/chains/createShard`, `/chains/createFromStore`, `/chains/submitBaseTrx`, `/chains/registerNode`

### PC + Misc
- `POST /pc/runPc`, `/pc/execCommand`
- `GET /api/hello`, `/api/time`, `/api/ping`

---

## HTTP surfaces

### Entity API (`ENTITY_API_PORT`)
`/storage/downloadUserEntity`, `/storage/uploadUserEntity`,
`/storage/uploadStoreEntity`, `/storage/uploadAppEntity`,
`/storage/downloadAppEntity`, `/storage/downloadStoreEntity`, `/stream/get`,
`/stream/send`.

### VM stream API (`VM_API_PORT`)
`/stream/send`.

### Telemetry API (`TELEMETRY_API_PORT`, default `9099`)
`GET /telemetry/snapshot`.

### HTTP ingress to VMs
The VMM exposes an HTTP ingress that accepts requests in two shapes:

- **Fully-qualified identity** —
  `{node instance url}/{creatureId}/{programId}/{entityId}/{vmId}/{path…}`. It
  strips the four identity segments, packages the remaining request, and calls
  the entity's runtime plugin (`forward_http`).
- **Custom route** —
  `{node instance url}/{creatureUsername}/{customPath…}`. A deployer may bind a
  VM entity's HTTP server to a friendly path at deploy time by passing
  `metadata.gatewayPath` (a prefix, e.g. `api` or `api/v1`) — and optionally
  `metadata.gatewayVmId` to pin a specific instance — to `/programs/deploy`. The
  node records the route on chain keyed by the owning creature id and the
  normalized prefix (`vmHttpRoute::<creatureId>::<prefix>`), so it replicates
  with a cluster deploy and a redeploy reconciles a changed/removed path. At
  request time the ingress resolves the leading segment to the owning creature
  and matches the longest registered prefix for it, forwarding the remaining
  sub-path to the VM. The leading segment may be, in resolution order: the full
  creature **username** (`name@source`, via the creature index); the bare
  **local part** of that username (`name`, via a `vmHttpRouteUser::<name>` alias
  written when the route is registered); or the creature **id** (`7@global`).
  The short local-part form is the friendly default (`/m-tool-github/…`) — the
  full username usually cannot go in a URL path because the node's `source` is
  URL-shaped (`name@http://host:port`), and the id is opaque. One creature can
  expose several entities under different paths. When no custom route matches,
  the fully-qualified identity form is parsed instead.

  A standalone serving entity started with `/programs/runEntity` may also carry
  `gatewayPath`: the node then binds the same custom route to *that* launched
  instance (its vm id), so the fixed URL points at the warm serving container and
  is re-pointed at the fresh instance on every redeploy. This is how a docker
  tool with an inbound HTTP callback (e.g. the github tool's OAuth callback) gets
  one permanent URL to register with an external provider.

By default the request is wrapped into a `creatures/signal` and delivered
asynchronously (a `202 Accepted`); runtimes with a long-lived HTTP server inside
the VM (e.g. docker) override this to proxy directly.

---

## The host-call ABI (WASM guest ⇄ host)

Every interaction a WASM creature has with the platform flows through a single
guest import, **`hostCall`**, which takes a JSON request in guest memory and
returns a packed `(offset << 32 | len)` handle to a JSON response. The request
shape is:

```json
{ "op": "<operation>", "input": { ... } }
```

The host recognises these operations (from `vms/wasm/src/host_calls.rs`):

| `op` | Meaning |
|------|---------|
| `output` | Set the VM's execution result (`input.text`). |
| `consoleLog` | Emit a runtime log line (`input.text`). |
| `dbOp` | Low-level raw KV op: `put` / `del` / `get` / `getByPrefix` (namespaced by machine id). |
| `putJson` / `getJson` / `getByPrefix` / `delKey` | Per-VM **JSON transaction** ops on the VM's single lifecycle transaction. |
| `commitTrx` | Commit & reset the per-VM JSON transaction and flush the raw dbOp buffer. |
| `lockResource` / `unlockResource` | Acquire / release a named resource lock. |
| `runVm` / `terminateVm` / `execVm` / `copyToVm` / `buildVmImage` | Orchestrate subordinate VMs (any runtime) via the VM packet router. |
| `deleteVm` | **Permanently** destroy a VM this creature launched. `runVm` records the launching program; a delete is allowed for that program or a sibling creature of the same owner (a deployment is many programs — the action that creates a resource is never the one that deletes it), and refused for anyone else. A VM with no recorded owner is refused rather than allowed. |
| `registerBridgeToken` / `revokeBridgeToken` / `publishUpdate` | The [gateway subscription channel](#the-gateway-subscription-channel-gateway): mint bearer tokens for a program running outside Caspar, and push updates to the ones holding a socket open. |
| `httpPost` / `httpRequest` | Perform an outbound HTTP request on behalf of the VM. |
| `verifyProgramExecution` (`elpifyProof`) | Verify a program-execution proof via the provable runtime plugin. |
| *anything else* | Forwarded to the unified host-call dispatcher (`signalUser`, `signalGroup`, …), with `programId`/`machineId` injected. |

Guest exports the host relies on: `malloc` (to allocate the response buffer),
`memory`, and an entry point such as `update`. Legacy single-purpose exports
(`output`, `consoleLog`, `plantTrigger`, `httpPost`, `runDocker`, `execDocker`,
`copyToDocker`, `signalStore`, `trxPut/Get/Del/GetByPrefix`) are still supported
for older creature SDK builds.

---

## The VM packet router (host-side dispatch)

Host-side, every VM operation is a JSON **packet** carrying a canonical `type`
(and usually a `runtime`/`vmType` hint). The router resolves the responsible
plugin and calls the matching method:

| Packet `type` | Plugin method |
|---------------|---------------|
| `runVm` | `run_vm` |
| `terminateVm` | `terminate_vm` |
| `execVm` | `exec_vm` |
| `copyToVm` / `copyFromVm` | `copy_to_vm` / `copy_from_vm` |
| `buildVmImage` | `build_image` |
| `verifyProgramExecution` | `verify_program_execution` |

Runtime resolution order (`registry::resolve_for_packet`): explicit `runtime`
field → `vmType` hint → artifact-extension detection → the default runtime.
For exec/copy/build packets a legacy alias suffix is also honoured
(`execDocker` → `docker`). VMs emit results back to the node by dispatching
`vmOutput` / `vmLog` / `signal` packets through the same channel. The full
plugin contract is in [VM SDK & Plugins](06-vm-sdk-and-plugins.md).

---

## The docker-host bridge gateway

WASM/elpian creatures reach the host in-process through `hostCall`. **Docker**
creatures can't — they run sandboxed (gVisor/`runsc`) on the gateway docker
network with **no other route out**. Their single channel to the node is one
long-lived TCP connection to the **docker-host bridge gateway** (default port
`8079`, `DOCKER_HOST_GATEWAY_PORT`; owned by the `Vmm` driver, started via
`tools().vmm().start_docker_gateway(port)`). Over that one socket a container
does everything: DB/storage host calls, outbound HTTP, signalling siblings,
spawning/terminating VMs — and it **receives** signals pushed back over the same
socket.

**Identity is derived from the docker source IP — never declared.** A container
holds no secret and is given no identity, only the gateway address
(`CASPAR_GATEWAY_HOST`/`CASPAR_GATEWAY_PORT`, plus
`--add-host host.docker.internal:host-gateway`). When the node launches a docker
creature it records `container name → (vmId, creatureId, programId, machineId)`.
On the container's `HELLO`, the node takes the connection's docker-network source
IP and asks the docker daemon (bollard `list_containers`) which container owns
that IP, maps it to the registered identity, and pins that identity to the
connection for its lifetime — stamping it onto every request and ignoring any
identity fields the container sends. A sandboxed container can't forge another
container's bridge IP, so this gives docker creatures the same spoof-resistant
posture as the in-process `hostCall` (which stamps the node-created runtime's own
ids).

### Wire protocol (chunked)

```text
[u32 BE frame_len][frame body]
frame body: [u8 op][u64 BE message_id][u64 BE correlation_id][u32 BE seq][u32 BE total][chunk]
```

A logical message larger than `MAX_CHUNK` (64 KiB) is split across frames
sharing `message_id` and reassembled (out-of-order tolerant); a single-chunk
message uses `seq = 0, total = 1`.

| op | name | direction | payload |
|----|------|-----------|---------|
| `0x01` | HELLO | container → node | `{}` (identity from source IP) |
| `0x02` | WELCOME | node → container | `{ok, sessionId, vmId, machineId, creatureId, programId}` |
| `0x10` | REQUEST | container → node | `{op, input}` |
| `0x11` | RESPONSE | node → container | host-function result (JSON) |
| `0x20` | SIGNAL | node → container | `{key, data}` |
| `0x30` / `0x31` | PING / PONG | container ↔ node | `{}` / `{ok}` |
| `0x40` | ERROR | node → container | `{ok:false, error}` (ends session) |

A `REQUEST`'s `{op, input}` accepts any name `handle_unified_host_call` supports
— the same entry point the in-process runtimes use (`dbOp`, `httpRequest`,
`signal`, `signalUser`, `signalGroup`, `runVm`, `createStore`, …) — and the
result is returned verbatim as the `RESPONSE`. When another creature signals the
machine, the node pushes a `SIGNAL` frame onto the container's connection instead
of cold-spawning a new VM. Config: `DOCKER_HOST_GATEWAY_PORT` (≤ 0 disables) and
`DOCKER_HOST_GATEWAY_ADVERTISE_HOST` (default `host.docker.internal`).

---

## The gateway subscription channel (`/gateway/*`)

The two fan-out paths above both need the receiver to **be** a creature:
`signal_user` addresses one by id, and `signal_store` resolves a store's
members from their access grants. A program running *beside* a VM — the crewAI
bridge inside a Modal sandbox, say — is neither. It has no Caspar key, so it
cannot sign an action, and no host ABI, because it is not the VM.

Such a program authenticates with a **bearer token its owning creature
minted**, and that grant is the whole of its authority.

```text
  creature ── registerBridgeToken ─►  grant: token hash → topics + owner
  bridge   ── /gateway/subscribe ──►  bound to those topics on this socket
  bridge   ── /gateway/signal ─────►  signals the granting creature
  creature ── publishUpdate ───────►  every subscriber of the topic
```

**Creature side (host ops):**

- `registerBridgeToken {token, topics[], ttlSecs, deliverTo}` — mint or replace
  a grant. The owner is the node-resolved calling program, never an input
  field. `deliverTo` names the creature the bridge's inbound signals reach, and
  is usually a *different* creature from the minter: a space's `create` mints
  the grant, but the bridge's messages belong to the crew creature that handles
  them. It is not an escalation — a creature can already signal any creature
  directly.
- `revokeBridgeToken {token | tokenHash}` — drop it. Revoking an unknown token
  is a no-op, so a bridge tearing itself down twice does not fail.
- `publishUpdate {topic, key, data}` — push one packet to every connection
  subscribed to a topic this creature owns.

**Bridge side (anonymous client actions):**

- `/gateway/subscribe {token, topics[]}` — verifies the token and answers with
  the granted topics. A requested list can only ever *narrow* the grant.
- `/gateway/unsubscribe {token}`.
- `/gateway/signal {token, topic, action, payload, correlationId}` — the
  inbound direction. The target creature is taken from the **grant**
  (`deliverTo`), not the request, so a token can only reach the handler its
  owner nominated.

Properties worth knowing:

- The token is stored only as its SHA-256, so a state dump hands nobody a
  working credential.
- A topic is claimed by the creature that first grants it — in practice by its
  **owner**, since a deployment is many creatures (each action is its own
  program) and the one that mints a grant is a sibling of the one that later
  publishes to it. Another tenant's creature is refused, which is the property
  that matters.
- Delivery never blocks the publisher: a subscriber's sink hands the frame to
  that connection's own outbound queue and returns.
- A subscriber whose connection has gone is reaped on the next publish, and
  dropped outright when its socket closes — a bridge that reconnects (a sandbox
  restart) must not leave its predecessor behind.
- The action authenticates; the **transport** binds the socket, because only it
  can write to the connection. That is why `/gateway/subscribe` returns a
  `gatewaySubscribe` block the WS driver consumes.

---

## Signals and the async result channel

`creatures.signal` sends a signal to a creature/program entity. When the payload
is a JSON object, the client injects a `correlationId`; the VM handles the
signal and emits its result as a `0x01` frame keyed `creatures/signal/result`
carrying the same `correlationId`, which the client matches to resolve the call.
This makes signals a request/response channel on top of the fire-and-forget
signal bus. `programId`/`entityId` are forwarded at the top level so the node
routes the packet to the program's VM listener (registered under the
`programId`).
