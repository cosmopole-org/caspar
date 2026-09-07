//! The fire (Firecracker) VM controller — full lifecycle management for
//! Firecracker microVM sandboxes, exposed to the Caspar VMM through the
//! `caspar_vm_sdk::VmPlugin` interface.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use once_cell_lite::Lazy;
use serde_json::{json, Value as JsonValue};

use caspar_vm_sdk::host::{host, log};
use caspar_vm_sdk::{parse_vm_resource_limits, VmPlugin, VmPluginMeta, VmResourceLimits};

use crate::models::FireVmProcess;

// Minimal Lazy shim so the plugin has no once_cell dependency.
mod once_cell_lite {
    use std::sync::OnceLock;

    pub struct Lazy<T> {
        cell: OnceLock<T>,
        init: fn() -> T,
    }

    impl<T> Lazy<T> {
        pub const fn new(init: fn() -> T) -> Self {
            Lazy {
                cell: OnceLock::new(),
                init,
            }
        }
    }

    impl<T> std::ops::Deref for Lazy<T> {
        type Target = T;
        fn deref(&self) -> &T {
            self.cell.get_or_init(self.init)
        }
    }
}

pub(crate) static GLOBAL_FIRE_VMS: Lazy<Arc<Mutex<HashMap<String, FireVmProcess>>>> =
    Lazy::new(|| Arc::new(Mutex::new(HashMap::new())));

/// Emit a packet through the VMM's unified dispatcher (the historical
/// `wasm_send` channel).
fn dispatch(packet: &JsonValue) -> String {
    match host() {
        Some(h) => h.dispatch(packet),
        None => json!({"ok": false, "error": "caspar vm host is not initialised"}).to_string(),
    }
}

pub struct FireVmController {
    meta: VmPluginMeta,
}

impl FireVmController {
    pub fn new(meta: VmPluginMeta) -> Self {
        Self { meta }
    }

    fn ensure_vm_root(&self) -> Result<(), String> {
        std::fs::create_dir_all("/opt/firecracker/vms")
            .map_err(|e| format!("failed to prepare firecracker vm dir: {}", e))
    }

    fn run_vm_inner(&self, packet: &JsonValue) -> Result<JsonValue, String> {
        self.ensure_vm_root()?;
        let machine_id = packet["machineId"].as_str().unwrap_or("").trim();
        if machine_id.is_empty() {
            return Err("machineId is required".to_string());
        }
        let vm_id = packet["vmId"].as_str().unwrap_or("main").trim();
        let vm_cache_key = if vm_id.is_empty() { "main" } else { vm_id };
        let requester_user_id = packet["requesterUserId"]
            .as_str()
            .unwrap_or("")
            .trim()
            .to_string();
        let requester_user_id_for_cache = requester_user_id.clone();
        let stream_store_id = packet["storeId"].as_str().unwrap_or("").trim().to_string();
        let process_key = fire_process_key(machine_id, vm_id);
        let socket_path = fire_socket_path(machine_id, vm_id);
        let limits = parse_vm_resource_limits(packet);

        // Session sandboxes are persistent by default: their disk lives under
        // `{storage}/vms` and is kept across suspend/resume cycles. A caller can
        // opt out with `persistent: false` for an ephemeral one-shot VM.
        let persistent = packet["persistent"].as_bool().unwrap_or(true);
        let force_restart = packet["forceRestart"].as_bool().unwrap_or(false);

        // Keep-alive: if this session's VM is already up, leave it running.
        if !force_restart {
            let already_running = {
                let mut fire_vms = GLOBAL_FIRE_VMS.lock().unwrap();
                match fire_vms.get_mut(&process_key) {
                    Some(proc) => matches!(proc.child.try_wait(), Ok(None)),
                    None => false,
                }
            };
            if already_running {
                return Ok(json!({
                    "ok": true,
                    "runtime": "fire",
                    "machineId": machine_id,
                    "vmId": vm_id,
                    "processKey": process_key,
                    "status": "already-running",
                    "persistent": persistent,
                }));
            }
        }

        // Resolve (and create) the non-escapable per-session sandbox directory
        // before doing anything else, so a bad store/vm id is rejected early.
        let vm_dir = if persistent {
            Some(session_vm_dir(vm_id)?)
        } else {
            None
        };

        self.terminate_by_key(&process_key);
        let _ = std::fs::remove_file(&socket_path);

        let firecracker_bin = std::env::var("FIRECRACKER_BIN")
            .unwrap_or_else(|_| "/usr/local/bin/firecracker".to_string());
        let child = Command::new(&firecracker_bin)
            .arg("--api-sock")
            .arg(&socket_path)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| format!("failed to start firecracker process: {}", e))?;
        configure_firecracker_machine_limits(&socket_path, &limits)?;

        // Provision persistent storage and, when boot images are configured,
        // boot a real guest whose only visible block devices are this session's
        // own disks — the host filesystem is unreachable from inside the VM.
        if let Some(ref dir) = vm_dir {
            if let Err(err) = provision_and_boot_guest(&socket_path, dir, &limits) {
                log(format!(
                    "fire vm {}: persistent guest boot unavailable, running in scaffold mode: {}",
                    process_key, err
                ));
            }
        }

        let mut child = child;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| "failed to acquire firecracker stdin".to_string())?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| "failed to acquire firecracker stdout".to_string())?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| "failed to acquire firecracker stderr".to_string())?;

        let output = Arc::new(Mutex::new(String::new()));
        let io_stop = Arc::new(AtomicBool::new(false));

        let output_stdout = Arc::clone(&output);
        let io_stop_stdout = Arc::clone(&io_stop);
        let machine_id_stdout = machine_id.to_string();
        let vm_id_stdout = vm_id.to_string();
        let requester_stdout = requester_user_id.clone();
        let store_id_stdout = stream_store_id.clone();
        let stdout_thread = thread::spawn(move || {
            let mut reader = std::io::BufReader::new(stdout);
            let mut line = String::new();
            while !io_stop_stdout.load(Ordering::Relaxed) {
                line.clear();
                match std::io::BufRead::read_line(&mut reader, &mut line) {
                    Ok(0) => break,
                    Ok(_) => {
                        if let Ok(mut out) = output_stdout.lock() {
                            out.push_str(&line);
                        }
                        emit_fire_output_signal(
                            &machine_id_stdout,
                            &vm_id_stdout,
                            &requester_stdout,
                            &store_id_stdout,
                            line.trim_end(),
                        );
                    }
                    Err(_) => break,
                }
            }
        });

        let output_stderr = Arc::clone(&output);
        let io_stop_stderr = Arc::clone(&io_stop);
        let machine_id_stderr = machine_id.to_string();
        let vm_id_stderr = vm_id.to_string();
        let requester_stderr = requester_user_id.clone();
        let store_id_stderr = stream_store_id.clone();
        let stderr_thread = thread::spawn(move || {
            let mut reader = std::io::BufReader::new(stderr);
            let mut line = String::new();
            while !io_stop_stderr.load(Ordering::Relaxed) {
                line.clear();
                match std::io::BufRead::read_line(&mut reader, &mut line) {
                    Ok(0) => break,
                    Ok(_) => {
                        if let Ok(mut out) = output_stderr.lock() {
                            out.push_str(&line);
                        }
                        emit_fire_output_signal(
                            &machine_id_stderr,
                            &vm_id_stderr,
                            &requester_stderr,
                            &store_id_stderr,
                            line.trim_end(),
                        );
                    }
                    Err(_) => break,
                }
            }
        });

        let vm_dir_for_response = vm_dir
            .as_ref()
            .map(|d| d.display().to_string())
            .unwrap_or_default();
        let mut fire_vms = GLOBAL_FIRE_VMS.lock().unwrap();
        fire_vms.insert(
            process_key.clone(),
            FireVmProcess {
                machine_id: machine_id.to_string(),
                vm_id: vm_id.to_string(),
                requester_user_id,
                stream_store_id,
                socket_path,
                child,
                stdin: Arc::new(Mutex::new(stdin)),
                output,
                io_stop,
                stdout_thread: Some(stdout_thread),
                stderr_thread: Some(stderr_thread),
                vm_dir,
                persistent,
            },
        );
        if let Some(h) = host() {
            h.register_vm_context(vm_cache_key, &requester_user_id_for_cache, machine_id);
        }
        let timeout_key = process_key.clone();
        let timeout_secs = limits.max_exec_time_secs;
        thread::spawn(move || {
            thread::sleep(Duration::from_secs(timeout_secs));
            let mut fire_vms = GLOBAL_FIRE_VMS.lock().unwrap();
            if let Some(mut proc) = fire_vms.remove(&timeout_key) {
                proc.io_stop.store(true, Ordering::Relaxed);
                let _ = proc.child.kill();
                let _ = proc.child.wait();
                if let Some(handle) = proc.stdout_thread.take() {
                    let _ = handle.join();
                }
                if let Some(handle) = proc.stderr_thread.take() {
                    let _ = handle.join();
                }
                let _ = std::fs::remove_file(proc.socket_path);
                if let Some(h) = host() {
                    h.unregister_vm_context(proc.vm_id.as_str());
                }
                let _ = dispatch(&json!({
                    "key": "vmLog",
                    "input": {
                        "vmId": proc.vm_id,
                        "logType": "runtime",
                        "text": format!("fire vm terminated due to max execution time ({}s)", timeout_secs),
                    }
                }));
            }
        });

        Ok(json!({
            "ok": true,
            "runtime": "fire",
            "machineId": machine_id,
            "vmId": vm_id,
            "processKey": process_key,
            "status": "running",
            "persistent": persistent,
            "vmDir": vm_dir_for_response,
            "resources": {
                "maxExecTimeSeconds": limits.max_exec_time_secs,
                "ramMb": limits.ram_mb,
                "diskGb": limits.disk_gb,
                "cpuCores": limits.cpu_cores
            }
        }))
    }

    /// Turn a fire VM off. By default this is a *suspend*; `purge: true`
    /// deletes the persistent sandbox directory as well.
    fn terminate_vm_inner(&self, packet: &JsonValue) -> Result<JsonValue, String> {
        let machine_id = packet["machineId"].as_str().unwrap_or("").trim();
        if machine_id.is_empty() {
            return Err("machineId is required".to_string());
        }
        let vm_id = packet["vmId"].as_str().unwrap_or("main").trim();
        let purge = packet["purge"].as_bool().unwrap_or(false);
        let vm_cache_key = if vm_id.is_empty() { "main" } else { vm_id };
        let process_key = fire_process_key(machine_id, vm_id);

        // Capture the live persistent dir (if the VM is currently running) so we
        // can purge it precisely; fall back to recomputing the deterministic
        // path so a purge still works on an already-suspended sandbox.
        let captured_dir = {
            let fire_vms = GLOBAL_FIRE_VMS.lock().unwrap();
            fire_vms.get(&process_key).and_then(|p| p.vm_dir.clone())
        };

        self.terminate_by_key(&process_key);
        if let Some(h) = host() {
            h.unregister_vm_context(vm_cache_key);
        }

        let mut purged = false;
        if purge {
            let target = captured_dir.or_else(|| vm_dir_path(vm_id));
            if let Some(dir) = target {
                if dir.exists() && is_within_vms_root(&dir) {
                    match std::fs::remove_dir_all(&dir) {
                        Ok(()) => purged = true,
                        Err(e) => {
                            return Err(format!(
                                "failed to purge sandbox dir {}: {}",
                                dir.display(),
                                e
                            ))
                        }
                    }
                }
            }
        }

        Ok(json!({
            "ok": true,
            "runtime": "fire",
            "machineId": machine_id,
            "vmId": vm_id,
            "processKey": process_key,
            "status": if purge { "deleted" } else { "suspended" },
            "purged": purged,
        }))
    }

    /// Permanently destroy a firecracker sandbox: the microVM process is
    /// killed, its API socket removed, and its persistent dir dropped
    /// unconditionally — a delete is never a suspend, so unlike terminate it
    /// does not wait for `purge` to be asked for.
    fn delete_vm_inner(&self, packet: &JsonValue) -> Result<JsonValue, String> {
        let machine_id = packet["machineId"].as_str().unwrap_or("").trim();
        if machine_id.is_empty() {
            return Err("machineId is required".to_string());
        }
        let vm_id = packet["vmId"].as_str().unwrap_or("main").trim();
        let mut purge_packet = packet.clone();
        if let Some(obj) = purge_packet.as_object_mut() {
            obj.insert("purge".to_string(), JsonValue::Bool(true));
        }
        let terminated = self.terminate_vm_inner(&purge_packet)?;
        // terminate_by_key removes the socket for a *running* VM; an already
        // suspended one leaves the file behind, so clear it here too.
        let _ = std::fs::remove_file(fire_socket_path(machine_id, vm_id));
        Ok(json!({
            "ok": true,
            "runtime": "fire",
            "machineId": machine_id,
            "vmId": vm_id,
            "deleted": true,
            "terminate": terminated,
        }))
    }

    fn exec_vm_inner(&self, packet: &JsonValue) -> Result<JsonValue, String> {
        let machine_id = packet["machineId"].as_str().unwrap_or("").trim();
        if machine_id.is_empty() {
            return Err("machineId is required".to_string());
        }
        let vm_id = packet["vmId"].as_str().unwrap_or("main").trim();
        let command = packet["command"].as_str().unwrap_or("").trim();
        if command.is_empty() {
            return Err("command is required".to_string());
        }

        let process_key = fire_process_key(machine_id, vm_id);

        // Wake-on-exec: if this session's sandbox is suspended, bring it back
        // up on its persistent disk before running the command.
        let is_running = {
            let mut fire_vms = GLOBAL_FIRE_VMS.lock().unwrap();
            match fire_vms.get_mut(&process_key) {
                Some(proc) => matches!(proc.child.try_wait(), Ok(None)),
                None => false,
            }
        };
        if !is_running {
            self.run_vm_inner(packet)?;
        }

        let (stdin_arc, output_arc) = {
            let fire_vms = GLOBAL_FIRE_VMS.lock().unwrap();
            let proc = fire_vms
                .get(&process_key)
                .ok_or_else(|| format!("fire vm is not running: {}", process_key))?;
            (Arc::clone(&proc.stdin), Arc::clone(&proc.output))
        };

        {
            let mut stdin = stdin_arc
                .lock()
                .map_err(|_| "failed to lock fire vm stdin".to_string())?;
            std::io::Write::write_all(&mut *stdin, command.as_bytes())
                .map_err(|e| format!("failed to write command to fire vm: {}", e))?;
            std::io::Write::write_all(&mut *stdin, b"\n")
                .map_err(|e| format!("failed to write command newline to fire vm: {}", e))?;
            std::io::Write::flush(&mut *stdin)
                .map_err(|e| format!("failed to flush fire vm stdin: {}", e))?;
        }

        thread::sleep(Duration::from_millis(100));
        let output = output_arc
            .lock()
            .map_err(|_| "failed to lock fire vm output buffer".to_string())?
            .clone();

        Ok(json!({
            "ok": true,
            "runtime": "fire",
            "machineId": machine_id,
            "vmId": vm_id,
            "output": output,
        }))
    }

    fn copy_to_vm_inner(&self, packet: &JsonValue) -> Result<JsonValue, String> {
        let machine_id = packet["machineId"].as_str().unwrap_or("").trim();
        if machine_id.is_empty() {
            return Err("machineId is required".to_string());
        }
        let file_name = packet["fileName"].as_str().unwrap_or("").trim();
        if file_name.is_empty() {
            return Err("fileName is required".to_string());
        }
        let target_path = packet["targetPath"].as_str().unwrap_or("/tmp").trim();
        let content = packet["content"].as_str().unwrap_or("");
        let escaped_content = content.replace('\\', "\\\\").replace('\'', "'\"'\"'");
        let copy_cmd = format!(
            "mkdir -p '{}' && printf '%s' '{}' > '{}/{}'",
            target_path, escaped_content, target_path, file_name
        );
        let mut copy_packet = packet.clone();
        copy_packet["command"] = JsonValue::String(copy_cmd);
        copy_packet["vmId"] =
            JsonValue::String(packet["vmId"].as_str().unwrap_or("main").to_string());
        self.exec_vm_inner(&copy_packet)?;
        Ok(json!({
            "ok": true,
            "runtime": "fire",
            "machineId": machine_id,
            "fileName": file_name,
        }))
    }

    fn build_image_inner(&self, packet: &JsonValue) -> Result<JsonValue, String> {
        let machine_id = packet["machineId"].as_str().unwrap_or("").trim();
        if machine_id.is_empty() {
            return Err("machineId is required".to_string());
        }
        let vm_id = packet["vmId"].as_str().unwrap_or("main");
        let _ = dispatch(&json!({
            "key": "vmLog",
            "input": {
                "vmId": vm_id,
                "logType": "build",
                "text": "fire vm build image request accepted",
            }
        }));
        Ok(json!({
            "ok": true,
            "runtime": "fire",
            "machineId": machine_id,
        }))
    }

    fn terminate_by_key(&self, process_key: &str) {
        let mut fire_vms = GLOBAL_FIRE_VMS.lock().unwrap();
        if let Some(mut proc) = fire_vms.remove(process_key) {
            proc.io_stop.store(true, Ordering::Relaxed);
            let _ = proc.child.kill();
            let _ = proc.child.wait();
            if let Some(handle) = proc.stdout_thread.take() {
                let _ = handle.join();
            }
            if let Some(handle) = proc.stderr_thread.take() {
                let _ = handle.join();
            }
            let _ = std::fs::remove_file(proc.socket_path);
            let vm_id = proc.vm_id;
            if let Some(h) = host() {
                h.unregister_vm_context(vm_id.as_str());
            }
        }
    }
}

impl VmPlugin for FireVmController {
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

    fn exec_vm(&self, packet: &JsonValue) -> Result<JsonValue, String> {
        self.exec_vm_inner(packet)
    }

    fn copy_to_vm(&self, packet: &JsonValue) -> Result<JsonValue, String> {
        self.copy_to_vm_inner(packet)
    }

    fn build_image(&self, packet: &JsonValue) -> Result<JsonValue, String> {
        self.build_image_inner(packet)
    }
}

fn configure_firecracker_machine_limits(
    socket_path: &PathBuf,
    limits: &VmResourceLimits,
) -> Result<(), String> {
    for _ in 0..50 {
        if socket_path.exists() {
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }
    if !socket_path.exists() {
        return Err(format!(
            "firecracker socket did not become available: {}",
            socket_path.display()
        ));
    }

    let mut stream = std::os::unix::net::UnixStream::connect(socket_path)
        .map_err(|e| format!("failed to connect firecracker socket: {}", e))?;
    let body = json!({
        "vcpu_count": limits.cpu_cores,
        "mem_size_mib": limits.ram_mb,
        "track_dirty_pages": false
    })
    .to_string();
    let req = format!(
        "PUT /machine-config HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
        body.len(),
        body
    );
    stream
        .write_all(req.as_bytes())
        .map_err(|e| format!("failed to write firecracker machine-config: {}", e))?;
    stream
        .flush()
        .map_err(|e| format!("failed to flush firecracker machine-config: {}", e))?;

    let mut response = String::new();
    let _ = stream.read_to_string(&mut response);
    if response.starts_with("HTTP/1.1 204") || response.starts_with("HTTP/1.1 200") {
        Ok(())
    } else {
        Err(format!(
            "firecracker machine-config rejected response={}",
            response.lines().next().unwrap_or("")
        ))
    }
}

fn fire_process_key(machine_id: &str, vm_id: &str) -> String {
    format!("{}_{}", machine_id.replace('@', "_"), vm_id)
}

fn fire_socket_path(machine_id: &str, vm_id: &str) -> PathBuf {
    PathBuf::from(format!(
        "/opt/firecracker/vms/fc.{}.{}.sock",
        machine_id.replace('@', "_"),
        vm_id
    ))
}

fn emit_fire_output_signal(
    creature_id: &str,
    vm_id: &str,
    requester_user_id: &str,
    store_id: &str,
    output_line: &str,
) {
    if requester_user_id.is_empty() || store_id.is_empty() || output_line.is_empty() {
        return;
    }
    let payload = json!({
        "event": "fireVmOutput",
        "creatureId": creature_id,
        "vmId": vm_id,
        "requesterUserId": requester_user_id,
        "output": output_line,
    })
    .to_string();
    let signal_packet = json!({
        "key": "signal",
        "input": {
            "machineId": creature_id,
            "creatureId": creature_id,
            "storeId": store_id,
            "userId": requester_user_id,
            "type": "fire.vm.output",
            "temp": true,
            "data": payload,
        }
    });
    let _ = dispatch(&signal_packet);
    let vm_log_packet = json!({
        "key": "vmLog",
        "input": {
            "vmId": vm_id,
            "logType": "runtime",
            "text": output_line,
        }
    });
    let _ = dispatch(&vm_log_packet);
}

// ── Persistent, non-escapable session storage ────────────────────────────────

/// Root of Caspar's data storage folder, as configured for the node
/// (`STORAGE_ROOT_PATH`). Falls back to the in-container default and finally to
/// a local dev path.
fn fire_storage_root() -> PathBuf {
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

/// The `vms` directory inside the storage folder that holds every session's
/// persistent sandbox.
fn fire_vms_root() -> PathBuf {
    fire_storage_root().join("vms")
}

/// Reduce a caller-supplied id to a single safe path component.
fn sanitize_component(raw: &str) -> String {
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

/// Deterministic on-disk path for a sandbox (no side effects).
fn vm_dir_path(vm_id: &str) -> Option<PathBuf> {
    let vm = sanitize_component(if vm_id.is_empty() { "main" } else { vm_id });
    Some(fire_vms_root().join(vm))
}

/// Resolve (and create) the per-VM sandbox directory, guaranteeing — by
/// canonicalising the result and asserting containment — that it can never
/// resolve outside the vms root.
fn session_vm_dir(vm_id: &str) -> Result<PathBuf, String> {
    let root = fire_vms_root();
    std::fs::create_dir_all(&root)
        .map_err(|e| format!("failed to prepare vms root {}: {}", root.display(), e))?;
    let canon_root = std::fs::canonicalize(&root)
        .map_err(|e| format!("failed to canonicalize vms root: {}", e))?;
    let dir = vm_dir_path(vm_id).ok_or_else(|| "failed to resolve sandbox dir".to_string())?;
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

/// Guard used before a destructive purge: the directory must sit strictly
/// inside the vms root.
fn is_within_vms_root(dir: &Path) -> bool {
    let root = fire_vms_root();
    let canon_root = std::fs::canonicalize(&root).unwrap_or(root);
    match std::fs::canonicalize(dir) {
        Ok(c) => c.starts_with(&canon_root) && c != canon_root,
        Err(_) => dir.starts_with(&canon_root) && dir != canon_root,
    }
}

/// Create a sparse, ext4-formatted backing disk if it does not already exist.
fn ensure_persistent_disk(path: &Path, disk_gb: u64) -> Result<(), String> {
    if path.exists() {
        return Ok(());
    }
    let bytes = disk_gb
        .max(1)
        .saturating_mul(1024)
        .saturating_mul(1024)
        .saturating_mul(1024);
    let file = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .open(path)
        .map_err(|e| format!("failed to create disk image {}: {}", path.display(), e))?;
    file.set_len(bytes)
        .map_err(|e| format!("failed to size disk image {}: {}", path.display(), e))?;
    drop(file);
    // Best-effort filesystem so the guest can mount the data disk. If mkfs is
    // unavailable the raw backing file is still provisioned and persisted.
    let _ = Command::new("mkfs.ext4").arg("-F").arg("-q").arg(path).status();
    Ok(())
}

/// Configured base boot images, if both a kernel and a base rootfs exist on
/// disk. When absent, the controller runs in scaffold mode.
fn fire_boot_images() -> Option<(PathBuf, PathBuf)> {
    let kernel = PathBuf::from(std::env::var("FIRECRACKER_KERNEL_IMAGE").ok()?);
    let rootfs = PathBuf::from(std::env::var("FIRECRACKER_ROOTFS_IMAGE").ok()?);
    if kernel.exists() && rootfs.exists() {
        Some((kernel, rootfs))
    } else {
        None
    }
}

/// Minimal HTTP/1.1-over-unix-socket call to the Firecracker API.
fn fc_api(socket_path: &Path, method: &str, url_path: &str, body: &str) -> Result<(), String> {
    let mut stream = std::os::unix::net::UnixStream::connect(socket_path)
        .map_err(|e| format!("failed to connect firecracker socket: {}", e))?;
    let req = format!(
        "{} {} HTTP/1.1\r\nHost: localhost\r\nAccept: application/json\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
        method,
        url_path,
        body.len(),
        body
    );
    stream
        .write_all(req.as_bytes())
        .map_err(|e| format!("firecracker {} {} write failed: {}", method, url_path, e))?;
    stream
        .flush()
        .map_err(|e| format!("firecracker {} {} flush failed: {}", method, url_path, e))?;
    let mut response = String::new();
    let _ = stream.read_to_string(&mut response);
    let status = response.lines().next().unwrap_or("");
    if status.contains(" 204") || status.contains(" 200") {
        Ok(())
    } else {
        Err(format!(
            "firecracker {} {} rejected: {}",
            method, url_path, status
        ))
    }
}

/// Provision the session's persistent disks and, when boot images are
/// configured, boot a real Firecracker guest whose only block devices are this
/// session's own rootfs + data disk.
fn provision_and_boot_guest(
    socket_path: &Path,
    vm_dir: &Path,
    limits: &VmResourceLimits,
) -> Result<(), String> {
    // Always provision the persistent data disk so the session's mounted path
    // exists and survives suspend/resume — even in scaffold mode.
    let data_disk = vm_dir.join("data.ext4");
    ensure_persistent_disk(&data_disk, limits.disk_gb)?;

    let (kernel, base_rootfs) = match fire_boot_images() {
        Some(pair) => pair,
        None => {
            return Err(
                "FIRECRACKER_KERNEL_IMAGE / FIRECRACKER_ROOTFS_IMAGE not configured".to_string(),
            )
        }
    };

    // Per-session writable rootfs: copy the base image once so software the
    // session installs persists across suspend/resume.
    let session_rootfs = vm_dir.join("rootfs.ext4");
    if !session_rootfs.exists() {
        std::fs::copy(&base_rootfs, &session_rootfs)
            .map_err(|e| format!("failed to materialise session rootfs: {}", e))?;
    }

    let boot_args = std::env::var("FIRECRACKER_BOOT_ARGS")
        .unwrap_or_else(|_| "console=ttyS0 reboot=k panic=1 pci=off root=/dev/vda rw".to_string());
    let boot_body = json!({
        "kernel_image_path": kernel.display().to_string(),
        "boot_args": boot_args,
    })
    .to_string();
    fc_api(socket_path, "PUT", "/boot-source", &boot_body)?;

    let rootfs_body = json!({
        "drive_id": "rootfs",
        "path_on_host": session_rootfs.display().to_string(),
        "is_root_device": true,
        "is_read_only": false,
    })
    .to_string();
    fc_api(socket_path, "PUT", "/drives/rootfs", &rootfs_body)?;

    let data_body = json!({
        "drive_id": "data",
        "path_on_host": data_disk.display().to_string(),
        "is_root_device": false,
        "is_read_only": false,
    })
    .to_string();
    fc_api(socket_path, "PUT", "/drives/data", &data_body)?;

    fc_api(
        socket_path,
        "PUT",
        "/actions",
        "{\"action_type\":\"InstanceStart\"}",
    )?;
    Ok(())
}
