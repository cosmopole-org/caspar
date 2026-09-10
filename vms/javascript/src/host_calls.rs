//! The javascript guest ABI: the op table behind `hostCall`.
//!
//! This is deliberately the same table, in the same order, with the same
//! semantics as `vms/wasm/src/host_calls.rs`. A creature must not be able to
//! tell which in-process runtime it was deployed to by the behaviour of a host
//! call — only by the language it is written in.
//!
//! The one structural difference is the transport. A wasm guest hands the host
//! an offset into its linear memory and gets a packed `(offset<<32|len)` handle
//! back; a javascript guest passes and receives an ordinary string, because
//! QuickJS values cross the boundary directly. Nothing above that layer differs.

use std::cell::RefCell;
use std::rc::Rc;

use serde_json::{json, Value as JsonValue};

use caspar_vm_sdk::host::{host, log_vm};

use crate::runtime::JsState;

fn host_dispatch(packet: &JsonValue) -> String {
    match host() {
        Some(h) => h.dispatch(packet),
        None => json!({"ok": false, "error": "caspar vm host is not initialised"}).to_string(),
    }
}

/// Wrap an input in a typed packet and dispatch it through the VMM router.
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

/// The node-stamped envelope every unified host call is made under.
///
/// Identity is NEVER taken from the guest. A javascript creature fully controls
/// `input`, so any identity it puts there is untrusted; the node-assigned VM
/// runtime context (set when the node launched this VM) is the sole source of
/// truth, and it is stamped at the PACKET level, which `resolve_host_hierarchy`
/// trusts over `input`. Operation arguments in `input` — which may legitimately
/// name *other* programs (a deploy target, a signal recipient) — are untouched.
fn envelope(state: &JsState, op: &str, input: JsonValue) -> JsonValue {
    json!({
        "type": "hostCall",
        "op": op,
        "input": input,
        "creatureId": state.machine_id,
        "programId": state.machine_id,
        "machineId": state.machine_id,
        "vmId": state.vm_id,
    })
}

/// Dispatch a VM lifecycle op through the *unified host-call* dispatcher
/// rather than straight at the packet router.
///
/// The difference is identity. The router takes a packet at face value; the
/// unified dispatcher resolves the calling program node-side from the envelope
/// above, which is what lets `runVm` record who owns the VM it launched and
/// `deleteVm` refuse a creature that does not own it.
fn dispatch_owned(state: &JsState, op: &str, input: &JsonValue) -> String {
    match host() {
        Some(h) => h.unified_host_call(&envelope(state, op, input.clone())),
        None => json!({"ok": false, "error": "caspar vm host is not initialised"}).to_string(),
    }
}

/// Ensure the VM's per-lifecycle JSON transaction exists and run one op on it.
fn vm_json_op(state: &mut JsState, op: &str, input: &JsonValue) -> String {
    match host() {
        Some(h) => match h.vm_json_trx_op(&state.vm_id, op, input) {
            Ok(v) => {
                state.vm_trx_open = true;
                v.to_string()
            }
            Err(e) => json!({"ok": false, "error": e}).to_string(),
        },
        None => json!({"ok": false, "error": "ICore not initialised"}).to_string(),
    }
}

/// Handle one `hostCall` from the guest. `raw` is the JSON `{op, input}`
/// envelope; the return value is the JSON response, verbatim.
///
/// Never panics and never returns an empty string: a guest that cannot parse
/// the answer has no way to tell a host failure from a silent one, and the
/// creature ABI has no out-of-band error channel.
pub fn dispatch(cell: &Rc<RefCell<JsState>>, raw: &str) -> String {
    let req: JsonValue = match serde_json::from_str(raw) {
        Ok(v) => v,
        Err(e) => {
            return json!({"ok": false, "error": format!("hostCall request is not valid JSON: {}", e)})
                .to_string()
        }
    };
    let op = req["op"].as_str().unwrap_or("");
    if op.is_empty() {
        return json!({"ok": false, "error": "hostCall requires an `op`"}).to_string();
    }
    let input = req["input"].clone();

    match op {
        "output" => {
            let mut state = cell.borrow_mut();
            state.execution_result = input["text"].as_str().unwrap_or("").to_string();
            state.has_output = true;
            "{}".to_string()
        }
        "consoleLog" => {
            let state = cell.borrow();
            log_vm(
                input["text"].as_str().unwrap_or("").to_string(),
                state.vm_id.clone(),
                input["logType"].as_str().unwrap_or("runtime"),
            );
            "{}".to_string()
        }
        "dbOp" => {
            let mut state = cell.borrow_mut();
            let machine = state.machine_id.clone();
            let op_type = input["op"].as_str().unwrap_or("");
            match op_type {
                "put" => {
                    let key = input["key"].as_str().unwrap_or("");
                    let val = input["val"].as_str().unwrap_or("");
                    state.trx.put(format!("{}::{}", machine, key), val.to_string());
                    "{}".to_string()
                }
                "del" => {
                    let key = input["key"].as_str().unwrap_or("");
                    state.trx.del(format!("{}::{}", machine, key));
                    "{}".to_string()
                }
                "get" => {
                    let key = input["key"].as_str().unwrap_or("");
                    state.trx.get(format!("{}::{}", machine, key))
                }
                "getByPrefix" => {
                    let prefix = input["prefix"].as_str().unwrap_or("");
                    let vals = state.trx.get_by_prefix(format!("{}::{}", machine, prefix));
                    json!({ "data": vals }).to_string()
                }
                _ => "{}".to_string(),
            }
        }
        "commitTrx" => {
            let mut state = cell.borrow_mut();
            // Commit the JSON-level per-VM transaction tracked by the host,
            // then reset so the next use starts a fresh transaction.
            if state.vm_trx_open {
                if let Some(h) = host() {
                    h.end_vm_json_trx(&state.vm_id);
                }
                state.vm_trx_open = false;
            }
            // Also flush the low-level raw dbOp buffer.
            state.trx.commit_as_offchain();
            state.trx = caspar_vm_sdk::trx::Trx::new();
            json!({"ok": true}).to_string()
        }
        "lockResource" | "unlockResource" => {
            let state = cell.borrow();
            let resource_id = input["resourceId"].as_str().unwrap_or("");
            let owner_id = input["ownerId"]
                .as_str()
                .unwrap_or(state.machine_id.as_str());
            let result = match host() {
                Some(h) => {
                    if op == "lockResource" {
                        h.acquire_resource_lock(resource_id, owner_id)
                    } else {
                        h.release_resource_lock(resource_id, owner_id)
                    }
                }
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
        "runVm" => {
            let state = cell.borrow();
            dispatch_owned(&state, "runVm", &input)
        }
        "deleteVm" | "destroyVm" => {
            let state = cell.borrow();
            dispatch_owned(&state, "deleteVm", &input)
        }
        "vmEndpoints" => {
            let state = cell.borrow();
            dispatch_owned(&state, "vmEndpoints", &input)
        }
        "terminateVm" => dispatch_typed("terminateVm", &input),
        "execVm" | "execDocker" => dispatch_typed("execVm", &input),
        "copyToVm" | "copyToDocker" => dispatch_typed("copyToVm", &input),
        "buildVmImage" | "buildDockerImage" => dispatch_typed("buildVmImage", &input),
        "httpPost" | "httpRequest" => match host() {
            Some(h) => match h.http_request(&input) {
                Ok(v) => v,
                Err(e) => json!({"ok": false, "error": e}).to_string(),
            },
            None => json!({"ok": false, "error": "caspar vm host is not initialised"}).to_string(),
        },
        "elpifyProof" | "verifyProgramExecution" => {
            dispatch_typed("verifyProgramExecution", &input)
        }
        // ── Per-VM JSON transaction ops ──────────────────────────────────
        // These operate on the single transaction held for this VM's entire
        // lifecycle; writes persist when `commitTrx` runs or the VM exits.
        "putJson" | "getJson" | "getByPrefix" | "delKey" => {
            let mut state = cell.borrow_mut();
            vm_json_op(&mut state, op, &input)
        }
        _ => {
            // Forward unrecognised ops (signalUser, signalGroup, publishUpdate,
            // registerBridgeToken, execShellAction, …) through the unified
            // host-call dispatcher, under the node-stamped envelope.
            let state = cell.borrow();
            match host() {
                Some(h) => h.unified_host_call(&envelope(&state, op, input)),
                None => {
                    json!({"ok": false, "error": "caspar vm host is not initialised"}).to_string()
                }
            }
        }
    }
}
