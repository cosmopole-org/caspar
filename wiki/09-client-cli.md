# 09 — Client CLI (`caspar-client`)

`client-cli/` is a TypeScript/Node.js client for a Caspar node's **signed binary
action protocol** (the "Caspar shell API"). Every command maps directly to a
node shell action route (`/creatures/*`, `/programs/*`) — there is no hosted
backend, billing, or miniapp layer involved. It also scaffolds deployable VM
projects for all seven runtimes. Each command is documented in its own section.

---

## Install & build

```bash
cd client-cli
npm install
npm run build            # tsc -> dist/index.cjs
npm install -g .         # exposes the `caspar-client` binary
caspar-client help
```

---

## Connecting to a node

The CLI talks to a node over TLS (WebSocket by default). Configure it with
environment variables:

| Variable | Meaning | Default |
|----------|---------|---------|
| `CASPAR_HOST` | node host | `127.0.0.1` |
| `CASPAR_PROTO` | `ws` or `tcp` | `ws` |
| `CASPAR_PORT` | action port | ws `8076`, tcp `8077` |
| `CASPAR_TLS` | `0` for plaintext `ws://` / TCP (direct-to-node, no proxy) | `1` (TLS) |
| `CASPAR_INSECURE` | `1` to skip TLS certificate verification (dev) | unset |
| `CASPAR_SIGNAL_TIMEOUT_MS` | signal round-trip timeout | `30000` |

> **Direct vs. proxied.** A Caspar node serves its `ws`/`tcp` client transports
> in **plaintext**; TLS is normally terminated by a front proxy (nginx). When
> you connect straight to a node with no proxy — e.g. one started by
> `casparctl run` — set `CASPAR_TLS=0`. Keep the default (TLS) when connecting
> through the proxied production endpoint.

Credentials are stored in the working directory under `auth/userId.txt` and
`auth/privateKey.txt`; downloads land in `files/`.

---

## Run modes

- **Interactive shell:** `caspar-client` (prompts `username$ `).
- **Single command:** `caspar-client creatures.me`.
- **Batch inline:** `caspar-client --batch "creatures.me; programs.list 0 10"`.
- **Batch file:** `caspar-client --batch-file ./commands.txt` (`#` lines are
  comments).

Offline commands (`help`, `clear`, `vm.types`, `vm.init`, `login`) don't require
an authenticated session; all others authenticate first
(`/creatures/authenticate` + `creatures.me`).

---

## Meta commands

- **`help`** — full command reference.
- **`help <command>`** — help for one command (e.g. `help creatures.signal`).
- **`clear`** — clear the screen.

---

## Auth & account

### login
`login [username] [optional email]` — authenticate directly against the node's
`/creatures/login`. The node issues/looks up the account (treating the email as
the account email) and returns the userId + private key, saved under `./auth`.
Works against any Caspar node — no external identity provider.
Example: `login alice alice@example.com`.

### logout
`logout` — clear local auth state (removes `auth/*`).

### printPrivateKey
`printPrivateKey` — print the local account private-key body.

---

## Creatures

Creatures are identities/accounts. All routes are `/creatures/*`.

### creatures.me
`creatures.me` — the current creature profile.

### creatures.get
`creatures.get [creatureId]` — a creature by id (e.g. `123@global`).

### creatures.list
`creatures.list [offset] [count]` — list creatures.

### creatures.lockToken
`creatures.lockToken [amount] [type] [target]` — lock tokens; prints a signed
token id.

### creatures.consumeLock
`creatures.consumeLock [lockId] [type] [amount]` — consume a token lock (signs
the lock id).

### creatures.signal
`creatures.signal [creatureId] [programId] [entity] [data] [optional storeId]`
— send a direct signal to a creature/program entity **and await its result**.
When `data` is a JSON object, a `correlationId` is injected and the call
resolves to the VM's asynchronous result (`creatures/signal/result`) instead of
just the ACK. `programId`/`entityId` are forwarded at the top level so the node
routes to the program's VM listener.
Example: `creatures.signal 123@global 456@global main '{"cmd":"ping"}'`.

### creatures.createMachine
`creatures.createMachine [chainId] [username] [title] [desc]` — create a
**machine-type creature** (the identity that owns programs). Returns the
creatureId used by `programs.create`.
Example: `creatures.createMachine 1 calcapp Calculator "simple calc app"`.

### creatures.listMachines
`creatures.listMachines [offset] [count]` — list machine creatures.

---

## Programs

Programs are the deployable/runnable VM units, attached to a creature. Routes
are `/programs/*` (and `/creatures/create` for the owning machine).

### programs.create
`programs.create [username] [creatureId] [path] [runtime] [comment]` — create a
program under a creature, targeting one of the seven runtimes.
Example: `programs.create calculator 984@global /api/sum wasm "sum machine"`.

### programs.delete
`programs.delete [programId]` — delete a program.

### programs.update
`programs.update [programId] [path] [metadataJsonOrFilePath] [optional promptFile]`
— update a program's path/metadata. The metadata arg may be inline JSON or a
path to a JSON file; an optional prompt file is read into `metadata.prompt`.

### programs.deploy
`programs.deploy [programId] [projectFolderPath] [runtime] [metadata]` — build
and deploy a **VM project folder** (see [vm.init](#vminit)). It runs
`<folder>/builder/build.sh` (which produces `builder/bytecode`), sends that as
the base64 payload, and ships every file in `<folder>/src/` as
`metadata.files`. `runtime` becomes the entity type.
Example: `programs.deploy 876@global ./calc-proj wasm '{}'`.

### programs.deployRaw
`programs.deployRaw [programId] [entityId] [artifactPath] [optional runtime=wasm] [optional metadataJson]`
— deploy a **prebuilt artifact file** directly (no builder/src convention).
Useful when an external toolchain produced the artifact.
Example: `programs.deployRaw 876@global main ./module.wasm wasm '{}'`.

### programs.run
`programs.run [programId]` — run the program's `main` entity (launches a VM).

### programs.stop
`programs.stop [programId]` — stop the program's `main` entity.

### programs.list
`programs.list [offset] [count]` — list programs.

### programs.readBuildLogs
`programs.readBuildLogs [vmId]` — read a program/VM's build & runtime logs
(`/programs/readVmLogs`).

---

## VM project templates

These commands are **offline** (no node needed) and scaffold projects that the
`programs.deploy` convention can build and ship.

### vm.types
`vm.types` — list the seven VM runtimes a Caspar node supports: `wasm`,
`javascript`, `docker`, `fire`, `elpian`, `elpify`, `modal`.

### vm.init
`vm.init [runtime] [path] [optional entityId]` — scaffold a deployable VM
project for the given runtime. It writes:

```text
<path>/
├── builder/build.sh    # produces builder/bytecode (the deploy payload)
├── src/<entity file>   # a minimal, valid sample entity for the runtime
├── vm.json             # descriptor: runtime, entityId, entityFileName, deploy hints
└── README.md           # per-project deploy instructions
```

The generated entity file per runtime: `wasm`→`module.wat` (compiled to
`module.wasm` by `wat2wasm` in `build.sh`), `fire`→`module.wat`,
`javascript`→`module.js`, `docker`→`Dockerfile` (+`index.html`),
`elpian`→`module.elpian.json`, `elpify`→`module.elpify.js`. For `docker` the
descriptor includes a default `imageName`.
Example: `vm.init wasm ./my-wasm-vm main`.

See [VM Types](07-vm-types-and-implementation.md) for how each runtime executes
its entity.

---

## End-to-end: deploy a VM to Caspar

```bash
# 0. point at the node
export CASPAR_HOST=127.0.0.1

# 1. authenticate
caspar-client login alice alice@example.com

# 2. scaffold a project (offline)
caspar-client vm.init wasm ./my-vm main

# 3. create the owning machine creature -> note the creatureId
caspar-client creatures.createMachine 1 my-app "My app" "demo"

# 4. create the program under that creature -> note the programId
caspar-client programs.create ep <creatureId> /api/main wasm "entry"

# 5. build + deploy the project to the program
caspar-client programs.deploy <programId> ./my-vm wasm '{}'

# 6. run it, then signal it
caspar-client programs.run <programId>
caspar-client creatures.signal <creatureId> <programId> main '{"cmd":"ping"}'
```

For docker, pass an image name in the deploy metadata, e.g.
`programs.deploy <programId> ./my-docker docker '{"imageName":"caspar-main:latest"}'`.

---

## Result codes

The CLI prints `res.obj` on success and `Error: <obj>` otherwise. Common codes:
`0` success, `10` not authenticated, `20` request timeout, `30` bad parameters,
`31`/`32` scaffolding/signal errors, `100`/`101` docker deploy validation. Node
action status codes (`0`–`4`) are described in
[Protocol → Status codes](05-caspar-protocol.md#status-codes).

---

## What was removed from the upstream CLI

This CLI is derived from the Decillion client but keeps **only** commands that
speak the Caspar shell API directly. Removed: command families routed through a
hosted miniapp signaling layer (`stores`, `invites`, `chains`, `storage`, `pc`),
the hosted browser/Auth0 login flow (replaced by the direct `/creatures/login`
`login`), and the `charge` billing command — none of which are Caspar node shell
APIs.
