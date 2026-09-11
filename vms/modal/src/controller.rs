//! The modal VM controller — sandbox lifecycle, exec, file transfer and HTTP
//! forwarding, exposed to the Caspar VMM through `caspar_vm_sdk::VmPlugin`.
//!
//! A modal VM is a Modal *sandbox*. Unlike docker (a container this node
//! supervises) or fire (a microVM it boots), the instance lives in Modal's
//! cloud and outlives the node process, so this runtime keeps no in-process
//! registry: the vm id → sandbox id mapping is written to node state at
//! create and read back by every later operation. That is also what makes
//! restore work — a node that comes back up re-attaches to sandboxes that
//! never stopped running.

use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine;
use futures_util::StreamExt;
use serde_json::{json, Map, Value as JsonValue};

use caspar_vm_sdk::host::{host, log_vm, KvOp};
use caspar_vm_sdk::{parse_vm_resource_limits, VmPlugin, VmPluginMeta};

use crate::client::{block_on, connect, is_configured, ModalConn};
use crate::models::{
    app_link_key, image_link_key, modal_app_name, modal_volume_name, sandbox_link_key,
    volume_link_key, ModalIdentity,
};
use crate::proto;

/// Port inside the sandbox the VMM ingress forwards HTTP to. Matches the
/// docker runtime's `CASPAR_VM_HTTP_PORT` so a creature written for one
/// runtime serves on the same port under the other.
fn vm_http_port() -> u32 {
    std::env::var("CASPAR_VM_HTTP_PORT")
        .ok()
        .and_then(|p| p.trim().parse::<u32>().ok())
        .unwrap_or(8080)
}

/// Timeout (seconds) for a forwarded HTTP request to the sandbox.
fn vm_http_timeout_secs() -> u64 {
    std::env::var("CASPAR_VM_HTTP_TIMEOUT_SECS")
        .ok()
        .and_then(|p| p.trim().parse::<u64>().ok())
        .filter(|s| *s > 0)
        .unwrap_or(30)
}

/// Base image used when a packet names none.
fn default_image_tag() -> String {
    std::env::var("MODAL_DEFAULT_IMAGE")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "ubuntu:24.04".to_string())
}

/// Where a VM's persistent Modal Volume is mounted inside the sandbox. Same
/// contract as the docker runtime's per-VM bind mount.
fn volume_mount_path() -> String {
    std::env::var("MODAL_VOLUME_MOUNT_PATH")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "/data".to_string())
}

#[derive(Clone)]
pub struct ModalVmPlugin {
    meta: VmPluginMeta,
}

impl ModalVmPlugin {
    pub fn new(meta: VmPluginMeta) -> Self {
        Self { meta }
    }
}

// ── Node state helpers ────────────────────────────────────────────────────

fn state_get(key: &str) -> String {
    match host() {
        Some(h) => h.state_get(key).trim().to_string(),
        None => String::new(),
    }
}

fn state_put(key: &str, value: &str) {
    if let Some(h) = host() {
        let _ = h.state_apply_ops(&[KvOp {
            op: "put".to_string(),
            key: key.to_string(),
            val: value.to_string(),
        }]);
    }
}

fn state_del(keys: &[String]) {
    if let Some(h) = host() {
        let ops: Vec<KvOp> = keys
            .iter()
            .map(|k| KvOp {
                op: "del".to_string(),
                key: k.clone(),
                val: String::new(),
            })
            .collect();
        let _ = h.state_apply_ops(&ops);
    }
}

fn provisioning_key(vm_id: &str) -> String {
    format!("ModalProvisioning::{}", vm_id)
}

fn provisioning_error_key(vm_id: &str) -> String {
    format!("ModalProvisioningError::{}", vm_id)
}

fn fresh_provisioning_marker(vm_id: &str) -> Option<String> {
    let marker = state_get(&provisioning_key(vm_id));
    let started_at = marker
        .split_once(':')
        .and_then(|(raw, _)| raw.parse::<i64>().ok())?;
    let now = chrono::Utc::now().timestamp_millis();
    ((0..20 * 60 * 1000).contains(&(now - started_at))).then_some(marker)
}

// ── Packet helpers ────────────────────────────────────────────────────────

fn string_list(value: &JsonValue) -> Vec<String> {
    match value {
        JsonValue::Array(items) => items
            .iter()
            .filter_map(|v| v.as_str().map(|s| s.to_string()))
            .filter(|s| !s.trim().is_empty())
            .collect(),
        JsonValue::String(s) if !s.trim().is_empty() => vec![s.clone()],
        _ => Vec::new(),
    }
}

/// Modal's accepted memory range, in MiB. A request outside it is refused
/// outright (`InvalidArgument`), so a packet that names no resources — which is
/// what the platform sends when its sandbox settings were never seeded — must
/// not be passed through as the 1 MiB the generic parser floors it to.
const MIN_MEMORY_MB: u32 = 1024;
const DEFAULT_MEMORY_MB: u32 = 8192;

/// Modal's *ephemeral disk* is a large scratch volume, sized in hundreds of
/// GiB: "must be between 524288 and 3145728 MiB". It is NOT a sandbox's root
/// disk, which Modal sizes itself and no request controls.
const MIN_EPHEMERAL_DISK_MB: u32 = 4_096_288;
const MAX_EPHEMERAL_DISK_MB: u32 = 8_192_728;

/// What a sandbox is given, from the packet's `resources`.
///
/// The generic `VmResourceLimits` this shares with docker and firecracker
/// describes a machine the node builds: RAM, cores, and a root disk in GiB.
/// Modal has the first two and NOT the third — `ephemeral_disk_mb` is a
/// separate, very large scratch volume, and a project's durable storage is the
/// Volume mounted at /data regardless. Mapping `diskGb` onto it (the platform
/// asks for single-digit GiB) produced a request two orders of magnitude below
/// Modal's minimum, and every sandbox create was refused with
/// `InvalidArgument` — reported to the client as "the project's machine could
/// not be started". So an ephemeral disk is sent only when a caller asks for
/// one BY NAME, and never inferred from `diskGb`.
fn sandbox_resources(packet: &JsonValue, limits: &caspar_vm_sdk::VmResourceLimits) -> proto::Resources {
    let memory_mb = match limits.ram_mb as u32 {
        // The parser floors an absent/zero value to 1, which Modal rejects.
        // Treat anything under Modal's own minimum as "not specified".
        m if m < MIN_MEMORY_MB => DEFAULT_MEMORY_MB,
        m => m,
    };
    let cpu_cores = (limits.cpu_cores as u32).max(1);

    // `ephemeralDiskMb`, or `ephemeralDiskGb` for callers that think in GiB.
    let requested_disk = packet["ephemeralDiskMb"]
        .as_u64()
        .or_else(|| packet["resources"]["ephemeralDiskMb"].as_u64())
        .or_else(|| {
            packet["ephemeralDiskGb"]
                .as_u64()
                .or_else(|| packet["resources"]["ephemeralDiskGb"].as_u64())
                .map(|gb| gb.saturating_mul(1024))
        })
        .unwrap_or(0) as u32;
    // Clamped rather than refused: a scratch disk is an optimisation, and
    // failing a whole sandbox over a mis-sized one helps nobody.
    let ephemeral_disk_mb = if requested_disk == 0 {
        0
    } else {
        requested_disk.clamp(MIN_EPHEMERAL_DISK_MB, MAX_EPHEMERAL_DISK_MB)
    };

    proto::Resources {
        memory_mb,
        milli_cpu: cpu_cores.saturating_mul(1000),
        ephemeral_disk_mb,
        ..Default::default()
    }
}

/// The command a sandbox runs as its entrypoint.
///
/// A sandbox with no command would exit immediately and take the VM with it,
/// so the default keeps it alive and idle — the platform addresses a modal VM
/// through exec and HTTP, not through its entrypoint.
fn entrypoint_args(packet: &JsonValue) -> Vec<String> {
    let explicit = string_list(&packet["entrypoint"]);
    if !explicit.is_empty() {
        return explicit;
    }
    if let Some(command) = packet["command"].as_str() {
        let command = command.trim();
        if !command.is_empty() {
            return vec!["sh".into(), "-lc".into(), command.to_string()];
        }
    }
    vec!["sleep".into(), "infinity".into()]
}

fn env_pairs(packet: &JsonValue) -> Vec<(String, String)> {
    let mut out = Vec::new();
    if let Some(obj) = packet["env"].as_object() {
        for (k, v) in obj {
            let value = match v {
                JsonValue::String(s) => s.clone(),
                other if other.is_null() => continue,
                other => other.to_string(),
            };
            out.push((k.clone(), value));
        }
    }
    out
}

/// Ports the sandbox should expose through a Modal tunnel. The HTTP port is
/// always included so `forward_http` has somewhere to proxy to.
fn exposed_ports(packet: &JsonValue) -> Vec<u32> {
    let mut ports: Vec<u32> = packet["ports"]
        .as_array()
        .map(|items| {
            items
                .iter()
                .filter_map(|v| v.as_u64().map(|p| p as u32))
                .collect()
        })
        .unwrap_or_default();
    let http = vm_http_port();
    if !ports.contains(&http) {
        ports.push(http);
    }
    ports
}

impl ModalVmPlugin {
    fn conn(&self) -> Result<ModalConn, String> {
        if !is_configured() {
            return Err(
                "modal runtime is not configured: set MODAL_API_KEY on the node".to_string(),
            );
        }
        connect()
    }

    /// Resolve (creating on first use) the Modal app that owns a machine's
    /// sandboxes. Modal groups resources under an app; one app per Caspar
    /// machine keeps a creature's sandboxes, volumes and images together and
    /// makes them findable in Modal's dashboard by the machine they belong to.
    fn app_id(&self, conn: &mut ModalConn, machine_id: &str) -> Result<String, String> {
        let key = app_link_key(machine_id);
        let cached = state_get(&key);
        if !cached.is_empty() {
            return Ok(cached);
        }
        let request = proto::AppGetOrCreateRequest {
            app_name: modal_app_name(machine_id),
            environment_name: conn.environment.clone(),
            object_creation_type: proto::ObjectCreationType::CreateIfMissing as i32,
        };
        let response = block_on(conn.stub.app_get_or_create(request))?
            .map_err(|e| format!("modal AppGetOrCreate failed: {}", e))?
            .into_inner();
        if response.app_id.is_empty() {
            return Err("modal returned an empty app id".to_string());
        }
        state_put(&key, &response.app_id);
        Ok(response.app_id)
    }

    /// Resolve the image a sandbox boots from.
    ///
    /// `image` names a registry tag (`ubuntu:24.04`, `ghcr.io/org/img:v1`).
    /// `dockerfileCommands` — what a deployed `Modalfile` becomes — layers on
    /// top of it. Modal builds asynchronously, so a freshly-created image is
    /// joined until it reports success before any sandbox is created from it.
    fn image_id(
        &self,
        conn: &mut ModalConn,
        app_id: &str,
        machine_id: &str,
        entity_id: &str,
        packet: &JsonValue,
    ) -> Result<String, String> {
        let tag = packet["image"]
            .as_str()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(default_image_tag);
        let extra_commands = string_list(&packet["dockerfileCommands"]);

        // Cache only the plain-tag case: a Dockerfile-derived image changes
        // whenever its commands do, and Modal's own recipe cache already makes
        // an unchanged rebuild cheap.
        let cache_key = image_link_key(machine_id, entity_id);
        if extra_commands.is_empty() && packet["forceBuild"].as_bool() != Some(true) {
            let cached = state_get(&cache_key);
            if !cached.is_empty() {
                return Ok(cached);
            }
        }

        let mut dockerfile_commands = vec![format!("FROM {}", tag)];
        dockerfile_commands.extend(extra_commands);

        let image = proto::Image {
            dockerfile_commands,
            ..Default::default()
        };
        let request = proto::ImageGetOrCreateRequest {
            image: Some(image),
            app_id: app_id.to_string(),
            force_build: packet["forceBuild"].as_bool().unwrap_or(false),
            builder_version: std::env::var("MODAL_BUILDER_VERSION").unwrap_or_default(),
            ..Default::default()
        };
        let response = block_on(conn.stub.image_get_or_create(request))?
            .map_err(|e| format!("modal ImageGetOrCreate failed: {}", e))?
            .into_inner();
        if response.image_id.is_empty() {
            return Err("modal returned an empty image id".to_string());
        }

        // `result` is set only when the image has finished building. When it
        // is absent the build is still running and the sandbox would fail to
        // start, so join the build stream until it completes.
        if response.result.is_none() {
            self.await_image_build(conn, &response.image_id, packet)?;
        } else if let Some(result) = response.result.as_ref() {
            check_generic_result(result, "image build")?;
        }

        state_put(&cache_key, &response.image_id);
        Ok(response.image_id)
    }

    /// Follow an image build to completion, streaming its log lines into the
    /// VM's build log so a stuck deploy is visible rather than a silent wait.
    fn await_image_build(
        &self,
        conn: &mut ModalConn,
        image_id: &str,
        packet: &JsonValue,
    ) -> Result<(), String> {
        let vm_id = packet["vmId"].as_str().unwrap_or("main").to_string();
        let mut last_entry_id = String::new();
        // Modal's join stream ends at each timeout window; loop until the
        // build reports a result or the overall budget is spent.
        let deadline = std::time::Instant::now()
            + std::time::Duration::from_secs(
                std::env::var("MODAL_IMAGE_BUILD_TIMEOUT_SECS")
                    .ok()
                    .and_then(|v| v.trim().parse::<u64>().ok())
                    .unwrap_or(900),
            );
        loop {
            if std::time::Instant::now() > deadline {
                return Err(format!("modal image {} did not finish building", image_id));
            }
            let request = proto::ImageJoinStreamingRequest {
                image_id: image_id.to_string(),
                timeout: 55.0,
                last_entry_id: last_entry_id.clone(),
                include_logs_for_finished: true,
            };
            let mut stream = block_on(conn.stub.image_join_streaming(request))?
                .map_err(|e| format!("modal ImageJoinStreaming failed: {}", e))?
                .into_inner();

            let mut finished: Option<proto::GenericResult> = None;
            loop {
                let next = block_on(stream.next())?;
                let Some(message) = next else { break };
                let message =
                    message.map_err(|e| format!("modal image build stream failed: {}", e))?;
                if !message.entry_id.is_empty() {
                    last_entry_id = message.entry_id.clone();
                }
                for logs in &message.task_logs {
                    let text = logs.data.trim_end();
                    if !text.is_empty() {
                        log_vm(text.to_string(), vm_id.clone(), "build");
                    }
                }
                if let Some(result) = message.result.clone() {
                    finished = Some(result);
                }
                if message.eof {
                    break;
                }
            }
            if let Some(result) = finished {
                return check_generic_result(&result, "image build");
            }
        }
    }

    /// Resolve (creating on first use) the persistent Modal Volume backing one
    /// VM instance. This is the modal equivalent of docker's per-VM bind
    /// mount: it survives terminate, and only a delete removes it.
    fn volume_id(&self, conn: &mut ModalConn, vm_id: &str) -> Result<String, String> {
        let key = volume_link_key(vm_id);
        let cached = state_get(&key);
        if !cached.is_empty() {
            return Ok(cached);
        }
        let request = proto::VolumeGetOrCreateRequest {
            deployment_name: modal_volume_name(vm_id),
            environment_name: conn.environment.clone(),
            object_creation_type: proto::ObjectCreationType::CreateIfMissing as i32,
            ..Default::default()
        };
        let response = block_on(conn.stub.volume_get_or_create(request))?
            .map_err(|e| format!("modal VolumeGetOrCreate failed: {}", e))?
            .into_inner();
        if response.volume_id.is_empty() {
            return Err("modal returned an empty volume id".to_string());
        }
        state_put(&key, &response.volume_id);
        Ok(response.volume_id)
    }

    /// Give a VM's Volume time to publish what was just written to it, before
    /// the container holding those writes is destroyed.
    ///
    /// A Modal Volume mounted with `allow_background_commits` is not written
    /// through: a write lands in the container's local layer and reaches the
    /// volume on a periodic background commit. Terminating the sandbox does not
    /// flush it, so writes newer than the last commit are lost — measured
    /// against the real API, a file written and immediately followed by a
    /// restart came back or vanished depending on where that timer happened to
    /// be, and a woken machine could come back without the project's files.
    ///
    /// It cannot be forced. `VolumeCommit` exists, but Modal refuses it from
    /// here — "commit() can only be called on a mounted volume inside a
    /// container" — with or without a `container_id`; it is for a process
    /// running INSIDE the sandbox, and a project's machine runs a plain Ubuntu
    /// image with no Modal client in it. So what is available is to wait, and
    /// the wait is bounded and explicit rather than hidden in a retry.
    ///
    /// This only costs anything on a teardown the platform itself initiates
    /// (a re-provision, a delete). The common case — Modal ending a machine
    /// after its idle window — is minutes past the last write and needs none of
    /// it. `MODAL_VOLUME_SETTLE_MS=0` turns it off.
    fn settle_volume_writes(&self, vm_id: &str) {
        if state_get(&volume_link_key(vm_id)).is_empty() {
            return;
        }
        let millis = std::env::var("MODAL_VOLUME_SETTLE_MS")
            .ok()
            .and_then(|v| v.trim().parse::<u64>().ok())
            .unwrap_or(3_000);
        if millis == 0 {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(millis));
    }

    /// The sandbox id recorded for a VM, or an error naming the VM when none
    /// is — an unrecorded sandbox means the VM was never started here (or was
    /// already deleted), which every caller needs to distinguish from an RPC
    /// failure.
    fn sandbox_id(&self, vm_id: &str) -> Result<String, String> {
        let id = state_get(&sandbox_link_key(vm_id));
        if id.is_empty() {
            return Err(format!("no modal sandbox is recorded for vm {}", vm_id));
        }
        Ok(id)
    }

    /// The running task inside a sandbox — the handle exec and filesystem
    /// operations address. `wait_until_ready` blocks until the container has
    /// finished starting, so an exec issued right after create does not race
    /// the boot.
    fn task_id(&self, conn: &mut ModalConn, sandbox_id: &str) -> Result<String, String> {
        let request = proto::SandboxGetTaskIdRequest {
            sandbox_id: sandbox_id.to_string(),
            timeout: Some(
                std::env::var("MODAL_TASK_READY_TIMEOUT_SECS")
                    .ok()
                    .and_then(|v| v.trim().parse::<f32>().ok())
                    .unwrap_or(60.0),
            ),
            wait_until_ready: true,
        };
        let response = block_on(conn.stub.sandbox_get_task_id(request))?
            .map_err(|e| format!("modal SandboxGetTaskId failed: {}", e))?
            .into_inner();
        if let Some(result) = response.task_result.as_ref() {
            check_generic_result(result, "sandbox task")?;
        }
        response
            .task_id
            .filter(|id| !id.trim().is_empty())
            .ok_or_else(|| format!("modal sandbox {} has no running task", sandbox_id))
    }

    /// Whether a sandbox is still usable, without mutating it.
    ///
    /// "Usable" is deliberately narrower than "not yet finished": every caller
    /// acts on this by either exec'ing in the sandbox or handing it back as a
    /// resumed VM, and both need a LIVE TASK. So the question asked is exactly
    /// that one — `SandboxGetTaskId` reports the task and, once it has exited,
    /// its result.
    ///
    /// `SandboxWait` is not used for this. Measured against the real API, a
    /// zero-timeout wait answers `result: None` for a sandbox that has just
    /// been terminated — for seconds afterwards — and the old implementation
    /// read `None` as "still running". A run that found a recorded sandbox
    /// therefore reported `resumed` for a machine that was already dead, never
    /// replaced it, and every later exec failed with "Sandbox was cancelled by
    /// user". That is the wake path for a project whose machine slept, so the
    /// machine could never come back.
    fn is_running(&self, conn: &mut ModalConn, sandbox_id: &str) -> Result<bool, String> {
        let request = proto::SandboxGetTaskIdRequest {
            sandbox_id: sandbox_id.to_string(),
            // `wait_until_ready: false` can return a stale `task_result: None`
            // for several seconds after Modal has idle-timed the task. Asking
            // for readiness makes the same API return the terminal
            // IdleTimeout result immediately; a live task is already ready by
            // the time its sandbox link is recorded.
            timeout: Some(5.0),
            wait_until_ready: true,
        };
        let response = block_on(conn.stub.sandbox_get_task_id(request))?
            .map_err(|e| format!("modal SandboxGetTaskId failed: {}", e))?
            .into_inner();
        // No task was ever scheduled: the sandbox was terminated before it ran.
        let Some(task_id) = response.task_id.filter(|id| !id.trim().is_empty()) else {
            return Ok(false);
        };
        let _ = task_id;
        // A task result at all means the task has exited; only an unset or
        // UNSPECIFIED status is a task still doing something.
        Ok(match response.task_result {
            None => true,
            Some(result) => {
                result.status == proto::generic_result::GenericStatus::Unspecified as i32
            }
        })
    }
}

/// Turn a Modal `GenericResult` into a plugin error when it reports failure.
fn check_generic_result(result: &proto::GenericResult, what: &str) -> Result<(), String> {
    use proto::generic_result::GenericStatus;
    let status = GenericStatus::try_from(result.status).unwrap_or(GenericStatus::Unspecified);
    match status {
        GenericStatus::Unspecified | GenericStatus::Success => Ok(()),
        other => {
            let mut message = format!("modal {} failed ({:?})", what, other);
            if !result.exception.is_empty() {
                message.push_str(&format!(": {}", result.exception));
            }
            Err(message)
        }
    }
}

// ── Lifecycle ─────────────────────────────────────────────────────────────

impl ModalVmPlugin {
    fn run_vm_inner(&self, packet: &JsonValue) -> Result<JsonValue, String> {
        let identity = ModalIdentity::from_packet(packet);
        if identity.machine_id.is_empty() {
            return Err("machineId is required".to_string());
        }

        // Sandbox semantics, parallel to docker and fire:
        //   - `persistent: true` (the default) gives the VM its own Modal
        //     Volume mounted at /data, which survives terminate.
        //   - `forceRestart: false` makes run idempotent: a sandbox that is
        //     still running is re-attached rather than replaced.
        let persistent = packet["persistent"].as_bool().unwrap_or(true);
        let force_restart = packet["forceRestart"].as_bool().unwrap_or(false);
        let async_provision = packet["asyncProvision"].as_bool().unwrap_or(false);
        // The detached worker re-enters this function to run the synchronous
        // implementation. It must not mistake its own marker for a duplicate.
        let async_worker = packet["asyncProvisionWorker"].as_bool().unwrap_or(false);
        let pending = if async_worker {
            None
        } else {
            fresh_provisioning_marker(&identity.vm_id)
        };

        let existing = state_get(&sandbox_link_key(&identity.vm_id));
        if !existing.is_empty() {
            let mut conn = self.conn()?;
            let reusable = !force_restart && self.is_running(&mut conn, &existing).unwrap_or(false);
            if reusable {
                state_del(&[
                    provisioning_key(&identity.vm_id),
                    provisioning_error_key(&identity.vm_id),
                ]);
                if let Some(h) = host() {
                    h.register_vm_context(
                        &identity.vm_id,
                        &identity.creature_id,
                        &identity.machine_id,
                    );
                }
                return Ok(json!({
                    "ok": true,
                    "runtime": "modal",
                    "machineId": identity.machine_id,
                    "vmId": identity.vm_id,
                    "sandboxId": existing,
                    "status": "running",
                    "resumed": true,
                }));
            }
            // Another request already owns the replacement. Do not terminate
            // the old sandbox twice or launch two expensive image builds.
            if pending.is_some() {
                return Ok(json!({
                    "ok": true,
                    "runtime": "modal",
                    "machineId": identity.machine_id,
                    "vmId": identity.vm_id,
                    "status": "provisioning",
                    "accepted": true,
                    "deduplicated": true,
                }));
            }
            // A replacement is being created, so the recorded sandbox must go —
            // whether this is an explicit restart or a machine that turned out
            // to be dead. Terminating unconditionally is what stops a sandbox
            // that only LOOKED dead from being left behind, running and billing,
            // with nothing pointing at it any more.
            //
            // The volume is committed first: the replacement mounts the same
            // one, and whatever was written since the last background commit
            // lives only in the container about to be destroyed.
            self.settle_volume_writes(&identity.vm_id);
            let _ = block_on(conn.stub.sandbox_terminate(proto::SandboxTerminateRequest {
                sandbox_id: existing.clone(),
            }));
            // Nothing may execute against this id once replacement has begun.
            // Keeping the dead link until the worker records its successor made
            // every concurrent exec hit the expired task and surface
            // `IdleTimeout`, even though wake provisioning was already active.
            state_del(&[sandbox_link_key(&identity.vm_id)]);
        } else if pending.is_some() {
            return Ok(json!({
                "ok": true,
                "runtime": "modal",
                "machineId": identity.machine_id,
                "vmId": identity.vm_id,
                "status": "provisioning",
                "accepted": true,
                "deduplicated": true,
            }));
        }

        if async_provision {
            let marker = format!(
                "{}:{}",
                chrono::Utc::now().timestamp_millis(),
                uuid::Uuid::new_v4()
            );
            let marker_key = provisioning_key(&identity.vm_id);
            state_put(&marker_key, &marker);
            state_del(&[provisioning_error_key(&identity.vm_id)]);

            let plugin = self.clone();
            let mut launch_packet = packet.clone();
            if let Some(obj) = launch_packet.as_object_mut() {
                obj.remove("asyncProvision");
                obj.insert("asyncProvisionWorker".to_string(), JsonValue::Bool(true));
            }
            let vm_id = identity.vm_id.clone();
            std::thread::Builder::new()
                .name(format!("modal-provision-{}", vm_id))
                .spawn(move || {
                    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        plugin.run_vm_inner(&launch_packet)
                    }))
                    .unwrap_or_else(|_| Err("modal provisioning worker panicked".to_string()));
                    // Only the owner of the current marker may publish the
                    // outcome. A later provision can supersede a stale one.
                    if state_get(&marker_key) == marker {
                        state_del(&[marker_key.clone()]);
                        if let Err(error) = result {
                            state_put(&provisioning_error_key(&vm_id), &error);
                            log_vm(
                                format!("modal provisioning failed for vm {}: {}", vm_id, error),
                                vm_id,
                                "runtime",
                            );
                        }
                    } else if let Ok(response) = result {
                        // Termination/deletion clears the marker. If that
                        // happened while Modal was building, remove the exact
                        // sandbox this cancelled worker eventually created.
                        if let Some(sandbox_id) = response["sandboxId"].as_str() {
                            if let Ok(mut conn) = plugin.conn() {
                                let _ = block_on(conn.stub.sandbox_terminate(
                                    proto::SandboxTerminateRequest {
                                        sandbox_id: sandbox_id.to_string(),
                                    },
                                ));
                            }
                            let link_key = sandbox_link_key(&vm_id);
                            if state_get(&link_key) == sandbox_id {
                                state_del(&[link_key]);
                            }
                        }
                    }
                })
                .map_err(|e| {
                    state_del(&[provisioning_key(&identity.vm_id)]);
                    format!("could not start modal provisioning worker: {}", e)
                })?;

            return Ok(json!({
                "ok": true,
                "runtime": "modal",
                "machineId": identity.machine_id,
                "entityId": identity.entity_id,
                "vmId": identity.vm_id,
                "status": "provisioning",
                "accepted": true,
            }));
        }

        let mut conn = self.conn()?;
        let app_id = self.app_id(&mut conn, &identity.machine_id)?;
        let image_id = self.image_id(
            &mut conn,
            &app_id,
            &identity.machine_id,
            &identity.entity_id,
            packet,
        )?;

        let limits = parse_vm_resource_limits(packet);
        let mut volume_mounts = Vec::new();
        if persistent {
            let volume_id = self.volume_id(&mut conn, &identity.vm_id)?;
            volume_mounts.push(proto::VolumeMount {
                volume_id,
                mount_path: volume_mount_path(),
                allow_background_commits: true,
                read_only: false,
                sub_path: None,
            });
        }

        // Environment reaches the sandbox as entrypoint-level exports rather
        // than a Modal Secret: a Secret is a durable account-level object, and
        // per-VM values (the gateway address, the VM id) have no business
        // outliving the VM they configure.
        let mut env = env_pairs(packet);
        env.push(("CASPAR_VM_ID".to_string(), identity.vm_id.clone()));
        env.push(("CASPAR_MACHINE_ID".to_string(), identity.machine_id.clone()));
        env.push((
            "CASPAR_VM_HTTP_PORT".to_string(),
            vm_http_port().to_string(),
        ));
        let mut entrypoint = entrypoint_args(packet);
        if !env.is_empty() {
            let exports = env
                .iter()
                .map(|(k, v)| format!("export {}={};", k, shell_quote(v)))
                .collect::<Vec<_>>()
                .join(" ");
            let command = entrypoint
                .iter()
                .map(|a| shell_quote(a))
                .collect::<Vec<_>>()
                .join(" ");
            entrypoint = vec![
                "sh".to_string(),
                "-lc".to_string(),
                format!("{} exec {}", exports, command),
            ];
        }

        let ports: Vec<proto::PortSpec> = exposed_ports(packet)
            .into_iter()
            .map(|port| proto::PortSpec {
                port,
                unencrypted: false,
                tunnel_type: None,
            })
            .collect();

        let definition = proto::Sandbox {
            entrypoint_args: entrypoint,
            image_id,
            resources: Some(sandbox_resources(packet, &limits)),
            timeout_secs: sandbox_timeout_secs(packet, limits.max_exec_time_secs),
            workdir: packet["workdir"].as_str().map(|s| s.to_string()),
            open_ports_oneof: Some(proto::sandbox::OpenPortsOneof::OpenPorts(
                proto::PortSpecs { ports },
            )),
            network_access: Some(proto::NetworkAccess {
                network_access_type: if packet["blockNetwork"].as_bool().unwrap_or(false) {
                    proto::network_access::NetworkAccessType::Blocked as i32
                } else {
                    proto::network_access::NetworkAccessType::Open as i32
                },
                ..Default::default()
            }),
            volume_mounts,
            idle_timeout_secs: packet["idleTimeoutSecs"].as_u64().map(|v| v as u32),
            ..Default::default()
        };

        // Tags are what make a sandbox findable from Caspar identity alone —
        // the recovery path when node state and Modal disagree.
        let tags = vec![
            tag("caspar-vm-id", &identity.vm_id),
            tag("caspar-machine-id", &identity.machine_id),
            tag("caspar-entity-id", &identity.entity_id),
            tag("caspar-creature-id", &identity.creature_id),
        ];

        let response = block_on(conn.stub.sandbox_create(proto::SandboxCreateRequest {
            app_id: app_id.clone(),
            definition: Some(definition),
            environment_name: conn.environment.clone(),
            tags,
        }))?
        .map_err(|e| format!("modal SandboxCreate failed: {}", e))?
        .into_inner();

        if response.sandbox_id.is_empty() {
            return Err("modal returned an empty sandbox id".to_string());
        }
        if let Some(metadata) = response.metadata.as_ref() {
            if let Some(result) = metadata.result.as_ref() {
                check_generic_result(result, "sandbox create")?;
            }
        }

        state_put(&sandbox_link_key(&identity.vm_id), &response.sandbox_id);
        state_del(&[provisioning_error_key(&identity.vm_id)]);
        if let Some(h) = host() {
            h.register_vm_context(&identity.vm_id, &identity.creature_id, &identity.machine_id);
        }
        log_vm(
            format!(
                "modal sandbox {} started for vm {}",
                response.sandbox_id, identity.vm_id
            ),
            identity.vm_id.clone(),
            "runtime",
        );

        Ok(json!({
            "ok": true,
            "runtime": "modal",
            "machineId": identity.machine_id,
            "entityId": identity.entity_id,
            "vmId": identity.vm_id,
            "sandboxId": response.sandbox_id,
            "appId": app_id,
            "status": "running",
        }))
    }

    /// Stop a modal VM.
    ///
    /// Modal sandboxes have no suspend: terminate ends the container. What
    /// makes this a *suspend* in Caspar's terms is that the VM's Volume is
    /// untouched, so a later `run_vm` mounts the same `/data` into a fresh
    /// sandbox and the VM continues where it left off. `purge` removes the
    /// volume too, which is what makes the stop unrecoverable.
    fn terminate_vm_inner(&self, packet: &JsonValue) -> Result<JsonValue, String> {
        let identity = ModalIdentity::from_packet(packet);
        let purge = packet["purge"].as_bool().unwrap_or(false);
        // Cancels an in-flight asynchronous worker. Its completion path owns
        // cleanup of the exact sandbox if Modal finishes after this call.
        state_del(&[provisioning_key(&identity.vm_id)]);
        let sandbox_id = state_get(&sandbox_link_key(&identity.vm_id));

        let mut terminated = false;
        if !sandbox_id.is_empty() {
            let mut conn = self.conn()?;
            // Make `/data` durable before the container that holds the
            // uncommitted writes goes away. Skipped on a purge, where the
            // volume is about to be deleted anyway.
            if !purge {
                self.settle_volume_writes(&identity.vm_id);
            }
            block_on(conn.stub.sandbox_terminate(proto::SandboxTerminateRequest {
                sandbox_id: sandbox_id.clone(),
            }))?
            .map_err(|e| format!("modal SandboxTerminate failed: {}", e))?;
            terminated = true;
        }

        if let Some(h) = host() {
            h.commit_vm_buffer(&identity.vm_id);
            h.unregister_vm_context(&identity.vm_id);
        }

        let mut purged = false;
        if purge {
            let volume_id = state_get(&volume_link_key(&identity.vm_id));
            if !volume_id.is_empty() {
                let mut conn = self.conn()?;
                block_on(conn.stub.volume_delete(proto::VolumeDeleteRequest {
                    volume_id,
                    ..Default::default()
                }))?
                .map_err(|e| format!("modal VolumeDelete failed: {}", e))?;
                purged = true;
            }
            state_del(&[
                sandbox_link_key(&identity.vm_id),
                volume_link_key(&identity.vm_id),
                provisioning_error_key(&identity.vm_id),
            ]);
        }

        Ok(json!({
            "ok": true,
            "runtime": "modal",
            "machineId": identity.machine_id,
            "vmId": identity.vm_id,
            "sandboxId": sandbox_id,
            "terminated": terminated,
            "status": if purge { "deleted" } else { "suspended" },
            "purged": purged,
        }))
    }

    /// Permanently destroy a modal VM: the sandbox is terminated, its Volume
    /// deleted, and every state link that described it dropped. Nothing is
    /// left for a later run to resume from — which is the difference from
    /// terminate, where the volume deliberately survives.
    fn delete_vm_inner(&self, packet: &JsonValue) -> Result<JsonValue, String> {
        let identity = ModalIdentity::from_packet(packet);
        let mut purge_packet = packet.clone();
        if let Some(obj) = purge_packet.as_object_mut() {
            obj.insert("purge".to_string(), JsonValue::Bool(true));
        }
        let terminated = self.terminate_vm_inner(&purge_packet)?;
        state_del(&[
            sandbox_link_key(&identity.vm_id),
            volume_link_key(&identity.vm_id),
        ]);
        log_vm(
            format!("modal sandbox for vm {} deleted", identity.vm_id),
            identity.vm_id.clone(),
            "runtime",
        );
        Ok(json!({
            "ok": true,
            "runtime": "modal",
            "machineId": identity.machine_id,
            "vmId": identity.vm_id,
            "deleted": true,
            "terminate": terminated,
        }))
    }

    fn status_vm_inner(&self, packet: &JsonValue) -> Result<JsonValue, String> {
        let identity = ModalIdentity::from_packet(packet);
        if fresh_provisioning_marker(&identity.vm_id).is_some() {
            return Ok(json!({
                "ok": true,
                "runtime": "modal",
                "vmId": identity.vm_id,
                "status": "provisioning",
                "running": false,
            }));
        }
        let marker_key = provisioning_key(&identity.vm_id);
        if !state_get(&marker_key).is_empty() {
            let error = "modal provisioning did not complete within 20 minutes";
            state_del(&[marker_key]);
            state_put(&provisioning_error_key(&identity.vm_id), error);
            return Ok(json!({
                "ok": false,
                "runtime": "modal",
                "vmId": identity.vm_id,
                "status": "failed",
                "running": false,
                "error": error,
            }));
        }
        let provision_error = state_get(&provisioning_error_key(&identity.vm_id));
        if !provision_error.is_empty() {
            return Ok(json!({
                "ok": false,
                "runtime": "modal",
                "vmId": identity.vm_id,
                "status": "failed",
                "running": false,
                "error": provision_error,
            }));
        }
        let sandbox_id = state_get(&sandbox_link_key(&identity.vm_id));
        if sandbox_id.is_empty() {
            return Ok(json!({
                "ok": true,
                "runtime": "modal",
                "vmId": identity.vm_id,
                "status": "absent",
                "running": false,
            }));
        }
        let mut conn = self.conn()?;
        let running = self.is_running(&mut conn, &sandbox_id)?;
        Ok(json!({
            "ok": true,
            "runtime": "modal",
            "machineId": identity.machine_id,
            "vmId": identity.vm_id,
            "sandboxId": sandbox_id,
            "status": if running { "running" } else { "stopped" },
            "running": running,
        }))
    }

    /// Run a command inside a running sandbox and collect its output.
    fn exec_vm_inner(&self, packet: &JsonValue) -> Result<JsonValue, String> {
        let identity = ModalIdentity::from_packet(packet);
        let command = packet["command"].as_str().unwrap_or("").trim().to_string();
        let argv = string_list(&packet["args"]);
        if command.is_empty() && argv.is_empty() {
            return Err("command is required".to_string());
        }
        let argv = if argv.is_empty() {
            vec!["sh".to_string(), "-lc".to_string(), command]
        } else {
            argv
        };

        let sandbox_id = self.sandbox_id(&identity.vm_id)?;
        let mut conn = self.conn()?;
        let task_id = self.task_id(&mut conn, &sandbox_id)?;

        let timeout_secs = packet["timeoutSecs"].as_u64().unwrap_or(300) as u32;
        let exec = block_on(conn.stub.container_exec(proto::ContainerExecRequest {
            task_id,
            command: argv,
            stdout_output: proto::ExecOutputOption::Pipe as i32,
            stderr_output: proto::ExecOutputOption::Pipe as i32,
            timeout_secs,
            workdir: packet["workdir"].as_str().map(|s| s.to_string()),
            ..Default::default()
        }))?
        .map_err(|e| format!("modal ContainerExec failed: {}", e))?
        .into_inner();

        let stdout = self.collect_exec_output(
            &mut conn,
            &exec.exec_id,
            proto::FileDescriptor::Stdout,
            timeout_secs,
        )?;
        let stderr = self.collect_exec_output(
            &mut conn,
            &exec.exec_id,
            proto::FileDescriptor::Stderr,
            timeout_secs,
        )?;

        let wait = block_on(conn.stub.container_exec_wait(proto::ContainerExecWaitRequest {
            exec_id: exec.exec_id.clone(),
            timeout: timeout_secs as f32,
        }))?
        .map_err(|e| format!("modal ContainerExecWait failed: {}", e))?
        .into_inner();

        let exit_code = wait.exit_code.unwrap_or(0);
        Ok(json!({
            "ok": exit_code == 0,
            "runtime": "modal",
            "vmId": identity.vm_id,
            "sandboxId": sandbox_id,
            "execId": exec.exec_id,
            "exitCode": exit_code,
            "completed": wait.completed,
            "stdout": stdout,
            "stderr": stderr,
        }))
    }

    /// Drain one output stream of an exec into a string.
    fn collect_exec_output(
        &self,
        conn: &mut ModalConn,
        exec_id: &str,
        fd: proto::FileDescriptor,
        timeout_secs: u32,
    ) -> Result<String, String> {
        let mut stream = block_on(conn.stub.container_exec_get_output(
            proto::ContainerExecGetOutputRequest {
                exec_id: exec_id.to_string(),
                timeout: timeout_secs as f32,
                last_batch_index: 0,
                file_descriptor: fd as i32,
                get_raw_bytes: true,
            },
        ))?
        .map_err(|e| format!("modal ContainerExecGetOutput failed: {}", e))?
        .into_inner();

        let mut out = String::new();
        loop {
            let next = block_on(stream.next())?;
            let Some(batch) = next else { break };
            let batch = batch.map_err(|e| format!("modal exec output stream failed: {}", e))?;
            for item in batch
                .items
                .iter()
                .chain(batch.stdout.iter())
                .chain(batch.stderr.iter())
            {
                if !item.message.is_empty() {
                    out.push_str(&item.message);
                } else if !item.message_bytes.is_empty() {
                    out.push_str(&String::from_utf8_lossy(&item.message_bytes));
                }
            }
            if batch.exit_code.is_some() {
                break;
            }
        }
        Ok(out)
    }

    /// Write a file into the sandbox through Modal's container filesystem API.
    fn copy_to_vm_inner(&self, packet: &JsonValue) -> Result<JsonValue, String> {
        let identity = ModalIdentity::from_packet(packet);
        let path = packet["path"]
            .as_str()
            .or_else(|| packet["targetPath"].as_str())
            .unwrap_or("")
            .trim()
            .to_string();
        if path.is_empty() {
            return Err("path is required".to_string());
        }
        let data = decode_payload(packet)?;

        let sandbox_id = self.sandbox_id(&identity.vm_id)?;
        let mut conn = self.conn()?;
        let task_id = self.task_id(&mut conn, &sandbox_id)?;

        // Create the parent directory first: a write into a missing directory
        // fails, and callers copy into paths the image never created.
        if let Some(parent) = std::path::Path::new(&path).parent() {
            let parent = parent.to_string_lossy().to_string();
            if !parent.is_empty() && parent != "/" {
                let _ = self.filesystem_exec(
                    &mut conn,
                    &task_id,
                    proto::container_filesystem_exec_request::FileExecRequestOneof::FileMkdirRequest(
                        proto::ContainerFileMkdirRequest {
                            path: parent,
                            make_parents: true,
                        },
                    ),
                );
            }
        }

        let open = self.filesystem_exec(
            &mut conn,
            &task_id,
            proto::container_filesystem_exec_request::FileExecRequestOneof::FileOpenRequest(
                proto::ContainerFileOpenRequest {
                    file_descriptor: None,
                    path: path.clone(),
                    mode: "w".to_string(),
                },
            ),
        )?;
        let descriptor = open
            .file_descriptor
            .clone()
            .ok_or_else(|| format!("modal did not return a file descriptor for {}", path))?;

        self.filesystem_exec(
            &mut conn,
            &task_id,
            proto::container_filesystem_exec_request::FileExecRequestOneof::FileWriteRequest(
                proto::ContainerFileWriteRequest {
                    file_descriptor: descriptor.clone(),
                    data,
                },
            ),
        )?;
        self.filesystem_exec(
            &mut conn,
            &task_id,
            proto::container_filesystem_exec_request::FileExecRequestOneof::FileCloseRequest(
                proto::ContainerFileCloseRequest {
                    file_descriptor: descriptor,
                },
            ),
        )?;

        Ok(json!({
            "ok": true,
            "runtime": "modal",
            "vmId": identity.vm_id,
            "sandboxId": sandbox_id,
            "path": path,
        }))
    }

    /// Read a file out of the sandbox, base64-encoded so binary content
    /// survives the JSON packet.
    fn copy_from_vm_inner(&self, packet: &JsonValue) -> Result<JsonValue, String> {
        let identity = ModalIdentity::from_packet(packet);
        let path = packet["path"]
            .as_str()
            .or_else(|| packet["sourcePath"].as_str())
            .unwrap_or("")
            .trim()
            .to_string();
        if path.is_empty() {
            return Err("path is required".to_string());
        }

        let sandbox_id = self.sandbox_id(&identity.vm_id)?;
        let mut conn = self.conn()?;
        let task_id = self.task_id(&mut conn, &sandbox_id)?;

        let open = self.filesystem_exec(
            &mut conn,
            &task_id,
            proto::container_filesystem_exec_request::FileExecRequestOneof::FileOpenRequest(
                proto::ContainerFileOpenRequest {
                    file_descriptor: None,
                    path: path.clone(),
                    mode: "r".to_string(),
                },
            ),
        )?;
        let descriptor = open
            .file_descriptor
            .clone()
            .ok_or_else(|| format!("modal did not return a file descriptor for {}", path))?;

        let read = self.filesystem_exec(
            &mut conn,
            &task_id,
            proto::container_filesystem_exec_request::FileExecRequestOneof::FileReadRequest(
                proto::ContainerFileReadRequest {
                    file_descriptor: descriptor.clone(),
                    n: None,
                },
            ),
        )?;
        let bytes = self.collect_filesystem_output(&mut conn, &read.exec_id)?;

        let _ = self.filesystem_exec(
            &mut conn,
            &task_id,
            proto::container_filesystem_exec_request::FileExecRequestOneof::FileCloseRequest(
                proto::ContainerFileCloseRequest {
                    file_descriptor: descriptor,
                },
            ),
        );

        Ok(json!({
            "ok": true,
            "runtime": "modal",
            "vmId": identity.vm_id,
            "sandboxId": sandbox_id,
            "path": path,
            "size": bytes.len(),
            "dataBase64": BASE64.encode(&bytes),
        }))
    }

    fn filesystem_exec(
        &self,
        conn: &mut ModalConn,
        task_id: &str,
        op: proto::container_filesystem_exec_request::FileExecRequestOneof,
    ) -> Result<proto::ContainerFilesystemExecResponse, String> {
        let response = block_on(conn.stub.container_filesystem_exec(
            proto::ContainerFilesystemExecRequest {
                file_exec_request_oneof: Some(op),
                task_id: task_id.to_string(),
            },
        ))?
        .map_err(|e| format!("modal ContainerFilesystemExec failed: {}", e))?
        .into_inner();
        Ok(response)
    }

    fn collect_filesystem_output(
        &self,
        conn: &mut ModalConn,
        exec_id: &str,
    ) -> Result<Vec<u8>, String> {
        let mut stream = block_on(conn.stub.container_filesystem_exec_get_output(
            proto::ContainerFilesystemExecGetOutputRequest {
                exec_id: exec_id.to_string(),
                timeout: 60.0,
            },
        ))?
        .map_err(|e| format!("modal ContainerFilesystemExecGetOutput failed: {}", e))?
        .into_inner();

        let mut out: Vec<u8> = Vec::new();
        loop {
            let next = block_on(stream.next())?;
            let Some(batch) = next else { break };
            let batch =
                batch.map_err(|e| format!("modal filesystem output stream failed: {}", e))?;
            if let Some(error) = batch.error.as_ref() {
                return Err(format!(
                    "modal filesystem operation failed: {}",
                    error.error_message
                ));
            }
            for chunk in &batch.output {
                out.extend_from_slice(chunk);
            }
            if batch.eof {
                break;
            }
        }
        Ok(out)
    }

    /// Proxy an inbound HTTP request to the sandbox's tunnel.
    ///
    /// A modal sandbox exposes its ports through Modal-hosted tunnels, so
    /// unlike docker there is no node-local address to reach: the tunnel host
    /// is looked up per request (cheap, and correct after a restart) and the
    /// request replayed against it. When the sandbox exposes no tunnel the
    /// generic signal fallback still applies, so a creature that serves over
    /// signals rather than HTTP keeps working.
    fn forward_http_inner(&self, packet: &JsonValue) -> Result<JsonValue, String> {
        let identity = ModalIdentity::from_packet(packet);
        let sandbox_id = match state_get(&sandbox_link_key(&identity.vm_id)) {
            id if id.is_empty() => {
                return caspar_vm_sdk::plugin::forward_http_via_signal(packet)
            }
            id => id,
        };

        let mut conn = self.conn()?;
        let tunnels = block_on(conn.stub.sandbox_get_tunnels(proto::SandboxGetTunnelsRequest {
            sandbox_id: sandbox_id.clone(),
            timeout: 30.0,
        }))?
        .map_err(|e| format!("modal SandboxGetTunnels failed: {}", e))?
        .into_inner();

        let http_port = vm_http_port();
        let tunnel = tunnels
            .tunnels
            .iter()
            .find(|t| t.container_port == http_port)
            .or_else(|| tunnels.tunnels.first());
        let Some(tunnel) = tunnel else {
            return caspar_vm_sdk::plugin::forward_http_via_signal(packet);
        };

        let path = packet["path"].as_str().unwrap_or("/");
        let path = if path.starts_with('/') {
            path.to_string()
        } else {
            format!("/{}", path)
        };
        let query = packet["query"].as_str().unwrap_or("");
        let url = if query.is_empty() {
            format!("https://{}:{}{}", tunnel.host, tunnel.port, path)
        } else {
            format!("https://{}:{}{}?{}", tunnel.host, tunnel.port, path, query)
        };

        let method = packet["method"].as_str().unwrap_or("GET").to_uppercase();
        let method = reqwest::Method::from_bytes(method.as_bytes())
            .map_err(|e| format!("invalid HTTP method: {}", e))?;
        let body = BASE64
            .decode(packet["bodyBase64"].as_str().unwrap_or(""))
            .unwrap_or_default();

        let client = reqwest::blocking::Client::builder()
            .timeout(std::time::Duration::from_secs(vm_http_timeout_secs()))
            .build()
            .map_err(|e| format!("http client init failed: {}", e))?;
        let mut request = client.request(method, &url).body(body);
        if let Some(headers) = packet["headers"].as_object() {
            for (name, value) in headers {
                if let Some(value) = value.as_str() {
                    // Hop-by-hop headers describe the connection to the node,
                    // not the request, and re-sending them corrupts the proxied
                    // exchange.
                    let lower = name.to_ascii_lowercase();
                    if lower == "host" || lower == "connection" || lower == "content-length" {
                        continue;
                    }
                    request = request.header(name.as_str(), value);
                }
            }
        }

        let response = request
            .send()
            .map_err(|e| format!("modal tunnel request failed ({}): {}", url, e))?;
        let status = response.status().as_u16();
        let mut headers = Map::new();
        for (name, value) in response.headers().iter() {
            if let Ok(value) = value.to_str() {
                headers.insert(name.as_str().to_string(), json!(value));
            }
        }
        let body = response
            .bytes()
            .map_err(|e| format!("failed to read modal tunnel response: {}", e))?;

        Ok(json!({
            "ok": (200..400).contains(&status),
            "status": status,
            "headers": JsonValue::Object(headers),
            "bodyBase64": BASE64.encode(&body),
        }))
    }
}

/// Sandbox lifetime. Modal terminates a sandbox at `timeout_secs`, so the
/// value is the VM's max lifetime, not one command's — a VM the platform
/// keeps addressing must not be capped at a single exec's budget.
fn sandbox_timeout_secs(packet: &JsonValue, max_exec_time_secs: u64) -> u32 {
    if let Some(explicit) = packet["timeoutSecs"].as_u64() {
        return explicit as u32;
    }
    let configured = std::env::var("MODAL_SANDBOX_TIMEOUT_SECS")
        .ok()
        .and_then(|v| v.trim().parse::<u32>().ok());
    if let Some(configured) = configured {
        return configured;
    }
    // A standalone VM is long-lived; the entity's exec budget is only a floor.
    let from_limits = max_exec_time_secs as u32;
    from_limits.max(60 * 60)
}

fn tag(name: &str, value: &str) -> proto::SandboxTag {
    proto::SandboxTag {
        tag_name: name.to_string(),
        tag_value: value.to_string(),
    }
}

/// Quote one argument for `sh -lc`.
fn shell_quote(raw: &str) -> String {
    format!("'{}'", raw.replace('\'', r"'\''"))
}

/// The bytes a copy-in packet carries, in either of the two shapes callers
/// use: base64 (binary safe) or inline text.
fn decode_payload(packet: &JsonValue) -> Result<Vec<u8>, String> {
    if let Some(b64) = packet["dataBase64"].as_str() {
        return BASE64
            .decode(b64)
            .map_err(|e| format!("invalid base64 payload: {}", e));
    }
    if let Some(text) = packet["data"].as_str() {
        return Ok(text.as_bytes().to_vec());
    }
    if let Some(text) = packet["content"].as_str() {
        return Ok(text.as_bytes().to_vec());
    }
    Err("data or dataBase64 is required".to_string())
}

// ── VmPlugin ──────────────────────────────────────────────────────────────

impl VmPlugin for ModalVmPlugin {
    fn meta(&self) -> &VmPluginMeta {
        &self.meta
    }

    fn run_vm(&self, packet: &JsonValue) -> Result<JsonValue, String> {
        self.run_vm_inner(packet)
    }

    fn terminate_vm(&self, packet: &JsonValue) -> Result<JsonValue, String> {
        self.terminate_vm_inner(packet)
    }

    fn delete_vm(&self, packet: &JsonValue) -> Result<JsonValue, String> {
        self.delete_vm_inner(packet)
    }

    fn status_vm(&self, packet: &JsonValue) -> Result<JsonValue, String> {
        self.status_vm_inner(packet)
    }

    fn exec_vm(&self, packet: &JsonValue) -> Result<JsonValue, String> {
        self.exec_vm_inner(packet)
    }

    fn copy_to_vm(&self, packet: &JsonValue) -> Result<JsonValue, String> {
        self.copy_to_vm_inner(packet)
    }

    fn copy_from_vm(&self, packet: &JsonValue) -> Result<JsonValue, String> {
        self.copy_from_vm_inner(packet)
    }

    fn forward_http(&self, packet: &JsonValue) -> Result<JsonValue, String> {
        self.forward_http_inner(packet)
    }

    /// A modal sandbox's Modal-hosted tunnels — the public URL each exposed
    /// port is reachable on. This is what lets a creature hand a member the
    /// address of something running inside the project's own machine.
    fn vm_endpoints(&self, packet: &JsonValue) -> Result<JsonValue, String> {
        let identity = ModalIdentity::from_packet(packet);
        let sandbox_id = state_get(&sandbox_link_key(&identity.vm_id));
        if sandbox_id.is_empty() {
            return Ok(json!({"ok": true, "runtime": "modal", "endpoints": []}));
        }
        let mut conn = self.conn()?;
        let tunnels = block_on(conn.stub.sandbox_get_tunnels(proto::SandboxGetTunnelsRequest {
            sandbox_id: sandbox_id.clone(),
            timeout: packet["timeout"].as_f64().unwrap_or(30.0) as f32,
        }))?
        .map_err(|e| format!("modal SandboxGetTunnels failed: {}", e))?
        .into_inner();

        let endpoints: Vec<JsonValue> = tunnels
            .tunnels
            .iter()
            .map(|t| {
                json!({
                    "containerPort": t.container_port,
                    "host": t.host,
                    "port": t.port,
                    // Modal terminates TLS on the tunnel, so the public form is
                    // always https — the port is only in the URL when it is not
                    // the default, which is what a browser will accept.
                    "url": if t.port == 443 {
                        format!("https://{}", t.host)
                    } else {
                        format!("https://{}:{}", t.host, t.port)
                    },
                })
            })
            .collect();

        Ok(json!({
            "ok": true,
            "runtime": "modal",
            "vmId": identity.vm_id,
            "sandboxId": sandbox_id,
            "endpoints": endpoints,
        }))
    }

    /// Build the sandbox's image from a deployed `Modalfile` without starting
    /// anything, so a deploy can fail on a bad recipe rather than at the first
    /// run.
    fn build_image(&self, packet: &JsonValue) -> Result<JsonValue, String> {
        let identity = ModalIdentity::from_packet(packet);
        if identity.machine_id.is_empty() {
            return Err("machineId is required".to_string());
        }
        let mut conn = self.conn()?;
        let app_id = self.app_id(&mut conn, &identity.machine_id)?;
        let image_id = self.image_id(
            &mut conn,
            &app_id,
            &identity.machine_id,
            &identity.entity_id,
            packet,
        )?;
        Ok(json!({
            "ok": true,
            "runtime": "modal",
            "machineId": identity.machine_id,
            "entityId": identity.entity_id,
            "appId": app_id,
            "imageId": image_id,
        }))
    }

    /// Re-attach to the sandboxes a snapshot says were running.
    ///
    /// A modal sandbox is not hosted by this node, so a node restart does not
    /// stop it: the correct restore is to check whether the recorded sandbox
    /// is still alive and adopt it, and only start a new one when it is gone.
    /// Relaunching unconditionally (what the SDK default does) would strand
    /// the running sandbox and bill for two.
    fn restore(&self, snapshot_entry: &JsonValue) -> Result<JsonValue, String> {
        let identity = ModalIdentity::from_packet(snapshot_entry);
        let sandbox_id = state_get(&sandbox_link_key(&identity.vm_id));
        if !sandbox_id.is_empty() {
            if let Ok(mut conn) = self.conn() {
                if self.is_running(&mut conn, &sandbox_id).unwrap_or(false) {
                    if let Some(h) = host() {
                        h.register_vm_context(
                            &identity.vm_id,
                            &identity.creature_id,
                            &identity.machine_id,
                        );
                    }
                    return Ok(json!({
                        "ok": true,
                        "runtime": "modal",
                        "vmId": identity.vm_id,
                        "sandboxId": sandbox_id,
                        "restored": "reattached",
                    }));
                }
            }
        }
        self.run_vm_inner(snapshot_entry)
    }

    /// A modal sandbox has no address on any node-local network, so a
    /// connection's source IP can never identify one.
    fn identify_instance_by_ip(&self, _ip: &str) -> Option<String> {
        None
    }

    /// Standalone runEntity plan.
    ///
    /// Unlike docker there is no name to allocate up front — the sandbox id
    /// only exists after Modal creates it, and `run_vm` records it against the
    /// vm id itself — so the plan writes no links and simply carries the
    /// launch parameters, with `params` passed through as the sandbox's
    /// environment.
    fn plan_run_entity(&self, ctx: &JsonValue) -> Result<JsonValue, String> {
        Ok(json!({
            "input": {
                "runtime": "modal",
                "machineId": ctx["machineId"],
                "creatureId": ctx["creatureId"],
                "entityId": ctx["entityId"],
                "vmId": ctx["vmId"],
                "standalone": true,
                "resources": ctx["resources"],
                "env": ctx["params"],
            },
            "links": [],
        }))
    }

    /// Standalone stopEntity plan: the sandbox is addressed by vm id (the
    /// plugin resolves the sandbox id from its own state), so no per-runtime
    /// link has to be resolved by the caller.
    fn plan_stop_entity(&self, ctx: &JsonValue) -> Result<JsonValue, String> {
        Ok(json!({
            "input": {
                "runtime": "modal",
                "machineId": ctx["machineId"],
                "entityId": ctx["entityId"],
                "vmId": ctx["vmId"],
            },
            "links": [],
        }))
    }

    fn build_terminate_request(&self, input: &JsonValue) -> Result<JsonValue, String> {
        let vm_id = input["vmId"].as_str().unwrap_or("").trim();
        if vm_id.is_empty() {
            return Err("modal terminate requires a vmId".to_string());
        }
        Ok(json!({
            "type": "terminateVm",
            "runtime": "modal",
            "machineId": input["machineId"].as_str().unwrap_or(""),
            "entityId": input["entityId"].as_str().unwrap_or("main"),
            "vmId": vm_id,
            "purge": input["purge"].as_bool().unwrap_or(false),
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resource_names_are_stable_and_modal_safe() {
        let a = crate::models::modal_app_name("7@global");
        assert!(a
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-'));
        assert_eq!(a, crate::models::modal_app_name("7@global"));
    }

    #[test]
    fn entrypoint_defaults_keep_the_sandbox_alive() {
        let packet = json!({});
        assert_eq!(entrypoint_args(&packet), vec!["sleep", "infinity"]);
        let packet = json!({"command": "python -m http.server"});
        assert_eq!(
            entrypoint_args(&packet),
            vec!["sh", "-lc", "python -m http.server"]
        );
    }

    /// `diskGb` must never become Modal's ephemeral disk, and a packet with no
    /// usable memory must not be sent as the 1 MiB the generic parser floors it
    /// to — Modal refuses both, and every sandbox create failed with
    /// `InvalidArgument` because of it.
    #[test]
    fn sandbox_resources_ignore_disk_gb_and_floor_memory() {
        let packet = json!({
            "resources": { "ramMb": 2048, "cpuCores": 2, "diskGb": 4 }
        });
        let res = sandbox_resources(&packet, &parse_vm_resource_limits(&packet));
        assert_eq!(res.memory_mb, 2048);
        assert_eq!(res.milli_cpu, 2000);
        assert_eq!(res.ephemeral_disk_mb, 0, "diskGb must not become a scratch disk");

        // What an unseeded deployment sends: a resource block of zeroes.
        let empty = json!({ "resources": { "ramMb": 0, "cpuCores": 0, "diskGb": 0 } });
        let res = sandbox_resources(&empty, &parse_vm_resource_limits(&empty));
        assert!(res.memory_mb >= MIN_MEMORY_MB, "memory must be usable: {}", res.memory_mb);
        assert_eq!(res.milli_cpu, 1000);
        assert_eq!(res.ephemeral_disk_mb, 0);

        // A scratch disk asked for BY NAME is honoured, clamped into range.
        let scratch = json!({ "ephemeralDiskGb": 1, "resources": {} });
        let res = sandbox_resources(&scratch, &parse_vm_resource_limits(&scratch));
        assert_eq!(res.ephemeral_disk_mb, MIN_EPHEMERAL_DISK_MB);
    }

    #[test]
    fn http_port_is_always_exposed() {
        let packet = json!({"ports": [5900]});
        let ports = exposed_ports(&packet);
        assert!(ports.contains(&5900));
        assert!(ports.contains(&vm_http_port()));
    }

    #[test]
    fn shell_quoting_survives_embedded_quotes() {
        assert_eq!(shell_quote("it's"), r"'it'\''s'");
    }
}
