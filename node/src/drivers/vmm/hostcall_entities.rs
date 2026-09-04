//! Translation of `drivers/vmm/hostcall_entities.go` — the CRUD-style host
//! calls available to wasm / javascript / fire runtimes.

use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use base64::Engine;
use serde_json::{json, Map, Value};

use crate::models::core::{StateClosure, ICore};
use crate::models::info::IInfo;
use crate::models::transaction::ITrx;
use crate::models::state::IState;
use crate::core::actor::model::base::info::Info as BaseInfo;
use crate::shell::api::model::{Creature, Entity, Program, Store, StorePermissions};

use super::driver::{check_bool, check_i64, check_str, normalize_runtime, Vmm};

fn number_from_input(input: &Value, key: &str, def: i64) -> i64 {
    check_i64(input, key, def)
}

fn bool_from_input(input: &Value, key: &str, def: bool) -> bool {
    check_bool(input, key, def)
}

impl Vmm {
    pub(crate) fn handle_creature_crud(&self, op: &str, input: &Value, req_id: i64) -> (String, i64) {
        match op {
            "create" => {
                let mut id = check_str(input, "id", "");
                if id.is_empty() {
                    id = self.gen_id("vm.creature");
                }
                let typ = check_str(input, "type", "agent");
                let username = check_str(input, "username", "");
                let public_key = check_str(input, "publicKey", "");
                let chain_id = check_str(input, "chainId", "main");
                let subchain_id = check_str(input, "subchainId", "main");
                let owner_id = check_str(input, "ownerId", "");
                let balance = number_from_input(input, "balance", 0);
                let id_owned = id.clone();
                let owner_owned = owner_id.clone();
                self.app.modify_state(
                    false,
                    Box::new(move |t: &dyn ITrx| {
                        let c = Creature {
                            id: id_owned.clone(),
                            type_name: typ.clone(),
                            username: username.clone(),
                            public_key: public_key.clone(),
                            chain_id: chain_id.clone(),
                            subchain_id: subchain_id.clone(),
                            owner_id: owner_owned.clone(),
                            balance,
                            ..Default::default()
                        };
                        c.push(t);
                        if !owner_owned.is_empty() {
                            t.put_link(&format!("ownerof::{}::{}", owner_owned, id_owned), "true");
                        }
                        Ok(())
                    }),
                );
                (format!("{{\"ok\":true,\"id\":\"{}\"}}", id), req_id)
            }
            "update" => {
                let id = check_str(input, "id", "");
                if id.is_empty() {
                    return (r#"{"ok":false,"error":"id is required"}"#.into(), req_id);
                }
                let input_owned = input.clone();
                let id_owned = id.clone();
                self.app.modify_state(
                    false,
                    Box::new(move |t: &dyn ITrx| {
                        let mut c = Creature {
                            id: id_owned.clone(),
                            ..Default::default()
                        }
                        .pull(t);
                        if c.id.is_empty() {
                            c.id = id_owned.clone();
                        }
                        if let Some(v) = input_owned.get("type").and_then(Value::as_str) {
                            c.type_name = v.to_string();
                        }
                        if let Some(v) = input_owned.get("username").and_then(Value::as_str) {
                            c.username = v.to_string();
                        }
                        if let Some(v) = input_owned.get("publicKey").and_then(Value::as_str) {
                            c.public_key = v.to_string();
                        }
                        if let Some(v) = input_owned.get("chainId").and_then(Value::as_str) {
                            c.chain_id = v.to_string();
                        }
                        if let Some(v) = input_owned.get("subchainId").and_then(Value::as_str) {
                            c.subchain_id = v.to_string();
                        }
                        if let Some(v) = input_owned.get("ownerId").and_then(Value::as_str) {
                            c.owner_id = v.to_string();
                        }
                        if let Some(v) = input_owned.get("balance").and_then(Value::as_f64) {
                            c.balance = v as i64;
                        }
                        c.push(t);
                        Ok(())
                    }),
                );
                (format!("{{\"ok\":true,\"id\":\"{}\"}}", id), req_id)
            }
            "delete" => {
                let mut id = check_str(input, "id", "");
                if id.is_empty() {
                    id = check_str(input, "creatureId", "");
                }
                if id.is_empty() {
                    id = check_str(input, "userId", "");
                }
                if id.is_empty() {
                    return (r#"{"ok":false,"error":"id is required"}"#.into(), req_id);
                }
                let id_owned = id.clone();
                self.app.modify_state(
                    false,
                    Box::new(move |t: &dyn ITrx| {
                        let c = Creature {
                            id: id_owned.clone(),
                            ..Default::default()
                        }
                        .pull(t);
                        if !c.username.is_empty() {
                            t.del_index("Creature", "username", "id", &c.username);
                        }
                        let email = t.get_link(&format!("UserIdToEmail::{}", id_owned));
                        if !email.is_empty() {
                            t.del_key(&format!("link::UserEmailToId::{}", email));
                        }
                        t.del_key(&format!("link::UserIdToEmail::{}", id_owned));
                        t.del_key(&format!("link::UserPrivateKey::{}", id_owned));
                        let stores = Store::list(
                            t,
                            &format!("hasaccess::{}::", id_owned),
                            false,
                            &HashMap::new(),
                            &HashMap::new(),
                            -1,
                            -1,
                        )
                        .unwrap_or_default();
                        for store in stores {
                            t.del_key(&format!("link::onaccess::{}::{}", store.id, id_owned));
                            t.del_key(&format!("link::hasaccess::{}::{}", id_owned, store.id));
                            t.del_key(&format!("link::creatorof::{}::{}", id_owned, store.id));
                            let prefix = format!("onaccess::{}::", store.id);
                            let remaining =
                                t.get_links_list(&prefix, -1, -1, &[]).unwrap_or_default();
                            let others = remaining.iter().any(|k| {
                                let member = k.strip_prefix(&prefix).unwrap_or(k);
                                !member.is_empty() && member != id_owned
                            });
                            if !others {
                                store.delete(t);
                                t.del_key(&format!("Json::StoreMeta::{}::metadata", store.id));
                            }
                        }
                        for col in [
                            "|",
                            "type",
                            "username",
                            "publicKey",
                            "chainId",
                            "subchainId",
                            "ownerId",
                            "balance",
                        ] {
                            t.del_key(&format!("obj::Creature::{}::{}", id_owned, col));
                        }
                        Ok(())
                    }),
                );
                (format!("{{\"ok\":true,\"id\":\"{}\"}}", id), req_id)
            }
            "get" => {
                let id = check_str(input, "id", "");
                if id.is_empty() {
                    return (r#"{"ok":false,"error":"id is required"}"#.into(), req_id);
                }
                let slot = Arc::new(Mutex::new(Creature::default()));
                let slot_clone = slot.clone();
                let id_owned = id.clone();
                self.app.modify_state(
                    true,
                    Box::new(move |t: &dyn ITrx| {
                        let c = Creature {
                            id: id_owned.clone(),
                            ..Default::default()
                        }
                        .pull(t);
                        *slot_clone.lock().unwrap() = c;
                        Ok(())
                    }),
                );
                let creature = slot.lock().unwrap().clone();
                let out = json!({"ok": true, "creature": creature});
                (serde_json::to_string(&out).unwrap_or_default(), req_id)
            }
            "list" => {
                let offset = number_from_input(input, "offset", 0);
                let mut count = number_from_input(input, "count", 100);
                if count <= 0 {
                    count = 100;
                }
                let slot: Arc<Mutex<Vec<Creature>>> = Arc::new(Mutex::new(Vec::new()));
                let slot_clone = slot.clone();
                self.app.modify_state(
                    true,
                    Box::new(move |t: &dyn ITrx| {
                        if let Ok(list) = Creature::all(t, offset, count) {
                            *slot_clone.lock().unwrap() = list;
                        }
                        Ok(())
                    }),
                );
                let creatures = slot.lock().unwrap().clone();
                let out = json!({"ok": true, "creatures": creatures});
                (serde_json::to_string(&out).unwrap_or_default(), req_id)
            }
            _ => (r#"{"ok":false,"error":"unsupported creature op"}"#.into(), req_id),
        }
    }

    /// Program CRUD reads/updates for VM host calls — the read side that the
    /// existing `createProgram`/`deleteProgram` lacked. A program's metadata is
    /// stored under `ProgMeta::{id}` (e.g. an MCP manifest); `listByMachine`
    /// enumerates the programs of a machine creature via the
    /// `machinePrograms::{machineId}::{programId}` links.
    pub(crate) fn handle_program_crud(&self, op: &str, input: &Value, req_id: i64) -> (String, i64) {
        match op {
            "create" => {
                let mut machine_id = check_str(input, "machineId", "");
                if machine_id.is_empty() {
                    machine_id = check_str(input, "appId", "");
                }
                if machine_id.is_empty() {
                    return (r#"{"ok":false,"error":"machineId is required"}"#.into(), req_id);
                }
                let mut id = check_str(input, "programId", "");
                if id.is_empty() {
                    id = check_str(input, "id", "");
                }
                if id.is_empty() {
                    id = self.gen_id("vm.program");
                }
                let runtime = check_str(input, "runtime", "wasm");
                let path = check_str(input, "path", "");
                let comment = check_str(input, "comment", "");
                let metadata = input
                    .get("metadata")
                    .cloned()
                    .unwrap_or_else(|| Value::Object(Map::new()));
                let id_owned = id.clone();
                let machine_id_owned = machine_id.clone();
                let create_error = Arc::new(Mutex::new(String::new()));
                let create_error_for_state = create_error.clone();
                self.app.modify_state(
                    false,
                    Box::new(move |t: &dyn ITrx| {
                        if t.has_obj("Program", &id_owned) {
                            *create_error_for_state.lock().unwrap() =
                                "program already exists".to_string();
                            return Ok(());
                        }
                        let mut machine = Creature {
                            id: machine_id_owned.clone(),
                            ..Default::default()
                        }
                        .pull(t);
                        if machine.id.is_empty() {
                            machine.id = machine_id_owned.clone();
                        }
                        machine.machines_count += 1;
                        machine.push(t);
                        Program {
                            id: id_owned.clone(),
                            machine_id: machine_id_owned.clone(),
                            runtime: runtime.clone(),
                            path: path.clone(),
                            comment: comment.clone(),
                        }
                        .push(t);
                        let _ = t.put_json(
                            &format!("ProgMeta::{}", id_owned),
                            "metadata",
                            &metadata,
                            true,
                        );
                        t.put_link(
                            &format!("machinePrograms::{}::{}", machine_id_owned, id_owned),
                            "true",
                        );
                        Ok(())
                    }),
                );
                let error = create_error.lock().unwrap().clone();
                if !error.is_empty() {
                    return (json!({"ok": false, "error": error}).to_string(), req_id);
                }
                (
                    format!(
                        "{{\"ok\":true,\"programId\":\"{}\",\"machineId\":\"{}\"}}",
                        id, machine_id
                    ),
                    req_id,
                )
            }
            "delete" => {
                let mut id = check_str(input, "programId", "");
                if id.is_empty() {
                    id = check_str(input, "id", "");
                }
                if id.is_empty() {
                    return (r#"{"ok":false,"error":"programId is required"}"#.into(), req_id);
                }
                let id_owned = id.clone();
                self.app.modify_state(
                    false,
                    Box::new(move |t: &dyn ITrx| {
                        let program = Program {
                            id: id_owned.clone(),
                            ..Default::default()
                        }
                        .pull(t);
                        if !program.machine_id.is_empty() {
                            let mut machine = Creature {
                                id: program.machine_id.clone(),
                                ..Default::default()
                            }
                            .pull(t);
                            machine.machines_count -= 1;
                            machine.push(t);
                            t.del_key(&format!(
                                "link::machinePrograms::{}::{}",
                                program.machine_id, id_owned
                            ));
                        }
                        t.del_index("Program", "id", "programId", &id_owned);
                        Ok(())
                    }),
                );
                (format!("{{\"ok\":true,\"programId\":\"{}\"}}", id), req_id)
            }
            "get" => {
                let mut id = check_str(input, "programId", "");
                if id.is_empty() {
                    id = check_str(input, "id", "");
                }
                if id.is_empty() {
                    return (r#"{"ok":false,"error":"programId is required"}"#.into(), req_id);
                }
                let prog_slot = Arc::new(Mutex::new(Program::default()));
                let meta_slot: Arc<Mutex<Map<String, Value>>> = Arc::new(Mutex::new(Map::new()));
                let ps = prog_slot.clone();
                let ms = meta_slot.clone();
                let id_owned = id.clone();
                self.app.modify_state(
                    true,
                    Box::new(move |t: &dyn ITrx| {
                        let p = Program { id: id_owned.clone(), ..Default::default() }.pull(t);
                        *ps.lock().unwrap() = p;
                        if let Ok(m) = t.get_json(&format!("ProgMeta::{}", id_owned), "metadata") {
                            *ms.lock().unwrap() = m;
                        }
                        Ok(())
                    }),
                );
                let program = prog_slot.lock().unwrap().clone();
                let metadata = Value::Object(meta_slot.lock().unwrap().clone());
                let out = json!({"ok": true, "program": program, "metadata": metadata});
                (serde_json::to_string(&out).unwrap_or_default(), req_id)
            }
            "list" => {
                let offset = number_from_input(input, "offset", 0);
                let mut count = number_from_input(input, "count", 100);
                if count <= 0 {
                    count = 100;
                }
                let slot: Arc<Mutex<Vec<Program>>> = Arc::new(Mutex::new(Vec::new()));
                let sc = slot.clone();
                self.app.modify_state(
                    true,
                    Box::new(move |t: &dyn ITrx| {
                        if let Ok(list) = Program::all(t, offset, count) {
                            *sc.lock().unwrap() = list;
                        }
                        Ok(())
                    }),
                );
                let programs = slot.lock().unwrap().clone();
                (serde_json::to_string(&json!({"ok": true, "programs": programs})).unwrap_or_default(), req_id)
            }
            "listByMachine" => {
                let mut machine_id = check_str(input, "machineId", "");
                if machine_id.is_empty() {
                    machine_id = check_str(input, "appId", "");
                }
                if machine_id.is_empty() {
                    return (r#"{"ok":false,"error":"machineId is required"}"#.into(), req_id);
                }
                let slot: Arc<Mutex<Vec<Program>>> = Arc::new(Mutex::new(Vec::new()));
                let sc = slot.clone();
                let mid = machine_id.clone();
                self.app.modify_state(
                    true,
                    Box::new(move |t: &dyn ITrx| {
                        let prefix = format!("machinePrograms::{}::", mid);
                        if let Ok(list) = Program::list(t, &prefix) {
                            *sc.lock().unwrap() = list;
                        }
                        Ok(())
                    }),
                );
                let programs = slot.lock().unwrap().clone();
                (serde_json::to_string(&json!({"ok": true, "programs": programs})).unwrap_or_default(), req_id)
            }
            "update" => {
                let mut id = check_str(input, "programId", "");
                if id.is_empty() {
                    id = check_str(input, "id", "");
                }
                if id.is_empty() {
                    return (r#"{"ok":false,"error":"programId is required"}"#.into(), req_id);
                }
                let input_owned = input.clone();
                let id_owned = id.clone();
                self.app.modify_state(
                    false,
                    Box::new(move |t: &dyn ITrx| {
                        let mut p = Program { id: id_owned.clone(), ..Default::default() }.pull(t);
                        if p.id.is_empty() {
                            p.id = id_owned.clone();
                        }
                        if let Some(v) = input_owned.get("comment").and_then(Value::as_str) {
                            p.comment = v.to_string();
                        }
                        if let Some(v) = input_owned.get("runtime").and_then(Value::as_str) {
                            p.runtime = v.to_string();
                        }
                        if let Some(v) = input_owned.get("path").and_then(Value::as_str) {
                            p.path = v.to_string();
                        }
                        p.push(t);
                        if let Some(md) = input_owned.get("metadata") {
                            let _ = t.put_json(&format!("ProgMeta::{}", id_owned), "metadata", md, true);
                        }
                        Ok(())
                    }),
                );
                (format!("{{\"ok\":true,\"id\":\"{}\"}}", id), req_id)
            }
            _ => (r#"{"ok":false,"error":"unsupported program op"}"#.into(), req_id),
        }
    }

    /// Host-call deploy of a program entity (`deployEntity`), the VM-side
    /// twin of the shell's `/programs/deploy`. Supports every registered VM
    /// runtime plus the pseudo-runtime `"proxy"`: a proxy entity stores the
    /// payload as a non-runnable data file and a target descriptor; incoming
    /// signals are forwarded to the target with the data attached (see
    /// `drivers::vmm::proxy`). Host-call deploys are always local — cluster
    /// distribution stays a shell-API concern.
    pub(crate) fn handle_deploy_entity(&self, input: &Value, req_id: i64) -> (String, i64) {
        use crate::drivers::vmm::proxy;

        let mut program_id = check_str(input, "programId", "");
        if program_id.is_empty() {
            program_id = check_str(input, "machineId", "");
        }
        if program_id.is_empty() {
            return (r#"{"ok":false,"error":"programId is required"}"#.into(), req_id);
        }
        let entity_id = check_str(input, "entityId", "");
        if entity_id.is_empty() {
            return (r#"{"ok":false,"error":"entityId is required"}"#.into(), req_id);
        }
        let entity_type = normalize_runtime(&check_str(input, "entityType", "wasm"));
        let payload_b64 = check_str(input, "payload", "");
        let data = match base64::engine::general_purpose::STANDARD.decode(&payload_b64) {
            Ok(d) => d,
            Err(e) => {
                return (
                    format!("{{\"ok\":false,\"error\":\"invalid payload base64: {}\"}}", e),
                    req_id,
                )
            }
        };
        let metadata = input
            .get("metadata")
            .cloned()
            .unwrap_or_else(|| Value::Object(Map::new()));
        let build_folder_path = format!(
            "{}/machines/{}/entities/{}",
            self.storage.storage_root(),
            program_id,
            entity_id
        );

        if entity_type == proxy::PROXY_RUNTIME_KEY {
            let config = match proxy::config_from_metadata(|k| metadata.get(k).cloned()) {
                Ok(c) => c,
                Err(e) => {
                    return (
                        format!("{{\"ok\":false,\"error\":\"{}\"}}", e.replace('"', "\\\"")),
                        req_id,
                    )
                }
            };
            if let Err(e) =
                self.file
                    .save_data_to_global_storage(&build_folder_path, &data, "proxy.data", true)
            {
                return (
                    format!("{{\"ok\":false,\"error\":\"{}\"}}", e.to_string().replace('"', "\\\"")),
                    req_id,
                );
            }
            let data_path = format!("{}/proxy.data", build_folder_path);
            let program_id_owned = program_id.clone();
            let entity_id_owned = entity_id.clone();
            let config_owned = config.clone();
            self.app.modify_state(
                false,
                Box::new(move |t: &dyn ITrx| {
                    // Make sure the program record exists so its signal
                    // listener resolves; a bare proxy program needs no
                    // runtime of its own.
                    let mut program = Program {
                        id: program_id_owned.clone(),
                        ..Default::default()
                    }
                    .pull(t);
                    if program.id.is_empty() {
                        program.id = program_id_owned.clone();
                    }
                    program.push(t);
                    proxy::record_proxy_entity(
                        t,
                        &program_id_owned,
                        &entity_id_owned,
                        &data_path,
                        &config_owned,
                    );
                    Ok(())
                }),
            );
            self.app.tools().vmm().assign(&program_id);
            let out = json!({
                "ok": true,
                "programId": program_id,
                "entityId": entity_id,
                "entityType": proxy::PROXY_RUNTIME_KEY,
                "proxy": config.to_value(),
            });
            return (serde_json::to_string(&out).unwrap_or_default(), req_id);
        }

        let spec = match caspar_vm_sdk::registry::get(&entity_type) {
            Some(p) => p.meta().deploy_spec_json(),
            None => {
                return (
                    format!(
                        "{{\"ok\":false,\"error\":\"invalid entityType, expected proxy or one of {}\"}}",
                        caspar_vm_sdk::registry::keys().join("|")
                    ),
                    req_id,
                )
            }
        };
        let primary_file_name = spec["entityFileName"]
            .as_str()
            .unwrap_or("module.wasm")
            .to_string();
        let accepts_extra_files = spec["acceptsExtraFiles"].as_bool().unwrap_or(false);
        let build_on_deploy = spec["buildOnDeploy"].as_bool().unwrap_or(false);
        let set_entity_links = spec["setEntityLinksOnDeploy"].as_bool().unwrap_or(false);
        if let Err(e) = self.file.save_data_to_global_storage(
            &build_folder_path,
            &data,
            &primary_file_name,
            true,
        ) {
            return (
                format!("{{\"ok\":false,\"error\":\"{}\"}}", e.to_string().replace('"', "\\\"")),
                req_id,
            );
        }
        if accepts_extra_files {
            if let Some(files) = metadata.get("files").and_then(Value::as_object) {
                for (name, raw) in files {
                    let Some(content_b64) = raw.as_str() else {
                        return (
                            r#"{"ok":false,"error":"file bytecode not string"}"#.into(),
                            req_id,
                        );
                    };
                    let bytes = match base64::engine::general_purpose::STANDARD
                        .decode(content_b64)
                    {
                        Ok(b) => b,
                        Err(e) => {
                            return (
                                format!(
                                    "{{\"ok\":false,\"error\":\"invalid file base64: {}\"}}",
                                    e
                                ),
                                req_id,
                            )
                        }
                    };
                    if let Err(e) = self.file.save_data_to_global_storage(
                        &build_folder_path,
                        &bytes,
                        name,
                        true,
                    ) {
                        return (
                            format!(
                                "{{\"ok\":false,\"error\":\"{}\"}}",
                                e.to_string().replace('"', "\\\"")
                            ),
                            req_id,
                        );
                    }
                }
            }
        }
        let downloadable = check_bool(input, "downloadable", false);
        let program_id_owned = program_id.clone();
        let entity_id_owned = entity_id.clone();
        let entity_type_owned = entity_type.clone();
        let primary_owned = primary_file_name.clone();
        let folder_owned = build_folder_path.clone();
        self.app.modify_state(
            false,
            Box::new(move |t: &dyn ITrx| {
                let mut program = Program {
                    id: program_id_owned.clone(),
                    ..Default::default()
                }
                .pull(t);
                if program.id.is_empty() {
                    program.id = program_id_owned.clone();
                }
                if program.runtime.is_empty() {
                    program.runtime = entity_type_owned.clone();
                }
                program.push(t);
                Entity {
                    program_id: program_id_owned.clone(),
                    entity_id: entity_id_owned.clone(),
                    entity_type: entity_type_owned.clone(),
                    image_name: entity_id_owned.clone(),
                }
                .push(t);
                if set_entity_links {
                    t.put_link(
                        &format!("vmEntityPath::{}::{}", program_id_owned, entity_id_owned),
                        &format!("{}/{}", folder_owned, primary_owned),
                    );
                    t.put_link(
                        &format!("vmEntityType::{}::{}", program_id_owned, entity_id_owned),
                        &entity_type_owned,
                    );
                }
                if downloadable {
                    // Downloadable entities (e.g. a front-end script executed
                    // client-side) are fetched by clients at any time via
                    // /programs/downloadEntity.
                    t.put_link(
                        &format!(
                            "vmEntityDownloadable::{}::{}",
                            program_id_owned, entity_id_owned
                        ),
                        &format!("{}/{}", folder_owned, primary_owned),
                    );
                }
                Ok(())
            }),
        );
        self.app.tools().vmm().assign(&program_id);
        if build_on_deploy {
            self.app
                .tools()
                .vmm()
                .build_vm_image(&program_id, &entity_id, &build_folder_path, &entity_type);
        }
        let out = json!({
            "ok": true,
            "programId": program_id,
            "entityId": entity_id,
            "entityType": entity_type,
            "entityPath": format!("{}/{}", build_folder_path, primary_file_name),
            "downloadable": downloadable,
        });
        (serde_json::to_string(&out).unwrap_or_default(), req_id)
    }

    pub(crate) fn handle_resource_store_crud(
        &self,
        op: &str,
        input: &Value,
        req_id: i64,
    ) -> (String, i64) {
        match op {
            "create" | "update" => {
                let mut store_id = check_str(input, "storeId", "");
                let machine_id = check_str(input, "machineId", "");
                if store_id.is_empty() {
                    store_id = self.gen_id("vm.store");
                }
                let name = check_str(input, "name", &store_id);
                let metadata = input
                    .get("metadata")
                    .cloned()
                    .unwrap_or_else(|| Value::Object(Map::new()));
                let store_id_owned = store_id.clone();
                let machine_id_owned = machine_id.clone();
                let name_owned = name.clone();
                self.app.modify_state(
                    false,
                    Box::new(move |t: &dyn ITrx| {
                        let key = format!("Json::VmResourceStore::{}", store_id_owned);
                        t.put_json(&key, "metadata", &metadata, true)?;
                        let core_meta = json!({
                            "id": store_id_owned.clone(),
                            "name": name_owned.clone(),
                            "machineId": machine_id_owned.clone(),
                        });
                        t.put_json(&key, "core", &core_meta, true)?;
                        if !machine_id_owned.is_empty() {
                            t.put_link(
                                &format!("vmOwnedStore::{}::{}", machine_id_owned, store_id_owned),
                                "true",
                            );
                        }
                        Ok(())
                    }),
                );
                (format!("{{\"ok\":true,\"storeId\":\"{}\"}}", store_id), req_id)
            }
            "delete" => {
                let store_id = check_str(input, "storeId", "");
                if store_id.is_empty() {
                    return (r#"{"ok":false,"error":"storeId is required"}"#.into(), req_id);
                }
                let machine_id = check_str(input, "machineId", "");
                let store_id_owned = store_id.clone();
                let machine_id_owned = machine_id.clone();
                self.app.modify_state(
                    false,
                    Box::new(move |t: &dyn ITrx| {
                        t.del_key(&format!("Json::VmResourceStore::{}::metadata", store_id_owned));
                        t.del_key(&format!("Json::VmResourceStore::{}::core", store_id_owned));
                        if !machine_id_owned.is_empty() {
                            t.del_key(&format!(
                                "link::vmOwnedStore::{}::{}",
                                machine_id_owned, store_id_owned
                            ));
                        }
                        Ok(())
                    }),
                );
                (format!("{{\"ok\":true,\"storeId\":\"{}\"}}", store_id), req_id)
            }
            "get" => {
                let store_id = check_str(input, "storeId", "");
                if store_id.is_empty() {
                    return (r#"{"ok":false,"error":"storeId is required"}"#.into(), req_id);
                }
                let core_slot: Arc<Mutex<Map<String, Value>>> = Arc::new(Mutex::new(Map::new()));
                let meta_slot: Arc<Mutex<Map<String, Value>>> = Arc::new(Mutex::new(Map::new()));
                let core_clone = core_slot.clone();
                let meta_clone = meta_slot.clone();
                let key = format!("Json::VmResourceStore::{}", store_id);
                self.app.modify_state(
                    true,
                    Box::new(move |t: &dyn ITrx| {
                        if let Ok(c) = t.get_json(&key, "core") {
                            *core_clone.lock().unwrap() = c;
                        }
                        if let Ok(m) = t.get_json(&key, "metadata") {
                            *meta_clone.lock().unwrap() = m;
                        }
                        Ok(())
                    }),
                );
                let core = Value::Object(core_slot.lock().unwrap().clone());
                let meta = Value::Object(meta_slot.lock().unwrap().clone());
                let out = json!({"ok": true, "store": {"core": core, "metadata": meta}});
                (serde_json::to_string(&out).unwrap_or_default(), req_id)
            }
            "list" => {
                let machine_id = check_str(input, "machineId", "");
                let prefix = if machine_id.is_empty() {
                    "Json::VmResourceStore::".to_string()
                } else {
                    format!("link::vmOwnedStore::{}::", machine_id)
                };
                let slot: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
                let slot_clone = slot.clone();
                self.app.modify_state(
                    true,
                    Box::new(move |t: &dyn ITrx| {
                        *slot_clone.lock().unwrap() = t.get_by_prefix(&prefix);
                        Ok(())
                    }),
                );
                let stores = slot.lock().unwrap().clone();
                let out = json!({"ok": true, "stores": stores});
                (serde_json::to_string(&out).unwrap_or_default(), req_id)
            }
            _ => (r#"{"ok":false,"error":"unsupported store op"}"#.into(), req_id),
        }
    }

    pub(crate) fn handle_resource_entity_create(
        &self,
        input: &Value,
        req_id: i64,
    ) -> (String, i64) {
        let store_id = check_str(input, "storeId", "");
        if store_id.is_empty() {
            return (r#"{"ok":false,"error":"storeId is required"}"#.into(), req_id);
        }
        let entity_type = check_str(input, "entityType", "default");
        let mut entity_id = check_str(input, "entityId", "");
        if entity_id.is_empty() {
            entity_id = self.gen_id("vm.entity");
        }
        let payload = input.get("payload").cloned().unwrap_or_else(|| json!({}));
        let data = check_str(input, "data", "");
        let base_path = PathBuf::from(&self.storage_root)
            .join("vm_stores")
            .join(&store_id)
            .join(&entity_type);
        let _ = fs::create_dir_all(&base_path);
        let path = base_path.join(format!("{}.json", entity_id));
        let _ = fs::write(&path, data.as_bytes());
        let path_str = path.to_string_lossy().into_owned();
        let store_id_owned = store_id.clone();
        let entity_id_owned = entity_id.clone();
        let entity_type_owned = entity_type.clone();
        let path_owned = path_str.clone();
        self.app.modify_state(
            false,
            Box::new(move |t: &dyn ITrx| {
                let key = format!(
                    "Json::VmResourceEntity::{}::{}::{}",
                    store_id_owned, entity_type_owned, entity_id_owned
                );
                t.put_json(&key, "payload", &payload, true)?;
                let meta = json!({
                    "id": entity_id_owned.clone(),
                    "storeId": store_id_owned.clone(),
                    "entityType": entity_type_owned.clone(),
                    "path": path_owned.clone(),
                });
                t.put_json(&key, "meta", &meta, true)?;
                Ok(())
            }),
        );
        (
            format!("{{\"ok\":true,\"entityId\":\"{}\",\"path\":\"{}\"}}", entity_id, path_str),
            req_id,
        )
    }

    pub(crate) fn handle_resource_entity_delete(
        &self,
        input: &Value,
        req_id: i64,
    ) -> (String, i64) {
        let store_id = check_str(input, "storeId", "");
        if store_id.is_empty() {
            return (r#"{"ok":false,"error":"storeId is required"}"#.into(), req_id);
        }
        let entity_type = check_str(input, "entityType", "default");
        let entity_id = check_str(input, "entityId", "");
        if entity_id.is_empty() {
            return (r#"{"ok":false,"error":"entityId is required"}"#.into(), req_id);
        }
        let path = PathBuf::from(&self.storage_root)
            .join("vm_stores")
            .join(&store_id)
            .join(&entity_type)
            .join(format!("{}.json", entity_id));
        let _ = fs::remove_file(&path);
        let store_id_owned = store_id.clone();
        let entity_id_owned = entity_id.clone();
        let entity_type_owned = entity_type.clone();
        self.app.modify_state(
            false,
            Box::new(move |t: &dyn ITrx| {
                let key = format!(
                    "Json::VmResourceEntity::{}::{}::{}",
                    store_id_owned, entity_type_owned, entity_id_owned
                );
                t.del_key(&format!("{}::payload", key));
                t.del_key(&format!("{}::meta", key));
                Ok(())
            }),
        );
        (r#"{"ok":true}"#.into(), req_id)
    }

    pub(crate) fn handle_vm_chain_request(
        &self,
        op: &str,
        input: &Value,
        req_id: i64,
    ) -> (String, i64) {
        let store_id = check_str(input, "storeId", "");
        let mut receivers: HashMap<String, HashMap<String, bool>> = HashMap::new();
        receivers.insert("*".to_string(), HashMap::new());

        match op {
            "createWorkchain" => {
                let chain_id = self
                    .app
                    .tools()
                    .network()
                    .chain()
                    .create_work_chain(&store_id);
                let payload =
                    json!({"op": op, "chainId": chain_id, "storeId": store_id});
                let payload_bytes = serde_json::to_vec(&payload).unwrap_or_default();
                let owner_id = self.app.owner_id();
                self.app.globe().send_typed_message_on_chain(
                    "main",
                    "chains/vm/request",
                    "vm.chain",
                    payload_bytes,
                    "",
                    &owner_id,
                    receivers,
                    "",
                    &store_id,
                    None,
                    None,
                );
                (format!("{{\"ok\":true,\"chainId\":\"{}\"}}", chain_id), req_id)
            }
            "createSubchain" => {
                let work_chain_id = check_str(input, "workChainId", "");
                let mut subchain_id = check_str(input, "subchainId", "");
                let peers: Vec<String> = input
                    .get("peers")
                    .and_then(Value::as_array)
                    .map(|arr| {
                        arr.iter()
                            .filter_map(|v| v.as_str().map(str::to_string))
                            .collect()
                    })
                    .unwrap_or_default();
                subchain_id = self.app.tools().network().chain().create_shard_chain(
                    &work_chain_id,
                    &subchain_id,
                    peers.clone(),
                );
                let payload = json!({
                    "op": op,
                    "workChainId": work_chain_id,
                    "subchainId": subchain_id,
                    "peers": peers,
                });
                let owner_id = self.app.owner_id();
                self.app.globe().send_typed_message_on_chain(
                    "main",
                    "chains/vm/request",
                    "vm.chain",
                    serde_json::to_vec(&payload).unwrap_or_default(),
                    "",
                    &owner_id,
                    receivers,
                    "",
                    &store_id,
                    None,
                    None,
                );
                (
                    format!(
                        "{{\"ok\":true,\"workChainId\":\"{}\",\"subchainId\":\"{}\"}}",
                        work_chain_id, subchain_id
                    ),
                    req_id,
                )
            }
            op if op.starts_with("delete") => {
                let payload = json!({"op": op, "input": input.clone()});
                let owner_id = self.app.owner_id();
                self.app.globe().send_typed_message_on_chain(
                    "main",
                    "chains/vm/request",
                    "vm.chain",
                    serde_json::to_vec(&payload).unwrap_or_default(),
                    "",
                    &owner_id,
                    receivers,
                    "",
                    &store_id,
                    None,
                    None,
                );
                (r#"{"ok":true,"notified":true}"#.into(), req_id)
            }
            _ => (r#"{"ok":false,"error":"unsupported chain op"}"#.into(), req_id),
        }
    }

    /// Run a registered shell action on behalf of a VM.
    ///
    /// `caller` is the node's own answer to "which creature is calling" —
    /// resolved from the VM context the docker gateway verifies (or the id an
    /// in-process runtime stamps on the packet), never from anything the guest
    /// can write. It is the ONLY identity a creature may act as.
    ///
    /// Two modes, and the difference matters:
    ///   * `asSelf: true` — act as the calling creature, through the applet
    ///     signature path. This is how a container performs an action that is
    ///     genuinely its own (uploading media it produced), with the record
    ///     showing the creature that did it.
    ///   * otherwise — the caller names the identity, which for an anonymous
    ///     action (`/creatures/login`) is deliberately empty. A creature cannot
    ///     reach a *user's* authenticated action this way: without a real
    ///     signature the guard refuses it.
    pub(crate) fn handle_exec_shell_action(
        &self,
        caller: &str,
        input: &Value,
        req_id: i64,
    ) -> (String, i64) {
        let path = check_str(input, "path", "");
        if path.is_empty() {
            return (r#"{"ok":false,"error":"path is required"}"#.into(), req_id);
        }
        let as_self = input
            .get("asSelf")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let owner = self.app.owner_id();
        let (user_id, signature) = if as_self {
            let caller = caller.trim();
            if caller.is_empty() {
                // Acting as an unidentifiable creature would mean acting as
                // nobody in particular — refuse instead of falling back to an
                // identity the caller did not earn.
                return (
                    r#"{"ok":false,"error":"caller identity unavailable"}"#.into(),
                    req_id,
                );
            }
            (caller.to_string(), "#appletsign".to_string())
        } else {
            (
                check_str(input, "userId", &owner),
                check_str(input, "signature", ""),
            )
        };
        let store_id = check_str(input, "storeId", "");
        let packet_id = check_str(input, "packetId", "");

        let secure = match self.app.actor().fetch_secure_action(&path) {
            Some(s) => s,
            None => return (r#"{"ok":false,"error":"action not found"}"#.into(), req_id),
        };
        let payload_raw = input.get("payload").cloned().unwrap_or(Value::Null);
        let payload_bytes = serde_json::to_vec(&payload_raw).unwrap_or_default();
        let parsed = match secure.parse_input("tcp", payload_raw) {
            Ok(p) => p,
            Err(e) => return (
                format!("{{\"ok\":false,\"error\":\"{}\"}}", e.to_string().replace('"', "\\\"")),
                req_id,
            ),
        };
        let result_slot: Arc<Mutex<(i64, Value, Option<String>)>> =
            Arc::new(Mutex::new((0, Value::Null, None)));
        let result_clone = result_slot.clone();
        let user_id_owned = user_id.clone();
        let packet_id_owned = packet_id.clone();
        let signature_owned = signature.clone();
        let payload_bytes_owned = payload_bytes.clone();
        let secure_clone = secure.clone();
        let ip_addr = self.app.ip_addr();
        let info: Arc<dyn IInfo> = Arc::new(BaseInfo::new(&user_id, &store_id));
        let closure: StateClosure = Box::new(move |_state: Arc<dyn IState>| {
            let r = secure_clone.securely_act(
                &user_id_owned,
                &packet_id_owned,
                &payload_bytes_owned,
                &signature_owned,
                parsed.clone(),
                &ip_addr,
                &[true],
            );
            match r {
                Ok((sc, v)) => *result_clone.lock().unwrap() = (sc, v, None),
                Err(e) => *result_clone.lock().unwrap() = (0, Value::Null, Some(format!("{}", e))),
            }
            Ok(())
        });
        self.app.modify_state_securly(false, info, closure);

        let (status, value, err) = {
            let guard = result_slot.lock().unwrap();
            (guard.0, guard.1.clone(), guard.2.clone())
        };
        match err {
            Some(e) => (
                format!("{{\"ok\":false,\"statusCode\":{},\"error\":\"{}\"}}", status, e.replace('"', "\\\"")),
                req_id,
            ),
            None => {
                let out = json!({"ok": true, "statusCode": status, "result": value});
                (serde_json::to_string(&out).unwrap_or_default(), req_id)
            }
        }
    }

    pub(crate) fn handle_micro_host_action(
        &self,
        op: &str,
        input: &Value,
        req_id: i64,
    ) -> (String, i64) {
        match op {
            "genId" => {
                let source = check_str(input, "source", "vm.micro");
                let id = self.gen_id(&source);
                (format!("{{\"ok\":true,\"id\":\"{}\"}}", id), req_id)
            }
            "getLink" => {
                let key = check_str(input, "key", "");
                if key.is_empty() {
                    return (r#"{"ok":false,"error":"key is required"}"#.into(), req_id);
                }
                let val_slot = Arc::new(Mutex::new(String::new()));
                let val_clone = val_slot.clone();
                let key_owned = key.clone();
                self.app.modify_state(
                    true,
                    Box::new(move |t: &dyn ITrx| {
                        *val_clone.lock().unwrap() = t.get_link(&key_owned);
                        Ok(())
                    }),
                );
                let v = val_slot.lock().unwrap().clone();
                (format!("{{\"ok\":true,\"value\":\"{}\"}}", v.replace('"', "\\\"")), req_id)
            }
            "delKey" => {
                let key = check_str(input, "key", "");
                if key.is_empty() {
                    return (r#"{"ok":false,"error":"key is required"}"#.into(), req_id);
                }
                if key.starts_with("link::") {
                    return (
                        r#"{"ok":false,"error":"link modifications are not allowed via delKey"}"#.into(),
                        req_id,
                    );
                }
                let key_owned = key.clone();
                self.app.modify_state(
                    false,
                    Box::new(move |t: &dyn ITrx| {
                        t.del_key(&key_owned);
                        Ok(())
                    }),
                );
                (r#"{"ok":true}"#.into(), req_id)
            }
            "createAccess" | "updateAccess" => {
                let user_id = check_str(input, "userId", "");
                if user_id.is_empty() {
                    return (r#"{"ok":false,"error":"userId is required"}"#.into(), req_id);
                }
                let store_id = check_str(input, "storeId", "");
                if store_id.is_empty() {
                    return (r#"{"ok":false,"error":"storeId is required"}"#.into(), req_id);
                }
                // A grant states what the member may do. It is required, not
                // defaulted: a caller that forgets it would otherwise mint a
                // member who can do nothing (and look like a bug in signalling)
                // or, worse under a different default, a viewer who can post.
                let permissions: Vec<String> = input
                    .get("permissions")
                    .and_then(Value::as_array)
                    .map(|a| {
                        a.iter()
                            .filter_map(|v| v.as_str().map(|s| s.to_string()))
                            .collect()
                    })
                    .unwrap_or_default();
                if permissions.is_empty() {
                    return (
                        r#"{"ok":false,"error":"permissions is required (e.g. [\"read\",\"signal\"])"}"#
                            .into(),
                        req_id,
                    );
                }
                let perms = StorePermissions::from_list(&permissions);
                if perms.is_empty() {
                    return (
                        r#"{"ok":false,"error":"permissions names no known flag"}"#.into(),
                        req_id,
                    );
                }
                let user_id_owned = user_id.clone();
                let store_id_owned = store_id.clone();
                let encoded = perms.encode();
                self.app.modify_state(
                    false,
                    Box::new(move |t: &dyn ITrx| {
                        t.put_link(&format!("onaccess::{}::{}", store_id_owned, user_id_owned), &encoded);
                        t.put_link(&format!("hasaccess::{}::{}", user_id_owned, store_id_owned), "true");
                        Ok(())
                    }),
                );
                let out = json!({"ok": true, "permissions": perms});
                (serde_json::to_string(&out).unwrap_or_default(), req_id)
            }
            "deleteAccess" => {
                let user_id = check_str(input, "userId", "");
                if user_id.is_empty() {
                    return (r#"{"ok":false,"error":"userId is required"}"#.into(), req_id);
                }
                let store_id = check_str(input, "storeId", "");
                if store_id.is_empty() {
                    return (r#"{"ok":false,"error":"storeId is required"}"#.into(), req_id);
                }
                let user_id_owned = user_id.clone();
                let store_id_owned = store_id.clone();
                self.app.modify_state(
                    false,
                    Box::new(move |t: &dyn ITrx| {
                        t.del_key(&format!("link::onaccess::{}::{}", store_id_owned, user_id_owned));
                        t.del_key(&format!("link::hasaccess::{}::{}", user_id_owned, store_id_owned));
                        Ok(())
                    }),
                );
                (r#"{"ok":true}"#.into(), req_id)
            }
            "getJson" => {
                let key = check_str(input, "key", "");
                if key.is_empty() {
                    return (r#"{"ok":false,"error":"key is required"}"#.into(), req_id);
                }
                let path = check_str(input, "path", "");
                let slot: Arc<Mutex<Map<String, Value>>> = Arc::new(Mutex::new(Map::new()));
                let slot_clone = slot.clone();
                let key_owned = key.clone();
                let path_owned = path.clone();
                self.app.modify_state(
                    true,
                    Box::new(move |t: &dyn ITrx| {
                        if let Ok(m) = t.get_json(&key_owned, &path_owned) {
                            *slot_clone.lock().unwrap() = m;
                        }
                        Ok(())
                    }),
                );
                let data = Value::Object(slot.lock().unwrap().clone());
                let out = json!({"ok": true, "data": data});
                (serde_json::to_string(&out).unwrap_or_default(), req_id)
            }
            "putJson" => {
                let key = check_str(input, "key", "");
                if key.is_empty() {
                    return (r#"{"ok":false,"error":"key is required"}"#.into(), req_id);
                }
                let path = check_str(input, "path", "");
                let merge = check_bool(input, "merge", true);
                let obj = input.get("data").cloned().unwrap_or(Value::Null);
                let key_owned = key.clone();
                let path_owned = path.clone();
                self.app.modify_state(
                    false,
                    Box::new(move |t: &dyn ITrx| {
                        let _ = t.put_json(&key_owned, &path_owned, &obj, merge);
                        Ok(())
                    }),
                );
                (r#"{"ok":true}"#.into(), req_id)
            }
            "getByPrefix" => {
                let prefix = check_str(input, "prefix", "");
                let slot: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
                let slot_clone = slot.clone();
                // putJson/getJson store keys as "json::{key}::{path}", so prefix
                // searches must also look under the "json::" namespace and strip
                // it from results so callers see the same key space they wrote to.
                let json_prefix = format!("json::{}", prefix);
                self.app.modify_state(
                    true,
                    Box::new(move |t: &dyn ITrx| {
                        let keys = t.get_by_prefix(&json_prefix);
                        *slot_clone.lock().unwrap() = keys
                            .into_iter()
                            .map(|k| {
                                k.strip_prefix("json::").unwrap_or(&k).to_string()
                            })
                            .collect();
                        Ok(())
                    }),
                );
                let data = slot.lock().unwrap().clone();
                let out = json!({"ok": true, "data": data});
                (serde_json::to_string(&out).unwrap_or_default(), req_id)
            }
            "readSignals" => {
                // The store-log read, for creatures and connected containers:
                // the same tag-filtered query `/stores/history` serves the app,
                // so an agent backbone reconstructs a conversation from exactly
                // the rows the client sees, with no second transcript anywhere.
                let store_id = check_str(input, "storeId", "");
                if store_id.is_empty() {
                    return (r#"{"ok":false,"error":"storeId is required"}"#.into(), req_id);
                }
                let str_list = |key: &str| -> Vec<String> {
                    input
                        .get(key)
                        .and_then(Value::as_array)
                        .map(|a| {
                            a.iter()
                                .filter_map(|v| v.as_str().map(|s| s.to_string()))
                                .collect()
                        })
                        .unwrap_or_default()
                };
                let query = crate::models::packet::LogQuery {
                    tags_all: str_list("tagsAll"),
                    tags_any: str_list("tagsAny"),
                    before_time: check_i64(input, "beforeTime", 0),
                    after_time: check_i64(input, "afterTime", 0),
                    count: check_i64(input, "count", 100),
                };
                let query = match query.validated() {
                    Ok(q) => q,
                    Err(e) => {
                        let out = json!({"ok": false, "error": format!("{}", e)});
                        return (serde_json::to_string(&out).unwrap_or_default(), req_id);
                    }
                };
                let packets = match self.app.tools().storage().read_store_logs(&store_id, &query) {
                    Ok(p) => p,
                    Err(e) => {
                        // A creature must be able to tell "no history" from "the
                        // log is unreachable"; an empty list for both would have
                        // an agent reason over a conversation it never read.
                        let out = json!({"ok": false, "error": format!("{}", e)});
                        return (serde_json::to_string(&out).unwrap_or_default(), req_id);
                    }
                };
                let out = json!({"ok": true, "storeId": store_id, "signals": packets});
                (serde_json::to_string(&out).unwrap_or_default(), req_id)
            }
            "hasAccessToStore" => {
                let machine_id = check_str(input, "machineId", "");
                if machine_id.is_empty() {
                    return (r#"{"ok":false,"error":"machineId is required"}"#.into(), req_id);
                }
                let store_id = check_str(input, "storeId", "");
                if store_id.is_empty() {
                    return (r#"{"ok":false,"error":"storeId is required"}"#.into(), req_id);
                }
                let allowed = self
                    .app
                    .tools()
                    .security()
                    .has_access_to_store(&machine_id, &store_id);
                let out = json!({"ok": true, "allowed": allowed});
                (serde_json::to_string(&out).unwrap_or_default(), req_id)
            }
            "signalUser" => {
                let key = check_str(input, "key", "");
                let user_id = check_str(input, "userId", "");
                let packet = check_str(input, "packet", "{}");
                let is_system = check_bool(input, "system", true);
                let value = serde_json::from_str::<Value>(&packet).unwrap_or(Value::Null);
                self.app
                    .tools()
                    .signaler()
                    .signal_user(&key, &user_id, value, is_system);
                (r#"{"ok":true}"#.into(), req_id)
            }
            "signalGroup" => {
                let key = check_str(input, "key", "");
                let group_id = check_str(input, "groupId", "");
                let packet = check_str(input, "packet", "{}");
                let is_system = check_bool(input, "system", true);
                let except: Vec<String> = input
                    .get("except")
                    .and_then(Value::as_array)
                    .map(|arr| arr.iter().filter_map(|v| v.as_str().map(str::to_string)).collect())
                    .unwrap_or_default();
                let value = serde_json::from_str::<Value>(&packet).unwrap_or(Value::Null);
                self.app
                    .tools()
                    .signaler()
                    .signal_group(&key, &group_id, value, is_system, except);
                (r#"{"ok":true}"#.into(), req_id)
            }
            "joinGroup" => {
                let group_id = check_str(input, "groupId", "");
                let user_id = check_str(input, "userId", "");
                self.app.tools().signaler().join_group(&group_id, &user_id);
                (r#"{"ok":true}"#.into(), req_id)
            }
            _ => (r#"{"ok":false,"error":"unsupported micro op"}"#.into(), req_id),
        }
    }

    pub(crate) fn handle_store_crud(&self, op: &str, input: &Value, req_id: i64) -> (String, i64) {
        match op {
            "create" => {
                let mut store_id = check_str(input, "storeId", "");
                let mut creator_id = check_str(input, "creatorId", "");
                if creator_id.is_empty() {
                    creator_id = check_str(input, "userId", "");
                }
                let tag = check_str(input, "tag", "");
                let parent_id = check_str(input, "parentId", "");
                let is_public = bool_from_input(input, "isPublic", false);
                let pers_hist = bool_from_input(input, "persHist", false);
                let metadata = input.get("metadata").cloned().unwrap_or_else(|| json!({}));
                if store_id.is_empty() {
                    store_id = self.gen_id("store");
                }
                let store_id_owned = store_id.clone();
                let creator_id_owned = creator_id.clone();
                let metadata_owned = metadata.clone();
                self.app.modify_state(
                    false,
                    Box::new(move |t: &dyn ITrx| {
                        let store = Store {
                            id: store_id_owned.clone(),
                            tag: tag.clone(),
                            parent_id: parent_id.clone(),
                            is_public,
                            pers_hist,
                            member_count: 1,
                            ..Default::default()
                        };
                        store.push(t);
                        let _ = t.put_json(
                            &format!("StoreMeta::{}", store_id_owned),
                            "metadata",
                            &metadata_owned,
                            true,
                        );
                        if !creator_id_owned.is_empty() {
                            t.put_link(&format!("hasaccess::{}::{}", creator_id_owned, store_id_owned), "true");
                            t.put_link(&format!("creatorof::{}::{}", creator_id_owned, store_id_owned), "true");
                            // The creator administers the store they just made.
                            t.put_link(
                                &format!("onaccess::{}::{}", store_id_owned, creator_id_owned),
                                &StorePermissions::owner().encode(),
                            );
                        }
                        Ok(())
                    }),
                );
                let out = json!({"ok": true, "storeId": store_id});
                (serde_json::to_string(&out).unwrap_or_default(), req_id)
            }
            "update" => {
                let store_id = check_str(input, "storeId", "");
                if store_id.is_empty() {
                    return (r#"{"ok":false,"error":"storeId is required"}"#.into(), req_id);
                }
                let input_owned = input.clone();
                let store_id_owned = store_id.clone();
                self.app.modify_state(
                    false,
                    Box::new(move |t: &dyn ITrx| {
                        let mut store = Store {
                            id: store_id_owned.clone(),
                            ..Default::default()
                        }
                        .pull(t);
                        if store.id.is_empty() {
                            return Ok(());
                        }
                        if let Some(v) = input_owned.get("isPublic").and_then(Value::as_bool) {
                            store.is_public = v;
                        }
                        if let Some(v) = input_owned.get("persHist").and_then(Value::as_bool) {
                            store.pers_hist = v;
                        }
                        if let Some(v) = input_owned.get("tag").and_then(Value::as_str) {
                            store.tag = v.to_string();
                        }
                        store.push(t);
                        if let Some(md) = input_owned.get("metadata") {
                            let _ = t.put_json(&format!("StoreMeta::{}", store_id_owned), "metadata", md, true);
                        }
                        Ok(())
                    }),
                );
                (format!("{{\"ok\":true,\"storeId\":\"{}\"}}", store_id), req_id)
            }
            "delete" => {
                let store_id = check_str(input, "storeId", "");
                if store_id.is_empty() {
                    return (r#"{"ok":false,"error":"storeId is required"}"#.into(), req_id);
                }
                let store_id_owned = store_id.clone();
                self.app.modify_state(
                    false,
                    Box::new(move |t: &dyn ITrx| {
                        let store = Store {
                            id: store_id_owned.clone(),
                            ..Default::default()
                        }
                        .pull(t);
                        if !store.id.is_empty() {
                            store.delete(t);
                        }
                        t.del_key(&format!("Json::StoreMeta::{}::metadata", store_id_owned));
                        // Membership links outlive the object unless we drop them:
                        // listStores walks hasaccess, and a later getStore still
                        // echoes the requested id, which is how a deleted space
                        // came back as an untitled project.
                        let prefix = format!("onaccess::{}::", store_id_owned);
                        let members = t.get_links_list(&prefix, -1, -1, &[]).unwrap_or_default();
                        for k in members {
                            let member_id = k.strip_prefix(&prefix).unwrap_or(&k).to_string();
                            if member_id.is_empty() {
                                continue;
                            }
                            t.del_key(&format!("link::onaccess::{}::{}", store_id_owned, member_id));
                            t.del_key(&format!("link::hasaccess::{}::{}", member_id, store_id_owned));
                            t.del_key(&format!("link::creatorof::{}::{}", member_id, store_id_owned));
                        }
                        Ok(())
                    }),
                );
                (format!("{{\"ok\":true,\"storeId\":\"{}\"}}", store_id), req_id)
            }
            "get" => {
                let store_id = check_str(input, "storeId", "");
                if store_id.is_empty() {
                    return (r#"{"ok":false,"error":"storeId is required"}"#.into(), req_id);
                }
                let store_slot = Arc::new(Mutex::new(Store::default()));
                let meta_slot: Arc<Mutex<Map<String, Value>>> = Arc::new(Mutex::new(Map::new()));
                let store_clone = store_slot.clone();
                let meta_clone = meta_slot.clone();
                let store_id_owned = store_id.clone();
                self.app.modify_state(
                    true,
                    Box::new(move |t: &dyn ITrx| {
                        let s = Store {
                            id: store_id_owned.clone(),
                            ..Default::default()
                        }
                        .pull(t);
                        *store_clone.lock().unwrap() = s;
                        if let Ok(m) = t.get_json(&format!("StoreMeta::{}", store_id_owned), "metadata") {
                            *meta_clone.lock().unwrap() = m;
                        }
                        Ok(())
                    }),
                );
                let store = store_slot.lock().unwrap().clone();
                let meta = Value::Object(meta_slot.lock().unwrap().clone());
                let out = json!({"ok": true, "store": store, "metadata": meta});
                (serde_json::to_string(&out).unwrap_or_default(), req_id)
            }
            "list" => {
                let user_id = check_str(input, "userId", "");
                let prefix = if user_id.is_empty() {
                    "obj::Store::".to_string()
                } else {
                    format!("hasaccess::{}::", user_id)
                };
                let slot: Arc<Mutex<Vec<Store>>> = Arc::new(Mutex::new(Vec::new()));
                let slot_clone = slot.clone();
                self.app.modify_state(
                    true,
                    Box::new(move |t: &dyn ITrx| {
                        if let Ok(list) = Store::list(t, &prefix, false, &HashMap::new(), &HashMap::new(), 0, 50) {
                            *slot_clone.lock().unwrap() = list;
                        }
                        Ok(())
                    }),
                );
                let stores = slot.lock().unwrap().clone();
                let out = json!({"ok": true, "stores": stores});
                (serde_json::to_string(&out).unwrap_or_default(), req_id)
            }
            // List the creatures with access to a store. An optional `type`
            // filter (e.g. "machine", "human") returns only creatures of that
            // type — each resolved to its full Creature record.
            "listAccess" | "listMembers" | "readMembers" => {
                let store_id = check_str(input, "storeId", "");
                if store_id.is_empty() {
                    return (r#"{"ok":false,"error":"storeId is required"}"#.into(), req_id);
                }
                let want_type = check_str(input, "type", "");
                let slot: Arc<Mutex<Vec<Creature>>> = Arc::new(Mutex::new(Vec::new()));
                let sc = slot.clone();
                let sid = store_id.clone();
                let want_owned = want_type.clone();
                self.app.modify_state(
                    true,
                    Box::new(move |t: &dyn ITrx| {
                        let prefix = format!("onaccess::{}::", sid);
                        let keys = t.get_links_list(&prefix, -1, -1, &[]).unwrap_or_default();
                        let mut out: Vec<Creature> = Vec::new();
                        for k in keys {
                            let member_id = k.strip_prefix(&prefix).unwrap_or(&k).to_string();
                            if member_id.is_empty() {
                                continue;
                            }
                            let c = Creature { id: member_id, ..Default::default() }.pull(t);
                            if c.id.is_empty() {
                                continue;
                            }
                            if !want_owned.is_empty() && c.type_name != want_owned {
                                continue;
                            }
                            out.push(c);
                        }
                        *sc.lock().unwrap() = out;
                        Ok(())
                    }),
                );
                let members = slot.lock().unwrap().clone();
                let out = json!({"ok": true, "storeId": store_id, "type": want_type, "members": members});
                (serde_json::to_string(&out).unwrap_or_default(), req_id)
            }
            _ => (r#"{"ok":false,"error":"unsupported store op"}"#.into(), req_id),
        }
    }

    /// `wm.handleTerminateVM` — terminates a VM by runtime.
    ///
    /// The typed terminate packet is built by the runtime's own plugin
    /// (`VmPlugin::build_terminate_request`), so per-runtime identity fields
    /// (e.g. container names) never leak into the node.
    pub(crate) fn handle_terminate_vm(&self, input: &Value, req_id: i64) -> (String, i64) {
        let target_runtime = normalize_runtime(&check_str(input, "runtime", ""));
        if target_runtime.is_empty() {
            return (r#"{"error":1}"#.into(), req_id);
        }
        let plugin = match caspar_vm_sdk::registry::get(&target_runtime) {
            Some(p) => p,
            None => return ("unsupported runtime".into(), req_id),
        };
        match plugin.build_terminate_request(input) {
            Ok(packet) => {
                self.send_to_engine(packet);
                ("{}".into(), req_id)
            }
            Err(_) => ("unsupported runtime".into(), req_id),
        }
    }

    pub(crate) fn handle_check_token_validity(&self, input: &Value, req_id: i64) -> (String, i64) {
        let token_owner_id = check_str(input, "tokenOwnerId", "");
        let token_id = check_str(input, "tokenId", "");
        if token_owner_id.is_empty() || token_id.is_empty() {
            return (r#"{"error":1}"#.into(), req_id);
        }
        let gas_slot = Arc::new(Mutex::new(0i64));
        let gas_clone = gas_slot.clone();
        let token_owner_owned = token_owner_id.clone();
        let token_id_owned = token_id.clone();
        self.app.modify_state(
            true,
            Box::new(move |t: &dyn ITrx| {
                let consumed_key = format!(
                    "Temp::User::{}::consumedTokens::{}",
                    token_owner_owned, token_id_owned
                );
                if t.get_string(&consumed_key) == "true" {
                    return Ok(());
                }
                if let Ok(m) = t.get_json(
                    &format!("Json::Creature::{}", token_owner_owned),
                    &format!("lockedTokens.{}", token_id_owned),
                ) {
                    if let Some(amount) = m.get("amount").and_then(Value::as_f64) {
                        *gas_clone.lock().unwrap() = amount as i64;
                    }
                }
                Ok(())
            }),
        );
        let gas_limit = *gas_slot.lock().unwrap();
        let out = json!({"gasLimit": gas_limit});
        (serde_json::to_string(&out).unwrap_or_default(), req_id)
    }

    pub(crate) fn handle_plant_trigger(&self, input: &Value, req_id: i64) -> (String, i64) {
        use std::thread;
        use std::time::Duration;

        let count = check_i64(input, "count", 0);
        let machine_id = check_str(input, "machineId", "");
        if machine_id.is_empty() {
            return (r#"{"error":1}"#.into(), req_id);
        }
        let tag = check_str(input, "tag", "");
        if tag.is_empty() {
            return (r#"{"error":1}"#.into(), req_id);
        }
        let store_id = check_str(input, "storeId", "");
        if store_id.is_empty() {
            return (r#"{"error":1}"#.into(), req_id);
        }
        let data = check_str(input, "input", "");
        // The entity of `machine_id` to re-run on wake. Creatures deploy their
        // module under a named entity ("main"), so the alarm must name it — the
        // program's default module path is not the wasm file. Defaults to "main".
        let mut entity_id = check_str(input, "entityId", "main");
        if entity_id.is_empty() {
            entity_id = "main".to_string();
        }
        if tag == "alarm" {
            let app = self.app.clone();
            let machine_id_owned = machine_id.clone();
            let store_id_owned = store_id.clone();
            let data_owned = data.clone();
            let entity_id_owned = entity_id.clone();
            thread::spawn(move || {
                let machine_id_inner = machine_id_owned.clone();
                let store_id_inner = store_id_owned.clone();
                let data_inner = data_owned.clone();
                let entity_id_inner = entity_id_owned.clone();
                let now_ms = super::driver::now_unix_ms();
                let alarm_time = now_ms + count * 1000;
                app.modify_state(
                    false,
                    Box::new(move |t: &dyn ITrx| {
                        t.put_link(&format!("vmAlarmStoreId::{}", machine_id_inner), &store_id_inner);
                        t.put_link(&format!("vmAlarmData::{}", machine_id_inner), &data_inner);
                        t.put_link(&format!("vmAlarmEntity::{}", machine_id_inner), &entity_id_inner);
                        t.put_link(&format!("vmAlarmTime::{}", machine_id_inner), &format!("{}", alarm_time));
                        Ok(())
                    }),
                );
                thread::sleep(Duration::from_secs(count.max(0) as u64));
                let machine_id_drain = machine_id_owned.clone();
                app.modify_state(
                    false,
                    Box::new(move |t: &dyn ITrx| {
                        t.del_key(&format!("link::vmAlarmStoreId::{}", machine_id_drain));
                        t.del_key(&format!("link::vmAlarmData::{}", machine_id_drain));
                        t.del_key(&format!("link::vmAlarmEntity::{}", machine_id_drain));
                        t.del_key(&format!("link::vmAlarmTime::{}", machine_id_drain));
                        Ok(())
                    }),
                );
                if app
                    .tools()
                    .security()
                    .has_access_to_store(&machine_id_owned, &store_id_owned)
                {
                    // Run via the IVmm trait so other implementations are not
                    // mandatory; this matches how Go re-entered itself. The entity
                    // is named so the creature's real module is resolved.
                    app.tools().vmm().run_vm_entity(
                        &machine_id_owned,
                        &store_id_owned,
                        &data_owned,
                        &entity_id_owned,
                    );
                }
            });
        } else {
            self.app
                .plant_chain_trigger(count, &machine_id, &tag, &machine_id, &store_id, &data);
        }
        ("{}".into(), req_id)
    }

    /// Post one signal into a store on behalf of the calling VM.
    ///
    /// The signaller is the node-stamped `machineId` — the identity the gateway
    /// resolved for this container, which is also what `/stores/signal` checks
    /// the `signal` permission against. A caller-supplied `userId` is NOT
    /// required and NOT used for identity: a creature never declares who it is.
    pub(crate) fn handle_signal_store(&self, input: &Value, req_id: i64) -> (String, i64) {
        let machine_id = check_str(input, "machineId", "");
        if machine_id.is_empty() {
            return (
                r#"{"ok":false,"error":"machineId is required (the node stamps it)"}"#.into(),
                req_id,
            );
        }
        let typ_and_temp = check_str(input, "type", "");
        let mut typ = typ_and_temp.clone();
        let mut temp = false;
        if let Some(t) = input.get("temp").and_then(Value::as_bool) {
            temp = t;
        } else {
            let parts: Vec<&str> = typ_and_temp.split('|').collect();
            if let Some(t) = parts.first() {
                typ = t.to_string();
            }
            if parts.len() > 1 {
                temp = parts[1] == "true";
            }
        }
        let store_id = check_str(input, "storeId", "");
        if store_id.is_empty() {
            return (r#"{"ok":false,"error":"storeId is required"}"#.into(), req_id);
        }
        // The signaller is the calling VM, which the node already knows. Anything
        // the caller puts in `userId` is carried on the packet as authorship
        // metadata only — it can never be the identity a permission is checked
        // against, and its absence must never reject the signal.
        let user_id = check_str(input, "userId", &machine_id);
        let data = check_str(input, "data", "");
        // Tags the calling creature attaches to the packet — the labels
        // `stores/history` later filters on. Malformed tags fail the signal in
        // the action body rather than being dropped here.
        let tags: Vec<String> = input
            .get("tags")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(|s| s.to_string()))
                    .collect()
            })
            .unwrap_or_default();
        // Build the SignalInput action call.
        use crate::shell::api::packets::stores::SignalInput;
        let signal_input = SignalInput {
            typ,
            data,
            store_id: store_id.clone(),
            user_id,
            tags,
            temp,
            origin: String::new(),
        };
        let info: Arc<dyn IInfo> = Arc::new(BaseInfo::new(&machine_id, &store_id));
        let app_for_closure = self.app.clone();
        // Carry the action's own answer back to the caller. Returning a bare
        // `{}` regardless of what happened is how a creature whose signals are
        // ALL being refused — no `signal` permission on the store, a log that
        // cannot be written — goes on believing every turn it posted landed.
        let outcome: Arc<Mutex<Result<Value, String>>> =
            Arc::new(Mutex::new(Err("/stores/signal is not registered".to_string())));
        let outcome_clone = outcome.clone();
        let closure: StateClosure = Box::new(move |state: Arc<dyn IState>| {
            if let Some(action) = app_for_closure.actor().fetch_action("/stores/signal") {
                *outcome_clone.lock().unwrap() = match action.act(state, Arc::new(signal_input.clone())) {
                    Ok((_code, v)) => Ok(v),
                    Err(e) => Err(format!("{}", e)),
                };
            }
            Ok(())
        });
        self.app.modify_state_securly(false, info, closure);
        let settled = outcome.lock().unwrap().clone();
        match settled {
            Ok(value) => {
                let mut out = match value {
                    Value::Object(map) => map,
                    other => {
                        let mut m = Map::new();
                        m.insert("result".to_string(), other);
                        m
                    }
                };
                out.insert("ok".to_string(), Value::Bool(true));
                (
                    serde_json::to_string(&Value::Object(out)).unwrap_or_default(),
                    req_id,
                )
            }
            Err(err) => {
                let out = json!({"ok": false, "error": err});
                (serde_json::to_string(&out).unwrap_or_default(), req_id)
            }
        }
    }

    pub(crate) fn handle_send_message_on_chain(
        &self,
        input: &Value,
        req_id: i64,
    ) -> (String, i64) {
        let chain_id = check_str(input, "chainId", "main");
        let mut key = check_str(input, "msgKey", "");
        if key.is_empty() {
            key = check_str(input, "key", "");
        }
        if key.is_empty() {
            return (r#"{"error":1}"#.into(), req_id);
        }
        let message_type = check_str(input, "messageType", "vm.execute");
        let payload_str = check_str(input, "payload", "{}");
        let signature = check_str(input, "signature", "");
        let owner = self.app.owner_id();
        let user_id = check_str(input, "userId", &owner);
        let reply_to = check_str(input, "replyTo", "");
        let store_id = check_str(input, "storeId", "");
        let receivers = parse_chain_receivers(input);
        let pay = parse_chain_pay_packet(input);
        self.app.globe().send_typed_message_on_chain(
            &chain_id,
            &key,
            &message_type,
            payload_str.into_bytes(),
            &signature,
            &user_id,
            receivers,
            &reply_to,
            &store_id,
            pay,
            None,
        );
        ("{}".into(), req_id)
    }

    /// Shared `gen_id(source)` helper using a read-only state transaction.
    pub(super) fn gen_id(&self, source: &str) -> String {
        let slot = Arc::new(Mutex::new(String::new()));
        let slot_clone = slot.clone();
        let storage = self.storage.clone();
        let source_owned = source.to_string();
        self.app.modify_state(
            true,
            Box::new(move |t: &dyn ITrx| {
                *slot_clone.lock().unwrap() = storage.gen_id(t, &source_owned);
                Ok(())
            }),
        );
        let id = slot.lock().unwrap().clone();
        id
    }
}

fn parse_chain_receivers(input: &Value) -> HashMap<String, HashMap<String, bool>> {
    let mut receivers: HashMap<String, HashMap<String, bool>> = HashMap::new();
    let Some(nodes) = input.get("receivers").and_then(Value::as_object) else {
        receivers.insert("*".to_string(), HashMap::new());
        return receivers;
    };
    for (node_id, machine_ids_raw) in nodes {
        let mut bucket: HashMap<String, bool> = HashMap::new();
        if let Some(arr) = machine_ids_raw.as_array() {
            for m in arr {
                if let Some(s) = m.as_str() {
                    bucket.insert(s.to_string(), true);
                }
            }
        }
        receivers.insert(node_id.clone(), bucket);
    }
    if receivers.is_empty() {
        receivers.insert("*".to_string(), HashMap::new());
    }
    receivers
}

fn parse_chain_pay_packet(
    input: &Value,
) -> Option<crate::models::chain::ChainPayPacket> {
    let Some(pay_obj) = input.get("pay").and_then(Value::as_object) else {
        return None;
    };
    use crate::models::chain::ChainPayPacket;
    let mut pay = ChainPayPacket::default();
    let s = |k: &str| pay_obj.get(k).and_then(Value::as_str).map(str::to_string);
    let i = |k: &str| pay_obj.get(k).and_then(Value::as_i64).or_else(|| pay_obj.get(k).and_then(Value::as_f64).map(|v| v as i64));
    if let Some(v) = s("type") { pay.typ = v; }
    if let Some(v) = s("sessionId") { pay.session_id = v; }
    if let Some(v) = s("userId") { pay.user_id = v; }
    if let Some(v) = s("lockId") { pay.lock_id = v; }
    if let Some(v) = s("lockSignature") { pay.lock_signature = v; }
    if let Some(v) = s("storeId") { pay.store_id = v; }
    if let Some(v) = s("vmPayload") { pay.vm_payload = v; }
    if let Some(v) = s("error") { pay.error = v; }
    if let Some(v) = i("amount") { pay.amount = v; }
    if let Some(v) = i("requestedSeconds") { pay.requested_seconds = v; }
    if let Some(v) = i("acceptedSeconds") { pay.accepted_seconds = v; }
    if let Some(v) = i("costPerSecond") { pay.cost_per_second = v; }
    if let Some(arr) = pay_obj.get("machineIds").and_then(Value::as_array) {
        pay.machine_ids = arr.iter().filter_map(|v| v.as_str().map(str::to_string)).collect();
    }
    Some(pay)
}


#[cfg(test)]
mod exec_shell_action_tests {
    use serde_json::json;

    /// The identity rule, expressed the way `handle_exec_shell_action` applies
    /// it. `asSelf` acts as the node-resolved caller and nothing else — a guest
    /// naming a `userId` alongside it cannot redirect who it acts as, and an
    /// unresolvable caller is refused rather than falling back to the node owner
    /// (which would hand a container the platform's own authority).
    fn acting_identity(caller: &str, input: &serde_json::Value, owner: &str) -> Option<(String, String)> {
        let as_self = input.get("asSelf").and_then(serde_json::Value::as_bool).unwrap_or(false);
        if as_self {
            let caller = caller.trim();
            if caller.is_empty() {
                return None;
            }
            return Some((caller.to_string(), "#appletsign".to_string()));
        }
        let user_id = input
            .get("userId")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string)
            .unwrap_or_else(|| owner.to_string());
        let signature = input
            .get("signature")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("")
            .to_string();
        Some((user_id, signature))
    }

    #[test]
    fn as_self_acts_as_the_resolved_caller_only() {
        let input = json!({"asSelf": true, "userId": "1@global", "signature": "forged"});
        let (user, sig) = acting_identity("42@global", &input, "1@global").unwrap();
        assert_eq!(user, "42@global", "the guest-named userId is ignored");
        assert_eq!(sig, "#appletsign", "a creature authenticates through the applet path");
    }

    #[test]
    fn as_self_without_a_resolved_caller_is_refused() {
        let input = json!({"asSelf": true});
        assert!(
            acting_identity("", &input, "1@global").is_none(),
            "an unidentifiable caller must not fall back to the node owner",
        );
    }

    #[test]
    fn an_explicitly_empty_user_stays_anonymous() {
        // How the auth creature reaches /creatures/login: no identity, no
        // signature, an anon-guarded action.
        let input = json!({"userId": ""});
        let (user, sig) = acting_identity("42@global", &input, "1@global").unwrap();
        assert_eq!(user, "");
        assert_eq!(sig, "");
    }

    #[test]
    fn an_omitted_user_still_defaults_to_the_owner() {
        let input = json!({"path": "/creatures/login"});
        let (user, _) = acting_identity("42@global", &input, "1@global").unwrap();
        assert_eq!(user, "1@global", "unchanged for callers that name no identity");
    }
}

#[cfg(test)]
mod signal_store_tests {
    use serde_json::json;

    /// Who a store signal is attributed to, expressed the way
    /// `handle_signal_store` resolves it.
    ///
    /// This is the rule that shipped wrong: the hostcall REQUIRED a `userId` the
    /// docker gateway never stamps and no creature sends, so every signal the
    /// agent backbone posted — every step, every tool call, every answer — was
    /// refused before it reached the action.
    fn signaller(input: &serde_json::Value) -> Option<String> {
        let machine_id = input.get("machineId").and_then(|v| v.as_str()).unwrap_or("");
        if machine_id.is_empty() {
            return None;
        }
        Some(machine_id.to_string())
    }

    #[test]
    fn a_caller_that_declares_no_user_is_still_accepted() {
        // Exactly what the backbone sends: the node's own stamp, nothing else.
        let input = json!({"machineId": "170@global", "storeId": "7@global", "type": "all"});
        assert_eq!(signaller(&input).as_deref(), Some("170@global"));
    }

    #[test]
    fn the_signaller_is_the_stamped_machine_not_a_declared_user() {
        // A creature naming somebody else does not become them: the permission
        // check runs against the identity the node stamped.
        let input = json!({"machineId": "170@global", "userId": "1@global", "storeId": "7@global"});
        assert_eq!(signaller(&input).as_deref(), Some("170@global"));
    }

    #[test]
    fn an_unstamped_call_is_refused() {
        assert!(signaller(&json!({"storeId": "7@global"})).is_none());
    }
}
