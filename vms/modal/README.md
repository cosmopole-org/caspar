# `modal` — Modal cloud sandbox runtime

The seventh Caspar VM plugin. A VM of this runtime is a **Modal sandbox**: a
container running in Modal's cloud, not on this node, with a persistent Modal
Volume mounted at `/data`.

## Configuration

The credential pair from Modal's Settings → API Tokens, either together or
split:

```
MODAL_API_KEY="<token-id>:<token-secret>"
# …or…
MODAL_TOKEN_ID="ak-…"
MODAL_TOKEN_SECRET="as-…"
```

Optional: `MODAL_ENVIRONMENT`, `MODAL_SERVER_URL`, `MODAL_APP_PREFIX`,
`MODAL_DEFAULT_IMAGE`, `MODAL_VOLUME_MOUNT_PATH`, `MODAL_SANDBOX_TIMEOUT_SECS`,
`MODAL_IMAGE_BUILD_TIMEOUT_SECS`. See `node/sample.env`.

With no credentials the plugin still registers; every operation then fails with
`modal runtime is not configured` rather than a transport error.

## The proto slice

Modal publishes no Rust SDK, so this plugin speaks Modal's gRPC control plane
directly. `proto/modal.proto` is a **generated slice** of modal-client's
`modal_proto/api.proto` — only the messages and RPCs this runtime calls, with
upstream field numbers preserved, so it is wire compatible while keeping the
generated Rust (and the blast radius of an upstream change) small.

Regenerate after an upstream break:

```sh
curl -sSfL https://raw.githubusercontent.com/modal-labs/modal-client/main/modal_proto/api.proto \
  -o /tmp/modal_api.proto
python3 ../../scripts/slice_modal_proto.py /tmp/modal_api.proto proto/modal.proto
cargo test
```

Modal states that direct gRPC use carries no compatibility guarantee. Treat a
broken call as an upstream change, not a bug here.

`protox` compiles the proto in pure Rust, so no `protoc` binary is required on
a build machine.

## Where a VM's identity lives

A Modal sandbox outlives the node process that started it, so this runtime
keeps **no in-process registry**. The mapping is node state:

| Key | Value |
|-----|-------|
| `ModalSandbox::<vmId>` | the sandbox id |
| `ModalVolume::<vmId>` | the VM's persistent Volume |
| `ModalApp::<machineId>` | the Modal app grouping a machine's resources |
| `ModalImage::<machineId>::<entityId>` | the resolved image |

This is what makes `restore` correct: a node coming back up re-attaches to a
sandbox that never stopped, instead of launching a second one and billing for
both.

## Terminate vs delete

`terminate_vm` ends the container but **keeps the Volume**, so a later run
mounts the same `/data` and the VM continues where it left off — Caspar's
suspend semantics, on a runtime that has no native suspend. `delete_vm`
terminates, deletes the Volume and drops every link: nothing is left to resume
from.
