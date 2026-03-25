# API Reference 📡

This project uses a **signed binary action protocol** over TLS TCP/WS for core actions, plus HTTPS gateways for entity/stream operations.

## 1) Action Protocol (TCP + WS)

### Request framing

For TCP, packets are length-prefixed:

```text
[4 bytes length (big-endian)] [request-packet]
```

Request packet layout:

```text
[4 bytes signature_len]
[signature bytes]
[4 bytes user_id_len]
[user_id bytes]
[4 bytes path_len]
[path bytes]                 # ex: /points/signal
[4 bytes request_id_len]
[request_id bytes]
[payload bytes]              # JSON of the action input
```

WS handler currently expects the same packet body format (with 4 leading bytes stripped in server handler path).

### Acknowledgement

Client ACK frame is a single byte:

```text
0x01
```

### Server frame types

- `0x01` -> update frame
- `0x02` -> response frame

Response frame layout:

```text
0x02
[4 bytes request_id_len]
[request_id]
[4 bytes status_code]
[json response bytes]
```

## 2) Response/Error Status Codes

| Code | Meaning |
|---|---|
| `0` | success |
| `1` | action not found |
| `2` | input parsing/validation error |
| `3` | action execution error |
| `4` | authentication/authorization failure |

## 3) Authentication Flow 🔐

Special command paths:

- `authenticate`
  - verifies signature, binds socket to user listener, restores queued messages
- `logout`
  - removes user listener after signature verification

Regular action paths are route keys like `/users/create`, `/points/signal`, etc.

## 4) Core Action Routes

Source: `node/src/shell/api/actions/*`

### Auth

- `GET /auths/getServerPublicKey`
- `GET /auths/getServersMap`

### Users

- `POST /users/authenticate`
- `POST /users/transfer`
- `POST /users/mint`
- `POST /users/checkSign`
- `POST /users/lockToken`
- `POST /users/consumeLock`
- `POST /users/login`
- `POST /users/create`
- `POST /users/delete`
- `POST /users/update`
- `GET /users/meta`
- `GET /users/get`
- `GET /users/getByUsername`
- `GET /users/find`
- `GET /users/list`

### Points

- `POST /points/addApp`
- `POST /points/listApps`
- `POST /points/updateMachine`
- `POST /points/removeApp`
- `POST /points/addMachine`
- `POST /points/removeMachine`
- `POST /points/addMember`
- `POST /points/updateMember`
- `POST /points/updateMemberAccess`
- `POST /points/updateMachineAccess`
- `POST /points/getDefaultAccess`
- `POST /points/readMembers`
- `POST /points/removeMember`
- `POST /points/create`
- `PUT /points/update`
- `DELETE /points/delete`
- `GET /points/meta`
- `GET /points/get`
- `GET /points/read`
- `POST /points/join`
- `POST /points/leave`
- `POST /points/signal`
- `POST /points/history`
- `GET /points/list`

### Invites

- `POST /invites/create`
- `POST /invites/listPointInvites`
- `POST /invites/listUserInvites`
- `POST /invites/cancel`
- `POST /invites/accept`
- `POST /invites/decline`

### Apps + Machines

- `POST /apps/create`
- `POST /apps/deleteApp`
- `POST /apps/updateApp`
- `GET /apps/myCreatedApps`
- `POST /machines/create`
- `POST /apps/deleteMachine`
- `POST /apps/updateMachine`
- `POST /machines/signal`
- `POST /apps/runMachine`
- `POST /apps/stopMachine`
- `POST /machines/readBuildLogs`
- `POST /machines/readMachineBuilds`
- `POST /machines/deploy`
- `GET /apps/list`
- `GET /machines/list`
- `GET /machines/listAppMachines`

### Storage (action routes)

- `POST /storage/upload`
- `POST /storage/uploadUserEntity`
- `POST /storage/deleteUserEntity`
- `POST /storage/uploadPointEntity`
- `POST /storage/uploadAppEntity`
- `POST /storage/deletePointEntity`
- `POST /storage/download`

### Chain

- `POST /chains/create`
- `POST /chains/createFromPoint`
- `POST /chains/registerNode`
- `POST /chains/submitBaseTrx`

### PC + Dummy

- `POST /pc/runPc`
- `POST /pc/execCommand`
- `GET /api/hello`
- `GET /api/time`
- `GET /api/ping`

## 5) HTTPS Entity/Stream APIs

Entity server (`ENTITY_API_PORT`) registers:

- `/storage/downloadUserEntity`
- `/storage/uploadUserEntity`
- `/storage/uploadPointEntity`
- `/storage/uploadAppEntity`
- `/storage/downloadAppEntity`
- `/storage/downloadPointEntity`
- `/stream/get`
- `/stream/send`

VM gateway (`VM_API_PORT`) registers:

- `/stream/send`

These endpoints also verify signatures and may proxy across origins.

## 6) Hashgraph Service API

Default service handlers include:

- `/stats`
- `/block/{index}`
- `/blocks/{start}?count=n`
- `/graph`
- `/peers`
- `/genesispeers`
- `/validators/{index}`
- `/history`

## 7) Input Schemas

Action inputs are implemented in:

- `node/src/shell/api/inputs/users`
- `node/src/shell/api/inputs/points`
- `node/src/shell/api/inputs/machine`

`POST /machines/deploy` accepts:

- `machineId` (string, required)
- `entityId` (string, required)
- `entityType` (string, required): one of `javascript`, `elpify`, `wasm`, `docker`
- `downloadable` (boolean): whether this machine entity can be downloaded through storage entity APIs
- `payload` (base64 string, required): dockerfile/module/program source payload
- `metadata` (object, optional): docker build metadata (for example extra files, image naming)
- `node/src/shell/api/inputs/invites`
- `node/src/shell/api/inputs/storage`
- `node/src/shell/api/inputs/chain`
- `node/src/shell/api/inputs/auth`
- `node/src/shell/api/inputs/pc`

Use those files as authoritative schema definitions (JSON fields + validation tags).
