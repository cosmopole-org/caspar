//! The wasm guest ABI: WasmEdge host functions exposed to creature modules.
//!
//! Every interaction with the Caspar platform flows through the unified
//! `hostCall` function (JSON request/response over guest memory); the other
//! exports are legacy single-purpose entry points kept for older creature
//! SDK builds. All host capabilities are reached exclusively through the
//! `caspar_vm_sdk` host interface.

use std::ops::DerefMut;
use std::str;

use serde_json::{json, Value as JsonValue};
use wasmedge_sys::{AsInstance, CallingFrame, Executor, Instance, WasmValue};
use wasmedge_types::error::CoreError;

use caspar_vm_sdk::host::{host, log_vm};

use crate::runtime::{HostData, WasmMac};

fn host_dispatch(packet: &JsonValue) -> String {
    match host() {
        Some(h) => h.dispatch(packet),
        None => json!({"ok": false, "error": "caspar vm host is not initialised"}).to_string(),
    }
}

/// Wrap an input in a typed packet and dispatch it through the VMM router
/// (the same path the node's host functions use).
fn dispatch_typed(packet_type: &str, input: &JsonValue) -> String {
    let mut packet = input.clone();
    if let JsonValue::Object(map) = &mut packet {
        map.insert(
            "type".to_string(),
            JsonValue::String(packet_type.to_string()),
        );
    }
    host_dispatch(&packet)
}

/// Dispatch a VM lifecycle op through the *unified host-call* dispatcher
/// rather than straight at the packet router.
///
/// The difference is identity. The router takes a packet at face value; the
/// unified dispatcher resolves the calling program node-side from the packet
/// envelope (which the node stamps here from the VM's runtime context, never
/// from guest `input`), which is what lets `runVm` record who owns the VM it
/// launched and `deleteVm` refuse a creature that does not own it.
fn dispatch_owned(rt: &WasmMac, op: &str, input: &JsonValue) -> String {
    match host() {
        Some(h) => h.unified_host_call(&json!({
            "type": "hostCall",
            "op": op,
            "input": input.clone(),
            "creatureId": rt.machine_id,
            "programId": rt.machine_id,
            "machineId": rt.machine_id,
            "vmId": rt.vm_id,
        })),
        None => json!({"ok": false, "error": "caspar vm host is not initialised"}).to_string(),
    }
}

/// Ensure the VM's per-lifecycle JSON transaction exists and run one op on it.
fn vm_json_op(rt: &mut WasmMac, op: &str, input: &JsonValue) -> String {
    match host() {
        Some(h) => match h.vm_json_trx_op(&rt.vm_id, op, input) {
            Ok(v) => {
                rt.vm_trx_open = true;
                v.to_string()
            }
            Err(e) => json!({"ok": false, "error": e}).to_string(),
        },
        None => json!({"ok": false, "error": "ICore not initialised"}).to_string(),
    }
}

pub fn host_call(
    host_data: &mut HostData,
    _inst: &mut Instance,
    _caller: &mut CallingFrame,
    _input: Vec<WasmValue>,
) -> Result<Vec<WasmValue>, CoreError> {
    let rt: &mut WasmMac = unsafe { &mut *host_data.runtime };
    let exec: &mut Executor = unsafe { &mut *host_data.exec };
    let mem = _caller.memory_mut(0).unwrap();

    let in_offset = _input[0].to_i32();
    let in_l = _input[1].to_i32();
    let in_bytes = mem
        .get_data(in_offset.cast_unsigned(), in_l.cast_unsigned())
        .unwrap_or_default();
    let req_raw = str::from_utf8(&in_bytes).unwrap_or("{}");
    let req: JsonValue = serde_json::from_str(req_raw).unwrap_or_default();
    let op = req["op"].as_str().unwrap_or("");

    let res_str = match op {
        "output" => {
            rt.execution_result = req["input"]["text"].as_str().unwrap_or("").to_string();
            rt.has_output = true;
            "{}".to_string()
        }
        "consoleLog" => {
            log_vm(
                req["input"]["text"].as_str().unwrap_or("").to_string(),
                rt.vm_id.clone(),
                "runtime",
            );
            "{}".to_string()
        }
        "dbOp" => {
            let op_type = req["input"]["op"].as_str().unwrap_or("");
            if op_type == "put" {
                let key = req["input"]["key"].as_str().unwrap_or("");
                let val = req["input"]["val"].as_str().unwrap_or("");
                rt.trx
                    .put(format!("{}::{}", rt.machine_id, key), val.to_string());
                "{}".to_string()
            } else if op_type == "del" {
                let key = req["input"]["key"].as_str().unwrap_or("");
                rt.trx.del(format!("{}::{}", rt.machine_id, key));
                "{}".to_string()
            } else if op_type == "get" {
                let key = req["input"]["key"].as_str().unwrap_or("");
                rt.trx.get(format!("{}::{}", rt.machine_id, key))
            } else if op_type == "getByPrefix" {
                let prefix = req["input"]["prefix"].as_str().unwrap_or("");
                let vals = rt
                    .trx
                    .get_by_prefix(format!("{}::{}", rt.machine_id, prefix));
                json!({"data": vals}).to_string()
            } else {
                "{}".to_string()
            }
        }
        "commitTrx" => {
            // Commit the JSON-level per-VM transaction tracked by the host,
            // then reset so the next use starts a fresh transaction.
            if rt.vm_trx_open {
                if let Some(h) = host() {
                    h.end_vm_json_trx(&rt.vm_id);
                }
                rt.vm_trx_open = false;
            }
            // Also flush the low-level raw dbOp buffer.
            rt.trx.commit_as_offchain();
            rt.trx = Box::new(crate::models::Trx::new());
            json!({"ok": true}).to_string()
        }
        "lockResource" => {
            let resource_id = req["input"]["resourceId"].as_str().unwrap_or("");
            let owner_id = req["input"]["ownerId"]
                .as_str()
                .unwrap_or(rt.machine_id.as_str());
            let result = match host() {
                Some(h) => h.acquire_resource_lock(resource_id, owner_id),
                None => Err("caspar vm host is not initialised".to_string()),
            };
            match result {
                Ok(()) => json!({"ok": true}).to_string(),
                Err(err) => json!({"ok": false, "error": err}).to_string(),
            }
        }
        "unlockResource" => {
            let resource_id = req["input"]["resourceId"].as_str().unwrap_or("");
            let owner_id = req["input"]["ownerId"]
                .as_str()
                .unwrap_or(rt.machine_id.as_str());
            let result = match host() {
                Some(h) => h.release_resource_lock(resource_id, owner_id),
                None => Err("caspar vm host is not initialised".to_string()),
            };
            match result {
                Ok(()) => json!({"ok": true}).to_string(),
                Err(err) => json!({"ok": false, "error": err}).to_string(),
            }
        }
        // runVm and deleteVm carry ownership: the node records the launching
        // creature and checks it before a destroy, so both go through the
        // identity-resolving dispatcher instead of the bare packet router.
        "runVm" => dispatch_owned(rt, "runVm", &req["input"]),
        "deleteVm" | "destroyVm" => dispatch_owned(rt, "deleteVm", &req["input"]),
        "terminateVm" => dispatch_typed("terminateVm", &req["input"]),
        "execVm" | "execDocker" => dispatch_typed("execVm", &req["input"]),
        "copyToVm" | "copyToDocker" => dispatch_typed("copyToVm", &req["input"]),
        "buildVmImage" | "buildDockerImage" => dispatch_typed("buildVmImage", &req["input"]),
        "httpPost" | "httpRequest" => {
            let result = match host() {
                Some(h) => h.http_request(&req["input"]),
                None => Err("caspar vm host is not initialised".to_string()),
            };
            match result {
                Ok(res) => res,
                Err(err) => json!({"ok": false, "error": err}).to_string(),
            }
        }
        // Proof verification is a capability of the provable runtime plugin;
        // reach it through the router's verifyProgramExecution packet.
        "elpifyProof" | "verifyProgramExecution" => {
            dispatch_typed("verifyProgramExecution", &req["input"])
        }
        // ── Per-VM JSON transaction ops ──────────────────────────────────
        // These operate on the single transaction held for this VM's entire
        // lifecycle; writes persist when `commitTrx` runs or the VM exits.
        "putJson" | "getJson" | "getByPrefix" | "delKey" => vm_json_op(rt, op, &req["input"]),
        _ => {
            // Forward unrecognised ops (signalUser, signalGroup, …) through
            // the unified host-call dispatcher so they reach handlers with
            // access to the global signaler/storage.
            //
            // Identity is NEVER taken from the guest. A wasm creature fully
            // controls `input`, so any identity it puts there is untrusted.
            // The node-assigned VM runtime context (machine_id / vm_id, set by
            // the node when it launched this VM — not by the guest) is the sole
            // source of truth. We stamp it at the *packet* level, which
            // `resolve_host_hierarchy` trusts over `input`, so the guest can
            // neither fabricate nor spoof its identity. Operation arguments in
            // `input` (which may legitimately name *other* programs/entities,
            // e.g. a deploy target) are left untouched.
            let input = req["input"].clone();
            match host() {
                Some(h) => h.unified_host_call(&json!({
                    "type": "hostCall",
                    "op": op,
                    "input": input,
                    "creatureId": rt.machine_id,
                    "programId": rt.machine_id,
                    "machineId": rt.machine_id,
                    "vmId": rt.vm_id,
                })),
                None => json!({"ok": false, "error": "caspar vm host is not initialised"})
                    .to_string(),
            }
        }
    };

    let val_l = res_str.len();
    let mut malloc_fn = _inst.get_func_mut("malloc").unwrap();
    let mfn = malloc_fn.deref_mut();
    let alloc_res = exec
        .call_func(mfn, [WasmValue::from_i32(val_l as i32)])
        .unwrap();
    let val_offset = alloc_res[0].to_i32();

    let arr = res_str.as_bytes().to_vec();
    let mut mem2 = _inst.get_memory_mut("memory").unwrap();
    mem2.set_data(arr, val_offset.cast_unsigned()).unwrap();

    let c = ((val_offset as i64) << 32) | (val_l as i64);
    Ok(vec![WasmValue::from_i64(c)])
}

/// Write a string result back into guest memory and return the packed
/// (offset << 32 | len) handle.
fn return_string(
    _inst: &mut Instance,
    exec: &mut Executor,
    val: String,
) -> Result<Vec<WasmValue>, CoreError> {
    let val_l = val.len();
    let mut malloc_fn = _inst.get_func_mut("malloc").unwrap();
    let mfn = malloc_fn.deref_mut();
    let res = exec
        .call_func(mfn, [WasmValue::from_i32(val_l as i32)])
        .unwrap();
    let val_offset = res[0].to_i32();

    let arr = val.as_bytes().to_vec();
    let mut mem2 = _inst.get_memory_mut("memory").unwrap();
    mem2.set_data(arr, val_offset.cast_unsigned()).unwrap();
    let c = ((val_offset as i64) << 32) | (val_l as i64);
    Ok(vec![WasmValue::from_i64(c)])
}

fn read_guest_str(_caller: &mut CallingFrame, offset: WasmValue, len: WasmValue) -> String {
    let mem = _caller.memory_mut(0).unwrap();
    let bytes = mem
        .get_data(offset.to_i32().cast_unsigned(), len.to_i32().cast_unsigned())
        .unwrap_or_default();
    str::from_utf8(&bytes).unwrap_or("").to_string()
}

// ── Legacy single-purpose exports (older creature SDK builds) ───────────────

pub fn output(
    host_data: &mut HostData,
    _inst: &mut Instance,
    _caller: &mut CallingFrame,
    _input: Vec<WasmValue>,
) -> Result<Vec<WasmValue>, CoreError> {
    let rt: &mut WasmMac = unsafe { &mut *host_data.runtime };
    let text = read_guest_str(_caller, _input[0], _input[1]);
    rt.execution_result = text;
    rt.has_output = true;
    Ok(vec![WasmValue::from_i64(0)])
}

pub fn console_log(
    host_data: &mut HostData,
    _inst: &mut Instance,
    _caller: &mut CallingFrame,
    _input: Vec<WasmValue>,
) -> Result<Vec<WasmValue>, CoreError> {
    let rt: &mut WasmMac = unsafe { &mut *host_data.runtime };
    let text = read_guest_str(_caller, _input[0], _input[1]);
    log_vm(text, rt.vm_id.clone(), "runtime");
    Ok(vec![WasmValue::from_i64(0)])
}

pub fn plant_trigger(
    host_data: &mut HostData,
    _inst: &mut Instance,
    _caller: &mut CallingFrame,
    _input: Vec<WasmValue>,
) -> Result<Vec<WasmValue>, CoreError> {
    let rt: &mut WasmMac = unsafe { &mut *host_data.runtime };
    let tag = read_guest_str(_caller, _input[0], _input[1]);
    let text = read_guest_str(_caller, _input[2], _input[3]);
    let store_id = read_guest_str(_caller, _input[4], _input[5]);
    let count = _input[6].to_i32();

    let j = json!({
        "key": "plantTrigger",
        "input": {
            "machineId": rt.machine_id,
            "storeId": store_id,
            "input": text,
            "tag": tag,
            "count": count
        }
    });
    (rt.callback)(j);
    Ok(vec![WasmValue::from_i64(0)])
}

pub fn http_post(
    host_data: &mut HostData,
    _inst: &mut Instance,
    _caller: &mut CallingFrame,
    _input: Vec<WasmValue>,
) -> Result<Vec<WasmValue>, CoreError> {
    let rt: &mut WasmMac = unsafe { &mut *host_data.runtime };
    let exec: &mut Executor = unsafe { &mut *host_data.exec };
    let url = read_guest_str(_caller, _input[0], _input[1]);
    let headers = read_guest_str(_caller, _input[2], _input[3]);
    let body = read_guest_str(_caller, _input[4], _input[5]);

    let j = json!({
        "key": "httpPost",
        "input": {
            "machineId": rt.machine_id,
            "url": url,
            "headers": headers,
            "body": body
        }
    });
    let val = (rt.callback)(j);
    return_string(_inst, exec, val)
}

/// Legacy guest entry point for spinning up a container VM owned by this
/// creature. The runtime key here is part of the frozen guest ABI (the guest
/// explicitly asks for a container), not node-side routing.
pub fn run_docker(
    host_data: &mut HostData,
    _inst: &mut Instance,
    _caller: &mut CallingFrame,
    _input: Vec<WasmValue>,
) -> Result<Vec<WasmValue>, CoreError> {
    let rt: &mut WasmMac = unsafe { &mut *host_data.runtime };
    let exec: &mut Executor = unsafe { &mut *host_data.exec };
    let image_name = read_guest_str(_caller, _input[0], _input[1]);
    let text = read_guest_str(_caller, _input[2], _input[3]);
    let container_name = read_guest_str(_caller, _input[4], _input[5]);

    let j = json!({
        "key": "runVm",
        "input": {
            "runtime": "docker",
            "machineId": rt.machine_id,
            "storeId": rt.store_id,
            "inputFiles": text,
            "imageName": image_name,
            "containerName": container_name
        }
    });
    let val = (rt.callback)(j);
    return_string(_inst, exec, val)
}

pub fn exec_docker(
    host_data: &mut HostData,
    _inst: &mut Instance,
    _caller: &mut CallingFrame,
    _input: Vec<WasmValue>,
) -> Result<Vec<WasmValue>, CoreError> {
    let rt: &mut WasmMac = unsafe { &mut *host_data.runtime };
    let exec: &mut Executor = unsafe { &mut *host_data.exec };
    let image_name = read_guest_str(_caller, _input[0], _input[1]);
    let container_name = read_guest_str(_caller, _input[2], _input[3]);
    let command = read_guest_str(_caller, _input[4], _input[5]);

    let j = json!({
        "key": "execDocker",
        "input": {
            "machineId": rt.machine_id,
            "imageName": image_name,
            "containerName": container_name,
            "command": command
        }
    });
    let res = (rt.callback)(j);
    let val = json!({ "data": res }).to_string();
    return_string(_inst, exec, val)
}

pub fn copy_to_docker(
    host_data: &mut HostData,
    _inst: &mut Instance,
    _caller: &mut CallingFrame,
    _input: Vec<WasmValue>,
) -> Result<Vec<WasmValue>, CoreError> {
    let rt: &mut WasmMac = unsafe { &mut *host_data.runtime };
    let exec: &mut Executor = unsafe { &mut *host_data.exec };
    let image_name = read_guest_str(_caller, _input[0], _input[1]);
    let container_name = read_guest_str(_caller, _input[2], _input[3]);
    let file_name = read_guest_str(_caller, _input[4], _input[5]);
    let content = read_guest_str(_caller, _input[6], _input[7]);

    let j = json!({
        "key": "copyToDocker",
        "input": {
            "machineId": rt.machine_id,
            "imageName": image_name,
            "containerName": container_name,
            "fileName": file_name,
            "content": content
        }
    });
    let res = (rt.callback)(j);
    let val = json!({ "data": res }).to_string();
    return_string(_inst, exec, val)
}

pub fn signal_store(
    host_data: &mut HostData,
    _inst: &mut Instance,
    _caller: &mut CallingFrame,
    _input: Vec<WasmValue>,
) -> Result<Vec<WasmValue>, CoreError> {
    let rt: &mut WasmMac = unsafe { &mut *host_data.runtime };
    let exec: &mut Executor = unsafe { &mut *host_data.exec };
    let typ = read_guest_str(_caller, _input[0], _input[1]);
    let store_id = read_guest_str(_caller, _input[2], _input[3]);
    let user_id = read_guest_str(_caller, _input[4], _input[5]);
    let payload = read_guest_str(_caller, _input[6], _input[7]);

    let j = json!({
        "key": "signal",
        "input": {
            "machineId": rt.machine_id,
            "type": typ,
            "storeId": store_id,
            "userId": user_id,
            "data": payload
        }
    });
    let val = (rt.callback)(j);
    return_string(_inst, exec, val)
}

pub fn trx_put(
    host_data: &mut HostData,
    _inst: &mut Instance,
    _caller: &mut CallingFrame,
    _input: Vec<WasmValue>,
) -> Result<Vec<WasmValue>, CoreError> {
    let rt: &mut WasmMac = unsafe { &mut *host_data.runtime };
    let key = read_guest_str(_caller, _input[0], _input[1]);
    let val = read_guest_str(_caller, _input[2], _input[3]);
    rt.trx.put(format!("{}::{}", rt.machine_id, key), val);
    Ok(vec![WasmValue::from_i64(0)])
}

pub fn trx_del(
    host_data: &mut HostData,
    _inst: &mut Instance,
    _caller: &mut CallingFrame,
    _input: Vec<WasmValue>,
) -> Result<Vec<WasmValue>, CoreError> {
    let rt: &mut WasmMac = unsafe { &mut *host_data.runtime };
    let key = read_guest_str(_caller, _input[0], _input[1]);
    rt.trx.del(format!("{}::{}", rt.machine_id, key));
    Ok(vec![WasmValue::from_i64(0)])
}

pub fn trx_get(
    host_data: &mut HostData,
    _inst: &mut Instance,
    _caller: &mut CallingFrame,
    _input: Vec<WasmValue>,
) -> Result<Vec<WasmValue>, CoreError> {
    let rt: &mut WasmMac = unsafe { &mut *host_data.runtime };
    let exec: &mut Executor = unsafe { &mut *host_data.exec };
    let key = read_guest_str(_caller, _input[0], _input[1]);
    let val = rt.trx.get(format!("{}::{}", rt.machine_id, key));
    return_string(_inst, exec, val)
}

pub fn trx_get_by_prefix(
    host_data: &mut HostData,
    _inst: &mut Instance,
    _caller: &mut CallingFrame,
    _input: Vec<WasmValue>,
) -> Result<Vec<WasmValue>, CoreError> {
    let rt: &mut WasmMac = unsafe { &mut *host_data.runtime };
    let exec: &mut Executor = unsafe { &mut *host_data.exec };
    let prefix = read_guest_str(_caller, _input[0], _input[1]);
    let vals = rt
        .trx
        .get_by_prefix(format!("{}::{}", rt.machine_id, prefix));
    let val = json!({ "data": vals }).to_string();
    return_string(_inst, exec, val)
}
