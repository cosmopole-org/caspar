use crate::drivers::vmm::bridge::runtime_io::wasm_send;
use crate::drivers::vmm::globals::with_global_app;
use crate::drivers::vmm::host::functions::*;
use crate::drivers::vmm::prelude::*;
use crate::models::core::ICore;

pub(crate) struct HostHierarchy {
    pub(crate) vm_id: String,
    pub(crate) creature_id: String,
    pub(crate) program_id: String,
    pub(crate) entity_name: String,
    pub(crate) entity_path: String,
}

#[derive(Default)]
pub(crate) struct CachedVmHierarchy {
    pub(crate) creature_id: String,
    pub(crate) program_id: String,
}

fn resolve_cached_vm_hierarchy(packet: &JsonValue, input: &JsonValue) -> CachedVmHierarchy {
    // The vmId used to resolve the out-of-band VM context must be the
    // authoritative one the runtime stamped on the *packet* (the guest cannot
    // reach it). Only fall back to `input.vmId` for runtimes that stamp their
    // verified identity into the input envelope (e.g. the docker gateway),
    // never letting an in-process guest pick which VM context it resolves to.
    let vm_id = packet["vmId"]
        .as_str()
        .filter(|v| !v.trim().is_empty())
        .or_else(|| input["vmId"].as_str().filter(|v| !v.trim().is_empty()))
        .unwrap_or("")
        .trim()
        .to_string();
    if vm_id.is_empty() {
        return CachedVmHierarchy::default();
    }
    with_global_app(|app| {
        app.tools()
            .vmm()
            .get_vm_context(&vm_id)
            .map(|(creature_id, program_id)| CachedVmHierarchy {
                creature_id,
                program_id,
            })
    })
    .flatten()
    .unwrap_or_default()
}

fn value_from_packet_or_input<'a>(
    packet: &'a JsonValue,
    input: &'a JsonValue,
    key: &str,
) -> &'a str {
    packet[key]
        .as_str()
        .filter(|v| !v.is_empty())
        .or_else(|| input[key].as_str().filter(|v| !v.is_empty()))
        .unwrap_or("")
}

pub(crate) fn resolve_host_hierarchy(packet: &JsonValue, input: &JsonValue) -> HostHierarchy {
    // Prefer the authoritative, runtime-stamped packet vmId over anything the
    // guest may have placed in `input`.
    let vm_id = packet["vmId"]
        .as_str()
        .filter(|v| !v.trim().is_empty())
        .or_else(|| input["vmId"].as_str())
        .unwrap_or("")
        .trim()
        .to_string();
    let cached = resolve_cached_vm_hierarchy(packet, input);

    // Identity (creature + program) comes ONLY from node-authoritative sources —
    // NEVER from a guest-supplied claim. There are two such sources, in priority
    // order:
    //   1. `get_vm_context` on the runtime-stamped / docker-gateway-verified vmId
    //      (docker containers + any VM that registers an execution context). The
    //      gateway resolves a container's identity from its source IP and refuses
    //      unidentified containers, so this is unforgeable.
    //   2. The *packet* envelope fields. The in-process VM runtimes stamp these
    //      from the node-assigned machine/vm ids when they forward a host call
    //      (see `vms/wasm/src/host_calls.rs`: "we stamp it at the packet level,
    //      which resolve_host_hierarchy trusts over input"). A wasm/elpian guest
    //      controls only `input`, never the packet the runtime wraps around it,
    //      so a packet-level id cannot be forged.
    // The guest-controlled `input` is NEVER consulted for identity — a docker
    // caller (whose input the gateway *does* stamp) always resolves via (1), and
    // for the docker path the packet carries no top-level id, so (2) is a no-op
    // there. This preserves per-creature storage isolation for every legitimate
    // runtime while trusting nothing the guest can set.
    let creature_id_owned = if !cached.creature_id.is_empty() {
        cached.creature_id
    } else {
        packet["creatureId"]
            .as_str()
            .filter(|v| !v.is_empty())
            .unwrap_or("")
            .to_string()
    };
    let program_id_owned = if !cached.program_id.is_empty() {
        cached.program_id
    } else {
        packet["programId"]
            .as_str()
            .filter(|v| !v.is_empty())
            .unwrap_or("")
            .to_string()
    };

    // entity_name / entity_path only subdivide storage *within* the already-
    // authenticated creature+program namespace, so they carry through as given.
    let entity_name = value_from_packet_or_input(packet, input, "entityName").to_string();
    let entity_path = packet["entityPath"]
        .as_str()
        .filter(|v| !v.is_empty())
        .or_else(|| packet["astPath"].as_str().filter(|v| !v.is_empty()))
        .or_else(|| input["entityPath"].as_str().filter(|v| !v.is_empty()))
        .or_else(|| input["astPath"].as_str().filter(|v| !v.is_empty()))
        .or_else(|| input["astpath"].as_str().filter(|v| !v.is_empty()))
        .unwrap_or("")
        .to_string();
    HostHierarchy {
        vm_id,
        creature_id: creature_id_owned,
        program_id: program_id_owned,
        entity_name,
        entity_path,
    }
}

/// Execute a low-level key-value DB operation for a VM host-call.
///
/// All logic (write-ahead buffering, read-your-own-writes, ICore fallthrough)
/// lives in `IVmm::vm_db_op` on the `Vmm` struct, which is reached via the
/// canonical `ICore → tools() → vmm()` path.  This function is a thin
/// adapter that computes the storage namespace from `HostHierarchy` and
/// delegates.
pub(crate) fn run_db_op(ctx: &HostHierarchy, input: &JsonValue) -> Result<String, String> {
    let op = input["op"].as_str().unwrap_or("");
    let key = input["key"].as_str().unwrap_or("");
    let val = input["val"].as_str().unwrap_or("");
    let prefix = input["prefix"].as_str().unwrap_or("");

    let db_prefix = if !ctx.creature_id.is_empty() && !ctx.program_id.is_empty() {
        if !ctx.entity_name.is_empty() && !ctx.entity_path.is_empty() {
            format!(
                "{}::{}::{}::{}",
                ctx.creature_id, ctx.program_id, ctx.entity_name, ctx.entity_path
            )
        } else if !ctx.entity_name.is_empty() {
            format!(
                "{}::{}::{}",
                ctx.creature_id, ctx.program_id, ctx.entity_name
            )
        } else {
            format!("{}::{}", ctx.creature_id, ctx.program_id)
        }
    } else if !ctx.program_id.is_empty() {
        ctx.program_id.clone()
    } else {
        ctx.creature_id.clone()
    };

    let namespaced_key = format!("AppletDb::{}::{}", db_prefix, key);
    let ns_prefix = format!("AppletDb::{}::{}", db_prefix, prefix);

    match with_global_app(|app| {
        app.tools()
            .vmm()
            .vm_db_op(&ctx.vm_id, op, &namespaced_key, val, &ns_prefix)
    }) {
        Some(result) => result,
        None => Err("vmm not initialised".to_string()),
    }
}

/// Default request ceiling, matching reqwest's own blocking default so no
/// existing caller changes behaviour by this becoming explicit.
const HTTP_DEFAULT_TIMEOUT_SECS: u64 = 30;
/// Hard ceiling on what a caller may ask for. Long enough for the longest model
/// generation anyone runs synchronously; short enough that a hung upstream
/// cannot hold a host thread indefinitely.
const HTTP_MAX_TIMEOUT_SECS: u64 = 900;

pub(crate) fn perform_http_request(input: &JsonValue) -> Result<String, String> {
    let mut url = input["url"].as_str().unwrap_or("").to_string();
    if url.is_empty() {
        return Err("url is required".to_string());
    }

    let mut method = input["method"].as_str().unwrap_or("").trim().to_uppercase();
    if method.is_empty() {
        if let Some((prefixed_method, rest_url)) = url.split_once('|') {
            method = prefixed_method.trim().to_uppercase();
            url = rest_url.to_string();
        } else {
            method = "POST".to_string();
        }
    }

    let http_method =
        Method::from_bytes(method.as_bytes()).map_err(|e| format!("invalid http method: {}", e))?;

    // reqwest's blocking client times out after 30s by default, which is fine
    // for an API call and much too short for a model call: a long generation
    // routinely runs for minutes, and a creature proxying one would see its
    // request cut off mid-answer with no way to lengthen it. `timeoutSecs` is
    // the caller's own ceiling, clamped so a stuck upstream cannot pin a host
    // thread forever.
    let timeout_secs = input["timeoutSecs"]
        .as_u64()
        .filter(|v| *v > 0)
        .unwrap_or(HTTP_DEFAULT_TIMEOUT_SECS)
        .min(HTTP_MAX_TIMEOUT_SECS);
    let client = Client::builder()
        .timeout(std::time::Duration::from_secs(timeout_secs))
        .build()
        .map_err(|e| format!("could not build http client: {}", e))?;

    let mut request = client.request(http_method, url);

    match &input["headers"] {
        JsonValue::Object(headers_obj) => {
            for (k, v) in headers_obj {
                if let Some(value) = v.as_str() {
                    request = request.header(k, value);
                } else {
                    request = request.header(k, v.to_string());
                }
            }
        }
        JsonValue::String(headers_raw) => {
            if !headers_raw.trim().is_empty() {
                let parsed_headers: JsonValue = serde_json::from_str(headers_raw)
                    .map_err(|e| format!("invalid headers json: {}", e))?;
                if let Some(headers_obj) = parsed_headers.as_object() {
                    for (k, v) in headers_obj {
                        if let Some(value) = v.as_str() {
                            request = request.header(k, value);
                        } else {
                            request = request.header(k, v.to_string());
                        }
                    }
                } else {
                    return Err("headers must be a JSON object".to_string());
                }
            }
        }
        JsonValue::Null => {}
        _ => return Err("headers must be a JSON object or stringified JSON object".to_string()),
    }

    if let Some(body) = input["body"].as_str() {
        request = request.body(body.to_string());
    } else if !input["body"].is_null() {
        request = request.body(input["body"].to_string());
    }

    // Callers that need more than the body ask for the full response. Without
    // it a creature cannot tell a 401 from a 200, cannot read a session id a
    // server returns in a header (MCP's `Mcp-Session-Id`, say), and cannot see
    // a rate-limit header — it only ever sees bytes. The bare base64 body
    // stays the default so every existing caller is unaffected.
    let want_full = input["withResponse"].as_bool().unwrap_or(false)
        || input["includeHeaders"].as_bool().unwrap_or(false);

    // Server-sent events, folded back into one response. A creature answers a
    // signal once, so it cannot forward a stream to its own caller — but the
    // stream still has to be CONSUMED, for two reasons that both matter to an
    // LLM proxy: a long generation delivered as chunks never trips the idle
    // read that a single slow response would, and several providers report
    // token usage only on the final event of a stream, so a metered proxy that
    // cannot read a stream cannot bill what it just bought. With `stream` set,
    // the host reads the event stream to completion and returns the assembled
    // message in the ordinary non-streamed shape.
    let aggregate_stream = input["stream"].as_bool().unwrap_or(false)
        || input["aggregateStream"].as_bool().unwrap_or(false);

    let response = request
        .send()
        .map_err(|e| format!("http request failed: {}", e))?;

    if aggregate_stream {
        let status = response.status().as_u16();
        let mut headers = serde_json::Map::new();
        for (name, value) in response.headers().iter() {
            if let Ok(value) = value.to_str() {
                headers.insert(name.as_str().to_ascii_lowercase(), json!(value));
            }
        }
        let raw = response
            .text()
            .map_err(|e| format!("failed to read event stream: {}", e))?;
        let (assembled, events) = aggregate_sse_stream(&raw);
        let body = assembled.to_string();
        return Ok(json!({
            "ok": (200..400).contains(&status),
            "status": status,
            "headers": JsonValue::Object(headers),
            "streamed": true,
            "events": events,
            "body": body,
            "bodyBase64": BASE64_STANDARD.encode(body.as_bytes()),
        })
        .to_string());
    }

    if !want_full {
        let bytes = response
            .bytes()
            .map_err(|e| format!("failed to read response body: {}", e))?;
        return Ok(BASE64_STANDARD.encode(bytes));
    }

    let status = response.status().as_u16();
    let mut headers = serde_json::Map::new();
    for (name, value) in response.headers().iter() {
        if let Ok(value) = value.to_str() {
            // Lowercased, because HTTP header names are case-insensitive and a
            // creature comparing them as JSON keys has no way to know that.
            headers.insert(name.as_str().to_ascii_lowercase(), json!(value));
        }
    }
    let bytes = response
        .bytes()
        .map_err(|e| format!("failed to read response body: {}", e))?;
    let body_base64 = BASE64_STANDARD.encode(&bytes);
    Ok(json!({
        "ok": (200..400).contains(&status),
        "status": status,
        "headers": JsonValue::Object(headers),
        "bodyBase64": body_base64,
        // Decoded too when the body is text, which is what an API returns and
        // what every caller then has to decode by hand otherwise.
        "body": String::from_utf8(bytes.to_vec()).unwrap_or_default(),
    })
    .to_string())
}

/// Fold a server-sent event stream back into the single response it describes.
///
/// Two vocabularies are understood, because those are the two an LLM proxy
/// meets: OpenAI-compatible `chat.completion.chunk` events (used by OpenAI,
/// xAI, Groq, DeepSeek, Mistral, Together, OpenRouter and every gateway that
/// imitates them), and Anthropic's `message_start` / `content_block_delta` /
/// `message_delta` events. Both are reduced to the OpenAI non-streamed shape,
/// so a caller writes one parser rather than one per vendor.
///
/// `usage` is the reason this exists as much as the text is: several providers
/// report token counts only on the last event of a stream, and a proxy that
/// meters what it bought has to read that event to bill it.
///
/// A stream that is not either vocabulary — an error object a server sent as a
/// single event, say — comes back as the last parsed event verbatim, so a
/// failure is diagnosable rather than flattened into an empty message.
fn aggregate_sse_stream(raw: &str) -> (JsonValue, usize) {
    let mut content = String::new();
    let mut role = String::new();
    let mut finish_reason = JsonValue::Null;
    let mut usage = JsonValue::Null;
    let mut model = String::new();
    let mut id = String::new();
    let mut last_event = JsonValue::Null;
    let mut events = 0usize;
    // Tool calls arrive as deltas keyed by index, with the arguments split
    // across events; they are reassembled in order rather than by arrival.
    let mut tool_calls: BTreeMap<i64, (String, String, String)> = BTreeMap::new();
    // Anthropic reports output tokens on `message_delta` and input tokens back
    // on `message_start`, so the two halves are collected separately.
    let mut anthropic_input = 0i64;
    let mut anthropic_output = 0i64;
    let mut saw_anthropic = false;

    for line in raw.lines() {
        let line = line.trim();
        let Some(payload) = line.strip_prefix("data:") else {
            continue;
        };
        let payload = payload.trim();
        if payload.is_empty() || payload == "[DONE]" {
            continue;
        }
        let Ok(event) = serde_json::from_str::<JsonValue>(payload) else {
            continue;
        };
        events += 1;
        last_event = event.clone();

        if let Some(value) = event["model"].as_str() {
            if model.is_empty() {
                model = value.to_string();
            }
        }
        if let Some(value) = event["id"].as_str() {
            if id.is_empty() {
                id = value.to_string();
            }
        }
        // Anthropic.
        match event["type"].as_str().unwrap_or("") {
            "message_start" => {
                saw_anthropic = true;
                if let Some(value) = event["message"]["id"].as_str() {
                    id = value.to_string();
                }
                if let Some(value) = event["message"]["model"].as_str() {
                    model = value.to_string();
                }
                anthropic_input += event["message"]["usage"]["input_tokens"]
                    .as_i64()
                    .unwrap_or(0);
                anthropic_output += event["message"]["usage"]["output_tokens"]
                    .as_i64()
                    .unwrap_or(0);
                continue;
            }
            "content_block_delta" => {
                saw_anthropic = true;
                if let Some(text) = event["delta"]["text"].as_str() {
                    content.push_str(text);
                }
                continue;
            }
            "message_delta" => {
                saw_anthropic = true;
                anthropic_input += event["usage"]["input_tokens"].as_i64().unwrap_or(0);
                anthropic_output += event["usage"]["output_tokens"].as_i64().unwrap_or(0);
                if let Some(reason) = event["delta"]["stop_reason"].as_str() {
                    finish_reason = json!(reason);
                }
                continue;
            }
            _ => {}
        }

        // OpenAI-compatible. The usage slot is only read here, below the
        // Anthropic arms: Anthropic's `message_delta` also carries a `usage`,
        // but it holds one half of the count (its input tokens came back on
        // `message_start`), so letting it land in this slot would report a
        // completion with no prompt.
        if !event["usage"].is_null() {
            usage = event["usage"].clone();
        }
        let Some(choices) = event["choices"].as_array() else {
            continue;
        };
        for choice in choices {
            let delta = &choice["delta"];
            if let Some(value) = delta["role"].as_str() {
                if role.is_empty() {
                    role = value.to_string();
                }
            }
            if let Some(text) = delta["content"].as_str() {
                content.push_str(text);
            }
            if let Some(reason) = choice["finish_reason"].as_str() {
                finish_reason = json!(reason);
            }
            if let Some(calls) = delta["tool_calls"].as_array() {
                for call in calls {
                    let index = call["index"].as_i64().unwrap_or(0);
                    let entry = tool_calls.entry(index).or_default();
                    if let Some(value) = call["id"].as_str() {
                        entry.0 = value.to_string();
                    }
                    if let Some(value) = call["function"]["name"].as_str() {
                        entry.1 = value.to_string();
                    }
                    if let Some(value) = call["function"]["arguments"].as_str() {
                        entry.2.push_str(value);
                    }
                }
            }
        }
    }

    if events == 0 {
        return (JsonValue::Null, 0);
    }
    if saw_anthropic && usage.is_null() {
        usage = json!({
            "prompt_tokens": anthropic_input,
            "completion_tokens": anthropic_output,
            "total_tokens": anthropic_input + anthropic_output,
            "input_tokens": anthropic_input,
            "output_tokens": anthropic_output,
        });
    }
    // Nothing recognisable: hand back the last event so an error a server sent
    // as a single frame survives instead of becoming an empty answer.
    if content.is_empty() && tool_calls.is_empty() && !saw_anthropic && usage.is_null() {
        return (last_event, events);
    }

    let mut message = serde_json::Map::new();
    message.insert(
        "role".to_string(),
        json!(if role.is_empty() { "assistant" } else { &role }),
    );
    message.insert("content".to_string(), json!(content));
    if !tool_calls.is_empty() {
        let calls: Vec<JsonValue> = tool_calls
            .into_iter()
            .map(|(_, (call_id, name, arguments))| {
                json!({
                    "id": call_id,
                    "type": "function",
                    "function": {"name": name, "arguments": arguments},
                })
            })
            .collect();
        message.insert("tool_calls".to_string(), JsonValue::Array(calls));
    }

    let assembled = json!({
        "id": id,
        "object": "chat.completion",
        "model": model,
        "choices": [{
            "index": 0,
            "message": JsonValue::Object(message),
            "finish_reason": finish_reason,
        }],
        "usage": usage,
    });
    (assembled, events)
}

pub(crate) fn handle_unified_host_call(packet: &JsonValue) -> String {
    let op = packet["op"]
        .as_str()
        .or_else(|| packet["key"].as_str())
        .unwrap_or("");
    let mut input = if packet["input"].is_null() {
        JsonValue::Null
    } else {
        packet["input"].clone()
    };
    let ctx = resolve_host_hierarchy(packet, &input);
    if ctx.program_id.is_empty() {
        return json!({"ok": false, "error": "programId is required"}).to_string();
    }
    if let Some(input_obj) = input.as_object_mut() {
        if input_obj
            .get("programId")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .is_empty()
        {
            input_obj.insert(
                "programId".to_string(),
                JsonValue::String(ctx.program_id.clone()),
            );
        }
        if input_obj
            .get("machineId")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .is_empty()
        {
            input_obj.insert(
                "machineId".to_string(),
                JsonValue::String(ctx.program_id.clone()),
            );
        }
    }
    match op {
        "commitTrx" => {
            let vm_id = input["vmId"]
                .as_str()
                .or_else(|| packet["vmId"].as_str())
                .unwrap_or("")
                .trim()
                .to_string();
            if vm_id.is_empty() {
                json!({"ok": false, "error": "vmId required for commitTrx"}).to_string()
            } else {
                match with_global_app(|app| app.tools().vmm().vm_db_commit_explicit(&vm_id)) {
                    Some(Ok(())) => json!({"ok": true}).to_string(),
                    Some(Err(e)) => json!({"ok": false, "error": e}).to_string(),
                    None => json!({"ok": false, "error": "vmm not initialised"}).to_string(),
                }
            }
        }
        "dbOp" => host_fn_db_op(&ctx, &input),
        "runVm" => host_fn_run_vm(&ctx.program_id, &input),
        "terminateVm" => host_fn_terminate_vm(&input),
        "deleteVm" | "destroyVm" => host_fn_delete_vm(&ctx.program_id, &input),
        "vmEndpoints" => host_fn_vm_endpoints(&ctx.program_id, &input),
        // The gateway subscription channel: a creature mints bearer tokens for
        // programs it runs outside Caspar, and pushes updates to the ones
        // holding a socket open. The owning creature is the resolved caller,
        // never an input field.
        "registerBridgeToken" => host_fn_register_bridge_token(&ctx.program_id, &input),
        "revokeBridgeToken" => host_fn_revoke_bridge_token(&ctx.program_id, &input),
        "publishUpdate" => host_fn_publish_update(&ctx.program_id, &input),
        "execVm" | "execDocker" => host_fn_exec_vm(&input),
        "copyToVm" | "copyToDocker" => host_fn_copy_to_vm(&input),
        "buildVmImage" | "buildDockerImage" => host_fn_build_vm_image(&input),
        "httpPost" | "httpRequest" => host_fn_http_request(&input),
        "elpifyProof" | "verifyProgramExecution" => host_fn_verify_program(&input),
        "protocolApi" | "callProtocolApi" => host_fn_protocol_api(&input),
        "signal" => host_fn_signal(&input),
        // A JavaScript creature reaches this unified dispatcher directly. Do
        // not send its alarm through the legacy wasm callback transport: that
        // path has no request/response owner here and the trigger can be lost
        // while the guest receives an empty success. The micro host action is
        // the same VMM implementation used by the callback path.
        "plantTrigger" => host_fn_micro(op, &input),
        // Read-only execution-host identity. The node supplies every returned
        // identity and checks the target against its in-process listener table;
        // a guest can ask about a program id but cannot claim a node owner or
        // make a remote program appear locally hosted.
        "nodeIdentity" => host_fn_node_identity(&ctx.program_id, &input),
        // Federated finance writes are deliberately not ordinary `putJson`
        // calls. Only a node-owned control program may ask the host to sign
        // them, and the resulting packet is committed on the global chain.
        "publishFinanceCatalog" => host_fn_submit_node_finance(
            &ctx.program_id,
            "/creatures/publishFinanceCatalog",
            input.clone(),
        ),
        "registerFinanceNode" => host_fn_register_finance_node(&ctx.program_id, &input),
        "retireFinanceNode" => host_fn_retire_finance_node(&ctx.program_id),
        "registerFinanceResource" => {
            host_fn_register_finance_resource(&ctx.program_id, &input)
        }
        "reviewFinanceResource" => host_fn_submit_node_finance(
            &ctx.program_id,
            "/creatures/reviewFinanceResource",
            input.clone(),
        ),
        "retireFinanceResource" => host_fn_submit_node_finance(
            &ctx.program_id,
            "/creatures/retireFinanceResource",
            input.clone(),
        ),
        "publishFinanceQuote" => host_fn_publish_finance_quote(&ctx.program_id, &input),
        // Secret reads, authenticated as the node-authoritative creature bound to
        // this VM (get_vm_context on the runtime-stamped / gateway-verified vmId),
        // never a `creatureId` the guest may put in the request — so
        // resolve_cached_vm_hierarchy, never ctx.creature_id. Unresolvable → deny.
        "secretGet" => {
            let caller = resolve_cached_vm_hierarchy(packet, &input).creature_id;
            host_fn_secret_get(&caller, &input)
        }
        // Running a shell action as the calling creature (`asSelf`) — same
        // node-authoritative caller resolution as `secretGet`, so a guest can
        // never nominate whose identity it acts under.
        "execShellAction" => {
            let caller = resolve_cached_vm_hierarchy(packet, &input).creature_id;
            host_fn_exec_shell_action(&caller, &input)
        }
        "secretListGranted" => {
            let caller = resolve_cached_vm_hierarchy(packet, &input).creature_id;
            host_fn_secret_list_granted(&caller)
        }
        "createAccess" | "createOwnedAccess" => host_fn_create_access(&input),
        "deleteAccess" | "removeAccess" | "deleteOwnedAccess" | "removeOwnedAccess" => {
            host_fn_delete_access(&input)
        }
        "createStore" | "createOwnedStore" => host_fn_create_store(&input),
        "deleteStore" | "removeStore" | "deleteOwnedStore" | "removeOwnedStore" => {
            host_fn_delete_store(&input)
        }
        "getStore" => host_fn_get_store(&input),
        "listStores" => host_fn_list_stores(&input),
        // List the creatures (members) that have access to a store.
        "listStoreAccess" | "listStoreMembers" | "readMembers" => host_fn_list_store_access(&input),
        "updateStore" => host_fn_update_store(&input),
        "createCreature" | "createOwnedCreature" => host_fn_create_creature(&input),
        "getCreature" => host_fn_get_creature(&input),
        "listCreatures" => host_fn_list_creatures(&input),
        "updateCreature" => host_fn_update_creature(&input),
        "validateSign" => host_fn_validate_sign(&input),
        "transfer" => host_fn_transfer(&input),
        "consumeLock" => host_fn_consume_lock(&input),
        "lockToken" => host_fn_lock_token(&input),
        // The meter's authority operations. Each verifies that the resolved
        // caller is the hold's or pool's registered meter before signing the
        // request as the node owner, so who the caller IS decides everything.
        //
        // `ctx.program_id`, not the VM-only resolver these used to use. That one
        // resolves an out-of-band VM context by `vmId` and returns EMPTY for any
        // caller that is not a VM — and every registered meter on this platform
        // is a WASM creature, which has no vmId. So the authority check saw an
        // anonymous caller and refused every one of them: no pool reservation,
        // no settlement, and therefore no finished run ever billed. `ctx` is the
        // same node-resolved identity `publishUpdate` and `registerBridgeToken`
        // already trust, and it still prefers the cached VM context when there
        // is one, so a container meter resolves exactly as before.
        "startHold" => host_fn_start_hold(&ctx.program_id, &input),
        "settleHold" => host_fn_settle_hold(&ctx.program_id, &input),
        "releaseHold" => host_fn_release_hold(&ctx.program_id, &input),
        "reservePool" => {
            host_fn_pool_authority_call(&ctx.program_id, &input, "/creatures/reservePool", "pool reservation")
        }
        "settlePool" => {
            host_fn_pool_authority_call(&ctx.program_id, &input, "/creatures/settlePool", "pool settlement")
        }
        "releasePool" => {
            host_fn_pool_authority_call(&ctx.program_id, &input, "/creatures/releasePool", "pool release")
        }
        "debitPool" => {
            host_fn_pool_authority_call(&ctx.program_id, &input, "/creatures/debitPool", "pool debit")
        }
        "createProgram" => host_fn_create_program(&input),
        "deleteProgram" | "deleteOwnedProgram" => host_fn_delete_program(&input),
        // Program CRUD reads — exposed so store/miniapp creatures can fetch a
        // program's record + metadata (e.g. an MCP manifest) and enumerate the
        // programs of a machine. Mirror the creature CRUD reads already present.
        "getProgram" => host_fn_get_program(&input),
        "listPrograms" => host_fn_list_programs(&input),
        "listProgramMachines" => host_fn_list_program_machines(&input),
        "updateProgram" => host_fn_update_program(&input),
        "deployEntity" | "deploy entity" => host_fn_deploy_entity(&input),
        "deleteCreature" | "removeCreature" | "deleteOwnedCreature" | "removeOwnedCreature" => {
            host_fn_delete_creature(&input)
        }
        "signalUser" => host_fn_signal_user(&input),
        "signalGroup" => host_fn_signal_group(&input),
        "lockResource" => host_fn_lock_resource(&input),
        "unlockResource" => host_fn_unlock_resource(&input),
        "vmLog" | "consoleLog" => host_fn_vm_log(&input),
        // Micro ops backed by `Vmm::handle_micro_host_action` (the real DB /
        // signaler / access-control implementations).
        "genId" | "getLink" | "delKey" | "getJson" | "putJson" | "getByPrefix"
        | "hasAccessToStore" | "joinGroup" => host_fn_micro(op, &input),
        // Resource (vm-scoped) store CRUD.
        "createResourceStore" | "createVmOwnedStore" => {
            host_fn_resource_store(&"create".to_string(), &input)
        }
        "updateResourceStore" | "updateVmOwnedStore" => {
            host_fn_resource_store(&"update".to_string(), &input)
        }
        "deleteResourceStore" | "deleteVmOwnedStore" => {
            host_fn_resource_store(&"delete".to_string(), &input)
        }
        "getResourceStore" | "getVmOwnedStore" => {
            host_fn_resource_store(&"get".to_string(), &input)
        }
        "listResourceStores" | "listVmOwnedStores" => {
            host_fn_resource_store(&"list".to_string(), &input)
        }
        // Resource entities (file blobs etc.).
        "createResourceEntity" => host_fn_resource_entity_create(&input),
        "deleteResourceEntity" => host_fn_resource_entity_delete(&input),
        _ => {
            let packet = json!({
                "key": op,
                "input": input
            });
            wasm_send(packet)
        }
    }
}

/// Run a registered shell action for the calling creature. `caller` is resolved
/// by the node from the VM context, and is what an `asSelf` call acts as.
pub(crate) fn host_fn_exec_shell_action(caller: &str, input: &JsonValue) -> String {
    match with_global_app(|app| app.tools().vmm().exec_shell_action(caller, input)) {
        Some(out) => out,
        None => json!({"ok": false, "error": "vmm not initialised"}).to_string(),
    }
}

/// Generic dispatch into `IVmm::host_action_micro` via the canonical tool path.
pub(crate) fn host_fn_micro(op: &str, input: &JsonValue) -> String {
    match with_global_app(|app| app.tools().vmm().host_action_micro(op, input, 0).0) {
        Some(out) => out,
        None => json!({"ok": false, "error": "vmm not initialised"}).to_string(),
    }
}

/// Read a creature-owned secret on behalf of the calling docker creature.
///
/// The authenticated caller is the container's registered creature id (resolved
/// out-of-band from the VM context — a guest cannot forge it), mirroring the
/// signed `/creatures/secretGet` route for external clients. A caller reading its
/// own secret always succeeds; reading another owner's secret requires an
/// unexpired grant (`SecretGrant::<owner>::<name>::<caller>`). This is how the
/// agent backbone fetches a platform LLM key the operator granted it, instead of
/// the key being baked into the creature image. put/grant/revoke/list stay on the
/// signed routes (the app/operator perform those); a creature only ever reads.
pub(crate) fn host_fn_secret_get(caller: &str, input: &JsonValue) -> String {
    use crate::models::transaction::ITrx;
    use crate::shell::utils::secret_crypto;
    use std::sync::{Arc, Mutex};

    let caller = caller.trim();
    if caller.is_empty() {
        return json!({"ok": false, "error": "caller identity unavailable"}).to_string();
    }
    let name = input["name"].as_str().unwrap_or("").trim().to_string();
    if name.is_empty() || name.contains(':') {
        return json!({"ok": false, "error": "secret name is required"}).to_string();
    }
    let owner = {
        let o = input["owner"].as_str().unwrap_or("").trim();
        if o.is_empty() {
            caller.to_string()
        } else {
            o.to_string()
        }
    };
    let need_grant = owner != caller;
    let secret_link = format!("Secret::{}::{}", owner, name);
    let grant_link = format!("SecretGrant::{}::{}::{}", owner, name, caller);

    let fetched = with_global_app(|app| {
        let root = app.tools().storage().storage_root().to_string();
        let blob = Arc::new(Mutex::new(String::new()));
        let grant = Arc::new(Mutex::new(String::new()));
        let (b, g) = (blob.clone(), grant.clone());
        let (sl, gl) = (secret_link.clone(), grant_link.clone());
        app.modify_state(
            true,
            Box::new(move |trx: &dyn ITrx| {
                *b.lock().unwrap() = trx.get_link(&sl);
                if need_grant {
                    *g.lock().unwrap() = trx.get_link(&gl);
                }
                Ok(())
            }),
        );
        let blob = blob.lock().unwrap().clone();
        let grant = grant.lock().unwrap().clone();
        (root, blob, grant)
    });
    let (root, blob, grant_raw) = match fetched {
        Some(v) => v,
        None => return json!({"ok": false, "error": "vmm not initialised"}).to_string(),
    };

    if need_grant {
        let expires_at: i64 = grant_raw.trim().parse().unwrap_or(0);
        if expires_at <= 0 || chrono::Utc::now().timestamp_millis() >= expires_at {
            return json!({"ok": false, "error": "access denied: no valid grant for this secret"})
                .to_string();
        }
    }
    if blob.is_empty() {
        return json!({"ok": false, "error": "secret not found"}).to_string();
    }
    let key = match secret_crypto::master_key(&root) {
        Ok(k) => k,
        Err(e) => return json!({"ok": false, "error": e.to_string()}).to_string(),
    };
    match secret_crypto::decrypt(&blob, &key) {
        Ok(bytes) => match String::from_utf8(bytes) {
            Ok(value) => {
                json!({"ok": true, "owner": owner, "name": name, "value": value}).to_string()
            }
            Err(_) => json!({"ok": false, "error": "stored secret is not valid UTF-8"}).to_string(),
        },
        Err(e) => json!({"ok": false, "error": e.to_string()}).to_string(),
    }
}

/// List the `{owner, name}` secret grants held by the calling docker creature, so
/// the agent backbone can discover the platform keys granted to it without a
/// hardcoded owner. Same node-authoritative caller resolution as `secretGet`.
pub(crate) fn host_fn_secret_list_granted(caller: &str) -> String {
    use crate::models::transaction::ITrx;
    use std::sync::{Arc, Mutex};

    let caller = caller.trim().to_string();
    if caller.is_empty() {
        return json!({"ok": false, "error": "caller identity unavailable"}).to_string();
    }
    let grants = with_global_app(|app| {
        let slot = Arc::new(Mutex::new(Vec::<JsonValue>::new()));
        let slot_c = slot.clone();
        let caller_c = caller.clone();
        app.modify_state(
            true,
            Box::new(move |trx: &dyn ITrx| {
                *slot_c.lock().unwrap() =
                    crate::shell::api::actions::creature::list_granted_secrets(trx, &caller_c);
                Ok(())
            }),
        );
        let v = slot.lock().unwrap().clone();
        v
    });
    match grants {
        Some(g) => json!({"ok": true, "grants": g}).to_string(),
        None => json!({"ok": false, "error": "vmm not initialised"}).to_string(),
    }
}

// --------------------------------------------------------------------------- //
// Program CRUD reads — the write side (`createProgram`/`deleteProgram`) existed
// but the read side did not, so a store/miniapp creature could not resolve a
// program's record + metadata (e.g. an MCP manifest) or enumerate programs.
// These route through the canonical `IVmm::host_action_program` tool path, the
// same persisted-state mechanism the creature CRUD reads use.
// --------------------------------------------------------------------------- //

/// Dispatch into `IVmm::host_action_program` via the canonical tool path.
fn host_fn_program(op: &str, input: &JsonValue) -> String {
    match with_global_app(|app| app.tools().vmm().host_action_program(op, input, 0).0) {
        Some(out) => out,
        None => json!({"ok": false, "error": "vmm not initialised"}).to_string(),
    }
}

/// List the creatures with access to a store. Input: `{ storeId }`.
pub(crate) fn host_fn_list_store_access(input: &JsonValue) -> String {
    match with_global_app(|app| {
        app.tools()
            .vmm()
            .host_action_store("listAccess", input, 0)
            .0
    }) {
        Some(out) => out,
        None => json!({"ok": false, "error": "vmm not initialised"}).to_string(),
    }
}

/// Fetch a single program's record + metadata. Input: `{ programId }`.
pub(crate) fn host_fn_get_program(input: &JsonValue) -> String {
    host_fn_program("get", input)
}

/// List programs (optionally a page). Input: `{ offset?, count? }`.
pub(crate) fn host_fn_list_programs(input: &JsonValue) -> String {
    host_fn_program("list", input)
}

/// List the programs belonging to a machine creature. Input: `{ machineId }`.
pub(crate) fn host_fn_list_program_machines(input: &JsonValue) -> String {
    host_fn_program("listByMachine", input)
}

/// Update a program's record/metadata. Input: `{ programId, metadata?, ... }`.
pub(crate) fn host_fn_update_program(input: &JsonValue) -> String {
    host_fn_program("update", input)
}

/// Dispatch into `IVmm::host_action_resource_store` via the canonical tool path.
pub(crate) fn host_fn_resource_store(op: &str, input: &JsonValue) -> String {
    match with_global_app(|app| app.tools().vmm().host_action_resource_store(op, input, 0).0) {
        Some(out) => out,
        None => json!({"ok": false, "error": "vmm not initialised"}).to_string(),
    }
}

/// Dispatch into `IVmm::host_action_resource_entity_create`.
pub(crate) fn host_fn_resource_entity_create(input: &JsonValue) -> String {
    match with_global_app(|app| {
        app.tools()
            .vmm()
            .host_action_resource_entity_create(input, 0)
            .0
    }) {
        Some(out) => out,
        None => json!({"ok": false, "error": "vmm not initialised"}).to_string(),
    }
}

/// Dispatch into `IVmm::host_action_resource_entity_delete`.
pub(crate) fn host_fn_resource_entity_delete(input: &JsonValue) -> String {
    match with_global_app(|app| {
        app.tools()
            .vmm()
            .host_action_resource_entity_delete(input, 0)
            .0
    }) {
        Some(out) => out,
        None => json!({"ok": false, "error": "vmm not initialised"}).to_string(),
    }
}

/// Deliver a `signalUser` request from a wasm host-call to the in-process
/// signaler so the target user (typically the requesting CLI client) receives
/// the creature's response packet.
pub(crate) fn host_fn_signal_user(input: &JsonValue) -> String {
    let key = input["key"].as_str().unwrap_or("");
    let user_id = input["userId"].as_str().unwrap_or("");
    if key.is_empty() || user_id.is_empty() {
        return json!({
            "ok": false,
            "error": "signalUser requires key and userId"
        })
        .to_string();
    }
    let packet_str = input["packet"].as_str().unwrap_or("{}");
    let value = serde_json::from_str::<JsonValue>(packet_str).unwrap_or(JsonValue::Null);
    let delivered = crate::drivers::vmm::globals::with_global_app(|app| {
        app.tools()
            .signaler()
            .signal_user(key, user_id, value, true);
    });
    match delivered {
        Some(()) => json!({"ok": true}).to_string(),
        None => json!({"ok": false, "error": "global app not initialised"}).to_string(),
    }
}

/// Deliver a `signalGroup` request from a wasm host-call.
pub(crate) fn host_fn_signal_group(input: &JsonValue) -> String {
    let key = input["key"].as_str().unwrap_or("");
    let group_id = input["groupId"].as_str().unwrap_or("");
    if key.is_empty() || group_id.is_empty() {
        return json!({
            "ok": false,
            "error": "signalGroup requires key and groupId"
        })
        .to_string();
    }
    let packet_str = input["packet"].as_str().unwrap_or("{}");
    let value = serde_json::from_str::<JsonValue>(packet_str).unwrap_or(JsonValue::Null);
    let except: Vec<String> = input
        .get("except")
        .and_then(JsonValue::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    let delivered = crate::drivers::vmm::globals::with_global_app(|app| {
        app.tools()
            .signaler()
            .signal_group(key, group_id, value, true, except);
    });
    match delivered {
        Some(()) => json!({"ok": true}).to_string(),
        None => json!({"ok": false, "error": "global app not initialised"}).to_string(),
    }
}

/// Return the current execution node's public billing identity and whether a
/// resource program is actually hosted by this node. This is deliberately a
/// read-only host fact: billing/market creatures use it to bind deployments to
/// the node that executed the deploy action instead of trusting client fields.
pub(crate) fn host_fn_node_identity(caller_program_id: &str, input: &JsonValue) -> String {
    let target_program_id = input["resourceProgramId"]
        .as_str()
        .or_else(|| input["programId"].as_str())
        .unwrap_or("")
        .trim()
        .to_string();
    let Some(app) = with_global_app(|app| app.clone()) else {
        return json!({"ok": false, "error": "vmm not initialised"}).to_string();
    };
    let listeners = app.tools().signaler().listeners();
    let caller_program_id = caller_program_id.trim();
    let caller_hosted = !caller_program_id.is_empty() && listeners.contains_key(caller_program_id);
    let resource_hosted = target_program_id.is_empty() || listeners.contains_key(&target_program_id);
    json!({
        "ok": true,
        "nodeOwnerAccountId": app.owner_id(),
        "originId": app.id(),
        "callerProgramId": caller_program_id,
        "callerHosted": caller_hosted,
        "resourceProgramId": target_program_id,
        "resourceHosted": resource_hosted,
    })
    .to_string()
}

// Resolve the account that owns a program through Program -> Machine ->
// Creature. Both the finance-control check and resource ownership stamping use
// persisted host state; no owner value supplied by a guest is trusted.
fn finance_program_binding(
    app: &Arc<dyn ICore>,
    program_id: &str,
) -> Option<(String, String)> {
    use crate::models::transaction::ITrx;
    use crate::shell::api::model::{Creature, Program};
    use std::sync::Mutex;

    let program_id = program_id.trim().to_string();
    if program_id.is_empty() {
        return None;
    }
    let slot = Arc::new(Mutex::new(None::<(String, String)>));
    let slot_c = slot.clone();
    app.modify_state(
        true,
        Box::new(move |trx: &dyn ITrx| {
            let program = Program {
                id: program_id.clone(),
                ..Default::default()
            }
            .pull(trx);
            if program.machine_id.is_empty() {
                return Ok(());
            }
            let machine_id = program.machine_id;
            let machine = Creature {
                id: machine_id.clone(),
                ..Default::default()
            }
            .pull(trx);
            if !machine.owner_id.is_empty() {
                *slot_c.lock().unwrap() = Some((machine_id, machine.owner_id));
            }
            Ok(())
        }),
    );
    let binding = slot.lock().unwrap().clone();
    binding
}

fn finance_program_owner(app: &Arc<dyn ICore>, program_id: &str) -> Option<String> {
    finance_program_binding(app, program_id).map(|(_, owner)| owner)
}

fn finance_node_control_program(app: &Arc<dyn ICore>, caller_program_id: &str) -> bool {
    let caller_program_id = caller_program_id.trim();
    !caller_program_id.is_empty()
        && app
            .tools()
            .signaler()
            .listeners()
            .contains_key(caller_program_id)
        && finance_program_owner(app, caller_program_id).as_deref() == Some(app.owner_id().as_str())
}

fn finance_node_record(
    app: &Arc<dyn ICore>,
    node_owner_account_id: &str,
) -> Option<serde_json::Map<String, JsonValue>> {
    use crate::models::transaction::ITrx;
    use std::sync::Mutex;

    let owner = node_owner_account_id.to_string();
    let slot = Arc::new(Mutex::new(None));
    let slot_c = slot.clone();
    app.modify_state(
        true,
        Box::new(move |trx: &dyn ITrx| {
            if let Ok(nodes) = trx.get_json("Json::CreatureNamespace::billing", "nodes") {
                *slot_c.lock().unwrap() = nodes.get(&owner).and_then(JsonValue::as_object).cloned();
            }
            Ok(())
        }),
    );
    let node = slot.lock().unwrap().clone();
    node
}

/// Submit a node-owner-signed finance mutation to the global chain. The guest
/// supplies only the proposed payload; authorization is derived from the
/// runtime-stamped caller program and its persisted owning machine.
pub(crate) fn host_fn_submit_node_finance(
    caller_program_id: &str,
    action: &str,
    payload_value: JsonValue,
) -> String {
    use std::sync::mpsc;
    use std::time::Duration;

    let Some(app) = with_global_app(|app| app.clone()) else {
        return json!({"ok": false, "error": "vmm not initialised"}).to_string();
    };
    if !finance_node_control_program(&app, caller_program_id) {
        return json!({
            "ok": false,
            "error": "a locally hosted node-owner control program is required"
        })
        .to_string();
    }
    let payload = match serde_json::to_vec(&payload_value) {
        Ok(value) => value,
        Err(error) => {
            return json!({"ok": false, "error": format!("cannot encode finance packet: {error}")})
                .to_string()
        }
    };
    let owner_id = app.owner_id();
    let signature = app.sign_packet_as_owner(&payload);
    let (tx, rx) = mpsc::channel::<(Vec<u8>, i64, Option<String>)>();
    let callback: crate::models::globe::BaseResponseCallback =
        Box::new(move |data, status, error| {
            let _ = tx.send((data, status, error.map(|value| value.to_string())));
        });
    app.globe().send_base_request_on_chain(
        action,
        payload,
        &signature,
        &owner_id,
        "",
        callback,
    );
    match rx.recv_timeout(Duration::from_secs(30)) {
        Ok((data, status, None)) => {
            let result = serde_json::from_slice::<JsonValue>(&data).unwrap_or(JsonValue::Null);
            json!({"ok": status < 400, "statusCode": status, "result": result}).to_string()
        }
        Ok((_data, status, Some(error))) => {
            json!({"ok": false, "statusCode": status, "error": error}).to_string()
        }
        Err(_) => json!({"ok": false, "error": "global finance commit timed out"}).to_string(),
    }
}

pub(crate) fn host_fn_register_finance_node(
    caller_program_id: &str,
    input: &JsonValue,
) -> String {
    let Some(app) = with_global_app(|app| app.clone()) else {
        return json!({"ok": false, "error": "vmm not initialised"}).to_string();
    };
    if !finance_node_control_program(&app, caller_program_id) {
        return json!({"ok": false, "error": "node-owner control program required"}).to_string();
    }
    let Some(mut node) = input.get("node").and_then(JsonValue::as_object).cloned() else {
        return json!({"ok": false, "error": "finance node object required"}).to_string();
    };
    let meter = node
        .get("meterProgramId")
        .and_then(JsonValue::as_str)
        .unwrap_or("");
    let talent_meter = node
        .get("talentMeterProgramId")
        .and_then(JsonValue::as_str)
        .unwrap_or("");
    let listeners = app.tools().signaler().listeners();
    if meter.is_empty()
        || talent_meter.is_empty()
        || !listeners.contains_key(meter)
        || !listeners.contains_key(talent_meter)
    {
        return json!({
            "ok": false,
            "error": "registered finance meter programs must be hosted by this node"
        })
        .to_string();
    }
    let owner_id = app.owner_id();
    let Some((meter_creature_id, meter_owner_id)) = finance_program_binding(&app, meter) else {
        return json!({"ok": false, "error": "billing meter program binding unavailable"}).to_string();
    };
    let Some((talent_creature_id, talent_owner_id)) =
        finance_program_binding(&app, talent_meter)
    else {
        return json!({"ok": false, "error": "talent meter program binding unavailable"}).to_string();
    };
    if meter_owner_id != owner_id || talent_owner_id != owner_id {
        return json!({"ok": false, "error": "finance meters must be owned by the node owner"}).to_string();
    }
    node.insert("nodeOwnerAccountId".into(), json!(owner_id));
    node.insert("meterCreatureId".into(), json!(meter_creature_id));
    // Historical entity name for the backbone slot; the meter is whatever
    // program the node registered, and this only labels it.
    node.insert("meterEntityId".into(), json!("davinci"));
    node.insert("talentMeterCreatureId".into(), json!(talent_creature_id));
    node.insert("talentMeterEntityId".into(), json!("main"));
    node.insert("settlementAuthority".into(), json!(app.owner_id()));
    node.insert("originId".into(), json!(app.id()));
    node.insert("status".into(), json!("active"));
    host_fn_submit_node_finance(
        caller_program_id,
        "/creatures/registerFinanceNode",
        json!({"node": node}),
    )
}

pub(crate) fn host_fn_retire_finance_node(caller_program_id: &str) -> String {
    let Some(app) = with_global_app(|app| app.clone()) else {
        return json!({"ok": false, "error": "vmm not initialised"}).to_string();
    };
    host_fn_submit_node_finance(
        caller_program_id,
        "/creatures/retireFinanceNode",
        json!({"nodeOwnerAccountId": app.owner_id()}),
    )
}

pub(crate) fn host_fn_register_finance_resource(
    caller_program_id: &str,
    input: &JsonValue,
) -> String {
    let Some(app) = with_global_app(|app| app.clone()) else {
        return json!({"ok": false, "error": "vmm not initialised"}).to_string();
    };
    if !finance_node_control_program(&app, caller_program_id) {
        return json!({"ok": false, "error": "node-owner control program required"}).to_string();
    }
    let Some(mut resource) = input
        .get("resource")
        .and_then(JsonValue::as_object)
        .cloned()
    else {
        return json!({"ok": false, "error": "finance resource object required"}).to_string();
    };
    let program_id = resource
        .get("programId")
        .and_then(JsonValue::as_str)
        .unwrap_or("")
        .trim()
        .to_string();
    if program_id.is_empty()
        || !app
            .tools()
            .signaler()
            .listeners()
            .contains_key(&program_id)
    {
        return json!({"ok": false, "error": "resource program is not hosted by this node"})
            .to_string();
    }
    let Some(resource_owner) = finance_program_owner(&app, &program_id) else {
        return json!({"ok": false, "error": "resource program owner unavailable"}).to_string();
    };
    let node_owner = app.owner_id();
    let Some(node) = finance_node_record(&app, &node_owner) else {
        return json!({"ok": false, "error": "active global finance node registration required"})
            .to_string();
    };
    if node.get("status").and_then(JsonValue::as_str) != Some("active") {
        return json!({"ok": false, "error": "finance node registration is not active"})
            .to_string();
    }
    resource.insert("owner".into(), json!(resource_owner));
    resource.insert("hostNodeOwnerAccountId".into(), json!(node_owner));
    resource.insert("hostOriginId".into(), json!(app.id()));
    resource.insert(
        "billingMeterProgramId".into(),
        node.get("meterProgramId").cloned().unwrap_or(JsonValue::Null),
    );
    resource.insert(
        "billingMeterCreatureId".into(),
        node.get("meterCreatureId").cloned().unwrap_or(JsonValue::Null),
    );
    resource.insert(
        "billingMeterEntityId".into(),
        node.get("meterEntityId").cloned().unwrap_or(JsonValue::Null),
    );
    resource.insert(
        "nodeRegistrationRevision".into(),
        node.get("revision").cloned().unwrap_or(JsonValue::Null),
    );
    resource.insert(
        "nodeSandboxPerMinuteMinor".into(),
        node.get("sandboxPerMinuteMinor")
            .cloned()
            .unwrap_or(JsonValue::Null),
    );
    host_fn_submit_node_finance(
        caller_program_id,
        "/creatures/registerFinanceResource",
        json!({"resource": resource}),
    )
}

pub(crate) fn host_fn_publish_finance_quote(
    caller_program_id: &str,
    input: &JsonValue,
) -> String {
    let Some(app) = with_global_app(|app| app.clone()) else {
        return json!({"ok": false, "error": "vmm not initialised"}).to_string();
    };
    if !finance_node_control_program(&app, caller_program_id) {
        return json!({"ok": false, "error": "node-owner control program required"}).to_string();
    }
    let Some(quote) = input.get("quote").and_then(JsonValue::as_object).cloned() else {
        return json!({"ok": false, "error": "finance quote object required"}).to_string();
    };
    let execution = quote
        .get("executionPlan")
        .and_then(JsonValue::as_object);
    let authority = execution
        .and_then(|plan| plan.get("settlementAuthority"))
        .and_then(JsonValue::as_str)
        .unwrap_or("");
    let meter = execution
        .and_then(|plan| plan.get("meterProgramId"))
        .and_then(JsonValue::as_str)
        .unwrap_or("");
    let quote_kind = quote.get("kind").and_then(JsonValue::as_str).unwrap_or("");
    let owner_id = app.owner_id();
    let issuer_node = finance_node_record(&app, &owner_id);
    let coordinator_node = finance_node_record(&app, authority);
    let registered_meter = coordinator_node.as_ref().and_then(|row| {
        if quote_kind == "talent" {
            row.get("talentMeterProgramId")
        } else {
            row.get("meterProgramId")
        }
    });
    if authority.is_empty()
        || meter.is_empty()
        || issuer_node
            .as_ref()
            .and_then(|row| row.get("status"))
            .and_then(JsonValue::as_str)
            != Some("active")
        || coordinator_node
            .as_ref()
            .and_then(|row| row.get("status"))
            .and_then(JsonValue::as_str)
            != Some("active")
        || registered_meter.and_then(JsonValue::as_str) != Some(meter)
        || (quote_kind == "talent"
            && (authority != owner_id
                || !app.tools().signaler().listeners().contains_key(meter)))
    {
        return json!({
            "ok": false,
            "error": "quote issuer or coordinator does not match the active finance registry"
        })
        .to_string();
    }
    host_fn_submit_node_finance(
        caller_program_id,
        "/creatures/publishFinanceQuote",
        json!({"quote": quote}),
    )
}

/// Settle a quote-bound financial hold on behalf of the authenticated metering
/// program. The VM never receives the node-owner private key: its program id is
/// resolved from node-owned VM context, checked against the signed hold, and
/// only then does the node sign and submit the settlement action.
/// Atomically reserve an open hold for one authenticated metering run.
pub(crate) fn host_fn_start_hold(caller_program_id: &str, input: &JsonValue) -> String {
    use crate::models::transaction::ITrx;
    use crate::shell::api::packets::creatures::StartHoldInput;
    use std::sync::{mpsc, Arc, Mutex};
    use std::time::Duration;

    let caller_program_id = caller_program_id.trim().to_string();
    let hold_id = input["holdId"].as_str().unwrap_or("").trim().to_string();
    if caller_program_id.is_empty() || hold_id.is_empty() {
        return json!({"ok": false, "error": "verified meter program and holdId are required"})
            .to_string();
    }
    let Some(app) = with_global_app(|app| app.clone()) else {
        return json!({"ok": false, "error": "vmm not initialised"}).to_string();
    };
    let hold_slot = Arc::new(Mutex::new(serde_json::Map::<String, JsonValue>::new()));
    let hold_slot_c = hold_slot.clone();
    let hold_id_c = hold_id.clone();
    app.modify_state(
        true,
        Box::new(move |trx: &dyn ITrx| {
            if let Ok(hold) = trx.get_json(&format!("Json::FinanceHold::{hold_id_c}"), "hold") {
                *hold_slot_c.lock().unwrap() = hold;
            }
            Ok(())
        }),
    );
    let hold = hold_slot.lock().unwrap().clone();
    if hold.is_empty() {
        return json!({"ok": false, "error": "hold not found"}).to_string();
    }
    let hold_status = hold.get("status").and_then(JsonValue::as_str).unwrap_or("");
    let already_started = hold_status == "running"
        && hold.get("runId").and_then(JsonValue::as_str) == input["runId"].as_str();
    if hold_status != "open" && !already_started {
        return json!({"ok": false, "error": "hold is not open for this run"}).to_string();
    }
    if hold.get("meterProgramId").and_then(JsonValue::as_str) != Some(caller_program_id.as_str()) {
        return json!({"ok": false, "error": "calling program is not authorized for this hold"})
            .to_string();
    }
    let owner_id = app.owner_id();
    if hold.get("settlementAuthority").and_then(JsonValue::as_str) != Some(owner_id.as_str()) {
        return json!({"ok": false, "error": "hold authority is not this node owner"}).to_string();
    }
    if already_started {
        return json!({"ok": true, "alreadyApplied": true, "holdId": hold_id}).to_string();
    }

    let start: StartHoldInput = match serde_json::from_value(input.clone()) {
        Ok(value) => value,
        Err(err) => {
            return json!({"ok": false, "error": format!("invalid start: {err}")}).to_string()
        }
    };
    let payload = match serde_json::to_vec(&start) {
        Ok(value) => value,
        Err(err) => {
            return json!({"ok": false, "error": format!("cannot encode start: {err}")}).to_string()
        }
    };
    let signature = app.sign_packet_as_owner(&payload);
    let (tx, rx) = mpsc::channel::<(Vec<u8>, i64, Option<String>)>();
    let callback: crate::models::globe::BaseResponseCallback =
        Box::new(move |data, status, err| {
            let _ = tx.send((data, status, err.map(|value| value.to_string())));
        });
    app.globe().send_base_request_on_chain(
        "/creatures/startHold",
        payload,
        &signature,
        &owner_id,
        "",
        callback,
    );
    match rx.recv_timeout(Duration::from_secs(30)) {
        Ok((data, status, None)) => {
            let result = serde_json::from_slice::<JsonValue>(&data).unwrap_or(JsonValue::Null);
            json!({"ok": status < 400, "statusCode": status, "result": result}).to_string()
        }
        Ok((_data, status, Some(error))) => {
            json!({"ok": false, "statusCode": status, "error": error}).to_string()
        }
        Err(_) => json!({"ok": false, "error": "hold start timed out"}).to_string(),
    }
}

pub(crate) fn host_fn_release_hold(caller_program_id: &str, input: &JsonValue) -> String {
    use crate::models::transaction::ITrx;
    use crate::shell::api::packets::creatures::ReleaseHoldInput;
    use std::sync::{mpsc, Arc, Mutex};
    use std::time::Duration;

    let caller_program_id = caller_program_id.trim().to_string();
    let hold_id = input["holdId"].as_str().unwrap_or("").trim().to_string();
    if caller_program_id.is_empty() || hold_id.is_empty() {
        return json!({"ok": false, "error": "verified meter program and holdId are required"})
            .to_string();
    }
    let Some(app) = with_global_app(|app| app.clone()) else {
        return json!({"ok": false, "error": "vmm not initialised"}).to_string();
    };
    let hold_slot = Arc::new(Mutex::new(serde_json::Map::<String, JsonValue>::new()));
    let hold_slot_c = hold_slot.clone();
    let hold_id_c = hold_id.clone();
    app.modify_state(
        true,
        Box::new(move |trx: &dyn ITrx| {
            if let Ok(hold) = trx.get_json(&format!("Json::FinanceHold::{hold_id_c}"), "hold") {
                *hold_slot_c.lock().unwrap() = hold;
            }
            Ok(())
        }),
    );
    let hold = hold_slot.lock().unwrap().clone();
    if hold.is_empty() {
        return json!({"ok": false, "error": "hold not found"}).to_string();
    }
    let status = hold.get("status").and_then(JsonValue::as_str).unwrap_or("");
    let release_id = input["releaseId"].as_str().unwrap_or("");
    let already_released = matches!(status, "released" | "expired")
        && hold.get("releaseId").and_then(JsonValue::as_str) == Some(release_id);
    if status != "open" && status != "running" && !already_released {
        return json!({"ok": false, "error": "hold is not active for this release"}).to_string();
    }
    if hold.get("meterProgramId").and_then(JsonValue::as_str) != Some(caller_program_id.as_str()) {
        return json!({"ok": false, "error": "calling program is not authorized for this hold"})
            .to_string();
    }
    let owner_id = app.owner_id();
    if hold.get("settlementAuthority").and_then(JsonValue::as_str) != Some(owner_id.as_str()) {
        return json!({"ok": false, "error": "hold authority is not this node owner"}).to_string();
    }
    if already_released {
        return json!({"ok": true, "alreadyApplied": true, "holdId": hold_id}).to_string();
    }
    let release: ReleaseHoldInput = match serde_json::from_value(input.clone()) {
        Ok(value) => value,
        Err(err) => {
            return json!({"ok": false, "error": format!("invalid release: {err}")}).to_string()
        }
    };
    let payload = match serde_json::to_vec(&release) {
        Ok(value) => value,
        Err(err) => {
            return json!({"ok": false, "error": format!("cannot encode release: {err}")})
                .to_string()
        }
    };
    let signature = app.sign_packet_as_owner(&payload);
    let (tx, rx) = mpsc::channel::<(Vec<u8>, i64, Option<String>)>();
    let callback: crate::models::globe::BaseResponseCallback =
        Box::new(move |data, status, err| {
            let _ = tx.send((data, status, err.map(|value| value.to_string())));
        });
    app.globe().send_base_request_on_chain(
        "/creatures/releaseHold",
        payload,
        &signature,
        &owner_id,
        "",
        callback,
    );
    match rx.recv_timeout(Duration::from_secs(30)) {
        Ok((data, status, None)) => {
            let result = serde_json::from_slice::<JsonValue>(&data).unwrap_or(JsonValue::Null);
            json!({"ok": status < 400, "statusCode": status, "result": result}).to_string()
        }
        Ok((_data, status, Some(error))) => {
            json!({"ok": false, "statusCode": status, "error": error}).to_string()
        }
        Err(_) => json!({"ok": false, "error": "hold release timed out"}).to_string(),
    }
}

pub(crate) fn host_fn_settle_hold(caller_program_id: &str, input: &JsonValue) -> String {
    use crate::models::transaction::ITrx;
    use crate::shell::api::packets::creatures::SettleHoldInput;
    use std::sync::{mpsc, Arc, Mutex};
    use std::time::Duration;

    let caller_program_id = caller_program_id.trim().to_string();
    let hold_id = input["holdId"].as_str().unwrap_or("").trim().to_string();
    if caller_program_id.is_empty() || hold_id.is_empty() {
        return json!({"ok": false, "error": "verified meter program and holdId are required"})
            .to_string();
    }

    let Some(app) = with_global_app(|app| app.clone()) else {
        return json!({"ok": false, "error": "vmm not initialised"}).to_string();
    };
    let hold_slot = Arc::new(Mutex::new(serde_json::Map::<String, JsonValue>::new()));
    let hold_slot_c = hold_slot.clone();
    let hold_id_c = hold_id.clone();
    app.modify_state(
        true,
        Box::new(move |trx: &dyn ITrx| {
            if let Ok(hold) = trx.get_json(&format!("Json::FinanceHold::{hold_id_c}"), "hold") {
                *hold_slot_c.lock().unwrap() = hold;
            }
            Ok(())
        }),
    );
    let hold = hold_slot.lock().unwrap().clone();
    if hold.is_empty() {
        return json!({"ok": false, "error": "hold not found"}).to_string();
    }
    let hold_status = hold.get("status").and_then(JsonValue::as_str).unwrap_or("");
    let already_settled = hold_status == "settled"
        && hold.get("settlementId").and_then(JsonValue::as_str) == input["settlementId"].as_str();
    if hold_status != "running" && !already_settled {
        return json!({"ok": false, "error": "hold is not running for this settlement"})
            .to_string();
    }
    if hold.get("meterProgramId").and_then(JsonValue::as_str) != Some(caller_program_id.as_str()) {
        return json!({"ok": false, "error": "calling program is not authorized for this hold"})
            .to_string();
    }

    let owner_id = app.owner_id();
    if hold.get("settlementAuthority").and_then(JsonValue::as_str) != Some(owner_id.as_str()) {
        return json!({
            "ok": false,
            "error": "hold settlement authority is not this node owner"
        })
        .to_string();
    }
    if already_settled {
        return json!({"ok": true, "alreadyApplied": true, "holdId": hold_id}).to_string();
    }

    let settle: SettleHoldInput = match serde_json::from_value(input.clone()) {
        Ok(value) => value,
        Err(err) => {
            return json!({"ok": false, "error": format!("invalid settlement: {err}")}).to_string()
        }
    };
    let payload = match serde_json::to_vec(&settle) {
        Ok(value) => value,
        Err(err) => {
            return json!({"ok": false, "error": format!("cannot encode settlement: {err}")})
                .to_string()
        }
    };
    let signature = app.sign_packet_as_owner(&payload);
    let (tx, rx) = mpsc::channel::<(Vec<u8>, i64, Option<String>)>();
    let callback: crate::models::globe::BaseResponseCallback =
        Box::new(move |data, status, err| {
            let _ = tx.send((data, status, err.map(|value| value.to_string())));
        });
    app.globe().send_base_request_on_chain(
        "/creatures/settleHold",
        payload,
        &signature,
        &owner_id,
        "",
        callback,
    );

    match rx.recv_timeout(Duration::from_secs(30)) {
        Ok((data, status, None)) => {
            let result = serde_json::from_slice::<JsonValue>(&data).unwrap_or(JsonValue::Null);
            json!({"ok": status < 400, "statusCode": status, "result": result}).to_string()
        }
        Ok((_data, status, Some(error))) => {
            json!({"ok": false, "statusCode": status, "error": error}).to_string()
        }
        Err(_) => json!({"ok": false, "error": "settlement timed out"}).to_string(),
    }
}

/// Shared gateway for the meter's authority-signed pool operations (reservePool,
/// settlePool, releasePool). Mirrors the hold gateways: loads the pool to prove
/// the calling program is its registered meter and this node owner is its
/// settlement authority, then signs the request as the node owner and forwards
/// it to the chain action. The meter (a guest creature) can therefore never
/// reserve or settle against a pool it was not bound to.
pub(crate) fn host_fn_pool_authority_call(
    caller_program_id: &str,
    input: &JsonValue,
    route: &str,
    op_label: &str,
) -> String {
    use crate::models::transaction::ITrx;
    use std::sync::{mpsc, Arc, Mutex};
    use std::time::Duration;

    let caller_program_id = caller_program_id.trim().to_string();
    let pool_id = input["poolId"].as_str().unwrap_or("").trim().to_string();
    if caller_program_id.is_empty() || pool_id.is_empty() {
        return json!({"ok": false, "error": "verified meter program and poolId are required"})
            .to_string();
    }
    let Some(app) = with_global_app(|app| app.clone()) else {
        return json!({"ok": false, "error": "vmm not initialised"}).to_string();
    };
    let pool_slot = Arc::new(Mutex::new(serde_json::Map::<String, JsonValue>::new()));
    let pool_slot_c = pool_slot.clone();
    let pool_id_c = pool_id.clone();
    app.modify_state(
        true,
        Box::new(move |trx: &dyn ITrx| {
            if let Ok(pool) = trx.get_json(&format!("Json::FinancePool::{pool_id_c}"), "pool") {
                *pool_slot_c.lock().unwrap() = pool;
            }
            Ok(())
        }),
    );
    let pool = pool_slot.lock().unwrap().clone();
    if pool.is_empty() {
        return json!({"ok": false, "error": "pool not found"}).to_string();
    }
    if pool.get("meterProgramId").and_then(JsonValue::as_str) != Some(caller_program_id.as_str()) {
        return json!({"ok": false, "error": "calling program is not authorized for this pool"})
            .to_string();
    }
    let owner_id = app.owner_id();
    if pool.get("settlementAuthority").and_then(JsonValue::as_str) != Some(owner_id.as_str()) {
        return json!({"ok": false, "error": "pool authority is not this node owner"}).to_string();
    }
    let payload = match serde_json::to_vec(input) {
        Ok(value) => value,
        Err(err) => {
            return json!({"ok": false, "error": format!("cannot encode {op_label}: {err}")})
                .to_string()
        }
    };
    let signature = app.sign_packet_as_owner(&payload);
    let (tx, rx) = mpsc::channel::<(Vec<u8>, i64, Option<String>)>();
    let callback: crate::models::globe::BaseResponseCallback =
        Box::new(move |data, status, err| {
            let _ = tx.send((data, status, err.map(|value| value.to_string())));
        });
    app.globe().send_base_request_on_chain(route, payload, &signature, &owner_id, "", callback);
    match rx.recv_timeout(Duration::from_secs(30)) {
        Ok((data, status, None)) => {
            let result = serde_json::from_slice::<JsonValue>(&data).unwrap_or(JsonValue::Null);
            json!({"ok": status < 400, "statusCode": status, "result": result}).to_string()
        }
        Ok((_data, status, Some(error))) => {
            json!({"ok": false, "statusCode": status, "error": error}).to_string()
        }
        Err(_) => json!({"ok": false, "error": format!("{op_label} timed out")}).to_string(),
    }
}

#[cfg(test)]
mod http_stream_tests {
    use super::*;

    #[test]
    fn openai_chunks_fold_into_one_message_with_usage() {
        let raw = concat!(
            "data: {\"id\":\"c1\",\"model\":\"gpt-x\",\"choices\":[{\"delta\":{\"role\":\"assistant\",\"content\":\"Hel\"}}]}\n",
            "\n",
            "data: {\"choices\":[{\"delta\":{\"content\":\"lo\"},\"finish_reason\":null}]}\n",
            "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":11,\"completion_tokens\":2}}\n",
            "data: [DONE]\n",
        );
        let (out, events) = aggregate_sse_stream(raw);
        assert_eq!(events, 3);
        assert_eq!(out["choices"][0]["message"]["content"], "Hello");
        assert_eq!(out["choices"][0]["message"]["role"], "assistant");
        assert_eq!(out["choices"][0]["finish_reason"], "stop");
        // Usage arrives only on the final event, which is exactly why the host
        // has to read the stream for a metered proxy to bill it.
        assert_eq!(out["usage"]["prompt_tokens"], 11);
        assert_eq!(out["id"], "c1");
        assert_eq!(out["model"], "gpt-x");
    }

    #[test]
    fn tool_call_arguments_split_across_events_are_reassembled_in_order() {
        let raw = concat!(
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"t1\",\"function\":{\"name\":\"search\",\"arguments\":\"{\\\"q\\\":\"}}]}}]}\n",
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"\\\"cats\\\"}\"}}]}}]}\n",
        );
        let (out, _) = aggregate_sse_stream(raw);
        let call = &out["choices"][0]["message"]["tool_calls"][0];
        assert_eq!(call["id"], "t1");
        assert_eq!(call["function"]["name"], "search");
        assert_eq!(call["function"]["arguments"], "{\"q\":\"cats\"}");
    }

    #[test]
    fn anthropic_events_are_reduced_to_the_same_shape() {
        let raw = concat!(
            "event: message_start\n",
            "data: {\"type\":\"message_start\",\"message\":{\"id\":\"m1\",\"model\":\"claude-x\",\"usage\":{\"input_tokens\":7,\"output_tokens\":0}}}\n",
            "data: {\"type\":\"content_block_delta\",\"delta\":{\"text\":\"hi\"}}\n",
            "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":3}}\n",
        );
        let (out, _) = aggregate_sse_stream(raw);
        assert_eq!(out["choices"][0]["message"]["content"], "hi");
        assert_eq!(out["choices"][0]["finish_reason"], "end_turn");
        // One vocabulary out, whichever went in: the caller writes one parser.
        assert_eq!(out["usage"]["prompt_tokens"], 7);
        assert_eq!(out["usage"]["completion_tokens"], 3);
    }

    #[test]
    fn an_unrecognised_single_frame_survives_instead_of_becoming_an_empty_answer() {
        let raw = "data: {\"error\":{\"message\":\"model not found\"}}\n";
        let (out, events) = aggregate_sse_stream(raw);
        assert_eq!(events, 1);
        assert_eq!(out["error"]["message"], "model not found");
    }

    #[test]
    fn an_empty_stream_reports_nothing_rather_than_an_invented_message() {
        let (out, events) = aggregate_sse_stream("\n\n");
        assert!(out.is_null());
        assert_eq!(events, 0);
    }
}
