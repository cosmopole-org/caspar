//! The elpify VM controller — VmPlugin implementation over the batched
//! provable runtime, plus the JS→MASM build step and program-execution
//! proof verification.

use std::path::Path;

use base64::Engine;
use elpify_lang::{
    execute_masm_file_with_proof, stack_outputs_from_ints, transpile_js_to_masm, verify_execution,
};
use serde_json::{json, Value as JsonValue};

use caspar_vm_sdk::util::{parse_u64_array_field, parse_u8_array_field, parse_vm_resource_limits};
use caspar_vm_sdk::{VmPlugin, VmPluginMeta};

use crate::queue::{enqueue_elpify_task, terminate_elpify_vms};

pub struct ElpifyVmController {
    meta: VmPluginMeta,
}

impl ElpifyVmController {
    pub fn new(meta: VmPluginMeta) -> Self {
        Self { meta }
    }
}

impl VmPlugin for ElpifyVmController {
    fn meta(&self) -> &VmPluginMeta {
        &self.meta
    }

    fn run_vm(&self, packet: &JsonValue) -> Result<JsonValue, String> {
        let machine_id = packet["machineId"].as_str().unwrap_or("").to_string();
        if machine_id.is_empty() {
            return Err("machineId is required".to_string());
        }
        let vm_id = packet["vmId"].as_str().unwrap_or("main").to_string();
        let ast_path = packet["astPath"].as_str().unwrap_or("").to_string();
        if ast_path.is_empty() {
            return Err("astPath (MASM file) is required".to_string());
        }
        let input = packet["input"].as_str().unwrap_or("{}").to_string();
        let limits = parse_vm_resource_limits(packet);

        // Two ways to pass a transaction's public inputs (payload):
        //   1. `inputs` array directly on the packet (preferred for
        //      host-fn callers).
        //   2. `input` is a JSON string of `{"inputs":[...]}` (legacy
        //      shape used by the wasm-runtime signal envelope).
        let inputs = if !packet["inputs"].is_null() {
            parse_u64_array_field(packet, "inputs")
        } else {
            match serde_json::from_str::<JsonValue>(&input) {
                Ok(parsed) => parse_u64_array_field(&parsed, "inputs"),
                Err(_) => Vec::new(),
            }
        };

        // `sync: true` keeps the legacy single-shot path: run this one
        // transaction to completion and return {outputs, proof} synchronously.
        if packet["sync"].as_bool().unwrap_or(false) {
            return match execute_masm_file_with_proof(&ast_path, &inputs) {
                Ok(artifacts) => {
                    // Encode the (tens-of-KB) STARK proof as base64 rather
                    // than a JSON number array — a single scalar survives
                    // wasm round-trips cheaply.
                    let proof_b64 = base64::engine::general_purpose::STANDARD
                        .encode(&artifacts.proof_bytes);
                    Ok(json!({
                        "ok": true,
                        "runtime": "elpify",
                        "machineId": machine_id,
                        "masmPath": ast_path,
                        "inputs": inputs,
                        "outputs": artifacts.stack_outputs,
                        "proof": proof_b64,
                        "sync": true,
                    }))
                }
                Err(err) => Err(err.to_string()),
            };
        }

        // Default path: enqueue into the per-program-entity batch queue. The
        // batch's proof and per-transaction outputs are delivered
        // asynchronously via a vmOutput packet.
        let input_raw = if !packet["inputs"].is_null() {
            json!({ "inputs": inputs }).to_string()
        } else {
            input
        };
        enqueue_elpify_task(&machine_id, ast_path.clone(), input_raw, vm_id, limits)?;
        Ok(json!({
            "ok": true,
            "runtime": "elpify",
            "machineId": machine_id,
            "masmPath": ast_path,
            "queued": true,
        }))
    }

    fn terminate_vm(&self, packet: &JsonValue) -> Result<JsonValue, String> {
        let machine_id = packet["machineId"].as_str().unwrap_or("");
        if machine_id.is_empty() {
            return Err("machineId is required".to_string());
        }
        terminate_elpify_vms(machine_id);
        Ok(json!({"ok": true, "runtime": "elpify", "machineId": machine_id}))
    }

    /// A delete stops every elpify VM of this machine and disposes of the
    /// lifecycle transaction and execution context the run left behind; with
    /// `removeArtifact` the transpiled MASM program is dropped too, so the
    /// program has to be rebuilt rather than re-executed from the deleted
    /// VM's artifact.
    fn delete_vm(&self, packet: &JsonValue) -> Result<JsonValue, String> {
        let machine_id = packet["machineId"].as_str().unwrap_or("");
        if machine_id.is_empty() {
            return Err("machineId is required".to_string());
        }
        let vm_id = packet["vmId"].as_str().unwrap_or("main");
        terminate_elpify_vms(machine_id);
        if let Some(h) = caspar_vm_sdk::host::host() {
            h.end_vm_json_trx(vm_id);
            h.commit_vm_buffer(vm_id);
            h.unregister_vm_context(vm_id);
        }
        let mut artifact_removed = false;
        if packet["removeArtifact"].as_bool().unwrap_or(false) {
            let masm_path = packet["masmPath"]
                .as_str()
                .or_else(|| packet["astPath"].as_str())
                .unwrap_or("")
                .trim();
            if !masm_path.is_empty() {
                artifact_removed = std::fs::remove_file(masm_path).is_ok();
            }
        }
        Ok(json!({
            "ok": true,
            "runtime": "elpify",
            "machineId": machine_id,
            "vmId": vm_id,
            "deleted": true,
            "artifactRemoved": artifact_removed,
        }))
    }

    fn exec_vm(&self, packet: &JsonValue) -> Result<JsonValue, String> {
        self.run_vm(packet)
    }

    /// Elpify "image build": transpile the deployed `module.elpify.js` source
    /// into the MASM program executed by the provable runtime.
    fn build_image(&self, packet: &JsonValue) -> Result<JsonValue, String> {
        let machine_id = packet["machineId"].as_str().unwrap_or("");
        if machine_id.is_empty() {
            return Err("machineId is required".to_string());
        }
        let build_path = packet["imageBuildPath"]
            .as_str()
            .or_else(|| packet["dockerfilePath"].as_str())
            .or_else(|| packet["path"].as_str())
            .unwrap_or("");
        if build_path.is_empty() {
            return Err("build path is required".to_string());
        }
        let source_path = resolve_source_path(build_path, "module.elpify.js")?;
        let source = std::fs::read_to_string(&source_path)
            .map_err(|e| format!("failed to read elpify source {}: {}", source_path, e))?;
        let masm = transpile_js_to_masm(&source)
            .map_err(|e| format!("failed to transpile elpify source: {}", e))?;
        let masm_path = if source_path.ends_with(".elpify.js") {
            source_path.replace(".elpify.js", ".masm")
        } else {
            let source_ref = Path::new(&source_path);
            source_ref
                .parent()
                .unwrap_or_else(|| Path::new("."))
                .join("module.masm")
                .to_string_lossy()
                .to_string()
        };
        std::fs::write(&masm_path, masm.as_bytes())
            .map_err(|e| format!("failed to write MASM output {}: {}", masm_path, e))?;
        Ok(json!({
            "ok": true,
            "machineId": machine_id,
            "sourcePath": source_path,
            "masmPath": masm_path,
            "runtime": "elpify"
        }))
    }

    /// Verify a STARK proof of a program execution.
    /// Packet: `{ masmPath, inputs, outputs, proof }`.
    fn verify_program_execution(&self, packet: &JsonValue) -> Result<JsonValue, String> {
        let masm_path = packet["masmPath"].as_str().unwrap_or("");
        let inputs = parse_u64_array_field(packet, "inputs");
        let outputs = parse_u64_array_field(packet, "outputs");
        let proof_bytes = parse_u8_array_field(packet, "proof");

        if masm_path.is_empty() {
            return Err("masmPath is required".to_string());
        }
        if proof_bytes.is_empty() {
            return Err("proof is required and must be an array of bytes".to_string());
        }

        let artifacts = execute_masm_file_with_proof(masm_path, &inputs)
            .map_err(|e| format!("unable to prepare program info for verification: {}", e))?;
        let stack_outputs = stack_outputs_from_ints(&outputs)
            .map_err(|e| format!("invalid output values for verification: {}", e))?;

        let security = verify_execution(
            artifacts.program_info,
            artifacts.stack_inputs,
            stack_outputs,
            &proof_bytes,
        )
        .map_err(|e| format!("proof verification failed: {}", e))?;
        Ok(json!({"ok": true, "security": security}))
    }
}

fn resolve_source_path(path: &str, default_file_name: &str) -> Result<String, String> {
    let path_ref = Path::new(path);
    if !path_ref.exists() {
        return Err(format!("build path does not exist: {}", path));
    }
    if path_ref.is_file() {
        return Ok(path.to_string());
    }
    let file_path = path_ref.join(default_file_name);
    if !file_path.exists() {
        return Err(format!(
            "required build source not found at {}",
            file_path.to_string_lossy()
        ));
    }
    Ok(file_path.to_string_lossy().to_string())
}
