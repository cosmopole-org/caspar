//! Translation of `shell/api/actions/program/program.go`.
//!
//! Wires the program / app / VM action surface and translates the Go bodies
//! one-to-one. A few notes on the things that didn't survive the port verbatim:
//!
//! * **Per-minute billing background loop.** Go's `Install` spawned a
//!   15-second ticker calling `chargeRunningStandaloneVmsIfNeeded`, which
//!   advances locked-token billing for every running standalone VM. The Rust
//!   port mirrors that ticker using a background thread.
//! * **Chain re-entry for `consumeLock`.** The billing helper now synchronously
//!   calls `Globe.SendBaseRequestOnChain("/creatures/consumeLock", ...)` and
//!   updates VM billing state on success.
//! * The Go module also boots the existing programs (calling `Vmm.Assign` and
//!   replaying any pending `vmAlarm*` links). The Rust `install` mirrors the
//!   one-shot scan; the timed alarm replay is preserved.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use anyhow::{anyhow, Result};
use base64::Engine;
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use uuid::Uuid;

use crate::core::actor::model::secured::guard::Guard;
use crate::models::action::ISecureAction;
use crate::models::core::ICore;
use crate::models::state::IState;
use crate::models::transaction::ITrx;
use crate::shell::api::model::{Creature, Entity, Program};
use crate::shell::api::packets::plugin::PlugInput;
use crate::shell::api::packets::program::{
    CreateMachineInput, DeleteProgramInput, DeployInput, DownloadEntityInput, ListAppMachsInput,
    ListInput, MachineBuildsInput, ReadVmLogsInput, RunProgramEntityInput, UpdateProgramInput,
    VmResourcesInput, VmTerminalInput,
};
use crate::shell::utils::future::async_once;

use super::util::build_secure_action;

const PLUGINS_TEMPLATE_NAME: &str = "/machines/";

fn user_guard() -> Guard {
    Guard {
        is_user: true,
        is_in_store: false,
        allow_applet_sign: true,
    }
}

fn normalize_entity_type(s: &str) -> String {
    s.trim().to_lowercase()
}

/// Resolve the machine that owns a program.
///
/// `Program::machine_id` is canonical for current state. Older/restored state can
/// contain a damaged program row while still retaining the immutable
/// `machinePrograms::<machine>::<program>` link written by `/programs/create` in
/// the same transaction. In that case, use the link as a compatibility fallback.
/// We deliberately fail closed when no linked owner exists or more than one
/// linked machine is found.
pub(crate) fn resolve_program_owner_machine(trx: &dyn ITrx, program: &Program) -> Creature {
    let canonical = Creature {
        id: program.machine_id.clone(),
        ..Default::default()
    }
    .pull(trx);
    if !canonical.owner_id.is_empty() {
        return canonical;
    }

    let prefix = "machinePrograms::";
    let suffix = format!("::{}", program.id);
    let mut resolved: Option<Creature> = None;
    let links = trx.get_links_list(prefix, -1, -1, &[]).unwrap_or_default();
    for link in links {
        let Some(machine_id) = link
            .strip_prefix(prefix)
            .and_then(|rest| rest.strip_suffix(&suffix))
        else {
            continue;
        };
        if machine_id.is_empty() {
            continue;
        }
        let candidate = Creature {
            id: machine_id.to_string(),
            ..Default::default()
        }
        .pull(trx);
        if candidate.owner_id.is_empty() {
            continue;
        }
        if resolved
            .as_ref()
            .is_some_and(|existing| existing.id != candidate.id)
        {
            return Creature::default();
        }
        resolved = Some(candidate);
    }
    resolved.unwrap_or_default()
}

/// Execute a runtime plugin's stop plan against the current transaction:
/// take the plan's terminate input and resolve every requested state link
/// into it. With `strict`, a missing required link aborts the stop (used by
/// the user-facing stopEntity action); without it, best-effort (used by the
/// billing reaper, which must always be able to tear a VM down).
fn build_stop_input_from_plan(
    app: &Arc<dyn ICore>,
    trx: &dyn ITrx,
    runtime: &str,
    ctx: &Value,
    strict: bool,
) -> Result<Map<String, Value>> {
    let plan = app
        .tools()
        .vmm()
        .plan_stop_entity(runtime, ctx)
        .map_err(|e| anyhow!(e))?;
    let mut stop_input: Map<String, Value> = plan["input"].as_object().cloned().unwrap_or_default();
    if let Some(links) = plan["links"].as_array() {
        for query in links {
            let field = query["field"].as_str().unwrap_or("");
            let key = query["key"].as_str().unwrap_or("");
            if field.is_empty() || key.is_empty() {
                continue;
            }
            let value = trx.get_link(key);
            if value.is_empty() && strict && query["required"].as_bool().unwrap_or(false) {
                return Err(anyhow!("entity runtime links are not found"));
            }
            stop_input.insert(field.to_string(), json!(value));
        }
    }
    Ok(stop_input)
}

/// Execute a runtime plugin's *delete* plan against the current transaction.
///
/// Same contract as [`build_stop_input_from_plan`] — the plan names the state
/// links a given runtime needs resolved into its packet — but resolved from
/// `plan_delete_entity`, and always strict: a delete that cannot address the
/// concrete instance must fail rather than destroy the wrong one (or nothing,
/// silently).
fn build_delete_input_from_plan(
    app: &Arc<dyn ICore>,
    trx: &dyn ITrx,
    runtime: &str,
    ctx: &Value,
) -> Result<Map<String, Value>> {
    let plan = app
        .tools()
        .vmm()
        .plan_delete_entity(runtime, ctx)
        .map_err(|e| anyhow!(e))?;
    let mut delete_input: Map<String, Value> =
        plan["input"].as_object().cloned().unwrap_or_default();
    if let Some(links) = plan["links"].as_array() {
        for query in links {
            let field = query["field"].as_str().unwrap_or("");
            let key = query["key"].as_str().unwrap_or("");
            if field.is_empty() || key.is_empty() {
                continue;
            }
            let value = trx.get_link(key);
            if value.is_empty() && query["required"].as_bool().unwrap_or(false) {
                return Err(anyhow!("entity runtime links are not found"));
            }
            delete_input.insert(field.to_string(), json!(value));
        }
    }
    Ok(delete_input)
}

fn as_i64(raw: &Value) -> Option<i64> {
    match raw {
        Value::Number(n) => n.as_i64().or_else(|| n.as_f64().map(|f| f as i64)),
        _ => None,
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct VmResources {
    #[serde(rename = "maxExecTimeSeconds")]
    max_exec_time_seconds: i64,
    #[serde(rename = "ramMb")]
    ram_mb: i64,
    #[serde(rename = "diskGb")]
    disk_gb: i64,
    #[serde(rename = "cpuCores")]
    cpu_cores: i64,
}

fn normalize_vm_resources(input: &VmResourcesInput) -> VmResources {
    let mut res = VmResources {
        max_exec_time_seconds: input.max_exec_time_seconds,
        ram_mb: input.ram_mb,
        disk_gb: input.disk_gb,
        cpu_cores: input.cpu_cores,
    };
    if res.max_exec_time_seconds <= 0 {
        res.max_exec_time_seconds = 60;
    }
    if res.ram_mb <= 0 {
        res.ram_mb = 64;
    }
    if res.disk_gb <= 0 {
        res.disk_gb = 1;
    }
    if res.cpu_cores <= 0 {
        res.cpu_cores = 1;
    }
    res
}

fn vm_per_minute_cost(app: &Arc<dyn ICore>, resources: &VmResources) -> i64 {
    let cost = (resources.ram_mb * app.vm_ram_cost_per_mb_per_minute())
        + (resources.cpu_cores * app.vm_cpu_core_cost_per_minute())
        + (resources.disk_gb * app.vm_disk_cost_per_gb_per_minute());
    if cost <= 0 {
        1
    } else {
        cost
    }
}

fn validate_and_build_vm_billing(
    app: &Arc<dyn ICore>,
    trx: &dyn ITrx,
    payer_id: &str,
    lock_id: &str,
    payment_signatures: &[String],
    resources: &VmResources,
) -> Result<Map<String, Value>> {
    if lock_id.is_empty() {
        return Err(anyhow!(
            "paymentLockId is required for standalone vm execution"
        ));
    }
    let payment = trx
        .get_json(
            &format!("Json::Creature::{}", payer_id),
            &format!("lockedTokens.{}", lock_id),
        )
        .map_err(|_| anyhow!("payment lock not found"))?;
    let target = payment.get("userId").and_then(|v| v.as_str()).unwrap_or("");
    if target != app.owner_id() {
        return Err(anyhow!("payment lock target is invalid"));
    }
    let steps_raw = match payment.get("steps") {
        Some(Value::Array(arr)) if !arr.is_empty() => arr.clone(),
        _ => return Err(anyhow!("payment lock does not include steps")),
    };
    if payment_signatures.len() != steps_raw.len() {
        return Err(anyhow!(
            "paymentSignatures count must match lock steps count"
        ));
    }
    let per_minute_cost = vm_per_minute_cost(app, resources);
    let mut step_unlocks = vec![0i64; steps_raw.len()];
    for (i, raw_step) in steps_raw.iter().enumerate() {
        let step = match raw_step {
            Value::Object(o) => o,
            _ => return Err(anyhow!("invalid payment lock step")),
        };
        let step_amount = step.get("amount").and_then(as_i64).unwrap_or(0);
        if step_amount != per_minute_cost {
            return Err(anyhow!(
                "payment lock step amount must match vm per-minute resource cost"
            ));
        }
        let unlock_at = step.get("unlockAt").and_then(as_i64).unwrap_or(0);
        if unlock_at <= 0 {
            return Err(anyhow!("payment lock step unlockAt is invalid"));
        }
        step_unlocks[i] = unlock_at;
        if i > 0 && (step_unlocks[i] - step_unlocks[i - 1] != 60_000) {
            return Err(anyhow!("payment lock steps must be one-minute apart"));
        }
        let sign_payload = format!(
            "{}:{}:{}:{}:{}",
            lock_id,
            i,
            unlock_at,
            step_amount,
            app.owner_id()
        );
        let (success, _, _) = app.tools().security().auth_with_signature(
            payer_id,
            sign_payload.as_bytes(),
            &payment_signatures[i],
        );
        if !success {
            return Err(anyhow!("payment signature verification failed"));
        }
    }
    let mut out: Map<String, Value> = Map::new();
    out.insert("payerUserId".into(), json!(payer_id));
    out.insert("lockId".into(), json!(lock_id));
    out.insert("perMinuteCost".into(), json!(per_minute_cost));
    out.insert("currentStep".into(), json!(0));
    out.insert("stepCount".into(), json!(steps_raw.len()));
    out.insert("lastChargeMinute".into(), json!(-1i64));
    out.insert("signatures".into(), json!(payment_signatures));
    out.insert("resources".into(), serde_json::to_value(resources)?);
    Ok(out)
}

/// Helper that walks the running `VmBilling::*` link space and produces the
/// list of charge targets the per-minute ticker would normally consume.
///
/// TODO: the timed scheduler is intentionally not started by `install`. Until
/// it is, this helper is unreachable; it's preserved so the scheduler can be
/// dropped in without re-translating the billing logic.
#[allow(dead_code)]
pub(crate) fn charge_running_standalone_vms_if_needed(app: &Arc<dyn ICore>, lock: &Mutex<i64>) {
    let mut guard = match lock.lock() {
        Ok(g) => g,
        Err(p) => p.into_inner(),
    };
    let now = chrono::Utc::now().timestamp();
    let current_minute = now / 60;
    if *guard == current_minute {
        return;
    }
    let targets: Arc<Mutex<Vec<Map<String, Value>>>> = Arc::new(Mutex::new(Vec::new()));
    let targets_for_closure = targets.clone();
    app.modify_state(
        true,
        Box::new(move |tx: &dyn ITrx| {
            let links = match tx.get_links_list("VmBilling::", -1, -1, &[]) {
                Ok(v) => v,
                Err(_) => return Ok(()),
            };
            let mut acc = targets_for_closure.lock().unwrap();
            for link in links {
                let vm_id = link.trim_start_matches("VmBilling::").to_string();
                if vm_id.is_empty() || tx.get_link(&format!("VmStatus::{}", vm_id)) != "running" {
                    continue;
                }
                let billing = match tx.get_json(&format!("Json::VmBilling::{}", vm_id), "payment") {
                    Ok(b) => b,
                    Err(_) => continue,
                };
                let next_step = match billing.get("currentStep").and_then(as_i64) {
                    Some(n) => n,
                    None => continue,
                };
                let payer_id = billing
                    .get("payerUserId")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let lock_id = billing
                    .get("lockId")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let per_minute_cost = billing.get("perMinuteCost").and_then(as_i64).unwrap_or(0);
                let last_charge_minute = billing
                    .get("lastChargeMinute")
                    .and_then(as_i64)
                    .unwrap_or(0);
                let machine_id = billing
                    .get("machineId")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let entity_id = billing
                    .get("entityId")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let signatures_raw: Vec<String> = billing
                    .get("signatures")
                    .and_then(|v| v.as_array())
                    .map(|arr| {
                        arr.iter()
                            .map(|s| s.as_str().unwrap_or("").to_string())
                            .collect()
                    })
                    .unwrap_or_default();
                if payer_id.is_empty()
                    || lock_id.is_empty()
                    || per_minute_cost <= 0
                    || machine_id.is_empty()
                    || entity_id.is_empty()
                {
                    continue;
                }
                if last_charge_minute == current_minute {
                    continue;
                }
                if next_step < 0 {
                    continue;
                }
                if (next_step as usize) >= signatures_raw.len() {
                    if last_charge_minute < current_minute {
                        let mut t: Map<String, Value> = Map::new();
                        t.insert("vmId".into(), json!(vm_id));
                        t.insert("machineId".into(), json!(machine_id));
                        t.insert("entityId".into(), json!(entity_id));
                        t.insert("stopOnly".into(), json!(true));
                        acc.push(t);
                    }
                    continue;
                }
                let mut t: Map<String, Value> = Map::new();
                t.insert("vmId".into(), json!(vm_id));
                t.insert("payerUserId".into(), json!(payer_id));
                t.insert("lockId".into(), json!(lock_id));
                t.insert("step".into(), json!(next_step));
                t.insert("amount".into(), json!(per_minute_cost));
                t.insert(
                    "signature".into(),
                    json!(signatures_raw[next_step as usize]),
                );
                t.insert("machineId".into(), json!(machine_id));
                t.insert("entityId".into(), json!(entity_id));
                acc.push(t);
            }
            Ok(())
        }),
    );
    let targets = std::mem::take(&mut *targets.lock().unwrap());
    for target in targets {
        if target
            .get("stopOnly")
            .and_then(|v| v.as_bool())
            .unwrap_or(false)
        {
            let machine_id = target
                .get("machineId")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let entity_id = target
                .get("entityId")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let vm_id = target
                .get("vmId")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            terminate_standalone_vm(app, &machine_id, &entity_id, &vm_id);
            let vm_id_for_closure = vm_id.clone();
            let machine_id_for_closure = machine_id.clone();
            let entity_id_for_closure = entity_id.clone();
            app.modify_state(
                false,
                Box::new(move |tx: &dyn ITrx| {
                    tx.del_key(&format!("link::VmStatus::{}", vm_id_for_closure));
                    tx.del_key(&format!(
                        "link::VmInstance::{}::{}::{}",
                        machine_id_for_closure, entity_id_for_closure, vm_id_for_closure
                    ));
                    tx.del_key(&format!("link::VmBilling::{}", vm_id_for_closure));
                    tx.del_json(
                        &format!("Json::VmBilling::{}", vm_id_for_closure),
                        "payment",
                    );
                    Ok(())
                }),
            );
            continue;
        }
        let payload = json!({
            "type": "pay",
            "userId": target.get("payerUserId").and_then(|v| v.as_str()).unwrap_or(""),
            "lockId": target.get("lockId").and_then(|v| v.as_str()).unwrap_or(""),
            "signature": target.get("signature").and_then(|v| v.as_str()).unwrap_or(""),
            "amount": target.get("amount").and_then(as_i64).unwrap_or(0),
            "step": target.get("step").and_then(as_i64).unwrap_or(-1),
        });
        let payload_bytes = serde_json::to_vec(&payload).unwrap_or_default();
        let sig = app.sign_packet_as_owner(&payload_bytes);
        let (tx, rx) = std::sync::mpsc::channel::<bool>();
        let owner = app.owner_id();
        app.globe().send_base_request_on_chain(
            "/creatures/consumeLock",
            payload_bytes,
            &sig,
            &owner,
            "",
            Box::new(move |_data, status, err| {
                let ok = err.is_none() && status < 400;
                let _ = tx.send(ok);
            }),
        );
        let consumed = rx
            .recv_timeout(std::time::Duration::from_secs(30))
            .unwrap_or(false);
        let vm_id = target
            .get("vmId")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let machine_id = target
            .get("machineId")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let entity_id = target
            .get("entityId")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        if consumed {
            let vm_for_closure = vm_id.clone();
            app.modify_state(
                false,
                Box::new(move |tx: &dyn ITrx| {
                    let mut billing = tx
                        .get_json(&format!("Json::VmBilling::{}", vm_for_closure), "payment")
                        .unwrap_or_default();
                    let current_step = billing.get("currentStep").and_then(as_i64).unwrap_or(0);
                    billing.insert("currentStep".into(), json!(current_step + 1));
                    billing.insert("lastChargeMinute".into(), json!(current_minute));
                    tx.put_json(
                        &format!("Json::VmBilling::{}", vm_for_closure),
                        "payment",
                        &Value::Object(billing),
                        true,
                    )?;
                    Ok(())
                }),
            );
        } else {
            terminate_standalone_vm(app, &machine_id, &entity_id, &vm_id);
        }
    }
    *guard = current_minute;
}

fn terminate_standalone_vm(app: &Arc<dyn ICore>, machine_id: &str, entity_id: &str, vm_id: &str) {
    let machine_id = machine_id.to_string();
    let entity_id = entity_id.to_string();
    let vm_id = vm_id.to_string();
    let app_for_closure = app.clone();
    app.modify_state(
        true,
        Box::new(move |tx: &dyn ITrx| {
            let entity = Entity {
                program_id: machine_id.clone(),
                entity_id: entity_id.clone(),
                ..Default::default()
            }
            .pull(tx);
            let entity_type = normalize_entity_type(&entity.entity_type);
            let ctx = json!({
                "machineId": machine_id,
                "programId": machine_id,
                "entityId": entity.entity_id,
                "vmId": vm_id,
            });
            let stop_input =
                match build_stop_input_from_plan(&app_for_closure, tx, &entity_type, &ctx, false) {
                    Ok(input) => input,
                    Err(_) => {
                        // Unknown runtime: fall back to a generic terminate so
                        // the billing reaper can still stop the instance.
                        let mut input: Map<String, Value> = Map::new();
                        input.insert("runtime".into(), json!(entity_type));
                        input.insert("machineId".into(), json!(machine_id));
                        input.insert("entityId".into(), json!(entity_id));
                        input.insert("vmId".into(), json!(vm_id));
                        input
                    }
                };
            let msg = json!({
                "key": "terminateVm",
                "input": stop_input,
            });
            app_for_closure.tools().vmm().vm_callback(&msg.to_string());
            Ok(())
        }),
    );
}

fn create_program(app: Arc<dyn ICore>) -> Arc<dyn ISecureAction> {
    let app_for_handler = app.clone();
    build_secure_action::<CreateMachineInput, _>(
        app,
        "/programs/create",
        user_guard(),
        move |state: Arc<dyn IState>, input: CreateMachineInput| -> Result<Value> {
            let trx = state.trx();
            if !trx.has_obj("Creature", &input.app_id) {
                return Err(anyhow!("machine not found"));
            }
            let mut machine = Creature {
                id: input.app_id.clone(),
                ..Default::default()
            }
            .pull(&*trx);
            if machine.owner_id != state.info().user_id() {
                return Err(anyhow!("you are not owner of machine"));
            }
            let program = Program {
                id: app_for_handler
                    .tools()
                    .storage()
                    .gen_id(&*trx, &crate::models::input::IInput::origin(&input)),
                machine_id: machine.id.clone(),
                path: input.path.clone(),
                runtime: input.runtime.clone(),
                comment: input.comment.clone(),
            };
            machine.machines_count += 1;
            machine.push(&*trx);
            program.push(&*trx);
            // The program stores `machine_id` = its owning Machine; ownership is
            // resolved by loading that Machine (no app_id, no side ownership link).
            let _ = trx.put_json(
                &format!("ProgMeta::{}", program.id),
                "metadata",
                &json!({}),
                true,
            );
            trx.put_link(
                &format!("machinePrograms::{}::{}", machine.id, program.id),
                "true",
            );
            Ok(json!({"program": program}))
        },
    )
}

fn delete_program(app: Arc<dyn ICore>) -> Arc<dyn ISecureAction> {
    build_secure_action::<DeleteProgramInput, _>(
        app,
        "/programs/delete",
        user_guard(),
        move |state: Arc<dyn IState>, input: DeleteProgramInput| -> Result<Value> {
            let trx = state.trx();
            if !trx.has_obj("Program", &input.program_id) {
                return Err(anyhow!("program does not exist"));
            }
            let app_id = trx.get_index("Program", "id", "programId", &input.program_id);
            let mut machine = Creature {
                id: app_id,
                ..Default::default()
            }
            .pull(&*trx);
            machine.machines_count -= 1;
            machine.push(&*trx);
            trx.del_index("Program", "id", "programId", &input.program_id);
            trx.del_key(&format!(
                "link::machinePrograms::{}::{}",
                machine.id, input.program_id
            ));
            Ok(json!({}))
        },
    )
}

fn update_program(app: Arc<dyn ICore>) -> Arc<dyn ISecureAction> {
    build_secure_action::<UpdateProgramInput, _>(
        app,
        "/programs/update",
        user_guard(),
        move |state: Arc<dyn IState>, input: UpdateProgramInput| -> Result<Value> {
            let trx = state.trx();
            if !trx.has_obj("Program", &input.program_id) {
                return Err(anyhow!("program does not exist"));
            }
            let mut program = Program {
                id: input.program_id.clone(),
                ..Default::default()
            }
            .pull(&*trx);
            program.path = input.path.clone();
            program.push(&*trx);
            if !input.metadata.is_empty() {
                let meta_value = Value::Object(
                    input
                        .metadata
                        .iter()
                        .map(|(k, v)| (k.clone(), v.clone()))
                        .collect(),
                );
                trx.put_json(
                    &format!("ProgMeta::{}", program.id),
                    "metadata",
                    &meta_value,
                    true,
                )?;
            }
            Ok(json!({}))
        },
    )
}

fn run_program_entity(app: Arc<dyn ICore>) -> Arc<dyn ISecureAction> {
    let app_for_handler = app.clone();
    build_secure_action::<RunProgramEntityInput, _>(
        app,
        "/programs/runEntity",
        user_guard(),
        move |state: Arc<dyn IState>, input: RunProgramEntityInput| -> Result<Value> {
            let trx = state.trx();
            let program_id = if input.program_id.is_empty() {
                input.machine_id.clone()
            } else {
                input.program_id.clone()
            };
            if !trx.has_obj("Program", &program_id) {
                return Err(anyhow!("program does not exist"));
            }
            let program = Program {
                id: program_id.clone(),
                ..Default::default()
            }
            .pull(&*trx);
            let entity = Entity {
                program_id: program.id.clone(),
                entity_id: input.entity_id.clone(),
                ..Default::default()
            }
            .pull(&*trx);
            if entity.entity_id.is_empty() {
                return Err(anyhow!("entity does not exist"));
            }
            let entity_type = normalize_entity_type(&entity.entity_type);
            // A program owns itself: authorize against the recorded program owner
            // rather than the deprecated app_id parent pointer.
            let owner_machine = resolve_program_owner_machine(&*trx, &program);
            if owner_machine.owner_id != state.info().user_id() {
                return Err(anyhow!("you are not owner of this program"));
            }
            let vm_id = Uuid::new_v4().to_string();
            trx.put_link(&format!("VmStatus::{}", vm_id), "running");
            trx.put_link(
                &format!("VmStartedAt::{}", vm_id),
                &chrono::Utc::now().timestamp_millis().to_string(),
            );
            // Bind a deterministic custom VM gateway route to this specific
            // instance when requested. The external URL stays fixed across
            // redeploys (keyed by the owning creature's username + path); only
            // the route's target vm id is refreshed here to the fresh instance.
            let gateway_path = crate::drivers::vmm::http_route::normalize_path(&input.gateway_path);
            if !gateway_path.is_empty() {
                register_gateway_route(
                    &*trx,
                    &owner_machine.id,
                    &program.id,
                    &input.entity_id,
                    &gateway_path,
                    &vm_id,
                    &entity_type,
                );
            }
            // Tag VMs of cluster-distributed programs so their state commits
            // are propagated through the raft consensus (local-mode VMs are
            // deliberately left untagged and never enter the log).
            if trx.get_link(&format!("vmDistribution::{}", program.id)) == "cluster"
                || trx.get_link(&format!(
                    "vmDistribution::{}::{}",
                    program.id, input.entity_id
                )) == "cluster"
            {
                trx.put_link(&format!("vmDistributed::{}", vm_id), "true");
            }
            let resources = normalize_vm_resources(&input.resources);
            // Free-tier bypass: when every VM cost rate is zero there is nothing
            // to bill, so a payment lock is not required. Paid nodes still
            // enforce the lock + per-step signatures via validate_and_build_vm_billing.
            let vm_is_free = app_for_handler.vm_ram_cost_per_mb_per_minute() == 0
                && app_for_handler.vm_cpu_core_cost_per_minute() == 0
                && app_for_handler.vm_disk_cost_per_gb_per_minute() == 0;
            if !vm_is_free {
                let mut billing_data = validate_and_build_vm_billing(
                    &app_for_handler,
                    &*trx,
                    &state.info().user_id(),
                    &input.payment_lock_id,
                    &input.payment_signatures,
                    &resources,
                )?;
                billing_data.insert("machineId".into(), json!(input.machine_id));
                billing_data.insert("entityId".into(), json!(input.entity_id));
                billing_data.insert("vmId".into(), json!(vm_id));
                trx.put_link(&format!("VmBilling::{}", vm_id), "true");
                trx.put_json(
                    &format!("Json::VmBilling::{}", vm_id),
                    "payment",
                    &Value::Object(billing_data),
                    true,
                )?;
            }
            if !app_for_handler
                .tools()
                .vmm()
                .is_supported_runtime(&entity_type)
            {
                return Err(anyhow!("invalid entity type"));
            }
            let params: HashMap<String, String> = if input.params.is_empty() {
                HashMap::new()
            } else {
                input.params.clone()
            };
            trx.put_link(
                &format!("VmInstance::{}::{}::{}", program.id, input.entity_id, vm_id),
                "true",
            );
            // Ask the runtime's plugin how to launch this entity: it returns
            // the full runVm input (per-runtime fields included) plus any
            // state links to record — no per-VM logic lives here.
            let ctx = json!({
                "machineId": input.machine_id,
                "programId": program.id,
                // The program's node-authoritative owner (the machine creature).
                // A docker entity must run as this so its host calls (secretGet,
                // dbOp namespacing) resolve to the creature that owns it — e.g. the
                // agent backbone reads its granted platform keys. Carried through
                // the runtime's plan_run_entity into the runVm packet.
                "creatureId": owner_machine.id,
                "entityId": input.entity_id,
                "vmId": vm_id,
                "resources": resources,
                "params": params,
            });
            let plan = app_for_handler
                .tools()
                .vmm()
                .plan_run_entity(&entity_type, &ctx)
                .map_err(|e| anyhow!(e))?;
            if let Some(links) = plan["links"].as_array() {
                for pair in links {
                    let key = pair[0].as_str().unwrap_or("");
                    let value = pair[1].as_str().unwrap_or("");
                    if !key.is_empty() {
                        trx.put_link(key, value);
                    }
                }
            }
            let run_input = plan["input"].clone();
            let app_async = app_for_handler.clone();
            let _ = async_once(move || {
                let msg = json!({
                    "key": "runVm",
                    "input": run_input,
                });
                app_async.tools().vmm().vm_callback(&msg.to_string());
            });
            Ok(json!({"vmId": vm_id}))
        },
    )
}

fn stop_program_entity(app: Arc<dyn ICore>) -> Arc<dyn ISecureAction> {
    let app_for_handler = app.clone();
    build_secure_action::<RunProgramEntityInput, _>(
        app,
        "/programs/stopEntity",
        user_guard(),
        move |state: Arc<dyn IState>, input: RunProgramEntityInput| -> Result<Value> {
            let trx = state.trx();
            let program_id = if input.program_id.is_empty() {
                input.machine_id.clone()
            } else {
                input.program_id.clone()
            };
            if !trx.has_obj("Program", &program_id) {
                return Err(anyhow!("program does not exist"));
            }
            let program = Program {
                id: program_id.clone(),
                ..Default::default()
            }
            .pull(&*trx);
            let entity = Entity {
                program_id: program.id.clone(),
                entity_id: input.entity_id.clone(),
                ..Default::default()
            }
            .pull(&*trx);
            if entity.entity_id.is_empty() {
                return Err(anyhow!("entity does not exist"));
            }
            let entity_type = normalize_entity_type(&entity.entity_type);
            // Authorize against the recorded program owner (no app_id).
            let owner_machine = resolve_program_owner_machine(&*trx, &program);
            if owner_machine.owner_id != state.info().user_id() {
                return Err(anyhow!("you are not owner of this program"));
            }
            let vm_id = input.vm_id.clone();
            trx.del_key(&format!("link::VmStatus::{}", vm_id));
            trx.del_key(&format!("link::VmStartedAt::{}", vm_id));
            trx.del_key(&format!(
                "link::VmInstance::{}::{}::{}",
                program.id, input.entity_id, vm_id
            ));
            trx.del_key(&format!("link::VmBilling::{}", vm_id));
            trx.del_json(&format!("Json::VmBilling::{}", vm_id), "payment");
            trx.del_key(&format!(
                "link::vmStandaloneImageName::{}::{}",
                program.id, input.entity_id
            ));
            trx.del_key(&format!(
                "link::vmStandaloneContainerName::{}::{}",
                program.id, input.entity_id
            ));
            // Ask the runtime's plugin how to stop this entity; the plan
            // resolves per-runtime state links (e.g. a recorded container
            // name) and fails when a required link is missing.
            let ctx = json!({
                "machineId": input.machine_id,
                "programId": program.id,
                "entityId": entity.entity_id,
                "vmId": vm_id,
            });
            let stop_input =
                build_stop_input_from_plan(&app_for_handler, &*trx, &entity_type, &ctx, true)?;
            trx.del_key(&format!(
                "link::VmContainerName::{}::{}::{}",
                program.id, input.entity_id, vm_id
            ));
            let msg = json!({
                "key": "terminateVm",
                "input": stop_input,
            });
            app_for_handler.tools().vmm().vm_callback(&msg.to_string());
            Ok(json!({}))
        },
    )
}

/// `/programs/deleteEntity` — permanently destroy one VM instance of a
/// deployed program entity.
///
/// This is the destructive sibling of `/programs/stopEntity`. Stop suspends:
/// the instance can be resumed and its persistent volume survives, which is
/// why a stopped VM still leaves its container, sandbox or disk behind.
/// Delete asks the runtime to destroy the instance and everything it owns,
/// then drops every state link that described it — the VM cannot come back.
///
/// Access control matches `stopEntity` exactly: only the recorded owner of the
/// program may delete its instances. The owner is resolved from the program
/// record (never the deprecated `app_id` pointer), so a creature that merely
/// knows a vm id cannot destroy somebody else's VM.
fn delete_program_entity(app: Arc<dyn ICore>) -> Arc<dyn ISecureAction> {
    let app_for_handler = app.clone();
    build_secure_action::<RunProgramEntityInput, _>(
        app,
        "/programs/deleteEntity",
        user_guard(),
        move |state: Arc<dyn IState>, input: RunProgramEntityInput| -> Result<Value> {
            let trx = state.trx();
            let program_id = if input.program_id.is_empty() {
                input.machine_id.clone()
            } else {
                input.program_id.clone()
            };
            if !trx.has_obj("Program", &program_id) {
                return Err(anyhow!("program does not exist"));
            }
            let program = Program {
                id: program_id.clone(),
                ..Default::default()
            }
            .pull(&*trx);
            let entity = Entity {
                program_id: program.id.clone(),
                entity_id: input.entity_id.clone(),
                ..Default::default()
            }
            .pull(&*trx);
            if entity.entity_id.is_empty() {
                return Err(anyhow!("entity does not exist"));
            }
            let entity_type = normalize_entity_type(&entity.entity_type);
            let owner_machine = resolve_program_owner_machine(&*trx, &program);
            if owner_machine.owner_id != state.info().user_id() {
                return Err(anyhow!("you are not owner of this program"));
            }
            let vm_id = input.vm_id.trim().to_string();
            if vm_id.is_empty() {
                return Err(anyhow!("vmId is required"));
            }
            // The instance must belong to THIS entity. Without the check a
            // caller who owns one program could pass any vm id and have the
            // runtime destroy an instance that program never launched.
            let instance_key = format!(
                "VmInstance::{}::{}::{}",
                program.id, input.entity_id, vm_id
            );
            if trx.get_link(&instance_key).is_empty() {
                return Err(anyhow!("vm does not belong to this entity"));
            }

            // Build the delete packet BEFORE dropping the links: the plan
            // resolves per-runtime state (a recorded container name, a sandbox
            // id) that only exists while those links do.
            let ctx = json!({
                "machineId": input.machine_id,
                "programId": program.id,
                "entityId": entity.entity_id,
                "vmId": vm_id,
            });
            let delete_input =
                build_delete_input_from_plan(&app_for_handler, &*trx, &entity_type, &ctx)?;
            let result = app_for_handler
                .tools()
                .vmm()
                .delete_vm_instance(&Value::Object(delete_input));
            if !result["ok"].as_bool().unwrap_or(false) {
                let err = result["error"]
                    .as_str()
                    .unwrap_or("vm delete failed")
                    .to_string();
                return Err(anyhow!(err));
            }

            // The runtime destroyed it, so forget it. Doing this only after a
            // successful delete keeps a failed destroy visible (and retryable)
            // instead of leaving a live VM with no record.
            trx.del_key(&format!("link::VmStatus::{}", vm_id));
            trx.del_key(&format!("link::VmStartedAt::{}", vm_id));
            trx.del_key(&instance_key);
            trx.del_key(&format!("link::VmBilling::{}", vm_id));
            trx.del_json(&format!("Json::VmBilling::{}", vm_id), "payment");
            trx.del_key(&format!("link::vmDistributed::{}", vm_id));
            trx.del_key(&format!("link::VmOwnerProgram::{}", vm_id));
            trx.del_key(&format!(
                "link::VmContainerName::{}::{}::{}",
                program.id, input.entity_id, vm_id
            ));
            trx.del_key(&format!(
                "link::vmStandaloneImageName::{}::{}",
                program.id, input.entity_id
            ));
            trx.del_key(&format!(
                "link::vmStandaloneContainerName::{}::{}",
                program.id, input.entity_id
            ));
            Ok(json!({"ok": true, "vmId": vm_id, "result": result}))
        },
    )
}

fn read_vm_logs(app: Arc<dyn ICore>) -> Arc<dyn ISecureAction> {
    let app_for_handler = app.clone();
    build_secure_action::<ReadVmLogsInput, _>(
        app,
        "/machines/readVmLogs",
        user_guard(),
        move |state: Arc<dyn IState>, input: ReadVmLogsInput| -> Result<Value> {
            let trx = state.trx();
            let mut owner_user_id = String::new();
            if let Ok(links) = trx.get_links_list("VmInstance::", -1, -1, &[]) {
                let suffix = format!("::{}", input.vm_id);
                for link in links {
                    if !link.ends_with(&suffix) {
                        continue;
                    }
                    let parts: Vec<&str> = link.split("::").collect();
                    if parts.len() < 4 {
                        continue;
                    }
                    let program = Program {
                        id: parts[1].to_string(),
                        ..Default::default()
                    }
                    .pull(&*trx);
                    if program.id.is_empty() {
                        break;
                    }
                    owner_user_id = resolve_program_owner_machine(&*trx, &program).owner_id;
                    break;
                }
            }
            if owner_user_id.is_empty() {
                // Docker image BUILD output is emitted before any VM exists,
                // under the runtime's node-wide "main" stream with log type
                // "build" (see the docker plugin's emit_vm_log calls) — there
                // is no VmInstance link to anchor ownership to, so build
                // streams stay readable by any authenticated user, matching
                // how the runtime records them (one shared stream, no
                // per-program isolation).
                if input.log_type != "build" {
                    return Err(anyhow!("vm not found"));
                }
            } else if owner_user_id != state.info().user_id() {
                return Err(anyhow!("you are not owner of this vm"));
            }
            let count = if input.count <= 0 { 100 } else { input.count };
            let offset = if input.offset < 0 { 0 } else { input.offset };
            let logs = app_for_handler.tools().storage().read_vm_logs(
                &input.vm_id,
                &input.log_type,
                offset,
                count,
            );
            Ok(json!({"logs": logs}))
        },
    )
}

/// List the VM instances recorded for one program entity and ask its runtime
/// plugin for the current process/container state.
fn list_entity_vms(app: Arc<dyn ICore>) -> Arc<dyn ISecureAction> {
    build_secure_action::<RunProgramEntityInput, _>(
        app,
        "/machines/listEntityVms",
        user_guard(),
        move |state: Arc<dyn IState>, input: RunProgramEntityInput| -> Result<Value> {
            let trx = state.trx();
            let program_id = if input.program_id.is_empty() {
                input.machine_id.clone()
            } else {
                input.program_id.clone()
            };
            if !trx.has_obj("Program", &program_id) {
                return Err(anyhow!("program does not exist"));
            }
            let program = Program {
                id: program_id.clone(),
                ..Default::default()
            }
            .pull(&*trx);
            let owner_machine = resolve_program_owner_machine(&*trx, &program);
            if owner_machine.owner_id != state.info().user_id() {
                return Err(anyhow!("you are not owner of this program"));
            }
            let entity = Entity {
                program_id: program.id.clone(),
                entity_id: input.entity_id.clone(),
                ..Default::default()
            }
            .pull(&*trx);
            if entity.entity_id.is_empty() {
                return Err(anyhow!("entity does not exist"));
            }

            let entity_type = normalize_entity_type(&entity.entity_type);
            let plugin = caspar_vm_sdk::registry::get(&entity_type)
                .ok_or_else(|| anyhow!("invalid entity type"))?;
            let prefix = format!("VmInstance::{}::{}::", program.id, input.entity_id);
            let links = trx.get_links_list(&prefix, -1, -1, &[]).unwrap_or_default();
            let mut instances: Vec<Value> = Vec::new();

            for link in links {
                let vm_id = link.strip_prefix(&prefix).unwrap_or(&link).to_string();
                if vm_id.is_empty() {
                    continue;
                }
                let recorded_status = trx.get_link(&format!("VmStatus::{}", vm_id));
                let started_at = trx
                    .get_link(&format!("VmStartedAt::{}", vm_id))
                    .parse::<i64>()
                    .unwrap_or(0);
                let container_name = trx.get_link(&format!(
                    "VmContainerName::{}::{}::{}",
                    program.id, input.entity_id, vm_id
                ));
                let probe = plugin.status_vm(&json!({
                    "runtime": entity_type,
                    "machineId": program.id,
                    "programId": program.id,
                    "entityId": input.entity_id,
                    "vmId": vm_id,
                    "containerName": container_name,
                }));
                let (status, running, detail) = match probe {
                    Ok(value) => {
                        let status = value["status"].as_str().unwrap_or("unknown").to_string();
                        let running = value["running"].as_bool().unwrap_or(status == "running");
                        (status, running, value)
                    }
                    Err(error) => (
                        if recorded_status.is_empty() {
                            "stopped"
                        } else {
                            "unknown"
                        }
                        .to_string(),
                        false,
                        json!({"error": error}),
                    ),
                };
                instances.push(json!({
                    "vmId": vm_id,
                    "status": status,
                    "running": running,
                    "recordedStatus": recorded_status,
                    "startedAt": started_at,
                    "detail": detail,
                }));
            }

            instances.sort_by(|a, b| {
                b["startedAt"]
                    .as_i64()
                    .unwrap_or(0)
                    .cmp(&a["startedAt"].as_i64().unwrap_or(0))
            });
            Ok(json!({
                "programId": program.id,
                "entityId": input.entity_id,
                "runtime": entity_type,
                "instances": instances,
            }))
        },
    )
}

fn open_vm_terminal(app: Arc<dyn ICore>) -> Arc<dyn ISecureAction> {
    build_secure_action::<VmTerminalInput, _>(
        app,
        "/machines/openVmTerminal",
        user_guard(),
        move |state: Arc<dyn IState>, input: VmTerminalInput| -> Result<Value> {
            let trx = state.trx();
            if !trx.has_obj("Program", &input.creature_id) {
                return Err(anyhow!("program does not exist"));
            }
            let program = Program {
                id: input.creature_id.clone(),
                ..Default::default()
            }
            .pull(&*trx);
            let owner_machine = resolve_program_owner_machine(&*trx, &program);
            if owner_machine.owner_id != state.info().user_id() {
                return Err(anyhow!("you are not owner of this creature"));
            }
            trx.put_link(
                &format!(
                    "VmTerminal::{}::{}::{}",
                    input.creature_id,
                    input.vm_id,
                    state.info().user_id()
                ),
                "true",
            );
            Ok(json!({"terminal": "on"}))
        },
    )
}

fn close_vm_terminal(app: Arc<dyn ICore>) -> Arc<dyn ISecureAction> {
    build_secure_action::<VmTerminalInput, _>(
        app,
        "/machines/closeVmTerminal",
        user_guard(),
        move |state: Arc<dyn IState>, input: VmTerminalInput| -> Result<Value> {
            let trx = state.trx();
            if !trx.has_obj("Program", &input.creature_id) {
                return Err(anyhow!("program does not exist"));
            }
            let program = Program {
                id: input.creature_id.clone(),
                ..Default::default()
            }
            .pull(&*trx);
            let owner_machine = resolve_program_owner_machine(&*trx, &program);
            if owner_machine.owner_id != state.info().user_id() {
                return Err(anyhow!("you are not owner of this creature"));
            }
            trx.del_key(&format!(
                "link::VmTerminal::{}::{}::{}",
                input.creature_id,
                input.vm_id,
                state.info().user_id()
            ));
            Ok(json!({"terminal": "off"}))
        },
    )
}

fn read_machine_builds(app: Arc<dyn ICore>) -> Arc<dyn ISecureAction> {
    build_secure_action::<MachineBuildsInput, _>(
        app,
        "/machines/readMachineBuilds",
        user_guard(),
        move |state: Arc<dyn IState>, input: MachineBuildsInput| -> Result<Value> {
            let prefix = format!("VmBuilds::{}::", input.machine_id);
            let builds =
                state
                    .trx()
                    .get_links_list(&prefix, input.offset, input.count, &[false])?;
            Ok(json!({"buildsList": builds}))
        },
    )
}

/// Record (or clear) an entity's custom VM gateway route, reconciling any route
/// a previous deploy of the same entity left behind. `creature_id` is the
/// program's owning creature (whose username the route is reached by),
/// `gateway_path` the normalized prefix (empty ⇒ the entity exposes no custom
/// route) and `gateway_vm_id` an optional specific instance to target.
pub(crate) fn register_gateway_route(
    trx: &dyn ITrx,
    creature_id: &str,
    program_id: &str,
    entity_id: &str,
    gateway_path: &str,
    gateway_vm_id: &str,
    runtime: &str,
) {
    use crate::drivers::vmm::http_route;

    let rev_key = http_route::route_rev_link_key(program_id, entity_id);
    // Drop a stale route from a prior deploy whose path changed or was removed.
    let previous = trx.get_link(&rev_key);
    if let Some((prev_creature, prev_path)) = previous.split_once("::") {
        let unchanged =
            !gateway_path.is_empty() && prev_creature == creature_id && prev_path == gateway_path;
        if !unchanged {
            trx.del_key(&format!(
                "link::{}",
                http_route::route_link_key(prev_creature, prev_path)
            ));
            if gateway_path.is_empty() {
                trx.del_key(&format!("link::{}", rev_key));
            }
        }
    }
    if gateway_path.is_empty() || creature_id.is_empty() {
        return;
    }
    trx.put_link(
        &http_route::route_link_key(creature_id, gateway_path),
        &http_route::encode_target(program_id, entity_id, gateway_vm_id, runtime),
    );
    trx.put_link(&rev_key, &format!("{}::{}", creature_id, gateway_path));
    // Alias the bare local part of the owning creature's username → its id, so a
    // request may address the route by the short name (`/m-tool-github/…`) as
    // well as by the full username or the numeric id. Best-effort: only when the
    // creature record + username resolve on this node.
    let username = Creature {
        id: creature_id.to_string(),
        ..Default::default()
    }
    .pull(trx)
    .username;
    let local_part = http_route::username_local_part(&username);
    if !local_part.is_empty() && local_part != creature_id {
        trx.put_link(&http_route::route_alias_link_key(local_part), creature_id);
    }
}

fn deploy(app: Arc<dyn ICore>) -> Arc<dyn ISecureAction> {
    let app_for_handler = app.clone();
    build_secure_action::<DeployInput, _>(
        app,
        "/programs/deploy",
        user_guard(),
        move |state: Arc<dyn IState>, input: DeployInput| -> Result<Value> {
            let trx = state.trx();
            let program_id = input.machine_id.clone();
            if !trx.has_obj("Program", &program_id) {
                return Err(anyhow!("program not found"));
            }
            let program = Program {
                id: program_id.clone(),
                ..Default::default()
            }
            .pull(&*trx);
            // Authorize against the recorded program owner (no app_id).
            let owner_machine = resolve_program_owner_machine(&*trx, &program);
            if owner_machine.owner_id != state.info().user_id() {
                return Err(anyhow!("access to vm denied"));
            }
            let entity_type = normalize_entity_type(&input.entity_type);
            // Proxy entities are non-runnable: the deploy stores the payload
            // as the entity's data file plus a target descriptor. Signals to
            // the entity are forwarded to the target with the file attached
            // and responses are routed back through the proxy (see
            // drivers::vmm::proxy). No plugin, no build, no billing.
            if entity_type == crate::drivers::vmm::proxy::PROXY_RUNTIME_KEY {
                let config = crate::drivers::vmm::proxy::config_from_metadata(|k| {
                    input.metadata.get(k).cloned()
                })
                .map_err(|e| anyhow!(e))?;
                let data = base64::engine::general_purpose::STANDARD
                    .decode(&input.payload)
                    .map_err(|e| anyhow!("{}", e))?;
                let build_folder_path = format!(
                    "{}{}{}/entities/{}",
                    app_for_handler.tools().storage().storage_root(),
                    PLUGINS_TEMPLATE_NAME,
                    program.id,
                    input.entity_id
                );
                app_for_handler.tools().file().save_data_to_global_storage(
                    &build_folder_path,
                    &data,
                    "proxy.data",
                    true,
                )?;
                crate::drivers::vmm::proxy::record_proxy_entity(
                    &*trx,
                    &program.id,
                    &input.entity_id,
                    &format!("{}/proxy.data", build_folder_path),
                    &config,
                );
                // Register the signal listener so the proxy entity actually
                // receives (and forwards) signals addressed to this program.
                program.push(&*trx);
                app_for_handler.tools().vmm().assign(&program.id);
                return Ok(json!({
                    "proxy": true,
                    "entityId": input.entity_id,
                    "entityType": crate::drivers::vmm::proxy::PROXY_RUNTIME_KEY,
                    "target": config.to_value(),
                }));
            }
            // The runtime's plugin declares how its entities deploy: the
            // primary file name, whether extra files are accepted, whether a
            // build must follow, and whether entity links are recorded. An
            // unknown runtime means no plugin was compiled into this node.
            let spec = app_for_handler
                .tools()
                .vmm()
                .runtime_deploy_spec(&entity_type)
                .ok_or_else(|| {
                    anyhow!(
                        "invalid entityType, expected one of {}",
                        app_for_handler.tools().vmm().supported_runtimes().join("|")
                    )
                })?;
            let primary_file_name = spec["entityFileName"]
                .as_str()
                .unwrap_or("module.wasm")
                .to_string();
            let accepts_extra_files = spec["acceptsExtraFiles"].as_bool().unwrap_or(false);
            let build_on_deploy = spec["buildOnDeploy"].as_bool().unwrap_or(false);
            let set_entity_links = spec["setEntityLinksOnDeploy"].as_bool().unwrap_or(false);
            let data = base64::engine::general_purpose::STANDARD
                .decode(&input.payload)
                .map_err(|e| anyhow!("{}", e))?;
            // The developer chooses the deployment scope: "cluster" ships the
            // creature to every instance of this origin (edge execution +
            // raft-propagated state), "local" pins it to this instance and
            // keeps all of its VM state out of the consensus.
            let distributed = input.wants_distribution() && crate::drivers::cluster::is_active();
            let distribution_label = if distributed { "cluster" } else { "local" };
            let mut entity_model = Entity {
                program_id: program.id.clone(),
                entity_id: input.entity_id.clone(),
                entity_type: entity_type.clone(),
                image_name: input.entity_id.clone(),
            };
            let vm_id = Uuid::new_v4().to_string();
            let build_folder_path = format!(
                "{}{}{}/entities/{}",
                app_for_handler.tools().storage().storage_root(),
                PLUGINS_TEMPLATE_NAME,
                program.id,
                input.entity_id
            );
            app_for_handler.tools().file().save_data_to_global_storage(
                &build_folder_path,
                &data,
                &primary_file_name,
                true,
            )?;
            // Artifact files shipped to the other instances on a distributed
            // deploy (base64 as received; primary file first).
            let mut artifact_files: Vec<(String, String)> =
                vec![(primary_file_name.clone(), input.payload.clone())];
            if accepts_extra_files {
                let mut files: HashMap<String, Value> = HashMap::new();
                if let Some(files_raw) = input.metadata.get("files") {
                    match files_raw {
                        Value::Object(o) => {
                            files = o.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
                        }
                        Value::Null => {}
                        _ => return Err(anyhow!("files is not map")),
                    }
                }
                for (k, v) in &files {
                    let data_str = match v {
                        Value::String(s) => s.clone(),
                        _ => return Err(anyhow!("file bytecode not string")),
                    };
                    let raw = base64::engine::general_purpose::STANDARD
                        .decode(&data_str)
                        .map_err(|e| anyhow!("{}", e))?;
                    app_for_handler.tools().file().save_data_to_global_storage(
                        &build_folder_path,
                        &raw,
                        k,
                        true,
                    )?;
                    artifact_files.push((k.clone(), data_str));
                }
            }
            if set_entity_links {
                trx.put_link(
                    &format!("vmEntityPath::{}::{}", program.id, input.entity_id),
                    &format!("{}/{}", build_folder_path, primary_file_name),
                );
                trx.put_link(
                    &format!("vmEntityType::{}::{}", program.id, input.entity_id),
                    &entity_type,
                );
            }
            // Custom VM gateway route: the deployer may bind this entity's HTTP
            // server to a friendly `/{creatureUsername}/{gatewayPath…}` path.
            // Stored on chain keyed by the owning creature + normalized prefix
            // so it replicates with the deploy and the ingress can resolve it.
            let gateway_path = crate::drivers::vmm::http_route::normalize_path(
                input
                    .metadata
                    .get("gatewayPath")
                    .and_then(|v| v.as_str())
                    .unwrap_or(""),
            );
            let gateway_vm_id = input
                .metadata
                .get("gatewayVmId")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .trim()
                .to_string();
            register_gateway_route(
                &*trx,
                &program.machine_id,
                &program.id,
                &input.entity_id,
                &gateway_path,
                &gateway_vm_id,
                &entity_type,
            );
            if input.downloadable {
                // Downloadable entities (front-end scripts executed on the
                // client) are served at any time via /programs/downloadEntity.
                trx.put_link(
                    &format!("vmEntityDownloadable::{}::{}", program.id, input.entity_id),
                    &format!("{}/{}", build_folder_path, primary_file_name),
                );
            }
            if build_on_deploy {
                let build_id = Uuid::new_v4().to_string();
                trx.put_link(&format!("VmBuilds::{}::{}", vm_id, build_id), "true");
                let app_async = app_for_handler.clone();
                let mid = program.id.clone();
                let eid = input.entity_id.clone();
                let path = build_folder_path.clone();
                let etype = entity_type.clone();
                let _ = async_once(move || {
                    app_async
                        .tools()
                        .vmm()
                        .build_vm_image(&mid, &eid, &path, &etype);
                });
            }
            // Register the machine signal listener for every runtime. Without
            // this, signals addressed to a creature's program are dropped (no
            // listener) and every creature-to-creature signal silently times
            // out.
            program.push(&*trx);
            app_for_handler.tools().vmm().assign(&program.id);
            entity_model.entity_type = entity_type.clone();
            entity_model.push(&*trx);
            // Persist the chosen scope; the VMM consults these links to decide
            // whether a VM's state mutations enter the raft consensus.
            trx.put_link(
                &format!("vmDistribution::{}", program.id),
                distribution_label,
            );
            trx.put_link(
                &format!("vmDistribution::{}::{}", program.id, input.entity_id),
                distribution_label,
            );
            if distributed {
                crate::drivers::cluster::propose_deploy(
                    crate::drivers::cluster::command::DeployArtifact {
                        program_id: program.id.clone(),
                        entity_id: input.entity_id.clone(),
                        entity_type: entity_type.clone(),
                        machine_id: program.machine_id.clone(),
                        runtime: program.runtime.clone(),
                        path: program.path.clone(),
                        comment: program.comment.clone(),
                        primary_file_name: primary_file_name.clone(),
                        files: artifact_files,
                        set_entity_links,
                        build_on_deploy,
                        gateway_route: gateway_path.clone(),
                        gateway_vm_id: gateway_vm_id.clone(),
                    },
                );
            }
            let mut result = serde_json::to_value(PlugInput::default())?;
            if let Value::Object(map) = &mut result {
                map.insert("distribution".into(), json!(distribution_label));
            }
            Ok(result)
        },
    )
}

/// `/programs/downloadEntity` — hand a deployed downloadable entity's file to
/// the caller (base64). This is how front-end apps deployed as entities are
/// fetched and executed on the client side at any time.
fn download_entity(app: Arc<dyn ICore>) -> Arc<dyn ISecureAction> {
    build_secure_action::<DownloadEntityInput, _>(
        app,
        "/programs/downloadEntity",
        user_guard(),
        move |state: Arc<dyn IState>, input: DownloadEntityInput| -> Result<Value> {
            let trx = state.trx();
            let program_id = if input.program_id.is_empty() {
                input.machine_id.clone()
            } else {
                input.program_id.clone()
            };
            if program_id.is_empty() || input.entity_id.is_empty() {
                return Err(anyhow!("programId and entityId are required"));
            }
            let path = trx.get_link(&format!(
                "vmEntityDownloadable::{}::{}",
                program_id, input.entity_id
            ));
            if path.is_empty() {
                return Err(anyhow!("entity is not downloadable"));
            }
            let entity = Entity {
                program_id: program_id.clone(),
                entity_id: input.entity_id.clone(),
                ..Default::default()
            }
            .pull(&*trx);
            let bytes =
                std::fs::read(&path).map_err(|e| anyhow!("entity file unavailable: {}", e))?;
            Ok(json!({
                "programId": program_id,
                "entityId": input.entity_id,
                "entityType": entity.entity_type,
                "payload": base64::engine::general_purpose::STANDARD.encode(bytes),
            }))
        },
    )
}

fn list_machines(app: Arc<dyn ICore>) -> Arc<dyn ISecureAction> {
    build_secure_action::<ListInput, _>(
        app,
        "/machines/list",
        user_guard(),
        move |state: Arc<dyn IState>, input: ListInput| -> Result<Value> {
            let trx = state.trx();
            // "Machines" are just creatures of type "machine".
            let mut type_filter: HashMap<String, String> = HashMap::new();
            type_filter.insert("type".to_string(), "machine".to_string());
            let machines = Creature::all_query(&*trx, input.offset, input.count, &type_filter)?;
            let mut result: Vec<Map<String, Value>> = Vec::new();
            for machine in machines {
                let profile = trx
                    .get_json(
                        &format!("CreatMeta::{}", machine.id),
                        "metadata.public.profile",
                    )
                    .ok();
                let mut row: Map<String, Value> = Map::new();
                row.insert("id".into(), json!(machine.id));
                row.insert("chainId".into(), json!(machine.chain_id));
                row.insert("username".into(), json!(machine.username));
                row.insert("ownerId".into(), json!(machine.owner_id));
                row.insert("programsCount".into(), json!(machine.machines_count));
                if let Some(p) = profile {
                    row.insert(
                        "title".into(),
                        p.get("title").cloned().unwrap_or_else(|| json!("untitled")),
                    );
                    row.insert(
                        "avatar".into(),
                        p.get("avatar").cloned().unwrap_or_else(|| json!("")),
                    );
                    row.insert(
                        "desc".into(),
                        p.get("desc").cloned().unwrap_or_else(|| json!("")),
                    );
                } else {
                    row.insert("title".into(), json!("untitled"));
                    row.insert("avatar".into(), json!(""));
                    row.insert("desc".into(), json!(""));
                }
                result.push(row);
            }
            Ok(json!({"machines": result}))
        },
    )
}

fn list_programs(app: Arc<dyn ICore>) -> Arc<dyn ISecureAction> {
    build_secure_action::<ListInput, _>(
        app,
        "/programs/list",
        user_guard(),
        move |state: Arc<dyn IState>, input: ListInput| -> Result<Value> {
            let machines = Program::all(&*state.trx(), input.offset, input.count)?;
            Ok(json!({"machines": machines}))
        },
    )
}

fn list_program_machines(app: Arc<dyn ICore>) -> Arc<dyn ISecureAction> {
    build_secure_action::<ListAppMachsInput, _>(
        app,
        "/machines/listProgramMachines",
        user_guard(),
        move |state: Arc<dyn IState>, input: ListAppMachsInput| -> Result<Value> {
            let trx = state.trx();
            let prefix = format!("machinePrograms::{}::", input.app_id);
            let users = Creature::list(&*trx, &prefix, &HashMap::new())?;
            let programs = Program::list(&*trx, &prefix)?;
            let mut program_by_machine_id: HashMap<String, Program> = HashMap::new();
            for program in programs {
                program_by_machine_id.insert(program.id.clone(), program);
            }
            let mut result: Vec<Map<String, Value>> = Vec::new();
            for user in users {
                let mut row: Map<String, Value> = Map::new();
                row.insert("id".into(), json!(user.id));
                row.insert("type".into(), json!(user.type_name));
                row.insert("username".into(), json!(user.username));
                let comment = program_by_machine_id
                    .get(&user.id)
                    .map(|p| p.comment.clone())
                    .unwrap_or_default();
                row.insert("comment".into(), json!(comment));
                result.push(row);
            }
            Ok(json!({"machines": result}))
        },
    )
}

/// Mirror of Go's `Install`: walk the existing programs, hand each one to the
/// VMM and replay any pending vm-alarm. The 15-second billing ticker is
/// intentionally not started here — see the module doc-comment.
fn install_program_bootstrap(app: Arc<dyn ICore>) {
    let app_for_closure = app.clone();
    app.modify_state(
        true,
        Box::new(move |trx: &dyn ITrx| {
            let programs = Program::all(trx, -1, -1)?;
            for program in programs {
                let is_proxy = normalize_entity_type(&program.runtime)
                    == crate::drivers::vmm::proxy::PROXY_RUNTIME_KEY;
                let is_vm = app_for_closure
                    .tools()
                    .vmm()
                    .is_supported_runtime(&program.runtime);
                // Proxy programs are non-runnable, but their signal listener must
                // still be re-registered on restart so forwarded prompts reach
                // them; only real VM runtimes additionally replay a pending alarm.
                if is_proxy || is_vm {
                    app_for_closure.tools().vmm().assign(&program.id);
                }
                if is_vm {
                    let store_id = trx.get_link(&format!("vmAlarmStoreId::{}", program.id));
                    if !store_id.is_empty() {
                        let app_async = app_for_closure.clone();
                        let machine_id = program.id.clone();
                        let store_id_clone = store_id.clone();
                        let alarm_time_raw = trx.get_link(&format!("vmAlarmTime::{}", machine_id));
                        let alarm_data = trx.get_link(&format!("vmAlarmData::{}", machine_id));
                        // The entity to re-run ("main" for creatures); older alarms
                        // without the link fall back to "main" so a wasm creature's
                        // module still resolves after a restart.
                        let mut alarm_entity = trx.get_link(&format!("vmAlarmEntity::{}", machine_id));
                        if alarm_entity.is_empty() {
                            alarm_entity = "main".to_string();
                        }
                        let _ = async_once(move || {
                            let t = alarm_time_raw.parse::<i64>().unwrap_or(0);
                            let ct = chrono::Utc::now().timestamp_millis();
                            if t > ct {
                                std::thread::sleep(std::time::Duration::from_millis(
                                    (t - ct) as u64,
                                ));
                            }
                            // Note: the original Go path cleared the alarm
                            // links inside a state-modifying closure; here we
                            // call into vmm directly. The links are reaped on
                            // the next bootstrap pass if they remain stale.
                            if app_async
                                .tools()
                                .security()
                                .has_access_to_store(&machine_id, &store_id_clone)
                            {
                                app_async.tools().vmm().run_vm_entity(
                                    &machine_id,
                                    &store_id_clone,
                                    &alarm_data,
                                    &alarm_entity,
                                );
                            }
                        });
                    }
                }
                let prefix = format!("hasaccess::{}::", program.id);
                let store_ids = trx.get_links_list(&prefix, -1, -1, &[]).unwrap_or_default();
                for store_id in store_ids {
                    let bare = store_id
                        .strip_prefix(&prefix)
                        .unwrap_or(&store_id)
                        .to_string();
                    app_for_closure
                        .tools()
                        .signaler()
                        .join_group(&bare, &program.id);
                }
            }
            Ok(())
        }),
    );
}

/// Plug every program action into the actor.
pub fn install(app: Arc<dyn ICore>) {
    let actor = app.actor();
    let handlers: Vec<Arc<dyn ISecureAction>> = vec![
        create_program(app.clone()),
        delete_program(app.clone()),
        update_program(app.clone()),
        run_program_entity(app.clone()),
        stop_program_entity(app.clone()),
        delete_program_entity(app.clone()),
        list_entity_vms(app.clone()),
        read_vm_logs(app.clone()),
        open_vm_terminal(app.clone()),
        close_vm_terminal(app.clone()),
        read_machine_builds(app.clone()),
        deploy(app.clone()),
        download_entity(app.clone()),
        list_machines(app.clone()),
        list_programs(app.clone()),
        list_program_machines(app.clone()),
    ];
    for h in handlers {
        actor.inject_secure_action(h);
    }
    install_program_bootstrap(app.clone());
    // Reap proxy-entity correlation records whose response never arrived,
    // so silent target failures cannot leak records into the database.
    crate::drivers::vmm::proxy::start_correlation_reaper(app.clone());
    let billing_lock = Arc::new(Mutex::new(-1i64));
    let app_bg = app.clone();
    std::thread::spawn(move || loop {
        charge_running_standalone_vms_if_needed(&app_bg, &billing_lock);
        std::thread::sleep(std::time::Duration::from_secs(15));
    });
}
