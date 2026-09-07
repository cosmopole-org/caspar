//! The docker VM controller — container lifecycle, image builds, exec and
//! file transfer, exposed to the Caspar VMM through the
//! `caspar_vm_sdk::VmPlugin` interface.

use std::collections::HashMap;
use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::Duration;

use bollard::container::{
    Config as DockerConfig, CreateContainerOptions, ListContainersOptions, LogOutput, LogsOptions,
    RemoveContainerOptions, StartContainerOptions, StopContainerOptions, UploadToContainerOptions,
};
use bollard::errors::Error as BollardError;
use bollard::exec::{CreateExecOptions, StartExecResults};
use bollard::image::BuildImageOptions;
use bollard::models::HostConfig;
use bollard::Docker;
use futures_util::stream::TryStreamExt;
use serde_json::{json, Map, Value as JsonValue};
use tar::Builder as TarBuilder;

use caspar_vm_sdk::host::host;
use caspar_vm_sdk::{parse_vm_resource_limits, VmPlugin, VmPluginMeta};

use crate::models::DockerIdentity;

/// Name of the docker bridge network shared with the docker-host gateway.
/// Must match the network the node's gateway advertises on; overridable for
/// non-standard deployments.
fn gateway_network_name() -> String {
    std::env::var("CASPAR_GATEWAY_NETWORK").unwrap_or_else(|_| "kasper".to_string())
}

/// TCP port the HTTP server inside a container listens on. The VMM ingress
/// proxies forwarded requests here; the same value is injected into the
/// container's environment so the application knows where to bind.
fn vm_http_port() -> u16 {
    std::env::var("CASPAR_VM_HTTP_PORT")
        .ok()
        .and_then(|p| p.trim().parse::<u16>().ok())
        .unwrap_or(8080)
}

/// Timeout (seconds) for a forwarded request to the container HTTP server.
fn vm_http_timeout_secs() -> u64 {
    std::env::var("CASPAR_VM_HTTP_TIMEOUT_SECS")
        .ok()
        .and_then(|p| p.trim().parse::<u64>().ok())
        .filter(|s| *s > 0)
        .unwrap_or(30)
}

pub struct DockerVmPlugin {
    meta: VmPluginMeta,
}

struct DockerVmController {
    docker: Docker,
}

impl DockerVmPlugin {
    pub fn new(meta: VmPluginMeta) -> Self {
        Self { meta }
    }

    fn with_controller<T>(
        &self,
        f: impl FnOnce(&DockerVmController) -> Result<T, String>,
    ) -> Result<T, String> {
        let controller = DockerVmController::new()?;
        f(&controller)
    }
}

impl DockerVmController {
    fn new() -> Result<Self, String> {
        Docker::connect_with_local_defaults()
            .map(|docker| Self { docker })
            .map_err(|e| format!("docker client init failed: {}", e))
    }

    fn with_async<T, F>(&self, fut: F) -> Result<T, String>
    where
        F: std::future::Future<Output = Result<T, BollardError>>,
    {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| format!("tokio runtime init failed: {}", e))?
            .block_on(fut)
            .map_err(|e| format!("docker api error: {}", e))
    }

    fn run_vm(&self, packet: &JsonValue) -> Result<JsonValue, String> {
        let machine_id = packet["machineId"].as_str().unwrap_or("");
        if machine_id.is_empty() {
            return Err("machineId is required".to_string());
        }
        let creature_id = packet["creatureId"]
            .as_str()
            .or_else(|| packet["userId"].as_str())
            .unwrap_or("")
            .to_string();
        let identity = DockerIdentity::from_packet(packet);
        let (entity_id, container_name, vm_id) =
            (identity.entity_id, identity.container_name, identity.vm_id);
        let vm_cache_key = if vm_id.is_empty() {
            "main".to_string()
        } else {
            vm_id.clone()
        };
        let container_id = docker_container_id(machine_id, &entity_id, &container_name, &vm_id);
        let image_ref = packet["imageRef"]
            .as_str()
            .map(|s| s.to_string())
            .unwrap_or_else(|| docker_image_ref(machine_id, &entity_id));
        let limits = parse_vm_resource_limits(packet);

        // Sandbox semantics (parallel to the fire runtime):
        //   - `persistent: true` (the default) carves a non-escapable per-VM
        //     dir under `{storage}/vms/<vm>` bind-mounted as `/data`.
        //   - `forceRestart: false` makes `run_vm` idempotent: an existing
        //     container is started (resumed) instead of rebuilt.
        let persistent = packet["persistent"].as_bool().unwrap_or(true);
        let force_restart = packet["forceRestart"].as_bool().unwrap_or(false);
        let mount_dir = if persistent {
            Some(docker_session_vm_dir(&vm_cache_key)?)
        } else {
            None
        };

        if !force_restart {
            if let Some(status) = self.container_state(&container_id)? {
                if status == "running" {
                    return Ok(json!({
                        "ok": true,
                        "machineId": machine_id,
                        "vmId": vm_cache_key,
                        "containerId": container_id,
                        "runtime": "docker",
                        "status": "already-running",
                        "persistent": persistent,
                        "vmDir": mount_dir.as_ref().map(|p| p.display().to_string()).unwrap_or_default(),
                    }));
                }
                // Container exists but is stopped (suspended). A serving creature
                // (e.g. a tool) that idled out takes exactly this path when it is
                // next signalled. Start it again — its writable layer and any
                // persistent bind mount survive.
                self.with_async(self.docker.start_container::<String>(
                    &container_id,
                    None::<StartContainerOptions<String>>,
                ))?;
                if let Some(h) = host() {
                    h.register_vm_context(&vm_cache_key, &creature_id, machine_id);
                    // Re-bind the container name to its identity so the gateway can
                    // resolve the restarted container from its source IP on the new
                    // connection (the create path does the same) — this is what lets
                    // the node flush the queued signals to the resumed tool.
                    // program_id == machine_id for docker.
                    h.register_vm_container(&container_id, &vm_cache_key, &creature_id, machine_id, machine_id, &entity_id);
                }
                return Ok(json!({
                    "ok": true,
                    "machineId": machine_id,
                    "vmId": vm_cache_key,
                    "containerId": container_id,
                    "runtime": "docker",
                    "status": "resumed",
                    "persistent": persistent,
                    "vmDir": mount_dir.as_ref().map(|p| p.display().to_string()).unwrap_or_default(),
                }));
            }
        } else {
            self.stop_and_remove_if_exists(&container_id)?;
        }

        // Base env from the caller, then the docker-host bridge gateway
        // coordinates + this container's verified identity.
        let mut env_vars = packet["env"]
            .as_array()
            .map(|v| {
                v.iter()
                    .filter_map(|x| x.as_str().map(|s| s.to_string()))
                    .collect::<Vec<String>>()
            })
            .unwrap_or_default();
        env_vars.extend(gateway_container_env(&vm_cache_key));
        let env = Some(env_vars);
        let cmd = packet["command"]
            .as_str()
            .map(|command| vec!["sh".to_string(), "-lc".to_string(), command.to_string()]);

        // Container sandbox runtime. Defaults to gVisor ("runsc"); environments
        // without gVisor set `CASPAR_DOCKER_RUNTIME=runc` (or `default`/empty).
        let runtime = match std::env::var("CASPAR_DOCKER_RUNTIME") {
            Ok(v) => {
                let v = v.trim();
                if v.is_empty()
                    || v.eq_ignore_ascii_case("default")
                    || v.eq_ignore_ascii_case("runc")
                {
                    None
                } else {
                    Some(v.to_string())
                }
            }
            Err(_) => Some("runsc".to_string()),
        };

        // Per-container disk quota via `storage_opt` only works on a backing
        // storage driver that supports it; opt out with
        // `CASPAR_DOCKER_DISK_QUOTA=0` (also accepts off/false/no).
        let storage_opt = match std::env::var("CASPAR_DOCKER_DISK_QUOTA") {
            Ok(v)
                if matches!(
                    v.trim().to_ascii_lowercase().as_str(),
                    "0" | "off" | "false" | "no"
                ) =>
            {
                None
            }
            _ => Some(HashMap::from([(
                "size".to_string(),
                format!("{}G", limits.disk_gb),
            )])),
        };

        // run_vm is launched fire-and-forget by /programs/runEntity, so a
        // failed create/start would otherwise be completely silent. Emit the
        // failure into THIS vm's log stream so the real cause is visible.
        self.with_async(self.docker.create_container(
            Some(CreateContainerOptions {
                name: container_id.clone(),
                platform: None,
            }),
            DockerConfig {
                image: Some(image_ref),
                env,
                cmd,
                host_config: Some(HostConfig {
                    runtime,
                    network_mode: Some(gateway_network_name()),
                    // Resolve `host.docker.internal` to the node host inside the
                    // bridge network so the container can dial the gateway.
                    extra_hosts: Some(vec!["host.docker.internal:host-gateway".to_string()]),
                    memory: Some((limits.ram_mb * 1024 * 1024) as i64),
                    cpu_count: Some(limits.cpu_cores as i64),
                    storage_opt,
                    // Per-session persistent storage bind-mounted as `/data`.
                    binds: mount_dir
                        .as_ref()
                        .map(|d| vec![format!("{}:/data", d.display())]),
                    ..Default::default()
                }),
                ..Default::default()
            },
        ))
        .map_err(|e| {
            emit_vm_log(&vm_cache_key, "runtime", &format!("CONTAINER_CREATE_FAILED {}", e));
            e
        })?;

        if !packet["inputFiles"].is_null() {
            let files = parse_input_files(&packet["inputFiles"])?;
            self.upload_files(&container_id, "/app/input", &files)?;
        }

        self.with_async(
            self.docker
                .start_container::<String>(&container_id, None::<StartContainerOptions<String>>),
        )
        .map_err(|e| {
            emit_vm_log(&vm_cache_key, "runtime", &format!("CONTAINER_START_FAILED {}", e));
            e
        })?;
        let timeout_container = container_id.clone();
        let timeout_vm = vm_cache_key.clone();
        let timeout_secs = limits.max_exec_time_secs;
        thread::spawn(move || {
            thread::sleep(Duration::from_secs(timeout_secs));
            if let Ok(docker) = Docker::connect_with_local_defaults() {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build();
                if let Ok(rt) = rt {
                    let _ = rt.block_on(docker.stop_container(
                        &timeout_container,
                        Some(StopContainerOptions { t: 1 }),
                    ));
                    let _ = rt.block_on(docker.remove_container(
                        &timeout_container,
                        Some(RemoveContainerOptions {
                            force: true,
                            v: true,
                            ..Default::default()
                        }),
                    ));
                }
            }
            if let Some(h) = host() {
                // Commit and remove the lifecycle transaction buffer.
                h.commit_vm_buffer(timeout_vm.as_str());
                h.unregister_vm_context(timeout_vm.as_str());
                // Drop the container→identity binding so its IP can't be reused.
                h.unregister_vm_container(timeout_container.as_str());
            }
        });
        let container_id_for_logs = container_id.clone();
        let vm_id_for_logs = vm_cache_key.clone();
        thread::spawn(move || {
            let docker = match Docker::connect_with_local_defaults() {
                Ok(d) => d,
                Err(_) => return,
            };
            let rt = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(r) => r,
                Err(_) => return,
            };
            rt.block_on(async move {
                let mut stream = docker.logs(
                    &container_id_for_logs,
                    Some(LogsOptions::<String> {
                        follow: true,
                        stdout: true,
                        stderr: true,
                        // "all" (not "0"): short-lived creatures print and exit
                        // within milliseconds, before this follower attaches.
                        tail: "all".to_string(),
                        ..Default::default()
                    }),
                );
                while let Some(msg) = stream.try_next().await.unwrap_or(None) {
                    match msg {
                        LogOutput::StdOut { message }
                        | LogOutput::StdErr { message }
                        | LogOutput::Console { message }
                        | LogOutput::StdIn { message } => {
                            let line = String::from_utf8_lossy(&message).to_string();
                            emit_vm_log(&vm_id_for_logs, "runtime", line.trim());
                        }
                    }
                }
            });
        });
        if let Some(h) = host() {
            h.register_vm_context(&vm_cache_key, &creature_id, machine_id);
            // Bind the docker container name to this VM's authoritative
            // identity so the gateway can resolve a connection's identity from
            // its source IP. program_id == machine_id for docker.
            h.register_vm_container(&container_id, &vm_cache_key, &creature_id, machine_id, machine_id, &entity_id);
            // Begin a lifecycle transaction buffer for this VM execution so all
            // dbOp writes are batched and committed atomically at VM end.
            h.begin_vm_buffer(&vm_cache_key);
        }
        Ok(json!({
            "ok": true,
            "machineId": machine_id,
            "vmId": vm_cache_key,
            "containerId": container_id,
            "runtime": "docker",
            "status": "running",
            "persistent": persistent,
            "vmDir": mount_dir.as_ref().map(|p| p.display().to_string()).unwrap_or_default(),
        }))
    }

    /// Turn a docker VM off. Default is a *suspend* (container stopped, not
    /// removed); `purge: true` removes the container and its persistent dir.
    fn terminate_vm(&self, packet: &JsonValue) -> Result<JsonValue, String> {
        let machine_id = packet["machineId"].as_str().unwrap_or("");
        if machine_id.is_empty() {
            return Err("machineId is required".to_string());
        }
        let entity_id = packet["entityId"]
            .as_str()
            .or_else(|| packet["imageName"].as_str())
            .unwrap_or("main")
            .to_string();
        let container_name = packet["containerName"]
            .as_str()
            .unwrap_or("main")
            .to_string();
        let vm_id = packet["vmId"].as_str().unwrap_or("").to_string();
        let vm_cache_key = if vm_id.is_empty() {
            "main".to_string()
        } else {
            vm_id.clone()
        };
        let purge = packet["purge"].as_bool().unwrap_or(false);
        let container_id = docker_container_id(machine_id, &entity_id, &container_name, &vm_id);

        if purge {
            self.stop_and_remove_if_exists(&container_id)?;
        } else {
            // Suspend: stop the container but DO NOT remove it.
            let _ = self.with_async(
                self.docker
                    .stop_container(&container_id, Some(StopContainerOptions { t: 1 })),
            );
        }

        if let Some(h) = host() {
            h.commit_vm_buffer(vm_cache_key.as_str());
            h.unregister_vm_context(vm_cache_key.as_str());
            // The container→identity binding is tied to a *running* IP; it must
            // be dropped on both suspend and purge.
            h.unregister_vm_container(container_id.as_str());
        }

        let mut purged = false;
        if purge {
            let dir = docker_vm_dir_path(&vm_cache_key);
            if dir.exists() && docker_is_within_vms_root(&dir) {
                std::fs::remove_dir_all(&dir)
                    .map_err(|e| format!("failed to purge sandbox dir {}: {}", dir.display(), e))?;
                purged = true;
            }
        }

        Ok(json!({
            "ok": true,
            "machineId": machine_id,
            "containerId": container_id,
            "runtime": "docker",
            "status": if purge { "deleted" } else { "suspended" },
            "purged": purged,
        }))
    }

    /// Permanently destroy a docker VM: the container is removed (never just
    /// stopped), its persistent `/data` dir is dropped, and — when the caller
    /// asks with `removeImage` — the image built for this entity goes too.
    ///
    /// Terminate's `purge` already removes the container and the dir; delete
    /// differs in that it is unconditional (a delete is never a suspend) and
    /// reaches the image, which no terminate does because a suspended VM must
    /// still have something to resume from.
    fn delete_vm(&self, packet: &JsonValue) -> Result<JsonValue, String> {
        let machine_id = packet["machineId"].as_str().unwrap_or("");
        if machine_id.is_empty() {
            return Err("machineId is required".to_string());
        }
        let mut purge_packet = packet.clone();
        if let Some(obj) = purge_packet.as_object_mut() {
            obj.insert("purge".to_string(), JsonValue::Bool(true));
        }
        let terminated = self.terminate_vm(&purge_packet)?;

        // The image is per (machine, entity), not per VM, so it is only
        // removed when the caller says the entity itself is going away —
        // deleting one instance must not break its siblings.
        let mut image_removed = false;
        if packet["removeImage"].as_bool().unwrap_or(false) {
            let entity_id = packet["entityId"]
                .as_str()
                .or_else(|| packet["imageName"].as_str())
                .unwrap_or("main");
            let image_ref = packet["imageRef"]
                .as_str()
                .map(|s| s.to_string())
                .unwrap_or_else(|| docker_image_ref(machine_id, entity_id));
            let removed = self.with_async(self.docker.remove_image(&image_ref, None, None));
            image_removed = removed.is_ok();
        }

        Ok(json!({
            "ok": true,
            "runtime": "docker",
            "machineId": machine_id,
            "vmId": packet["vmId"].as_str().unwrap_or("main"),
            "deleted": true,
            "imageRemoved": image_removed,
            "terminate": terminated,
        }))
    }

    /// Inspect the concrete container backing a standalone program entity.
    fn status_vm(&self, packet: &JsonValue) -> Result<JsonValue, String> {
        let machine_id = packet["machineId"].as_str().unwrap_or("");
        if machine_id.is_empty() {
            return Err("machineId is required".to_string());
        }
        let entity_id = packet["entityId"]
            .as_str()
            .or_else(|| packet["imageName"].as_str())
            .unwrap_or("main");
        let container_name = packet["containerName"].as_str().unwrap_or("main");
        let vm_id = packet["vmId"].as_str().unwrap_or("");
        let container_id = docker_container_id(machine_id, entity_id, container_name, vm_id);
        let status = self
            .container_state(&container_id)?
            .unwrap_or_else(|| "not-found".to_string());
        Ok(json!({
            "ok": true,
            "runtime": "docker",
            "machineId": machine_id,
            "vmId": vm_id,
            "containerId": container_id,
            "status": status,
            "running": status == "running",
        }))
    }

    fn exec_vm(&self, packet: &JsonValue) -> Result<JsonValue, String> {
        let machine_id = packet["machineId"].as_str().unwrap_or("");
        let identity = DockerIdentity::from_packet(packet);
        let command = packet["command"].as_str().unwrap_or("");
        if machine_id.is_empty() {
            return Err("machineId is required".to_string());
        }
        if command.is_empty() {
            return Err("command is required".to_string());
        }
        let container_id = docker_container_id(
            machine_id,
            &identity.entity_id,
            &identity.container_name,
            &identity.vm_id,
        );
        // Wake-on-exec: resume a suspended sandbox before running the command.
        match self.container_state(&container_id)? {
            Some(s) if s == "running" => {}
            _ => {
                let _ = self.run_vm(packet);
            }
        }
        let create_res = self.with_async(self.docker.create_exec(
            &container_id,
            CreateExecOptions {
                attach_stdout: Some(true),
                attach_stderr: Some(true),
                cmd: Some(vec![
                    "sh".to_string(),
                    "-lc".to_string(),
                    command.to_string(),
                ]),
                ..Default::default()
            },
        ))?;
        let mut output = String::new();
        let start_res = self.with_async(self.docker.start_exec(&create_res.id, None))?;
        if let StartExecResults::Attached {
            output: mut stream, ..
        } = start_res
        {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(|e| format!("tokio runtime init failed: {}", e))?
                .block_on(async {
                    while let Some(msg) = stream.try_next().await? {
                        match msg {
                            LogOutput::StdOut { message }
                            | LogOutput::StdErr { message }
                            | LogOutput::Console { message }
                            | LogOutput::StdIn { message } => {
                                output.push_str(&String::from_utf8_lossy(&message));
                            }
                        }
                    }
                    Ok::<(), BollardError>(())
                })
                .map_err(|e| format!("docker exec stream error: {}", e))?;
        }
        Ok(json!({
            "ok": true,
            "machineId": machine_id,
            "containerId": container_id,
            "output": output
        }))
    }

    fn copy_to_vm(&self, packet: &JsonValue) -> Result<JsonValue, String> {
        let machine_id = packet["machineId"].as_str().unwrap_or("");
        let identity = DockerIdentity::from_packet(packet);
        let file_name = packet["fileName"].as_str().unwrap_or("");
        let content = packet["content"].as_str().unwrap_or("");
        let target_path = packet["targetPath"].as_str().unwrap_or("/app/input");
        if machine_id.is_empty() {
            return Err("machineId is required".to_string());
        }
        if file_name.is_empty() {
            return Err("fileName is required".to_string());
        }
        let container_id = docker_container_id(
            machine_id,
            &identity.entity_id,
            &identity.container_name,
            &identity.vm_id,
        );
        let mut files = HashMap::new();
        files.insert(file_name.to_string(), content.as_bytes().to_vec());
        self.upload_files(&container_id, target_path, &files)?;
        Ok(json!({
            "ok": true,
            "machineId": machine_id,
            "containerId": container_id,
            "fileName": file_name
        }))
    }

    /// Resolve the docker container name that owns `ip` on any docker network.
    ///
    /// This is the authoritative, spoof-resistant way to identify which
    /// creature a gateway connection belongs to.
    fn find_container_name_by_ip(&self, ip: &str) -> Result<Option<String>, String> {
        if ip.is_empty() {
            return Ok(None);
        }
        let summaries = self.with_async(self.docker.list_containers(Some(
            ListContainersOptions::<String> {
                all: false,
                ..Default::default()
            },
        )))?;
        for c in summaries {
            let matched = c
                .network_settings
                .as_ref()
                .and_then(|ns| ns.networks.as_ref())
                .map(|nets| nets.values().any(|ep| ep.ip_address.as_deref() == Some(ip)))
                .unwrap_or(false);
            if matched {
                let name = c
                    .names
                    .as_ref()
                    .and_then(|n| n.first())
                    .map(|s| s.trim_start_matches('/').to_string());
                return Ok(name);
            }
        }
        Ok(None)
    }

    /// Resolve the IP address of `container_id` on the gateway bridge network
    /// (falling back to any network it is attached to). Used to proxy inbound
    /// HTTP requests to the HTTP server running inside the container.
    fn find_container_ip(&self, container_id: &str) -> Result<Option<String>, String> {
        let summaries = self.with_async(self.docker.list_containers(Some(
            ListContainersOptions::<String> {
                all: false,
                ..Default::default()
            },
        )))?;
        let gateway_net = gateway_network_name();
        for c in summaries {
            let is_match = c
                .names
                .as_ref()
                .map(|names| {
                    names
                        .iter()
                        .any(|n| n.trim_start_matches('/') == container_id)
                })
                .unwrap_or(false);
            if !is_match {
                continue;
            }
            let networks = match c
                .network_settings
                .as_ref()
                .and_then(|ns| ns.networks.as_ref())
            {
                Some(n) => n,
                None => return Ok(None),
            };
            // Prefer the shared gateway network; fall back to any attached net.
            if let Some(ep) = networks.get(&gateway_net) {
                if let Some(ip) = ep.ip_address.as_deref().filter(|s| !s.is_empty()) {
                    return Ok(Some(ip.to_string()));
                }
            }
            for ep in networks.values() {
                if let Some(ip) = ep.ip_address.as_deref().filter(|s| !s.is_empty()) {
                    return Ok(Some(ip.to_string()));
                }
            }
            return Ok(None);
        }
        Ok(None)
    }

    /// Proxy a packaged inbound HTTP request to the HTTP server running inside
    /// the target container and return its real response.
    fn forward_http(&self, packet: &JsonValue) -> Result<JsonValue, String> {
        let machine_id = packet["machineId"].as_str().unwrap_or("");
        if machine_id.is_empty() {
            return Err("machineId is required".to_string());
        }
        let identity = DockerIdentity::from_packet(packet);
        let container_id = docker_container_id(
            machine_id,
            &identity.entity_id,
            &identity.container_name,
            &identity.vm_id,
        );

        // Resume a suspended sandbox before forwarding, mirroring exec_vm's
        // wake-on-use behaviour.
        match self.container_state(&container_id)? {
            Some(s) if s == "running" => {}
            _ => {
                let _ = self.run_vm(packet);
            }
        }

        let ip = self
            .find_container_ip(&container_id)?
            .ok_or_else(|| format!("no running container found for {}", container_id))?;
        let port = vm_http_port();

        let raw_path = packet["path"].as_str().unwrap_or("/");
        let path = if raw_path.starts_with('/') {
            raw_path.to_string()
        } else {
            format!("/{}", raw_path)
        };
        let query = packet["query"].as_str().unwrap_or("");
        let url = if query.is_empty() {
            format!("http://{}:{}{}", ip, port, path)
        } else {
            format!("http://{}:{}{}?{}", ip, port, path, query)
        };

        let method_str = packet["method"].as_str().unwrap_or("GET").to_uppercase();
        let method = reqwest::Method::from_bytes(method_str.as_bytes())
            .map_err(|e| format!("invalid HTTP method '{}': {}", method_str, e))?;
        let body = packet["bodyBase64"]
            .as_str()
            .filter(|s| !s.is_empty())
            .map(|s| {
                use base64::Engine;
                base64::engine::general_purpose::STANDARD
                    .decode(s)
                    .map_err(|e| format!("invalid bodyBase64: {}", e))
            })
            .transpose()?
            .unwrap_or_default();

        let client = reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs(vm_http_timeout_secs()))
            .build()
            .map_err(|e| format!("http client init failed: {}", e))?;
        let mut builder = client.request(method, &url).body(body);
        if let Some(headers) = packet["headers"].as_object() {
            for (k, v) in headers {
                // Skip hop-by-hop headers the client will recompute.
                if k.eq_ignore_ascii_case("host")
                    || k.eq_ignore_ascii_case("content-length")
                    || k.eq_ignore_ascii_case("connection")
                {
                    continue;
                }
                if let Some(val) = v.as_str() {
                    builder = builder.header(k, val);
                }
            }
        }

        let response = builder
            .send()
            .map_err(|e| format!("forwarding to container HTTP server failed: {}", e))?;
        let status = response.status().as_u16();
        let mut resp_headers = Map::new();
        for (k, v) in response.headers().iter() {
            if let Ok(val) = v.to_str() {
                resp_headers.insert(k.as_str().to_string(), json!(val));
            }
        }
        let bytes = response
            .bytes()
            .map_err(|e| format!("failed to read container HTTP response body: {}", e))?;
        use base64::Engine;
        let body_b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);

        Ok(json!({
            "ok": true,
            "runtime": "docker",
            "machineId": machine_id,
            "vmId": identity.vm_id,
            "containerId": container_id,
            "status": status,
            "headers": JsonValue::Object(resp_headers),
            "bodyBase64": body_b64,
        }))
    }

    /// Returns the docker container state (`"running"` / `"exited"` / ...)
    /// or `None` if the container does not exist.
    fn container_state(&self, container_id: &str) -> Result<Option<String>, String> {
        let summaries = self.with_async(self.docker.list_containers(Some(
            ListContainersOptions::<String> {
                all: true,
                ..Default::default()
            },
        )))?;
        for c in summaries {
            let is_match = c
                .names
                .as_ref()
                .map(|names| {
                    names
                        .iter()
                        .any(|n| n.trim_start_matches('/') == container_id)
                })
                .unwrap_or(false);
            if is_match {
                return Ok(c.state.clone());
            }
        }
        Ok(None)
    }

    fn stop_and_remove_if_exists(&self, container_id: &str) -> Result<(), String> {
        let _ = self.with_async(
            self.docker
                .stop_container(container_id, Some(StopContainerOptions { t: 1 })),
        );
        self.with_async(self.docker.remove_container(
            container_id,
            Some(RemoveContainerOptions {
                force: true,
                v: true,
                ..Default::default()
            }),
        ))
        .or_else(|e| {
            if e.contains("No such container") {
                Ok(())
            } else {
                Err(e)
            }
        })
    }

    fn upload_files(
        &self,
        container_id: &str,
        target_path: &str,
        files: &HashMap<String, Vec<u8>>,
    ) -> Result<(), String> {
        let tar_bytes = build_tar(files)?;
        self.with_async(self.docker.upload_to_container(
            container_id,
            Some(UploadToContainerOptions {
                path: target_path,
                ..Default::default()
            }),
            tar_bytes.into(),
        ))
    }

    fn build_image(&self, packet: &JsonValue) -> Result<JsonValue, String> {
        let machine_id = packet["machineId"].as_str().unwrap_or("");
        if machine_id.is_empty() {
            return Err("machineId is required".to_string());
        }
        let entity_id = packet["entityId"]
            .as_str()
            .or_else(|| packet["imageName"].as_str())
            .unwrap_or("main")
            .to_string();
        let image_build_path = packet["imageBuildPath"]
            .as_str()
            .or_else(|| packet["dockerfilePath"].as_str())
            .or_else(|| packet["path"].as_str())
            .unwrap_or("");
        if image_build_path.is_empty() {
            return Err("build path is required".to_string());
        }

        let image_ref = packet["imageRef"]
            .as_str()
            .map(|s| s.to_string())
            .unwrap_or_else(|| docker_image_ref(machine_id, &entity_id));
        let context = build_context_from_path(image_build_path)?;
        let options = BuildImageOptions {
            dockerfile: "Dockerfile".to_string(),
            t: image_ref.clone(),
            rm: true,
            pull: false,
            ..Default::default()
        };

        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| format!("tokio runtime init failed: {}", e))?
            .block_on(async {
                let mut stream = self.docker.build_image(options, None, Some(context.into()));
                while let Some(update) = stream.try_next().await.map_err(|e| e.to_string())? {
                    if let Some(status) = update.stream.clone() {
                        emit_vm_log("main", "build", status.trim());
                    }
                    if let Some(error) = update.error {
                        emit_vm_log("main", "build", error.trim());
                        return Err(format!("docker build failed: {}", error));
                    }
                }
                Ok::<(), String>(())
            })?;

        Ok(json!({
            "ok": true,
            "machineId": machine_id,
            "imageRef": image_ref,
            "runtime": "docker"
        }))
    }
}

impl VmPlugin for DockerVmPlugin {
    fn meta(&self) -> &VmPluginMeta {
        &self.meta
    }

    fn run_vm(&self, packet: &JsonValue) -> Result<JsonValue, String> {
        self.with_controller(|c| c.run_vm(packet))
    }

    fn terminate_vm(&self, packet: &JsonValue) -> Result<JsonValue, String> {
        self.with_controller(|c| c.terminate_vm(packet))
    }

    fn delete_vm(&self, packet: &JsonValue) -> Result<JsonValue, String> {
        self.with_controller(|c| c.delete_vm(packet))
    }

    fn status_vm(&self, packet: &JsonValue) -> Result<JsonValue, String> {
        self.with_controller(|c| c.status_vm(packet))
    }

    fn exec_vm(&self, packet: &JsonValue) -> Result<JsonValue, String> {
        self.with_controller(|c| c.exec_vm(packet))
    }

    fn copy_to_vm(&self, packet: &JsonValue) -> Result<JsonValue, String> {
        self.with_controller(|c| c.copy_to_vm(packet))
    }

    fn build_image(&self, packet: &JsonValue) -> Result<JsonValue, String> {
        self.with_controller(|c| c.build_image(packet))
    }

    fn identify_instance_by_ip(&self, ip: &str) -> Option<String> {
        self.with_controller(|c| c.find_container_name_by_ip(ip))
            .ok()
            .flatten()
    }

    /// Docker VMs run a persistent HTTP server inside the container, so the
    /// packaged request is proxied straight to it (rather than the generic
    /// signal fallback the other runtimes use).
    fn forward_http(&self, packet: &JsonValue) -> Result<JsonValue, String> {
        self.with_controller(|c| c.forward_http(packet))
    }

    /// Standalone runEntity plan: docker launches need a generated container
    /// name (recorded as a state link so stop/billing can find it), the image
    /// reference derived from machine + entity, and the params delivered as
    /// container input files.
    fn plan_run_entity(&self, ctx: &JsonValue) -> Result<JsonValue, String> {
        let machine_id = ctx["machineId"].as_str().unwrap_or("");
        let program_id = ctx["programId"].as_str().unwrap_or("");
        let entity_id = ctx["entityId"].as_str().unwrap_or("");
        let vm_id = ctx["vmId"].as_str().unwrap_or("");
        let container_name = uuid::Uuid::new_v4().to_string();
        let image_ref = format!("{}/{}", machine_id.replace('@', "_"), entity_id);
        Ok(json!({
            "input": {
                "runtime": "docker",
                "machineId": machine_id,
                // Node-authoritative owner (from the runEntity ctx); run_vm records
                // it as the container's creature identity so its host calls resolve
                // to the owning creature (e.g. the backbone reads its granted keys).
                "creatureId": ctx["creatureId"],
                "entityId": entity_id,
                "containerName": container_name,
                "standalone": true,
                "vmId": vm_id,
                "resources": ctx["resources"],
                "imageRef": image_ref,
                "inputFiles": ctx["params"],
            },
            "links": [[
                format!("VmContainerName::{}::{}::{}", program_id, entity_id, vm_id),
                container_name,
            ]],
        }))
    }

    /// Standalone stopEntity plan: the terminate packet must carry the
    /// container name recorded at launch, read back from the state link.
    fn plan_stop_entity(&self, ctx: &JsonValue) -> Result<JsonValue, String> {
        let program_id = ctx["programId"].as_str().unwrap_or("");
        let entity_id = ctx["entityId"].as_str().unwrap_or("");
        let vm_id = ctx["vmId"].as_str().unwrap_or("");
        Ok(json!({
            "input": {
                "runtime": "docker",
                "machineId": ctx["machineId"],
                "entityId": entity_id,
                "vmId": vm_id,
            },
            "links": [{
                "field": "containerName",
                "key": format!("VmContainerName::{}::{}::{}", program_id, entity_id, vm_id),
                "required": true,
            }],
        }))
    }

    /// Host-call terminate requests for docker must address a concrete VM
    /// instance and carry the container identity fields.
    fn build_terminate_request(&self, input: &JsonValue) -> Result<JsonValue, String> {
        let vm_id = input["vmId"].as_str().unwrap_or("").trim().to_string();
        if vm_id.is_empty() {
            return Err("docker terminate requires a vmId".to_string());
        }
        let mut entity_id = input["entityId"].as_str().unwrap_or("").trim().to_string();
        if entity_id.is_empty() {
            entity_id = input["imageName"].as_str().unwrap_or("main").to_string();
        }
        let container_name = input["containerName"].as_str().unwrap_or("main");
        Ok(json!({
            "type": "terminateVm",
            "runtime": "docker",
            "machineId": input["machineId"].as_str().unwrap_or(""),
            "entityId": entity_id,
            "containerName": container_name,
            "vmId": vm_id,
        }))
    }

    /// Host-call delete requests carry the same container identity as a
    /// terminate, plus the flags that make the removal unconditional and the
    /// caller's choice about the entity's image.
    fn build_delete_request(&self, input: &JsonValue) -> Result<JsonValue, String> {
        let mut packet = self.build_terminate_request(input)?;
        if let Some(obj) = packet.as_object_mut() {
            obj.insert("type".to_string(), JsonValue::String("deleteVm".to_string()));
            obj.insert("purge".to_string(), JsonValue::Bool(true));
            obj.insert("delete".to_string(), JsonValue::Bool(true));
            obj.insert("removeImage".to_string(), input["removeImage"].clone());
        }
        Ok(packet)
    }
}

/// Build the environment variables that tell a docker creature how to reach
/// the docker-host bridge gateway.
///
/// Security: the container is given **no identity and no secret** — only the
/// gateway address. The node identifies which creature a connection belongs to
/// from the connection's docker-network source IP, which a container cannot
/// forge. `CASPAR_VM_ID` is exposed for log readability only.
fn gateway_container_env(vm_id: &str) -> Vec<String> {
    let host = std::env::var("DOCKER_HOST_GATEWAY_ADVERTISE_HOST")
        .unwrap_or_else(|_| "host.docker.internal".to_string());
    let port = std::env::var("DOCKER_HOST_GATEWAY_PORT").unwrap_or_else(|_| "8079".to_string());
    vec![
        format!("CASPAR_GATEWAY_HOST={}", host),
        format!("CASPAR_GATEWAY_PORT={}", port),
        format!("CASPAR_VM_ID={}", vm_id),
        // The port the container's own HTTP server should bind to so the VMM
        // HTTP ingress can proxy inbound requests to it.
        format!("CASPAR_VM_HTTP_PORT={}", vm_http_port()),
    ]
}

fn docker_container_id(
    machine_id: &str,
    entity_id: &str,
    container_name: &str,
    vm_id: &str,
) -> String {
    if !vm_id.is_empty() {
        return format!("{}_{}", machine_id.replace('@', "_"), vm_id);
    }
    format!(
        "{}_{}_{}",
        machine_id.replace('@', "_"),
        entity_id,
        container_name
    )
}

fn docker_image_ref(machine_id: &str, entity_id: &str) -> String {
    format!("{}/{}", machine_id.replace('@', "_"), entity_id)
}

fn build_tar(files: &HashMap<String, Vec<u8>>) -> Result<Vec<u8>, String> {
    let mut buf = Vec::<u8>::new();
    {
        let mut tar = TarBuilder::new(&mut buf);
        for (name, content) in files {
            let mut header = tar::Header::new_gnu();
            header.set_path(name).map_err(|e| e.to_string())?;
            header.set_size(content.len() as u64);
            header.set_mode(0o644);
            header.set_cksum();
            tar.append(&header, Cursor::new(content.as_slice()))
                .map_err(|e| e.to_string())?;
        }
        tar.finish().map_err(|e| e.to_string())?;
    }
    Ok(buf)
}

fn parse_input_files(input_files: &JsonValue) -> Result<HashMap<String, Vec<u8>>, String> {
    if let Some(serialized) = input_files.as_str() {
        let parsed = serde_json::from_str::<JsonValue>(serialized)
            .map_err(|e| format!("inputFiles must be valid JSON: {}", e))?;
        return parse_input_files(&parsed);
    }
    let mut files = HashMap::new();
    let obj: &Map<String, JsonValue> = input_files
        .as_object()
        .ok_or_else(|| "inputFiles must be an object or JSON-encoded object".to_string())?;
    for (key, value) in obj {
        let raw = value.as_str().unwrap_or("");
        files.insert(key.to_string(), raw.as_bytes().to_vec());
    }
    Ok(files)
}

// Background log-writer: build/run log capture happens *inside* tokio
// `block_on` contexts, but the storage log writer drives a synchronous
// Postgres (QuestDB) client whose connection is torn down on a background
// tokio runtime — dropping it from within another runtime aborts the process.
// We therefore funnel every captured line to a single dedicated OS thread
// that owns no tokio context and performs the blocking write safely.
fn vm_log_sender() -> std::sync::mpsc::Sender<(String, String, String)> {
    static LOG_TX: std::sync::OnceLock<
        std::sync::Mutex<std::sync::mpsc::Sender<(String, String, String)>>,
    > = std::sync::OnceLock::new();
    LOG_TX
        .get_or_init(|| {
            let (tx, rx) = std::sync::mpsc::channel::<(String, String, String)>();
            thread::spawn(move || {
                for (vm_id, log_type, text) in rx {
                    let ts = chrono::Utc::now().timestamp_millis();
                    if let Some(h) = host() {
                        h.storage_log_vm(&vm_id, &log_type, &text, ts);
                    }
                }
            });
            std::sync::Mutex::new(tx)
        })
        .lock()
        .unwrap()
        .clone()
}

fn emit_vm_log(vm_id: &str, log_type: &str, text: &str) {
    if vm_id.trim().is_empty() || text.trim().is_empty() {
        return;
    }
    let _ = vm_log_sender().send((vm_id.to_string(), log_type.to_string(), text.to_string()));
}

fn build_context_from_path(path: &str) -> Result<Vec<u8>, String> {
    let path_ref = Path::new(path);
    if !path_ref.exists() {
        return Err(format!("dockerfile path does not exist: {}", path));
    }
    let mut buf = Vec::<u8>::new();
    {
        let mut tar = TarBuilder::new(&mut buf);
        if path_ref.is_dir() {
            tar.append_dir_all(".", path_ref)
                .map_err(|e| format!("failed to archive docker context directory: {}", e))?;
        } else {
            let parent = path_ref.parent().unwrap_or_else(|| Path::new("."));
            tar.append_dir_all(".", parent)
                .map_err(|e| format!("failed to archive docker context parent directory: {}", e))?;
        }
        tar.finish().map_err(|e| e.to_string())?;
    }
    Ok(buf)
}

// ── Persistent, non-escapable per-VM storage for docker sandboxes ────────────
//
// Mirrors the fire runtime layout: `{STORAGE_ROOT_PATH}/vms/<vm>`.

fn docker_storage_root() -> PathBuf {
    if let Ok(p) = std::env::var("STORAGE_ROOT_PATH") {
        let trimmed = p.trim();
        if !trimmed.is_empty() {
            return PathBuf::from(trimmed);
        }
    }
    let in_container = PathBuf::from("/app/data/storage");
    if in_container.exists() {
        return in_container;
    }
    PathBuf::from("/tmp/caspar/storage")
}

fn docker_vms_root() -> PathBuf {
    docker_storage_root().join("vms")
}

fn docker_sanitize_component(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    for ch in raw.chars() {
        if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
            out.push(ch);
        } else {
            out.push('_');
        }
    }
    if out.is_empty() {
        "default".to_string()
    } else {
        out
    }
}

fn docker_vm_dir_path(vm_id: &str) -> PathBuf {
    let vm = docker_sanitize_component(if vm_id.is_empty() { "main" } else { vm_id });
    docker_vms_root().join(vm)
}

fn docker_session_vm_dir(vm_id: &str) -> Result<PathBuf, String> {
    let root = docker_vms_root();
    std::fs::create_dir_all(&root)
        .map_err(|e| format!("failed to prepare vms root {}: {}", root.display(), e))?;
    let canon_root = std::fs::canonicalize(&root)
        .map_err(|e| format!("failed to canonicalize vms root: {}", e))?;
    let dir = docker_vm_dir_path(vm_id);
    std::fs::create_dir_all(&dir)
        .map_err(|e| format!("failed to create session vm dir {}: {}", dir.display(), e))?;
    let canon_dir = std::fs::canonicalize(&dir)
        .map_err(|e| format!("failed to canonicalize session vm dir: {}", e))?;
    if !canon_dir.starts_with(&canon_root) || canon_dir == canon_root {
        return Err(format!(
            "refusing to use vm dir outside sandbox root: {}",
            canon_dir.display()
        ));
    }
    Ok(canon_dir)
}

fn docker_is_within_vms_root(dir: &Path) -> bool {
    let root = docker_vms_root();
    let canon_root = std::fs::canonicalize(&root).unwrap_or(root);
    match std::fs::canonicalize(dir) {
        Ok(c) => c.starts_with(&canon_root) && c != canon_root,
        Err(_) => dir.starts_with(&canon_root) && dir != canon_root,
    }
}
