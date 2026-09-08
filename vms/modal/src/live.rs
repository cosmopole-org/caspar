//! End-to-end tests against the real Modal API.
//!
//! Everything else in this crate is unit-testable without a network, which is
//! exactly why the failures that took the platform's sandboxes down were not
//! caught: a Tokio runtime that was never entered, and a client version header
//! Modal parses and refuses. Neither is visible without talking to Modal.
//!
//! These drive the plugin through its real `VmPlugin` surface — the same
//! `runVm` / `execVm` / `deleteVm` packets `spaces/*` sends — against a
//! stand-in host that keeps node state in memory. So what is exercised is the
//! production path, not a reimplementation of it.
//!
//! They are `#[ignore]`d: they cost real Modal resources and need credentials.
//!
//!     MODAL_TOKEN_ID=… MODAL_TOKEN_SECRET=… \
//!       cargo test -p caspar-vm-modal --ignored -- --nocapture --test-threads=1
//!
//! Each test cleans up the sandbox and volume it created, including on the
//! failure paths, so a failed run does not leave a machine billing.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use serde_json::{json, Value};

use caspar_vm_sdk::host::{set_host, KvOp, VmHost};
use caspar_vm_sdk::{VmPlugin, VmPluginMeta};

use crate::controller::ModalVmPlugin;

/// The node's state layer, in a HashMap.
///
/// The plugin keeps its sandbox/volume/app/image links here — that is how a
/// sandbox is found again by a later `execVm`, and it is the piece a naive
/// "just call the API" test would skip. Everything else the trait requires is
/// stubbed to the inert answer, because the modal plugin does not use it.
#[derive(Default)]
struct MemoryHost {
    links: Mutex<HashMap<String, String>>,
    contexts: Mutex<HashMap<String, (String, String)>>,
}

impl VmHost for MemoryHost {
    fn dispatch(&self, _packet: &Value) -> String {
        "{}".to_string()
    }
    fn unified_host_call(&self, _packet: &Value) -> String {
        "{}".to_string()
    }
    fn storage_log_vm(&self, _vm_id: &str, _log_type: &str, _text: &str, _timestamp_ms: i64) {}
    fn register_vm_context(&self, vm_id: &str, creature_id: &str, machine_id: &str) {
        self.contexts
            .lock()
            .unwrap()
            .insert(vm_id.to_string(), (creature_id.to_string(), machine_id.to_string()));
    }
    fn unregister_vm_context(&self, vm_id: &str) {
        self.contexts.lock().unwrap().remove(vm_id);
    }
    fn get_vm_context(&self, vm_id: &str) -> Option<(String, String)> {
        self.contexts.lock().unwrap().get(vm_id).cloned()
    }
    fn register_vm_container(
        &self,
        _container_name: &str,
        _vm_id: &str,
        _creature_id: &str,
        _program_id: &str,
        _machine_id: &str,
        _entity_id: &str,
    ) {
    }
    fn unregister_vm_container(&self, _container_name: &str) {}
    fn begin_vm_buffer(&self, _vm_id: &str) {}
    fn commit_vm_buffer(&self, _vm_id: &str) {}
    fn acquire_resource_lock(&self, _resource_id: &str, _owner_id: &str) -> Result<(), String> {
        Ok(())
    }
    fn release_resource_lock(&self, _resource_id: &str, _owner_id: &str) -> Result<(), String> {
        Ok(())
    }
    fn state_get(&self, key: &str) -> String {
        self.links.lock().unwrap().get(key).cloned().unwrap_or_default()
    }
    fn state_get_by_prefix(&self, prefix: &str) -> Vec<String> {
        self.links
            .lock()
            .unwrap()
            .iter()
            .filter(|(k, _)| k.starts_with(prefix))
            .map(|(_, v)| v.clone())
            .collect()
    }
    fn state_apply_ops(&self, ops: &[KvOp]) -> Result<(), String> {
        let mut links = self.links.lock().unwrap();
        for op in ops {
            if op.op == "del" {
                links.remove(&op.key);
            } else {
                links.insert(op.key.clone(), op.val.clone());
            }
        }
        Ok(())
    }
    fn vm_json_trx_op(&self, _vm_id: &str, _op: &str, _input: &Value) -> Result<Value, String> {
        Ok(json!({}))
    }
    fn end_vm_json_trx(&self, _vm_id: &str) {}
    fn http_request(&self, _input: &Value) -> Result<String, String> {
        Err("not implemented in the live test host".to_string())
    }
    fn storage_root(&self) -> String {
        std::env::temp_dir().to_string_lossy().to_string()
    }
}

/// The plugin, wired to an in-memory host, as the node would wire it.
fn plugin() -> ModalVmPlugin {
    // `set_host` is a OnceLock: the first test to run publishes the host and
    // every later one shares it, which is fine — it is keyed by vm id.
    set_host(Arc::new(MemoryHost::default()));
    let meta = VmPluginMeta::from_config_str(include_str!("../vm.config.json"))
        .expect("vm.config.json is invalid");
    ModalVmPlugin::new(meta)
}

fn have_credentials() -> bool {
    crate::client::is_configured()
}

/// A vm id unique to this run, so concurrent/repeat runs never collide on
/// Modal's deterministic resource names.
fn scratch_vm_id(tag: &str) -> String {
    format!(
        "caspar-live-{}-{}",
        tag,
        uuid::Uuid::new_v4().simple().to_string()[..10].to_string()
    )
}

/// The `runVm` packet `spaces/create` sends, minus the bootstrap script — the
/// sandbox is kept alive by a trivial entrypoint so the test measures the
/// runtime, not a crewAI install.
fn run_packet(machine_id: &str, vm_id: &str) -> Value {
    json!({
        "runtime": "modal",
        "machineId": machine_id,
        "entityId": "space",
        "vmId": vm_id,
        "standalone": true,
        "persistent": true,
        "forceRestart": false,
        "image": "ubuntu:24.04",
        "command": "sleep infinity",
        "idleTimeoutSecs": 300,
        // Exactly what `spaces/create` sends, including the stray `diskGb`
        // an older deploy may still carry — Modal has no such knob and it must
        // be ignored rather than mapped onto the scratch disk.
        "resources": { "ramMb": 2048, "cpuCores": 1, "diskGb": 4 },
        "env": { "DECILLION_SPACE_ID": machine_id }
    })
}

fn exec_packet(machine_id: &str, vm_id: &str, command: &str) -> Value {
    json!({
        "runtime": "modal",
        "machineId": machine_id,
        "entityId": "space",
        "vmId": vm_id,
        "command": command,
        "timeoutSecs": 120
    })
}

fn delete_packet(machine_id: &str, vm_id: &str) -> Value {
    json!({
        "runtime": "modal",
        "machineId": machine_id,
        "entityId": "space",
        "vmId": vm_id
    })
}

/// The whole lifecycle a project's machine goes through, in order.
///
/// This is the test that would have caught both outages: it starts a sandbox,
/// runs the exact command the file explorer runs, checks the volume is mounted
/// where `spaces/files` expects it, proves a re-run re-attaches instead of
/// replacing (the wake path), and destroys everything.
#[test]
#[ignore]
fn a_project_machine_starts_execs_and_is_destroyed() {
    if !have_credentials() {
        eprintln!("skipping: MODAL_TOKEN_ID / MODAL_TOKEN_SECRET are not set");
        return;
    }
    let plugin = plugin();
    let machine_id = "caspar-live-space";
    let vm_id = scratch_vm_id("lifecycle");

    let started = plugin
        .run_vm(&run_packet(machine_id, &vm_id))
        .unwrap_or_else(|e| panic!("run_vm failed: {}", e));
    assert_eq!(started["ok"], json!(true), "run_vm: {}", started);
    let sandbox_id = started["sandboxId"].as_str().unwrap_or("").to_string();
    assert!(!sandbox_id.is_empty(), "run_vm returned no sandboxId: {}", started);
    println!("started sandbox {} for vm {}", sandbox_id, vm_id);

    // Everything below must run even when an assertion fails, or a failed test
    // leaves a sandbox running and billing.
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        // 1) The file explorer's own command, verbatim.
        let listed = plugin
            .exec_vm(&exec_packet(machine_id, &vm_id, "mkdir -p '/data' && ls -Ap1 '/data'"))
            .unwrap_or_else(|e| panic!("exec_vm (list) failed: {}", e));
        assert_eq!(listed["ok"], json!(true), "listing /data: {}", listed);
        assert_eq!(listed["exitCode"], json!(0), "listing /data: {}", listed);

        // 2) /data is the persistent volume, not just a directory — this is what
        //    makes a woken machine come back with the project's files.
        // /data must be the Modal Volume, not a directory in the image. Modal
        // wires a volume mount as a SYMLINK into /__modal/volumes rather than a
        // mount-table entry, so this reads the link rather than /proc/mounts —
        // an assertion about the mount table passes on a plain directory and
        // fails on a working volume, which is exactly backwards.
        let resolved = plugin
            .exec_vm(&exec_packet(machine_id, &vm_id, "readlink -f /data"))
            .unwrap_or_else(|e| panic!("exec_vm (readlink) failed: {}", e));
        assert!(
            resolved["stdout"].as_str().unwrap_or("").contains("/__modal/volumes/"),
            "/data is not the project's Modal volume: {}",
            resolved,
        );

        // 3) Round-trip a file, and read it back in a SEPARATE exec — one exec
        //    seeing its own write proves nothing about the sandbox's state.
        let written = plugin
            .exec_vm(&exec_packet(machine_id, &vm_id, "echo decillion > /data/live-test.txt"))
            .unwrap_or_else(|e| panic!("exec_vm (write) failed: {}", e));
        assert_eq!(written["ok"], json!(true), "writing a file: {}", written);
        let read_back = plugin
            .exec_vm(&exec_packet(machine_id, &vm_id, "cat /data/live-test.txt"))
            .unwrap_or_else(|e| panic!("exec_vm (read) failed: {}", e));
        assert_eq!(
            read_back["stdout"].as_str().unwrap_or("").trim(),
            "decillion",
            "file did not survive between execs: {}",
            read_back,
        );

        // 4) A failing command must come back as a failure WITH its exit code —
        //    `execInSpaceVm` decides whether to wake the machine on exactly
        //    that: an exit code means the command ran and failed on its own.
        let failed = plugin
            .exec_vm(&exec_packet(machine_id, &vm_id, "ls /definitely-not-here"))
            .unwrap_or_else(|e| panic!("exec_vm (failing command) failed: {}", e));
        assert_eq!(failed["ok"], json!(false), "a failing command: {}", failed);
        assert!(
            failed["exitCode"].as_i64().unwrap_or(0) != 0,
            "a command that ran and failed must report its exit code: {}",
            failed,
        );

        // 5) status_vm sees it running.
        let status = plugin
            .status_vm(&delete_packet(machine_id, &vm_id))
            .unwrap_or_else(|e| panic!("status_vm failed: {}", e));
        assert_eq!(status["running"], json!(true), "status_vm: {}", status);

        // 6) The wake path: run again with forceRestart:false. It must RE-ATTACH
        //    to the same sandbox, not replace it — replacing one on every file
        //    read would throw away the crewAI install and the running bridge.
        let resumed = plugin
            .run_vm(&run_packet(machine_id, &vm_id))
            .unwrap_or_else(|e| panic!("run_vm (resume) failed: {}", e));
        assert_eq!(resumed["resumed"], json!(true), "second run_vm: {}", resumed);
        assert_eq!(
            resumed["sandboxId"].as_str().unwrap_or(""),
            sandbox_id,
            "a wake replaced the machine instead of re-attaching: {}",
            resumed,
        );

        // 7) The promise the whole design rests on: a machine that has gone
        //    away and come back still has the project's files. Proved by
        //    replacing the sandbox and reading the file back from the new one,
        //    because that is what a sleep and a wake actually do.
        let mut forced = run_packet(machine_id, &vm_id);
        forced["forceRestart"] = json!(true);
        let replaced = plugin
            .run_vm(&forced)
            .unwrap_or_else(|e| panic!("run_vm (replace) failed: {}", e));
        assert_ne!(
            replaced["sandboxId"].as_str().unwrap_or(""),
            sandbox_id,
            "forceRestart did not replace the machine: {}",
            replaced,
        );
        let survived = plugin
            .exec_vm(&exec_packet(machine_id, &vm_id, "cat /data/live-test.txt"))
            .unwrap_or_else(|e| panic!("exec_vm (read after restart) failed: {}", e));
        assert_eq!(
            survived["stdout"].as_str().unwrap_or("").trim(),
            "decillion",
            "the project's files did not survive the machine being replaced: {}",
            survived,
        );
    }));

    let deleted = plugin.delete_vm(&delete_packet(machine_id, &vm_id));
    if let Err(payload) = outcome {
        std::panic::resume_unwind(payload);
    }
    let deleted = deleted.unwrap_or_else(|e| panic!("delete_vm failed: {}", e));
    assert_eq!(deleted["ok"], json!(true), "delete_vm: {}", deleted);

    // After a delete there is no sandbox to find, and `execInSpaceVm` relies on
    // that answering without an exit code so it knows to start one.
    let after = plugin.status_vm(&delete_packet(machine_id, &vm_id));
    match after {
        Ok(v) => assert_ne!(v["running"], json!(true), "sandbox still running after delete: {}", v),
        Err(_) => {}
    }
}

/// A `forceRestart` run replaces the machine — which is what a re-provision
/// means, and what makes a leaked bridge token stop working.
#[test]
#[ignore]
fn a_forced_run_replaces_the_machine() {
    if !have_credentials() {
        eprintln!("skipping: MODAL_TOKEN_ID / MODAL_TOKEN_SECRET are not set");
        return;
    }
    let plugin = plugin();
    let machine_id = "caspar-live-space";
    let vm_id = scratch_vm_id("restart");

    let first = plugin
        .run_vm(&run_packet(machine_id, &vm_id))
        .unwrap_or_else(|e| panic!("run_vm failed: {}", e));
    let first_id = first["sandboxId"].as_str().unwrap_or("").to_string();
    assert!(!first_id.is_empty(), "run_vm returned no sandboxId: {}", first);

    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let mut forced = run_packet(machine_id, &vm_id);
        forced["forceRestart"] = json!(true);
        let second = plugin
            .run_vm(&forced)
            .unwrap_or_else(|e| panic!("run_vm (force) failed: {}", e));
        let second_id = second["sandboxId"].as_str().unwrap_or("").to_string();
        assert!(!second_id.is_empty(), "forced run returned no sandboxId: {}", second);
        assert_ne!(
            second_id, first_id,
            "forceRestart re-attached instead of replacing the machine: {}",
            second,
        );
        // The replacement must be usable, not merely created.
        let listed = plugin
            .exec_vm(&exec_packet(machine_id, &vm_id, "ls -Ap1 /data"))
            .unwrap_or_else(|e| panic!("exec on the replacement failed: {}", e));
        assert_eq!(listed["ok"], json!(true), "exec on the replacement: {}", listed);
    }));

    let _ = plugin.delete_vm(&delete_packet(machine_id, &vm_id));
    if let Err(payload) = outcome {
        std::panic::resume_unwind(payload);
    }
}

/// Modal must accept the client version this plugin sends.
///
/// `x-modal-client-version` is parsed by Modal, not just logged: a name-and-
/// slash string is refused with `Invalid client version`, and a deprecated
/// semver is refused too. Both refusals arrive as a `FailedPrecondition` on the
/// FIRST call, which is how "the project's machine could not be started" was
/// all anyone saw. One cheap call proves the header is still accepted.
#[test]
#[ignore]
fn modal_accepts_our_client_version() {
    if !have_credentials() {
        eprintln!("skipping: MODAL_TOKEN_ID / MODAL_TOKEN_SECRET are not set");
        return;
    }
    let mut conn = crate::client::connect().expect("connect");
    let res = crate::client::block_on(conn.stub.app_get_or_create(
        crate::proto::AppGetOrCreateRequest {
            app_name: "caspar-live-version-check".to_string(),
            environment_name: conn.environment.clone(),
            object_creation_type: crate::proto::ObjectCreationType::CreateIfMissing as i32,
        },
    ))
    .expect("transport");
    match res {
        Ok(ok) => assert!(!ok.into_inner().app_id.is_empty(), "no app id returned"),
        Err(status) => panic!(
            "Modal refused the client version this plugin sends ({}): {}",
            crate::client::client_version(),
            status.message(),
        ),
    }
}

/// The production wake path, end to end.
///
/// A project's machine is terminated after its idle window — by Modal, minutes
/// after anyone last touched it — and the next file read or prompt has to bring
/// it back with the project's files intact. That is one `run_vm` with
/// `forceRestart: false`, and it has to notice the machine is gone.
///
/// It did not. `is_running` was built on a zero-timeout `SandboxWait`, which
/// answers `result: None` for a sandbox that has already been terminated, and
/// `None` was read as "still running" — so the wake reported `resumed`, changed
/// nothing, and every exec after it failed with "Sandbox was cancelled by
/// user". A project whose machine had slept once could never get it back.
#[test]
#[ignore]
fn a_slept_machine_is_woken_with_its_files() {
    if !have_credentials() {
        eprintln!("skipping: MODAL_TOKEN_ID / MODAL_TOKEN_SECRET are not set");
        return;
    }
    let plugin = plugin();
    let machine_id = "caspar-live-space";
    let vm_id = scratch_vm_id("wake");

    let started = plugin
        .run_vm(&run_packet(machine_id, &vm_id))
        .unwrap_or_else(|e| panic!("run_vm failed: {}", e));
    let first_sandbox = started["sandboxId"].as_str().unwrap_or("").to_string();
    assert!(!first_sandbox.is_empty(), "run_vm returned no sandboxId: {}", started);

    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let wrote = plugin
            .exec_vm(&exec_packet(machine_id, &vm_id, "echo woken > /data/wake-test.txt"))
            .unwrap_or_else(|e| panic!("exec_vm (write) failed: {}", e));
        assert_eq!(wrote["ok"], json!(true), "writing before the sleep: {}", wrote);

        // Stop the machine the way its idle window does: terminate it, leaving
        // the volume and the recorded link exactly as a sleep leaves them.
        let suspended = plugin
            .terminate_vm(&delete_packet(machine_id, &vm_id))
            .unwrap_or_else(|e| panic!("terminate_vm failed: {}", e));
        assert_eq!(suspended["terminated"], json!(true), "terminate_vm: {}", suspended);

        // Now the wake: the same call `ensureSpaceVmAwake` makes.
        let woken = plugin
            .run_vm(&run_packet(machine_id, &vm_id))
            .unwrap_or_else(|e| panic!("run_vm (wake) failed: {}", e));
        assert_ne!(
            woken["resumed"],
            json!(true),
            "the wake re-attached to the terminated machine instead of starting a new one: {}",
            woken,
        );
        assert_ne!(
            woken["sandboxId"].as_str().unwrap_or(""),
            first_sandbox,
            "the wake returned the dead sandbox: {}",
            woken,
        );

        // And the machine that came back is usable, with the project's files.
        let read_back = plugin
            .exec_vm(&exec_packet(machine_id, &vm_id, "cat /data/wake-test.txt"))
            .unwrap_or_else(|e| panic!("exec_vm after the wake failed: {}", e));
        assert_eq!(
            read_back["stdout"].as_str().unwrap_or("").trim(),
            "woken",
            "the woken machine does not have the project's files: {}",
            read_back,
        );
    }));

    let _ = plugin.delete_vm(&delete_packet(machine_id, &vm_id));
    if let Err(payload) = outcome {
        std::panic::resume_unwind(payload);
    }
}

/// A sandbox's public URLs, which is how a member reaches anything running on
/// their project's machine — the desktop `spaces/installGui` puts there, a
/// preview server an agent starts.
///
/// The platform never asks for a port explicitly: the plugin always exposes the
/// VM HTTP port, and `spaces/installGui` reads the URL back out of
/// `vmEndpoints`. If tunnels do not come back, that feature has no link to give
/// and fails with nothing to explain it.
#[test]
#[ignore]
fn a_machine_publishes_reachable_endpoints() {
    if !have_credentials() {
        eprintln!("skipping: MODAL_TOKEN_ID / MODAL_TOKEN_SECRET are not set");
        return;
    }
    let plugin = plugin();
    let machine_id = "caspar-live-space";
    let vm_id = scratch_vm_id("ports");

    plugin
        .run_vm(&run_packet(machine_id, &vm_id))
        .unwrap_or_else(|e| panic!("run_vm failed: {}", e));

    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let endpoints = plugin
            .vm_endpoints(&delete_packet(machine_id, &vm_id))
            .unwrap_or_else(|e| panic!("vm_endpoints failed: {}", e));
        let list = endpoints["endpoints"].as_array().cloned().unwrap_or_default();
        assert!(
            !list.is_empty(),
            "the machine published no endpoints, so nothing running on it can be reached: {}",
            endpoints,
        );
        for entry in &list {
            let url = entry["url"].as_str().unwrap_or("");
            assert!(
                url.starts_with("http://") || url.starts_with("https://"),
                "endpoint has no usable url: {}",
                entry,
            );
            assert!(
                entry["containerPort"].as_u64().unwrap_or(0) > 0,
                "endpoint names no container port, so nothing can be keyed to it: {}",
                entry,
            );
        }
        println!("endpoints: {}", endpoints);
    }));

    let _ = plugin.delete_vm(&delete_packet(machine_id, &vm_id));
    if let Err(payload) = outcome {
        std::panic::resume_unwind(payload);
    }
}

/// Look inside a project's machine, by the vm id the platform gave it.
///
/// A project's runtime installs itself from the sandbox's entrypoint and logs to
/// `/var/log/decillion/bridge.log` — outside `/data`, so `spaces/files` cannot
/// reach it by design. When a project reports its runtime as "installing"
/// forever there is otherwise nothing to look at, which is exactly when
/// somebody needs to look. The sandbox is found by the `caspar-vm-id` tag the
/// plugin stamps at create time, so only the vm id (and the app's machine id)
/// is needed — no node state.
///
///     MODAL_TOKEN_ID=… MODAL_TOKEN_SECRET=… \
///     PROBE_MACHINE_ID='11@store' PROBE_VM_ID='12@spaces.vm' \
///     PROBE_CMD='tail -50 /var/log/decillion/bridge.log' \
///       cargo test -p caspar-vm-modal probe_project_machine -- --ignored --nocapture
#[test]
#[ignore]
fn probe_project_machine() {
    if !have_credentials() {
        eprintln!("skipping: MODAL_TOKEN_ID / MODAL_TOKEN_SECRET are not set");
        return;
    }
    let machine_id = std::env::var("PROBE_MACHINE_ID").unwrap_or_default();
    let vm_id = std::env::var("PROBE_VM_ID").unwrap_or_default();
    let command = std::env::var("PROBE_CMD")
        .unwrap_or_else(|_| "tail -60 /var/log/decillion/bridge.log 2>&1".to_string());
    if machine_id.is_empty() || vm_id.is_empty() {
        eprintln!("set PROBE_MACHINE_ID and PROBE_VM_ID");
        return;
    }

    let mut conn = crate::client::connect().expect("connect");
    let app_id = crate::client::block_on(conn.stub.app_get_or_create(
        crate::proto::AppGetOrCreateRequest {
            app_name: crate::models::modal_app_name(&machine_id),
            environment_name: conn.environment.clone(),
            object_creation_type: crate::proto::ObjectCreationType::CreateIfMissing as i32,
        },
    ))
    .expect("transport")
    .expect("app")
    .into_inner()
    .app_id;

    let listed = crate::client::block_on(conn.stub.sandbox_list(
        crate::proto::SandboxListRequest {
            app_id,
            environment_name: conn.environment.clone(),
            include_finished: false,
            ..Default::default()
        },
    ))
    .expect("transport")
    .expect("list")
    .into_inner();

    let sandbox = listed
        .sandboxes
        .into_iter()
        .find(|s| s.tags.iter().any(|t| t.tag_name == "caspar-vm-id" && t.tag_value == vm_id));
    let Some(sandbox) = sandbox else {
        println!("no live sandbox tagged caspar-vm-id={}", vm_id);
        return;
    };
    println!("sandbox {} for vm {}", sandbox.id, vm_id);

    // Reuse the plugin's own exec so what is observed is what the platform does.
    set_host(Arc::new(MemoryHost::default()));
    if let Some(h) = caspar_vm_sdk::host::host() {
        let _ = h.state_apply_ops(&[KvOp {
            op: "put".into(),
            key: crate::models::sandbox_link_key(&vm_id),
            val: sandbox.id.clone(),
        }]);
    }
    let plugin = {
        let meta = VmPluginMeta::from_config_str(include_str!("../vm.config.json")).unwrap();
        ModalVmPlugin::new(meta)
    };
    let out = plugin
        .exec_vm(&exec_packet(&machine_id, &vm_id, &command))
        .unwrap_or_else(|e| panic!("exec failed: {}", e));
    println!("exit={} \n--- stdout ---\n{}\n--- stderr ---\n{}",
        out["exitCode"], out["stdout"].as_str().unwrap_or(""), out["stderr"].as_str().unwrap_or(""));
}
